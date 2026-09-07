//! 视图行 ↔ 文件行的映射，以及命中集上的导航。
//!
//! 「仅显示筛选结果」时视图里的第 N 行不是文件的第 N 行。加上上下文行（grep -C）
//! 之后关系更绕：相邻命中的上下文会重叠、要合并，中间还得留出「跳过了多少行」的提示。
//!
//! **不能把展开后的行号物化成数组**：1000 万命中 × 上下文 3 行 = 7000 万个 u64 = 560MB。
//! 这里存的是**合并后的区间 + 前缀和**，`row → line` 走一次二分，
//! 内存只和「区间数」有关，而相邻命中一合并区间数就掉下来了。

use std::ops::Range;
use std::sync::Arc;

/// 命中行号集合，升序无重复。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Matches {
    lines: Vec<u64>,
}

impl Matches {
    pub fn new(lines: Vec<u64>) -> Self {
        debug_assert!(
            lines.windows(2).all(|w| w[0] < w[1]),
            "命中行号必须升序且不重复"
        );
        Self { lines }
    }

    pub fn as_slice(&self) -> &[u64] {
        &self.lines
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// 第 `i` 个命中的文件行号。
    pub fn nth(&self, i: usize) -> Option<u64> {
        self.lines.get(i).copied()
    }

    /// `line` 是第几个命中（0-based）。不是命中行则返回 `Err(插入位置)`。
    pub fn ordinal(&self, line: u64) -> Result<usize, usize> {
        self.lines.binary_search(&line)
    }

    /// 严格在 `line` 之后的下一个命中。F3。
    pub fn next_after(&self, line: u64) -> Option<u64> {
        let i = self.lines.partition_point(|&l| l <= line);
        self.lines.get(i).copied()
    }

    /// 严格在 `line` 之前的上一个命中。Shift+F3。
    pub fn prev_before(&self, line: u64) -> Option<u64> {
        let i = self.lines.partition_point(|&l| l < line);
        i.checked_sub(1).and_then(|i| self.lines.get(i).copied())
    }

    /// 落在 `range` 内的命中数。用来在滚动条上标密度、或显示「本屏 N 处」。
    pub fn count_in(&self, range: Range<u64>) -> usize {
        if range.start >= range.end {
            return 0;
        }
        let a = self.lines.partition_point(|&l| l < range.start);
        let b = self.lines.partition_point(|&l| l < range.end);
        b - a
    }
}

/// 一段连续的可见行，以及它前面被跳过了多少行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// 文件行区间 `[start, end)`
    pub start: u64,
    pub end: u64,
    /// 与上一段之间被省略掉的行数。第一段则是文件开头被省略的行数。
    pub gap_before: u64,
}

impl Segment {
    pub fn len(&self) -> u64 {
        self.end - self.start
    }
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// 命中行 + 上下文，合并成不重叠的区间。
#[derive(Debug, Default, Clone)]
pub struct ContextMap {
    segments: Vec<Segment>,
    /// `prefix[i]` = 前 `i` 段的总行数，`prefix[len]` = 总行数
    prefix: Vec<u64>,
}

impl ContextMap {
    /// `context` 是每个命中行前后各带几行。`total_lines` 用来夹住文件末尾。
    pub fn build(matches: &[u64], context: u64, total_lines: u64) -> ContextMap {
        let mut segments: Vec<Segment> = Vec::new();
        let mut prev_end = 0u64; // 上一段的 end，用来算 gap

        for &m in matches {
            if m >= total_lines {
                break;
            }
            let start = m.saturating_sub(context);
            let end = (m + context + 1).min(total_lines);

            match segments.last_mut() {
                // 和上一段挨着或重叠就并进去。相邻命中一合并，区间数就掉下来了
                Some(last) if start <= last.end => {
                    last.end = last.end.max(end);
                }
                _ => {
                    segments.push(Segment {
                        start,
                        end,
                        gap_before: start - prev_end,
                    });
                }
            }
            prev_end = segments.last().map(|s| s.end).unwrap_or(0);
        }

        let mut prefix = Vec::with_capacity(segments.len() + 1);
        let mut acc = 0u64;
        prefix.push(0);
        for s in &segments {
            acc += s.len();
            prefix.push(acc);
        }
        ContextMap { segments, prefix }
    }

    pub fn total_rows(&self) -> u64 {
        self.prefix.last().copied().unwrap_or(0)
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// 视图行 → 文件行。
    pub fn row_to_line(&self, row: u64) -> Option<u64> {
        let i = self.segment_of_row(row)?;
        Some(self.segments[i].start + (row - self.prefix[i]))
    }

    /// 视图行 → 段下标。
    pub fn segment_of_row(&self, row: u64) -> Option<usize> {
        if row >= self.total_rows() {
            return None;
        }
        // prefix 升序，找最后一个 <= row 的
        let i = self.prefix.partition_point(|&p| p <= row) - 1;
        Some(i)
    }

    /// 这一行是不是某段的第一行——UI 据此在上面画「跳过 N 行」的分隔条。
    pub fn row_starts_segment(&self, row: u64) -> Option<Segment> {
        let i = self.segment_of_row(row)?;
        (self.prefix[i] == row).then(|| self.segments[i])
    }

    /// 文件行 → 视图行。不在任何段里时返回它之后最近的那一行。
    pub fn line_to_row(&self, line: u64) -> u64 {
        // 第一个 end > line 的段
        let i = self.segments.partition_point(|s| s.end <= line);
        match self.segments.get(i) {
            None => self.total_rows(),
            Some(s) if line < s.start => self.prefix[i], // 落在空隙里，取段首
            Some(s) => self.prefix[i] + (line - s.start),
        }
    }
}

/// 当前视图用哪种行映射。
#[derive(Clone)]
pub enum RowIndex {
    /// 显示全部。视图行 == 文件行。
    All { total: u64 },
    /// 仅显示命中行，无上下文。视图行 == 命中集下标，最省内存。
    Matches(Arc<Matches>),
    /// 命中行 + 上下文。
    Context(Arc<ContextMap>),
}

impl RowIndex {
    pub fn rows(&self) -> u64 {
        match self {
            RowIndex::All { total } => *total,
            RowIndex::Matches(m) => m.len() as u64,
            RowIndex::Context(c) => c.total_rows(),
        }
    }

    pub fn row_to_line(&self, row: u64) -> Option<u64> {
        match self {
            RowIndex::All { total } => (row < *total).then_some(row),
            RowIndex::Matches(m) => m.nth(row as usize),
            RowIndex::Context(c) => c.row_to_line(row),
        }
    }

    /// 文件行 → 视图行。目标行不可见时给它之后最近的一行。
    pub fn line_to_row(&self, line: u64) -> u64 {
        match self {
            RowIndex::All { total } => line.min(*total),
            RowIndex::Matches(m) => match m.ordinal(line) {
                Ok(i) => i as u64,
                Err(i) => i as u64,
            },
            RowIndex::Context(c) => c.line_to_row(line),
        }
    }

    /// 视图里的行是不是连续的。连续时渲染可以一次锚点定位后顺序扫，快得多。
    pub fn is_contiguous(&self) -> bool {
        matches!(self, RowIndex::All { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(v: &[u64]) -> Matches {
        Matches::new(v.to_vec())
    }

    // ---- Matches ----

    #[test]
    fn navigation_next_prev() {
        let m = m(&[10, 20, 30]);
        assert_eq!(m.next_after(0), Some(10));
        assert_eq!(m.next_after(10), Some(20), "严格之后，不含自己");
        assert_eq!(m.next_after(19), Some(20));
        assert_eq!(m.next_after(30), None);

        assert_eq!(m.prev_before(30), Some(20));
        assert_eq!(m.prev_before(21), Some(20));
        assert_eq!(m.prev_before(10), None);
        assert_eq!(m.prev_before(u64::MAX), Some(30));
    }

    #[test]
    fn navigation_on_empty() {
        let m = Matches::default();
        assert_eq!(m.next_after(0), None);
        assert_eq!(m.prev_before(100), None);
        assert_eq!(m.count_in(0..1000), 0);
    }

    #[test]
    fn ordinal_and_count() {
        let m = m(&[10, 20, 30]);
        assert_eq!(m.ordinal(20), Ok(1));
        assert_eq!(m.ordinal(25), Err(2));
        assert_eq!(m.count_in(10..31), 3);
        assert_eq!(m.count_in(11..30), 1);
        assert_eq!(m.count_in(0..10), 0);
        assert_eq!(m.count_in(5..5), 0);
    }

    // ---- ContextMap ----

    #[test]
    fn context_zero_is_one_row_per_match() {
        let c = ContextMap::build(&[5, 9], 0, 100);
        assert_eq!(c.total_rows(), 2);
        assert_eq!(c.row_to_line(0), Some(5));
        assert_eq!(c.row_to_line(1), Some(9));
        assert_eq!(c.row_to_line(2), None);
    }

    #[test]
    fn context_expands_around_match() {
        let c = ContextMap::build(&[10], 2, 100);
        assert_eq!(
            c.segments(),
            &[Segment {
                start: 8,
                end: 13,
                gap_before: 8
            }]
        );
        assert_eq!(c.total_rows(), 5);
        assert_eq!(c.row_to_line(0), Some(8));
        assert_eq!(c.row_to_line(4), Some(12));
    }

    #[test]
    fn overlapping_contexts_merge() {
        // 10 和 13 的 ±2 上下文重叠，应并成 8..16
        let c = ContextMap::build(&[10, 13], 2, 100);
        assert_eq!(c.segments().len(), 1);
        assert_eq!(
            c.segments()[0],
            Segment {
                start: 8,
                end: 16,
                gap_before: 8
            }
        );
        assert_eq!(c.total_rows(), 8);
    }

    #[test]
    fn adjacent_contexts_merge() {
        // 10 的 ±1 = 9..12，14 的 ±1 = 13..16。start(13) > end(12)，不合并
        let c = ContextMap::build(&[10, 14], 1, 100);
        assert_eq!(c.segments().len(), 2);
        // 12 的 ±1 = 11..14，正好接上 9..12 → 合并
        let c = ContextMap::build(&[10, 12], 1, 100);
        assert_eq!(c.segments().len(), 1);
        assert_eq!(
            c.segments()[0],
            Segment {
                start: 9,
                end: 14,
                gap_before: 9
            }
        );
    }

    #[test]
    fn gap_before_counts_skipped_lines() {
        let c = ContextMap::build(&[10, 50], 2, 100);
        assert_eq!(c.segments()[0].gap_before, 8, "开头跳过 0..8");
        assert_eq!(c.segments()[1].gap_before, 35, "13..48 被跳过");
    }

    #[test]
    fn clamped_at_file_edges() {
        let c = ContextMap::build(&[0, 99], 5, 100);
        assert_eq!(c.segments()[0].start, 0, "不能是负数");
        assert_eq!(c.segments()[0].gap_before, 0);
        assert_eq!(c.segments().last().unwrap().end, 100, "不能超过总行数");
    }

    #[test]
    fn matches_beyond_total_are_dropped() {
        let c = ContextMap::build(&[5, 500], 0, 100);
        assert_eq!(c.total_rows(), 1);
    }

    #[test]
    fn row_starts_segment_marks_dividers() {
        let c = ContextMap::build(&[10, 50], 2, 100);
        assert!(c.row_starts_segment(0).is_some());
        assert!(c.row_starts_segment(1).is_none());
        // 第一段 5 行，所以第二段从第 5 行开始
        assert_eq!(c.row_starts_segment(5).map(|s| s.gap_before), Some(35));
    }

    #[test]
    fn row_to_line_round_trips() {
        let c = ContextMap::build(&[10, 13, 50, 80], 2, 100);
        for row in 0..c.total_rows() {
            let line = c.row_to_line(row).unwrap();
            assert_eq!(c.line_to_row(line), row, "row {row} 往返不一致");
        }
    }

    #[test]
    fn line_to_row_in_gap_returns_next_segment() {
        let c = ContextMap::build(&[10, 50], 2, 100);
        // 30 落在 13..48 的空隙里，应返回第二段段首
        assert_eq!(c.line_to_row(30), 5);
        assert_eq!(c.row_to_line(5), Some(48));
    }

    #[test]
    fn line_to_row_past_end() {
        let c = ContextMap::build(&[10], 2, 100);
        assert_eq!(c.line_to_row(99), c.total_rows());
    }

    #[test]
    fn empty_matches_yield_empty_map() {
        let c = ContextMap::build(&[], 3, 100);
        assert_eq!(c.total_rows(), 0);
        assert_eq!(c.row_to_line(0), None);
        assert_eq!(c.line_to_row(50), 0);
    }

    /// 密集命中时区间必须合并，否则内存会炸。
    #[test]
    fn dense_matches_collapse_into_few_segments() {
        let all: Vec<u64> = (0..10_000).collect();
        let c = ContextMap::build(&all, 3, 10_000);
        assert_eq!(c.segments().len(), 1, "连续命中应合并成一段");
        assert_eq!(c.total_rows(), 10_000);
    }

    // ---- RowIndex ----

    #[test]
    fn row_index_all() {
        let r = RowIndex::All { total: 100 };
        assert_eq!(r.rows(), 100);
        assert_eq!(r.row_to_line(42), Some(42));
        assert_eq!(r.row_to_line(100), None);
        assert_eq!(r.line_to_row(42), 42);
        assert!(r.is_contiguous());
    }

    #[test]
    fn row_index_matches_keeps_file_line_numbers() {
        let r = RowIndex::Matches(Arc::new(m(&[7, 42, 99])));
        assert_eq!(r.rows(), 3);
        assert_eq!(r.row_to_line(1), Some(42), "视图第 1 行是文件第 42 行");
        assert_eq!(r.line_to_row(42), 1);
        assert_eq!(r.line_to_row(50), 2, "不是命中行时取之后最近的");
        assert!(!r.is_contiguous());
    }

    #[test]
    fn row_index_context() {
        let r = RowIndex::Context(Arc::new(ContextMap::build(&[10], 1, 100)));
        assert_eq!(r.rows(), 3);
        assert_eq!(r.row_to_line(0), Some(9));
        assert_eq!(r.row_to_line(1), Some(10));
        assert_eq!(r.row_to_line(2), Some(11));
    }
}
