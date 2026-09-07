//! Background self-update through GitHub Releases.

use kaishin::{detect_install_method, Checker, InstallMethod, KaishinOptions};
use std::time::Duration;

const GITHUB_OWNER: &str = "965962591";
const GITHUB_REPOSITORY: &str = "logd";
const BINARY_NAME: &str = "logd";

/// Checks once on every launch and installs a newer release silently.
/// The running process keeps its loaded image; the update is used next launch.
pub fn start_auto_update() {
    if std::env::var_os("LOGD_NO_AUTOUPDATE").is_some() {
        return;
    }

    // kaishin falls back to `gh auth token` when no GitHub token is supplied.
    // `gh.exe` is a console application and Windows briefly creates a console
    // for it when launched from a GUI process. Avoid that fallback when the
    // CLI is installed; authenticated environments can still auto-update.
    #[cfg(windows)]
    if !has_github_token() && gh_cli_is_available() {
        return;
    }
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    if detect_install_method(&executable) == InstallMethod::DevBuild {
        return;
    }

    let options = KaishinOptions::new(
        GITHUB_OWNER,
        GITHUB_REPOSITORY,
        BINARY_NAME,
        env!("CARGO_PKG_VERSION"),
    );
    let checker = Checker::new(BINARY_NAME, options).interval(Duration::ZERO);

    let _ = std::thread::Builder::new()
        .name("logd-auto-update".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            let _ = runtime.block_on(checker.auto_update());
        });
}

#[cfg(windows)]
fn has_github_token() -> bool {
    ["GH_TOKEN", "GITHUB_TOKEN"].iter().any(|name| {
        std::env::var(name)
            .ok()
            .is_some_and(|value| !value.trim().is_empty())
    })
}

#[cfg(windows)]
fn gh_cli_is_available() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| directory.join("gh.exe").is_file())
}
