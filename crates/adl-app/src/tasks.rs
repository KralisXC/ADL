//! 任务列表持久化（JSON 文件，原子写）。
//!
//! 为什么要有它：`history.json` 只记"下完/失败过什么"给主页热力图用，
//! 完全没有"现在列表里有哪些任务"这件事 —— 关掉再开列表就空了，用户只能
//! 靠「继续」按 URL 重新加一条（用户 m18583："任务列表可以添加筛选和排序
//! ……其他的按你的来"）。这里存的是**列表本身**：每条的 URL、目标目录、
//! 单任务 UA / Referer / 额外请求头 / Cookie、状态、已下字节。
//!
//! 和 `history.json` 的分工：
//! - `history.json`：只追加不可变的历史（用于统计）；
//! - `tasks.json`：当前工作集，随界面的增删改实时改写。

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::path::PathBuf;

/// 一条可恢复的任务。
///
/// 所有带 `#[serde(default)]` 的字段都是为了向后兼容：以后加字段时旧文件
/// 不会整份解析失败（解析失败就等于用户的任务全丢，代价太大）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedTask {
    pub id: u64,
    pub url: String,
    /// 显式指定的文件名（`--out` / 新建下载的"保存文件名"）
    #[serde(default)]
    pub out_name: Option<String>,
    /// 保存目录（空 = 用配置里的默认目录）
    #[serde(default)]
    pub dir: Option<String>,
    /// 这个任务专用的 UA（空 = 用设置里的默认 UA）
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub referer: Option<String>,
    /// 浏览器接管时带过来的额外请求头
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// 浏览器接管时带过来的 Cookie 串（拼成 `Cookie:` 头发出去）
    #[serde(default)]
    pub cookies: Option<String>,
    /// "downloading" | "queued" | "paused" | "completed" | "failed"
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub completed: u64,
    #[serde(default)]
    pub total: u64,
    /// 落盘路径（探测完才有；下载过程中是 `None`）
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub added_unix: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TaskStore {
    #[serde(default)]
    pub tasks: Vec<SavedTask>,
    /// 下一个可用 id。存下来是为了让重启后的新任务不会复用旧 id
    /// （否则"删除 #3 再新建"会出现两条 #3，历史对不上）。
    #[serde(default)]
    pub next_id: u64,
}

/// 任务列表文件路径（跟配置、历史放一起）。
pub fn tasks_path() -> PathBuf {
    crate::config::app_data_dir().join("tasks.json")
}

impl TaskStore {
    pub fn load() -> Self {
        Self::load_from(&tasks_path())
    }

    /// 从指定路径读。文件不存在 / 坏掉都返回空表 —— 列表丢了也不能挡住启动。
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => TaskStore::default(),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&tasks_path())
    }

    /// 原子写：先写 `.json.tmp` 再 rename，中途断电不会留下半个 JSON。
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// 表里最大的 id（老文件没写 `next_id` 时用它兜底）。
    pub fn max_id(&self) -> u64 {
        self.tasks.iter().map(|t| t.id).max().unwrap_or(0)
    }

    /// 需要继续跑的任务（下载中被强杀 / 排队中的）。
    pub fn unfinished(&self) -> Vec<&SavedTask> {
        self.tasks
            .iter()
            .filter(|t| matches!(t.status.as_str(), "downloading" | "queued" | "paused"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        // 不能用 std::env::temp_dir()：本机 %TEMP% 对 Rust 进程是拒绝访问的。
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("test-tmp")
            .join(format!("adl-tasks-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn sample(id: u64, status: &str) -> SavedTask {
        SavedTask {
            id,
            url: format!("https://example.com/f{id}.bin"),
            out_name: Some(format!("f{id}.bin")),
            dir: Some("E:/Downloads".into()),
            user_agent: Some("TestUA/1.0".into()),
            referer: Some("https://example.com/".into()),
            headers: vec![("X-Test".into(), "1".into())],
            cookies: Some("a=b".into()),
            status: status.into(),
            completed: 10,
            total: 100,
            path: None,
            error: None,
            added_unix: 1,
        }
    }

    #[test]
    fn roundtrips_all_per_task_options() {
        let dir = tmpdir("rt");
        let path = dir.join("tasks.json");
        let mut store = TaskStore::default();
        store.tasks.push(sample(1, "paused"));
        store.next_id = 9;
        store.save_to(&path).unwrap();

        let back = TaskStore::load_from(&path);
        assert_eq!(back.tasks.len(), 1);
        assert_eq!(back.next_id, 9);
        let t = &back.tasks[0];
        // 单任务的 UA / Referer / headers / cookies 必须一起回来，
        // 否则重启后继续下载会退成默认 UA，被站点拒绝。
        assert_eq!(t.user_agent.as_deref(), Some("TestUA/1.0"));
        assert_eq!(t.referer.as_deref(), Some("https://example.com/"));
        assert_eq!(t.headers, vec![("X-Test".to_string(), "1".to_string())]);
        assert_eq!(t.cookies.as_deref(), Some("a=b"));
        assert_eq!(t.dir.as_deref(), Some("E:/Downloads"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_empty_store() {
        let dir = tmpdir("empty");
        let back = TaskStore::load_from(&dir.join("nope.json"));
        assert!(back.tasks.is_empty());
        assert_eq!(back.max_id(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unfinished_keeps_paused_and_queued_only() {
        let mut store = TaskStore::default();
        store.tasks.push(sample(1, "downloading"));
        store.tasks.push(sample(2, "queued"));
        store.tasks.push(sample(3, "paused"));
        store.tasks.push(sample(4, "completed"));
        store.tasks.push(sample(5, "failed"));
        let ids: Vec<u64> = store.unfinished().iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!(store.max_id(), 5);
    }
}
