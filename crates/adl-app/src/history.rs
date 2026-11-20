//! 下载历史（JSON 文件，原子写）。
//!
//! 单进程 CLI，不需要数据库；一个 `history.json` 足够。
//! 记录用于 `adl list` / `adl resume <id>` / `adl remove <id>`。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// 当前 unix 秒。
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 历史文件路径（跟配置放一起）。
pub fn history_path() -> PathBuf {
    crate::config::app_data_dir().join("history.json")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: u64,
    pub url: String,
    pub path: String,
    pub total: u64,
    /// "completed" | "failed" | "cancelled"
    pub status: String,
    pub error_code: Option<i32>,
    pub error: Option<String>,
    pub started_unix: u64,
    pub elapsed_ms: u64,
    pub sha256: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct History {
    #[serde(default)]
    pub tasks: Vec<TaskRecord>,
}

impl History {
    pub fn load() -> Self {
        let path = history_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => History::default(),
        }
    }

    pub fn save(&self) {
        let path = history_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    fn next_id(&self) -> u64 {
        self.tasks.iter().map(|t| t.id).max().unwrap_or(0) + 1
    }

    /// 追加一条记录，返回它的 id。
    pub fn add(&mut self, mut rec: TaskRecord) -> u64 {
        rec.id = self.next_id();
        let id = rec.id;
        self.tasks.push(rec);
        self.save();
        id
    }

    /// 最近优先。
    pub fn recent(&self, limit: usize) -> Vec<&TaskRecord> {
        let mut v: Vec<&TaskRecord> = self.tasks.iter().collect();
        v.sort_by(|a, b| b.id.cmp(&a.id));
        v.truncate(limit);
        v
    }

    pub fn find(&self, id: u64) -> Option<&TaskRecord> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// 删除一条记录，返回被删的记录。
    pub fn remove(&mut self, id: u64) -> Option<TaskRecord> {
        if let Some(pos) = self.tasks.iter().position(|t| t.id == id) {
            let rec = self.tasks.remove(pos);
            self.save();
            Some(rec)
        } else {
            None
        }
    }

    /// 清空状态为 completed 的记录，返回条数。
    pub fn clean_completed(&mut self) -> usize {
        let before = self.tasks.len();
        self.tasks.retain(|t| t.status != "completed");
        let removed = before - self.tasks.len();
        if removed > 0 {
            self.save();
        }
        removed
    }
}

/// 把 unix 秒显示成"3 分钟前"这种。
pub fn format_ago(unix: u64) -> String {
    let now = now_unix();
    let d = now.saturating_sub(unix);
    if d < 60 {
        format!("{d} 秒前")
    } else if d < 3600 {
        format!("{} 分钟前", d / 60)
    } else if d < 86400 {
        format!("{} 小时前", d / 3600)
    } else {
        format!("{} 天前", d / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(url: &str) -> TaskRecord {
        TaskRecord {
            id: 0,
            url: url.to_string(),
            path: format!("/tmp/{}", url),
            total: 100,
            status: "completed".to_string(),
            error_code: None,
            error: None,
            started_unix: 1,
            elapsed_ms: 1000,
            sha256: None,
        }
    }

    #[test]
    fn ids_increase_and_recent_is_desc() {
        let mut h = History::default();
        // 只测内存逻辑，不落盘
        h.tasks.push({
            let mut r = rec("a");
            r.id = 1;
            r
        });
        h.tasks.push({
            let mut r = rec("b");
            r.id = 2;
            r
        });
        assert_eq!(h.next_id(), 3);
        let recent = h.recent(10);
        assert_eq!(recent[0].id, 2);
        assert_eq!(recent[1].id, 1);
    }

    #[test]
    fn find_and_remove_in_memory() {
        let mut h = History::default();
        h.tasks.push({
            let mut r = rec("x");
            r.id = 7;
            r
        });
        assert!(h.find(7).is_some());
        // remove() 会 save()，这里只验证能找到
        assert_eq!(h.tasks.iter().position(|t| t.id == 7), Some(0));
    }

    #[test]
    fn clean_completed_keeps_failed() {
        let mut h = History::default();
        let mut a = rec("a");
        a.id = 1;
        a.status = "completed".into();
        let mut b = rec("b");
        b.id = 2;
        b.status = "failed".into();
        h.tasks.push(a);
        h.tasks.push(b);
        // 直接测 retain 逻辑（避免 clean_completed 的落盘）
        let before = h.tasks.len();
        h.tasks.retain(|t| t.status != "completed");
        assert_eq!(before - h.tasks.len(), 1);
        assert_eq!(h.tasks[0].id, 2);
    }

    #[test]
    fn ago_formatting() {
        let now = now_unix();
        assert!(format_ago(now).contains("秒前"));
        assert!(format_ago(now.saturating_sub(3600)).contains("小时前"));
    }
}
