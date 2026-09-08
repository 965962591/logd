use std::path::PathBuf;
use std::sync::Arc;

use gpui::*;
use gpui_component::scroll::{Scrollbar, ScrollbarMode};
use gpui_component::{h_flex, v_flex};
use logd_core::{Encoding, FileSource, FilterSpec, MatcherSet};
use logdrain::Miner;
use rayon::prelude::*;

use crate::theme;

#[derive(Clone, Debug)]
pub struct AnalysisRow {
    pub template: String,
    pub count: u64,
}

pub struct LogAnalysisPanel {
    path: Option<PathBuf>,
    running: bool,
    rows: Vec<AnalysisRow>,
    error: Option<String>,
    generation: u64,
    enabled_filter_count: usize,
    analyzed_line_count: u64,
    scroll: UniformListScrollHandle,
}

impl LogAnalysisPanel {
    pub fn new() -> Self {
        Self {
            path: None,
            running: false,
            rows: Vec::new(),
            error: None,
            generation: 0,
            enabled_filter_count: 0,
            analyzed_line_count: 0,
            scroll: UniformListScrollHandle::new(),
        }
    }

    pub fn analyze(
        &mut self,
        source: Arc<FileSource>,
        filters: Vec<FilterSpec>,
        encoding: Encoding,
        cx: &mut Context<Self>,
    ) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.path = Some(source.path().to_path_buf());
        self.running = true;
        self.error = None;
        self.rows.clear();
        self.reset_scroll_position();
        self.enabled_filter_count = filters.iter().filter(|filter| filter.is_active()).count();
        self.analyzed_line_count = 0;
        cx.notify();
        let weak = cx.entity().downgrade();
        let executor = cx.background_executor().clone();
        cx.spawn(async move |_, cx| {
            let result = executor
                .spawn(async move { analyze_source(source, filters, encoding) })
                .await;
            weak.update(cx, |panel, cx| {
                if panel.generation != generation {
                    return;
                }
                panel.running = false;
                match result {
                    Ok((rows, analyzed_line_count)) => {
                        panel.rows = rows;
                        panel.analyzed_line_count = analyzed_line_count;
                    }
                    Err(error) => panel.error = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.path = None;
        self.running = false;
        self.rows.clear();
        self.error = None;
        self.generation = self.generation.wrapping_add(1);
        self.enabled_filter_count = 0;
        self.analyzed_line_count = 0;
        self.reset_scroll_position();
        cx.notify();
    }

    fn reset_scroll_position(&mut self) {
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(0.)));
    }
}

fn analyze_source(
    source: Arc<FileSource>,
    filters: Vec<FilterSpec>,
    encoding: Encoding,
) -> Result<(Vec<AnalysisRow>, u64), String> {
    let matcher = MatcherSet::new(filters, encoding).map_err(|e| format!("{e:#}"))?;
    let miner = Miner::builder()
        .sim_threshold(0.4)
        .depth(4)
        .parametrize_numeric_tokens(true)
        .wildcard("<*>")
        .build()
        .map_err(|e| e.to_string())?;
    // LogDrain::Miner is internally synchronized, so all lines can be ingested
    // concurrently. Build a dedicated Rayon pool instead of using the global
    // pool: analysis then has a predictable amount of CPU and does not contend
    // with other background work in the application.
    let workers = std::thread::available_parallelism()
        .map(|n| n.get().max(1))
        .unwrap_or(1);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .thread_name(|index| format!("logdrain-{index}"))
        .build()
        .map_err(|e| e.to_string())?;
    let analyzed_line_count = std::sync::atomic::AtomicU64::new(0);
    pool.install(|| {
        source.data()[source.bom_len()..]
            .par_split(|byte| *byte == b'\n')
            .for_each(|line| {
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                if line.is_empty() || !matcher.is_visible(line) {
                    return;
                }
                let decoded = encoding.decode_bytes(line);
                if decoded.trim().is_empty() {
                    return;
                }
                miner.add(decoded.as_ref());
                analyzed_line_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            });
    });
    let mut rows: Vec<_> = miner
        .clusters()
        .into_iter()
        .map(|cluster| AnalysisRow {
            template: cluster.template().to_string(),
            count: cluster.size(),
        })
        .collect();
    rows.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.template.cmp(&b.template))
    });
    rows.truncate(200);
    Ok((
        rows,
        analyzed_line_count.load(std::sync::atomic::Ordering::Relaxed),
    ))
}

impl Render for LogAnalysisPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = theme::palette(cx);
        let filter_status = if self.enabled_filter_count == 0 {
            "未启用过滤器，分析全部日志".to_string()
        } else {
            format!("基于 {} 个已启用过滤器", self.enabled_filter_count)
        };
        let message = if self.running {
            v_flex()
                .gap_2()
                .p_3()
                .child("正在分析日志…")
                .child(filter_status.clone())
                .into_any_element()
        } else if let Some(error) = &self.error {
            v_flex()
                .gap_2()
                .p_3()
                .child("日志分析失败")
                .child(error.clone())
                .into_any_element()
        } else if self.rows.is_empty() {
            v_flex()
                .gap_2()
                .p_3()
                .child("点击标题栏的分析按钮开始")
                .into_any_element()
        } else {
            v_flex()
                .gap_1()
                .p_2()
                .child(
                    h_flex()
                        .gap_2()
                        .text_color(palette.muted)
                        .child(filter_status)
                        .child(format!("已分析 {} 行", self.analyzed_line_count)),
                )
                .into_any_element()
        };
        if self.running || self.error.is_some() || self.rows.is_empty() {
            return v_flex()
                .size_full()
                .bg(palette.background)
                .text_color(palette.foreground)
                .text_size(px(theme::FONT_SIZE))
                .line_height(px(theme::LINE_HEIGHT))
                .font_family(theme::MONO)
                .child(message);
        }

        let rows = self.rows.clone();
        let scroll = self.scroll.clone();
        // Keep every row at the widest measured template width.  A fixed row
        // width is important here: UniformList's unconstrained mode can only
        // expose horizontal overflow when its children report their real
        // content width (a flex child with `min_w_0` would otherwise shrink).
        let content_width = rows
            .iter()
            .map(|row| {
                let text_width = row
                    .template
                    .chars()
                    .map(|ch| if ch.is_ascii() { 0.62 } else { 1.0 })
                    .sum::<f32>()
                    * theme::FONT_SIZE;
                text_width + 130.0
            })
            .fold(900.0, f32::max);
        let list = uniform_list(
            "logdrain-analysis-results",
            rows.len(),
            move |range, _window, _cx| {
                range
                    .map(|index| {
                        let row = &rows[index];
                        h_flex()
                            .id(("logdrain-analysis-row", index))
                            .h(px(theme::LINE_HEIGHT))
                            .w(px(content_width))
                            .flex_none()
                            .items_center()
                            .gap_2()
                            .border_b_1()
                            .border_color(palette.border)
                            .child(
                                div()
                                    .w(px(42.))
                                    .flex_none()
                                    .text_color(palette.muted)
                                    .child(format!("{:>4}", index + 1)),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .whitespace_nowrap()
                                    .child(row.template.clone()),
                            )
                            .child(
                                div()
                                    .w(px(72.))
                                    .flex_none()
                                    .text_color(palette.muted)
                                    .child(format!("×{}", row.count)),
                            )
                    })
                    .collect::<Vec<_>>()
            },
        )
        .size_full()
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .track_scroll(&scroll);
        let scrollbar_width = Scrollbar::width();
        v_flex()
            .size_full()
            .bg(palette.background)
            .text_color(palette.foreground)
            .text_size(px(theme::FONT_SIZE))
            .line_height(px(theme::LINE_HEIGHT))
            .font_family(theme::MONO)
            .child(message)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .right(scrollbar_width)
                            .bottom(scrollbar_width)
                            .child(list),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .right_0()
                            .bottom(scrollbar_width)
                            .w(scrollbar_width)
                            .child(
                                Scrollbar::vertical(&scroll)
                                    .id("logdrain-vscrollbar")
                                    .mode(ScrollbarMode::Always)
                                    .viewport_from_layout(),
                            ),
                    )
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right(scrollbar_width)
                            .bottom_0()
                            .h(scrollbar_width)
                            .child(
                                Scrollbar::horizontal(&scroll)
                                    .id("logdrain-hscrollbar")
                                    .mode(ScrollbarMode::Always)
                                    .viewport_from_layout(),
                            ),
                    ),
            )
    }
}
