//! Extract tabular values from text (regex) and JSON/NDJSON (object nodes).

use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use logd_core::{Encoding, FileSource, LineIndex};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecordTable {
    pub columns: Vec<String>,
    pub rows: Arc<Vec<Vec<String>>>,
    pub error: Option<String>,
    pub scanned: usize,
    pub matched: usize,
}

/// Text logs use the supplied regex. JSON and NDJSON are parsed structurally;
/// their object members become columns and the regex is not applied.
pub fn extract_table(input: &str, pattern: &str, max_rows: usize) -> RecordTable {
    let mut table = RecordTable::default();
    let pattern = pattern.trim();
    // A non-empty pattern explicitly selects text-log mode. This avoids a
    // mixed/JSON-looking file accidentally swallowing a user regex.
    let whole_json = pattern
        .is_empty()
        .then(|| serde_json::from_str::<Value>(input.trim()).ok())
        .flatten();
    let looks_ndjson = pattern.is_empty() && whole_json.is_none() && {
        let mut lines = input
            .lines()
            .take(max_rows)
            .filter(|line| !line.trim().is_empty())
            .peekable();
        lines.peek().is_some()
            && lines.all(|line| serde_json::from_str::<Value>(line.trim()).is_ok())
    };
    let mut records = Vec::<BTreeMap<String, String>>::new();
    if let Some(value) = whole_json {
        match value {
            Value::Array(values) => {
                table.scanned = values.len().min(max_rows);
                records.extend(values.into_iter().take(max_rows).filter_map(json_record));
                table.matched = records.len();
            }
            value => {
                table.scanned = 1;
                if let Some(record) = json_record(value) {
                    records.push(record);
                    table.matched = 1;
                }
            }
        }
    } else if looks_ndjson {
        for line in input.lines() {
            table.scanned += 1;
            if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
                if let Some(record) = json_record(value) {
                    records.push(record);
                    table.matched += 1;
                    if records.len() >= max_rows {
                        break;
                    }
                }
            }
        }
    } else if !pattern.is_empty() {
        let regex = match Regex::new(pattern) {
            Ok(regex) => regex,
            Err(error) => {
                table.error = Some(error.to_string());
                return table;
            }
        };
        table.columns = regex
            .capture_names()
            .skip(1)
            .enumerate()
            .map(|(index, name)| {
                name.map(str::to_owned)
                    .unwrap_or_else(|| format!("group_{}", index + 1))
            })
            .collect();
        if table.columns.is_empty() {
            table.columns.push("match".into());
        }
        for line in input.lines() {
            table.scanned += 1;
            let Some(captures) = regex.captures(line) else {
                continue;
            };
            let mut record = BTreeMap::new();
            let names: Vec<_> = regex.capture_names().skip(1).collect();
            if names.iter().any(|name| name.is_some()) {
                for (index, name) in names.into_iter().enumerate() {
                    let key = name
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("group_{}", index + 1));
                    record.insert(
                        key,
                        captures
                            .get(index + 1)
                            .map(|m| m.as_str())
                            .unwrap_or_default()
                            .to_owned(),
                    );
                }
            } else if regex.captures_len() > 1 {
                for index in 1..regex.captures_len() {
                    record.insert(
                        format!("group_{index}"),
                        captures
                            .get(index)
                            .map(|m| m.as_str())
                            .unwrap_or_default()
                            .to_owned(),
                    );
                }
            } else if let Some(matched) = captures.get(0) {
                record.insert("match".into(), matched.as_str().into());
            }
            if !record.is_empty() {
                records.push(record);
                table.matched += 1;
                if records.len() >= max_rows {
                    break;
                }
            }
        }
    }
    if table.columns.is_empty() {
        let mut columns = BTreeSet::new();
        for record in &records {
            columns.extend(record.keys().cloned());
        }
        table.columns = columns.into_iter().collect();
    }
    table.rows = Arc::new(
        records
            .into_iter()
            .map(|record| {
                table
                    .columns
                    .iter()
                    .map(|column| record.get(column).cloned().unwrap_or_default())
                    .collect()
            })
            .collect(),
    );
    table
}

/// Extract a text log without materializing the whole file in memory.
pub fn extract_text_lines<I>(lines: I, pattern: &str, max_rows: usize) -> RecordTable
where
    I: IntoIterator<Item = String>,
{
    let mut table = RecordTable::default();
    let regex = match Regex::new(pattern.trim()) {
        Ok(regex) => regex,
        Err(error) => {
            table.error = Some(error.to_string());
            return table;
        }
    };
    table.columns = regex
        .capture_names()
        .skip(1)
        .enumerate()
        .map(|(index, name)| {
            name.map(str::to_owned)
                .unwrap_or_else(|| format!("group_{}", index + 1))
        })
        .collect();
    if table.columns.is_empty() {
        table.columns.push("match".into());
    }
    let mut records = Vec::<BTreeMap<String, String>>::new();
    for line in lines {
        table.scanned += 1;
        let Some(captures) = regex.captures(&line) else {
            continue;
        };
        let mut record = BTreeMap::new();
        if regex.captures_len() > 1 {
            for index in 1..regex.captures_len() {
                let key = regex
                    .capture_names()
                    .nth(index)
                    .flatten()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("group_{index}"));
                record.insert(
                    key,
                    captures
                        .get(index)
                        .map(|m| m.as_str())
                        .unwrap_or_default()
                        .to_owned(),
                );
            }
        } else if let Some(matched) = captures.get(0) {
            record.insert("match".into(), matched.as_str().into());
        }
        if !record.is_empty() {
            records.push(record);
            table.matched += 1;
            if records.len() >= max_rows {
                break;
            }
        }
    }
    table.rows = Arc::new(
        records
            .into_iter()
            .map(|record| {
                table
                    .columns
                    .iter()
                    .map(|column| record.get(column).cloned().unwrap_or_default())
                    .collect()
            })
            .collect(),
    );
    table
}

/// Scan an indexed/memory-mapped log in the background without copying the
/// complete file into a single String.
pub fn extract_text_source(
    source: Arc<FileSource>,
    index: Arc<LineIndex>,
    encoding: Encoding,
    pattern: &str,
    max_rows: usize,
) -> RecordTable {
    let data = source.data();
    let mut spans = Vec::with_capacity(1);
    let lines = (0..index.total_lines).filter_map(|line| {
        index.line_spans(data, line, 1, &mut spans);
        let &(start, end) = spans.first()?;
        let mut start = start as usize;
        let end = end as usize;
        if start == 0 {
            start = source.bom_len().min(end);
        }
        Some(encoding.decode_bytes(&data[start..end]).into_owned())
    });
    extract_text_lines(lines, pattern, max_rows)
}

/// Extract a regex or JSON/NDJSON table from a memory-mapped source. The
/// caller must run this on a background executor: JSON arrays still require a
/// whole-document decode, while ordinary logs are rejected from a small
/// prefix without materializing the complete file.
pub fn extract_source(
    source: Arc<FileSource>,
    index: Arc<LineIndex>,
    encoding: Encoding,
    pattern: &str,
    max_rows: usize,
) -> RecordTable {
    if !pattern.trim().is_empty() {
        return extract_text_source(source, index, encoding, pattern, max_rows);
    }

    let prefix_end = source.len().min(64 * 1024);
    let prefix = source.decode(0, prefix_end);
    if !matches!(
        prefix.chars().find(|ch| !ch.is_whitespace()),
        Some('{' | '[')
    ) {
        return RecordTable::default();
    }

    let input = source.decode(0, source.len());
    extract_table(&input, "", max_rows)
}

fn json_record(value: Value) -> Option<BTreeMap<String, String>> {
    let Value::Object(object) = value else {
        return None;
    };
    let mut record = BTreeMap::new();
    for (key, value) in object {
        let rendered = match value {
            Value::String(value) => value,
            Value::Null => String::new(),
            Value::Bool(value) => value.to_string(),
            Value::Number(value) => value.to_string(),
            Value::Array(value) => serde_json::to_string(&value).unwrap_or_default(),
            Value::Object(value) => serde_json::to_string(&value).unwrap_or_default(),
        };
        record.insert(key, rendered);
    }
    (!record.is_empty()).then_some(record)
}

#[cfg(test)]
mod tests {
    use super::extract_table;
    #[test]
    fn named_capture_groups_form_columns() {
        let table = extract_table(
            "level=INFO user=alice\nlevel=ERROR user=bob",
            r"level=(?P<level>\w+) user=(?P<user>\w+)",
            100,
        );
        assert_eq!(table.columns, ["level", "user"]);
        assert_eq!(table.rows[1], ["ERROR", "bob"]);
    }

    #[test]
    fn angle_named_capture_groups_form_columns() {
        let table = extract_table(
            "cur(s(1692)0.020000s,g[3.42 14016],dmy1128)",
            r"cur\(s\((?<sensor_id>\d+)\)(?<exposure>[\d.]+)s,g\[(?<gain>[\d.]+)\s+(?<code>\d+)\],dmy(?<dmy>\d+)\)",
            100,
        );
        assert_eq!(table.rows[0], ["1692", "0.020000", "3.42", "14016", "1128"]);
    }
    #[test]
    fn ndjson_uses_object_nodes() {
        let table = extract_table("{\"a\":1,\"nested\":{\"b\":\"x\"}}\n{\"a\":2}", "", 100);
        assert_eq!(table.columns, ["a", "nested"]);
        assert_eq!(table.rows[0], ["1", "{\"b\":\"x\"}"]);
    }
    #[test]
    fn json_array_is_supported() {
        let table = extract_table(r#"[{"id":1},{"id":2,"ok":true}]"#, "", 100);
        assert_eq!(table.columns, ["id", "ok"]);
        assert_eq!(table.rows[1], ["2", "true"]);
    }

    #[test]
    fn supplied_pattern_matches_sample_log() {
        let input = "M079F6D  09-04 10:46:59.967   842 26978 D Libae:[Sub_fun]: 493,aec_lcg_alg_status_printf:ae.m,frm_id,0,unstb,near_stb,unlock,100Hz,sensor_fps[0,30],fps[14,30]:30.00,cur(s(1692)0.020000s,g[3.42 14016],dmy1128),nxt:idx241.88";
        let table = extract_table(
            input,
            r"cur\(s\((\d+)\)([\d.]+)s,g\[([\d.]+)\s+(\d+)\],dmy(\d+)\)",
            100,
        );
        assert_eq!(table.rows[0], ["1692", "0.020000", "3.42", "14016", "1128"]);
        assert_eq!(table.scanned, 1);
        assert_eq!(table.matched, 1);
    }

    #[test]
    fn row_limit_does_not_limit_lines_scanned_before_a_match() {
        let input = format!("{}value=42", "irrelevant\n".repeat(6_000));
        let table = extract_table(&input, r"value=(\d+)", 5_000);
        assert_eq!(table.rows.as_ref(), &vec![vec!["42".to_owned()]]);
        assert_eq!(table.scanned, 6_001);
    }

    #[test]
    fn streaming_text_extraction_preserves_named_and_unnamed_columns() {
        let lines = vec![
            "cur(s(1692)0.020000s,g[3.42 14016],dmy1128)".to_owned(),
            "noise".to_owned(),
        ];
        let table = super::extract_text_lines(
            lines,
            r"cur\(s\((?<sensor_id>\d+)\)([\d.]+)s,g\[(?<gain>[\d.]+)\s+(\d+)\],dmy(?<dmy>\d+)\)",
            100,
        );
        assert_eq!(
            table.columns,
            ["sensor_id", "group_2", "gain", "group_4", "dmy"]
        );
        assert_eq!(table.matched, 1);
    }
}
