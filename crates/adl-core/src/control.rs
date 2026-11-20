//! 断点续传控制文件 `<target>.adl`（DESIGN.md §3.5）。
//!
//! 三条硬规则：
//! 1. **原子写**：写 `.tmp` 再 `rename`，中途断电不会留下半个 JSON。
//! 2. **`piece_length` 必须存**。位图精度与它绑定，换了 piece_length 同一个 bit 就
//!    对应不同字节区间 → 文件静默损坏。不匹配直接拒绝续传（错误码 10）。
//! 3. **恢复时先发 `If-Range`**。服务器回 200 而不是 206 = 文件已变 → 整单重下。
//!    这条规则避免绝大多数续传损坏。

use crate::error::{AdlError, Result};
use crate::segment::SegmentTable;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const CONTROL_VERSION: u16 = 1;
/// 位图精度：1 MiB。与 aria2 的 `--piece-length` 默认值一致。
pub const DEFAULT_PIECE_LENGTH: u64 = 1024 * 1024;
pub const CONTROL_EXT: &str = "adl";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlFile {
    pub version: u16,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    pub total: u64,
    pub piece_length: u64,
    /// base64 位图。用 `hex` 之外的编码以免引入新依赖 —— 这里用十六进制，够短。
    pub bitfield: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

impl ControlFile {
    pub fn new(url: &str, total: u64, table: &SegmentTable, piece_length: u64) -> Self {
        Self {
            version: CONTROL_VERSION,
            url: url.to_string(),
            etag: None,
            last_modified: None,
            total,
            piece_length,
            bitfield: hex::encode(table.to_bitfield(piece_length)),
            sha256: None,
        }
    }

    /// 控制文件路径：`<target>.adl`。
    pub fn path_for(target: &Path) -> PathBuf {
        let mut s = target.as_os_str().to_os_string();
        s.push(".");
        s.push(CONTROL_EXT);
        PathBuf::from(s)
    }

    /// 原子写：先写 `.tmp`，再 `rename`。
    pub fn save(&self, target: &Path) -> Result<()> {
        let path = Self::path_for(target);
        let tmp = {
            let mut s = path.as_os_str().to_os_string();
            s.push(".tmp");
            PathBuf::from(s)
        };

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let json = serde_json::to_vec_pretty(self)?;
        std::fs::write(&tmp, &json)?;

        // Windows 上 rename 到已存在的目标会失败，先删
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// 读控制文件。不存在返回 `Ok(None)`，不是错误。
    pub fn load(target: &Path) -> Result<Option<Self>> {
        let path = Self::path_for(target);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path)?;
        // 容忍 UTF-8 BOM：用户可能用记事本/PowerShell 手改过控制文件
        let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
        let cf: ControlFile = serde_json::from_slice(bytes)?;
        Ok(Some(cf))
    }

    pub fn remove(target: &Path) {
        let _ = std::fs::remove_file(Self::path_for(target));
    }

    /// 校验能否续传，返回恢复后的分片表。
    ///
    /// 拒绝条件：
    /// - `version` 不认识（未来格式）
    /// - `piece_length` 与当前配置不一致（错误码 10）
    /// - `total` 与服务器给的不一致（文件已变）
    pub fn resume_table(&self, expected_total: u64, piece_length: u64) -> Result<SegmentTable> {
        if self.version != CONTROL_VERSION {
            return Err(AdlError::ResumeNotSupported);
        }
        if self.piece_length != piece_length {
            return Err(AdlError::PieceLengthChanged);
        }
        if self.total != expected_total {
            // 总长变了说明文件被替换，位图全部失效
            return Err(AdlError::ResumeNotSupported);
        }
        let bits = hex::decode(&self.bitfield).map_err(|_| AdlError::ResumeNotSupported)?;
        Ok(SegmentTable::from_bitfield(self.total, piece_length, &bits))
    }

    /// 更新位图（保存进度时调用）。
    pub fn update_from(&mut self, table: &SegmentTable) {
        self.bitfield = hex::encode(table.to_bitfield(self.piece_length));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        crate::testutil::tmpdir("ctl")
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tmpdir();
        let target = dir.join("a.bin");
        let table = SegmentTable::single(1000);
        let cf = ControlFile::new("https://example.com/a.bin", 1000, &table, 1024);
        cf.save(&target).unwrap();

        let back = ControlFile::load(&target).unwrap().expect("应有控制文件");
        assert_eq!(back.total, 1000);
        assert_eq!(back.piece_length, 1024);
        assert_eq!(back.url, "https://example.com/a.bin");

        ControlFile::remove(&target);
        assert!(ControlFile::load(&target).unwrap().is_none());
    }

    #[test]
    fn missing_control_is_none() {
        let dir = tmpdir();
        let target = dir.join("never-existed.bin");
        assert!(ControlFile::load(&target).unwrap().is_none());
    }

    #[test]
    fn piece_length_change_is_rejected() {
        let table = SegmentTable::single(1000);
        let cf = ControlFile::new("u", 1000, &table, 1024);
        // 用不同的 piece_length 续传
        let err = cf.resume_table(1000, 2048).unwrap_err();
        assert!(matches!(err, AdlError::PieceLengthChanged));
        assert_eq!(err.aria2_code(), 10);
    }

    #[test]
    fn total_change_is_rejected() {
        let table = SegmentTable::single(1000);
        let cf = ControlFile::new("u", 1000, &table, 1024);
        let err = cf.resume_table(2000, 1024).unwrap_err();
        assert!(matches!(err, AdlError::ResumeNotSupported));
        assert_eq!(err.aria2_code(), 8);
    }

    #[test]
    fn resume_preserves_done_segments() {
        let total = 4 * 1024u64;
        let pl = 1024u64;
        let mut table = SegmentTable::split(total, 4);
        // 完成第 0 和第 2 片
        table.assign(0, 1, 1);
        table.report_done(0, 1);
        table.assign(2, 1, 2);
        table.report_done(2, 2);

        let cf = ControlFile::new("u", total, &table, pl);
        let restored = cf.resume_table(total, pl).unwrap();
        assert_eq!(restored.completed(), 2 * pl);
        assert!(!restored.is_done());
        assert!(restored.segments[0].state.is_done());
        assert!(restored.segments[2].state.is_done());
    }

    #[test]
    fn atomic_write_leaves_no_tmp() {
        let dir = tmpdir();
        let target = dir.join("b.bin");
        let table = SegmentTable::single(10);
        ControlFile::new("u", 10, &table, 1024).save(&target).unwrap();

        let mut tmp = ControlFile::path_for(&target).as_os_str().to_os_string();
        tmp.push(".tmp");
        assert!(!PathBuf::from(tmp).exists(), "临时文件应已被 rename 掉");
    }

    #[test]
    fn overwrite_existing_control() {
        let dir = tmpdir();
        let target = dir.join("c.bin");
        let t1 = SegmentTable::single(10);
        ControlFile::new("u1", 10, &t1, 1024).save(&target).unwrap();
        let t2 = SegmentTable::single(20);
        ControlFile::new("u2", 20, &t2, 1024).save(&target).unwrap();

        let back = ControlFile::load(&target).unwrap().unwrap();
        assert_eq!(back.total, 20);
        assert_eq!(back.url, "u2");
    }

    #[test]
    fn control_path_appends_ext() {
        let p = ControlFile::path_for(Path::new(r"E:\dl\file.zip"));
        assert_eq!(p, PathBuf::from(r"E:\dl\file.zip.adl"));
    }
}
