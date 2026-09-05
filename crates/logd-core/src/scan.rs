//! 并行全量筛选。
//!
//! 复用 [`crate::index::LineIndex`] 的分块划分——每块已经知道自己的起始行号，
//! 所以各 worker 可以独立产出**全局**行号，合并时只需按块序拼接，无需再排序。

use memchr::memchr;
use rayon::prelude::*;

use crate::index::{ChunkIndex, LineIndex};
use crate::matcher::MatcherSet;
use crate::progress::Progress;
use crate::query::{Query, QueryScratch};

/// 多少行检查一次取消信号。
const CANCEL_CHECK_LINES: u64 = 4096;

/// 命中行数超过这个量级时，`Vec<u64>` 本身就要吃掉 1.6GB，UI 该提示用户收紧条件了。
pub const MATCH_COUNT_WARN: usize = 200_000_000;

#[derive(Debug)]
pub enum ScanOutcome {
    /// 一条过滤器都没启用，等价于「全部可见」。调用方不应进入筛选视图。
    AllVisible,
    /// 命中行号，全局、升序。
    Matched(Vec<u64>),
}

impl ScanOutcome {
    pub fn len(&self) -> Option<usize> {
        match self {
            ScanOutcome::AllVisible => None,
            ScanOutcome::Matched(v) => Some(v.len()),
        }
    }
}

/// 全量筛选。被取消时返回 `None`。
pub fn scan_all(
    data: &[u8],
    index: &LineIndex,
    matcher: &MatcherSet,
    progress: &Progress,
) -> Option<ScanOutcome> {
    progress.set_total(index.indexed_bytes.max(1));

    if matcher.is_noop() {
        progress.add(index.indexed_bytes);
        return Some(ScanOutcome::AllVisible);
    }

    scan_matching(data, index, progress, |line, _| matcher.is_visible(line))
}

/// Scan all lines with a compiled boolean query. Returns `None` when cancelled.
pub fn scan_query_all(
    data: &[u8],
    index: &LineIndex,
    query: &Query,
    progress: &Progress,
) -> Option<ScanOutcome> {
    progress.set_total(index.indexed_bytes.max(1));

    if query.is_empty() {
        progress.add(index.indexed_bytes);
        return Some(ScanOutcome::AllVisible);
    }

    scan_matching(data, index, progress, |line, scratch| {
        query.matches(line, scratch)
    })
}

/// Apply the configured filter set and a temporary search query together.
pub fn scan_all_with_query(
    data: &[u8],
    index: &LineIndex,
    matcher: &MatcherSet,
    query: &Query,
    progress: &Progress,
) -> Option<ScanOutcome> {
    progress.set_total(index.indexed_bytes.max(1));

    if matcher.is_noop() && query.is_empty() {
        progress.add(index.indexed_bytes);
        return Some(ScanOutcome::AllVisible);
    }

    scan_matching(data, index, progress, |line, scratch| {
        matcher.is_visible(line) && query.matches(line, scratch)
    })
}

fn scan_matching(
    data: &[u8],
    index: &LineIndex,
    progress: &Progress,
    predicate: impl Fn(&[u8], &mut QueryScratch) -> bool + Sync,
) -> Option<ScanOutcome> {
    let parts: Vec<Vec<u64>> = index
        .chunks
        .par_iter()
        .map(|chunk| scan_chunk(data, chunk, progress, &predicate))
        .collect();

    if progress.is_cancelled() {
        return None;
    }

    let total: usize = parts.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(total);
    for p in &parts {
        out.extend_from_slice(p);
    }
    Some(ScanOutcome::Matched(out))
}

fn scan_chunk(
    data: &[u8],
    c: &ChunkIndex,
    progress: &Progress,
    predicate: &(impl Fn(&[u8], &mut QueryScratch) -> bool + Sync),
) -> Vec<u64> {
    let end = (c.end_byte as usize).min(data.len());
    let mut pos = c.start_byte as usize;
    let mut out = Vec::new();
    let mut scratch = QueryScratch::default();

    for i in 0..c.line_count {
        if pos >= end {
            break;
        }
        let (line_end, next) = match memchr(b'\n', &data[pos..end]) {
            Some(p) => (pos + p, pos + p + 1),
            // 只可能发生在文件最后一行（没有换行结尾）
            None => (end, end),
        };
        let mut e = line_end;
        if e > pos && data[e - 1] == b'\r' {
            e -= 1;
        }
        if predicate(&data[pos..e], &mut scratch) {
            out.push(c.start_line + i);
        }
        pos = next;

        if i % CANCEL_CHECK_LINES == 0 && progress.is_cancelled() {
            break;
        }
    }

    progress.add(c.end_byte - c.start_byte);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::FilterSpec;
    use crate::query::{CompileOptions, Query};
    use crate::source::Encoding;

    fn build(text: &str) -> (Vec<u8>, LineIndex) {
        let data = text.as_bytes().to_vec();
        let idx = LineIndex::build_full(&data, &Progress::default()).unwrap();
        (data, idx)
    }

    fn matcher(filters: Vec<FilterSpec>) -> MatcherSet {
        MatcherSet::new(filters, Encoding::Utf8).unwrap()
    }

    #[test]
    fn no_filters_reports_all_visible() {
        let (data, idx) = build("a\nb\nc\n");
        let m = matcher(vec![]);
        let out = scan_all(&data, &idx, &m, &Progress::default()).unwrap();
        assert!(matches!(out, ScanOutcome::AllVisible));
    }

    #[test]
    fn returns_global_line_numbers_in_order() {
        let (data, idx) = build("hit 0\nmiss\nhit 2\nmiss\nhit 4\n");
        let m = matcher(vec![FilterSpec {
            text: "hit".into(),
            ..Default::default()
        }]);
        let ScanOutcome::Matched(v) = scan_all(&data, &idx, &m, &Progress::default()).unwrap()
        else {
            panic!("应该有命中");
        };
        assert_eq!(v, vec![0, 2, 4]);
    }

    #[test]
    fn exclude_filter_removes_lines() {
        let (data, idx) = build("ae one\nae noise\nae two\n");
        let m = matcher(vec![
            FilterSpec {
                text: "ae".into(),
                ..Default::default()
            },
            FilterSpec {
                text: "noise".into(),
                excluding: true,
                ..Default::default()
            },
        ]);
        let ScanOutcome::Matched(v) = scan_all(&data, &idx, &m, &Progress::default()).unwrap()
        else {
            panic!()
        };
        assert_eq!(v, vec![0, 2]);
    }

    /// 跨多个 chunk 时全局行号必须仍然正确且有序。
    #[test]
    fn spans_multiple_chunks() {
        let mut s = String::new();
        for i in 0..200_000 {
            if i % 1000 == 0 {
                s.push_str(&format!("{i:08} TARGET payload\n"));
            } else {
                s.push_str(&format!("{i:08} filler payload\n"));
            }
        }
        let (data, idx) = build(&s);
        assert_eq!(idx.total_lines, 200_000);

        let m = matcher(vec![FilterSpec {
            text: "TARGET".into(),
            ..Default::default()
        }]);
        let ScanOutcome::Matched(v) = scan_all(&data, &idx, &m, &Progress::default()).unwrap()
        else {
            panic!()
        };
        assert_eq!(v.len(), 200);
        assert!(v.windows(2).all(|w| w[0] < w[1]), "结果必须升序");
        assert_eq!(v[0], 0);
        assert_eq!(v[1], 1000);
    }

    #[test]
    fn cancellation_returns_none() {
        let (data, idx) = build("a\nb\nc\n");
        let m = matcher(vec![FilterSpec {
            text: "a".into(),
            ..Default::default()
        }]);
        let p = Progress::default();
        p.cancel();
        assert!(scan_all(&data, &idx, &m, &p).is_none());
    }

    #[test]
    fn last_line_without_newline_is_scanned() {
        let (data, idx) = build("miss\nhit");
        let m = matcher(vec![FilterSpec {
            text: "hit".into(),
            ..Default::default()
        }]);
        let ScanOutcome::Matched(v) = scan_all(&data, &idx, &m, &Progress::default()).unwrap()
        else {
            panic!()
        };
        assert_eq!(v, vec![1]);
    }

    #[test]
    fn crlf_is_stripped_before_matching() {
        let (data, idx) = build("prefix\r\n");
        // 行尾锚定的正则，如果 \r 没剥掉就匹配不上
        let m = matcher(vec![FilterSpec {
            text: "prefix$".into(),
            regex: true,
            ..Default::default()
        }]);
        let ScanOutcome::Matched(v) = scan_all(&data, &idx, &m, &Progress::default()).unwrap()
        else {
            panic!()
        };
        assert_eq!(v, vec![0]);
    }

    #[test]
    fn boolean_query_requires_all_and_terms() {
        let (data, idx) = build("alpha beta\nalpha\nbeta gamma\ngamma\n");
        let query = Query::parse(
            "\"alpha\" and \"beta\" or \"gamma\"",
            CompileOptions::default(),
        )
        .unwrap();
        let ScanOutcome::Matched(lines) =
            scan_query_all(&data, &idx, &query, &Progress::default()).unwrap()
        else {
            panic!()
        };
        assert_eq!(lines, vec![0, 2, 3]);
    }

    #[test]
    fn configured_excludes_still_apply_to_boolean_search() {
        let (data, idx) = build("alpha beta\nalpha beta noise\nalpha\n");
        let matcher = matcher(vec![
            FilterSpec {
                text: "alpha".into(),
                ..Default::default()
            },
            FilterSpec {
                text: "noise".into(),
                excluding: true,
                ..Default::default()
            },
        ]);
        let query = Query::parse("\"alpha\" and \"beta\"", CompileOptions::default()).unwrap();
        let ScanOutcome::Matched(lines) =
            scan_all_with_query(&data, &idx, &matcher, &query, &Progress::default()).unwrap()
        else {
            panic!()
        };
        assert_eq!(lines, vec![0]);
    }
}
