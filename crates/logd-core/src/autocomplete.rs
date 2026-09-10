//! 日志搜索框的模糊匹配与字段联想。
//!
//! 该模块故意不依赖 UI。调用方可以在后台线程构建 [`FieldCatalog`]，
//! 然后在输入变化时调用 [`FieldCatalog::suggest`] 获取联想项。

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::logline::{self, LogLine};
use crate::source::Encoding;

const DEFAULT_MAX_LINES: usize = 100_000;
const DEFAULT_MAX_VALUES_PER_FIELD: usize = 256;
const MAX_FUZZY_PATTERN_BYTES: usize = 64;

/// 可用于查询表达式的日志字段。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogField {
    Level,
    Pid,
    Tid,
    Tag,
    Message,
}

impl LogField {
    pub const ALL: [Self; 5] = [Self::Level, Self::Pid, Self::Tid, Self::Tag, Self::Message];

    /// Canonical spelling accepted by the query parser.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Level => "level",
            Self::Pid => "pid",
            Self::Tid => "tid",
            Self::Tag => "tag",
            Self::Message => "msg",
        }
    }

    const fn completion_operator(self) -> &'static str {
        match self {
            Self::Tag | Self::Message => ":",
            Self::Level | Self::Pid | Self::Tid => "=",
        }
    }

    fn parse_prefix(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "level" | "lvl" | "priority" => Some(Self::Level),
            "pid" => Some(Self::Pid),
            "tid" => Some(Self::Tid),
            "tag" => Some(Self::Tag),
            "msg" | "message" | "text" => Some(Self::Message),
            _ => None,
        }
    }
}

/// 一个可直接放回搜索框的联想项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    pub field: Option<LogField>,
    pub value: String,
    pub expression: String,
    /// 越小越匹配。用于 UI 自行排序或显示匹配优先级。
    pub score: usize,
}

/// 从日志样本中收集字段值。每个字段有独立上限，避免超大日志导致内存增长。
#[derive(Debug)]
pub struct FieldCatalog {
    values: BTreeMap<LogField, BTreeSet<String>>,
    max_values_per_field: usize,
    matcher: Mutex<Matcher>,
}

impl Clone for FieldCatalog {
    fn clone(&self) -> Self {
        Self {
            values: self.values.clone(),
            max_values_per_field: self.max_values_per_field,
            matcher: Mutex::new(new_matcher()),
        }
    }
}

impl Default for FieldCatalog {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_VALUES_PER_FIELD)
    }
}

impl FieldCatalog {
    pub fn new(max_values_per_field: usize) -> Self {
        Self {
            values: BTreeMap::new(),
            max_values_per_field: max_values_per_field.max(1),
            matcher: Mutex::new(new_matcher()),
        }
    }

    /// 从最多 `max_lines` 行构建字段目录。
    pub fn from_data(data: &[u8], encoding: Encoding, max_lines: usize) -> Self {
        let mut catalog = Self::default();
        for line in data.split(|b| *b == b'\n').take(max_lines) {
            catalog.add_line(line, encoding);
        }
        catalog
    }

    /// 使用默认采样上限构建字段目录。
    pub fn from_sample(data: &[u8], encoding: Encoding) -> Self {
        Self::from_data(data, encoding, DEFAULT_MAX_LINES)
    }

    /// 将一行中的结构化字段和值加入目录。
    pub fn add_line(&mut self, line: &[u8], encoding: Encoding) {
        let line = line.strip_suffix(&[b'\r']).unwrap_or(line);
        let Some(parsed) = logline::parse(line) else {
            return;
        };
        self.add_parsed(line, &parsed, encoding);
    }

    fn add_parsed(&mut self, line: &[u8], parsed: &LogLine, encoding: Encoding) {
        if let Some(level) = parsed.level {
            self.insert(LogField::Level, level.as_str().to_owned());
        }
        if let Some(pid) = parsed.pid {
            self.insert(LogField::Pid, pid.to_string());
        }
        if let Some(tid) = parsed.tid {
            self.insert(LogField::Tid, tid.to_string());
        }
        if parsed.tag.start < parsed.tag.end {
            let value = encoding
                .decode_bytes(parsed.tag_bytes(line))
                .trim()
                .to_owned();
            if !value.is_empty() {
                self.insert(LogField::Tag, value);
            }
        }
        // Message values are tokenized rather than storing entire long lines.
        // This makes `msg:` completion useful without making the catalog huge.
        let message = encoding.decode_bytes(parsed.message_bytes(line));
        for token in message.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-') {
            if token.chars().count() >= 2 && !token.chars().all(|ch| ch.is_ascii_digit()) {
                self.insert(LogField::Message, token.to_owned());
            }
        }
    }

    fn insert(&mut self, field: LogField, value: String) {
        let values = self.values.entry(field).or_default();
        if values.contains(&value) || values.len() < self.max_values_per_field {
            values.insert(value);
        }
    }

    pub fn values(&self, field: LogField) -> impl Iterator<Item = &str> {
        self.values
            .get(&field)
            .into_iter()
            .flat_map(|values| values.iter().map(String::as_str))
    }

    /// 根据当前输入返回最多 `limit` 个联想项。
    ///
    /// 输入形如 `tag:ae`、`level=` 时只联想对应字段；没有字段前缀时，
    /// 联想字段名以及所有字段中匹配的值。匹配支持大小写不敏感子序列和
    /// 小编辑距离，因此输入少量拼写错误也能得到结果。
    pub fn suggest(&self, input: &str, limit: usize) -> Vec<Suggestion> {
        if limit == 0 {
            return Vec::new();
        }
        let (expression_prefix, fragment) = split_completion_fragment(input);
        let (field, prefix) = split_field_prefix(fragment);
        let mut out = Vec::new();
        let atom = suggestion_atom(prefix);
        let mut char_buf = Vec::new();
        let mut matcher = self
            .matcher
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if field.is_none() {
            for candidate in LogField::ALL {
                let expression = format!("{}{}", candidate.name(), candidate.completion_operator());
                if let Some(score) = nucleo_score(&atom, &expression, &mut matcher, &mut char_buf) {
                    out.push(Suggestion {
                        field: Some(candidate),
                        value: candidate.name().to_owned(),
                        expression: format!("{expression_prefix}{expression}"),
                        score,
                    });
                }
            }
        }

        let fields = field
            .map(|f| vec![f])
            .unwrap_or_else(|| LogField::ALL.to_vec());
        for candidate in fields {
            for value in self.values(candidate) {
                let Some(score) = nucleo_score(&atom, value, &mut matcher, &mut char_buf) else {
                    continue;
                };
                out.push(Suggestion {
                    field: Some(candidate),
                    value: value.to_owned(),
                    expression: format!(
                        "{expression_prefix}{}",
                        format_field_expression(candidate, value)
                    ),
                    score,
                });
            }
        }
        out.sort_by(|a, b| {
            a.score
                .cmp(&b.score)
                .then_with(|| a.expression.cmp(&b.expression))
        });
        out.truncate(limit);
        out
    }
}

fn split_completion_fragment(input: &str) -> (&str, &str) {
    let start = input
        .char_indices()
        .rev()
        .find_map(|(index, ch)| matches!(ch, '&' | '|').then_some(index + ch.len_utf8()))
        .unwrap_or(0);
    let fragment = &input[start..];
    let whitespace = fragment.len() - fragment.trim_start().len();
    (&input[..start + whitespace], &fragment[whitespace..])
}

fn format_field_expression(field: LogField, value: &str) -> String {
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        format!("{}{}{}", field.name(), field.completion_operator(), value)
    } else {
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        format!(
            "{}{}\"{}\"",
            field.name(),
            field.completion_operator(),
            escaped
        )
    }
}

/// Compiled matcher used by title-bar fuzzy text queries.
///
/// It first checks a case-aware literal substring. For simple word-like patterns it then
/// checks similarly sized line tokens using subsequence and bounded edit-distance matching.
/// The caller owns the two scratch rows, so scanning a large file does not allocate per line.
#[derive(Clone, Debug)]
pub struct FuzzyPattern {
    pattern: Vec<u8>,
    case_sensitive: bool,
    allow_approximate: bool,
}

impl FuzzyPattern {
    pub fn new(mut pattern: Vec<u8>, case_sensitive: bool) -> Self {
        if !case_sensitive {
            pattern.make_ascii_lowercase();
        }
        let allow_approximate = pattern.is_ascii()
            && (3..=MAX_FUZZY_PATTERN_BYTES).contains(&pattern.len())
            && pattern.iter().copied().all(is_word_byte);
        Self {
            pattern,
            case_sensitive,
            allow_approximate,
        }
    }

    pub fn is_match(&self, line: &[u8], row: &mut Vec<usize>, next: &mut Vec<usize>) -> bool {
        if self.pattern.is_empty() {
            return true;
        }
        if contains_bytes(line, &self.pattern, self.case_sensitive) {
            return true;
        }
        if !self.allow_approximate {
            return false;
        }

        line.split(|byte| !is_word_byte(*byte))
            .any(|token| self.approximate_token_match(token, row, next))
    }

    fn approximate_token_match(
        &self,
        token: &[u8],
        row: &mut Vec<usize>,
        next: &mut Vec<usize>,
    ) -> bool {
        if token.is_empty() {
            return false;
        }
        let pattern_len = self.pattern.len();
        let max_gap = (pattern_len / 2).max(2);
        if token.len() <= pattern_len + max_gap
            && is_subsequence(&self.pattern, token, self.case_sensitive)
        {
            return true;
        }

        let allowed = (pattern_len.max(3) / 3).max(1);
        if token.len().abs_diff(pattern_len) > allowed {
            return false;
        }
        bounded_levenshtein(
            &self.pattern,
            token,
            self.case_sensitive,
            allowed,
            row,
            next,
        )
    }
}

#[inline]
fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte >= 0x80 || matches!(byte, b'_' | b'-')
}

#[inline]
fn folded(byte: u8, case_sensitive: bool) -> u8 {
    if case_sensitive {
        byte
    } else {
        byte.to_ascii_lowercase()
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8], case_sensitive: bool) -> bool {
    haystack.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(left, right)| folded(*left, case_sensitive) == *right)
    })
}

fn is_subsequence(pattern: &[u8], candidate: &[u8], case_sensitive: bool) -> bool {
    let mut at = 0;
    for byte in candidate {
        if folded(*byte, case_sensitive) == pattern[at] {
            at += 1;
            if at == pattern.len() {
                return true;
            }
        }
    }
    false
}

fn bounded_levenshtein(
    pattern: &[u8],
    candidate: &[u8],
    case_sensitive: bool,
    allowed: usize,
    row: &mut Vec<usize>,
    next: &mut Vec<usize>,
) -> bool {
    row.clear();
    row.extend(0..=candidate.len());
    next.resize(candidate.len() + 1, 0);
    for (i, left) in pattern.iter().enumerate() {
        next[0] = i + 1;
        let mut row_min = next[0];
        for (j, right) in candidate.iter().enumerate() {
            next[j + 1] = (row[j + 1] + 1)
                .min(next[j] + 1)
                .min(row[j] + usize::from(*left != folded(*right, case_sensitive)));
            row_min = row_min.min(next[j + 1]);
        }
        if row_min > allowed {
            return false;
        }
        std::mem::swap(row, next);
    }
    row[candidate.len()] <= allowed
}

/// 返回模糊匹配分数。`None` 表示差异太大。
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<usize> {
    let atom = suggestion_atom(query.trim());
    THREAD_MATCHER.with(|matcher| {
        let mut matcher = matcher.borrow_mut();
        let mut char_buf = Vec::new();
        nucleo_score(&atom, candidate, &mut matcher, &mut char_buf)
    })
}

pub fn fuzzy_match(query: &str, candidate: &str) -> bool {
    fuzzy_score(query, candidate).is_some()
}

thread_local! {
    static THREAD_MATCHER: RefCell<Matcher> = RefCell::new(new_matcher());
}

fn new_matcher() -> Matcher {
    Matcher::new(Config::DEFAULT.match_paths())
}

fn suggestion_atom(query: &str) -> Atom {
    Atom::new(
        query,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
        false,
    )
}

fn nucleo_score(
    atom: &Atom,
    candidate: &str,
    matcher: &mut Matcher,
    char_buf: &mut Vec<char>,
) -> Option<usize> {
    atom.score(Utf32Str::new(candidate, char_buf), matcher)
        // Suggestion scores are sorted ascending in the public API.
        .map(|score| usize::from(u16::MAX - score))
}

fn split_field_prefix(input: &str) -> (Option<LogField>, &str) {
    let trimmed = input.trim();
    let Some((name, value)) = trimmed.split_once(|c| c == ':' || c == '=' || c == '~') else {
        return (None, trimmed);
    };
    LogField::parse_prefix(name.trim()).map_or((None, trimmed), |field| (Some(field), value.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATA: &str = concat!(
        "01-02 03:04:05.678  1234  5678 D AeAlgo  : Magic gain=1024\\n",
        "01-02 03:04:06.000  1234  9999 W Hal3Av3 : parseMeta noise\\n",
    );

    #[test]
    fn catalog_collects_structured_values() {
        let catalog = FieldCatalog::from_sample(DATA.as_bytes(), Encoding::Utf8);
        assert!(catalog.values(LogField::Tag).any(|v| v == "AeAlgo"));
        assert!(catalog.values(LogField::Pid).any(|v| v == "1234"));
        assert!(catalog.values(LogField::Message).any(|v| v == "Magic"));
    }

    #[test]
    fn field_prefix_limits_suggestions() {
        let catalog = FieldCatalog::from_sample(DATA.as_bytes(), Encoding::Utf8);
        let out = catalog.suggest("tag:ae", 10);
        assert!(out.iter().any(|s| s.expression == "tag:AeAlgo"));
        assert!(out.iter().all(|s| s.expression.starts_with("tag:")));
    }

    #[test]
    fn fuzzy_matching_supports_subsequence_and_typo() {
        assert!(fuzzy_match("lvl", "level"));
        assert!(fuzzy_match("Aelgo", "AeAlgo"));
        assert!(!fuzzy_match("zzzz", "AeAlgo"));
    }

    #[test]
    fn compiled_fuzzy_pattern_matches_words_without_per_line_allocation() {
        let pattern = FuzzyPattern::new(b"Aelgo".to_vec(), false);
        let mut row = Vec::new();
        let mut next = Vec::new();
        assert!(pattern.is_match(b"01-02 D AeAlgo: initialized", &mut row, &mut next));
        assert!(!pattern.is_match(b"01-02 D Camera: initialized", &mut row, &mut next));
    }

    #[test]
    fn suggestions_quote_field_values_with_spaces() {
        assert_eq!(
            format_field_expression(LogField::Tag, "Audio Service"),
            "tag:\"Audio Service\""
        );
    }

    #[test]
    fn zero_line_sample_is_empty() {
        let catalog = FieldCatalog::from_data(DATA.as_bytes(), Encoding::Utf8, 0);
        assert_eq!(catalog.values(LogField::Tag).count(), 0);
    }

    #[test]
    fn suggestions_replace_only_the_active_boolean_fragment() {
        let catalog = FieldCatalog::from_sample(DATA.as_bytes(), Encoding::Utf8);
        let out = catalog.suggest("Magic & tag:ae", 10);
        assert!(out
            .iter()
            .any(|suggestion| suggestion.expression == "Magic & tag:AeAlgo"));
    }
}
