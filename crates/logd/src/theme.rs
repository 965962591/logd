//! 配色与排版常量。深色底，参照 VS Code 暗色主题。

use gpui::{rgb, Rgba};

pub const BG: u32 = 0x1e1e1e;
pub const FG: u32 = 0xd4d4d4;
pub const TITLE_BAR_BG: u32 = 0x181818;
pub const CONTROL_HOVER: u32 = 0x2a2d2e;
pub const SEARCH_FORE: u32 = 0xffd75f;
pub const DANGER: u32 = 0xc42b1c;
pub const SELECTION: u32 = 0x264f78;
/// 行号槽、状态栏次要文字
pub const MUTED: u32 = 0x858585;
/// 行号槽背景
pub const GUTTER_BG: u32 = 0x252526;
pub const BORDER: u32 = 0x3c3c3c;
pub const STATUS_BG: u32 = 0x007acc;
pub const STATUS_FG: u32 = 0xffffff;
pub const SCROLL_TRACK: u32 = 0x252526;
pub const SCROLL_THUMB: u32 = 0x4e4e4e;
pub const SCROLL_THUMB_HOVER: u32 = 0x6e6e6e;

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
