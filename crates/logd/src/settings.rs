use std::io;
use std::path::PathBuf;

use gpui_component::dock::{DockAreaState, DockPlacement};

const FILTER_PLACEMENT_KEY: &str = "filter_placement";
const DOCK_LAYOUT_FILE: &str = "dock-layout.json";

pub fn load_filter_placement() -> DockPlacement {
    settings_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|contents| parse_filter_placement(&contents))
        .unwrap_or(DockPlacement::Right)
}

pub fn load_dock_layout() -> Option<DockAreaState> {
    let contents = std::fs::read_to_string(dock_layout_path()?).ok()?;
    serde_json::from_str(&contents).ok()
}

pub fn save_dock_layout(state: &DockAreaState) -> io::Result<()> {
    let path = dock_layout_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "local data directory not found"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = serde_json::to_string_pretty(state)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    std::fs::write(path, contents)
}

fn settings_path() -> Option<PathBuf> {
    app_data_dir().map(|path| path.join("settings.conf"))
}

fn dock_layout_path() -> Option<PathBuf> {
    app_data_dir().map(|path| path.join(DOCK_LAYOUT_FILE))
}

fn app_data_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|path| path.join("logd"))
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
}
