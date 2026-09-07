//! 文件接入层：内存映射 + 编码探测 + 按行解码。
//!
//! 50GB 文件全程不进堆。`memmap2` 把整个文件映射进 64 位地址空间（128TB 够用），
//! 常驻内存由 OS page cache 管，进程 RSS 不随文件大小增长。
//!
//! **匹配跑在原始字节上**，只有可见的 ~60 行才解码成 `str`。所以关键字在建
//! matcher 时就按本文件的编码转成字节串（见 [`crate::matcher::encode_pattern`]）。

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use memmap2::Mmap;

/// 支持可独立逐行解码的 ASCII 兼容编码。
///
/// 行索引靠扫 `b'\n'`，所以不能加入 UTF-16/UTF-32；ISO-2022-JP 的转义状态又可能
/// 跨行延续，也不适合当前按任意行随机读取的模型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    /// GB18030 是 GBK/GB2312 的超集，国内平台日志用这个覆盖面最广。
    Gb18030,
    Gbk,
    Big5,
    ShiftJis,
    EucJp,
    EucKr,
    Windows1250,
    Windows1251,
    Windows1252,
    Windows1253,
    Windows1254,
    Windows1255,
    Windows1256,
    Windows1257,
    Windows1258,
    Windows874,
    Iso8859_2,
    Iso8859_3,
    Iso8859_4,
    Iso8859_5,
    Iso8859_6,
    Iso8859_7,
    Iso8859_8,
    Iso8859_8I,
    Iso8859_10,
    Iso8859_13,
    Iso8859_14,
    Iso8859_15,
    Iso8859_16,
    Koi8R,
    Koi8U,
    Ibm866,
    Macintosh,
    XMacCyrillic,
    XUserDefined,
}

impl Encoding {
    pub const ALL: [Self; 36] = [
        Self::Utf8,
        Self::Gb18030,
        Self::Gbk,
        Self::Big5,
        Self::ShiftJis,
        Self::EucJp,
        Self::EucKr,
        Self::Windows1250,
        Self::Windows1251,
        Self::Windows1252,
        Self::Windows1253,
        Self::Windows1254,
        Self::Windows1255,
        Self::Windows1256,
        Self::Windows1257,
        Self::Windows1258,
        Self::Windows874,
        Self::Iso8859_2,
        Self::Iso8859_3,
        Self::Iso8859_4,
        Self::Iso8859_5,
        Self::Iso8859_6,
        Self::Iso8859_7,
        Self::Iso8859_8,
        Self::Iso8859_8I,
        Self::Iso8859_10,
        Self::Iso8859_13,
        Self::Iso8859_14,
        Self::Iso8859_15,
        Self::Iso8859_16,
        Self::Koi8R,
        Self::Koi8U,
        Self::Ibm866,
        Self::Macintosh,
        Self::XMacCyrillic,
        Self::XUserDefined,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Gb18030 => "GB18030",
            Encoding::Gbk => "GBK / GB2312",
            Encoding::Big5 => "Big5",
            Encoding::ShiftJis => "Shift_JIS",
            Encoding::EucJp => "EUC-JP",
            Encoding::EucKr => "EUC-KR",
            Encoding::Windows1250 => "Windows-1250",
            Encoding::Windows1251 => "Windows-1251",
            Encoding::Windows1252 => "Windows-1252 / ISO-8859-1",
            Encoding::Windows1253 => "Windows-1253",
            Encoding::Windows1254 => "Windows-1254 / ISO-8859-9",
            Encoding::Windows1255 => "Windows-1255",
            Encoding::Windows1256 => "Windows-1256",
            Encoding::Windows1257 => "Windows-1257",
            Encoding::Windows1258 => "Windows-1258",
            Encoding::Windows874 => "Windows-874 / ISO-8859-11",
            Encoding::Iso8859_2 => "ISO-8859-2",
            Encoding::Iso8859_3 => "ISO-8859-3",
            Encoding::Iso8859_4 => "ISO-8859-4",
            Encoding::Iso8859_5 => "ISO-8859-5",
            Encoding::Iso8859_6 => "ISO-8859-6",
            Encoding::Iso8859_7 => "ISO-8859-7",
            Encoding::Iso8859_8 => "ISO-8859-8",
            Encoding::Iso8859_8I => "ISO-8859-8-I",
            Encoding::Iso8859_10 => "ISO-8859-10",
            Encoding::Iso8859_13 => "ISO-8859-13",
            Encoding::Iso8859_14 => "ISO-8859-14",
            Encoding::Iso8859_15 => "ISO-8859-15",
            Encoding::Iso8859_16 => "ISO-8859-16",
            Encoding::Koi8R => "KOI8-R",
            Encoding::Koi8U => "KOI8-U",
            Encoding::Ibm866 => "IBM866",
            Encoding::Macintosh => "macintosh",
            Encoding::XMacCyrillic => "x-mac-cyrillic",
            Encoding::XUserDefined => "x-user-defined",
        }
    }

    pub(crate) fn codec(self) -> &'static encoding_rs::Encoding {
        match self {
            Encoding::Utf8 => encoding_rs::UTF_8,
            Encoding::Gb18030 => encoding_rs::GB18030,
            Encoding::Gbk => encoding_rs::GBK,
            Encoding::Big5 => encoding_rs::BIG5,
            Encoding::ShiftJis => encoding_rs::SHIFT_JIS,
            Encoding::EucJp => encoding_rs::EUC_JP,
            Encoding::EucKr => encoding_rs::EUC_KR,
            Encoding::Windows1250 => encoding_rs::WINDOWS_1250,
            Encoding::Windows1251 => encoding_rs::WINDOWS_1251,
            Encoding::Windows1252 => encoding_rs::WINDOWS_1252,
            Encoding::Windows1253 => encoding_rs::WINDOWS_1253,
            Encoding::Windows1254 => encoding_rs::WINDOWS_1254,
            Encoding::Windows1255 => encoding_rs::WINDOWS_1255,
            Encoding::Windows1256 => encoding_rs::WINDOWS_1256,
            Encoding::Windows1257 => encoding_rs::WINDOWS_1257,
            Encoding::Windows1258 => encoding_rs::WINDOWS_1258,
            Encoding::Windows874 => encoding_rs::WINDOWS_874,
            Encoding::Iso8859_2 => encoding_rs::ISO_8859_2,
            Encoding::Iso8859_3 => encoding_rs::ISO_8859_3,
            Encoding::Iso8859_4 => encoding_rs::ISO_8859_4,
            Encoding::Iso8859_5 => encoding_rs::ISO_8859_5,
            Encoding::Iso8859_6 => encoding_rs::ISO_8859_6,
            Encoding::Iso8859_7 => encoding_rs::ISO_8859_7,
            Encoding::Iso8859_8 => encoding_rs::ISO_8859_8,
            Encoding::Iso8859_8I => encoding_rs::ISO_8859_8_I,
            Encoding::Iso8859_10 => encoding_rs::ISO_8859_10,
            Encoding::Iso8859_13 => encoding_rs::ISO_8859_13,
            Encoding::Iso8859_14 => encoding_rs::ISO_8859_14,
            Encoding::Iso8859_15 => encoding_rs::ISO_8859_15,
            Encoding::Iso8859_16 => encoding_rs::ISO_8859_16,
            Encoding::Koi8R => encoding_rs::KOI8_R,
            Encoding::Koi8U => encoding_rs::KOI8_U,
            Encoding::Ibm866 => encoding_rs::IBM866,
            Encoding::Macintosh => encoding_rs::MACINTOSH,
            Encoding::XMacCyrillic => encoding_rs::X_MAC_CYRILLIC,
            Encoding::XUserDefined => encoding_rs::X_USER_DEFINED,
        }
    }
}

enum Backing {
    Mapped(Mmap),
    /// 空文件在 Windows 上没法 mmap。
    Empty,
    /// 测试用的内存缓冲，免得每个用例都要落盘。
    #[cfg(test)]
    Owned(Vec<u8>),
}

pub struct FileSource {
    path: PathBuf,
    backing: Backing,
    len: u64,
    bom_len: usize,
    encoding: Encoding,
}

impl FileSource {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path).with_context(|| format!("打不开 {}", path.display()))?;
        let len = file
            .metadata()
            .with_context(|| format!("读不到 {} 的元信息", path.display()))?
            .len();

        let backing = if len == 0 {
            Backing::Empty
        } else {
            // SAFETY: 映射期间文件被外部截断会触发 SIGBUS/EXCEPTION。
            // 当前只支持静态日志文件，实时 tail 不在范围内（见 doc/PLAN.md 风险 4）。
            let mmap = unsafe { Mmap::map(&file) }
                .with_context(|| format!("映射 {} 失败", path.display()))?;
            Backing::Mapped(mmap)
        };

        let mut src = Self {
            path,
            backing,
            len,
            bom_len: 0,
            encoding: Encoding::Utf8,
        };
        let (enc, bom) = detect_encoding(src.data())?;
        src.encoding = enc;
        src.bom_len = bom;
        Ok(src)
    }

    #[inline]
    pub fn data(&self) -> &[u8] {
        match &self.backing {
            Backing::Mapped(m) => m,
            Backing::Empty => &[],
            #[cfg(test)]
            Backing::Owned(v) => v,
        }
    }

    /// 用内存里的字节造一个 `FileSource`，只给单测用。
    #[cfg(test)]
    pub fn from_bytes_for_test(bytes: Vec<u8>, encoding: Encoding) -> Self {
        let bom_len = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
            3
        } else {
            0
        };
        Self {
            path: PathBuf::from("<memory>"),
            len: bytes.len() as u64,
            backing: Backing::Owned(bytes),
            bom_len,
            encoding,
        }
    }

    #[inline]
    pub fn len(&self) -> u64 {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }

    #[inline]
    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// 文件头 BOM 的字节数。第一行的原始切片里含 BOM，渲染前要跳过。
    #[inline]
    pub fn bom_len(&self) -> usize {
        self.bom_len
    }

    /// 手动覆盖编码（UI 上的编码菜单）。不影响已建好的行索引——
    /// 菜单中的编码都是 ASCII 兼容的字节流，`b'\n'` 位置不变。
    pub fn set_encoding(&mut self, enc: Encoding) {
        self.encoding = enc;
    }

    /// 把 `[start, end)` 解码成字符串。非法字节走替换字符，绝不 panic。
    pub fn decode(&self, start: u64, end: u64) -> std::borrow::Cow<'_, str> {
        let data = self.data();
        let mut s = (start as usize).min(data.len());
        let e = (end as usize).clamp(s, data.len());
        // 文件头的 BOM 不该出现在第一行的正文里。
        if s == 0 {
            s = self.bom_len.min(e);
        }
        let bytes = &data[s..e];
        self.encoding.codec().decode_without_bom_handling(bytes).0
    }
}

/// 返回 (编码, BOM 字节数)。
fn detect_encoding(data: &[u8]) -> Result<(Encoding, usize)> {
    if data.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Ok((Encoding::Utf8, 3));
    }
    if data.starts_with(&[0xFF, 0xFE]) || data.starts_with(&[0xFE, 0xFF]) {
        bail!("检测到 UTF-16 BOM。logd 的行索引基于字节扫描 '\\n'，不支持 UTF-16，请先转成 UTF-8");
    }

    // 采样头部，能过 UTF-8 校验就当 UTF-8，否则回落 GB18030。
    let sample = &data[..data.len().min(64 * 1024)];
    match std::str::from_utf8(sample) {
        Ok(_) => Ok((Encoding::Utf8, 0)),
        // 采样正好切在多字节序列中间：error_len() == None，不算真错。
        Err(e)
            if e.error_len().is_none()
                && std::str::from_utf8(&sample[..e.valid_up_to()]).is_ok() =>
        {
            Ok((Encoding::Utf8, 0))
        }
        Err(_) => Ok((Encoding::Gb18030, 0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_utf8_bom() {
        let mut d = vec![0xEF, 0xBB, 0xBF];
        d.extend_from_slice(b"hello\n");
        assert_eq!(detect_encoding(&d).unwrap(), (Encoding::Utf8, 3));
    }

    #[test]
    fn detects_plain_utf8_with_chinese() {
        let d = "曝光表 AEtable\n".as_bytes();
        assert_eq!(detect_encoding(d).unwrap(), (Encoding::Utf8, 0));
    }

    #[test]
    fn falls_back_to_gb18030() {
        let (d, _, _) = encoding_rs::GB18030.encode("曝光表 AEtable\n");
        assert_eq!(detect_encoding(&d).unwrap(), (Encoding::Gb18030, 0));
    }

    #[test]
    fn rejects_utf16() {
        assert!(detect_encoding(&[0xFF, 0xFE, 0x41, 0x00]).is_err());
    }

    #[test]
    fn truncated_multibyte_at_sample_edge_is_still_utf8() {
        // 用一个超过 64KB 的串，让采样边界落在汉字中间
        let mut s = String::new();
        while s.len() < 64 * 1024 + 1 {
            s.push('曝');
        }
        assert_eq!(detect_encoding(s.as_bytes()).unwrap(), (Encoding::Utf8, 0));
    }

    #[test]
    fn every_supported_encoding_decodes_ascii() {
        for encoding in Encoding::ALL {
            let (text, had_errors) = encoding.codec().decode_without_bom_handling(b"log line");
            assert_eq!(text, "log line", "{}", encoding.label());
            assert!(!had_errors, "{}", encoding.label());
        }
    }

    #[test]
    fn decodes_big5_when_selected() {
        let (bytes, _, had_errors) = encoding_rs::BIG5.encode("繁體日誌");
        assert!(!had_errors);
        let source = FileSource::from_bytes_for_test(bytes.into_owned(), Encoding::Big5);
        assert_eq!(source.decode(0, source.len()), "繁體日誌");
    }
}
