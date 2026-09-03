//! logd 引擎层：内存映射、稀疏行索引、多关键字匹配、并行筛选、`.tat` 配置读写。
//!
//! 这一层完全不依赖 UI，可以单独 `cargo test -p logd-core`。

pub mod cache;
pub mod document;
pub mod index;
pub mod matcher;
pub mod progress;
pub mod render;
pub mod scan;
pub mod source;
pub mod tat;
pub mod viewport;

pub use document::{Document, RenderRow};
pub use index::{ChunkIndex, LineIndex, ANCHOR_STRIDE};
pub use matcher::{FilterSpec, HighlightMode, MatcherSet, Span, Verdict};
pub use progress::Progress;
pub use render::{prepare_line, prepare_plain, RenderLine, DEFAULT_MAX_RENDER_BYTES};
pub use scan::{scan_all, ScanOutcome};
pub use source::{Encoding, FileSource};
pub use tat::TatFile;
pub use viewport::{ScrollTo, Viewport};
