//! Cross-platform window policy for Windows and macOS.

use gpui::{px, size, App, WindowDecorations, WindowOptions};

pub fn window_options(cx: &App) -> WindowOptions {
    WindowOptions {
        titlebar: None,
        window_bounds: Some(gpui::WindowBounds::centered(size(px(1280.), px(800.)), cx)),
        window_min_size: Some(size(px(760.), px(480.))),
        window_decorations: Some(WindowDecorations::Client),
        app_owns_titlebar_drag: true,
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
