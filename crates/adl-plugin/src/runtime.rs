//! QuickJS 插件运行时（DESIGN.md §4.7 / §4.8）。
//!
//! QuickJS 的 `Runtime`/`Context` 是 `!Send`，所以**每个插件独占一条 OS 线程**，
//! 宿主通过 `mpsc` 投递 `Job`。同步 hook 等回复并带超时（`set_interrupt_handler`
//! 到点强制中断 JS），Observe hook 用 `try_send` 投递、队列满直接丢，绝不阻塞下载。

use crate::hostapi::{HostCtx, Mode};
use crate::manifest::{HookPoint, Manifest, MANIFEST_FILE};
use rquickjs::{Context, Function, Runtime, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 注入到 JS 全局的薄封装。唯一宿主出口是 `__adl_host`；`__adl_invoke` 是调用入口，
/// 内部 try/catch 把异常转成 `{"action":"error"}`（Rust 侧永不进入 JS 异常悬置态）。
const PRELUDE: &str = r#"
"use strict";
globalThis.__adl_handlers = {};
globalThis.adl = {
  onResolve:       (f) => { __adl_handlers.onResolve = f; },
  onPlan:          (f) => { __adl_handlers.onPlan = f; },
  onBeforeRequest: (f) => { __adl_handlers.onBeforeRequest = f; },
  onResponse:      (f) => { __adl_handlers.onResponse = f; },
  onProgress:      (f) => { __adl_handlers.onProgress = f; },
  onRetry:         (f) => { __adl_handlers.onRetry = f; },
  onPreempt:       (f) => { __adl_handlers.onPreempt = f; },
  onDegrade:       (f) => { __adl_handlers.onDegrade = f; },
  onComplete:      (f) => { __adl_handlers.onComplete = f; },
  onUiRender:      (f) => { __adl_handlers.onUiRender = f; },
  onAction:        (f) => { __adl_handlers.onAction = f; },
  onCommand:       (f) => { __adl_handlers.onCommand = f; },
  log:    (...a) => __adl_host("log", JSON.stringify({ msg: a.map(String).join(" ") })),
  notify: (title, body) => __adl_host("notify", JSON.stringify({ title: String(title), body: String(body == null ? "" : body) })),
  getTask: () => { try { return JSON.parse(__adl_host("getTask", "{}")).data || {}; } catch(e) { return {}; } },
  // 插件自己的安装目录（绝对路径）。随包分发的可执行文件放这里。
  pluginDir: () => { try { var r = JSON.parse(__adl_host("pluginDir", "{}")); return (r && r.ok && r.data) ? r.data.dir : ""; } catch(e) { return ""; } },
  fs: {
    exists: (path) => { try { var r = JSON.parse(__adl_host("fs.exists", JSON.stringify({path: String(path)}))); return !!(r && r.ok && r.data && r.data.exists); } catch(e) { return false; } },
    read: (path) => { try { var r = JSON.parse(__adl_host("fs.read", JSON.stringify({path: String(path)}))); return (r && r.ok && r.data) ? r.data.content : null; } catch(e) { return null; } },
    write: (path, content) => { try { var r = JSON.parse(__adl_host("fs.write", JSON.stringify({path: String(path), content: String(content)}))); return !!(r && r.ok); } catch(e) { return false; } },
    writeBytes: (path, base64) => { try { var r = JSON.parse(__adl_host("fs.writeBytes", JSON.stringify({path: String(path), base64: String(base64)}))); return !!(r && r.ok); } catch(e) { return false; } }
  },
  // 执行外部程序（需要 manifest 的 `exec = true` 且 trusted 模式）。
  // 返回 {ok, code, timedOut, stdout, stderr} —— 失败也返回 ok:false 而不是抛。
  exec: (opts) => {
    opts = opts || {};
    var payload = { program: String(opts.program || ""), args: (opts.args || []).map(String) };
    if (opts.cwd) payload.cwd = String(opts.cwd);
    if (opts.timeoutMs) payload.timeoutMs = opts.timeoutMs | 0;
    try {
      var r = JSON.parse(__adl_host("exec", JSON.stringify(payload)));
      if (r && r.ok) { r.ok = true; return r; }
      return r || { ok: false };
    } catch(e) { return { ok: false, error: String(e) }; }
  },
  settings: {
    get: (key, dflt) => { try { var r = JSON.parse(__adl_host("settings.get", JSON.stringify({key: key, default: dflt}))); return (r && r.ok && "data" in r) ? r.data : dflt; } catch(e) { return dflt; } }
  }
};
globalThis.__adl_invoke = function(hook, payloadJson) {
  try {
    var f = __adl_handlers[hook];
    if (typeof f !== "function") return JSON.stringify({ action: "pass" });
    var r = f(JSON.parse(payloadJson));
    if (r === undefined || r === null) return JSON.stringify({ action: "pass" });
    if (typeof r === "string") return JSON.stringify({ action: "pass", value: r });
    return JSON.stringify(r);
  } catch (e) {
    return JSON.stringify({ action: "error", error: String(e && e.stack ? e.stack : e) });
  }
};
"#;

#[derive(Debug)]
pub enum Job {
    Invoke {
        hook: String,
        payload: String,
        reply: SyncSender<String>,
    },
    Notify {
        hook: String,
        payload: String,
    },
    Shutdown,
}

/// 一个已加载的插件实例。
pub struct LoadedPlugin {
    pub id: String,
    pub manifest: Manifest,
    pub dir: PathBuf,
    pub mode: Mode,
    pub subs: Vec<(HookPoint, i64)>,
    pub active: HashSet<HookPoint>,
    pub tx: SyncSender<Job>,
    pub timeout_ms: u64,
    pub audit: Arc<Mutex<Vec<String>>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl LoadedPlugin {
    pub fn has(&self, p: HookPoint) -> bool {
        self.active.contains(&p)
    }

    fn send_job(&self, job: Job) -> Result<(), ()> {
        self.tx.send(job).map_err(|_| ())
    }

    /// 同步调用（等回复，带超时）。超时/线程死掉都返回 `None`（调用方按 pass 处理）。
    pub fn invoke_sync(&self, hook: &str, payload: &str) -> Option<String> {
        let (rtx, rrx) = sync_channel::<String>(1);
        let job = Job::Invoke {
            hook: hook.to_string(),
            payload: payload.to_string(),
            reply: rtx,
        };
        if self.tx.send(job).is_err() {
            return None;
        }
        match rrx.recv_timeout(Duration::from_millis(self.timeout_ms)) {
            Ok(s) => Some(s),
            Err(RecvTimeoutError::Timeout) => {
                eprintln!(
                    "[plugin:{}] hook {hook} 超时（{}ms），按 pass 处理",
                    self.id, self.timeout_ms
                );
                None
            }
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }

    /// 异步投递（不等回复）。队列满直接丢，绝不阻塞。
    pub fn notify_async(&self, hook: &str, payload: &str) {
        let job = Job::Notify {
            hook: hook.to_string(),
            payload: payload.to_string(),
        };
        if let Err(std::sync::mpsc::TrySendError::Full(_)) = self.tx.try_send(job) {
            // 丢弃即可（Observe hook 语义）
        }
    }

    pub fn shutdown(&mut self) {
        let _ = self.send_job(Job::Shutdown);
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}

impl Drop for LoadedPlugin {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn js_lit(s: &str) -> String {
    // serde_json 产出的双引号转义串是合法 JS 字符串字面量
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// 加载一个插件目录（含 `plugin.toml`）。失败返回 Err（调用方决定是否继续）。
pub fn load_plugin(dir: &Path, mode: Mode) -> Result<LoadedPlugin, String> {
    let manifest_path = dir.join(MANIFEST_FILE);
    let manifest = Manifest::load(&manifest_path).map_err(|e| e.to_string())?;
    let entry_path = dir.join(&manifest.entry);
    let source = std::fs::read_to_string(&entry_path)
        .map_err(|e| format!("读不到入口 {}: {e}", entry_path.display()))?;

    let subs: Vec<(HookPoint, i64)> = manifest.hook_subscriptions();
    let active = manifest.active_hooks(mode.trusted());

    let host = Arc::new(
        HostCtx::new(
            manifest.id.clone(),
            manifest.permissions.clone(),
            mode,
        )
        .with_dir(dir),
    );
    let audit = host.audit.clone();
    let memory_mb = manifest.limits.memory_mb;
    let default_timeout = manifest.limits.timeout_ms;

    let (tx, rx) = sync_channel::<Job>(64);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let thread_name = format!("adl-plugin-{}", manifest.id);

    let join = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let rt = match Runtime::new() {
                Ok(r) => r,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("创建 QuickJS Runtime 失败: {e}")));
                    return;
                }
            };
            rt.set_memory_limit(memory_mb.saturating_mul(1024 * 1024));
            rt.set_max_stack_size(512 * 1024);

            // 超时中断：deadline 是"自 start 起的毫秒数"，0 表示不限制。
            let start = Instant::now();
            let deadline = Arc::new(AtomicU64::new(0));
            {
                let dl = deadline.clone();
                rt.set_interrupt_handler(Some(Box::new(move || {
                    let d = dl.load(Ordering::Relaxed);
                    d != 0 && start.elapsed().as_millis() as u64 >= d
                })));
            }

            let ctx = match Context::full(&rt) {
                Ok(c) => c,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("创建 Context 失败: {e}")));
                    return;
                }
            };

            let init: Result<(), String> = ctx.with(|ctx| {
                let host2 = host.clone();
                let f = Function::new(ctx.clone(), move |op: String, payload: String| -> String {
                    host2.respond(&op, &payload)
                })
                .map_err(|e| format!("注册 __adl_host 失败: {e}"))?;
                ctx.globals()
                    .set("__adl_host", f)
                    .map_err(|e| format!("挂载 __adl_host 失败: {e}"))?;
                let _: Value = ctx
                    .eval(PRELUDE)
                    .map_err(|e| format!("PRELUDE 执行失败: {e}"))?;
                let _: Value = ctx
                    .eval(source)
                    .map_err(|e| format!("插件入口执行失败: {e}"))?;
                Ok(())
            });
            match init {
                Ok(()) => {
                    let _ = ready_tx.send(Ok(()));
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            }

            let start2 = Instant::now();
            loop {
                match rx.recv() {
                    Ok(Job::Invoke {
                        hook,
                        payload,
                        reply,
                    }) => {
                        deadline.store(
                            (start2.elapsed().as_millis() as u64)
                                .saturating_add(default_timeout),
                            Ordering::Relaxed,
                        );
                        let out = ctx.with(|ctx| call_invoke(&ctx, &hook, &payload));
                        deadline.store(0, Ordering::Relaxed);
                        let _ = reply.send(out);
                    }
                    Ok(Job::Notify { hook, payload }) => {
                        let _ = ctx.with(|ctx| call_invoke(&ctx, &hook, &payload));
                    }
                    Ok(Job::Shutdown) | Err(_) => break,
                }
            }
        })
        .map_err(|e| format!("创建插件线程失败: {e}"))?;

    match ready_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let _ = join.join();
            return Err(e);
        }
        Err(_) => {
            return Err("插件初始化超时".to_string());
        }
    }

    Ok(LoadedPlugin {
        id: manifest.id.clone(),
        manifest,
        dir: dir.to_path_buf(),
        mode,
        subs,
        active,
        tx,
        timeout_ms: default_timeout,
        audit,
        join: Some(join),
    })
}

fn call_invoke(ctx: &rquickjs::Ctx<'_>, hook: &str, payload: &str) -> String {
    let script = format!("__adl_invoke({}, {})", js_lit(hook), js_lit(payload));
    match ctx.eval::<String, _>(script) {
        Ok(s) => s,
        Err(e) => format!(
            "{{\"action\":\"error\",\"error\":{}}}",
            js_lit(&format!("JS 执行异常: {e}"))
        ),
    }
}

/// 扫描插件根目录：每个含 `plugin.toml` 的子目录算一个插件。坏插件只警告不中断。
pub fn load_dir(root: &Path, mode: Mode) -> Vec<LoadedPlugin> {
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[plugin] 无法读取插件目录 {}: {e}", root.display());
            return out;
        }
    };
    let mut entries: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join(MANIFEST_FILE).is_file())
        .collect();
    entries.sort();
    for dir in entries {
        match load_plugin(&dir, mode) {
            Ok(p) => {
                eprintln!(
                    "[plugin] 已加载 {}（{}）hooks={:?}",
                    p.manifest.name,
                    p.id,
                    p.active
                        .iter()
                        .map(|h| h.canonical())
                        .collect::<Vec<_>>()
                );
                out.push(p);
            }
            Err(e) => eprintln!("[plugin] 加载 {} 失败: {e}", dir.display()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn base_root() -> PathBuf {
        // 本机 %TEMP% 不可写（沙箱 ACL），统一落到 workspace 的 target/test-tmp。
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop();
        p.pop();
        p.push("target");
        p.push("test-tmp");
        p
    }

    fn tmp_plugin(id: &str, toml: &str, js: &str) -> PathBuf {
        let _ = std::fs::create_dir_all(base_root());
        let base = base_root().join(format!("adl-plugin-test-{}-{}", id, std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::File::create(base.join("plugin.toml"))
            .unwrap()
            .write_all(toml.as_bytes())
            .unwrap();
        std::fs::File::create(base.join("main.js"))
            .unwrap()
            .write_all(js.as_bytes())
            .unwrap();
        base
    }

    #[test]
    fn loads_and_invokes_hook() {
        let toml = r#"
id = "t"
name = "T"
api_version = 1
entry = "main.js"
kind = "quickjs"
[permissions]
[[hooks]]
point = "onBeforeRequest"
"#;
        let js = r#"adl.onBeforeRequest((r) => ({ action: "rewrite", headers: { "X-Test": "yes" } }));"#;
        let dir = tmp_plugin("load", toml, js);
        let p = load_plugin(&dir, Mode::Trusted).unwrap();
        let out = p.invoke_sync("onBeforeRequest", r#"{"segIndex":0,"start":0,"end":8}"#).unwrap();
        assert!(out.contains("X-Test"), "got {out}");
        assert!(out.contains("rewrite"), "got {out}");
    }

    #[test]
    fn js_exception_becomes_error_action() {
        let toml = r#"
id = "t2"
name = "T2"
api_version = 1
entry = "main.js"
[[hooks]]
point = "onBeforeRequest"
"#;
        let js = r#"adl.onBeforeRequest(() => { throw new Error("boom"); });"#;
        let dir = tmp_plugin("exc", toml, js);
        let p = load_plugin(&dir, Mode::Trusted).unwrap();
        let out = p.invoke_sync("onBeforeRequest", "{}").unwrap();
        assert!(out.contains("\"action\":\"error\""), "got {out}");
    }

    #[test]
    fn host_log_works() {
        let toml = r#"
id = "t3"
name = "T3"
api_version = 1
entry = "main.js"
[[hooks]]
point = "onComplete"
"#;
        let js = r#"adl.onComplete(() => { adl.log("hello-host"); return { action: "pass" }; });"#;
        let dir = tmp_plugin("log", toml, js);
        let p = load_plugin(&dir, Mode::Trusted).unwrap();
        let out = p.invoke_sync("onComplete", r#"{"path":"x","total":1,"sha256":null}"#).unwrap();
        assert!(out.contains("pass"), "got {out}");
    }
}
