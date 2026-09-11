//! Cross-platform window policy for Windows and macOS.

use gpui::{px, size, App, Size, TitlebarOptions, WindowDecorations, WindowOptions};

/// The initial client area used by every build and platform.
pub const INITIAL_WINDOW_WIDTH: f32 = 1000.0;
pub const INITIAL_WINDOW_HEIGHT: f32 = 650.0;

pub fn initial_window_size() -> Size<gpui::Pixels> {
    size(px(INITIAL_WINDOW_WIDTH), px(INITIAL_WINDOW_HEIGHT))
}

pub fn window_options(cx: &App) -> WindowOptions {
    WindowOptions {
        // Client-side decorations are required for the custom title bar.  An
        // explicit transparent titlebar also enables GPUI's Windows hit-test
        // callback instead of creating a native titlebar above our content.
        titlebar: Some(TitlebarOptions {
            title: Some("logd".into()),
            appears_transparent: true,
            ..Default::default()
        }),
        is_movable: true,
        window_bounds: Some(gpui::WindowBounds::centered(initial_window_size(), cx)),
        window_min_size: Some(size(px(760.), px(480.))),
        window_decorations: Some(WindowDecorations::Client),
        app_owns_titlebar_drag: true,
        app_id: Some("com.github.965962591.logd".into()),
        ..Default::default()
    }
}

pub fn primary_modifier(modifiers: &gpui::Modifiers) -> bool {
    if cfg!(target_os = "macos") {
        modifiers.platform
    } else {
        modifiers.control
    }
}
