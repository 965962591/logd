//! 日志搜索框的模糊匹配与字段联想。
//!
//! 该模块故意不依赖 UI。调用方可以在后台线程构建 [`FieldCatalog`]，
//! 然后在输入变化时调用 [`FieldCatalog::suggest`] 获取联想项。

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};
use std::sync::Mutex;

use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::logline::{self, LogLine};
use crate::source::Encoding;

const DEFAULT_MAX_LINES: usize = 100_000;
const DEFAULT_MAX_VALUES_PER_FIELD: usize = 256;
const MAX_EXACT_VALUES_PER_FIELD: usize = DEFAULT_MAX_LINES;
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct ValueStats {
    count: u64,
    last_seen: u64,
}

/// Streaming heavy-hitter index. It keeps memory bounded while allowing values
/// that become frequent later in the log to replace one-off values seen early.
#[derive(Clone, Debug)]
struct RankedValueIndex {
    values: HashMap<String, ValueStats>,
    least: BinaryHeap<Reverse<(u64, u64, String)>>,
    capacity: usize,
}

impl RankedValueIndex {
    fn new(capacity: usize) -> Self {
        Self {
            values: HashMap::with_capacity(capacity),
            least: BinaryHeap::with_capacity(capacity),
            capacity,
        }
    }

    fn insert(&mut self, value: String, seen_at: u64) {
        if let Some(stats) = self.values.get_mut(&value) {
            stats.count = stats.count.saturating_add(1);
            stats.last_seen = seen_at;
            self.least
                .push(Reverse((stats.count, stats.last_seen, value)));
            self.compact_heap_if_needed();
            return;
        }

        let count = if self.values.len() < self.capacity {
            1
        } else {
            let (removed, stats) = self.pop_least_current().expect("full index has a value");
            self.values.remove(&removed);
            stats.count.saturating_add(1)
        };
        self.values.insert(
            value.clone(),
            ValueStats {
                count,
                last_seen: seen_at,
            },
        );
        self.least.push(Reverse((count, seen_at, value)));
        self.compact_heap_if_needed();
    }

    fn pop_least_current(&mut self) -> Option<(String, ValueStats)> {
        while let Some(Reverse((count, last_seen, value))) = self.least.pop() {
            let Some(stats) = self.values.get(&value) else {
                continue;
            };
            if stats.count == count && stats.last_seen == last_seen {
                return Some((value, stats.clone()));
            }
        }
        None
    }

    fn compact_heap_if_needed(&mut self) {
        let threshold = self.capacity.saturating_mul(4).max(64);
        if self.least.len() <= threshold {
            return;
        }
        self.least = self
            .values
            .iter()
            .map(|(value, stats)| Reverse((stats.count, stats.last_seen, value.clone())))
            .collect();
    }
}

/// 从日志样本中收集字段值。低基数字段保留采样范围内的唯一值；tag 和
/// message 使用有界重频索引，避免大日志导致内存增长或候选只偏向文件开头。
#[derive(Debug)]
pub struct FieldCatalog {
    exact_values: BTreeMap<LogField, BTreeSet<String>>,
    ranked_values: BTreeMap<LogField, RankedValueIndex>,
    max_values_per_field: usize,
    seen_lines: u64,
    matcher: Mutex<Matcher>,
}

impl Clone for FieldCatalog {
    fn clone(&self) -> Self {
        Self {
            exact_values: self.exact_values.clone(),
            ranked_values: self.ranked_values.clone(),
            max_values_per_field: self.max_values_per_field,
            seen_lines: self.seen_lines,
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
            exact_values: BTreeMap::new(),
            ranked_values: BTreeMap::new(),
            max_values_per_field: max_values_per_field.max(1),
            seen_lines: 0,
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
        self.seen_lines = self.seen_lines.saturating_add(1);
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
        if matches!(field, LogField::Tag | LogField::Message) {
            self.ranked_values
                .entry(field)
                .or_insert_with(|| RankedValueIndex::new(self.max_values_per_field))
                .insert(value, self.seen_lines);
        } else {
            let values = self.exact_values.entry(field).or_default();
            if values.len() < MAX_EXACT_VALUES_PER_FIELD || values.contains(&value) {
                values.insert(value);
            }
        }
    }

    pub fn values(&self, field: LogField) -> impl Iterator<Item = &str> {
        self.exact_values
            .get(&field)
            .into_iter()
            .flat_map(|values| values.iter().map(String::as_str))
            .chain(
                self.ranked_values
                    .get(&field)
                    .into_iter()
                    .flat_map(|index| index.values.keys().map(String::as_str)),
            )
    }

    fn value_stats(&self, field: LogField, value: &str) -> ValueStats {
        self.ranked_values
            .get(&field)
            .and_then(|index| index.values.get(value))
            .cloned()
            .unwrap_or(ValueStats {
                count: 1,
                last_seen: 0,
            })
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
                    out.push((
                        match_priority(prefix, &expression),
                        ValueStats {
                            count: u64::MAX,
                            last_seen: u64::MAX,
                        },
                        Suggestion {
                            field: Some(candidate),
                            value: candidate.name().to_owned(),
                            expression: format!("{expression_prefix}{expression}"),
                            score,
                        },
                    ));
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
                out.push((
                    match_priority(prefix, value),
                    self.value_stats(candidate, value),
                    Suggestion {
                        field: Some(candidate),
                        value: value.to_owned(),
                        expression: format!(
                            "{expression_prefix}{}",
                            format_field_expression(candidate, value)
                        ),
                        score,
                    },
                ));
            }
        }
        out.sort_by(|(a_priority, a_stats, a), (b_priority, b_stats, b)| {
            a_priority
                .cmp(b_priority)
                .then_with(|| b_stats.count.cmp(&a_stats.count))
                .then_with(|| b_stats.last_seen.cmp(&a_stats.last_seen))
                .then_with(|| a.score.cmp(&b.score))
                .then_with(|| a.expression.cmp(&b.expression))
        });
        out.truncate(limit);
        out.into_iter()
            .map(|(_, _, suggestion)| suggestion)
            .collect()
    }
}

fn split_completion_fragment(input: &str) -> (&str, &str) {
    let start = completion_fragment_start(input);
    let fragment = &input[start..];
    let whitespace = fragment.len() - fragment.trim_start().len();
    (&input[..start + whitespace], &fragment[whitespace..])
}

/// Find the last symbolic boolean operator that is part of query syntax, not
/// text inside a quoted value or regex. The query lexer uses the same quote,
/// slash-regex, escape, and doubled-operator rules.
fn completion_fragment_start(input: &str) -> usize {
    #[derive(Clone, Copy)]
    enum Literal {
        Quote(char),
        Regex,
    }

    let mut literal = None;
    let mut escaped = false;
    let mut start = 0;
    let mut chars = input.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        match literal {
            Some(Literal::Quote(quote)) => {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == quote {
                    literal = None;
                }
            }
            Some(Literal::Regex) => {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '/' {
                    literal = None;
                }
            }
            None => match ch {
                '\'' | '"' => literal = Some(Literal::Quote(ch)),
                '/' => literal = Some(Literal::Regex),
                '&' | '|' => {
                    let mut end = index + ch.len_utf8();
                    if chars.peek().is_some_and(|(_, next)| *next == ch) {
                        let (next_index, next) = chars.next().expect("peeked operator");
                        end = next_index + next.len_utf8();
                    }
                    start = end;
                }
                _ => {}
            },
        }
    }
    start
}

fn match_priority(query: &str, candidate: &str) -> u8 {
    if query.is_empty() {
        return 0;
    }
    let query = query.to_lowercase();
    let candidate = candidate.to_lowercase();
    if candidate.starts_with(&query) {
        0
    } else if candidate.contains(&query) {
        1
    } else {
        2
    }
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

    #[test]
    fn suggestions_preserve_whitespace_after_boolean_operator() {
        let catalog = FieldCatalog::from_sample(DATA.as_bytes(), Encoding::Utf8);
        let out = catalog.suggest("Magic &  tag:ae", 10);
        assert!(out
            .iter()
            .any(|suggestion| suggestion.expression == "Magic &  tag:AeAlgo"));
    }

    #[test]
    fn boolean_characters_inside_literals_do_not_split_completion() {
        assert_eq!(
            split_completion_fragment(r#"tag:"Audio & Video" | msg:par"#),
            (r#"tag:"Audio & Video" | "#, "msg:par")
        );
        assert_eq!(
            split_completion_fragment(r#"msg:/start|stop/ & tag:ae"#),
            (r#"msg:/start|stop/ & "#, "tag:ae")
        );
        assert_eq!(
            split_completion_fragment(r#"msg:/start\|stop/"#),
            ("", r#"msg:/start\|stop/"#)
        );
    }

    #[test]
    fn bounded_ranked_index_keeps_later_frequent_values() {
        let mut index = RankedValueIndex::new(2);
        index.insert("early-a".into(), 1);
        index.insert("early-b".into(), 2);
        for seen_at in 3..10 {
            index.insert("late-hot".into(), seen_at);
        }
        assert!(index.values.contains_key("late-hot"));
        assert!(index.values["late-hot"].count > 1);
        assert_eq!(index.values.len(), 2);
    }

    #[test]
    fn ranked_suggestions_prefer_frequency_then_recency() {
        let mut catalog = FieldCatalog::new(4);
        for value in ["Alpha", "Alpine", "Alpha", "Alpine", "Alpha"] {
            catalog.seen_lines += 1;
            catalog.insert(LogField::Tag, value.into());
        }
        let out = catalog.suggest("tag:Al", 4);
        assert_eq!(out[0].value, "Alpha");
        assert_eq!(out[1].value, "Alpine");
    }

    #[test]
    fn exact_fields_are_not_limited_by_ranked_value_capacity() {
        let mut catalog = FieldCatalog::new(1);
        for value in ["1", "2", "3"] {
            catalog.insert(LogField::Pid, value.into());
        }
        assert_eq!(catalog.values(LogField::Pid).count(), 3);
    }
}
