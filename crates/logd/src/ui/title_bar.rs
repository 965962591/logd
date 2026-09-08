//! VS Code-style client title bar.

use std::sync::{Arc, LazyLock};

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{
    h_flex, Disableable as _, Icon, IconName, InteractiveElementExt as _, Sizable as _,
};

use crate::i18n::{text, Key, Language};
use crate::theme;

const HEIGHT: f32 = 34.0;
const CONTROL_WIDTH: f32 = 46.0;
const SIDE_DRAG_MIN_WIDTH: f32 = 64.0;
const SEARCH_MAX_WIDTH: f32 = 720.0;
const CENTER_DRAG_MIN_WIDTH: f32 = 20.0;
const APP_ICON_BYTES: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../public/2.png"));

static APP_ICON: LazyLock<Arc<Image>> =
    LazyLock::new(|| Arc::new(Image::from_bytes(ImageFormat::Png, APP_ICON_BYTES.to_vec())));

pub fn app_icon() -> impl IntoElement {
    img(APP_ICON.clone()).size(px(20.)).flex_none()
}

pub fn panel_toggle(
    id: &'static str,
    open_icon: IconName,
    closed_icon: IconName,
    open: bool,
    disabled: bool,
    tooltip: impl Into<SharedString>,
    action: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    Button::new(id)
        .icon(if open { open_icon } else { closed_icon })
        .small()
        .ghost()
        .toggled(open)
        .disabled(disabled)
        .tab_stop(false)
        .tooltip(tooltip)
        .on_click(move |event, window, cx| {
            // Panel buttons live inside the title bar, whose double-click
            // handler is reserved for maximizing the window.
            cx.stop_propagation();
            action(event, window, cx);
        })
}

pub fn render(
    left: AnyElement,
    center: AnyElement,
    right: AnyElement,
    window: &mut Window,
    lang: Language,
    cx: &App,
    on_close: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let maximized = window.is_maximized();
    let palette = theme::palette(cx);

    div()
        .id("app-title-bar")
        .relative()
        .grid()
        .grid_cols(4)
        .flex_none()
        .w_full()
        .h(px(HEIGHT))
        .items_center()
        .bg(palette.title_bar)
        .border_b_1()
        .border_color(palette.title_bar_border)
        .text_color(palette.foreground)
        // Windows toggles maximize/restore natively for HTCAPTION. GPUI's
        // zoom_window implementation on Windows only maximizes, so keep the
        // application-level handler for the other platforms.
        .when(!cfg!(windows), |el| {
            el.on_double_click(|_, window, _| window.zoom_window())
        })
        .child(
            h_flex()
                .h_full()
                .min_w_0()
                .flex_1()
                .items_center()
                .child(left)
                .child(drag_region("title-drag-left", SIDE_DRAG_MIN_WIDTH)),
        )
        .child(
            h_flex()
                .h_full()
                .w_full()
                .min_w_0()
                .col_span(2)
                .justify_center()
                .child(drag_region("title-drag-center-left", CENTER_DRAG_MIN_WIDTH))
                .child(div().w_full().max_w(px(SEARCH_MAX_WIDTH)).child(center))
                .child(drag_region(
                    "title-drag-center-right",
                    CENTER_DRAG_MIN_WIDTH,
                )),
        )
        .child(
            h_flex()
                .h_full()
                .w_full()
                .min_w_0()
                .justify_end()
                // The space before the window controls needs its own hitbox;
                // otherwise it cannot move the window on Windows.
                .child(drag_region("title-drag-right", SIDE_DRAG_MIN_WIDTH))
                .child(right)
                .child(control(
                    "window-minimize",
                    IconName::WindowMinimize,
                    text(Key::Minimize, lang),
                    false,
                    false,
                    palette,
                    |window, _| window.minimize_window(),
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
                    cfg!(windows),
                    palette,
                    |window, _| window.zoom_window(),
                ))
                .child(control(
                    "window-close",
                    IconName::WindowClose,
                    text(Key::Close, lang),
                    true,
                    false,
                    palette,
                    on_close,
                )),
        )
}

fn drag_region(id: &'static str, min_width: f32) -> impl IntoElement {
    div()
        .id(id)
        .h_full()
        .flex_1()
        // Keep a real, non-zero hitbox even when the title bar is narrow or
        // the neighboring search/menu content is measured as min-content.
        .min_w(px(min_width))
        .window_control_area(WindowControlArea::Drag)
}

fn control(
    id: &'static str,
    icon: IconName,
    tooltip: &'static str,
    danger: bool,
    native_control: bool,
    palette: theme::Palette,
    action: impl Fn(&mut Window, &mut App) + 'static,
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
        .window_control_area(if danger {
            WindowControlArea::Close
        } else if id == "window-minimize" {
            WindowControlArea::Min
        } else {
            WindowControlArea::Max
        })
        .when(danger, |el| {
            el.hover(|s| s.bg(palette.danger).text_color(palette.danger_foreground))
        })
        .when(!danger, |el| el.hover(|s| s.bg(palette.control_hover)))
        .when(!native_control, |el| {
            el.on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                action(window, cx);
            })
        })
        .tooltip(move |window, cx| gpui_component::tooltip::Tooltip::new(tooltip).build(window, cx))
        .child(Icon::new(icon).small())
}
