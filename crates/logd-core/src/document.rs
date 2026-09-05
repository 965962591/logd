//! 一个打开的日志文件的完整可视状态：数据源 + 行索引 + 过滤器 + 命中集 + 视口。
//!
//! 刻意不依赖 gpui：产出的是纯 [`RenderRow`]，由 UI 层翻译成元素。
//! 这样「第 N 行该显示什么、行号是几、哪段要高亮」这套最容易出错的逻辑可以单测。
//!
//! 「行」有两个坐标系，混了就会显示错行号：
//! - **文件行号**（`file_line`）：文件里的真实行号，0-based。行号槽里显示的是这个 +1。
//! - **视图行号**（`row`）：当前列表里的第几项。显示全部时等于文件行号；
//!   仅显示筛选结果时是 `matches` 数组的下标。

use std::sync::Arc;

use crate::index::LineIndex;
use crate::matcher::{MatcherSet, Span};
use crate::render::{prepare_line, prepare_plain, DEFAULT_MAX_RENDER_BYTES};
use crate::source::{Encoding, FileSource};
use crate::viewport::{ScrollTo, Viewport};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderRow {
    /// 文件里的真实行号，0-based。
    pub file_line: u64,
    pub text: String,
    pub truncated: bool,
    /// 命中的行模式过滤器下标 → 整行上色。
    pub line_filter: Option<usize>,
    /// 字段模式命中区间，坐标已经是 `text` 的字节偏移且落在 char 边界。
    pub spans: Vec<Span>,
}

pub struct Document {
    source: Arc<FileSource>,
    index: Arc<LineIndex>,
    matcher: Arc<MatcherSet>,
    /// `Some` = 已经跑过筛选；`None` = 还没筛过。
    matches: Option<Arc<Vec<u64>>>,
    /// UI 的「仅显示筛选结果」开关。
    show_only_filtered: bool,
    viewport: Viewport,
    max_render_bytes: usize,
    /// 生效的编码。`FileSource` 在 `Arc` 里改不动，所以用户手动切换的编码存这儿。
    encoding: Encoding,
}

impl Document {
    pub fn new(source: Arc<FileSource>, index: Arc<LineIndex>, line_height: f32) -> Self {
        let encoding = source.encoding();
        let mut d = Self {
            source,
            index,
            matcher: Arc::new(MatcherSet::empty()),
            matches: None,
            show_only_filtered: false,
            viewport: Viewport::new(line_height),
            max_render_bytes: DEFAULT_MAX_RENDER_BYTES,
            encoding,
        };
        d.sync_viewport();
        d
    }

    #[inline]
    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// 手动切换编码。行索引不受影响——菜单中的编码都是 ASCII 兼容的字节流，
    /// `b'\n'` 的位置不变；但关键字要按新编码重新编码，所以命中集作废。
    pub fn set_encoding(&mut self, enc: Encoding) {
        if self.encoding != enc {
            self.encoding = enc;
            self.matches = None;
            self.sync_viewport();
        }
    }

    // ---- 状态读取 ----

    pub fn source(&self) -> &Arc<FileSource> {
        &self.source
    }

    pub fn index(&self) -> &Arc<LineIndex> {
        &self.index
    }

    pub fn matcher(&self) -> &Arc<MatcherSet> {
        &self.matcher
    }

    pub fn viewport(&self) -> &Viewport {
        &self.viewport
    }

    pub fn viewport_mut(&mut self) -> &mut Viewport {
        &mut self.viewport
    }

    pub fn total_file_lines(&self) -> u64 {
        self.index.total_lines
    }

    /// 索引是否已覆盖整个文件。false 时行数显示应带 `+`。
    pub fn index_complete(&self) -> bool {
        self.index.complete
    }

    pub fn match_count(&self) -> Option<usize> {
        self.matches.as_ref().map(|m| m.len())
    }

    pub fn show_only_filtered(&self) -> bool {
        self.show_only_filtered
    }

    /// 当前视图里有多少项。
    pub fn display_rows(&self) -> u64 {
        match self.active_matches() {
            Some(m) => m.len() as u64,
            None => self.index.total_lines,
        }
    }

    /// 真正生效的命中集：只有开了「仅显示筛选」且确实筛过，才用它。
    fn active_matches(&self) -> Option<&Arc<Vec<u64>>> {
        if self.show_only_filtered {
            self.matches.as_ref()
        } else {
            None
        }
    }

    // ---- 状态更新 ----

    /// 后台全量索引完成。
    pub fn set_index(&mut self, index: Arc<LineIndex>) {
        self.index = index;
        self.sync_viewport();
    }

    /// 过滤器变了：命中集立即作废，等新的扫描结果。
    pub fn set_matcher(&mut self, matcher: Arc<MatcherSet>) {
        self.matcher = matcher;
        self.matches = None;
        self.sync_viewport();
    }

    pub fn set_matches(&mut self, matches: Option<Arc<Vec<u64>>>) {
        self.matches = matches;
        self.sync_viewport();
    }

    /// 切换「仅显示筛选结果」。切换时保持当前顶端所在的**文件行**不动，
    /// 否则用户一按开关视图就跳到不知哪里去了。
    pub fn set_show_only_filtered(&mut self, on: bool) {
        if self.show_only_filtered == on {
            return;
        }
        let keep = self.top_file_line();
        self.show_only_filtered = on;
        self.sync_viewport();
        if let Some(line) = keep {
            self.goto_file_line(line, ScrollTo::Top);
        }
    }

    pub fn set_max_render_bytes(&mut self, n: usize) {
        self.max_render_bytes = n.max(16);
    }

    fn sync_viewport(&mut self) {
        let n = self.display_rows();
        self.viewport.set_total_lines(n);
    }

    // ---- 坐标换算 ----

    /// 视图行号 → 文件行号。
    pub fn row_to_file_line(&self, row: u64) -> Option<u64> {
        match self.active_matches() {
            Some(m) => m.get(row as usize).copied(),
            None => (row < self.index.total_lines).then_some(row),
        }
    }

    /// 文件行号 → 视图行号。筛选视图里目标行没被选中时，返回它之后最近的一行。
    pub fn file_line_to_row(&self, file_line: u64) -> u64 {
        match self.active_matches() {
            Some(m) => m.partition_point(|&l| l < file_line) as u64,
            None => file_line,
        }
    }

    pub fn top_file_line(&self) -> Option<u64> {
        self.row_to_file_line(self.viewport.anchor_line())
    }

    /// Decode one file line without changing the viewport. UI-only edit and
    /// copy operations use this to keep their buffers proportional to the
    /// user's selection rather than to the mapped file size.
    pub fn line_text(&self, file_line: u64) -> Option<String> {
        let mut line_buf = Vec::with_capacity(1);
        self.index
            .line_spans(self.source.data(), file_line, 1, &mut line_buf);
        let &(start, end) = line_buf.first()?;
        let mut start = start as usize;
        if start == 0 {
            start = self.source.bom_len().min(end as usize);
        }
        Some(
            prepare_plain(
                &self.source.data()[start..end as usize],
                self.encoding,
                self.max_render_bytes,
            )
            .text,
        )
    }

    /// 按**文件行号**跳转，两种视图下都好用。
    pub fn goto_file_line(&mut self, file_line: u64, how: ScrollTo) {
        let row = self.file_line_to_row(file_line);
        self.viewport.scroll_to_line(row, how);
    }

    // ---- 渲染 ----

    /// 当前视口应该画的那些行。每帧调用。
    pub fn rows(&self) -> Vec<RenderRow> {
        let count = self.viewport.visible_rows();
        if count == 0 {
            return Vec::new();
        }
        let first = self.viewport.anchor_line();
        let mut out = Vec::with_capacity(count);
        let mut line_buf = Vec::with_capacity(count);
        let mut spans = Vec::new();

        match self.active_matches() {
            // 筛选视图：命中行号是跳跃的，只能逐行定位
            Some(m) => {
                let start = first as usize;
                for &file_line in m.iter().skip(start).take(count) {
                    self.index
                        .line_spans(self.source.data(), file_line, 1, &mut line_buf);
                    if let Some(&(s, e)) = line_buf.first() {
                        out.push(self.make_row(file_line, s, e, &mut spans));
                    }
                }
            }
            // 全部视图：一次锚点定位后顺序扫，最省
            None => {
                self.index
                    .line_spans(self.source.data(), first, count, &mut line_buf);
                for (i, &(s, e)) in line_buf.iter().enumerate() {
                    out.push(self.make_row(first + i as u64, s, e, &mut spans));
                }
            }
        }
        out
    }

    fn make_row(&self, file_line: u64, start: u64, end: u64, spans: &mut Vec<Span>) -> RenderRow {
        let data = self.source.data();
        let mut s = start as usize;
        // 第一行的原始切片含 BOM，别把它画出来
        if s == 0 {
            s = self.source.bom_len().min(end as usize);
        }
        let raw = &data[s..end as usize];
        let enc = self.encoding;

        if self.matcher.is_noop() {
            let line = prepare_plain(raw, enc, self.max_render_bytes);
            return RenderRow {
                file_line,
                text: line.text,
                truncated: line.truncated,
                line_filter: None,
                spans: Vec::new(),
            };
        }

        let verdict = self.matcher.analyze(raw, spans);
        let line = prepare_line(raw, enc, spans, self.max_render_bytes);
        RenderRow {
            file_line,
            text: line.text,
            truncated: line.truncated,
            line_filter: verdict.line_filter,
            spans: std::mem::take(spans),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::{FilterSpec, HighlightMode};
    use crate::progress::Progress;
    use crate::source::Encoding;

    /// 造一个不落盘的 Document。
    fn doc_from(text: &str, line_height: f32, height: f32) -> Document {
        let src = Arc::new(FileSource::from_bytes_for_test(
            text.as_bytes().to_vec(),
            Encoding::Utf8,
        ));
        let index = Arc::new(LineIndex::build_full(src.data(), &Progress::default()).unwrap());
        let mut d = Document::new(src, index, line_height);
        d.viewport_mut().set_height(height);
        d
    }

    fn sample() -> Document {
        let mut s = String::new();
        for i in 0..1000 {
            if i % 10 == 0 {
                s.push_str(&format!("{i:04} TARGET payload\n"));
            } else {
                s.push_str(&format!("{i:04} filler payload\n"));
            }
        }
        // 行高 10、视口 50 → 每屏 5 行
        doc_from(&s, 10.0, 50.0)
    }

    fn set_filter(d: &mut Document, f: FilterSpec) {
        d.set_matcher(Arc::new(MatcherSet::new(vec![f], Encoding::Utf8).unwrap()));
    }

    fn run_scan(d: &mut Document) {
        let out = crate::scan::scan_all(
            d.source().data(),
            d.index(),
            d.matcher(),
            &Progress::default(),
        )
        .unwrap();
        match out {
            crate::scan::ScanOutcome::Matched(v) => d.set_matches(Some(Arc::new(v))),
            crate::scan::ScanOutcome::AllVisible => d.set_matches(None),
        }
    }

    #[test]
    fn rows_start_at_viewport_anchor() {
        let d = sample();
        let rows = d.rows();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].file_line, 0);
        assert_eq!(rows[0].text, "0000 TARGET payload");
        assert_eq!(rows[4].file_line, 4);
    }

    #[test]
    fn rows_follow_scroll() {
        let mut d = sample();
        d.viewport_mut().scroll_to_line(100, ScrollTo::Top);
        let rows = d.rows();
        assert_eq!(rows[0].file_line, 100);
        assert_eq!(rows[0].text, "0100 TARGET payload");
    }

    #[test]
    fn line_text_decodes_without_moving_viewport() {
        let d = sample();
        assert_eq!(d.line_text(10).as_deref(), Some("0010 TARGET payload"));
        assert_eq!(d.viewport().anchor_line(), 0);
        assert_eq!(d.line_text(1000), None);
    }

    #[test]
    fn display_rows_is_file_lines_when_unfiltered() {
        let d = sample();
        assert_eq!(d.display_rows(), 1000);
        assert_eq!(d.viewport().total_lines(), 1000);
    }

    #[test]
    fn filtered_view_shows_only_matches_but_keeps_file_line_numbers() {
        let mut d = sample();
        set_filter(
            &mut d,
            FilterSpec {
                text: "TARGET".into(),
                ..Default::default()
            },
        );
        run_scan(&mut d);
        d.set_show_only_filtered(true);

        assert_eq!(d.display_rows(), 100);
        assert_eq!(d.viewport().total_lines(), 100);

        let rows = d.rows();
        assert_eq!(rows.len(), 5);
        // 关键：行号槽里必须是原始文件行号，不是 0..5
        assert_eq!(
            rows.iter().map(|r| r.file_line).collect::<Vec<_>>(),
            vec![0, 10, 20, 30, 40]
        );
    }

    #[test]
    fn toggling_filter_view_keeps_top_file_line() {
        let mut d = sample();
        set_filter(
            &mut d,
            FilterSpec {
                text: "TARGET".into(),
                ..Default::default()
            },
        );
        run_scan(&mut d);

        d.viewport_mut().scroll_to_line(500, ScrollTo::Top);
        assert_eq!(d.top_file_line(), Some(500));

        d.set_show_only_filtered(true);
        // 500 是命中行（500 % 10 == 0），应正好停在它上面
        assert_eq!(d.top_file_line(), Some(500));

        d.set_show_only_filtered(false);
        assert_eq!(d.top_file_line(), Some(500));
    }

    #[test]
    fn toggling_from_unmatched_line_lands_on_next_match() {
        let mut d = sample();
        set_filter(
            &mut d,
            FilterSpec {
                text: "TARGET".into(),
                ..Default::default()
            },
        );
        run_scan(&mut d);
        d.viewport_mut().scroll_to_line(503, ScrollTo::Top);
        d.set_show_only_filtered(true);
        assert_eq!(d.top_file_line(), Some(510), "该落到之后最近的命中行");
    }

    #[test]
    fn goto_file_line_works_in_filtered_view() {
        let mut d = sample();
        set_filter(
            &mut d,
            FilterSpec {
                text: "TARGET".into(),
                ..Default::default()
            },
        );
        run_scan(&mut d);
        d.set_show_only_filtered(true);
        d.goto_file_line(300, ScrollTo::Top);
        assert_eq!(d.rows()[0].file_line, 300);
    }

    #[test]
    fn line_mode_filter_marks_whole_row() {
        let mut d = sample();
        set_filter(
            &mut d,
            FilterSpec {
                text: "TARGET".into(),
                mode: HighlightMode::Line,
                ..Default::default()
            },
        );
        let rows = d.rows();
        assert_eq!(rows[0].line_filter, Some(0));
        assert!(rows[0].spans.is_empty());
        assert_eq!(rows[1].line_filter, None, "没命中的行不该上色");
    }

    #[test]
    fn field_mode_filter_yields_spans_on_matched_row_only() {
        let mut d = sample();
        set_filter(
            &mut d,
            FilterSpec {
                text: "TARGET".into(),
                mode: HighlightMode::Field,
                ..Default::default()
            },
        );
        let rows = d.rows();
        assert_eq!(rows[0].line_filter, None);
        assert_eq!(rows[0].spans.len(), 1);
        assert_eq!(
            &rows[0].text[rows[0].spans[0].start..rows[0].spans[0].end],
            "TARGET"
        );
        assert!(rows[1].spans.is_empty(), "span 不该串到下一行");
    }

    #[test]
    fn changing_filter_invalidates_matches() {
        let mut d = sample();
        set_filter(
            &mut d,
            FilterSpec {
                text: "TARGET".into(),
                ..Default::default()
            },
        );
        run_scan(&mut d);
        d.set_show_only_filtered(true);
        assert_eq!(d.display_rows(), 100);

        set_filter(
            &mut d,
            FilterSpec {
                text: "filler".into(),
                ..Default::default()
            },
        );
        assert_eq!(d.match_count(), None, "换了过滤器旧命中集必须作废");
        assert_eq!(d.display_rows(), 1000, "没有命中集时回退到显示全部");
    }

    #[test]
    fn growing_index_extends_display_rows() {
        let mut s = String::new();
        for i in 0..1000 {
            s.push_str(&format!("{i:04} payload\n"));
        }
        let src = Arc::new(FileSource::from_bytes_for_test(
            s.into_bytes(),
            Encoding::Utf8,
        ));
        // 先只索引头部
        let head = Arc::new(LineIndex::build_head(src.data(), 1000));
        let mut d = Document::new(src.clone(), head, 10.0);
        d.viewport_mut().set_height(50.0);
        assert!(!d.index_complete());
        let partial = d.display_rows();
        assert!(partial > 0 && partial < 1000);

        let full = Arc::new(LineIndex::build_full(src.data(), &Progress::default()).unwrap());
        d.set_index(full);
        assert!(d.index_complete());
        assert_eq!(d.display_rows(), 1000);
    }

    #[test]
    fn bom_is_not_rendered() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"first line\nsecond\n");
        let src = Arc::new(FileSource::from_bytes_for_test(bytes, Encoding::Utf8));
        let index = Arc::new(LineIndex::build_full(src.data(), &Progress::default()).unwrap());
        let mut d = Document::new(src, index, 10.0);
        d.viewport_mut().set_height(50.0);
        let rows = d.rows();
        assert_eq!(rows[0].text, "first line", "BOM 不该出现在正文里");
    }

    #[test]
    fn empty_file_renders_nothing() {
        let d = doc_from("", 10.0, 50.0);
        assert_eq!(d.display_rows(), 0);
        assert!(d.rows().is_empty());
    }

    #[test]
    fn last_page_does_not_overrun() {
        let mut d = sample();
        d.viewport_mut().scroll_to_bottom();
        let rows = d.rows();
        assert_eq!(rows.last().unwrap().file_line, 999);
        assert!(rows.len() <= 6);
    }

    #[test]
    fn long_line_is_truncated() {
        let mut s = "x".repeat(10_000);
        s.push('\n');
        let mut d = doc_from(&s, 10.0, 50.0);
        d.set_max_render_bytes(100);
        let rows = d.rows();
        assert!(rows[0].truncated);
        assert!(rows[0].text.chars().count() <= 101);
    }
}
