//! TextAnalysisTool.NET `.tat` 配置的读写。
//!
//! 目标是**双向兼容**：读得懂现有文件，写出来的文件 TAT.NET 还能打开。
//! 做法是保留所有不认识的属性（[`FilterSpec::extra`]），logd 自己的新特性
//! 用 `logd_` 前缀的普通属性承载（例如高亮样式和搜索作用域）。
//!
//! 为什么不用 XML namespace（`logd:mode`）：带前缀但没声明 `xmlns:logd` 是非法 XML，
//! 声明了又可能让 TAT.NET 的 XmlSerializer 报错。普通属性名任何宽容解析器都会忽略。
//!
//! 手写 writer 而不是 serde，是因为要控制属性顺序、保留未知属性、
//! 并复刻 TAT.NET 的缩进风格，方便 diff。

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::matcher::{FilterScope, FilterSpec, HighlightMode};

const ROOT: &str = "TextAnalysisTool.NET";
const DEFAULT_VERSION: &str = "2020-12-17";
const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TatFile {
    pub version: String,
    /// 根元素的 `showOnlyFilteredLines`，映射到 UI 的「仅显示筛选结果」开关。
    pub show_only_filtered: bool,
    pub filters: Vec<FilterSpec>,
    /// 根元素上不认识的属性，回写时保留。
    pub root_extra: Vec<(String, String)>,
}

impl Default for TatFile {
    fn default() -> Self {
        Self {
            version: DEFAULT_VERSION.to_string(),
            show_only_filtered: false,
            filters: Vec::new(),
            root_extra: Vec::new(),
        }
    }
}

impl TatFile {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).with_context(|| format!("读不了 {}", path.display()))?;
        Self::parse(&bytes).with_context(|| format!("解析 {} 失败", path.display()))
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let bytes = bytes.strip_prefix(BOM).unwrap_or(bytes);
        let mut reader = Reader::from_reader(bytes);
        let mut buf = Vec::new();
        let mut out = TatFile {
            filters: Vec::new(),
            ..Default::default()
        };
        let mut saw_root = false;

        loop {
            let ev = reader
                .read_event_into(&mut buf)
                .map_err(|e| anyhow!("XML 位置 {}: {e}", reader.buffer_position()))?;
            match ev {
                Event::Start(e) | Event::Empty(e) => {
                    let name = e.name();
                    match name.as_ref() {
                        b"TextAnalysisTool.NET" => {
                            saw_root = true;
                            for (k, v) in attrs(&e)? {
                                match k.as_str() {
                                    "version" => out.version = v,
                                    "showOnlyFilteredLines" => out.show_only_filtered = parse_bool(&v),
                                    _ => out.root_extra.push((k, v)),
                                }
                            }
                        }
                        b"filter" => out.filters.push(parse_filter(&attrs(&e)?)),
                        _ => {}
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }

        if !saw_root {
            return Err(anyhow!("不是 .tat 文件：找不到根元素 <{ROOT}>"));
        }
        Ok(out)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let mut bytes = BOM.to_vec();
        bytes.extend_from_slice(self.to_xml().as_bytes());
        std::fs::write(path, bytes).with_context(|| format!("写不了 {}", path.display()))
    }

    /// 不含 BOM 的 XML 文本。行尾用 CRLF，和 TAT.NET 自己的输出一致。
    pub fn to_xml(&self) -> String {
        let mut s = String::with_capacity(256 + self.filters.len() * 160);
        s.push_str("<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\r\n");
        s.push('<');
        s.push_str(ROOT);
        attr(&mut s, "version", &self.version);
        attr(
            &mut s,
            "showOnlyFilteredLines",
            if self.show_only_filtered { "True" } else { "False" },
        );
        for (k, v) in &self.root_extra {
            attr(&mut s, k, v);
        }
        s.push_str(">\r\n  <filters>\r\n");

        for f in &self.filters {
            s.push_str("    <filter");
            attr(&mut s, "enabled", yn(f.enabled));
            attr(&mut s, "excluding", yn(f.excluding));
            attr(&mut s, "description", &f.description);
            if let Some(c) = f.fore {
                attr(&mut s, "foreColor", &format!("{c:06x}"));
            }
            if let Some(c) = f.back {
                attr(&mut s, "backColor", &format!("{c:06x}"));
            }
            attr(&mut s, "type", &f.kind);
            attr(&mut s, "case_sensitive", yn(f.case_sensitive));
            attr(&mut s, "regex", yn(f.regex));
            attr(&mut s, "text", &f.text);
            for (k, v) in &f.extra {
                attr(&mut s, k, v);
            }
            // logd 扩展：只在偏离缺省时才写，保持文件干净、diff 最小
            if f.mode != HighlightMode::default() {
                attr(&mut s, "logd_mode", f.mode.as_str());
            }
            if f.bold {
                attr(&mut s, "logd_bold", "y");
            }
            if f.italic {
                attr(&mut s, "logd_italic", "y");
            }
            if f.scope != FilterScope::default() {
                attr(
                    &mut s,
                    "logd_scope",
                    match f.scope {
                        FilterScope::AllFiles => "all",
                        FilterScope::CurrentFile => "current",
                    },
                );
            }
            s.push_str(" />\r\n");
        }

        s.push_str("  </filters>\r\n</");
        s.push_str(ROOT);
        s.push_str(">\r\n");
        s
    }
}

fn attrs(e: &quick_xml::events::BytesStart<'_>) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for a in e.attributes() {
        let a = a.map_err(|err| anyhow!("属性解析失败: {err}"))?;
        let k = String::from_utf8_lossy(a.key.as_ref()).into_owned();
        let v = a
            .unescape_value()
            .map_err(|err| anyhow!("属性 {k} 反转义失败: {err}"))?
            .into_owned();
        out.push((k, v));
    }
    Ok(out)
}

fn parse_filter(attrs: &[(String, String)]) -> FilterSpec {
    let mut f = FilterSpec {
        // .tat 里 enabled 是显式的；缺省按 TAT.NET 的行为当作未启用更安全，
        // 但样本里从来都带这个属性，这里给 false 只是兜底。
        enabled: false,
        ..Default::default()
    };
    for (k, v) in attrs {
        match k.as_str() {
            "enabled" => f.enabled = parse_bool(v),
            "excluding" => f.excluding = parse_bool(v),
            "description" => f.description = v.clone(),
            "foreColor" => f.fore = parse_color(v),
            "backColor" => f.back = parse_color(v),
            "type" => f.kind = v.clone(),
            "case_sensitive" => f.case_sensitive = parse_bool(v),
            "regex" => f.regex = parse_bool(v),
            "text" => f.text = v.clone(),
            "logd_mode" => f.mode = HighlightMode::parse(v),
            "logd_bold" => f.bold = parse_bool(v),
            "logd_italic" => f.italic = parse_bool(v),
            "logd_scope" => {
                f.scope = match v.trim().to_ascii_lowercase().as_str() {
                    "current" | "file" => FilterScope::CurrentFile,
                    _ => FilterScope::AllFiles,
                }
            }
            _ => f.extra.push((k.clone(), v.clone())),
        }
    }
    f
}

/// `.tat` 里 filter 用 `y`/`n`，根元素用 `True`/`False`，两种都认。
fn parse_bool(v: &str) -> bool {
    matches!(v.trim(), "y" | "Y" | "yes" | "true" | "True" | "TRUE" | "1")
}

fn yn(b: bool) -> &'static str {
    if b {
        "y"
    } else {
        "n"
    }
}

/// 无 `#` 的 6 位 hex RGB。
fn parse_color(v: &str) -> Option<u32> {
    let v = v.trim().trim_start_matches('#');
    if v.len() != 6 {
        return None;
    }
    u32::from_str_radix(v, 16).ok()
}

fn attr(out: &mut String, key: &str, value: &str) {
    out.push(' ');
    out.push_str(key);
    out.push_str("=\"");
    out.push_str(&quick_xml::escape::escape(value));
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    const AE_LOG: &[u8] = include_bytes!("../../../tat/ae_log.tat");
    const SPRD: &[u8] = include_bytes!("../../../tat/展锐ae.tat");

    #[test]
    fn parses_real_ae_log_tat() {
        let t = TatFile::parse(AE_LOG).unwrap();
        assert_eq!(t.version, "2020-12-17");
        assert!(!t.show_only_filtered);
        assert_eq!(t.filters.len(), 24);

        let f = &t.filters[0];
        assert!(f.enabled);
        assert!(!f.excluding);
        assert_eq!(f.description, "ae_algo统计信息");
        assert_eq!(f.text, "updateAEInfo2ISP");
        assert_eq!(f.fore, Some(0xff0000));
        assert_eq!(f.back, None);
        assert_eq!(f.kind, "matches_text");
        assert!(!f.regex);
        assert!(!f.case_sensitive);
        // 没有 logd_ 属性时应落到缺省的行模式
        assert_eq!(f.mode, HighlightMode::Line);

        // 同时带前景和背景的那条
        let f = t.filters.iter().find(|f| f.text == "AEtable").unwrap();
        assert_eq!(f.fore, Some(0xfa8072));
        assert_eq!(f.back, Some(0xffff00));

        // 关键字里带方括号，不能被 XML 或正则逻辑搞坏
        assert!(t
            .filters
            .iter()
            .any(|f| f.text == "[getAEPLineMappingID]"));
    }

    #[test]
    fn parses_show_only_filtered_true() {
        let t = TatFile::parse(SPRD).unwrap();
        assert!(t.show_only_filtered, "展锐ae.tat 的 showOnlyFilteredLines 是 True");
        assert!(!t.filters.is_empty());
    }

    #[test]
    fn round_trip_preserves_everything() {
        let a = TatFile::parse(AE_LOG).unwrap();
        let b = TatFile::parse(a.to_xml().as_bytes()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn logd_extensions_round_trip() {
        let mut t = TatFile::parse(AE_LOG).unwrap();
        t.filters[0].mode = HighlightMode::Field;
        t.filters[0].bold = true;
        t.filters[1].italic = true;
        t.filters[2].scope = FilterScope::CurrentFile;
        t.show_only_filtered = true;

        let xml = t.to_xml();
        assert!(xml.contains("logd_mode=\"field\""));
        assert!(xml.contains("logd_bold=\"y\""));
        assert!(xml.contains("logd_scope=\"current\""));
        assert!(xml.contains("showOnlyFilteredLines=\"True\""));

        let back = TatFile::parse(xml.as_bytes()).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn unknown_attributes_are_preserved() {
        let src = r#"<?xml version="1.0" encoding="utf-8" standalone="yes"?>
<TextAnalysisTool.NET version="2020-12-17" showOnlyFilteredLines="False" futureThing="42">
  <filters>
    <filter enabled="y" excluding="n" description="" type="matches_text"
            case_sensitive="n" regex="n" text="abc" someNewAttr="keepme" />
  </filters>
</TextAnalysisTool.NET>"#;
        let t = TatFile::parse(src.as_bytes()).unwrap();
        assert_eq!(t.root_extra, vec![("futureThing".into(), "42".into())]);
        assert_eq!(
            t.filters[0].extra,
            vec![("someNewAttr".into(), "keepme".into())]
        );
        assert!(t.to_xml().contains("someNewAttr=\"keepme\""));
        assert!(t.to_xml().contains("futureThing=\"42\""));
    }

    #[test]
    fn escapes_special_chars_in_text() {
        let t = TatFile {
            filters: vec![FilterSpec {
                text: r#"a<b>&"c""#.into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let back = TatFile::parse(t.to_xml().as_bytes()).unwrap();
        assert_eq!(back.filters[0].text, r#"a<b>&"c""#);
    }

    #[test]
    fn handles_bom() {
        let mut with_bom = BOM.to_vec();
        with_bom.extend_from_slice(&TatFile::default().to_xml().into_bytes());
        assert!(TatFile::parse(&with_bom).is_ok());
    }

    #[test]
    fn rejects_non_tat_xml() {
        assert!(TatFile::parse(b"<?xml version=\"1.0\"?><other/>").is_err());
    }

    #[test]
    fn color_parsing() {
        assert_eq!(parse_color("ff0000"), Some(0xff0000));
        assert_eq!(parse_color("#00ff00"), Some(0x00ff00));
        assert_eq!(parse_color(""), None);
        assert_eq!(parse_color("zzzzzz"), None);
        assert_eq!(parse_color("fff"), None);
    }
}
