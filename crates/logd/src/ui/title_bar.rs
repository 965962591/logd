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
const WINDOW_CONTROLS_WIDTH: f32 = CONTROL_WIDTH * 3.0;
const SIDE_DRAG_MIN_WIDTH: f32 = 64.0;
const SEARCH_MIN_WIDTH: f32 = 300.0;
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

// Map-shaped mode icons for the log display, inlined so the title bar needs
// no extra icon asset. The outline `map` selects "show all", the filled
// `map` selects "show only filtered"; `currentColor` is tinted by the button.
const SHOW_ALL_SVG: &[u8] = br#"<svg width="16" height="16" viewBox="0 0 16 16" xmlns="http://www.w3.org/2000/svg" fill="currentColor"><path d="M5.235 2.076C5.38271 1.98368 5.56781 1.97489 5.72361 2.05279L10.4728 4.42738L14.235 2.076C14.3891 1.97967 14.5834 1.97457 14.7424 2.06268C14.9014 2.15079 15 2.31824 15 2.5V11C15 11.1724 14.9112 11.3326 14.765 11.424L10.765 13.924C10.6173 14.0163 10.4322 14.0251 10.2764 13.9472L5.52721 11.5726L1.765 13.924C1.61087 14.0203 1.41659 14.0254 1.25762 13.9373C1.09864 13.8492 1 13.6818 1 13.5V5C1 4.82761 1.08881 4.66737 1.235 4.576L5.235 2.076ZM6 10.691L10 12.691V5.30902L6 3.30902V10.691ZM5 3.40212L2 5.27712V12.5979L5 10.7229V3.40212ZM11 5.27712V12.5979L14 10.7229V3.40212L11 5.27712Z"/></svg>"#;
const SHOW_ONLY_FILTERED_SVG: &[u8] = br#"<svg width="16" height="16" viewBox="0 0 16 16" xmlns="http://www.w3.org/2000/svg" fill="currentColor"><path d="M5 2.22288L1.235 4.576C1.08881 4.66737 1 4.82761 1 5V13.5C1 13.6818 1.09864 13.8492 1.25762 13.9373C1.41659 14.0254 1.61087 14.0203 1.765 13.924L5 11.9021V2.22288ZM6 11.809L10 13.809V4.19098L6 2.19098V11.809ZM14.765 11.424L11 13.7771V4.09788L14.235 2.076C14.3891 1.97967 14.5834 1.97457 14.7424 2.06268C14.9014 2.15079 15 2.31824 15 2.5V11C15 11.1724 14.9112 11.3326 14.765 11.424Z"/></svg>"#;

fn inline_icon(data: &'static [u8], palette: theme::Palette) -> Svg {
    // GPUI only paints SVG elements that carry an explicit text color (it is
    // also the tint used for `fill="currentColor"`), so set one here.
    svg()
        .data(data)
        .size(px(14.))
        .flex_none()
        .text_color(palette.foreground)
}

/// Toggle between showing all log lines and only the lines matching the
/// active filters. One button whose icon switches between the outline `map`
/// ("show all") and the filled `map` ("show only filtered"); clicking flips
/// the mode, mirroring the View menu's checked items and the `Shift+L`
/// shortcut.
pub fn show_only_toggle(
    only: bool,
    disabled: bool,
    lang: Language,
    palette: theme::Palette,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let data = if only {
        SHOW_ONLY_FILTERED_SVG
    } else {
        SHOW_ALL_SVG
    };
    // Reveal the mode the click will activate.
    let target = if only { Key::ShowAll } else { Key::ShowOnlyFiltered };
    Button::new("title-show-mode")
        .children(vec![inline_icon(data, palette).into_any_element()])
        .small()
        .ghost()
        .toggled(only)
        .disabled(disabled)
        .tab_stop(false)
        .tooltip(text(target, lang))
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            on_toggle(window, cx);
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

    h_flex()
        .id("app-title-bar")
        .relative()
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
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .items_center()
                .child(left)
                .child(drag_region("title-drag-left", SIDE_DRAG_MIN_WIDTH)),
        )
        .child(
            h_flex()
                .h_full()
                .flex_1()
                .min_w_0()
                .min_w(px(SEARCH_MIN_WIDTH + CENTER_DRAG_MIN_WIDTH * 2.))
                .justify_center()
                .child(fixed_drag_region(
                    "title-drag-center-left",
                    CENTER_DRAG_MIN_WIDTH,
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(SEARCH_MIN_WIDTH))
                        .max_w(px(SEARCH_MAX_WIDTH))
                        .child(center),
                )
                .child(fixed_drag_region(
                    "title-drag-center-right",
                    CENTER_DRAG_MIN_WIDTH,
                )),
        )
        .child(
            h_flex()
                .h_full()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .justify_end()
                .child(
                    h_flex()
                        .h_full()
                        .flex_1()
                        .min_w_0()
                        // Reserve the absolutely positioned window controls
                        // without changing the slot width used for centering.
                        .pr(px(WINDOW_CONTROLS_WIDTH))
                        .justify_end()
                        // The space before the window controls needs its own
                        // hitbox; otherwise it cannot move the window on Windows.
                        .child(drag_region("title-drag-right", SIDE_DRAG_MIN_WIDTH))
                        .child(right),
                ),
        )
        // Keep native window controls out of the flex flow. They remain
        // anchored to the window edge when the title bar is space-constrained.
        .child(
            h_flex()
                .id("window-controls")
                .absolute()
                .top_0()
                .right_0()
                .h_full()
                .flex_none()
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

fn fixed_drag_region(id: &'static str, width: f32) -> impl IntoElement {
    div()
        .id(id)
        .h_full()
        .w(px(width))
        .flex_none()
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
