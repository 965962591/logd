//! 行索引的磁盘缓存（M5）。
//!
//! 50GB 文件首次建索引要 15–25s，但索引本身只有约 4MB。存成 sidecar，
//! 二次打开同一文件就能跳过整趟扫描。
//!
//! 缓存键是 (路径, 文件长度, mtime)。三者任一变化就作废重建——不做内容校验，
//! 因为对 50GB 文件算摘要比重建索引还慢。

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::index::{ChunkIndex, LineIndex};

const MAGIC: &[u8; 8] = b"LOGDIDX\x00";
/// 索引结构或 ANCHOR_STRIDE 一变就要 +1，旧缓存自动失效。
const FORMAT_VERSION: u32 = 1;

/// All application-owned state lives beside the executable so a deployed
/// folder remains self-contained.
pub fn application_cache_dir() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("找不到当前程序路径")?;
    let directory = executable
        .parent()
        .ok_or_else(|| anyhow!("当前程序路径没有父目录: {}", executable.display()))?;
    Ok(directory.join("cache"))
}

/// 缓存文件路径：`程序目录\cache\{hash}-{len}-{mtime}.idx`
pub fn cache_path(source: &Path) -> Result<PathBuf> {
    let meta = std::fs::metadata(source)
        .with_context(|| format!("读不到 {} 的元信息", source.display()))?;
    let mtime = mtime_nanos(&meta);
    let key = fnv1a64(source.to_string_lossy().as_bytes());
    let dir = application_cache_dir()?;
    Ok(dir.join(format!("{key:016x}-{}-{mtime}.idx", meta.len())))
}

/// 读缓存。任何不匹配/损坏都返回 `Ok(None)`，让调用方安静地回退到重建。
pub fn load(source: &Path) -> Result<Option<LineIndex>> {
    let path = cache_path(source)?;
    let (bytes, migrate) = match std::fs::read(&path) {
        Ok(bytes) => (bytes, false),
        Err(_) => {
            let Some(legacy_path) = legacy_cache_path(&path) else {
                return Ok(None);
            };
            let Ok(bytes) = std::fs::read(legacy_path) else {
                return Ok(None);
            };
            (bytes, true)
        }
    };
    match decode(&bytes) {
        Ok(idx) => {
            let meta = std::fs::metadata(source)?;
            if idx.file_len == meta.len() {
                if migrate {
                    let _ = write_cache(&path, &bytes);
                }
                Ok(Some(idx))
            } else {
                Ok(None)
            }
        }
        // 缓存坏了不是错误，删掉重建即可
        Err(_) => {
            let _ = std::fs::remove_file(&path);
            Ok(None)
        }
    }
}

pub fn store(source: &Path, index: &LineIndex) -> Result<()> {
    if !index.complete {
        // 首屏的部分索引没有缓存价值
        return Ok(());
    }
    let path = cache_path(source)?;
    write_cache(&path, &encode(index))
}

fn write_cache(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("建不了 {}", dir.display()))?;
    }
    // 先写临时文件再 rename，避免半截文件被当成有效缓存
    let tmp = path.with_extension("idx.tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("写不了 {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("重命名到 {} 失败", path.display()))?;
    Ok(())
}

fn legacy_cache_path(path: &Path) -> Option<PathBuf> {
    Some(
        dirs::data_local_dir()?
            .join("logd")
            .join("cache")
            .join(path.file_name()?),
    )
}

pub fn encode(index: &LineIndex) -> Vec<u8> {
    let anchors: usize = index.chunks.iter().map(|c| c.anchors.len()).sum();
    let mut out = Vec::with_capacity(48 + index.chunks.len() * 40 + anchors * 8);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&crate::index::ANCHOR_STRIDE.to_le_bytes());
    out.extend_from_slice(&index.file_len.to_le_bytes());
    out.extend_from_slice(&index.total_lines.to_le_bytes());
    out.extend_from_slice(&index.indexed_bytes.to_le_bytes());
    out.extend_from_slice(&(index.chunks.len() as u64).to_le_bytes());
    for c in &index.chunks {
        out.extend_from_slice(&c.start_byte.to_le_bytes());
        out.extend_from_slice(&c.end_byte.to_le_bytes());
        out.extend_from_slice(&c.start_line.to_le_bytes());
        out.extend_from_slice(&c.line_count.to_le_bytes());
        out.extend_from_slice(&(c.anchors.len() as u64).to_le_bytes());
        for a in &c.anchors {
            out.extend_from_slice(&a.to_le_bytes());
        }
    }
    out
}

pub fn decode(bytes: &[u8]) -> Result<LineIndex> {
    let mut r = Cursor { b: bytes, p: 0 };
    if r.take(8)? != MAGIC {
        bail!("magic 不对");
    }
    if r.u32()? != FORMAT_VERSION {
        bail!("格式版本不匹配");
    }
    if r.u64()? != crate::index::ANCHOR_STRIDE {
        bail!("ANCHOR_STRIDE 变了");
    }
    let file_len = r.u64()?;
    let total_lines = r.u64()?;
    let indexed_bytes = r.u64()?;
    let n = r.u64()? as usize;

    let mut chunks = Vec::with_capacity(n);
    let mut acc = 0u64;
    for _ in 0..n {
        let start_byte = r.u64()?;
        let end_byte = r.u64()?;
        let start_line = r.u64()?;
        let line_count = r.u64()?;
        let na = r.u64()? as usize;
        let mut anchors = Vec::with_capacity(na);
        for _ in 0..na {
            anchors.push(r.u64()?);
        }
        // 自洽性检查：行号必须严格前缀和，否则文件被改过
        if start_line != acc {
            bail!("chunk 行号不连续");
        }
        acc += line_count;
        chunks.push(ChunkIndex {
            start_byte,
            end_byte,
            start_line,
            line_count,
            anchors,
        });
    }
    if acc != total_lines {
        bail!("总行数与 chunk 之和不符");
    }

    Ok(LineIndex {
        chunks,
        total_lines,
        indexed_bytes,
        file_len,
        complete: true,
    })
}

struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let s = self
            .b
            .get(self.p..self.p + n)
            .ok_or_else(|| anyhow!("数据截断"))?;
        self.p += n;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

fn mtime_nanos(meta: &std::fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// FNV-1a。只用来给缓存文件名去重，不需要抗碰撞强度。
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::Progress;

    fn sample_index() -> LineIndex {
        let mut s = Vec::new();
        for i in 0..20_000 {
            s.extend_from_slice(format!("{i:08} payload\n").as_bytes());
        }
        LineIndex::build_full(&s, &Progress::default()).unwrap()
    }

    #[test]
    fn encode_decode_round_trip() {
        let idx = sample_index();
        let back = decode(&encode(&idx)).unwrap();
        assert_eq!(back.total_lines, idx.total_lines);
        assert_eq!(back.file_len, idx.file_len);
        assert_eq!(back.chunks.len(), idx.chunks.len());
        for (a, b) in idx.chunks.iter().zip(&back.chunks) {
            assert_eq!(a.start_byte, b.start_byte);
            assert_eq!(a.start_line, b.start_line);
            assert_eq!(a.line_count, b.line_count);
            assert_eq!(a.anchors, b.anchors);
        }
    }

    #[test]
    fn decoded_index_still_resolves_lines() {
        let mut s = Vec::new();
        for i in 0..20_000 {
            s.extend_from_slice(format!("{i:08} payload\n").as_bytes());
        }
        let idx = LineIndex::build_full(&s, &Progress::default()).unwrap();
        let back = decode(&encode(&idx)).unwrap();

        let (mut a, mut b) = (Vec::new(), Vec::new());
        idx.line_spans(&s, 12_345, 5, &mut a);
        back.line_spans(&s, 12_345, 5, &mut b);
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn rejects_truncated() {
        let bytes = encode(&sample_index());
        assert!(decode(&bytes[..bytes.len() / 2]).is_err());
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = encode(&sample_index());
        bytes[0] = b'X';
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_ne!(fnv1a64(b"a"), fnv1a64(b"b"));
    }

    #[test]
    fn application_cache_is_beside_the_executable() {
        let executable = std::env::current_exe().unwrap();
        assert_eq!(
            application_cache_dir().unwrap(),
            executable.parent().unwrap().join("cache")
        );
    }
}
