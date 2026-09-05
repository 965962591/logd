use std::io;
use std::path::PathBuf;

use gpui_component::dock::{DockAreaState, DockPlacement};

const FILTER_PLACEMENT_KEY: &str = "filter_placement";
const SETTINGS_FILE: &str = "settings.conf";
const DOCK_LAYOUT_FILE: &str = "dock-layout.json";
const RECENT_FILES_FILE: &str = "recent-files.json";
const MAX_RECENT_FILES: usize = 10;

pub fn load_filter_placement() -> DockPlacement {
    read_cache_file(SETTINGS_FILE)
        .and_then(|contents| parse_filter_placement(&contents))
        .unwrap_or(DockPlacement::Right)
}

pub fn load_dock_layout() -> Option<DockAreaState> {
    let contents = read_cache_file(DOCK_LAYOUT_FILE)?;
    serde_json::from_str(&contents).ok()
}

pub fn save_dock_layout(state: &DockAreaState) -> io::Result<()> {
    let path = dock_layout_path().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "executable cache directory not found",
        )
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = serde_json::to_string_pretty(state)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    std::fs::write(path, contents)
}

pub fn load_recent_files() -> Vec<PathBuf> {
    read_cache_file(RECENT_FILES_FILE)
        .map(|contents| parse_recent_files(&contents))
        .unwrap_or_default()
}

pub fn save_recent_files(paths: &[PathBuf]) -> io::Result<()> {
    let path = recent_files_path().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "executable cache directory not found",
        )
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = serde_json::to_string_pretty(paths)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    std::fs::write(path, contents)
}

fn dock_layout_path() -> Option<PathBuf> {
    cache_file_path(DOCK_LAYOUT_FILE)
}

fn recent_files_path() -> Option<PathBuf> {
    cache_file_path(RECENT_FILES_FILE)
}

fn cache_file_path(file_name: &str) -> Option<PathBuf> {
    logd_core::cache::application_cache_dir()
        .ok()
        .map(|path| path.join(file_name))
}

fn read_cache_file(file_name: &str) -> Option<String> {
    let path = cache_file_path(file_name)?;
    if let Ok(contents) = std::fs::read_to_string(&path) {
        return Some(contents);
    }

    let legacy_path = dirs::data_local_dir()?.join("logd").join(file_name);
    let contents = std::fs::read_to_string(legacy_path).ok()?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, &contents);
    Some(contents)
}

fn parse_filter_placement(contents: &str) -> Option<DockPlacement> {
    contents.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        if key.trim() != FILTER_PLACEMENT_KEY {
            return None;
        }
        match value.trim() {
            "left" => Some(DockPlacement::Left),
            "right" => Some(DockPlacement::Right),
            "bottom" => Some(DockPlacement::Bottom),
            _ => None,
        }
    })
}

fn parse_recent_files(contents: &str) -> Vec<PathBuf> {
    let mut paths = serde_json::from_str::<Vec<PathBuf>>(contents).unwrap_or_default();
    paths.retain(|path| !path.as_os_str().is_empty());
    let mut unique = Vec::with_capacity(paths.len().min(MAX_RECENT_FILES));
    for path in paths {
        if !unique.contains(&path) {
            unique.push(path);
            if unique.len() == MAX_RECENT_FILES {
                break;
            }
        }
    }
    unique
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_saved_filter_placement() {
        assert_eq!(
            parse_filter_placement("filter_placement=left\n"),
            Some(DockPlacement::Left)
        );
        assert_eq!(
            parse_filter_placement("future_setting=x\nfilter_placement=bottom\n"),
            Some(DockPlacement::Bottom)
        );
    }

    #[test]
    fn ignores_unknown_filter_placement() {
        assert_eq!(parse_filter_placement("filter_placement=center\n"), None);
        assert_eq!(parse_filter_placement("broken\n"), None);
    }

    #[test]
    fn dock_layout_state_round_trips_as_json() {
        let mut state = DockAreaState::default();
        state.version = Some(2);
        state.center.panel_name = "logd.workspace".to_string();

        let json = serde_json::to_string(&state).unwrap();
        let restored: DockAreaState = serde_json::from_str(&json).unwrap();

        assert_eq!(restored, state);
    }

    #[test]
    fn recent_files_round_trip_and_are_normalized() {
        let paths = parse_recent_files(
            r#"["C:\\logs\\first.log", "", "C:\\logs\\first.log", "D:\\second.log"]"#,
        );
        assert_eq!(
            paths,
            vec![
                PathBuf::from(r"C:\logs\first.log"),
                PathBuf::from(r"D:\second.log")
            ]
        );
        let json = serde_json::to_string(&paths).unwrap();
        assert_eq!(parse_recent_files(&json), paths);
    }

    #[test]
    fn invalid_recent_files_json_is_ignored() {
        assert!(parse_recent_files("not json").is_empty());
    }
}
