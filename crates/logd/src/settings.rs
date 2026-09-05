use std::io;
use std::path::PathBuf;

use gpui_component::dock::DockPlacement;

const FILTER_PLACEMENT_KEY: &str = "filter_placement";

pub fn load_filter_placement() -> DockPlacement {
    settings_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|contents| parse_filter_placement(&contents))
        .unwrap_or(DockPlacement::Right)
}

pub fn save_filter_placement(placement: DockPlacement) -> io::Result<()> {
    let path = settings_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "local data directory not found"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        path,
        format!("{FILTER_PLACEMENT_KEY}={}\n", placement_name(placement)),
    )
}

fn settings_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|path| path.join("logd").join("settings.conf"))
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

fn placement_name(placement: DockPlacement) -> &'static str {
    match placement {
        DockPlacement::Left => "left",
        DockPlacement::Right => "right",
        DockPlacement::Bottom => "bottom",
        DockPlacement::Center => "right",
    }
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
}
