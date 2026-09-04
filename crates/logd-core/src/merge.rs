//! 多个日志文件按时间戳归并成一个视图。
//!
//! # 为什么只归并筛选结果
//!
//! 4 个 50GB 文件全量归并是 20 亿条记录，每条至少 (来源, 行号) 12 字节 = 24GB，
//! 物化不了；而随机访问第 N 行又必须知道前面每个文件各贡献了多少行，绕不开物化。
//!
//! 所以归并**只作用在筛选结果上**：先各自筛（`AeAlgo` 之类），再把命中按时间穿插。
//! 这正是对比两台设备日志时的实际用法，命中集通常是几千到几百万条，完全放得下。
//!
//! # 时间戳从哪来
//!
//! 扫描时顺手记下命中行的时间戳（[`TimedLine`]，16 字节），避免归并时再回文件里
//! 随机读几百万次。代价是命中集内存翻倍，[`MERGE_ROWS_WARN`] 给出提示阈值。

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::logline::Ts;

/// 归并结果超过这个量级就该提示用户收紧条件了（约 1.6GB）。
pub const MERGE_ROWS_WARN: usize = 100_000_000;

/// 一条带时间戳的命中行。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TimedLine {
    pub ts: Ts,
    pub line: u64,
}

/// 归并后的一行：来自哪个文件的哪一行。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MergedRow {
    /// 在传入的 sources 数组里的下标
    pub source: u16,
    pub line: u64,
}

/// 堆里的元素。按 (时间, 来源下标, 行号) 排序，保证结果**稳定且确定**——
/// 同一时刻的多条记录，顺序不会因为堆的实现细节而抖动。
#[derive(PartialEq, Eq)]
struct HeapItem {
    ts: Ts,
    source: u16,
    line: u64,
    /// 在该来源里的下标
    idx: usize,
}

impl Ord for HeapItem {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.ts
            .cmp(&o.ts)
            .then(self.source.cmp(&o.source))
            .then(self.line.cmp(&o.line))
    }
}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}

/// 归并后的视图。视图行 → (来源, 文件行)。
#[derive(Debug, Default, Clone)]
pub struct MergedView {
    rows: Vec<MergedRow>,
}

impl MergedView {
    /// k 路归并。每个 `sources[i]` 必须按时间升序（扫描天然就是按行号顺序，
    /// 而 logcat 的时间基本随行号单调，不满足时结果仍然可用只是不完美有序）。
    pub fn build(sources: &[&[TimedLine]]) -> MergedView {
        let total: usize = sources.iter().map(|s| s.len()).sum();
        let mut rows = Vec::with_capacity(total);
        // k 很小（几个文件），用小根堆
        let mut heap: BinaryHeap<Reverse<HeapItem>> = BinaryHeap::with_capacity(sources.len());

        for (si, s) in sources.iter().enumerate() {
            if let Some(first) = s.first() {
                heap.push(Reverse(HeapItem {
                    ts: first.ts,
                    source: si as u16,
                    line: first.line,
                    idx: 0,
                }));
            }
        }

        while let Some(Reverse(it)) = heap.pop() {
            rows.push(MergedRow {
                source: it.source,
                line: it.line,
            });
            let next = it.idx + 1;
            if let Some(n) = sources[it.source as usize].get(next) {
                heap.push(Reverse(HeapItem {
                    ts: n.ts,
                    source: it.source,
                    line: n.line,
                    idx: next,
                }));
            }
        }

        MergedView { rows }
    }

    pub fn rows(&self) -> u64 {
        self.rows.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn get(&self, row: u64) -> Option<MergedRow> {
        self.rows.get(row as usize).copied()
    }

    pub fn as_slice(&self) -> &[MergedRow] {
        &self.rows
    }

    /// 某个来源的某一行落在归并视图的第几行。线性查找，只给「跳回原文件位置」用，
    /// 不在热路径上。
    pub fn find(&self, source: u16, line: u64) -> Option<u64> {
        self.rows
            .iter()
            .position(|r| r.source == source && r.line == line)
            .map(|i| i as u64)
    }

    /// 估算内存占用，UI 可据此提示用户。
    pub fn heap_bytes(&self) -> usize {
        self.rows.len() * std::mem::size_of::<MergedRow>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(ts: i64, line: u64) -> TimedLine {
        TimedLine { ts: Ts(ts), line }
    }

    fn merged(v: &MergedView) -> Vec<(u16, u64)> {
        v.as_slice().iter().map(|r| (r.source, r.line)).collect()
    }

    #[test]
    fn empty_input() {
        let v = MergedView::build(&[]);
        assert!(v.is_empty());
        assert_eq!(v.get(0), None);
    }

    #[test]
    fn single_source_passes_through() {
        let a = [t(10, 0), t(20, 5), t(30, 9)];
        let v = MergedView::build(&[&a]);
        assert_eq!(merged(&v), vec![(0, 0), (0, 5), (0, 9)]);
    }

    #[test]
    fn two_sources_interleave_by_time() {
        let a = [t(10, 0), t(30, 1), t(50, 2)];
        let b = [t(20, 7), t(40, 8)];
        let v = MergedView::build(&[&a, &b]);
        assert_eq!(
            merged(&v),
            vec![(0, 0), (1, 7), (0, 1), (1, 8), (0, 2)],
            "应严格按时间穿插"
        );
    }

    #[test]
    fn ties_are_deterministic_by_source_then_line() {
        let a = [t(10, 5)];
        let b = [t(10, 1)];
        let c = [t(10, 9)];
        let v = MergedView::build(&[&a, &b, &c]);
        assert_eq!(
            merged(&v),
            vec![(0, 5), (1, 1), (2, 9)],
            "同一时刻按来源下标排，结果必须可复现"
        );
    }

    #[test]
    fn one_empty_source_is_skipped() {
        let a = [t(10, 0), t(20, 1)];
        let b: [TimedLine; 0] = [];
        let v = MergedView::build(&[&a, &b]);
        assert_eq!(merged(&v), vec![(0, 0), (0, 1)]);
    }

    #[test]
    fn all_of_one_source_before_the_other() {
        let a = [t(1, 0), t(2, 1)];
        let b = [t(100, 0), t(200, 1)];
        let v = MergedView::build(&[&a, &b]);
        assert_eq!(merged(&v), vec![(0, 0), (0, 1), (1, 0), (1, 1)]);
    }

    #[test]
    fn four_sources_stay_sorted() {
        let src: Vec<Vec<TimedLine>> = (0..4)
            .map(|s| (0..50).map(|i| t(i * 4 + s, i as u64)).collect())
            .collect();
        let refs: Vec<&[TimedLine]> = src.iter().map(|v| v.as_slice()).collect();
        let v = MergedView::build(&refs);
        assert_eq!(v.rows(), 200);

        // 把归并结果的时间戳取回来，必须非递减
        let mut prev = i64::MIN;
        for r in v.as_slice() {
            let ts = src[r.source as usize]
                .iter()
                .find(|x| x.line == r.line)
                .unwrap()
                .ts
                .0;
            assert!(ts >= prev, "归并结果不是有序的");
            prev = ts;
        }
    }

    #[test]
    fn find_locates_row() {
        let a = [t(10, 0), t(30, 1)];
        let b = [t(20, 7)];
        let v = MergedView::build(&[&a, &b]);
        assert_eq!(v.find(1, 7), Some(1));
        assert_eq!(v.find(0, 1), Some(2));
        assert_eq!(v.find(1, 99), None);
    }

    #[test]
    fn row_lookup_is_bounds_checked() {
        let a = [t(10, 0)];
        let v = MergedView::build(&[&a]);
        assert!(v.get(0).is_some());
        assert_eq!(v.get(1), None);
        assert_eq!(v.get(u64::MAX), None);
    }

    #[test]
    fn duplicate_timestamps_within_one_source_keep_line_order() {
        let a = [t(10, 0), t(10, 1), t(10, 2)];
        let v = MergedView::build(&[&a]);
        assert_eq!(merged(&v), vec![(0, 0), (0, 1), (0, 2)]);
    }
}
