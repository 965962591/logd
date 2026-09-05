//! Shared layout values and colors derived from the active gpui-kit theme.

use gpui::{rgb, App, Hsla, Rgba};
use gpui_component::ActiveTheme as _;

const DARK_SURFACE_RGB: u32 = 0x171717;

#[derive(Clone, Copy)]
pub struct Palette {
    pub background: Hsla,
    pub input_background: Hsla,
    pub foreground: Hsla,
    pub title_bar: Hsla,
    pub title_bar_border: Hsla,
    pub control_hover: Hsla,
    pub danger: Hsla,
    pub danger_foreground: Hsla,
    pub selection: Hsla,
    pub muted: Hsla,
    pub gutter: Hsla,
    pub border: Hsla,
    pub status: Hsla,
    pub scroll_track: Hsla,
    pub scroll_thumb: Hsla,
    pub scroll_thumb_hover: Hsla,
    pub tab_bar: Hsla,
    pub tab: Hsla,
    pub tab_active: Hsla,
    pub tab_foreground: Hsla,
    pub tab_active_foreground: Hsla,
    pub tab_active_indicator: Hsla,
    pub search_foreground: Hsla,
}

pub fn palette(cx: &App) -> Palette {
    let active = cx.theme();
    let dark_surface = dark_surface();
    let surface = if active.mode.is_dark() {
        dark_surface
    } else {
        active.background
    };
    Palette {
        background: surface,
        input_background: if active.mode.is_dark() {
            surface
        } else {
            active.sidebar
        },
        foreground: active.foreground,
        title_bar: active.title_bar,
        title_bar_border: active.title_bar_border,
        control_hover: active.list_hover,
        danger: active.danger,
        danger_foreground: active.danger_foreground,
        selection: active.selection,
        muted: active.muted_foreground,
        gutter: if active.mode.is_dark() {
            surface
        } else {
            active.sidebar
        },
        border: active.border,
        status: active.status_bar,
        scroll_track: active.scrollbar,
        scroll_thumb: active.scrollbar_thumb,
        scroll_thumb_hover: active.scrollbar_thumb_hover,
        tab_bar: active.tab_bar,
        tab: active.tab,
        tab_active: active.accent.opacity(0.18),
        tab_foreground: active.tab_foreground,
        tab_active_foreground: active.tab_active_foreground,
        tab_active_indicator: active.accent,
        search_foreground: active.yellow,
    }
}

/// Keep gpui-kit Dock chrome on the same surface as the log workspace.
///
/// Dock renderers read these legacy theme tokens directly instead of going
/// through the application's palette, so changing only the root view leaves
/// split frames and tiles with the stock dark-theme background.
pub fn apply_dark_surface(cx: &mut App) {
    if !cx.theme().mode.is_dark() {
        return;
    }

    let surface = dark_surface();
    {
        let active = gpui_component::Theme::global_mut(cx);
        active.background = surface;
        active.sidebar = surface;
        active.tab_bar = surface;
        active.tiles = surface;
        active.tokens.background = surface.into();
        active.tokens.tab_bar = surface.into();
        active.tokens.tiles = surface.into();
    }
    gpui_component::Theme::sync_base(cx);
}

fn dark_surface() -> Hsla {
    Hsla::from(rgb(DARK_SURFACE_RGB))
}

pub fn search_foreground_rgb(cx: &App) -> u32 {
    u32::from(palette(cx).search_foreground.to_rgb()) >> 8
}

/// 等宽字体。Consolas 在 Windows 上必然存在。
#[cfg(target_os = "macos")]
pub const MONO: &str = "Menlo";
#[cfg(not(target_os = "macos"))]
pub const MONO: &str = "Consolas";
pub const FONT_SIZE: f32 = 13.0;
pub const LINE_HEIGHT: f32 = 18.0;
/// 滚动条宽度
pub const SCROLLBAR_W: f32 = 12.0;
/// 滑块最短长度，5 亿行时按比例算出来几乎是 0，必须托底
pub const MIN_THUMB: f32 = 24.0;
/// 一次滚轮「行」步进多少行
pub const WHEEL_LINES: f32 = 3.0;

pub fn c(v: u32) -> Rgba {
    rgb(v)
}
