//! 消息中心：集中存放"发生过什么"。
//!
//! 以前所有反馈都走底部状态条，6 秒后自己消失 —— 用户切走了就再也看不到
//! （"下完了没？""刚才那个失败了吗？"）。这里把值得留档的事件落成一条消息，
//! 同时在右下角弹一个小窗，弹窗消失后还能在「消息」页翻回来。
//!
//! 刻意不做系统级通知（`Shell_NotifyIcon` / toast）：那要走 COM 与
//! `AppUserModelID`，而插件本来就能通过 `notify` 权限触发这里，先保证
//! "看得见、翻得到"。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 消息级别：只影响配色与图标，不参与过滤逻辑（过滤按"未读"来）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Info,
    Ok,
    Warn,
    Error,
}

impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Level::Info => "提示",
            Level::Ok => "完成",
            Level::Warn => "注意",
            Level::Error => "失败",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notice {
    pub id: u64,
    pub level: Level,
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// 秒级 Unix 时间戳，和 `history.json` 用同一套格式。
    pub at: u64,
    #[serde(default)]
    pub read: bool,
}

/// 消息库。条数上限刻意压得很低：这是个下载器，不是日志系统，
/// 留太多只会让「消息」页变成负担。
const MAX_KEEP: usize = 200;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub next_id: u64,
    #[serde(default)]
    pub items: Vec<Notice>,
}

impl Store {
    pub fn path() -> PathBuf {
        adl_app::config::app_data_dir().join("notices.json")
    }

    pub fn load() -> Self {
        let p = Self::path();
        // 读不到 / 读坏了都当空库：消息是辅助信息，不该因为一个坏 JSON
        // 让整个界面起不来。
        let Ok(bytes) = std::fs::read(&p) else {
            return Self::default();
        };
        let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
        serde_json::from_slice(bytes).unwrap_or_default()
    }

    pub fn save(&self) {
        let p = Self::path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let Ok(text) = serde_json::to_vec_pretty(self) else {
            return;
        };
        // 原子写：先写 .tmp 再替换，避免半截文件让下次启动读不出来。
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, &text).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }

    /// 追一条消息，返回它的 id。
    pub fn push(&mut self, level: Level, title: impl Into<String>, body: impl Into<String>) -> u64 {
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        self.items.push(Notice {
            id,
            level,
            title: title.into(),
            body: body.into(),
            at: adl_app::history::now_unix(),
            read: false,
        });
        // 只保留最新的 MAX_KEEP 条，老的从前面砍掉。
        if self.items.len() > MAX_KEEP {
            let cut = self.items.len() - MAX_KEEP;
            self.items.drain(0..cut);
        }
        id
    }

    pub fn unread(&self) -> usize {
        self.items.iter().filter(|n| !n.read).count()
    }

    pub fn mark_all_read(&mut self) {
        for n in &mut self.items {
            n.read = true;
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }
}

/// 把"多久以前"写成中文字符串（消息页用）。
///
/// 和 `history` 里那个的区别：这里偏向"刚刚 / 5 分钟前"这种口语说法。
pub fn ago(unix: u64) -> String {
    let now = adl_app::history::now_unix();
    let d = now.saturating_sub(unix);
    if d < 60 {
        "刚刚".to_string()
    } else if d < 3600 {
        format!("{} 分钟前", d / 60)
    } else if d < 86_400 {
        format!("{} 小时前", d / 3600)
    } else {
        format!("{} 天前", d / 86_400)
    }
}
