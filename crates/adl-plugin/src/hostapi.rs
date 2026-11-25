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
    /// 插件自己的目录（绝对路径）。插件常要调用随包分发的可执行文件
    /// （比如媒体插件自带的 `bin/ffmpeg.exe`），它没法自己知道装在哪儿。
    pub plugin_dir: String,
    pub declared: Permissions,
    pub mode: Mode,
    pub audit: Arc<Mutex<Vec<String>>>,
}

impl HostCtx {
    pub fn new(plugin_id: String, declared: Permissions, mode: Mode) -> Self {
        Self {
            plugin_id,
            plugin_dir: String::new(),
            declared,
            mode,
            audit: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 带上插件目录（由 `load_plugin` 传进来）。
    pub fn with_dir(mut self, dir: &std::path::Path) -> Self {
        self.plugin_dir = dir.display().to_string().replace('\\', "/");
        self
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
            "pluginDir" => {
                // 插件查自己的安装目录。取随包分发的可执行文件（ffmpeg、
                // yt-dlp 之类）时必须有这条 —— 插件不知道自己被装在哪。
                self.ok_data(json!({ "dir": self.plugin_dir }))
            }
            "fs.exists" => {
                // 只问"在不在"，不读内容，所以 fs_read 白名单够用，不需要 trusted
                // 之外的新权限。仍要求 trusted：路径本身也是信息。
                if !self.mode.trusted() {
                    return self.deny(op, "safe 模式不允许探测文件");
                }
                let path = p.get("path").and_then(Value::as_str).unwrap_or("");
                if !self.path_allowed(path, &self.declared.fs_read) {
                    return self.deny(op, "路径不在 manifest 声明的 fs_read 白名单内");
                }
                self.ok_data(json!({ "exists": std::path::Path::new(path).exists() }))
            }
            "fs.writeBytes" => {
                // 二进制写：`content` 是 base64。媒体插件要用它写封面、切片之类
                // 的二进制，文本写（fs.write）办不到。
                if !self.mode.trusted() {
                    return self.deny(op, "safe 模式不允许写文件");
                }
                let path = p.get("path").and_then(Value::as_str).unwrap_or("");
                let b64 = p.get("base64").and_then(Value::as_str).unwrap_or("");
                if !self.path_allowed(path, &self.declared.fs_write) {
                    return self.deny(op, "路径不在 manifest 声明的 fs_write 白名单内");
                }
                let bytes = match base64_decode(b64) {
                    Ok(b) => b,
                    Err(e) => return json!({ "ok": false, "error": format!("base64 解码失败: {e}") }).to_string(),
                };
                match std::fs::write(path, &bytes) {
                    Ok(()) => self.ok_data(json!({ "bytes": bytes.len() })),
                    Err(e) => json!({ "ok": false, "error": format!("写入失败: {e}") }).to_string(),
                }
            }
            "exec" => {
                if !self.declared.exec {
                    return self.deny(op, "manifest 未声明 exec 权限");
                }
                if !self.mode.trusted() {
                    return self.deny(op, "safe 模式不允许执行外部程序");
                }
                let program = p.get("program").and_then(Value::as_str).unwrap_or("");
                if program.trim().is_empty() {
                    return json!({ "ok": false, "error": "缺少 program" }).to_string();
                }
                let args: Vec<String> = p
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let cwd = p.get("cwd").and_then(Value::as_str).unwrap_or("");
                let timeout_ms = p
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(120_000)
                    .clamp(1_000, 3_600_000);
                match run_process(program, &args, cwd, timeout_ms) {
                    Ok(v) => v.to_string(),
                    Err(e) => json!({ "ok": false, "error": e }).to_string(),
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
            // `"*"` 是"任意路径"的显式写法。
            //
            // 媒体转换这类插件要处理用户在任意目录里的文件，写死一个
            // 前缀等于没法用；而它们本来就要 `exec` 权限（能跑任意程序
            // 已经比读写文件强得多），再拿路径白名单当安全边界是自欺
            // 欺人。所以给一个可读的显式开关，插件页里能一眼看到它声
            // 明了 `"*"`。
            if pre == "*" {
                return true;
            }
            let pre = std::path::Path::new(pre);
            !pre.as_os_str().is_empty() && p.starts_with(pre)
        })
    }
}

/// 极简 base64 解码（标准字母表 + padding）。空串返回空 Vec。
fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .map_err(|e| e.to_string())
}

/// 跑一个外部程序，带超时。
///
/// 用 `std::process` + 轮询而不是 tokio 的 `Command`：这个函数是在插件的 OS
/// 线程里被同步调用的（`invoke_sync` 那条路），那里没有 async 运行时可用。
/// 超时就 `kill` 并等到子进程真的退掉 —— 否则会留下孤儿子进程继续占着文件。
fn run_process(program: &str, args: &[String], cwd: &str, timeout_ms: u64) -> Result<Value, String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !cwd.trim().is_empty() {
        cmd.current_dir(cwd);
    }
    // Windows 上不要弹出控制台窗口（GUI 是 windows_subsystem，子进程默认会闪）。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("无法启动 {program}: {e}"))?;

    // 两个管道都得读，否则子进程写满管道缓冲就卡住了。
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_h = out_pipe.take().map(|mut p| {
        std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = p.read_to_end(&mut v);
            v
        })
    });
    let err_h = err_pipe.take().map(|mut p| {
        std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = p.read_to_end(&mut v);
            v
        })
    });

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    // kill 之后还要 wait 回收，不然是僵尸。
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(format!("等待 {program} 失败: {e}")),
        }
    };

    let stdout = out_h
        .and_then(|h| h.join().ok())
        .map(|b| String::from_utf8_lossy(&b).to_string())
        .unwrap_or_default();
    let stderr = err_h
        .and_then(|h| h.join().ok())
        .map(|b| String::from_utf8_lossy(&b).to_string())
        .unwrap_or_default();
    let code = status.and_then(|s| s.code()).unwrap_or(-1);

    Ok(json!({
        // 🔴 必须带 `ok`：插件侧 `adl.exec` 看到 `r.ok` 不为真就当成
        // "程序没起来"（PRELUDE 里那句 `if (r && r.ok) { r.ok = true; return r; }`），
        // 少这个字段会让跑成功的转码被报成"ffmpeg 起不来"。
        "ok": true,
        "code": code,
        "timedOut": timed_out,
        "stdout": stdout,
        "stderr": stderr,
    }))
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
