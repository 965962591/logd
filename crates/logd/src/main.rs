//! logd —— 超大日志文件的筛选与阅读工具。
//!
//! 引擎全在 `logd-core`（索引、匹配、筛选、视口数学），这里只做 UI。

use gpui::*;
use gpui_component::{h_flex, v_flex, Root, TitleBar};

fn main() {
    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets);

    app.run(move |cx| {
        gpui_component::init(cx);

        cx.spawn(async move |cx| {
            cx.open_window(TitleBar::window_options(), |window, cx| {
                let view = cx.new(|_| LogdApp);
                // 窗口第一层必须是 Root，弹窗/通知/tooltip 都挂在它上面
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("创建窗口失败");
        })
        .detach();
    });
}

struct LogdApp;

impl Render for LogdApp {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .child(TitleBar::new().child(h_flex().w_full().pr_2().child("logd")))
            .child(
                v_flex()
                    .id("body")
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child("logd")
                    .child("拖入日志文件，或按 Ctrl+O 打开"),
            )
    }
}
