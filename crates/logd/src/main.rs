//! logd —— 超大日志文件的筛选与阅读工具。
//!
//! 引擎全在 `logd-core`（mmap、稀疏行索引、多关键字匹配、并行筛选、视口数学），
//! 这个 crate 只做 UI。分成两个 crate 是为了让引擎能脱离 gpui 单独 `cargo test`。

mod app;
mod log_view;
mod theme;

fn main() {
    // 命令行直接给路径，方便拿大文件做性能验收
    let initial = std::env::args().nth(1).map(std::path::PathBuf::from);
    app::run(initial);
}
