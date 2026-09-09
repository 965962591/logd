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

#[derive(Debug)]
pub struct FilterScanResult {
    pub outcome: ScanOutcome,
    /// Number of matching lines for each filter in `MatcherSet::filters()`.
    pub filter_counts: Vec<u64>,
    /// Lines matching the selected include filters after selected excludes.
    pub selected_filter_lines: Vec<u64>,
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

/// Apply configured filters and a temporary query while collecting per-filter
/// matching line counts in the same pass.
pub fn scan_all_with_query_and_counts(
    data: &[u8],
    index: &LineIndex,
    matcher: &MatcherSet,
    query: &Query,
    progress: &Progress,
) -> Option<FilterScanResult> {
    scan_all_with_query_and_counts_for_filters(data, index, matcher, query, &[], progress)
}

/// Apply configured filters together with a temporary title-bar query.
///
/// `configured_filter_count` identifies the prefix of `matcher.filters()` that
/// came from the persisted filter configuration. The query is treated as an
/// additional, temporary include condition: it is not persisted and it does
/// not participate in configured-filter counts. Configured excludes still
/// hide matching lines. The result is cancelled by `progress` in the same way
/// as the other full-file scanners.
pub fn scan_all_with_temporary_query_and_counts_for_filters(
    data: &[u8],
    index: &LineIndex,
    matcher: &MatcherSet,
    query: &Query,
    configured_filter_count: usize,
    result_filters: &[bool],
    progress: &Progress,
) -> Option<FilterScanResult> {
    scan_all_with_query_and_counts_for_filters_impl(
        data,
        index,
        matcher,
        query,
        result_filters,
        progress,
        Some(configured_filter_count),
    )
}

/// Apply configured filters and a temporary query while also collecting the
/// lines hit by selected include filters, minus lines hit by selected excludes.
pub fn scan_all_with_query_and_counts_for_filters(
    data: &[u8],
    index: &LineIndex,
    matcher: &MatcherSet,
    query: &Query,
    result_filters: &[bool],
    progress: &Progress,
) -> Option<FilterScanResult> {
    scan_all_with_query_and_counts_for_filters_impl(
        data,
        index,
        matcher,
        query,
        result_filters,
        progress,
        None,
    )
}

fn scan_all_with_query_and_counts_for_filters_impl(
    data: &[u8],
    index: &LineIndex,
    matcher: &MatcherSet,
    query: &Query,
    result_filters: &[bool],
    progress: &Progress,
    temporary_query_filter_count: Option<usize>,
) -> Option<FilterScanResult> {
    progress.set_total(index.indexed_bytes.max(1));

    if matcher.is_noop() && query.is_empty() {
        progress.add(index.indexed_bytes);
        return Some(FilterScanResult {
            outcome: ScanOutcome::AllVisible,
            filter_counts: vec![0; matcher.filters().len()],
            selected_filter_lines: Vec::new(),
        });
    }

    let parts: Vec<FilterScanChunk> = index
        .chunks
        .par_iter()
        .map(|chunk| {
            scan_filter_chunk(
                data,
                chunk,
                matcher,
                query,
                result_filters,
                progress,
                temporary_query_filter_count,
            )
        })
        .collect();

    if progress.is_cancelled() {
        return None;
    }

    let total: usize = parts.iter().map(|part| part.lines.len()).sum();
    let selected_total: usize = parts
        .iter()
        .map(|part| part.selected_filter_lines.len())
        .sum();
    let mut lines = Vec::with_capacity(total);
    let mut selected_filter_lines = Vec::with_capacity(selected_total);
    let mut filter_counts = vec![0u64; matcher.filters().len()];
    for part in parts {
        lines.extend(part.lines);
        selected_filter_lines.extend(part.selected_filter_lines);
        for (total, count) in filter_counts.iter_mut().zip(part.filter_counts) {
            *total = total.saturating_add(count);
        }
    }
    Some(FilterScanResult {
        outcome: ScanOutcome::Matched(lines),
        filter_counts,
        selected_filter_lines,
    })
}

struct FilterScanChunk {
    lines: Vec<u64>,
    filter_counts: Vec<u64>,
    selected_filter_lines: Vec<u64>,
}

fn scan_filter_chunk(
    data: &[u8],
    chunk: &ChunkIndex,
    matcher: &MatcherSet,
    query: &Query,
    result_filters: &[bool],
    progress: &Progress,
    temporary_query_filter_count: Option<usize>,
) -> FilterScanChunk {
    let end = (chunk.end_byte as usize).min(data.len());
    let mut pos = chunk.start_byte as usize;
    let mut lines = Vec::new();
    let mut selected_filter_lines = Vec::new();
    let mut filter_counts = vec![0u64; matcher.filters().len()];
    let mut filter_hits = Vec::with_capacity(matcher.filters().len());
    let mut query_scratch = QueryScratch::default();

    for line_offset in 0..chunk.line_count {
        if pos >= end {
            break;
        }
        let (line_end, next) = match memchr(b'\n', &data[pos..end]) {
            Some(offset) => (pos + offset, pos + offset + 1),
            None => (end, end),
        };
        let mut content_end = line_end;
        if content_end > pos && data[content_end - 1] == b'\r' {
            content_end -= 1;
        }
        let line = &data[pos..content_end];
        let filters_visible = matcher.matching_filters(line, &mut filter_hits);
        for (count, hit) in filter_counts.iter_mut().zip(filter_hits.iter().copied()) {
            if hit {
                *count = count.saturating_add(1);
            }
        }
        let mut selected_include = false;
        let mut selected_exclude = false;
        for ((hit, selected), filter) in filter_hits
            .iter()
            .zip(result_filters)
            .zip(matcher.filters())
        {
            if !*hit || !*selected {
                continue;
            }
            if filter.excluding {
                selected_exclude = true;
            } else {
                selected_include = true;
            }
        }
        if selected_include && !selected_exclude {
            selected_filter_lines.push(chunk.start_line + line_offset);
        }

        let line_visible = match temporary_query_filter_count {
            Some(configured_filter_count) if !query.is_empty() => temporary_query_visible(
                matcher,
                &filter_hits,
                configured_filter_count,
                query,
                line,
                &mut query_scratch,
            ),
            _ => filters_visible && query.matches(line, &mut query_scratch),
        };
        if line_visible {
            lines.push(chunk.start_line + line_offset);
        }
        pos = next;

        if line_offset % CANCEL_CHECK_LINES == 0 && progress.is_cancelled() {
            break;
        }
    }

    progress.add(chunk.end_byte - chunk.start_byte);
    FilterScanChunk {
        lines,
        filter_counts,
        selected_filter_lines,
    }
}

/// Return the visibility of a line when the title-bar query is a temporary
/// include alongside the configured filter prefix. Configured excludes always
/// win. If configured includes exist, either one of those or the query may
/// select a line; with no configured include, the query itself is the only
/// include condition while it is active.
fn temporary_query_visible(
    matcher: &MatcherSet,
    filter_hits: &[bool],
    configured_filter_count: usize,
    query: &Query,
    line: &[u8],
    query_scratch: &mut QueryScratch,
) -> bool {
    let configured_filters = matcher.filters().iter().take(configured_filter_count);
    let mut has_configured_include = false;
    let mut configured_include_hit = false;

    for (index, filter) in configured_filters.enumerate() {
        if !filter.is_active() {
            continue;
        }
        if filter.excluding {
            if filter_hits.get(index).copied().unwrap_or(false) {
                return false;
            }
        } else {
            has_configured_include = true;
            configured_include_hit |= filter_hits.get(index).copied().unwrap_or(false);
        }
    }

    let query_hit = query.matches(line, query_scratch);
    if has_configured_include {
        configured_include_hit || query_hit
    } else {
        query_hit
    }
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

    #[test]
    fn filter_counts_are_collected_in_the_combined_scan() {
        let (data, idx) = build("alpha alpha beta\nalpha\nbeta noise\nnoise\n");
        let matcher = matcher(vec![
            FilterSpec {
                text: "alpha".into(),
                ..Default::default()
            },
            FilterSpec {
                text: "beta".into(),
                ..Default::default()
            },
            FilterSpec {
                text: "noise".into(),
                excluding: true,
                ..Default::default()
            },
            FilterSpec {
                text: "alpha".into(),
                enabled: false,
                ..Default::default()
            },
        ]);
        let query = Query::parse("beta", CompileOptions::default()).unwrap();

        let result =
            scan_all_with_query_and_counts(&data, &idx, &matcher, &query, &Progress::default())
                .unwrap();
        let ScanOutcome::Matched(lines) = result.outcome else {
            panic!()
        };
        assert_eq!(lines, vec![0]);
        assert_eq!(result.filter_counts, vec![2, 2, 2, 0]);
    }

    #[test]
    fn selected_filter_lines_ignore_query_but_respect_excludes() {
        let (data, idx) = build("alpha beta\nalpha\nalpha noise\nalpha local\nbeta\n");
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
            FilterSpec {
                text: "local".into(),
                excluding: true,
                ..Default::default()
            },
        ]);
        let query = Query::parse("beta", CompileOptions::default()).unwrap();

        let result = scan_all_with_query_and_counts_for_filters(
            &data,
            &idx,
            &matcher,
            &query,
            &[true, true, false],
            &Progress::default(),
        )
        .unwrap();

        let ScanOutcome::Matched(lines) = result.outcome else {
            panic!()
        };
        assert_eq!(lines, vec![0]);
        assert_eq!(result.selected_filter_lines, vec![0, 1, 3]);
    }

    #[test]
    fn temporary_query_is_an_include_without_being_persisted() {
        let (data, idx) = build("configured only\nquery only\nboth configured query\nother\n");
        let matcher = matcher(vec![
            FilterSpec {
                text: "configured".into(),
                ..Default::default()
            },
            FilterSpec {
                text: "query".into(),
                mode: crate::matcher::HighlightMode::Field,
                ..Default::default()
            },
        ]);
        let query = Query::parse("query", CompileOptions::default()).unwrap();

        let result = scan_all_with_temporary_query_and_counts_for_filters(
            &data,
            &idx,
            &matcher,
            &query,
            1,
            &[],
            &Progress::default(),
        )
        .unwrap();
        let ScanOutcome::Matched(lines) = result.outcome else {
            panic!()
        };
        assert_eq!(lines, vec![0, 1, 2]);
        assert_eq!(result.filter_counts, vec![2, 2]);
    }

    #[test]
    fn temporary_query_still_respects_configured_excludes() {
        let (data, idx) = build("configured\nquery\nquery blocked\n");
        let matcher = matcher(vec![
            FilterSpec {
                text: "configured".into(),
                ..Default::default()
            },
            FilterSpec {
                text: "blocked".into(),
                excluding: true,
                ..Default::default()
            },
            FilterSpec {
                text: "query".into(),
                mode: crate::matcher::HighlightMode::Field,
                ..Default::default()
            },
        ]);
        let query = Query::parse("query", CompileOptions::default()).unwrap();

        let result = scan_all_with_temporary_query_and_counts_for_filters(
            &data,
            &idx,
            &matcher,
            &query,
            2,
            &[],
            &Progress::default(),
        )
        .unwrap();
        let ScanOutcome::Matched(lines) = result.outcome else {
            panic!()
        };
        assert_eq!(lines, vec![0, 1]);
    }
}
