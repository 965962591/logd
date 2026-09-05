//! 多关键字匹配引擎。
//!
//! 现实中的 `.tat` 配置（见 `tat/ae_log.tat`）几乎全是 `regex="n"` 的字面量，
//! 所以主力是 Aho–Corasick：**一次遍历同时命中 20+ 关键字**，~1–2 GB/s/核。
//! 正则走 `regex::bytes::RegexSet` 判命中，取 span 时才逐条跑——那只发生在
//! 渲染的 ~60 行上，成本无所谓。
//!
//! 两个入口的分工很关键：
//! - [`MatcherSet::is_visible`] 给全量筛选扫描用，只回答 bool，尽量早退
//! - [`MatcherSet::analyze`] 给渲染用，同时给出行级样式和字段级 span

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use anyhow::{Context, Result};
use regex::bytes::{Regex, RegexBuilder, RegexSet, RegexSetBuilder};

use crate::source::Encoding;

/// 单条过滤器命中后怎么上色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HighlightMode {
    /// 整行上色。TAT.NET 的原生语义，也是缺省。
    #[default]
    Line,
    /// 只给命中的那几个字符上色。
    Field,
}

impl HighlightMode {
    pub fn as_str(self) -> &'static str {
        match self {
            HighlightMode::Line => "line",
            HighlightMode::Field => "field",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "field" => HighlightMode::Field,
            _ => HighlightMode::Line,
        }
    }
}

/// Which imported log files a configured filter applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FilterScope {
    /// Preserve the historical behavior of filters loaded from `.tat` files.
    #[default]
    AllFiles,
    /// Apply the filter only to the currently active log tab.
    CurrentFile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterSpec {
    pub enabled: bool,
    /// `.tat` 的 `excluding="y"`：命中就把整行藏掉。
    pub excluding: bool,
    pub description: String,
    pub text: String,
    pub regex: bool,
    pub case_sensitive: bool,
    /// `.tat` 的 `type`，目前只见过 `matches_text`。原样保留以便回写。
    pub kind: String,
    /// 0xRRGGBB
    pub fore: Option<u32>,
    pub back: Option<u32>,
    // 以下是 logd 扩展，写进 .tat 时用 `logd_` 前缀的普通属性
    pub bold: bool,
    pub italic: bool,
    pub mode: HighlightMode,
    /// Whether this filter is shared by all imported files or only the active file.
    pub scope: FilterScope,
    /// 读到的、我们不认识的属性。回写时原样吐回去，保证 .tat 往返不丢信息。
    pub extra: Vec<(String, String)>,
}

impl Default for FilterSpec {
    fn default() -> Self {
        Self {
            enabled: true,
            excluding: false,
            description: String::new(),
            text: String::new(),
            regex: false,
            case_sensitive: false,
            kind: "matches_text".to_string(),
            fore: None,
            back: None,
            bold: false,
            italic: false,
            mode: HighlightMode::default(),
            scope: FilterScope::default(),
            extra: Vec::new(),
        }
    }
}

impl FilterSpec {
    /// 参与匹配的条件：启用 + 关键字非空。
    pub fn is_active(&self) -> bool {
        self.enabled && !self.text.is_empty()
    }
}

/// 一段命中区间，`filter` 是它在 `MatcherSet::filters` 里的下标（下标即优先级）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub filter: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Verdict {
    pub visible: bool,
    /// 命中的行模式过滤器里下标最小（优先级最高）的那个。
    pub line_filter: Option<usize>,
}

pub struct MatcherSet {
    filters: Vec<FilterSpec>,
    ac_cs: Option<AhoCorasick>,
    ac_cs_ids: Vec<usize>,
    ac_ci: Option<AhoCorasick>,
    ac_ci_ids: Vec<usize>,
    re_set: Option<RegexSet>,
    re_ids: Vec<usize>,
    /// 只在渲染取 span 时用。
    re_each: Vec<(usize, Regex)>,
    has_include: bool,
    has_exclude: bool,
}

/// 把关键字转成目标文件编码下的字节串。
pub fn encode_pattern(text: &str, enc: Encoding) -> Vec<u8> {
    enc.codec().encode(text).0.into_owned()
}

impl MatcherSet {
    /// 一条过滤器都没有：全部行可见，无高亮。
    pub fn empty() -> Self {
        Self {
            filters: Vec::new(),
            ac_cs: None,
            ac_cs_ids: Vec::new(),
            ac_ci: None,
            ac_ci_ids: Vec::new(),
            re_set: None,
            re_ids: Vec::new(),
            re_each: Vec::new(),
            has_include: false,
            has_exclude: false,
        }
    }

    pub fn new(filters: Vec<FilterSpec>, enc: Encoding) -> Result<Self> {
        let (mut lit_cs, mut ac_cs_ids) = (Vec::new(), Vec::new());
        let (mut lit_ci, mut ac_ci_ids) = (Vec::new(), Vec::new());
        let (mut res, mut re_ids) = (Vec::new(), Vec::new());
        let mut re_each = Vec::new();
        let (mut has_include, mut has_exclude) = (false, false);

        for (i, f) in filters.iter().enumerate() {
            if !f.is_active() {
                continue;
            }
            if f.excluding {
                has_exclude = true;
            } else {
                has_include = true;
            }

            if f.regex {
                // 大小写用内联 flag 控制，这样一个 RegexSet 就能混装两种。
                let pat = if f.case_sensitive {
                    f.text.clone()
                } else {
                    format!("(?i){}", f.text)
                };
                let one = RegexBuilder::new(&pat)
                    .build()
                    .with_context(|| format!("正则编译失败：{}", f.text))?;
                re_each.push((i, one));
                res.push(pat);
                re_ids.push(i);
            } else {
                let bytes = encode_pattern(&f.text, enc);
                if f.case_sensitive {
                    lit_cs.push(bytes);
                    ac_cs_ids.push(i);
                } else {
                    lit_ci.push(bytes);
                    ac_ci_ids.push(i);
                }
            }
        }

        // MatchKind::Standard 是 find_overlapping_iter 的前提。
        // 不能用 leftmost 语义：模式 "abc" 和 "bcd" 在 "abcd" 里只会命中前者，
        // 后者对应的过滤器就被漏判了，可见性会算错。
        let build_ac = |pats: Vec<Vec<u8>>, ci: bool| -> Result<Option<AhoCorasick>> {
            if pats.is_empty() {
                return Ok(None);
            }
            let ac = AhoCorasickBuilder::new()
                .match_kind(MatchKind::Standard)
                .ascii_case_insensitive(ci)
                .build(&pats)
                .context("构建 Aho-Corasick 自动机失败")?;
            Ok(Some(ac))
        };

        let re_set = if res.is_empty() {
            None
        } else {
            Some(
                RegexSetBuilder::new(&res)
                    .build()
                    .context("构建 RegexSet 失败")?,
            )
        };

        Ok(Self {
            ac_cs: build_ac(lit_cs, false)?,
            ac_cs_ids,
            ac_ci: build_ac(lit_ci, true)?,
            ac_ci_ids,
            re_set,
            re_ids,
            re_each,
            has_include,
            has_exclude,
            filters,
        })
    }

    pub fn filters(&self) -> &[FilterSpec] {
        &self.filters
    }

    /// 一条过滤器都没启用时，全部行都可见，且没有任何高亮。
    pub fn is_noop(&self) -> bool {
        !self.has_include && !self.has_exclude
    }

    pub fn has_include(&self) -> bool {
        self.has_include
    }

    /// 筛选扫描的热路径。语义对齐 TAT.NET：
    /// `(无 include || 命中任一 include) && 命中 0 个 exclude`
    pub fn is_visible(&self, line: &[u8]) -> bool {
        if self.is_noop() {
            return true;
        }
        let mut hit_include = false;

        for (ac, ids) in [
            (&self.ac_cs, &self.ac_cs_ids),
            (&self.ac_ci, &self.ac_ci_ids),
        ] {
            let Some(ac) = ac else { continue };
            for m in ac.find_overlapping_iter(line) {
                let f = ids[m.pattern().as_usize()];
                if self.filters[f].excluding {
                    return false;
                }
                hit_include = true;
                // 没有 exclude 过滤器时，命中一个 include 就能定论。
                if !self.has_exclude {
                    return true;
                }
            }
        }

        if let Some(set) = &self.re_set {
            for pid in set.matches(line).iter() {
                let f = self.re_ids[pid];
                if self.filters[f].excluding {
                    return false;
                }
                hit_include = true;
                if !self.has_exclude {
                    return true;
                }
            }
        }

        !self.has_include || hit_include
    }

    /// Record every active filter that matches this line and return the
    /// configured-filter visibility verdict. A filter is recorded at most
    /// once even if its pattern occurs multiple times on the same line.
    pub fn matching_filters(&self, line: &[u8], hits: &mut Vec<bool>) -> bool {
        hits.clear();
        hits.resize(self.filters.len(), false);
        if self.is_noop() {
            return true;
        }

        for (ac, ids) in [
            (&self.ac_cs, &self.ac_cs_ids),
            (&self.ac_ci, &self.ac_ci_ids),
        ] {
            let Some(ac) = ac else { continue };
            for matched in ac.find_overlapping_iter(line) {
                hits[ids[matched.pattern().as_usize()]] = true;
            }
        }

        if let Some(set) = &self.re_set {
            for pattern in set.matches(line).iter() {
                hits[self.re_ids[pattern]] = true;
            }
        }

        let mut hit_include = false;
        for (index, hit) in hits.iter().copied().enumerate() {
            if !hit {
                continue;
            }
            if self.filters[index].excluding {
                return false;
            }
            hit_include = true;
        }
        !self.has_include || hit_include
    }

    /// 渲染路径。同时给出可见性、行级样式、以及字段模式的命中区间。
    ///
    /// `spans` 只装**字段模式**过滤器的命中；行模式不需要逐字区间。
    /// 返回前已经用 [`flatten_spans`] 拍平成互不重叠的有序区间。
    pub fn analyze(&self, line: &[u8], spans: &mut Vec<Span>) -> Verdict {
        spans.clear();
        if self.is_noop() {
            return Verdict {
                visible: true,
                line_filter: None,
            };
        }

        let mut hit_include = false;
        let mut excluded = false;
        let mut line_filter: Option<usize> = None;

        let mut note = |f: usize, start: usize, end: usize, spans: &mut Vec<Span>| {
            let spec = &self.filters[f];
            if spec.excluding {
                excluded = true;
                return;
            }
            hit_include = true;
            match spec.mode {
                HighlightMode::Line => {
                    // 下标越小优先级越高
                    line_filter = Some(line_filter.map_or(f, |cur| cur.min(f)));
                }
                HighlightMode::Field => spans.push(Span { filter: f, start, end }),
            }
        };

        for (ac, ids) in [
            (&self.ac_cs, &self.ac_cs_ids),
            (&self.ac_ci, &self.ac_ci_ids),
        ] {
            let Some(ac) = ac else { continue };
            for m in ac.find_overlapping_iter(line) {
                note(ids[m.pattern().as_usize()], m.start(), m.end(), spans);
            }
        }

        if let Some(set) = &self.re_set {
            let hits = set.matches(line);
            if hits.matched_any() {
                // re_each、re_ids、RegexSet 的模式三者同序，下标可以直接对上。
                for (k, (f, re)) in self.re_each.iter().enumerate() {
                    if !hits.matched(k) {
                        continue;
                    }
                    for m in re.find_iter(line) {
                        note(*f, m.start(), m.end(), spans);
                    }
                }
            }
        }

        if excluded {
            spans.clear();
            return Verdict {
                visible: false,
                line_filter: None,
            };
        }

        let visible = !self.has_include || hit_include;
        if !visible {
            spans.clear();
        } else {
            flatten_spans(spans);
        }
        Verdict {
            visible,
            line_filter,
        }
    }
}

/// 把可能重叠的命中区间拍平成互不重叠、按 `start` 升序的序列。
///
/// 冲突规则：先开始的赢；同起点则过滤器下标小的（列表里靠前的）赢。
/// 被压住的区间会被裁掉左半段，整段被盖住就丢弃。
pub fn flatten_spans(spans: &mut Vec<Span>) {
    if spans.len() < 2 {
        return;
    }
    // 同起点时下标小的优先；同起点同下标时长的优先。
    spans.sort_unstable_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then(a.filter.cmp(&b.filter))
            .then(b.end.cmp(&a.end))
    });

    let mut cursor = 0usize;
    let mut w = 0usize;
    for r in 0..spans.len() {
        let mut s = spans[r];
        if s.end <= cursor {
            continue;
        }
        s.start = s.start.max(cursor);
        cursor = s.end;
        spans[w] = s;
        w += 1;
    }
    spans.truncate(w);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(text: &str) -> FilterSpec {
        FilterSpec {
            text: text.to_string(),
            ..Default::default()
        }
    }

    fn set(filters: Vec<FilterSpec>) -> MatcherSet {
        MatcherSet::new(filters, Encoding::Utf8).unwrap()
    }

    #[test]
    fn no_filters_shows_everything() {
        let m = set(vec![]);
        assert!(m.is_noop());
        assert!(m.is_visible(b"anything"));
    }

    #[test]
    fn disabled_filter_is_ignored() {
        let m = set(vec![FilterSpec {
            enabled: false,
            ..lit("Magic:")
        }]);
        assert!(m.is_noop());
        assert!(m.is_visible(b"no magic here"));
    }

    #[test]
    fn include_only() {
        let m = set(vec![lit("Magic:")]);
        assert!(m.is_visible(b"[AE] Magic: 42"));
        assert!(!m.is_visible(b"[AE] nothing"));
    }

    #[test]
    fn case_insensitive_by_default() {
        let m = set(vec![lit("magic:")]);
        assert!(m.is_visible(b"MAGIC: 1"));

        let m = set(vec![FilterSpec {
            case_sensitive: true,
            ..lit("magic:")
        }]);
        assert!(!m.is_visible(b"MAGIC: 1"));
    }

    #[test]
    fn exclude_wins_over_include() {
        let m = set(vec![
            lit("ae"),
            FilterSpec {
                excluding: true,
                ..lit("noise")
            },
        ]);
        assert!(m.is_visible(b"ae flow"));
        assert!(!m.is_visible(b"ae flow with noise"));
    }

    #[test]
    fn exclude_only_hides_matching_and_keeps_rest() {
        let m = set(vec![FilterSpec {
            excluding: true,
            ..lit("noise")
        }]);
        assert!(m.is_visible(b"clean line"));
        assert!(!m.is_visible(b"has noise"));
    }

    #[test]
    fn matching_filters_reports_each_filter_once_per_line() {
        let m = set(vec![
            lit("alpha"),
            lit("beta"),
            FilterSpec {
                excluding: true,
                ..lit("noise")
            },
        ]);
        let mut hits = Vec::new();

        assert!(m.matching_filters(b"alpha alpha beta", &mut hits));
        assert_eq!(hits, vec![true, true, false]);
        assert!(!m.matching_filters(b"alpha noise", &mut hits));
        assert_eq!(hits, vec![true, false, true]);
    }

    /// 回归：非重叠的 leftmost 语义会让 "bcd" 被 "abc" 吃掉。
    #[test]
    fn overlapping_patterns_all_reported() {
        let m = set(vec![
            lit("abc"),
            FilterSpec {
                excluding: true,
                ..lit("bcd")
            },
        ]);
        assert!(!m.is_visible(b"xxabcdxx"), "重叠的 exclude 模式被漏掉了");
    }

    #[test]
    fn regex_filter() {
        let m = set(vec![FilterSpec {
            regex: true,
            ..lit(r"Magic:\s*\d+")
        }]);
        assert!(m.is_visible(b"Magic:  1234"));
        assert!(!m.is_visible(b"Magic: abc"));
    }

    #[test]
    fn analyze_field_mode_yields_spans() {
        let m = set(vec![FilterSpec {
            mode: HighlightMode::Field,
            ..lit("Magic:")
        }]);
        let mut spans = Vec::new();
        let v = m.analyze(b"[AE] Magic: 42", &mut spans);
        assert!(v.visible);
        assert_eq!(v.line_filter, None);
        assert_eq!(spans, vec![Span { filter: 0, start: 5, end: 11 }]);
    }

    #[test]
    fn analyze_line_mode_yields_no_spans() {
        let m = set(vec![lit("Magic:")]);
        let mut spans = Vec::new();
        let v = m.analyze(b"[AE] Magic: 42", &mut spans);
        assert!(v.visible);
        assert_eq!(v.line_filter, Some(0));
        assert!(spans.is_empty());
    }

    #[test]
    fn line_mode_priority_is_list_order() {
        let m = set(vec![lit("beta"), lit("alpha")]);
        let mut spans = Vec::new();
        let v = m.analyze(b"alpha beta", &mut spans);
        assert_eq!(v.line_filter, Some(0), "靠前的过滤器优先");
    }

    #[test]
    fn flatten_resolves_overlap_by_priority() {
        let mut s = vec![
            Span { filter: 1, start: 2, end: 8 },
            Span { filter: 0, start: 0, end: 5 },
        ];
        flatten_spans(&mut s);
        assert_eq!(
            s,
            vec![
                Span { filter: 0, start: 0, end: 5 },
                Span { filter: 1, start: 5, end: 8 },
            ]
        );
    }

    #[test]
    fn flatten_drops_fully_covered() {
        let mut s = vec![
            Span { filter: 0, start: 0, end: 10 },
            Span { filter: 1, start: 3, end: 6 },
        ];
        flatten_spans(&mut s);
        assert_eq!(s, vec![Span { filter: 0, start: 0, end: 10 }]);
    }

    #[test]
    fn gb18030_pattern_matches_gb18030_bytes() {
        let (hay, _, _) = encoding_rs::GB18030.encode("曝光表 AEtable");
        let m = MatcherSet::new(vec![lit("曝光表")], Encoding::Gb18030).unwrap();
        assert!(m.is_visible(&hay));

        // 用 UTF-8 编的关键字去打 GB18030 的干草堆，必然不中
        let m = MatcherSet::new(vec![lit("曝光表")], Encoding::Utf8).unwrap();
        assert!(!m.is_visible(&hay));
    }

    #[test]
    fn big5_pattern_matches_big5_bytes() {
        let (hay, _, had_errors) = encoding_rs::BIG5.encode("繁體日誌 AEtable");
        assert!(!had_errors);

        let m = MatcherSet::new(vec![lit("繁體日誌")], Encoding::Big5).unwrap();
        assert!(m.is_visible(&hay));
    }
}
