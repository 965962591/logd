//! Shared layout values and colors derived from the active gpui-kit theme.

use gpui::{rgb, App, Hsla, Rgba, SharedString, Window};
use gpui_component::{ActiveTheme as _, ThemeMode, ThemeRegistry};

const DARK_SURFACE_RGB: u32 = 0x171717;
pub const DEFAULT_LIGHT_THEME: &str = "Default Light";
pub const DEFAULT_DARK_THEME: &str = "Default Dark";

const BUILTIN_THEME_SETS: &[&str] = &[
    include_str!("../themes/adventure.json"),
    include_str!("../themes/alduin.json"),
    include_str!("../themes/asciinema.json"),
    include_str!("../themes/aurora.json"),
    include_str!("../themes/ayu.json"),
    include_str!("../themes/catppuccin.json"),
    include_str!("../themes/everforest.json"),
    include_str!("../themes/fahrenheit.json"),
    include_str!("../themes/flexoki.json"),
    include_str!("../themes/gruvbox.json"),
    include_str!("../themes/harper.json"),
    include_str!("../themes/hybrid.json"),
    include_str!("../themes/jellybeans.json"),
    include_str!("../themes/kibble.json"),
    include_str!("../themes/macos-classic.json"),
    include_str!("../themes/mellifluous.json"),
    include_str!("../themes/molokai.json"),
    include_str!("../themes/solarized.json"),
    include_str!("../themes/spaceduck.json"),
    include_str!("../themes/tokyonight.json"),
    include_str!("../themes/twilight.json"),
];

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
    pub caret: Hsla,
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

pub fn register_builtin_themes(cx: &mut App) {
    let registry = ThemeRegistry::global_mut(cx);
    for contents in BUILTIN_THEME_SETS {
        registry
            .load_themes_from_str(contents)
            .expect("gpui-kit built-in theme must be valid");
    }
}

pub fn available_themes(cx: &App) -> Vec<(SharedString, ThemeMode)> {
    ThemeRegistry::global(cx)
        .sorted_themes()
        .into_iter()
        .map(|theme| (theme.name.clone(), theme.mode))
        .collect()
}

pub fn apply_named_theme(name: &str, window: Option<&mut Window>, cx: &mut App) -> bool {
    let Some(config) = ThemeRegistry::global(cx).themes().get(name).cloned() else {
        return false;
    };
    let mode = config.mode;
    gpui_component::Theme::global_mut(cx).apply_config(&config);
    gpui_component::Theme::change(mode, window, cx);
    apply_dark_surface(cx);
    true
}

pub fn palette(cx: &App) -> Palette {
    let active = cx.theme();
    let dark_surface = dark_surface();
    let surface = if active.theme_name().as_ref() == DEFAULT_DARK_THEME {
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
        caret: active.caret,
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
        tab_active: active.tab_active,
        tab_foreground: active.tab_foreground,
        tab_active_foreground: active.tab_active_foreground,
        tab_active_indicator: active.primary,
        search_foreground: active.yellow,
    }
}

/// Keep gpui-kit Dock chrome on the same surface as the log workspace.
///
/// Dock renderers read these legacy theme tokens directly instead of going
/// through the application's palette, so changing only the root view leaves
/// split frames and tiles with the stock dark-theme background.
pub fn apply_dark_surface(cx: &mut App) {
    if cx.theme().theme_name().as_ref() != DEFAULT_DARK_THEME {
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use gpui_component::ThemeSet;

    use super::BUILTIN_THEME_SETS;

    #[test]
    fn bundled_gpui_kit_themes_are_valid_and_unique() {
        let themes = BUILTIN_THEME_SETS
            .iter()
            .flat_map(|contents| {
                serde_json::from_str::<ThemeSet>(contents)
                    .expect("bundled theme must parse")
                    .themes
            })
            .collect::<Vec<_>>();
        let names = themes
            .iter()
            .map(|theme| theme.name.as_ref())
            .collect::<HashSet<_>>();

        assert_eq!(BUILTIN_THEME_SETS.len(), 21);
        assert_eq!(themes.len(), 36);
        assert_eq!(names.len(), themes.len());
    }
}
