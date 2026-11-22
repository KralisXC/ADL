//! 宿主能力（host API）。JS 侧唯一出口是 `__adl_host(op, payloadJson) -> String`。
//!
//! 原则（DESIGN.md §4.6）：**manifest 声明 ≠ 用户授权**。这里同时检查
//! `declared`（manifest 写的）与运行模式（`--safe` / `--trusted`）；
//! 任何越权调用一律返回 `{"ok":false,"error":...}` 并写审计日志。

use crate::manifest::Permissions;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex, OnceLock};

/// 插件主动通知的出口。宿主（GUI）在启动时注册一个回调，把 `notify` 变成
/// 界面上的消息；CLI 不注册，就还退化成写 stderr。
///
/// 用全局而不是 `HostCtx` 字段：插件各自跑在独立线程上，`HostCtx` 在插件加载
/// 时就固定了，而宿主可能比插件晚就绪（GUI 是先建 hooks 再建窗口）。
pub type NotifySink = Arc<dyn Fn(&str, &str, &str) + Send + Sync>;

static NOTIFY_SINK: OnceLock<Mutex<Option<NotifySink>>> = OnceLock::new();

/// 注册插件通知出口（传 `None` 取消）。后注册的覆盖先注册的。
pub fn set_notify_sink(sink: Option<NotifySink>) {
    let slot = NOTIFY_SINK.get_or_init(|| Mutex::new(None));
    if let Ok(mut g) = slot.lock() {
        *g = sink;
    }
}

fn emit_notify(plugin: &str, title: &str, body: &str) -> bool {
    let Some(slot) = NOTIFY_SINK.get() else {
        return false;
    };
    let Ok(g) = slot.lock() else {
        return false;
    };
    match g.as_ref() {
        Some(f) => {
            f(plugin, title, body);
            true
        }
        None => false,
    }
}

/// 运行模式。`safe` 只允许 Observe/Guard 类能力；`trusted` 额外允许网络/文件/设置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Safe,
    Trusted,
}

impl Mode {
    pub fn trusted(self) -> bool {
        matches!(self, Mode::Trusted)
    }
}

#[derive(Debug, Clone)]
pub struct HostCtx {
    pub plugin_id: String,
    pub declared: Permissions,
    pub mode: Mode,
    pub audit: Arc<Mutex<Vec<String>>>,
}

impl HostCtx {
    pub fn new(plugin_id: String, declared: Permissions, mode: Mode) -> Self {
        Self {
            plugin_id,
            declared,
            mode,
            audit: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn deny(&self, op: &str, why: &str) -> String {
        let line = format!("[plugin:{}] 能力调用被拒绝：{op}（{why}）", self.plugin_id);
        eprintln!("{line}");
        if let Ok(mut a) = self.audit.lock() {
            a.push(line);
        }
        json!({ "ok": false, "error": format!("权限不足: {why}") }).to_string()
    }

    fn ok(&self) -> String {
        json!({ "ok": true }).to_string()
    }
    fn ok_data(&self, data: Value) -> String {
        json!({ "ok": true, "data": data }).to_string()
    }

    /// 唯一入口。
    pub fn respond(&self, op: &str, payload: &str) -> String {
        let p: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
        match op {
            "log" => {
                let msg = p.get("msg").and_then(Value::as_str).unwrap_or("");
                eprintln!("[plugin:{}] {msg}", self.plugin_id);
                self.ok()
            }
            "notify" => {
                if !self.declared.notify {
                    return self.deny(op, "manifest 未声明 notify 权限");
                }
                let title = p.get("title").and_then(Value::as_str).unwrap_or("");
                let body = p.get("body").and_then(Value::as_str).unwrap_or("");
                // 宿主有界面时走界面消息（可翻查），否则退回 stderr。
                if !emit_notify(&self.plugin_id, title, body) {
                    eprintln!("[plugin:{}] 🔔 {title} — {body}", self.plugin_id);
                }
                self.ok()
            }
            "getTask" => {
                // M3 CLI 无任务库；返回空对象，插件应容忍。
                self.ok_data(json!({}))
            }
            "settings.get" => {
                if !self.mode.trusted() {
                    return self.deny(op, "safe 模式不允许读取设置");
                }
                let key = p.get("key").and_then(Value::as_str).unwrap_or("");
                let def = p.get("default").cloned().unwrap_or(Value::Null);
                let _ = key;
                self.ok_data(def)
            }
            "httpGet" => {
                if !self.mode.trusted() || !self.declared.net {
                    return self.deny(op, "未获得网络权限");
                }
                // M3 暂不实现宿主侧 HTTP（留给 M4+）；诚实返回未实现。
                json!({ "ok": false, "error": "httpGet 在 M3 尚未实现" }).to_string()
            }
            "fs.read" => {
                if !self.mode.trusted() {
                    return self.deny(op, "safe 模式不允许读文件");
                }
                let path = p.get("path").and_then(Value::as_str).unwrap_or("");
                if !self.path_allowed(path, &self.declared.fs_read) {
                    return self.deny(op, "路径不在 manifest 声明的 fs_read 白名单内");
                }
                match std::fs::read(path) {
                    Ok(bytes) => {
                        let s = String::from_utf8_lossy(&bytes).to_string();
                        self.ok_data(json!({ "content": s, "bytes": bytes.len() }))
                    }
                    Err(e) => json!({ "ok": false, "error": format!("读取失败: {e}") }).to_string(),
                }
            }
            "fs.write" => {
                if !self.mode.trusted() {
                    return self.deny(op, "safe 模式不允许写文件");
                }
                let path = p.get("path").and_then(Value::as_str).unwrap_or("");
                let content = p.get("content").and_then(Value::as_str).unwrap_or("");
                if !self.path_allowed(path, &self.declared.fs_write) {
                    return self.deny(op, "路径不在 manifest 声明的 fs_write 白名单内");
                }
                match std::fs::write(path, content.as_bytes()) {
                    Ok(()) => self.ok(),
                    Err(e) => json!({ "ok": false, "error": format!("写入失败: {e}") }).to_string(),
                }
            }
            _ => self.deny(op, "未知能力"),
        }
    }

    fn path_allowed(&self, path: &str, prefixes: &[String]) -> bool {
        if path.is_empty() {
            return false;
        }
        let p = std::path::Path::new(path);
        prefixes.iter().any(|pre| {
            let pre = std::path::Path::new(pre);
            !pre.as_os_str().is_empty() && p.starts_with(pre)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(safe: bool) -> HostCtx {
        let mut perm = Permissions::default();
        perm.notify = true;
        perm.net = true;
        perm.fs_read = vec!["E:/dl".into()];
        perm.fs_write = vec!["E:/dl".into()];
        HostCtx::new(
            "t".into(),
            perm,
            if safe { Mode::Safe } else { Mode::Trusted },
        )
    }

    #[test]
    fn log_always_allowed() {
        let r = ctx(true).respond("log", r#"{"msg":"hi"}"#);
        assert!(r.contains("\"ok\":true"));
    }

    #[test]
    fn notify_requires_declaration() {
        let mut c = ctx(true);
        c.declared.notify = false;
        let r = c.respond("notify", r#"{"title":"a","body":"b"}"#);
        assert!(r.contains("\"ok\":false"));
    }

    #[test]
    fn fs_blocked_in_safe_mode() {
        let r = ctx(true).respond("fs.read", r#"{"path":"E:/dl/x"}"#);
        assert!(r.contains("\"ok\":false"));
        assert!(r.contains("safe"));
    }

    #[test]
    fn fs_prefix_enforced_when_trusted() {
        let c = ctx(false);
        let bad = c.respond("fs.read", r#"{"path":"E:/other/x"}"#);
        assert!(bad.contains("\"ok\":false"));
        let good = c.respond("fs.read", r#"{"path":"E:/dl/definitely-missing-xyz"}"#);
        // 路径在白名单内 → 通过权限检查，只是文件不存在
        assert!(!good.contains("权限不足"));
    }

    #[test]
    fn unknown_op_denied() {
        let r = ctx(false).respond("evil", "{}");
        assert!(r.contains("\"ok\":false"));
    }
}
