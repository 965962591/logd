//! VS Code-style client title bar.

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{h_flex, Icon, IconName, InteractiveElementExt as _, Sizable as _};

use crate::i18n::{text, Key, Language};
use crate::theme;

const HEIGHT: f32 = 34.0;
const CONTROL_WIDTH: f32 = 46.0;

pub fn render(
    left: AnyElement,
    center: AnyElement,
    right: AnyElement,
    window: &mut Window,
    lang: Language,
) -> impl IntoElement {
    let maximized = window.is_maximized();

    div()
        .id("app-title-bar")
        .relative()
        .grid()
        .grid_cols(3)
        .flex_none()
        .w_full()
        .h(px(HEIGHT))
        .items_center()
        .bg(theme::c(theme::TITLE_BAR_BG))
        .border_b_1()
        .border_color(theme::c(theme::BORDER))
        .text_color(theme::c(theme::FG))
        .on_double_click(|_, window, _| window.zoom_window())
        .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
        .child(
            h_flex()
                .h_full()
                .min_w_0()
                .flex_1()
                .items_center()
                .child(left),
        )
        .child(
            h_flex()
                .h_full()
                .w_full()
                .min_w_0()
                .justify_center()
                .child(div().w_full().max_w(px(360.)).child(center)),
        )
        .child(
            h_flex()
                .h_full()
                .w_full()
                .min_w_0()
                .justify_end()
                .child(right)
                .child(control(
                    "window-minimize",
                    IconName::WindowMinimize,
                    text(Key::Minimize, lang),
                    false,
                    |window| window.minimize_window(),
                ))
                .child(control(
                    "window-maximize",
                    if maximized {
                        IconName::WindowRestore
                    } else {
                        IconName::WindowMaximize
                    },
                    text(
                        if maximized {
                            Key::Restore
                        } else {
                            Key::Maximize
                        },
                        lang,
                    ),
                    false,
                    |window| window.zoom_window(),
                ))
                .child(control(
                    "window-close",
                    IconName::WindowClose,
                    text(Key::Close, lang),
                    true,
                    |window| window.remove_window(),
                )),
        )
}

fn control(
    id: &'static str,
    icon: IconName,
    tooltip: &'static str,
    danger: bool,
    action: impl Fn(&mut Window) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .flex_none()
        .w(px(CONTROL_WIDTH))
        .h_full()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .when(danger, |el| {
            el.hover(|s| s.bg(theme::c(theme::DANGER)).text_color(gpui::white()))
        })
        .when(!danger, |el| {
            el.hover(|s| s.bg(theme::c(theme::CONTROL_HOVER)))
        })
        .on_mouse_down(MouseButton::Left, |_, window, cx| {
            window.prevent_default();
            cx.stop_propagation();
        })
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            action(window);
        })
        .tooltip(move |window, cx| gpui_component::tooltip::Tooltip::new(tooltip).build(window, cx))
        .child(Icon::new(icon).small())
}
