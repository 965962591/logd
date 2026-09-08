use std::path::PathBuf;
use std::sync::Arc;

use gpui::*;
use gpui_component::scroll::ScrollableElement as _;
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
        }
    }

    pub fn analyze(
        &mut self,
        source: Arc<FileSource>,
        filters: Vec<FilterSpec>,
        encoding: Encoding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.path = Some(source.path().to_path_buf());
        self.running = true;
        self.error = None;
        self.rows.clear();
        self.enabled_filter_count = filters.iter().filter(|filter| filter.is_active()).count();
        self.analyzed_line_count = 0;
        cx.notify();
        let weak = cx.entity().downgrade();
        let executor = cx.background_executor().clone();
        cx.spawn_in(window, async move |_, window| {
            let result = executor
                .spawn(async move { analyze_source(source, filters, encoding) })
                .await;
            let _ = window.update(|_, cx| {
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
            });
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
        cx.notify();
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
        let content = if self.running {
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
                .children(self.rows.iter().enumerate().map(|(i, row)| {
                    h_flex()
                        .gap_2()
                        .items_start()
                        .child(format!("{:>4}", i + 1))
                        .child(format!("{}  ×{}", row.template, row.count))
                }))
                .into_any_element()
        };
        v_flex()
            .size_full()
            .bg(palette.background)
            .text_color(palette.foreground)
            .font_family(theme::MONO)
            .child(div().flex_1().overflow_y_scrollbar().child(content))
    }
}
