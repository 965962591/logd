//! 根视图：标题栏 + 日志视口 + 状态栏。
//!
//! M1 只支持单个文档；多标签在 M3。

use std::path::PathBuf;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{h_flex, v_flex, Root, TitleBar};

use crate::log_view::LogView;
use crate::theme;

pub struct LogdApp {
    view: Option<Entity<LogView>>,
    /// 打开失败时的提示，显示在空状态里
    error: Option<String>,
}

impl LogdApp {
    pub fn new(initial: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut app = Self {
            view: None,
            error: None,
        };
        if let Some(path) = initial {
            app.open(&path, window, cx);
        }
        app
    }

    pub fn open(&mut self, path: &std::path::Path, window: &mut Window, cx: &mut Context<Self>) {
        match LogView::load(path) {
            Ok(loaded) => {
                self.view = Some(cx.new(|cx| LogView::new(loaded, window, cx)));
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
        cx.notify();
    }

    fn status_bar(&self, cx: &App) -> AnyElement {
        let Some(view) = &self.view else {
            return h_flex()
                .h(px(22.))
                .w_full()
                .px_2()
                .bg(theme::c(theme::STATUS_BG))
                .text_color(theme::c(theme::STATUS_FG))
                .text_size(px(11.))
                .child("就绪")
                .into_any_element();
        };

        let v = view.read(cx);
        let doc = v.doc();
        let total = doc.total_file_lines();
        let lines = if doc.index_complete() {
            format!("{} 行", group(total))
        } else {
            // 索引没跑完，行数还会涨，用 + 提示
            format!("{}+ 行", group(total))
        };
        let top = doc.top_file_line().map(|l| l + 1).unwrap_or(0);
        let progress = v.indexing_progress();

        h_flex()
            .h(px(22.))
            .w_full()
            .px_2()
            .gap_4()
            .bg(theme::c(theme::STATUS_BG))
            .text_color(theme::c(theme::STATUS_FG))
            .text_size(px(11.))
            .child(doc.source().file_name())
            .child(lines)
            .child(format!("第 {} 行", group(top)))
            .child(doc.source().encoding().label().to_string())
            .when_some(progress, |el, p| {
                el.child(format!("建索引 {:.0}%", p * 100.0))
            })
            .into_any_element()
    }

    fn empty_state(&self) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .bg(theme::c(theme::BG))
            .text_color(theme::c(theme::FG))
            .child("logd")
            .child(
                div()
                    .text_color(theme::c(theme::MUTED))
                    .child("把日志文件拖进来"),
            )
            .when_some(self.error.clone(), |el, e| {
                el.child(div().text_color(theme::c(0xf48771)).child(e))
            })
            .into_any_element()
    }
}

impl Render for LogdApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body: AnyElement = match &self.view {
            Some(v) => v.clone().into_any_element(),
            None => self.empty_state(),
        };

        v_flex()
            .id("root")
            .size_full()
            .bg(theme::c(theme::BG))
            .child(TitleBar::new().child(h_flex().w_full().pr_2().child("logd")))
            .child(div().flex_1().overflow_hidden().child(body))
            .child(self.status_bar(cx))
            .on_drop(cx.listener(
                |this, paths: &ExternalPaths, window, cx| {
                    // M1 只开第一个；多文件多标签在 M3
                    if let Some(p) = paths.paths().first() {
                        this.open(p, window, cx);
                    }
                },
            ))
    }
}

/// 千分位分隔，5 亿行的数字不加分隔没法读。
fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

pub fn run(initial: Option<PathBuf>) {
    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets);

    app.run(move |cx| {
        gpui_component::init(cx);

        let initial = initial.clone();
        cx.spawn(async move |cx| {
            cx.open_window(TitleBar::window_options(), |window, cx| {
                let view = cx.new(|cx| LogdApp::new(initial, window, cx));
                // 窗口第一层必须是 Root
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("创建窗口失败");
        })
        .detach();
    });
}

#[cfg(test)]
mod tests {
    use super::group;

    #[test]
    fn groups_thousands() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1000), "1,000");
        assert_eq!(group(500_000_000), "500,000,000");
    }
}
