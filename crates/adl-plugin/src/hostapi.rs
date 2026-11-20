//! 宿主能力（host API）。JS 侧唯一出口是 `__adl_host(op, payloadJson) -> String`。
//!
//! 原则（DESIGN.md §4.6）：**manifest 声明 ≠ 用户授权**。这里同时检查
//! `declared`（manifest 写的）与运行模式（`--safe` / `--trusted`）；
//! 任何越权调用一律返回 `{"ok":false,"error":...}` 并写审计日志。

use crate::manifest::Permissions;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

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
                // M3：写到 stderr；M5 接系统通知与 UI。
                eprintln!("[plugin:{}] 🔔 {title} — {body}", self.plugin_id);
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
