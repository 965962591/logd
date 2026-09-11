//! Background self-update through GitHub Releases.

use kaishin::{detect_install_method, Checker, InstallMethod, KaishinOptions};
use serde::Deserialize;
use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

const GITHUB_OWNER: &str = "965962591";
const GITHUB_REPOSITORY: &str = "logd";
const BINARY_NAME: &str = "logd";
const GITHUB_DOWNLOAD_PROXY: &str = "https://gh-proxy.com/";

#[derive(Clone, Debug)]
pub struct ManualRelease {
    pub version: String,
    download_url: String,
    pub size: u64,
}

#[derive(Debug)]
pub enum ManualUpdateEvent {
    Available(ManualRelease),
    UpToDate,
    Progress { downloaded: u64, total: u64 },
    Restarting,
    Error(String),
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

/// Checks GitHub Releases without invoking `gh.exe`. Results are delivered to
/// the UI through the supplied channel.
pub fn check_manual_update(events: Sender<ManualUpdateEvent>) {
    let _ = std::thread::Builder::new()
        .name("logd-update-check".into())
        .spawn(move || {
            let event = match fetch_manual_release() {
                Ok(Some(release)) => ManualUpdateEvent::Available(release),
                Ok(None) => ManualUpdateEvent::UpToDate,
                Err(error) => ManualUpdateEvent::Error(error.to_string()),
            };
            let _ = events.send(event);
        });
}

/// Downloads an update beside the running executable. Once complete, a
/// detached helper waits for this process to exit, replaces the locked binary,
/// and launches the new version.
pub fn download_and_restart(release: ManualRelease, events: Sender<ManualUpdateEvent>) {
    let _ = std::thread::Builder::new()
        .name("logd-update-download".into())
        .spawn(move || {
            if let Err(error) = download_and_stage(&release, &events) {
                let _ = events.send(ManualUpdateEvent::Error(error.to_string()));
            }
        });
}

fn fetch_manual_release() -> anyhow::Result<Option<ManualRelease>> {
    let url =
        format!("https://api.github.com/repos/{GITHUB_OWNER}/{GITHUB_REPOSITORY}/releases/latest");
    let mut response = ureq::get(&url)
        .header("User-Agent", concat!("logd/", env!("CARGO_PKG_VERSION")))
        .call()?;
    let body = response.body_mut().read_to_string()?;
    let release: GithubRelease = serde_json::from_str(&body)?;
    let current = semver::Version::parse(env!("CARGO_PKG_VERSION"))?;
    let latest_text = release.tag_name.trim_start_matches(['v', 'V']);
    let latest = semver::Version::parse(latest_text)?;
    if latest <= current {
        return Ok(None);
    }

    let asset_name = platform_asset_name();
    let asset = release
        .assets
        .into_iter()
        .find(|asset| asset.name == asset_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "release {} 缺少当前平台资源 {}",
                release.tag_name,
                asset_name
            )
        })?;
    Ok(Some(ManualRelease {
        version: release.tag_name,
        download_url: format!("{GITHUB_DOWNLOAD_PROXY}{}", asset.browser_download_url),
        size: asset.size,
    }))
}

fn platform_asset_name() -> &'static str {
    #[cfg(windows)]
    {
        "logd.exe"
    }
    #[cfg(not(windows))]
    {
        "logd"
    }
}

fn download_and_stage(
    release: &ManualRelease,
    events: &Sender<ManualUpdateEvent>,
) -> anyhow::Result<()> {
    let executable = std::env::current_exe()?;
    if detect_install_method(&executable) == InstallMethod::DevBuild {
        anyhow::bail!("开发版本不能覆盖自身，请使用发布版测试更新");
    }
    let staged = staged_update_path(&executable);
    let result = download_file(release, &staged, events);
    if let Err(error) = result {
        let _ = std::fs::remove_file(&staged);
        return Err(error);
    }
    launch_replacement_helper(&staged, &executable)?;
    let _ = events.send(ManualUpdateEvent::Restarting);
    // The helper now owns the replacement/relaunch sequence. Exit immediately
    // so the current executable is no longer locked on Windows.
    std::process::exit(0);
}

fn staged_update_path(executable: &std::path::Path) -> PathBuf {
    let extension = executable.extension().and_then(|value| value.to_str());
    let name = match extension {
        Some(extension) => format!(".logd-update-{}.{}", std::process::id(), extension),
        None => format!(".logd-update-{}", std::process::id()),
    };
    executable.with_file_name(name)
}

fn download_file(
    release: &ManualRelease,
    destination: &std::path::Path,
    events: &Sender<ManualUpdateEvent>,
) -> anyhow::Result<()> {
    let mut response = ureq::get(&release.download_url)
        .header("User-Agent", concat!("logd/", env!("CARGO_PKG_VERSION")))
        .call()?;
    let header_total = response
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let total = release.size.max(header_total);
    let mut reader = response.body_mut().as_reader();
    let mut file = File::create(destination)?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut downloaded = 0_u64;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        file.write_all(&buffer[..count])?;
        downloaded += count as u64;
        let _ = events.send(ManualUpdateEvent::Progress { downloaded, total });
    }
    file.sync_all()?;
    if release.size > 0 && downloaded != release.size {
        anyhow::bail!(
            "下载大小不匹配：预期 {} 字节，实际 {} 字节",
            release.size,
            downloaded
        );
    }
    Ok(())
}

#[cfg(windows)]
fn launch_replacement_helper(
    staged: &std::path::Path,
    executable: &std::path::Path,
) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let script = concat!(
        "& { param($processId, $source, $target) ",
        "Wait-Process -Id $processId -ErrorAction SilentlyContinue; ",
        "Move-Item -LiteralPath $source -Destination $target -Force; ",
        "Start-Process -FilePath $target }"
    );
    std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            script,
        ])
        .arg(std::process::id().to_string())
        .arg(staged)
        .arg(executable)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    Ok(())
}

#[cfg(unix)]
fn launch_replacement_helper(
    staged: &std::path::Path,
    executable: &std::path::Path,
) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(staged, std::fs::Permissions::from_mode(0o755))?;
    std::process::Command::new("sh")
        .args(["-c", "while kill -0 \"$1\" 2>/dev/null; do sleep 0.1; done; mv -f \"$2\" \"$3\" && exec \"$3\"", "logd-updater"])
        .arg(std::process::id().to_string())
        .arg(staged)
        .arg(executable)
        .spawn()?;
    Ok(())
}

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
