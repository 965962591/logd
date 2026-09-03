//! 日志视口：自建滚动 + 自绘滚动条 + 行渲染。
//!
//! **为什么不用 `uniform_list` / `virtual_list`**：gpui 的 `Pixels` 是 `f32`。
//! 5 亿行 × 18px = 9e9 px，f32 尾数 24 位，超过约 1.67e7 px（≈93 万行）滚动偏移
//! 就开始丢精度、抖动、跳行。任何「以像素总高为基准」的虚拟列表在这个量级都不可用。
//!
//! 这里的滚动位置由 [`logd_core::Viewport`] 维护为 `(anchor_line: u64, pixel_offset: f32)`，
//! 全部比例运算走 f64，只在最后一步落到像素。滚动数学本身在 `logd-core` 里有单测。

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use logd_core::{
    index::HEAD_BYTES, Document, FileSource, LineIndex, Progress, RenderRow, ScrollTo,
};

use crate::theme;

pub struct LogView {
    doc: Document,
    focus: FocusHandle,
    /// 视口在窗口里的矩形。由 `canvas` 在 prepaint 阶段回填，render 读上一帧的值。
    area: Rc<Cell<Bounds<Pixels>>>,
    /// 上一帧用过的尺寸，用来判断是否真的变了，避免 notify 死循环。
    last_area: Bounds<Pixels>,
    /// 正在拖滚动条：记录抓取点在滑块内的偏移。
    drag_grab: Option<f32>,
    /// 后台全量索引的进度。`None` 表示没有在跑。
    indexing: Option<Arc<Progress>>,
}

/// 打开文件的**易错部分**：mmap + 编码探测 + 首屏索引。
///
/// 和构造视图分开，是因为 `cx.new()` 的闭包必须返回 `Self` 而不是 `Result<Self>`。
pub struct Loaded {
    source: Arc<FileSource>,
    head: Arc<LineIndex>,
}

impl Loaded {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let source = Arc::new(FileSource::open(path)?);
        // 阶段 A：只索引头部，先把首屏顶出来
        let head = Arc::new(LineIndex::build_head(source.data(), HEAD_BYTES));
        Ok(Self { source, head })
    }
}

impl LogView {
    /// 打开文件但先不建视图。失败在这一步暴露。
    pub fn load(path: &std::path::Path) -> Result<Loaded> {
        Loaded::open(path)
    }
    pub fn new(loaded: Loaded, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let complete = loaded.head.complete;
        let mut view = Self {
            doc: Document::new(loaded.source, loaded.head, theme::LINE_HEIGHT),
            focus: cx.focus_handle(),
            area: Rc::new(Cell::new(Bounds::default())),
            last_area: Bounds::default(),
            drag_grab: None,
            indexing: None,
        };
        window.focus(&view.focus, cx);
        // 阶段 B：文件没索引完就丢到后台跑全量
        if !complete {
            view.start_full_index(cx);
        }
        view
    }

    pub fn doc(&self) -> &Document {
        &self.doc
    }

    pub fn indexing_progress(&self) -> Option<f32> {
        self.indexing.as_ref().map(|p| p.fraction())
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    fn start_full_index(&mut self, cx: &mut Context<Self>) {
        let source = self.doc.source().clone();
        let progress = Arc::new(Progress::new(source.len()));
        self.indexing = Some(progress.clone());

        cx.spawn(async move |this, cx| {
            let built = cx
                .background_executor()
                .spawn(async move { LineIndex::build_full(source.data(), &progress) })
                .await;
            this.update(cx, |this, cx| {
                if let Some(index) = built {
                    this.doc.set_index(Arc::new(index));
                }
                this.indexing = None;
                cx.notify();
            })
            .ok();
        })
        .detach();

        // 索引期间 30Hz 刷一次，进度条和行数才会动
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(33))
                    .await;
                let running = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        this.indexing.is_some()
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
        })
        .detach();
    }

    // ---- 输入 ----

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let lh = theme::LINE_HEIGHT;
        let (dx, dy) = match ev.delta {
            ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
            ScrollDelta::Lines(l) => (l.x * lh, l.y * lh * theme::WHEEL_LINES),
        };
        // gpui 的 dy 是「内容跟随手指移动」的方向，视口位移要取反
        let vp = self.doc.viewport_mut();
        vp.scroll_by_pixels(-dy);
        if dx != 0.0 {
            vp.scroll_h_by(-dx);
        }
        cx.notify();
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let ctrl = ks.modifiers.control;
        let vp = self.doc.viewport_mut();
        match ks.key.as_str() {
            "up" => vp.scroll_by_lines(-1),
            "down" => vp.scroll_by_lines(1),
            "pageup" => vp.page(-1),
            "pagedown" => vp.page(1),
            "home" if ctrl => vp.scroll_to_top(),
            "end" if ctrl => vp.scroll_to_bottom(),
            "home" => vp.scroll_h_by(-f32::MAX),
            "end" => vp.scroll_h_by(f32::MAX),
            "left" => vp.scroll_h_by(-40.0),
            "right" => vp.scroll_h_by(40.0),
            _ => return,
        }
        cx.notify();
    }

    /// 按文件行号跳转，给 Ctrl+G 和外部调用用。
    pub fn goto_line(&mut self, file_line: u64, cx: &mut Context<Self>) {
        self.doc.goto_file_line(file_line, ScrollTo::Center);
        cx.notify();
    }

    // ---- 滚动条 ----

    fn track_metrics(&self) -> (f32, f32, f32) {
        let area = self.last_area;
        let top = f32::from(area.origin.y);
        let len = f32::from(area.size.height);
        let (thumb_off, thumb_len) = self.doc.viewport().thumb(len, theme::MIN_THUMB);
        let _ = thumb_len;
        (top, len, thumb_off)
    }

    fn on_scrollbar_down(&mut self, y: f32, cx: &mut Context<Self>) {
        let (top, len, thumb_off) = self.track_metrics();
        let (_, thumb_len) = self.doc.viewport().thumb(len, theme::MIN_THUMB);
        let local = y - top;
        if local >= thumb_off && local <= thumb_off + thumb_len {
            // 抓住滑块本体，记住抓取点，拖动时保持相对位置
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
        let (top, len, _) = self.track_metrics();
        self.doc
            .viewport_mut()
            .set_thumb_offset(y - top - grab, len, theme::MIN_THUMB);
        cx.notify();
    }

    // ---- 渲染 ----

    fn render_row(&self, row: &RenderRow, gutter_w: f32, h_scroll: f32) -> AnyElement {
        let filters = self.doc.matcher().filters();
        let line_spec = row.line_filter.and_then(|i| filters.get(i));

        let mut content = div()
            .flex_none()
            .ml(px(-h_scroll))
            .when_some(line_spec.and_then(|f| f.fore), |el, c| {
                el.text_color(theme::c(c))
            })
            .when_some(line_spec.filter(|f| f.bold).map(|_| ()), |el, _| {
                el.font_weight(FontWeight::BOLD)
            });

        if row.spans.is_empty() {
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
            content =
                content.child(StyledText::new(row.text.clone()).with_highlights(runs));
        }

        div()
            .flex()
            .flex_row()
            .h(px(theme::LINE_HEIGHT))
            .w_full()
            .overflow_hidden()
            .when_some(line_spec.and_then(|f| f.back), |el, c| el.bg(theme::c(c)))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .w(px(gutter_w))
                    .pr_2()
                    .justify_end()
                    .bg(theme::c(theme::GUTTER_BG))
                    .text_color(theme::c(theme::MUTED))
                    // 行号槽显示的永远是**文件行号**，筛选视图下也不变
                    .child(format!("{}", row.file_line + 1)),
            )
            .child(div().flex_1().pl_2().overflow_hidden().child(content))
            .into_any_element()
    }

    /// 行号槽宽度按当前总行数的位数算，别让它随滚动跳来跳去。
    fn gutter_width(&self) -> f32 {
        let digits = (self.doc.total_file_lines().max(1) as f64).log10().floor() as usize + 1;
        (digits.max(4) as f32) * (theme::FONT_SIZE * 0.62) + 20.0
    }
}

impl Render for LogView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 上一帧 canvas 量到的尺寸；首帧是 0，canvas 回填后会触发再画一次
        let area = self.area.get();
        if area != self.last_area {
            self.last_area = area;
            self.doc
                .viewport_mut()
                .set_height(f32::from(area.size.height));
        }

        let vp = self.doc.viewport();
        let h_scroll = vp.h_scroll();
        let pixel_offset = vp.pixel_offset();
        let track_len = f32::from(area.size.height);
        let (thumb_off, thumb_len) = vp.thumb(track_len, theme::MIN_THUMB);
        let show_scrollbar = self.doc.display_rows() > 0 && thumb_len < track_len;

        let gutter_w = self.gutter_width();
        let rows = self.doc.rows();

        let bounds_sink = self.area.clone();
        let handle = cx.entity().downgrade();

        div()
            .id("log-view")
            .key_context("LogView")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(theme::c(theme::BG))
            .text_color(theme::c(theme::FG))
            .font_family(theme::MONO)
            .text_size(px(theme::FONT_SIZE))
            .line_height(px(theme::LINE_HEIGHT))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_key_down(cx.listener(Self::on_key))
            .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, _w, cx| {
                if this.drag_grab.is_some() && ev.pressed_button == Some(MouseButton::Left) {
                    this.on_drag_move(f32::from(ev.position.y), cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _ev: &MouseUpEvent, _w, cx| {
                    if this.drag_grab.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            // 量视口尺寸。prepaint 里回填，尺寸变了才 notify，避免每帧重画
            .child(
                canvas(
                    move |bounds, _window, cx| {
                        if bounds_sink.get() != bounds {
                            bounds_sink.set(bounds);
                            handle.update(cx, |_, cx| cx.notify()).ok();
                        }
                    },
                    |_, _, _, _| {},
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
                    .children(
                        rows.iter()
                            .map(|r| self.render_row(r, gutter_w, h_scroll)),
                    ),
            )
            .when(show_scrollbar, |el| {
                el.child(
                    div()
                        .id("vscrollbar")
                        .absolute()
                        .top_0()
                        .right_0()
                        .bottom_0()
                        .w(px(theme::SCROLLBAR_W))
                        .bg(theme::c(theme::SCROLL_TRACK))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, ev: &MouseDownEvent, _w, cx| {
                                this.on_scrollbar_down(f32::from(ev.position.y), cx);
                            }),
                        )
                        .child(
                            div()
                                .absolute()
                                .top(px(thumb_off))
                                .left_0()
                                .right_0()
                                .h(px(thumb_len))
                                .rounded_sm()
                                .bg(theme::c(if self.drag_grab.is_some() {
                                    theme::SCROLL_THUMB_HOVER
                                } else {
                                    theme::SCROLL_THUMB
                                })),
                        ),
                )
            })
    }
}
