//! Concurrent `adb logcat` capture with bounded-latency file persistence.
//!
//! Each connected device has one Tokio child process. Lines are sent to a
//! single writer task, which buffers them in memory and appends the buffers on
//! a short interval. The UI opens those append-only files as ordinary log tabs.

use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};

const FLUSH_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub enum LiveLogEvent {
    DeviceReady { serial: String, path: PathBuf },
    DeviceStopped,
    CaptureFinished,
    Error(String),
}

pub struct LiveLogService {
    stop: watch::Sender<bool>,
    thread: Option<JoinHandle<()>>,
}

impl LiveLogService {
    pub fn start(events: Sender<LiveLogEvent>) -> Result<Self, String> {
        let adb = adb_path()?;
        let output_dir = live_log_dir()?;
        let (stop, stop_rx) = watch::channel(false);
        let thread = std::thread::Builder::new()
            .name("logd-adb-capture".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_io()
                    .enable_time()
                    .worker_threads(2)
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = events.send(LiveLogEvent::Error(format!(
                            "cannot start Tokio runtime: {error}"
                        )));
                        let _ = events.send(LiveLogEvent::CaptureFinished);
                        return;
                    }
                };
                runtime.block_on(run_capture(adb, output_dir, events, stop_rx));
            })
            .map_err(|error| format!("cannot start capture thread: {error}"))?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    pub fn stop(&self) {
        // The workers kill their child process and the writer flushes its last
        // in-memory batch once all senders have been dropped.
        let _ = self.stop.send(true);
    }

    /// Signal capture shutdown and wait for the worker to flush its final
    /// in-memory batch. This is intended for background maintenance actions
    /// such as deleting the history directory.
    pub fn stop_and_wait(mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for LiveLogService {
    fn drop(&mut self) {
        self.stop();
        // Do not join from the GPUI thread. The capture thread observes the
        // watch signal, kills adb, flushes its final batch, and exits on its
        // own. Dropping the handle detaches it safely.
        self.thread.take();
    }
}

async fn run_capture(
    adb: PathBuf,
    output_dir: PathBuf,
    events: Sender<LiveLogEvent>,
    stop: watch::Receiver<bool>,
) {
    let devices = match list_devices(&adb).await {
        Ok(devices) if !devices.is_empty() => devices,
        Ok(_) => {
            let _ = events.send(LiveLogEvent::Error(
                "No authorized ADB devices found".into(),
            ));
            let _ = events.send(LiveLogEvent::CaptureFinished);
            return;
        }
        Err(error) => {
            let _ = events.send(LiveLogEvent::Error(error));
            let _ = events.send(LiveLogEvent::CaptureFinished);
            return;
        }
    };

    // Backpressure caps the hot in-memory queue even if filesystem latency
    // temporarily exceeds a device's log rate.
    let (lines_tx, lines_rx) = mpsc::channel(4_096);
    let writer_events = events.clone();
    let writer = tokio::spawn(write_batches(lines_rx, writer_events));
    let mut workers = Vec::with_capacity(devices.len());
    // One UTC timestamp identifies this capture session. Including
    // milliseconds keeps rapid stop/start operations from reusing the same
    // filename while keeping names sortable when browsing the history folder.
    let session_timestamp = capture_timestamp();

    for serial in devices {
        let path = unique_log_path(&output_dir, &serial, &session_timestamp);
        if let Err(error) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            let _ = events.send(LiveLogEvent::Error(format!(
                "Cannot create log file for {serial}: {error}"
            )));
            continue;
        }
        let _ = events.send(LiveLogEvent::DeviceReady {
            serial: serial.clone(),
            path: path.clone(),
        });
        workers.push(tokio::spawn(capture_device(
            adb.clone(),
            serial,
            path,
            lines_tx.clone(),
            events.clone(),
            stop.clone(),
        )));
    }
    drop(lines_tx);

    for worker in workers {
        let _ = worker.await;
    }
    let _ = writer.await;
    let _ = events.send(LiveLogEvent::CaptureFinished);
}

async fn capture_device(
    adb: PathBuf,
    serial: String,
    path: PathBuf,
    lines: mpsc::Sender<(PathBuf, String)>,
    events: Sender<LiveLogEvent>,
    mut stop: watch::Receiver<bool>,
) {
    let mut command = Command::new(&adb);
    command
        .arg("-s")
        .arg(&serial)
        .arg("logcat")
        .arg("-v")
        .arg("threadtime")
        // Do not replay the entire device ring buffer on every click. Keep the
        // latest buffered line, then continue reading the live stream.
        .arg("-T")
        .arg("1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    command.kill_on_drop(true);
    configure_adb_environment(&mut command, &adb);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = events.send(LiveLogEvent::Error(format!(
                "Cannot start adb logcat for {serial}: {error}"
            )));
            return;
        }
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = events.send(LiveLogEvent::Error(format!(
            "adb logcat has no stdout for {serial}"
        )));
        return;
    };
    let stderr_task = child.stderr.take().map(|stderr| {
        tokio::spawn(async move {
            let mut bytes = Vec::new();
            let mut reader = BufReader::new(stderr);
            let _ = reader.read_to_end(&mut bytes).await;
            String::from_utf8_lossy(&bytes).trim().to_owned()
        })
    });
    // `lines()` validates UTF-8 and terminates on one malformed byte. Android
    // logs can contain arbitrary payload bytes, so read raw lines and decode
    // with replacement characters instead.
    let mut reader = BufReader::new(stdout);
    let mut requested_stop = false;

    'capture: loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    requested_stop = true;
                    let _ = child.kill().await;
                    break;
                }
            }
            line = read_log_line(&mut reader) => match line {
                Ok(Some(line)) => {
                    tokio::select! {
                        result = lines.send((path.clone(), format!("{line}\n"))) => {
                            if result.is_err() {
                                break 'capture;
                            }
                        }
                        changed = stop.changed() => {
                            if changed.is_err() || *stop.borrow() {
                                requested_stop = true;
                                let _ = child.kill().await;
                                break 'capture;
                            }
                        }
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    let _ = events.send(LiveLogEvent::Error(format!("adb logcat read failed for {serial}: {error}")));
                    // A read-side failure must not leave adb logcat orphaned;
                    // otherwise the service waits forever in child.wait().
                    let _ = child.kill().await;
                    break;
                }
            }
        }
    }
    let status = child.wait().await;
    let stderr = match stderr_task {
        Some(task) => task.await.unwrap_or_default(),
        None => String::new(),
    };
    if !requested_stop {
        match status {
            Ok(status) if !status.success() => {
                let detail = if stderr.is_empty() {
                    format!("exit status {status}")
                } else {
                    stderr
                };
                let _ = events.send(LiveLogEvent::Error(format!(
                    "adb logcat failed for {serial}: {detail}"
                )));
            }
            Err(error) => {
                let _ = events.send(LiveLogEvent::Error(format!(
                    "adb logcat process failed for {serial}: {error}"
                )));
            }
            _ => {}
        }
    }
    let _ = events.send(LiveLogEvent::DeviceStopped);
}

async fn read_log_line<R>(reader: &mut BufReader<R>) -> std::io::Result<Option<String>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(256);
    let count = reader.read_until(b'\n', &mut bytes).await?;
    if count == 0 {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

async fn write_batches(mut lines: mpsc::Receiver<(PathBuf, String)>, events: Sender<LiveLogEvent>) {
    let mut buffers: HashMap<PathBuf, String> = HashMap::new();
    let mut flush = tokio::time::interval(FLUSH_INTERVAL);
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            item = lines.recv() => match item {
                Some((path, line)) => buffers.entry(path).or_default().push_str(&line),
                None => {
                    flush_buffers(&mut buffers, &events);
                    break;
                }
            },
            _ = flush.tick() => flush_buffers(&mut buffers, &events),
        }
    }
}

fn flush_buffers(buffers: &mut HashMap<PathBuf, String>, events: &Sender<LiveLogEvent>) {
    use std::io::Write as _;

    for (path, buffer) in buffers.iter_mut() {
        if buffer.is_empty() {
            continue;
        }
        let write = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(buffer.as_bytes()))
            .and_then(|_| {
                // A periodic flush is deliberate: memory remains the hot path,
                // while a successful batch survives process interruption.
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(path)?
                    .sync_data()
            });
        match write {
            Ok(()) => buffer.clear(),
            Err(error) => {
                let _ = events.send(LiveLogEvent::Error(format!(
                    "Cannot write {}: {error}",
                    path.display()
                )));
            }
        }
    }
}

async fn list_devices(adb: &Path) -> Result<Vec<String>, String> {
    let mut command = Command::new(adb);
    command.arg("devices");
    configure_adb_environment(&mut command, adb);
    let output = command
        .output()
        .await
        .map_err(|error| format!("Cannot run adb devices: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "adb devices failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(parse_devices(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_devices(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let serial = fields.next()?;
            (fields.next() == Some("device")).then(|| serial.to_string())
        })
        .collect()
}

fn live_log_dir() -> Result<PathBuf, String> {
    let path = logd_core::cache::application_cache_dir()
        .map_err(|error| format!("Cannot determine the application cache directory: {error:#}"))?
        .join("logs");
    std::fs::create_dir_all(&path)
        .map_err(|error| format!("Cannot create {}: {error}", path.display()))?;
    Ok(path)
}

pub fn history_dir() -> Result<PathBuf, String> {
    logd_core::cache::application_cache_dir()
        .map(|path| path.join("logs"))
        .map_err(|error| format!("Cannot determine the application cache directory: {error:#}"))
}

pub fn clear_history() -> Result<usize, String> {
    let directory = history_dir()?;
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(format!("Cannot read {}: {error}", directory.display()));
        }
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry
            .map_err(|error| format!("Cannot read an entry in {}: {error}", directory.display()))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("Cannot inspect {}: {error}", entry.path().display()))?;
        if !file_type.is_file() {
            continue;
        }
        std::fs::remove_file(entry.path())
            .map_err(|error| format!("Cannot delete {}: {error}", entry.path().display()))?;
        removed += 1;
    }
    Ok(removed)
}

pub fn is_history_log(path: &Path) -> bool {
    history_dir()
        .ok()
        .is_some_and(|directory| path.parent() == Some(directory.as_path()))
}

fn adb_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("LOGD_ADB_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!("LOGD_ADB_PATH does not exist: {}", path.display()));
    }

    let platform = if cfg!(windows) { "win" } else { "mac" };
    let executable = if cfg!(windows) { "adb.exe" } else { "adb" };
    let mut candidates = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../resources")
        .join(platform)
        .join(executable)];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("resources").join(platform).join(executable));
            if let Some(contents) = parent.parent() {
                candidates.push(
                    contents
                        .join("Resources")
                        .join("resources")
                        .join(platform)
                        .join(executable),
                );
            }
        }
    }
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| "Bundled adb was not found in resources".to_string())
}

fn configure_adb_environment(command: &mut Command, adb: &Path) {
    command.current_dir(adb.parent().unwrap_or_else(|| Path::new(".")));
    #[cfg(windows)]
    {
        // adb.exe is a console subsystem executable. Do not flash a console
        // window over the GUI each time a capture starts.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(target_os = "macos")]
    if let Some(parent) = adb.parent() {
        let mut library_path = OsString::from(parent.join("lib64"));
        if let Some(existing) = std::env::var_os("DYLD_LIBRARY_PATH") {
            library_path.push(":");
            library_path.push(existing);
        }
        command.env("DYLD_LIBRARY_PATH", library_path);
    }
}

fn safe_file_name(serial: &str) -> String {
    serial
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn unique_log_path(output_dir: &Path, serial: &str, timestamp: &str) -> PathBuf {
    let stem = format!("{}_{}", safe_file_name(serial), timestamp);
    let initial = output_dir.join(format!("{stem}.log"));
    if !initial.exists() {
        return initial;
    }
    // A suffix is only needed if the system clock has the same millisecond on
    // two starts. It preserves the device_timestamp naming scheme while
    // making the no-overwrite guarantee explicit.
    for suffix in 1..=1000 {
        let candidate = output_dir.join(format!("{stem}_{suffix}.log"));
        if !candidate.exists() {
            return candidate;
        }
    }
    // This is practically unreachable; the create_new call below still
    // reports a useful error if the directory is unexpectedly saturated.
    initial
}

fn capture_timestamp() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = duration.as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3_600;
    let minute = (day_seconds % 3_600) / 60;
    let second = day_seconds % 60;
    let millis = duration.subsec_millis();
    format!("{year:04}{month:02}{day:02}_{hour:02}{minute:02}{second:02}_{millis:03}")
}

// Gregorian calendar conversion based on the civil_from_days algorithm. The
// result is UTC, which is stable across machines and avoids locale characters
// in filenames.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month_part = (5 * doy + 2) / 153;
    let day = doy - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::{capture_timestamp, parse_devices, safe_file_name};

    #[test]
    fn parses_only_authorized_devices() {
        assert_eq!(
            parse_devices("List of devices attached\nemulator-5554\tdevice\nABC\toffline\nXYZ\tunauthorized\n"),
            vec!["emulator-5554"]
        );
    }

    #[test]
    fn makes_device_serials_safe_file_names() {
        assert_eq!(safe_file_name("usb:1/2"), "usb_1_2");
    }

    #[test]
    fn capture_timestamp_is_non_empty() {
        assert!(!capture_timestamp().is_empty());
    }
}
