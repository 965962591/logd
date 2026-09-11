//! Extract tabular values from text (regex) and JSON/NDJSON (object nodes).

use regex::Regex;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecordTable {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub error: Option<String>,
}

/// Text logs use the supplied regex. JSON and NDJSON are parsed structurally;
/// their object members become columns and the regex is not applied.
pub fn extract_table(input: &str, pattern: &str, max_rows: usize) -> RecordTable {
    let mut table = RecordTable::default();
    let pattern = pattern.trim();
    // A non-empty pattern explicitly selects text-log mode. This avoids a
    // mixed/JSON-looking file accidentally swallowing a user regex.
    let json_lines = input
        .lines()
        .take(max_rows)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let looks_json = pattern.is_empty()
        && (serde_json::from_str::<Value>(input.trim()).is_ok()
            || (!json_lines.is_empty()
                && json_lines
                    .iter()
                    .all(|line| serde_json::from_str::<Value>(line.trim()).is_ok())));
    let mut records = Vec::<BTreeMap<String, String>>::new();
    if looks_json {
        if let Ok(value) = serde_json::from_str::<Value>(input.trim()) {
            match value {
                Value::Array(values) => {
                    records.extend(values.into_iter().take(max_rows).filter_map(json_record))
                }
                value => {
                    if let Some(record) = json_record(value) {
                        records.push(record);
                    }
                }
            }
        } else {
            for line in input.lines().take(max_rows) {
                if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
                    if let Some(record) = json_record(value) {
                        records.push(record);
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
        for line in input.lines().take(max_rows) {
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
            }
        }
    }
    let mut columns = BTreeSet::new();
    for record in &records {
        columns.extend(record.keys().cloned());
    }
    table.columns = columns.into_iter().collect();
    table.rows = records
        .into_iter()
        .map(|record| {
            table
                .columns
                .iter()
                .map(|column| record.get(column).cloned().unwrap_or_default())
                .collect()
        })
        .collect();
    table
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
        let input = "cur(s(1692)0.020000s,g[3.42 14016],dmy1128)";
        let table = extract_table(
            input,
            r"cur\(s\((\d+)\)([\d.]+)s,g\[([\d.]+)\s+(\d+)\],dmy(\d+)\)",
            100,
        );
        assert_eq!(table.rows[0], ["1692", "0.020000", "3.42", "14016", "1128"]);
    }
}
