use std::path::{Path, PathBuf};

use gpui::*;
use gpui_component::{h_flex, v_flex, Icon, IconName, Sizable as _};
use gpui_component::scroll::ScrollableElement as _;
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
}

impl LogAnalysisPanel {
    pub fn new() -> Self {
        Self { path: None, running: false, rows: Vec::new(), error: None }
    }

    pub fn analyze(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let path = path.to_path_buf();
        self.path = Some(path.clone());
        self.running = true;
        self.error = None;
        self.rows.clear();
        cx.notify();
        let weak = cx.entity().downgrade();
        cx.spawn_in(window, async move |_, window| {
            let result = analyze_file(&path);
            let _ = window.update(|_, cx| {
                weak.update(cx, |panel, cx| {
                    panel.running = false;
                    match result {
                        Ok(rows) => panel.rows = rows,
                        Err(error) => panel.error = Some(error),
                    }
                    cx.notify();
                })
            });
        }).detach();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.path = None;
        self.running = false;
        self.rows.clear();
        self.error = None;
        cx.notify();
    }
}

fn analyze_file(path: &Path) -> Result<Vec<AnalysisRow>, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&bytes);
    let miner = Miner::builder()
        .sim_threshold(0.4)
        .depth(4)
        .parametrize_numeric_tokens(true)
        .wildcard("<*>" )
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
    pool.install(|| {
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .par_bridge()
            .for_each(|line| {
            miner.add(line);
            });
    });
    let mut rows: Vec<_> = miner
        .clusters()
        .into_iter()
        .map(|cluster| AnalysisRow { template: cluster.template().to_string(), count: cluster.size() })
        .collect();
    rows.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.template.cmp(&b.template)));
    rows.truncate(200);
    Ok(rows)
}

impl Render for LogAnalysisPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = theme::palette(cx);
        let title = self.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
        let content = if self.running {
            v_flex().gap_2().p_3().child("正在分析日志…").into_any_element()
        } else if let Some(error) = &self.error {
            v_flex().gap_2().p_3().child("日志分析失败").child(error.clone()).into_any_element()
        } else if self.rows.is_empty() {
            v_flex().gap_2().p_3().child("点击标题栏的分析按钮开始").into_any_element()
        } else {
            v_flex().gap_1().p_2().children(self.rows.iter().enumerate().map(|(i, row)| {
                h_flex().gap_2().items_start().child(format!("{:>4}", i + 1)).child(format!("{}  ×{}", row.template, row.count))
            })).into_any_element()
        };
        v_flex()
            .size_full()
            .bg(palette.background)
            .text_color(palette.foreground)
            .font_family(theme::MONO)
            .child(h_flex().h(px(30.)).px_3().items_center().gap_2().bg(palette.tab_bar).child(Icon::new(IconName::PanelLeft).small()).child("LogDrain 分析").child(title))
            .child(div().flex_1().overflow_y_scrollbar().child(content))
    }
}
