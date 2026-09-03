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

/// 只支持 ASCII 兼容的字节编码——行索引靠扫 `b'\n'`，UTF-16 的低字节会误伤。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    /// GB18030 是 GBK/GB2312 的超集，国内平台日志用这个覆盖面最广。
    Gb18030,
}

impl Encoding {
    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Gb18030 => "GB18030",
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
        let bom_len = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) { 3 } else { 0 };
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
    /// 两种编码都是 ASCII 兼容的字节流，`b'\n'` 位置不变。
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
        match self.encoding {
            Encoding::Utf8 => String::from_utf8_lossy(bytes),
            Encoding::Gb18030 => encoding_rs::GB18030.decode_without_bom_handling(bytes).0,
        }
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
        Err(e) if e.error_len().is_none() && std::str::from_utf8(&sample[..e.valid_up_to()]).is_ok() => {
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
}
