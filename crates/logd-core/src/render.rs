//! 把一行原始字节变成可渲染的字符串，并把命中区间搬到解码后的坐标系。
//!
//! 三个坑，都在这里一次性处理掉：
//!
//! 1. **坐标系不同**。匹配跑在原始字节上（见 [`crate::matcher`]），但渲染的是解码后的
//!    `String`。UTF-8 下两者偏移相同，**GB18030 下完全不同**（`曝` 原始 2 字节、
//!    解码后 3 字节）。不换算的话高亮会错位。
//! 2. **char 边界**。gpui 的 `StyledText::with_highlights` 对非 char 边界的 range
//!    直接 `debug_assert!` 炸掉。替换字符 U+FFFD 也会让偏移漂移。
//! 3. **超长行**。某些平台会打印几百 KB 的 dump，整行丢给 layout 会拖垮帧率。
//!    截断后落在区间中间的 span 得跟着裁。

use crate::matcher::Span;
use crate::source::Encoding;

/// 单行渲染的字节上限。超出部分截掉并打省略号。
pub const DEFAULT_MAX_RENDER_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderLine {
    pub text: String,
    /// 是否被截断过（UI 可以据此提供「展开本行」）。
    pub truncated: bool,
}

/// 解码一行，并把 `spans` 原地改写成解码后字符串的字节偏移。
///
/// `spans` 进来时是**原始字节**偏移（相对行首），出去时是 `text` 的字节偏移，
/// 且保证落在 char 边界、按 `start` 升序、不越界。落在截断点之外的会被裁掉或丢弃。
pub fn prepare_line(
    raw: &[u8],
    enc: Encoding,
    spans: &mut Vec<Span>,
    max_bytes: usize,
) -> RenderLine {
    // 先按原始字节截断，避免解码几百 KB 只为了丢掉
    let raw_cut = floor_raw_boundary(raw, enc, max_bytes);
    let truncated = raw_cut < raw.len();
    let head = &raw[..raw_cut];

    let mut text = decode(head, enc);

    if !spans.is_empty() {
        remap_spans(head, enc, &text, spans);
    }
    if truncated {
        text.push('…');
    }

    RenderLine { text, truncated }
}

/// 只解码，不管 span。给不需要高亮的行走的快路径。
pub fn prepare_plain(raw: &[u8], enc: Encoding, max_bytes: usize) -> RenderLine {
    let raw_cut = floor_raw_boundary(raw, enc, max_bytes);
    let truncated = raw_cut < raw.len();
    let mut text = decode(&raw[..raw_cut], enc);
    if truncated {
        text.push('…');
    }
    RenderLine { text, truncated }
}

fn decode(bytes: &[u8], enc: Encoding) -> String {
    enc.codec()
        .decode_without_bom_handling(bytes)
        .0
        .into_owned()
}

/// 在原始字节里找一个 `<= max_bytes` 的安全切点，别把多字节序列劈开。
fn floor_raw_boundary(raw: &[u8], enc: Encoding, max_bytes: usize) -> usize {
    if raw.len() <= max_bytes {
        return raw.len();
    }
    match enc {
        // UTF-8 续接字节是 10xxxxxx，往回退到首字节
        Encoding::Utf8 => {
            let mut i = max_bytes;
            while i > 0 && (raw[i] & 0xC0) == 0x80 {
                i -= 1;
            }
            i
        }
        // 其他多字节编码没有 UTF-8 那样的自同步位。受支持编码的码元最多 4 字节，
        // 从截断点最多回看 3 字节，直到完整解码不以替换字符结尾。
        _ => {
            if enc.codec().is_single_byte() {
                return max_bytes;
            }
            let floor = max_bytes.saturating_sub(3);
            for cut in (floor..=max_bytes).rev() {
                let (text, had_errors) = enc.codec().decode_without_bom_handling(&raw[..cut]);
                if !had_errors || !text.ends_with('\u{FFFD}') {
                    return cut;
                }
            }
            floor
        }
    }
}

/// 把 span 从原始字节坐标搬到解码后字符串的坐标。
fn remap_spans(raw: &[u8], enc: Encoding, text: &str, spans: &mut Vec<Span>) {
    match enc {
        // 偏移相同，但非法字节被换成了 U+FFFD（1 → 3 字节），所以仍要校验
        Encoding::Utf8 if text.len() == raw.len() => {
            spans.retain_mut(|s| {
                s.start = snap(text, s.start.min(text.len()));
                s.end = snap(text, s.end.min(text.len()));
                s.end > s.start
            });
        }
        _ => {
            // 逐个换算：解码 raw[..off] 的长度就是它在 text 里的偏移。
            // 一行最多几十个 span，且只对可见的 ~60 行调用。
            spans.retain_mut(|s| {
                let a = raw_to_text_offset(raw, enc, s.start);
                let b = raw_to_text_offset(raw, enc, s.end);
                s.start = snap(text, a.min(text.len()));
                s.end = snap(text, b.min(text.len()));
                s.end > s.start
            });
        }
    }
}

fn raw_to_text_offset(raw: &[u8], enc: Encoding, raw_off: usize) -> usize {
    let off = raw_off.min(raw.len());
    decode(&raw[..off], enc).len()
}

/// 把偏移吸附到最近的、不超过它的 char 边界。
fn snap(text: &str, mut off: usize) -> usize {
    if off >= text.len() {
        return text.len();
    }
    while off > 0 && !text.is_char_boundary(off) {
        off -= 1;
    }
    off
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: usize, end: usize) -> Span {
        Span {
            filter: 0,
            start,
            end,
        }
    }

    #[test]
    fn utf8_ascii_passthrough() {
        let mut s = vec![span(5, 11)];
        let line = prepare_line(b"[AE] Magic: 42", Encoding::Utf8, &mut s, 4096);
        assert_eq!(line.text, "[AE] Magic: 42");
        assert!(!line.truncated);
        assert_eq!(&line.text[s[0].start..s[0].end], "Magic:");
    }

    /// UTF-8 下汉字前后的 span 不该漂移。
    #[test]
    fn utf8_with_chinese_keeps_offsets() {
        let raw = "前缀 Magic: 42".as_bytes();
        let start = raw.windows(6).position(|w| w == b"Magic:").unwrap();
        let mut s = vec![span(start, start + 6)];
        let line = prepare_line(raw, Encoding::Utf8, &mut s, 4096);
        assert_eq!(&line.text[s[0].start..s[0].end], "Magic:");
    }

    /// 核心回归：GB18030 的原始偏移必须换算，否则高亮错位。
    #[test]
    fn gb18030_offsets_are_remapped() {
        let (raw, _, _) = encoding_rs::GB18030.encode("曝光表 Magic: 42");
        // "曝光表" 在 GB18030 里是 6 字节，UTF-8 解码后是 9 字节
        let start = raw.windows(6).position(|w| w == b"Magic:").unwrap();
        assert_eq!(start, 7, "GB18030 下 Magic: 从第 7 字节开始");

        let mut s = vec![span(start, start + 6)];
        let line = prepare_line(&raw, Encoding::Gb18030, &mut s, 4096);
        assert_eq!(line.text, "曝光表 Magic: 42");
        assert_eq!(
            &line.text[s[0].start..s[0].end],
            "Magic:",
            "换算后应仍然框住 Magic:"
        );
        assert_eq!(s[0].start, 10, "UTF-8 下应从第 10 字节开始");
    }

    #[test]
    fn gb18030_span_covering_chinese() {
        let (raw, _, _) = encoding_rs::GB18030.encode("aa曝光表bb");
        let mut s = vec![span(2, 8)]; // GB18030 里的 "曝光表"
        let line = prepare_line(&raw, Encoding::Gb18030, &mut s, 4096);
        assert_eq!(&line.text[s[0].start..s[0].end], "曝光表");
    }

    #[test]
    fn truncates_long_line_at_char_boundary() {
        let raw = "曝".repeat(100).into_bytes();
        let line = prepare_line(raw.as_slice(), Encoding::Utf8, &mut Vec::new(), 10);
        assert!(line.truncated);
        // 10 字节切在第 4 个汉字中间 → 退到 9 字节 = 3 个汉字
        assert_eq!(line.text, "曝曝曝…");
    }

    #[test]
    fn spans_beyond_truncation_are_dropped() {
        let raw = vec![b'x'; 100];
        let mut s = vec![span(0, 5), span(50, 60)];
        let line = prepare_line(&raw, Encoding::Utf8, &mut s, 10);
        assert!(line.truncated);
        assert_eq!(s.len(), 1, "截断点之后的 span 该丢掉");
        assert_eq!(s[0], span(0, 5));
    }

    #[test]
    fn span_straddling_truncation_is_clipped() {
        let raw = vec![b'x'; 100];
        let mut s = vec![span(5, 60)];
        let line = prepare_line(&raw, Encoding::Utf8, &mut s, 10);
        assert!(line.truncated);
        assert_eq!(s[0], span(5, 10), "跨过截断点的 span 该被裁到边界");
    }

    #[test]
    fn gb18030_truncation_does_not_split_multibyte() {
        let long = "曝".repeat(50);
        let (raw, _, _) = encoding_rs::GB18030.encode(&long);
        // 每个汉字 2 字节，切在 7 应退到 6
        let line = prepare_line(&raw, Encoding::Gb18030, &mut Vec::new(), 7);
        assert!(line.truncated);
        assert_eq!(line.text, "曝曝曝…", "不该出现替换字符");
    }

    #[test]
    fn big5_truncation_does_not_split_multibyte() {
        let long = "繁".repeat(50);
        let (raw, _, had_errors) = encoding_rs::BIG5.encode(&long);
        assert!(!had_errors);

        let line = prepare_line(&raw, Encoding::Big5, &mut Vec::new(), 7);
        assert!(line.truncated);
        assert_eq!(line.text, "繁繁繁…", "不该出现替换字符");
    }

    #[test]
    fn every_supported_multibyte_encoding_truncates_cleanly() {
        let cases = [
            (Encoding::Gb18030, "😀"),
            (Encoding::Gbk, "曝"),
            (Encoding::Big5, "繁"),
            (Encoding::ShiftJis, "日"),
            (Encoding::EucJp, "日"),
            (Encoding::EucKr, "한"),
        ];
        for (encoding, ch) in cases {
            let text = ch.repeat(12);
            let (raw, _, had_errors) = encoding.codec().encode(&text);
            assert!(!had_errors, "{} cannot encode {ch}", encoding.label());
            for limit in 1..raw.len() {
                let line = prepare_plain(&raw, encoding, limit);
                assert!(
                    !line.text.contains('\u{FFFD}'),
                    "{} split a character at byte {limit}: {:?}",
                    encoding.label(),
                    line.text
                );
            }
        }
    }

    /// 非法 UTF-8 会变成 3 字节的 U+FFFD，偏移会漂，必须走换算分支。
    #[test]
    fn invalid_utf8_still_yields_char_boundaries() {
        let raw = b"a\xffb MARK";
        let start = 4;
        let mut s = vec![span(start, start + 4)];
        let line = prepare_line(raw, Encoding::Utf8, &mut s, 4096);
        assert!(line.text.is_char_boundary(s[0].start));
        assert!(line.text.is_char_boundary(s[0].end));
        assert_eq!(&line.text[s[0].start..s[0].end], "MARK");
    }

    #[test]
    fn empty_line() {
        let line = prepare_line(b"", Encoding::Utf8, &mut Vec::new(), 4096);
        assert_eq!(line.text, "");
        assert!(!line.truncated);
    }

    #[test]
    fn plain_fast_path_matches_span_path() {
        let raw = "hello 世界".as_bytes();
        let a = prepare_plain(raw, Encoding::Utf8, 4096);
        let b = prepare_line(raw, Encoding::Utf8, &mut Vec::new(), 4096);
        assert_eq!(a, b);
    }

    /// 所有产出的 span 边界都必须能安全切片，否则 gpui 会 debug_assert 炸。
    #[test]
    fn all_spans_are_sliceable() {
        let cases: Vec<(Vec<u8>, Encoding)> = vec![
            ("曝光表 Magic: 42".as_bytes().to_vec(), Encoding::Utf8),
            (
                encoding_rs::GB18030
                    .encode("曝光表 Magic: 42")
                    .0
                    .into_owned(),
                Encoding::Gb18030,
            ),
            (b"a\xff\xfeb Magic: 42".to_vec(), Encoding::Utf8),
        ];
        for (raw, enc) in cases {
            for start in 0..raw.len() {
                for end in start..=raw.len() {
                    let mut s = vec![span(start, end)];
                    let line = prepare_line(&raw, enc, &mut s, 4096);
                    for sp in &s {
                        assert!(
                            line.text.is_char_boundary(sp.start)
                                && line.text.is_char_boundary(sp.end)
                                && sp.end <= line.text.len(),
                            "{enc:?} raw[{start}..{end}] 产出了非法 span {sp:?} for {:?}",
                            line.text
                        );
                        let _ = &line.text[sp.start..sp.end]; // 真切一刀
                    }
                }
            }
        }
    }
}
