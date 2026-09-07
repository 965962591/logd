//! Native `.logd` filter configuration format.
//!
//! `.tat` remains supported as an import/export format for compatibility with
//! TextAnalysisTool.NET.  The native format is deliberately JSON so it can
//! carry logd-only presentation settings without inventing an XML dialect.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::matcher::FilterSpec;
use crate::tat::TatFile;

const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogdFile {
    pub version: u32,
    pub show_only_filtered: bool,
    pub filters: Vec<FilterSpec>,
}

impl Default for LogdFile {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            show_only_filtered: false,
            filters: Vec::new(),
        }
    }
}

impl LogdFile {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).with_context(|| format!("读不了 {}", path.display()))?;
        Self::parse(&bytes).with_context(|| format!("解析 {} 失败", path.display()))
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut file: Self = serde_json::from_slice(bytes).context("不是有效的 .logd JSON")?;
        if file.version == 0 {
            file.version = CURRENT_VERSION;
        }
        Ok(file)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let bytes = serde_json::to_vec_pretty(self).context("编码 .logd JSON 失败")?;
        std::fs::write(path, bytes).with_context(|| format!("写不了 {}", path.display()))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("LogdFile is always JSON serializable")
    }
}

impl From<TatFile> for LogdFile {
    fn from(file: TatFile) -> Self {
        Self {
            show_only_filtered: file.show_only_filtered,
            filters: file.filters,
            ..Default::default()
        }
    }
}

impl From<LogdFile> for TatFile {
    fn from(file: LogdFile) -> Self {
        Self {
            show_only_filtered: file.show_only_filtered,
            filters: file.filters,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::FilterSpec;

    #[test]
    fn round_trip_preserves_filter_styles() {
        let file = LogdFile {
            show_only_filtered: true,
            filters: vec![FilterSpec {
                text: "AEtable".into(),
                bold: true,
                font_size: Some(16),
                ..Default::default()
            }],
            ..Default::default()
        };
        let back = LogdFile::parse(file.to_json().as_bytes()).unwrap();
        assert_eq!(file, back);
    }

    #[test]
    fn missing_optional_fields_use_defaults() {
        let file = LogdFile::parse(br#"{"filters":[{"text":"x"}]}"#).unwrap();
        assert_eq!(file.filters[0].font_size, None);
        assert!(!file.filters[0].bold);
    }
}
