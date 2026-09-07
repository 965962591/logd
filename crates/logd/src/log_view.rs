//! 日志视口：自建滚动 + 自绘滚动条 + 行渲染 + 后台索引/筛选。
//!
//! **为什么不用 `uniform_list` / `virtual_list`**：gpui 的 `Pixels` 是 `f32`。
//! 5 亿行 × 18px = 9e9 px，f32 尾数 24 位，超过约 1.67e7 px（≈93 万行）滚动偏移
//! 就开始丢精度、抖动、跳行。任何「以像素总高为基准」的虚拟列表在这个量级都不可用。
//!
//! 滚动位置由 [`logd_core::Viewport`] 维护成 `(anchor_line: u64, pixel_offset: f32)`，
//! 比例运算全走 f64，只在最后一步落到像素。滚动数学在 `logd-core` 里有单测。

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::input::{Copy, Input, InputEvent, InputState};
use gpui_component::GlobalState;
use gpui_component::Sizable as _;
use logd_core::{
    cache, index::HEAD_BYTES, scan_all_with_query_and_counts_for_filters, scan_query_all,
    CompileOptions, Document, Encoding, FileSource, FilterScanResult, FilterSpec, LineIndex,
    MatcherSet, Progress, Query, RenderRow, ScanOutcome, ScrollTo,
};

use crate::theme;

const MIN_FONT_SIZE: f32 = 8.0;
const MAX_FONT_SIZE: f32 = 32.0;
const LINE_HEIGHT_PADDING: f32 = 5.0;
const SEARCH_RESULT_FLASH_DURATION: Duration = Duration::from_millis(1_200);

/// 打开文件的**易错部分**：mmap + 编码探测 + 首屏索引。
///
/// 和构造视图分开，是因为 `cx.new()` 的闭包必须返回 `Self` 而不是 `Result<Self>`。
pub struct Loaded {
    source: Arc<FileSource>,
    index: Arc<LineIndex>,
    /// 索引是不是从磁盘缓存直接读出来的（省掉一整趟扫描）
    from_cache: bool,
}

pub struct LogView {
    doc: Document,
    focus: FocusHandle,
    /// 视口在窗口里的矩形。由 `canvas` 在 prepaint 阶段回填，render 读上一帧的值。
    area: Rc<Cell<Bounds<Pixels>>>,
    last_area: Bounds<Pixels>,
    /// 正在拖滚动条：记录抓取点在滑块内的偏移。
    drag_grab: Option<f32>,
    /// 正在拖动横向滚动条滑块。
    h_drag_grab: Option<f32>,
    indexing: Option<Arc<Progress>>,
    scanning: Option<Arc<Progress>>,
    filter_match_counts: Option<Arc<Vec<u64>>>,
    multi_file_filter_mask: Arc<Vec<bool>>,
    multi_file_filter_matches: Option<Arc<Vec<u64>>>,
    search_query: Arc<Query>,
    search_matches: Option<Arc<Vec<u64>>>,
    search_scanning: Option<Arc<Progress>>,
    search_result_flash: Option<(u64, u64)>,
    search_result_flash_epoch: u64,
    search_result_flash_task: Option<Task<()>>,
    /// 每次发起筛选自增。回调里对不上就说明结果已经过期，直接丢弃。
    scan_gen: u64,
    search_scan_gen: u64,
    /// 过滤器变了但这个标签页还没重扫（非活动标签页先记账，切过去再扫）
    dirty: bool,
    /// 正则编译失败之类的提示
    error: Option<String>,
    from_cache: bool,
    selection: Option<LineSelection>,
    selecting: bool,
    text_selection: Option<TextSelection>,
    text_selecting: bool,
    edits: BTreeMap<u64, String>,
    /// 修改是否已经写入编辑副本。保存后仍保留编辑内容用于显示。
    edits_saved: bool,
    /// 当前已绘制行中观测到的最长内容宽度，避免滚动后横向范围跳变。
    max_line_width: f32,
    editing_line: Option<u64>,
    /// 每个可见文件行对应一个输入状态。只保留视口附近的状态，避免把整个大文件
    /// 物化进输入控件，同时让 InputState 接管光标、选择、复制和编辑交互。
    line_inputs: BTreeMap<u64, Entity<InputState>>,
    active_input_line: Option<u64>,
    editing_changed: bool,
    font_size: f32,
    line_height: f32,
}

#[derive(Clone, Copy)]
struct LineSelection {
    anchor_row: u64,
    active_row: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TextPoint {
    view_row: u64,
    byte: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TextSelection {
    anchor: TextPoint,
    active: TextPoint,
}

impl LineSelection {
    fn range(self) -> std::ops::RangeInclusive<u64> {
        self.anchor_row.min(self.active_row)..=self.anchor_row.max(self.active_row)
    }
}

impl TextSelection {
    fn ordered(self) -> (TextPoint, TextPoint) {
        if self.anchor <= self.active {
            (self.anchor, self.active)
        } else {
            (self.active, self.anchor)
        }
    }

    fn is_empty(self) -> bool {
        self.anchor == self.active
    }

    fn range_for_row(self, view_row: u64, line_len: usize) -> Option<std::ops::Range<usize>> {
        let (start, end) = self.ordered();
        if !(start.view_row..=end.view_row).contains(&view_row) {
            return None;
        }
        if start.view_row == end.view_row {
            return Some(start.byte.min(line_len)..end.byte.min(line_len));
        }

        let range = if view_row == start.view_row {
            start.byte.min(line_len)..line_len
        } else if view_row == end.view_row {
            0..end.byte.min(line_len)
        } else {
            0..line_len
        };
        Some(range)
    }
}

impl LogView {
    /// 打开文件但先不建视图。失败在这一步暴露。
    pub fn load(path: &Path) -> Result<Loaded> {
        let source = Arc::new(FileSource::open(path)?);
        // M5：命中索引缓存就整趟扫描都省了，50GB 二次打开 <1s
        if let Ok(Some(cached)) = cache::load(path) {
            return Ok(Loaded {
                source,
                index: Arc::new(cached),
                from_cache: true,
            });
        }
        // 阶段 A：只索引头部，先把首屏顶出来
        let index = Arc::new(LineIndex::build_head(source.data(), HEAD_BYTES));
        Ok(Loaded {
            source,
            index,
            from_cache: false,
        })
    }

    pub fn new(loaded: Loaded, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let complete = loaded.index.complete;
        let search_query = Arc::new(Query::always_true());
        let mut view = Self {
            doc: Document::new(loaded.source, loaded.index, theme::LINE_HEIGHT),
            focus: cx.focus_handle(),
            area: Rc::new(Cell::new(Bounds::default())),
            last_area: Bounds::default(),
            drag_grab: None,
            h_drag_grab: None,
            indexing: None,
            scanning: None,
            filter_match_counts: None,
            multi_file_filter_mask: Arc::new(Vec::new()),
            multi_file_filter_matches: None,
            search_query,
            search_matches: None,
            search_scanning: None,
            search_result_flash: None,
            search_result_flash_epoch: 0,
            search_result_flash_task: None,
            scan_gen: 0,
            search_scan_gen: 0,
            dirty: false,
            error: None,
            from_cache: loaded.from_cache,
            selection: None,
            selecting: false,
            text_selection: None,
            text_selecting: false,
            edits: BTreeMap::new(),
            edits_saved: true,
            max_line_width: 0.0,
            editing_line: None,
            line_inputs: BTreeMap::new(),
            active_input_line: None,
            editing_changed: false,
            font_size: theme::FONT_SIZE,
            line_height: theme::LINE_HEIGHT,
        };
        window.focus(&view.focus, cx);
        // 阶段 B：文件没索引完就丢到后台跑全量
        if !complete {
            view.start_full_index(cx);
        }
        view
    }

    // ---- 只读状态，给状态栏用 ----

    pub fn doc(&self) -> &Document {
        &self.doc
    }

    pub fn indexing_progress(&self) -> Option<f32> {
        self.indexing.as_ref().map(|p| p.fraction())
    }

    pub fn scanning_progress(&self) -> Option<f32> {
        self.scanning.as_ref().map(|p| p.fraction())
    }

    pub fn filter_match_counts(&self) -> Option<Arc<Vec<u64>>> {
        self.filter_match_counts.clone()
    }

    pub fn has_multi_file_filter_results(&self) -> bool {
        self.multi_file_filter_mask
            .iter()
            .zip(self.doc.matcher().filters())
            .any(|(selected, filter)| *selected && !filter.excluding)
    }

    pub fn multi_file_filter_matches(&self) -> Option<Arc<Vec<u64>>> {
        self.multi_file_filter_matches.clone()
    }

    pub fn search_scanning_progress(&self) -> Option<f32> {
        self.search_scanning.as_ref().map(|p| p.fraction())
    }

    pub fn search_matches(&self) -> Option<Arc<Vec<u64>>> {
        self.search_matches.clone()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn from_cache(&self) -> bool {
        self.from_cache
    }

    // ---- 后台索引 ----

    fn start_full_index(&mut self, cx: &mut Context<Self>) {
        let source = self.doc.source().clone();
        let progress = Arc::new(Progress::new(source.len()));
        self.indexing = Some(progress.clone());

        let for_task = progress.clone();
        cx.spawn(async move |this, cx| {
            let path = source.path().to_path_buf();
            let built = cx
                .background_executor()
                .spawn(async move {
                    let idx = LineIndex::build_full(source.data(), &for_task);
                    // 顺手落盘，下次打开就不用再扫了。写失败不影响功能。
                    if let Some(i) = idx.as_ref() {
                        let _ = cache::store(&path, i);
                    }
                    idx
                })
                .await;
            this.update(cx, |this, cx| {
                if let Some(index) = built {
                    this.doc.set_index(Arc::new(index));
                    // 之前基于部分索引扫出来的命中集不完整，重扫
                    if !this.doc.matcher().is_noop() {
                        this.start_scan(cx);
                    }
                    if !this.search_query.is_empty() {
                        this.start_search_scan(cx);
                    }
                }
                this.indexing = None;
                cx.notify();
            })
            .ok();
        })
        .detach();

        self.poll_while_busy(cx);
    }

    /// 后台任务跑着的时候 30Hz 刷一次，进度和行数才会动。
    fn poll_while_busy(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(33))
                .await;
            let busy = this
                .update(cx, |this, cx| {
                    cx.notify();
                    this.indexing.is_some()
                        || this.scanning.is_some()
                        || this.search_scanning.is_some()
                })
                .unwrap_or(false);
            if !busy {
                break;
            }
        })
        .detach();
    }

    // ---- 筛选 ----

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn can_close_without_prompt(&self) -> bool {
        !self.editing_changed && (self.edits.is_empty() || self.edits_saved)
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// 换一套过滤器。非活动标签页可以先 [`mark_dirty`]，切过去时再调这个。
    pub fn apply_filters(
        &mut self,
        filters: Vec<FilterSpec>,
        configured_filter_count: usize,
        cx: &mut Context<Self>,
    ) {
        self.dirty = false;
        self.filter_match_counts = None;
        self.multi_file_filter_mask = Arc::new(
            filters
                .iter()
                .enumerate()
                .map(|(index, filter)| {
                    index < configured_filter_count
                        && filter.is_active()
                        && filter.scope == logd_core::FilterScope::AllFiles
                })
                .collect(),
        );
        self.multi_file_filter_matches = None;
        match MatcherSet::new(filters, self.doc.encoding()) {
            Ok(m) => {
                self.error = None;
                self.clear_text_selection(cx);
                self.selection = None;
                self.selecting = false;
                self.doc.set_matcher(Arc::new(m));
                self.start_scan(cx);
            }
            Err(e) => {
                self.error = Some(format!("{e:#}"));
                cx.notify();
            }
        }
    }

    /// Replace presentation-only filter colors without rescanning the file.
    pub fn restyle_filters(&mut self, filters: Vec<FilterSpec>, cx: &mut Context<Self>) {
        match MatcherSet::new(filters, self.doc.encoding()) {
            Ok(matcher) => {
                self.error = None;
                self.doc.set_matcher(Arc::new(matcher));
                cx.notify();
            }
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                cx.notify();
            }
        }
    }

    pub fn set_show_only_filtered(&mut self, on: bool, cx: &mut Context<Self>) {
        self.clear_text_selection(cx);
        self.selection = None;
        self.selecting = false;
        self.doc.set_show_only_filtered(on);
        cx.notify();
    }

    pub fn set_show_only_filtered_at_pointer(
        &mut self,
        on: bool,
        pointer_y: f32,
        cx: &mut Context<Self>,
    ) {
        if !self.doc.show_only_filtered() || on {
            self.set_show_only_filtered(on, cx);
            return;
        }

        let anchor = self.doc.viewport().anchor_line();
        let pixel_offset = self.doc.viewport().pixel_offset();
        let row_to_anchor = |row| {
            self.doc
                .row_to_file_line(row)
                .map(|file_line| (file_line, row.saturating_sub(anchor)))
        };
        let pointer_line = self
            .row_at_y(pointer_y)
            .and_then(row_to_anchor)
            .or_else(|| {
                self.active_input_line
                    .filter(|file_line| self.line_inputs.contains_key(file_line))
                    .map(|file_line| {
                        let row = self.doc.file_line_to_row(file_line);
                        (file_line, row.saturating_sub(anchor))
                    })
            })
            .or_else(|| {
                self.selection
                    .and_then(|selection| row_to_anchor(selection.active_row))
            });

        self.clear_text_selection(cx);
        self.selection = None;
        self.selecting = false;
        self.doc.set_show_only_filtered(on);
        if let Some((file_line, row_offset)) = pointer_line {
            let new_row = self.doc.file_line_to_row(file_line);
            let new_anchor = new_row.saturating_sub(row_offset);
            let viewport = self.doc.viewport_mut();
            viewport.scroll_to_line(new_anchor, ScrollTo::Top);
            viewport.scroll_by_pixels(pixel_offset);
        }
        cx.notify();
    }

    pub fn set_encoding(
        &mut self,
        enc: Encoding,
        filters: Vec<FilterSpec>,
        configured_filter_count: usize,
        search_query: String,
        cx: &mut Context<Self>,
    ) {
        self.doc.set_encoding(enc);
        // 关键字要按新编码重新编码成字节串，matcher 必须重建
        self.apply_filters(filters, configured_filter_count, cx);
        self.apply_search(search_query, cx);
    }

    pub fn apply_search(&mut self, query_source: String, cx: &mut Context<Self>) {
        // Use the first structured log timestamp as the date context for
        // time-of-day queries. This keeps searches on logs with an explicit
        // year aligned with the same file's timestamps.
        let base_date = self
            .doc
            .source()
            .data()
            .split(|byte| *byte == b'\n')
            .take(64)
            .find_map(|line| {
                let line = line.strip_suffix(&[b'\r']).unwrap_or(line);
                let line = line.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(line);
                logd_core::logline::parse(line)
            })
            .and_then(|line| line.ts);
        let query = Query::parse(
            &query_source,
            CompileOptions {
                base_date,
                encoding: Some(self.doc.encoding()),
                ..Default::default()
            },
        );
        match query {
            Ok(query) => {
                self.search_query = Arc::new(query);
                self.start_scan(cx);
                self.start_search_scan(cx);
            }
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                cx.notify();
            }
        }
    }

    fn start_scan(&mut self, cx: &mut Context<Self>) {
        // 上一轮还在跑就叫停，它的结果会被 scan_gen 挡掉
        if let Some(p) = self.scanning.take() {
            p.cancel();
        }
        self.filter_match_counts = None;
        self.multi_file_filter_matches = None;
        if self.doc.matcher().is_noop() && self.search_query.is_empty() {
            self.filter_match_counts = Some(Arc::new(vec![0; self.doc.matcher().filters().len()]));
            self.doc.set_matches(None);
            cx.notify();
            return;
        }

        self.scan_gen += 1;
        let gen = self.scan_gen;
        let source = self.doc.source().clone();
        let index = self.doc.index().clone();
        let matcher = self.doc.matcher().clone();
        let multi_file_filter_mask = self.multi_file_filter_mask.clone();
        let query = self.search_query.clone();
        let progress = Arc::new(Progress::new(index.indexed_bytes.max(1)));
        self.scanning = Some(progress.clone());

        cx.spawn(async move |this, cx| {
            let out = cx
                .background_executor()
                .spawn(async move {
                    scan_all_with_query_and_counts_for_filters(
                        source.data(),
                        &index,
                        &matcher,
                        &query,
                        multi_file_filter_mask.as_slice(),
                        &progress,
                    )
                })
                .await;
            this.update(cx, |this, cx| {
                // 结果过期了（用户又改了过滤器）就丢掉
                if this.scan_gen != gen {
                    return;
                }
                match out {
                    Some(FilterScanResult {
                        outcome,
                        filter_counts,
                        selected_filter_lines,
                    }) => {
                        this.filter_match_counts = Some(Arc::new(filter_counts));
                        this.multi_file_filter_matches = this
                            .has_multi_file_filter_results()
                            .then(|| Arc::new(selected_filter_lines));
                        match outcome {
                            ScanOutcome::Matched(lines) => {
                                this.doc.set_matches(Some(Arc::new(lines)))
                            }
                            ScanOutcome::AllVisible => this.doc.set_matches(None),
                        }
                    }
                    None => {} // 被取消
                }
                this.scanning = None;
                cx.notify();
            })
            .ok();
        })
        .detach();

        self.poll_while_busy(cx);
    }

    fn start_search_scan(&mut self, cx: &mut Context<Self>) {
        if let Some(progress) = self.search_scanning.take() {
            progress.cancel();
        }
        self.search_matches = None;
        self.search_scan_gen += 1;
        let generation = self.search_scan_gen;
        if self.search_query.is_empty() {
            cx.notify();
            return;
        }

        let source = self.doc.source().clone();
        let index = self.doc.index().clone();
        let query = self.search_query.clone();
        let progress = Arc::new(Progress::new(index.indexed_bytes.max(1)));
        self.search_scanning = Some(progress.clone());

        cx.spawn(async move |this, cx| {
            let out = cx
                .background_executor()
                .spawn(async move { scan_query_all(source.data(), &index, &query, &progress) })
                .await;
            this.update(cx, |this, cx| {
                if this.search_scan_gen != generation {
                    return;
                }
                match out {
                    Some(ScanOutcome::Matched(lines)) => {
                        this.search_matches = Some(Arc::new(lines));
                    }
                    Some(ScanOutcome::AllVisible) => this.search_matches = None,
                    None => {}
                }
                this.search_scanning = None;
                cx.notify();
            })
            .ok();
        })
        .detach();

        self.poll_while_busy(cx);
    }

    pub fn goto_file_line(&mut self, file_line: u64, cx: &mut Context<Self>) {
        self.clear_text_selection(cx);
        self.selection = None;
        self.selecting = false;
        self.doc.goto_file_line(file_line, ScrollTo::Center);
        cx.notify();
    }

    pub fn reveal_search_result(&mut self, file_line: u64, cx: &mut Context<Self>) {
        self.goto_file_line(file_line, cx);
        self.search_result_flash_epoch = self.search_result_flash_epoch.wrapping_add(1);
        let epoch = self.search_result_flash_epoch;
        self.search_result_flash = Some((file_line, epoch));
        self.search_result_flash_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(SEARCH_RESULT_FLASH_DURATION)
                .await;
            this.update(cx, |this, cx| {
                if this.search_result_flash == Some((file_line, epoch)) {
                    this.search_result_flash = None;
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    // ---- 输入 ----

    pub fn selected_text(&self, cx: &App) -> Option<String> {
        if let Some(selection) = self
            .text_selection
            .filter(|selection| !selection.is_empty())
        {
            let (start, end) = selection.ordered();
            let mut output = String::new();
            for view_row in start.view_row..=end.view_row {
                let Some(file_line) = self.doc.row_to_file_line(view_row) else {
                    continue;
                };
                let text = self
                    .edits
                    .get(&file_line)
                    .cloned()
                    .or_else(|| self.doc.line_text(file_line))
                    .unwrap_or_default();
                let Some(range) = selection.range_for_row(view_row, text.len()) else {
                    continue;
                };
                if view_row != start.view_row {
                    output.push('\n');
                }
                output.push_str(&text[range]);
            }
            return (!output.is_empty()).then_some(output);
        }

        if let Some(input) = self
            .active_input_line
            .and_then(|file_line| self.line_inputs.get(&file_line))
        {
            let input = input.read(cx);
            let range = input.selected_range();
            if !range.is_empty() {
                return Some(input.selected_value().to_string());
            }
        }

        let Some(selection) = self.selection else {
            return None;
        };
        let mut output = String::new();
        for row in selection.range() {
            let Some(file_line) = self.doc.row_to_file_line(row) else {
                continue;
            };
            if !output.is_empty() {
                output.push('\n');
            }
            if let Some(edited) = self.edits.get(&file_line) {
                output.push_str(edited);
            } else if let Some(text) = self.doc.line_text(file_line) {
                output.push_str(&text);
            }
        }
        (!output.is_empty()).then_some(output)
    }

    pub fn copy_selection(&self, cx: &mut App) {
        if let Some(output) = self.selected_text(cx) {
            cx.write_to_clipboard(output.into());
        }
    }

    fn clear_text_selection(&mut self, cx: &mut Context<Self>) {
        self.text_selection = None;
        self.text_selecting = false;
        let inputs = self.line_inputs.values().cloned().collect::<Vec<_>>();
        for input in inputs {
            let state = input.read(cx);
            let range = state.selected_range();
            let cursor = state.cursor();
            if !range.is_empty() {
                input.update(cx, |state, cx| state.set_selected_range(cursor..cursor, cx));
            }
        }
    }

    fn discard_text_selection(&mut self) -> bool {
        let had_selection = self.text_selection.take().is_some();
        self.text_selecting = false;
        had_selection
    }

    fn apply_text_selection(&self, cx: &mut Context<Self>) {
        let Some(selection) = self.text_selection else {
            return;
        };
        let inputs = self
            .line_inputs
            .iter()
            .map(|(&file_line, input)| (file_line, input.clone()))
            .collect::<Vec<_>>();
        for (file_line, input) in inputs {
            let view_row = self.doc.file_line_to_row(file_line);
            let state = input.read(cx);
            let current = state.selected_range();
            let cursor = state.cursor();
            let desired = selection
                .range_for_row(view_row, state.value().len())
                .unwrap_or(cursor..cursor);
            if current != desired {
                input.update(cx, |state, cx| state.set_selected_range(desired, cx));
            }
        }
    }

    fn begin_text_selection(&mut self, view_row: u64, cx: &mut Context<Self>) {
        let Some(file_line) = self.doc.row_to_file_line(view_row) else {
            return;
        };
        let Some(input) = self.line_inputs.get(&file_line) else {
            return;
        };
        let state = input.read(cx);
        let range = state.selected_range();
        let active_byte = state.cursor();
        let anchor_byte = if range.is_empty() {
            active_byte
        } else if active_byte == range.start {
            range.end
        } else {
            range.start
        };

        self.selection = None;
        self.selecting = false;
        self.active_input_line = Some(file_line);
        self.text_selection = Some(TextSelection {
            anchor: TextPoint {
                view_row,
                byte: anchor_byte,
            },
            active: TextPoint {
                view_row,
                byte: active_byte,
            },
        });
        self.text_selecting = true;
        self.apply_text_selection(cx);
        cx.notify();
    }

    fn extend_text_selection(
        &mut self,
        position: Point<Pixels>,
        gutter_w: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view_row) = self.row_at_y(f32::from(position.y)) else {
            return;
        };
        let Some(file_line) = self.doc.row_to_file_line(view_row) else {
            return;
        };
        let Some(input) = self.line_inputs.get(&file_line) else {
            return;
        };
        let text = input.read(cx).value();
        let byte = if text.is_empty() {
            0
        } else {
            let run = TextRun {
                len: text.len(),
                font: font(theme::MONO),
                ..Default::default()
            };
            let local_x =
                f32::from(position.x) - f32::from(self.last_area.origin.x) - gutter_w - 8.0
                    + self.doc.viewport().h_scroll();
            window
                .text_system()
                .shape_line(text, px(self.font_size), &[run], None)
                .closest_index_for_x(px(local_x))
        };
        let Some(selection) = &mut self.text_selection else {
            return;
        };
        let active = TextPoint { view_row, byte };
        if selection.active == active {
            return;
        }
        selection.active = active;
        self.apply_text_selection(cx);
        cx.notify();
    }

    pub fn edit_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let row = self
            .selection
            .map(|selection| selection.active_row)
            .unwrap_or_else(|| self.doc.viewport().anchor_line());
        self.focus_row_input(row, window, cx);
    }

    fn focus_row_input(&mut self, row: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file_line) = self.doc.row_to_file_line(row) else {
            return;
        };
        let Some(input) = self.line_inputs.get(&file_line).cloned() else {
            return;
        };
        self.clear_text_selection(cx);
        if let Some(previous) = self.editing_line.filter(|previous| *previous != file_line) {
            self.commit_line_input(previous, cx);
        }
        self.editing_line = Some(file_line);
        self.active_input_line = Some(file_line);
        self.editing_changed = false;
        self.selection = None;
        let focus = input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn commit_line_input(&mut self, file_line: u64, cx: &mut Context<Self>) {
        let Some(input) = self.line_inputs.get(&file_line) else {
            return;
        };
        let value = input.read(cx).value().to_string();
        let original = self.doc.line_text(file_line).unwrap_or_default();
        let previous = self.edits.get(&file_line).cloned();
        if value == original {
            self.edits.remove(&file_line);
        } else {
            self.edits.insert(file_line, value);
        }
        if previous != self.edits.get(&file_line).cloned() {
            self.edits_saved = false;
        }
        self.editing_changed = false;
    }

    fn sync_line_inputs(
        &mut self,
        rows: &[RenderRow],
        h_scroll: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let visible = rows
            .iter()
            .map(|row| row.file_line)
            .collect::<BTreeSet<_>>();
        if let Some(file_line) = self.editing_line.filter(|line| !visible.contains(line)) {
            self.commit_line_input(file_line, cx);
            self.editing_line = None;
            self.active_input_line = None;
            window.focus(&self.focus, cx);
        }
        self.line_inputs
            .retain(|file_line, _| visible.contains(file_line));

        for row in rows {
            let file_line = row.file_line;
            let expected = self
                .edits
                .get(&file_line)
                .cloned()
                .unwrap_or_else(|| row.text.clone());
            if !self.line_inputs.contains_key(&file_line) {
                let input = cx.new(|cx| {
                    InputState::new(window, cx)
                        .default_value(expected.clone())
                        .context_menu(true)
                });
                cx.subscribe_in(
                    &input,
                    window,
                    move |this, _, event: &InputEvent, window, cx| match event {
                        InputEvent::Focus => {
                            if let Some(previous) =
                                this.editing_line.filter(|previous| *previous != file_line)
                            {
                                this.commit_line_input(previous, cx);
                                this.editing_line = None;
                            }
                            this.active_input_line = Some(file_line);
                            this.selection = None;
                            cx.notify();
                        }
                        InputEvent::Change if this.editing_line == Some(file_line) => {
                            this.editing_changed = true;
                            this.discard_text_selection();
                            cx.notify();
                        }
                        InputEvent::PressEnter { .. } => this.commit_edit(window, cx),
                        InputEvent::Blur if this.editing_line == Some(file_line) => {
                            this.commit_line_input(file_line, cx);
                            this.editing_line = None;
                            cx.notify();
                        }
                        _ => {}
                    },
                )
                .detach();
                self.line_inputs.insert(file_line, input);
            } else if self.editing_line != Some(file_line) {
                let input = &self.line_inputs[&file_line];
                if input.read(cx).value().as_ref() != expected {
                    input.update(cx, |state, cx| state.set_value(expected, window, cx));
                }
            }
        }

        let offset = point(px(-h_scroll), px(0.));
        for input in self.line_inputs.values() {
            if input.read(cx).scroll_offset() != offset {
                input.update(cx, |state, cx| state.set_scroll_offset(offset, cx));
            }
        }
        self.apply_text_selection(cx);
    }

    pub fn save_edited_copy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing_line.is_some() {
            self.commit_edit(window, cx);
        }
        if self.edits.is_empty() {
            return;
        }
        let source = self.doc.source().clone();
        let index = self.doc.index().clone();
        let encoding = self.doc.encoding();
        let edits = self.edits.clone();
        let saved_edits = edits.clone();
        let directory = source.path().parent().unwrap_or_else(|| Path::new("."));
        let name = source
            .path()
            .file_name()
            .map(|name| format!("{}.edited", name.to_string_lossy()))
            .unwrap_or_else(|| "log.edited".to_string());
        let target = cx.prompt_for_new_path(directory, Some(&name));
        let executor = cx.background_executor().clone();

        cx.spawn_in(window, async move |this, window| {
            let Some(target) = target.await.ok().and_then(Result::ok).flatten() else {
                return;
            };
            if target == source.path() {
                _ = window.update(|_, cx| {
                    _ = this.update(cx, |this, cx| {
                        this.error = Some("edited copy must use a new path".to_string());
                        cx.notify();
                    });
                });
                return;
            }
            let result = executor
                .spawn(async move { write_edited_copy(target, source, index, encoding, edits) })
                .await;
            _ = window.update(|_, cx| {
                _ = this.update(cx, |this, cx| {
                    this.error = result.err().map(|error| format!("{error:#}"));
                    if this.error.is_none() && this.edits == saved_edits {
                        this.edits_saved = true;
                    }
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file_line) = self.editing_line.take() else {
            return;
        };
        self.commit_line_input(file_line, cx);
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(file_line) = self.editing_line.take() {
            let value = self
                .edits
                .get(&file_line)
                .cloned()
                .or_else(|| self.doc.line_text(file_line))
                .unwrap_or_default();
            if let Some(input) = self.line_inputs.get(&file_line) {
                input.update(cx, |state, cx| state.set_value(value, window, cx));
            }
            self.editing_changed = false;
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    fn select_row(&mut self, row: u64, extend: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_text_selection(cx);
        self.active_input_line = None;
        self.selection = match (extend, self.selection) {
            (true, Some(selection)) => Some(LineSelection {
                active_row: row,
                ..selection
            }),
            _ => Some(LineSelection {
                anchor_row: row,
                active_row: row,
            }),
        };
        self.selecting = true;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn row_at_y(&self, y: f32) -> Option<u64> {
        let area = self.last_area;
        let local = y - f32::from(area.origin.y);
        if local < 0. || local > f32::from(area.size.height) {
            return None;
        }
        let visible_offset = ((local + self.doc.viewport().pixel_offset()) / self.line_height)
            .floor()
            .max(0.) as u64;
        let row = self.doc.viewport().anchor_line() + visible_offset;
        (row < self.doc.display_rows()).then_some(row)
    }

    fn extend_selection_to_y(&mut self, y: f32, cx: &mut Context<Self>) {
        let Some(row) = self.row_at_y(y) else {
            return;
        };
        if let Some(selection) = &mut self.selection {
            if selection.active_row != row {
                selection.active_row = row;
                cx.notify();
            }
        }
    }

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let lh = self.line_height;
        let (mut dx, mut dy) = match ev.delta {
            ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
            ScrollDelta::Lines(l) => (l.x * lh, l.y * lh * theme::WHEEL_LINES),
        };
        if crate::platform::primary_modifier(&ev.modifiers) {
            let step = if dy > 0.0 {
                1.0
            } else if dy < 0.0 {
                -1.0
            } else {
                0.0
            };
            if step != 0.0 {
                self.set_font_size(self.font_size + step, cx);
            }
            return;
        }
        // Windows already translates Shift+wheel into an X delta. Keep this
        // fallback for platforms that leave it as a vertical wheel event.
        if ev.modifiers.shift && dx == 0.0 {
            dx = dy;
            dy = 0.0;
        }
        // gpui 的 dy 是「内容跟随手指」的方向，视口位移要取反
        let vp = self.doc.viewport_mut();
        vp.scroll_by_pixels(-dy);
        if dx != 0.0 {
            vp.scroll_h_by(-dx);
        }
        cx.notify();
    }

    fn set_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        let size = size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        if (size - self.font_size).abs() < f32::EPSILON {
            return;
        }
        self.font_size = size;
        self.line_height = size + LINE_HEIGHT_PADDING;
        self.doc.viewport_mut().set_line_height(self.line_height);
        self.max_line_width = 0.0;
        cx.notify();
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let primary = crate::platform::primary_modifier(&ks.modifiers);
        let input_focused = self
            .active_input_line
            .and_then(|file_line| self.line_inputs.get(&file_line))
            .is_some_and(|input| input.read(cx).focus_handle(cx).is_focused(_window));
        if input_focused {
            if primary && ks.modifiers.shift && ks.key == "f" {
                // Let the app-level "selection to filter" shortcut inspect
                // the selection before ordinary input handling clears it.
                return;
            }
            if primary
                && ks.key == "c"
                && self
                    .text_selection
                    .is_some_and(|selection| !selection.is_empty())
            {
                // The focused per-row input copies only its own fragment. This
                // handler runs while the event bubbles and replaces it with the
                // complete cross-row selection.
                self.copy_selection(cx);
                return;
            }
            if ks.key == "escape" {
                self.cancel_edit(_window, cx);
                return;
            }
            if self.discard_text_selection() {
                cx.notify();
            }
            return;
        }
        if primary && ks.key == "c" {
            self.copy_selection(cx);
            return;
        }
        if ks.key == "enter" {
            self.edit_selected(_window, cx);
            return;
        }
        if ks.key == "escape" {
            self.cancel_edit(_window, cx);
            return;
        }
        let vp = self.doc.viewport_mut();
        match ks.key.as_str() {
            "up" => vp.scroll_by_lines(-1),
            "down" => vp.scroll_by_lines(1),
            "pageup" => vp.page(-1),
            "pagedown" => vp.page(1),
            "home" if primary => vp.scroll_to_top(),
            "end" if primary => vp.scroll_to_bottom(),
            "home" => vp.scroll_h_by(-f32::MAX),
            "end" => vp.scroll_h_by(f32::MAX),
            "left" => vp.scroll_h_by(-40.0),
            "right" => vp.scroll_h_by(40.0),
            _ => return,
        }
        cx.notify();
    }

    // ---- 滚动条 ----

    fn track(&self) -> (f32, f32) {
        let a = self.last_area;
        let height = (f32::from(a.size.height)
            - if self.doc.viewport().max_h_scroll() > 0.0 {
                theme::SCROLLBAR_W
            } else {
                0.0
            })
        .max(0.0);
        (f32::from(a.origin.y), height)
    }

    fn on_scrollbar_down(&mut self, y: f32, cx: &mut Context<Self>) {
        let (top, len) = self.track();
        let (thumb_off, thumb_len) = self.doc.viewport().thumb(len, theme::MIN_THUMB);
        let local = y - top;
        if local >= thumb_off && local <= thumb_off + thumb_len {
            // 抓住滑块本体，拖动时保持相对位置
            self.drag_grab = Some(local - thumb_off);
        } else {
            // 点空白轨道：滑块中心跳到点击处
            let grab = thumb_len / 2.0;
            self.drag_grab = Some(grab);
            self.doc
                .viewport_mut()
                .set_thumb_offset(local - grab, len, theme::MIN_THUMB);
        }
        cx.notify();
    }

    fn on_drag_move(&mut self, y: f32, cx: &mut Context<Self>) {
        let Some(grab) = self.drag_grab else { return };
        let (top, len) = self.track();
        self.doc
            .viewport_mut()
            .set_thumb_offset(y - top - grab, len, theme::MIN_THUMB);
        cx.notify();
    }

    fn h_track(&self, gutter_w: f32, vertical: bool) -> (f32, f32) {
        let a = self.last_area;
        let left = f32::from(a.origin.x) + gutter_w;
        let width =
            (f32::from(a.size.width) - gutter_w - if vertical { theme::SCROLLBAR_W } else { 0.0 })
                .max(0.0);
        (left, width)
    }

    fn on_hscrollbar_down(
        &mut self,
        x: f32,
        gutter_w: f32,
        vertical: bool,
        cx: &mut Context<Self>,
    ) {
        let (left, track) = self.h_track(gutter_w, vertical);
        let viewport_width = track;
        let (thumb_off, thumb_len) =
            self.doc
                .viewport()
                .h_thumb(track, viewport_width, theme::MIN_THUMB);
        let local = x - left;
        if local >= thumb_off && local <= thumb_off + thumb_len {
            self.h_drag_grab = Some(local - thumb_off);
        } else {
            let grab = thumb_len / 2.0;
            self.h_drag_grab = Some(grab);
            self.doc.viewport_mut().set_h_thumb_offset(
                local - grab,
                track,
                viewport_width,
                theme::MIN_THUMB,
            );
        }
        cx.notify();
    }

    fn on_hdrag_move(&mut self, x: f32, gutter_w: f32, vertical: bool, cx: &mut Context<Self>) {
        let Some(grab) = self.h_drag_grab else { return };
        let (left, track) = self.h_track(gutter_w, vertical);
        self.doc
            .viewport_mut()
            .set_h_thumb_offset(x - left - grab, track, track, theme::MIN_THUMB);
        cx.notify();
    }

    // ---- 渲染 ----

    fn render_row(
        &self,
        row: &RenderRow,
        view_row: u64,
        gutter_w: f32,
        h_scroll: f32,
        palette: theme::Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let filters = self.doc.matcher().filters();
        let line_spec = row.line_filter.and_then(|i| filters.get(i));
        let row_font_size = filter_font_size_for_row(row, filters);
        let selected = self
            .selection
            .is_some_and(|selection| selection.range().contains(&view_row));
        let edited = self.edits.get(&row.file_line);
        let is_editing = self.editing_line == Some(row.file_line);
        let input = self
            .line_inputs
            .get(&row.file_line)
            .expect("visible rows have synchronized InputState instances");
        let text_selection_bounds = self
            .text_selection
            .filter(|selection| !selection.is_empty())
            .and_then(|selection| {
                let text = input.read(cx).value();
                let range = selection.range_for_row(view_row, text.len())?;
                let run = TextRun {
                    len: text.len(),
                    font: font(theme::MONO),
                    ..Default::default()
                };
                let shaped = window.text_system().shape_line(
                    text,
                    px(row_font_size.unwrap_or(self.font_size)),
                    &[run],
                    None,
                );
                let start_x = f32::from(shaped.x_for_index(range.start));
                let end_x = f32::from(shaped.x_for_index(range.end));
                Some((8.0 + start_x - h_scroll, (end_x - start_x).max(1.0)))
            });

        let mut content = div()
            .flex_none()
            .ml(px(-h_scroll))
            .when_some(line_spec.and_then(|f| f.fore), |el, c| {
                el.text_color(theme::c(c))
            })
            .when(line_spec.is_some_and(|f| f.bold), |el| {
                el.font_weight(FontWeight::BOLD)
            })
            .when(line_spec.is_some_and(|f| f.italic), |el| el.italic());
        if let Some(size) = row_font_size {
            content = content.text_size(px(size));
        }

        if let Some(edited) = edited {
            content = content.child(edited.clone());
        } else if row.spans.is_empty() {
            content = content.child(row.text.clone());
        } else {
            let runs: Vec<(std::ops::Range<usize>, HighlightStyle)> = row
                .spans
                .iter()
                .filter_map(|s| {
                    let f = filters.get(s.filter)?;
                    Some((
                        s.start..s.end,
                        HighlightStyle {
                            color: f.fore.map(|c| Hsla::from(theme::c(c))),
                            background_color: f.back.map(|c| Hsla::from(theme::c(c))),
                            font_weight: f.bold.then_some(FontWeight::BOLD),
                            font_style: f.italic.then_some(FontStyle::Italic),
                            ..Default::default()
                        },
                    ))
                })
                .collect();
            content = content.child(StyledText::new(row.text.clone()).with_highlights(runs));
        }

        let mut input = Input::new(input)
            .xsmall()
            .appearance(false)
            .bordered(false)
            .focus_bordered(false)
            .readonly(!is_editing)
            .px_0()
            .text_size(px(row_font_size.unwrap_or(self.font_size)))
            .line_height(px(
                row_font_size.map_or(self.line_height, |size| size + LINE_HEIGHT_PADDING)
            ));
        input.style().size.height = Some(relative(1.).into());
        let input = if is_editing {
            input
        } else {
            // The normal rendering underneath retains field-level highlight
            // spans. This transparent InputState layer supplies native caret,
            // selection and clipboard behavior without painting duplicate text.
            input.text_color(transparent_black())
        };
        let line_content = div()
            .relative()
            .flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .pl_2()
            .overflow_hidden()
            .when_some(text_selection_bounds, |el, (left, width)| {
                el.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(left))
                        .w(px(width))
                        .bg(palette.selection),
                )
            })
            .when(!is_editing, |el| {
                el.child(
                    div()
                        .absolute()
                        .top_0()
                        .right_0()
                        .bottom_0()
                        .left(px(8.))
                        .flex()
                        .items_center()
                        .whitespace_nowrap()
                        .child(content),
                )
            })
            .child(input);
        let line_content = if let Some((_, epoch)) = self
            .search_result_flash
            .filter(|(file_line, _)| *file_line == row.file_line)
        {
            let flash = palette.search_foreground;
            line_content
                .with_animation(
                    ElementId::NamedInteger("search-result-flash".into(), epoch),
                    Animation::new(SEARCH_RESULT_FLASH_DURATION),
                    move |line, phase| {
                        let fade = if phase < 0.2 {
                            1.0
                        } else {
                            1.0 - (phase - 0.2) / 0.8
                        };
                        line.bg(flash.opacity(0.45 * fade.clamp(0.0, 1.0)))
                    },
                )
                .into_any_element()
        } else {
            line_content.into_any_element()
        };

        div()
            .id(("log-row", view_row as usize))
            .flex()
            .flex_row()
            .h(px(self.line_height))
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .when_some(line_spec.and_then(|f| f.back), |el, c| el.bg(theme::c(c)))
            .when(
                selected && line_spec.and_then(|filter| filter.back).is_none(),
                |el| el.bg(palette.selection),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    let in_content = f32::from(event.position.x)
                        >= f32::from(this.last_area.origin.x) + gutter_w;
                    if in_content {
                        this.begin_text_selection(view_row, cx);
                        return;
                    }
                    if this.editing_line.is_some() {
                        this.commit_edit(window, cx);
                    }
                    GlobalState::init(cx);
                    GlobalState::suppress_text_selection(cx);
                    this.select_row(view_row, event.modifiers.shift, window, cx);
                }),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .w(px(gutter_w))
                    .pr_2()
                    .justify_end()
                    .bg(palette.gutter)
                    .text_color(palette.muted)
                    // 行号槽显示的永远是**文件行号**，筛选视图下也不变
                    .child(format!("{}", row.file_line + 1)),
            )
            .child(line_content)
            .into_any_element()
    }

    /// 行号槽宽度按总行数的位数算，别让它随滚动跳来跳去。
    fn gutter_width(&self) -> f32 {
        let digits = (self.doc.total_file_lines().max(1) as f64).log10().floor() as usize + 1;
        (digits.max(4) as f32) * (self.font_size * 0.62) + 20.0
    }
}

fn write_edited_copy(
    target: PathBuf,
    source: Arc<FileSource>,
    index: Arc<LineIndex>,
    encoding: Encoding,
    edits: BTreeMap<u64, String>,
) -> anyhow::Result<()> {
    let mut output = std::io::BufWriter::new(std::fs::File::create(&target)?);
    let data = source.data();
    let mut cursor = 0usize;
    let mut spans = Vec::with_capacity(1);
    for (file_line, text) in edits {
        index.line_spans(data, file_line, 1, &mut spans);
        let Some(&(start, end)) = spans.first() else {
            continue;
        };
        let mut start = start as usize;
        if start == 0 {
            start = source.bom_len().min(end as usize);
        }
        output.write_all(&data[cursor..start])?;
        output.write_all(&logd_core::matcher::encode_pattern(&text, encoding))?;
        cursor = end as usize;
    }
    output.write_all(&data[cursor..])?;
    output.flush()?;
    Ok(())
}

impl Focusable for LogView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn filter_font_size_for_row(row: &RenderRow, filters: &[FilterSpec]) -> Option<f32> {
    row.line_filter
        .and_then(|index| filters.get(index))
        .and_then(|filter| filter.font_size)
        .or_else(|| {
            row.spans
                .iter()
                .find_map(|span| filters.get(span.filter)?.font_size)
        })
        .map(f32::from)
}

impl Render for LogView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = theme::palette(cx);
        // 上一帧 canvas 量到的尺寸；首帧是 0，canvas 回填后会触发再画一次
        let area = self.area.get();
        if area != self.last_area {
            self.last_area = area;
            self.doc
                .viewport_mut()
                .set_height(f32::from(area.size.height));
        }

        let gutter_w = self.gutter_width();
        let configured_line_height = self
            .doc
            .matcher()
            .filters()
            .iter()
            .filter(|filter| filter.is_active())
            .filter_map(|filter| filter.font_size)
            .map(f32::from)
            .fold(self.font_size + LINE_HEIGHT_PADDING, |height, size| {
                height.max(size + LINE_HEIGHT_PADDING)
            });
        if (configured_line_height - self.line_height).abs() > f32::EPSILON {
            self.line_height = configured_line_height;
            self.doc
                .viewport_mut()
                .set_line_height(configured_line_height);
        }
        let rows = self.doc.rows();
        let editing_line = self.editing_line;
        let editing_text = editing_line
            .and_then(|file_line| self.line_inputs.get(&file_line))
            .map(|input| input.read(cx).value().to_string());
        let estimated_width = rows
            .iter()
            .map(|row| {
                let text = if editing_line == Some(row.file_line) {
                    editing_text.as_deref().unwrap_or(&row.text)
                } else {
                    self.edits
                        .get(&row.file_line)
                        .map(String::as_str)
                        .unwrap_or(&row.text)
                };
                let font_size = filter_font_size_for_row(row, self.doc.matcher().filters())
                    .unwrap_or(self.font_size);
                text.chars()
                    .map(|ch| if ch.is_ascii() { 0.62 } else { 1.0 })
                    .sum::<f32>()
                    * font_size
            })
            .fold(0.0, f32::max);
        // Rendered text has the same left padding as `line_content` below.
        self.max_line_width = self.max_line_width.max(estimated_width + 8.0);
        let full_height = f32::from(area.size.height);
        self.doc.viewport_mut().set_height(full_height);
        let vertical_thumb_len = self.doc.viewport().thumb(full_height, theme::MIN_THUMB).1;
        let show_vertical = self.doc.display_rows() > 0 && vertical_thumb_len < full_height;
        let content_viewport_width = (f32::from(area.size.width)
            - gutter_w
            - if show_vertical {
                theme::SCROLLBAR_W
            } else {
                0.0
            })
        .max(0.0);
        self.doc
            .viewport_mut()
            .set_max_h_scroll((self.max_line_width - content_viewport_width).max(0.0));
        let show_horizontal =
            self.doc.viewport().h_scroll() > 0.0 || self.doc.viewport().max_h_scroll() > 0.0;
        let viewport_height = (full_height
            - if show_horizontal {
                theme::SCROLLBAR_W
            } else {
                0.0
            })
        .max(0.0);
        self.doc.viewport_mut().set_height(viewport_height);
        let (thumb_off, thumb_len) = self.doc.viewport().thumb(viewport_height, theme::MIN_THUMB);
        let (h_track_left, h_track_len) = self.h_track(gutter_w, show_vertical);
        let (h_thumb_off, h_thumb_len) =
            self.doc
                .viewport()
                .h_thumb(h_track_len, h_track_len, theme::MIN_THUMB);
        let vp = self.doc.viewport();
        let h_scroll = vp.h_scroll();
        let pixel_offset = vp.pixel_offset();
        let first_row = vp.anchor_line();
        self.sync_line_inputs(&rows, h_scroll, window, cx);
        let rendered_rows = rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                self.render_row(
                    row,
                    first_row + index as u64,
                    gutter_w,
                    h_scroll,
                    palette,
                    window,
                    cx,
                )
            })
            .collect::<Vec<_>>();

        let bounds_sink = self.area.clone();
        let handle = cx.entity().downgrade();
        let drag_handle = cx.entity().downgrade();

        div()
            .id("log-view")
            .key_context("LogView")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .overflow_hidden()
            .cursor_default()
            .bg(palette.background)
            .text_color(palette.foreground)
            .font_family(theme::MONO)
            .text_size(px(self.font_size))
            .line_height(px(self.line_height))
            .capture_action(cx.listener(|this, _: &Copy, _window, cx| {
                if this
                    .text_selection
                    .is_some_and(|selection| !selection.is_empty())
                {
                    this.copy_selection(cx);
                    cx.stop_propagation();
                }
            }))
            .on_key_down(cx.listener(Self::on_key))
            .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, window, cx| {
                if this.drag_grab.is_some() || this.h_drag_grab.is_some() {
                    return;
                }
                if this.text_selecting && ev.pressed_button == Some(MouseButton::Left) {
                    this.extend_text_selection(ev.position, gutter_w, window, cx);
                } else if this.selecting && ev.pressed_button == Some(MouseButton::Left) {
                    this.extend_selection_to_y(f32::from(ev.position.y), cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _ev: &MouseUpEvent, _w, _cx| {
                    this.selecting = false;
                    this.text_selecting = false;
                }),
            )
            // 量视口尺寸：prepaint 回填，尺寸真变了才 notify，避免每帧重画
            .child(
                canvas(
                    move |bounds, window, _cx| {
                        if bounds_sink.get() != bounds {
                            bounds_sink.set(bounds);
                            let handle = handle.clone();
                            // Rows are computed during render from the previous measured bounds.
                            // Notify after this frame finishes so the invalidation cannot be
                            // consumed by the frame that performed the measurement.
                            window.on_next_frame(move |_, cx| {
                                handle.update(cx, |_, cx| cx.notify()).ok();
                            });
                        }
                    },
                    move |bounds, _, window, _| {
                        window.on_mouse_event({
                            let scroll_handle = drag_handle.clone();
                            move |ev: &ScrollWheelEvent, phase, window, cx| {
                                if phase != DispatchPhase::Capture || !bounds.contains(&ev.position)
                                {
                                    return;
                                }
                                if scroll_handle
                                    .update(cx, |this, cx| this.on_scroll(ev, window, cx))
                                    .is_ok()
                                {
                                    // Per-row InputState elements also register scroll
                                    // listeners. Handle the log viewport first so native
                                    // horizontal-wheel events cannot be consumed by a row.
                                    cx.stop_propagation();
                                }
                            }
                        });
                        window.on_mouse_event({
                            let drag_handle = drag_handle.clone();
                            move |ev: &MouseMoveEvent, phase, _, cx| {
                                if phase != DispatchPhase::Capture
                                    || ev.pressed_button != Some(MouseButton::Left)
                                {
                                    return;
                                }
                                drag_handle
                                    .update(cx, |this, cx| {
                                        if this.h_drag_grab.is_some() {
                                            this.on_hdrag_move(
                                                f32::from(ev.position.x),
                                                gutter_w,
                                                show_vertical,
                                                cx,
                                            );
                                        } else if this.drag_grab.is_some() {
                                            this.on_drag_move(f32::from(ev.position.y), cx);
                                        }
                                    })
                                    .ok();
                            }
                        });
                        window.on_mouse_event({
                            let drag_handle = drag_handle.clone();
                            move |ev: &MouseUpEvent, phase, _, cx| {
                                if phase != DispatchPhase::Capture || ev.button != MouseButton::Left
                                {
                                    return;
                                }
                                drag_handle
                                    .update(cx, |this, cx| {
                                        let was_dragging = this.drag_grab.take().is_some();
                                        let was_h_dragging = this.h_drag_grab.take().is_some();
                                        if was_dragging || was_h_dragging {
                                            cx.notify();
                                        }
                                    })
                                    .ok();
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            )
            .child(
                div()
                    .absolute()
                    .top(px(-pixel_offset))
                    .left_0()
                    .right_0()
                    .flex()
                    .flex_col()
                    .children(rendered_rows),
            )
            .when(show_vertical, |el| {
                el.child(
                    div()
                        .id("vscrollbar")
                        .absolute()
                        .top_0()
                        .bottom(if show_horizontal {
                            px(theme::SCROLLBAR_W)
                        } else {
                            px(0.)
                        })
                        .right_0()
                        .w(px(theme::SCROLLBAR_W))
                        .cursor_default()
                        .group("log-vscrollbar")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, ev: &MouseDownEvent, _w, cx| {
                                cx.stop_propagation();
                                this.on_scrollbar_down(f32::from(ev.position.y), cx);
                            }),
                        )
                        .child(
                            div()
                                .absolute()
                                .top(px(thumb_off))
                                .right(px(4.))
                                .w(px(4.))
                                .h(px(thumb_len))
                                .rounded_sm()
                                .bg(if self.drag_grab.is_some() {
                                    palette.scroll_thumb_hover
                                } else {
                                    palette.scroll_thumb
                                })
                                .group_hover("log-vscrollbar", |style| {
                                    style.right(px(2.)).w(px(8.)).bg(palette.scroll_thumb_hover)
                                }),
                        ),
                )
            })
            .when(show_horizontal, |el| {
                el.child(
                    div()
                        .id("hscrollbar")
                        .absolute()
                        .left(px(h_track_left - f32::from(area.origin.x)))
                        .right(if show_vertical {
                            px(theme::SCROLLBAR_W)
                        } else {
                            px(0.)
                        })
                        .bottom_0()
                        .h(px(theme::SCROLLBAR_W))
                        .cursor_default()
                        .group("log-hscrollbar")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, ev: &MouseDownEvent, _w, cx| {
                                cx.stop_propagation();
                                this.on_hscrollbar_down(
                                    f32::from(ev.position.x),
                                    gutter_w,
                                    show_vertical,
                                    cx,
                                );
                            }),
                        )
                        .child(
                            div()
                                .absolute()
                                .left(px(h_thumb_off))
                                .bottom(px(4.))
                                .h(px(4.))
                                .w(px(h_thumb_len))
                                .rounded_sm()
                                .bg(if self.h_drag_grab.is_some() {
                                    palette.scroll_thumb_hover
                                } else {
                                    palette.scroll_thumb
                                })
                                .group_hover("log-hscrollbar", |style| {
                                    style
                                        .bottom(px(2.))
                                        .h(px(8.))
                                        .bg(palette.scroll_thumb_hover)
                                }),
                        ),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{TextPoint, TextSelection};

    fn selection(anchor: (u64, usize), active: (u64, usize)) -> TextSelection {
        TextSelection {
            anchor: TextPoint {
                view_row: anchor.0,
                byte: anchor.1,
            },
            active: TextPoint {
                view_row: active.0,
                byte: active.1,
            },
        }
    }

    #[test]
    fn text_selection_splits_across_rows() {
        let selection = selection((2, 3), (4, 2));

        assert_eq!(selection.range_for_row(1, 10), None);
        assert_eq!(selection.range_for_row(2, 10), Some(3..10));
        assert_eq!(selection.range_for_row(3, 7), Some(0..7));
        assert_eq!(selection.range_for_row(4, 10), Some(0..2));
        assert_eq!(selection.range_for_row(5, 10), None);
    }

    #[test]
    fn text_selection_supports_upward_dragging() {
        let selection = selection((4, 2), (2, 3));

        assert_eq!(selection.range_for_row(2, 10), Some(3..10));
        assert_eq!(selection.range_for_row(3, 7), Some(0..7));
        assert_eq!(selection.range_for_row(4, 10), Some(0..2));
    }

    #[test]
    fn text_selection_keeps_single_row_offsets() {
        let selection = selection((8, 7), (8, 2));

        assert_eq!(selection.range_for_row(8, 10), Some(2..7));
    }
}
