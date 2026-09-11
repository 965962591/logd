#![recursion_limit = "512"]
// logd is a GUI application in both dev and release builds. Keeping the
// Windows subsystem consistent prevents a console window from appearing when
// a debug-built executable is launched directly.
#![cfg_attr(windows, windows_subsystem = "windows")]

//! logd —— 超大日志文件的筛选与阅读工具。
//!
//! 引擎全在 `logd-core`（mmap、稀疏行索引、多关键字匹配、并行筛选、视口数学），
//! 这个 crate 只做 UI。分成两个 crate 是为了让引擎能脱离 gpui 单独 `cargo test`。

mod app;
mod i18n;
mod log_view;
mod platform;
mod regex_table;
mod settings;
mod theme;
mod ui;
mod updater;

// Keep the machine-readable CLI in logd-core so the GUI binary and the
// standalone development binary expose exactly the same behavior.
#[path = "../../logd-core/src/bin/logd-core.rs"]
mod cli;

fn main() {
    let mut args = std::env::args().skip(1);
    if cli::is_cli_command(args.next().as_deref()) {
        if let Err(error) = cli::run_from(std::env::args().skip(1)) {
            eprintln!("错误: {error:#}");
            std::process::exit(1);
        }
        return;
    }

    updater::start_auto_update();

    // 命令行可以直接给若干路径：日志文件开标签页，.logd/.tat 当配置加载。
    // 方便拿大文件做性能验收。
    let initial: Vec<_> = std::env::args()
        .skip(1)
        .map(std::path::PathBuf::from)
        .collect();
    app::run(initial);
}
