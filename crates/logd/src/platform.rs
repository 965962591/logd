//! Cross-platform window policy for Windows and macOS.

use gpui::{px, size, App, TitlebarOptions, WindowDecorations, WindowOptions};

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
        window_bounds: Some(gpui::WindowBounds::centered(size(px(900.), px(600.)), cx)),
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
