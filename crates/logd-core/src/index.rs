//! 分块稀疏行索引。
//!
//! 50GB 日志按 100 字节/行算约 5 亿行。每行存一个 u64 偏移就是 4GB，不可接受。
//! 这里每 [`ANCHOR_STRIDE`] 行才记一个锚点，5 亿行只占约 4MB；取任意行时从最近的
//! 锚点起 `memchr` 前进最多 1023 个换行（约 100KB，~20µs）。
//!
//! 锚点按「块」存放而不是一张全局表：并行建索引时每个 worker 只知道块内的局部行号，
//! 全局行号要等所有块统计完才能前缀和得出。按块存让合并退化成一次前缀和，
//! 既精确又不需要二次扫描。

use memchr::{memchr, memchr_iter};
use rayon::prelude::*;

use crate::progress::Progress;

/// 每多少行打一个锚点。
pub const ANCHOR_STRIDE: u64 = 1024;

/// 并行分块的目标块大小。块多一点进度条更平滑、rayon 负载更均衡；
/// 50GB / 64MiB = 800 块，每块的 `anchors` 只有几百项。
const CHUNK_BYTES: u64 = 64 * 1024 * 1024;

/// 首屏快速索引的字节上限，单线程扫完约 100ms。
pub const HEAD_BYTES: u64 = 256 * 1024 * 1024;

/// 取消检查的行间隔。
const CANCEL_CHECK_LINES: u64 = 1 << 16;

#[derive(Debug, Clone)]
pub struct ChunkIndex {
    /// 本块第一整行的起始字节（块边界总是对齐到行首）。
    pub start_byte: u64,
    /// 本块结束字节（不含）。等于下一块的 `start_byte`，所以跨界的那一行归本块。
    pub end_byte: u64,
    /// 本块第一行的全局行号，由前缀和填入。
    pub start_line: u64,
    pub line_count: u64,
    /// 块内局部第 0、STRIDE、2*STRIDE… 行的**绝对**字节偏移。
    pub anchors: Vec<u64>,
}

#[derive(Debug, Default, Clone)]
pub struct LineIndex {
    /// 按 `start_line` 升序。
    pub chunks: Vec<ChunkIndex>,
    pub total_lines: u64,
    /// 索引实际覆盖到的字节位置。首屏索引时 < `file_len`。
    pub indexed_bytes: u64,
    pub file_len: u64,
    /// 是否已覆盖整个文件。false 时行数显示应带 `+`。
    pub complete: bool,
}

impl LineIndex {
    /// 只索引头部 `limit` 字节，单线程，用于打开文件后立刻出首屏。
    ///
    /// 为了不把一行劈成两半，实际覆盖范围会回退到 `limit` 之前最后一个换行处。
    pub fn build_head(data: &[u8], limit: u64) -> LineIndex {
        let file_len = data.len() as u64;
        if data.is_empty() {
            return LineIndex {
                file_len,
                complete: true,
                ..Default::default()
            };
        }

        let complete = limit >= file_len;
        let end = if complete {
            data.len()
        } else {
            // 回退到 limit 之前的最后一个换行之后，保证末尾不是半行。
            match memchr::memrchr(b'\n', &data[..limit as usize]) {
                Some(p) => p + 1,
                // 头 256MB 一个换行都没有：这文件不像日志，直接整份交给全量索引。
                None => return LineIndex::build_head(data, file_len),
            }
        };

        let mut chunk = index_chunk(data, 0, end, complete, None);
        chunk.start_line = 0;
        let total_lines = chunk.line_count;
        let chunks = if total_lines == 0 { Vec::new() } else { vec![chunk] };

        LineIndex {
            chunks,
            total_lines,
            indexed_bytes: end as u64,
            file_len,
            complete,
        }
    }

    /// 全量并行索引。被取消时返回 `None`。
    pub fn build_full(data: &[u8], progress: &Progress) -> Option<LineIndex> {
        let file_len = data.len() as u64;
        progress.set_total(file_len);

        if data.is_empty() {
            return Some(LineIndex {
                file_len,
                complete: true,
                ..Default::default()
            });
        }

        // 先串行算出每块对齐到行首的起点。每次 memchr 只扫几百字节，可忽略。
        let nchunks = (file_len / CHUNK_BYTES).max(1) as usize;
        let mut starts: Vec<usize> = Vec::with_capacity(nchunks + 1);
        starts.push(0);
        for k in 1..nchunks {
            let raw = (k as u64 * file_len / nchunks as u64) as usize;
            let Some(s) = align_to_line_start(data, raw) else {
                // 从 raw 到 EOF 没有换行了，后面的块全是空的。
                break;
            };
            if s > *starts.last().unwrap() && s < data.len() {
                starts.push(s);
            }
        }
        starts.push(data.len());

        let ranges: Vec<(usize, usize, bool)> = starts
            .windows(2)
            .enumerate()
            .map(|(i, w)| (w[0], w[1], i == starts.len() - 2))
            .collect();

        let mut chunks: Vec<ChunkIndex> = ranges
            .par_iter()
            .map(|&(s, e, last)| index_chunk(data, s, e, last, Some(progress)))
            .collect();

        if progress.is_cancelled() {
            return None;
        }

        // 前缀和填全局行号，顺手丢掉空块（超长行会造出空块）。
        chunks.retain(|c| c.line_count > 0);
        let mut acc = 0u64;
        for c in chunks.iter_mut() {
            c.start_line = acc;
            acc += c.line_count;
        }

        Some(LineIndex {
            chunks,
            total_lines: acc,
            indexed_bytes: file_len,
            file_len,
            complete: true,
        })
    }

    /// 第 `line` 行的起始字节偏移。`line >= total_lines` 时返回 `None`。
    pub fn line_start(&self, data: &[u8], line: u64) -> Option<u64> {
        if line >= self.total_lines {
            return None;
        }
        // 第一个满足 start_line + line_count > line 的块。空块自然被跳过。
        let ci = self
            .chunks
            .partition_point(|c| c.start_line + c.line_count <= line);
        let c = self.chunks.get(ci)?;
        let local = line - c.start_line;
        let mut pos = *c.anchors.get((local / ANCHOR_STRIDE) as usize)? as usize;
        for _ in 0..(local % ANCHOR_STRIDE) {
            pos += memchr(b'\n', data.get(pos..)?)? + 1;
        }
        Some(pos as u64)
    }

    /// 取从 `first` 起最多 `count` 行的字节区间 `[start, end)`，已剥掉行尾的 `\r\n`。
    ///
    /// 渲染路径每帧只会要 ~60 行，成本是一次锚点定位 + 顺序 memchr。
    pub fn line_spans(&self, data: &[u8], first: u64, count: usize, out: &mut Vec<(u64, u64)>) {
        out.clear();
        let Some(start) = self.line_start(data, first) else {
            return;
        };
        let mut pos = start as usize;
        let n = count.min(self.total_lines.saturating_sub(first) as usize);
        for _ in 0..n {
            let (mut end, next) = match memchr(b'\n', &data[pos..]) {
                Some(p) => (pos + p, pos + p + 1),
                None => (data.len(), data.len()),
            };
            if end > pos && data[end - 1] == b'\r' {
                end -= 1;
            }
            out.push((pos as u64, end as u64));
            pos = next;
            if pos >= data.len() {
                break;
            }
        }
    }

    /// 索引自身的内存占用，用于状态栏显示。
    pub fn heap_bytes(&self) -> usize {
        self.chunks.len() * std::mem::size_of::<ChunkIndex>()
            + self
                .chunks
                .iter()
                .map(|c| c.anchors.len() * 8)
                .sum::<usize>()
    }
}

/// 返回 `>= raw` 的第一个整行起点；`raw` 之后没有换行时返回 `None`。
fn align_to_line_start(data: &[u8], raw: usize) -> Option<usize> {
    if raw == 0 {
        return Some(0);
    }
    memchr(b'\n', data.get(raw..)?).map(|p| raw + p + 1)
}

/// 索引 `[start, end)`。非末块的 `end` 必定紧跟在一个换行之后，块内全是完整行。
fn index_chunk(
    data: &[u8],
    start: usize,
    end: usize,
    is_last: bool,
    progress: Option<&Progress>,
) -> ChunkIndex {
    let mut anchors = Vec::new();
    let mut line_count: u64 = 0;

    if end > start {
        anchors.push(start as u64); // 局部第 0 行
        for p in memchr_iter(b'\n', &data[start..end]) {
            line_count += 1; // 这个换行结束了局部第 line_count-1 行
            let next = start + p + 1;
            // next == end 说明下一行属于下一块，不该在这里打锚点。
            if line_count % ANCHOR_STRIDE == 0 && next < end {
                anchors.push(next as u64);
            }
            if line_count % CANCEL_CHECK_LINES == 0 {
                if let Some(pr) = progress {
                    if pr.is_cancelled() {
                        break;
                    }
                }
            }
        }
        // 文件末尾可能有一行没有换行结尾。
        if is_last && data[end - 1] != b'\n' {
            line_count += 1;
        }
        if line_count == 0 {
            anchors.clear(); // 整块是一行的中间部分（超长行），本块不拥有任何行
        }
    }

    if let Some(pr) = progress {
        pr.add((end - start) as u64);
    }

    ChunkIndex {
        start_byte: start as u64,
        end_byte: end as u64,
        start_line: 0,
        line_count,
        anchors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 朴素实现，作为对照组。
    fn naive_lines(data: &[u8]) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos < data.len() {
            let (mut end, next) = match memchr(b'\n', &data[pos..]) {
                Some(p) => (pos + p, pos + p + 1),
                None => (data.len(), data.len()),
            };
            if end > pos && data[end - 1] == b'\r' {
                end -= 1;
            }
            out.push((pos as u64, end as u64));
            pos = next;
        }
        out
    }

    fn check(data: &[u8]) {
        let expect = naive_lines(data);
        let idx = LineIndex::build_full(data, &Progress::default()).unwrap();
        assert_eq!(idx.total_lines, expect.len() as u64, "行数不符");

        let mut got = Vec::new();
        idx.line_spans(data, 0, expect.len(), &mut got);
        assert_eq!(got, expect, "整体行区间不符");

        // 随机抽查，验证锚点定位而不是顺序扫描
        for i in (0..expect.len()).step_by(7) {
            let mut one = Vec::new();
            idx.line_spans(data, i as u64, 1, &mut one);
            assert_eq!(one, vec![expect[i]], "第 {i} 行定位错误");
        }
    }

    #[test]
    fn empty_file() {
        check(b"");
    }

    #[test]
    fn no_trailing_newline() {
        check(b"a\nbb\nccc");
    }

    #[test]
    fn with_trailing_newline() {
        check(b"a\nbb\nccc\n");
    }

    #[test]
    fn crlf_and_blank_lines() {
        check(b"a\r\n\r\nbb\r\n\r\n");
    }

    #[test]
    fn crosses_anchor_stride() {
        let mut s = Vec::new();
        for i in 0..(ANCHOR_STRIDE * 5 + 13) {
            s.extend_from_slice(format!("line {i} payload\n").as_bytes());
        }
        check(&s);
    }

    #[test]
    fn single_huge_line() {
        let mut s = vec![b'x'; 4 << 20];
        s.push(b'\n');
        s.extend_from_slice(b"tail");
        check(&s);
    }

    #[test]
    fn head_index_is_prefix_of_full() {
        let mut s = Vec::new();
        for i in 0..50_000 {
            s.extend_from_slice(format!("{i:08} some log payload here\n").as_bytes());
        }
        let head = LineIndex::build_head(&s, 100_000);
        assert!(!head.complete);
        assert!(head.total_lines > 0);

        let full = LineIndex::build_full(&s, &Progress::default()).unwrap();
        assert!(head.total_lines < full.total_lines);

        // 头部索引给出的行必须和全量索引一致
        let (mut a, mut b) = (Vec::new(), Vec::new());
        head.line_spans(&s, 0, head.total_lines as usize, &mut a);
        full.line_spans(&s, 0, head.total_lines as usize, &mut b);
        assert_eq!(a, b);
    }

    #[test]
    fn out_of_range_line() {
        let data = b"a\nb\n";
        let idx = LineIndex::build_full(data, &Progress::default()).unwrap();
        assert_eq!(idx.line_start(data, 2), None);
        let mut out = vec![(9, 9)];
        idx.line_spans(data, 99, 10, &mut out);
        assert!(out.is_empty());
    }
}
