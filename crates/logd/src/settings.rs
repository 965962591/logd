use std::io;
use std::path::PathBuf;

use gpui_component::dock::{DockAreaState, DockPlacement};

const FILTER_PLACEMENT_KEY: &str = "filter_placement";
const SETTINGS_FILE: &str = "settings.conf";
const DOCK_LAYOUT_FILE: &str = "dock-layout.json";
const RECENT_FILES_FILE: &str = "recent-files.json";
const SEARCH_HISTORY_FILE: &str = "search-history.json";
const THEME_FILE: &str = "theme.conf";
const MAX_RECENT_FILES: usize = 10;
pub const MAX_SEARCH_HISTORY: usize = 20;

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

pub fn load_search_history() -> Vec<String> {
    read_cache_file(SEARCH_HISTORY_FILE)
        .map(|contents| parse_search_history(&contents))
        .unwrap_or_default()
}

pub fn save_search_history(queries: &[String]) -> io::Result<()> {
    let path = cache_file_path(SEARCH_HISTORY_FILE).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "executable cache directory not found",
        )
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = serde_json::to_string_pretty(queries)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    std::fs::write(path, contents)
}

pub fn load_theme_name() -> String {
    read_cache_file(THEME_FILE)
        .and_then(|contents| parse_theme_name(&contents))
        .unwrap_or_else(|| crate::theme::DEFAULT_DARK_THEME.to_string())
}

pub fn save_theme_name(name: &str) -> io::Result<()> {
    let path = cache_file_path(THEME_FILE).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "executable cache directory not found",
        )
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, name)
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

fn parse_theme_name(contents: &str) -> Option<String> {
    let name = contents.trim();
    if name.is_empty() {
        return None;
    }
    match name.to_ascii_lowercase().as_str() {
        "light" => Some(crate::theme::DEFAULT_LIGHT_THEME.to_string()),
        "dark" => Some(crate::theme::DEFAULT_DARK_THEME.to_string()),
        _ => Some(name.to_string()),
    }
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

fn parse_search_history(contents: &str) -> Vec<String> {
    let queries = serde_json::from_str::<Vec<String>>(contents).unwrap_or_default();
    let mut unique = Vec::with_capacity(queries.len().min(MAX_SEARCH_HISTORY));
    for query in queries {
        let query = query.trim();
        if !query.is_empty() && !unique.iter().any(|item| item == query) {
            unique.push(query.to_string());
            if unique.len() == MAX_SEARCH_HISTORY {
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

    #[test]
    fn search_history_is_trimmed_deduplicated_and_limited() {
        let queries = (0..=MAX_SEARCH_HISTORY)
            .map(|index| format!("query {index}"))
            .collect::<Vec<_>>();
        let mut json_queries = vec!["  first & second  ".to_string(), String::new()];
        json_queries.push("first & second".to_string());
        json_queries.extend(queries);

        let history = parse_search_history(&serde_json::to_string(&json_queries).unwrap());

        assert_eq!(history.first().map(String::as_str), Some("first & second"));
        assert_eq!(history.len(), MAX_SEARCH_HISTORY);
        assert_eq!(
            history
                .iter()
                .filter(|query| query.as_str() == "first & second")
                .count(),
            1
        );
    }

    #[test]
    fn invalid_search_history_json_is_ignored() {
        assert!(parse_search_history("not json").is_empty());
    }

    #[test]
    fn parses_theme_name_and_migrates_legacy_modes() {
        assert_eq!(
            parse_theme_name("light\n").as_deref(),
            Some("Default Light")
        );
        assert_eq!(parse_theme_name("DARK").as_deref(), Some("Default Dark"));
        assert_eq!(parse_theme_name("Ayu Dark").as_deref(), Some("Ayu Dark"));
        assert_eq!(parse_theme_name(" \n"), None);
    }
}
