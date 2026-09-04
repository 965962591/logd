//! Android logcat 行解析。
//!
//! 字段级筛选（`level>=W`、`tag==AeAlgo`、时间区间）全建在这上面，所以它跑在最热的
//! 路径上——50GB 全量筛选时每行都可能过一遍。因此：
//!
//! - **零拷贝**：`tag` / `message` 只返回字节区间，不分配
//! - **早退**：前两个字节不像日期就立刻返回 `None`，绝大多数非日志行一次比较就滚蛋
//! - **只在需要时调用**：查询里没有字段项时 [`crate::query`] 根本不会调它
//!
//! 支持三种常见形态：
//!
//! ```text
//! 01-02 03:04:05.678  1234  5678 D AeAlgo  : msg     threadtime
//! 2024-01-02 03:04:05.678  1234  5678 D Tag : msg    threadtime + 年份
//! 01-02 03:04:05.678 D/AeAlgo  ( 1234): msg          time（旧格式）
//! ```

use std::ops::Range;

/// logcat 优先级。序关系就是 `level>=W` 这类比较的依据。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Level {
    Verbose = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
    Fatal = 5,
    Silent = 6,
}

impl Level {
    pub fn from_byte(b: u8) -> Option<Level> {
        Some(match b {
            b'V' | b'v' => Level::Verbose,
            b'D' | b'd' => Level::Debug,
            b'I' | b'i' => Level::Info,
            b'W' | b'w' => Level::Warn,
            b'E' | b'e' => Level::Error,
            b'F' | b'f' => Level::Fatal,
            b'S' | b's' => Level::Silent,
            _ => return None,
        })
    }

    /// 认全称也认单字母：`warn` / `W` / `w` 都行。
    pub fn parse(s: &str) -> Option<Level> {
        let t = s.trim();
        if t.len() == 1 {
            return Level::from_byte(t.as_bytes()[0]);
        }
        Some(match t.to_ascii_lowercase().as_str() {
            "verbose" | "trace" => Level::Verbose,
            "debug" => Level::Debug,
            "info" => Level::Info,
            "warn" | "warning" => Level::Warn,
            "error" | "err" => Level::Error,
            "fatal" | "assert" => Level::Fatal,
            "silent" => Level::Silent,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Level::Verbose => "V",
            Level::Debug => "D",
            Level::Info => "I",
            Level::Warn => "W",
            Level::Error => "E",
            Level::Fatal => "F",
            Level::Silent => "S",
        }
    }
}

/// 归一化时间戳：自 Unix 纪元的毫秒数。
///
/// threadtime 不带年份，这时按 1970 年折算——**同一份日志内单调可比**，
/// 这正是时间区间筛选需要的。绝对日期不准，也跨不了年（已知局限，见 doc/PLAN.md）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default, Hash)]
pub struct Ts(pub i64);

impl Ts {
    pub const MIN: Ts = Ts(i64::MIN);
    pub const MAX: Ts = Ts(i64::MAX);

    fn from_parts(year: i32, mon: u32, day: u32, h: u32, m: u32, s: u32, ms: u32) -> Ts {
        let days = days_from_civil(year, mon, day);
        Ts(days * 86_400_000 + (h as i64 * 3600 + m as i64 * 60 + s as i64) * 1000 + ms as i64)
    }
}

/// Howard Hinnant 的 days_from_civil。比调 chrono 省一个依赖，也更快。
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 一行 logcat 拆出来的字段。`tag` / `message` 是相对**行首**的字节区间。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct LogLine {
    pub ts: Option<Ts>,
    pub pid: Option<u32>,
    pub tid: Option<u32>,
    pub level: Option<Level>,
    pub tag: Range<usize>,
    pub message: Range<usize>,
}

impl LogLine {
    /// 整行都当 message，没有任何字段。解析不出结构时的兜底。
    pub fn plain(len: usize) -> Self {
        Self {
            message: 0..len,
            ..Default::default()
        }
    }

    pub fn tag_bytes<'a>(&self, line: &'a [u8]) -> &'a [u8] {
        line.get(self.tag.clone()).unwrap_or(&[])
    }

    pub fn message_bytes<'a>(&self, line: &'a [u8]) -> &'a [u8] {
        line.get(self.message.clone()).unwrap_or(&[])
    }
}

/// 解析一行。不像 logcat 就返回 `None`，调用方自行决定是否退化成 [`LogLine::plain`]。
pub fn parse(line: &[u8]) -> Option<LogLine> {
    // 最短的合法行：`01-02 03:04:05.678 D/T(1):x`，30 字节都不到就别试了
    if line.len() < 24 {
        return None;
    }
    // 早退：threadtime 的第 3 个字节是 '-'（无年份）或第 5 个是 '-'（有年份）。
    // 非日志行绝大多数在这里就被踢掉，一次比较的代价。
    let (ts, mut p) = parse_timestamp(line)?;

    skip_spaces(line, &mut p);

    // ---- 分支 1：threadtime —— pid tid LEVEL tag: msg ----
    if line.get(p).is_some_and(u8::is_ascii_digit) {
        let pid = parse_u32(line, &mut p)?;
        skip_spaces(line, &mut p);
        let tid = parse_u32(line, &mut p)?;
        skip_spaces(line, &mut p);
        let level = Level::from_byte(*line.get(p)?)?;
        p += 1;
        // level 后面必须是空白，否则那只是个恰好是 V/D/I 的普通词
        if !line.get(p).is_some_and(|b| *b == b' ') {
            return None;
        }
        skip_spaces(line, &mut p);

        let tag_start = p;
        let colon = find_tag_colon(line, p)?;
        // tag 右侧常被空格补齐到固定宽度，去掉
        let tag_end = trim_end(line, tag_start, colon);
        let msg = skip_one_space(line, colon + 1);

        return Some(LogLine {
            ts: Some(ts),
            pid: Some(pid),
            tid: Some(tid),
            level: Some(level),
            tag: tag_start..tag_end,
            message: msg..line.len(),
        });
    }

    // ---- 分支 2：time 旧格式 —— `D/Tag  ( 1234): msg` ----
    let level = Level::from_byte(*line.get(p)?)?;
    p += 1;
    if *line.get(p)? != b'/' {
        return None;
    }
    p += 1;
    let tag_start = p;
    let paren = memchr::memchr(b'(', line.get(p..)?)? + p;
    let tag_end = trim_end(line, tag_start, paren);
    p = paren + 1;
    skip_spaces(line, &mut p);
    let pid = parse_u32(line, &mut p)?;
    skip_spaces(line, &mut p);
    if *line.get(p)? != b')' {
        return None;
    }
    p += 1;
    if *line.get(p)? != b':' {
        return None;
    }
    let msg = skip_one_space(line, p + 1);

    Some(LogLine {
        ts: Some(ts),
        pid: Some(pid),
        tid: None,
        level: Some(level),
        tag: tag_start..tag_end,
        message: msg..line.len(),
    })
}

/// 解析行首时间戳，返回 `(时间戳, 消费到的下标)`。
fn parse_timestamp(line: &[u8]) -> Option<(Ts, usize)> {
    let mut p = 0usize;
    // 可选的 4 位年份
    let year = if line.len() > 4 && line[4] == b'-' && line[..4].iter().all(u8::is_ascii_digit) {
        let y = two(line, 0)? as i32 * 100 + two(line, 2)? as i32;
        p = 5;
        y
    } else if line.len() > 2 && line[2] == b'-' {
        // threadtime 不带年。用 1970 只为让同一份日志内可比。
        1970
    } else {
        return None;
    };

    let mon = two(line, p)?;
    if *line.get(p + 2)? != b'-' {
        return None;
    }
    let day = two(line, p + 3)?;
    if *line.get(p + 5)? != b' ' {
        return None;
    }
    p += 6;

    let h = two(line, p)?;
    if *line.get(p + 2)? != b':' {
        return None;
    }
    let m = two(line, p + 3)?;
    if *line.get(p + 5)? != b':' {
        return None;
    }
    let s = two(line, p + 6)?;
    p += 8;

    // 小数秒可有可无，位数不定（Android 13+ 会打 6 位）
    let mut ms = 0u32;
    if line.get(p) == Some(&b'.') {
        p += 1;
        let start = p;
        let mut digits = 0u32;
        while line.get(p).is_some_and(u8::is_ascii_digit) {
            if digits < 3 {
                ms = ms * 10 + (line[p] - b'0') as u32;
            }
            digits += 1;
            p += 1;
        }
        if p == start {
            return None;
        }
        // 位数不足 3 位要补齐：`.5` 是 500ms 不是 5ms
        for _ in digits..3 {
            ms *= 10;
        }
    }

    // 日期字段本身要合法，否则 `12-34 56:78:90` 这种会被误认
    if !(1..=12).contains(&mon) || !(1..=31).contains(&day) || h > 23 || m > 59 || s > 60 {
        return None;
    }
    Some((Ts::from_parts(year, mon, day, h, m, s, ms), p))
}

#[inline]
fn two(line: &[u8], at: usize) -> Option<u32> {
    let a = *line.get(at)?;
    let b = *line.get(at + 1)?;
    if !a.is_ascii_digit() || !b.is_ascii_digit() {
        return None;
    }
    Some(((a - b'0') * 10 + (b - b'0')) as u32)
}

#[inline]
fn skip_spaces(line: &[u8], p: &mut usize) {
    while line.get(*p) == Some(&b' ') {
        *p += 1;
    }
}

#[inline]
fn skip_one_space(line: &[u8], p: usize) -> usize {
    if line.get(p) == Some(&b' ') {
        p + 1
    } else {
        p
    }
}

#[inline]
fn parse_u32(line: &[u8], p: &mut usize) -> Option<u32> {
    let start = *p;
    let mut v: u32 = 0;
    while let Some(&b) = line.get(*p) {
        if !b.is_ascii_digit() {
            break;
        }
        v = v.wrapping_mul(10).wrapping_add((b - b'0') as u32);
        *p += 1;
    }
    (*p > start).then_some(v)
}

/// 找 tag 和 message 之间的分隔冒号。
///
/// tag 里可能带冒号（少见但有），所以认「冒号 + 空格」或「冒号 + 行尾」；
/// 都找不到时退回第一个冒号。
fn find_tag_colon(line: &[u8], from: usize) -> Option<usize> {
    let rest = line.get(from..)?;
    let mut first = None;
    for i in memchr::memchr_iter(b':', rest) {
        let at = from + i;
        if first.is_none() {
            first = Some(at);
        }
        if line.get(at + 1).is_none_or(|b| *b == b' ') {
            return Some(at);
        }
    }
    first
}

#[inline]
fn trim_end(line: &[u8], start: usize, mut end: usize) -> usize {
    while end > start && line[end - 1] == b' ' {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Option<LogLine> {
        parse(s.as_bytes())
    }

    #[test]
    fn threadtime_basic() {
        let s = "01-02 03:04:05.678  1234  5678 D AeAlgo  : [updateAEInfo2ISP] gain=1024";
        let l = p(s).expect("应该能解析");
        assert_eq!(l.pid, Some(1234));
        assert_eq!(l.tid, Some(5678));
        assert_eq!(l.level, Some(Level::Debug));
        assert_eq!(l.tag_bytes(s.as_bytes()), b"AeAlgo", "tag 右侧补齐的空格要去掉");
        assert_eq!(
            l.message_bytes(s.as_bytes()),
            b"[updateAEInfo2ISP] gain=1024"
        );
    }

    #[test]
    fn threadtime_with_year() {
        let s = "2024-01-02 03:04:05.678  1234  5678 W Hal3Av3 : parseMeta";
        let l = p(s).unwrap();
        assert_eq!(l.level, Some(Level::Warn));
        assert_eq!(l.tag_bytes(s.as_bytes()), b"Hal3Av3");
        assert_eq!(l.message_bytes(s.as_bytes()), b"parseMeta");
        // 2024-01-02 的 Unix 天数
        assert_eq!(l.ts.unwrap().0 / 86_400_000, days_from_civil(2024, 1, 2));
    }

    #[test]
    fn time_legacy_format() {
        let s = "01-02 03:04:05.678 D/AeAlgo  ( 1234): doCapAE";
        let l = p(s).unwrap();
        assert_eq!(l.pid, Some(1234));
        assert_eq!(l.tid, None);
        assert_eq!(l.level, Some(Level::Debug));
        assert_eq!(l.tag_bytes(s.as_bytes()), b"AeAlgo");
        assert_eq!(l.message_bytes(s.as_bytes()), b"doCapAE");
    }

    #[test]
    fn six_digit_fraction() {
        let a = p("01-02 03:04:05.678000  1 2 I T: x").unwrap();
        let b = p("01-02 03:04:05.678  1 2 I T: x").unwrap();
        assert_eq!(a.ts, b.ts, "多余的小数位应被忽略而不是算进毫秒");
    }

    #[test]
    fn short_fraction_is_padded() {
        let a = p("01-02 03:04:05.5  1 2 I T: x").unwrap();
        let b = p("01-02 03:04:05.500  1 2 I T: x").unwrap();
        assert_eq!(a.ts, b.ts, ".5 是 500ms 不是 5ms");
    }

    #[test]
    fn no_fraction() {
        assert!(p("01-02 03:04:05  1 2 I T: x").is_some());
    }

    #[test]
    fn timestamps_are_ordered() {
        let a = p("01-02 03:04:05.100  1 2 I T: x").unwrap().ts.unwrap();
        let b = p("01-02 03:04:05.200  1 2 I T: x").unwrap().ts.unwrap();
        let c = p("01-02 03:05:00.000  1 2 I T: x").unwrap().ts.unwrap();
        let d = p("01-03 00:00:00.000  1 2 I T: x").unwrap().ts.unwrap();
        assert!(a < b && b < c && c < d);
    }

    #[test]
    fn tag_containing_colon() {
        let s = "01-02 03:04:05.678  1 2 I AeAlgo:sub: payload here";
        let l = p(s).unwrap();
        assert_eq!(l.tag_bytes(s.as_bytes()), b"AeAlgo:sub");
        assert_eq!(l.message_bytes(s.as_bytes()), b"payload here");
    }

    #[test]
    fn empty_message() {
        let s = "01-02 03:04:05.678  1 2 I Tag:";
        let l = p(s).unwrap();
        assert_eq!(l.tag_bytes(s.as_bytes()), b"Tag");
        assert_eq!(l.message_bytes(s.as_bytes()), b"");
    }

    #[test]
    fn rejects_non_log_lines() {
        assert!(p("").is_none());
        assert!(p("just some plain text without any structure").is_none());
        assert!(p("[  123.456789] usb 1-1: new high-speed device").is_none());
        assert!(p("*** AE_DEBUG: frame=42 exp=33000").is_none());
    }

    /// 日期字段必须校验，否则随便一串数字都会被当成日志行。
    #[test]
    fn rejects_out_of_range_datetime() {
        assert!(p("13-02 03:04:05.678  1 2 I T: x").is_none(), "13 月");
        assert!(p("01-32 03:04:05.678  1 2 I T: x").is_none(), "32 日");
        assert!(p("01-02 24:04:05.678  1 2 I T: x").is_none(), "24 时");
        assert!(p("01-02 03:60:05.678  1 2 I T: x").is_none(), "60 分");
    }

    /// level 后面不跟空格就说明那只是个恰好以 V/D/I 开头的词。
    #[test]
    fn rejects_bogus_level() {
        assert!(p("01-02 03:04:05.678  1 2 Dxx Tag: x").is_none());
        assert!(p("01-02 03:04:05.678  1 2 Z Tag: x").is_none(), "Z 不是合法 level");
    }

    #[test]
    fn level_ordering_and_parsing() {
        assert!(Level::Verbose < Level::Debug);
        assert!(Level::Warn < Level::Error);
        assert!(Level::Error < Level::Fatal);
        assert_eq!(Level::parse("W"), Some(Level::Warn));
        assert_eq!(Level::parse("warn"), Some(Level::Warn));
        assert_eq!(Level::parse("WARNING"), Some(Level::Warn));
        assert_eq!(Level::parse("error"), Some(Level::Error));
        assert_eq!(Level::parse("nope"), None);
    }

    #[test]
    fn days_from_civil_matches_known_values() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1970, 1, 2), 1);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        assert_eq!(days_from_civil(2024, 1, 1), 19723);
    }

    #[test]
    fn plain_fallback_covers_whole_line() {
        let l = LogLine::plain(10);
        assert_eq!(l.message, 0..10);
        assert_eq!(l.level, None);
    }

    /// 解析器绝不能 panic —— 它跑在 5 亿行上，一次越界就是整个进程没了。
    #[test]
    fn never_panics_on_truncated_input() {
        let full = "2024-01-02 03:04:05.678  1234  5678 D AeAlgo  : payload";
        for i in 0..=full.len() {
            let _ = parse(&full.as_bytes()[..i]);
        }
        for i in 0..full.len() {
            let _ = parse(&full.as_bytes()[i..]);
        }
        // 随便一些字节序列
        for b in 0u8..=255 {
            let _ = parse(&[b; 40]);
            let _ = parse(&[b]);
        }
    }
}
