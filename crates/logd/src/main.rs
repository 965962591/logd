//! logd —— 超大日志文件的筛选与阅读工具。
//!
//! 引擎在 `logd-core`，这里只管 UI。

use gpui::{
    div, prelude::*, px, rgb, size, App, Application, Bounds, Context, TitlebarOptions, Window,
    WindowBounds, WindowOptions,
};

mod theme {
    pub const BG: u32 = 0x1e1e1e;
    pub const FG: u32 = 0xd4d4d4;
    pub const MUTED: u32 = 0x808080;
}

struct LogdApp;

impl Render for LogdApp {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(theme::BG))
            .text_color(rgb(theme::FG))
            .justify_center()
            .items_center()
            .gap_2()
            .child("logd")
            .child(
                div()
                    .text_color(rgb(theme::MUTED))
                    .child("拖入日志文件，或按 Ctrl+O 打开"),
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("logd".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| cx.new(|_cx| LogdApp),
        )
        .expect("创建窗口失败");
        cx.activate(true);
    });
}
