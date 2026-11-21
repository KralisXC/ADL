//! `Hooks` 的插件实现：把引擎的每次 hook 调用转成对 JS 插件的调用（DESIGN.md §4.3）。
//!
//! 引擎主流程一行不改 —— M1/M2 预留的 9 个 hook 点在这里全部接上。
//!
//! 合并语义（DEGISN.md §4.3）：
//! - Guard：任一插件 Deny 即短路。
//! - Transform：按 priority 升序链式；后写覆盖先写的同名 header。
//! - Observe：只投递不等结果，队列满丢弃。

use crate::manifest::HookPoint;
use crate::runtime::{load_dir, LoadedPlugin};
use crate::hostapi::Mode;
use adl_core::hooks::{
    Candidate, Completed, DownloadPlan, Hooks, Progress, ResponseVerdict, Verdict,
};
use adl_core::error::AdlError;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

pub struct PluginHooks {
    plugins: Vec<LoadedPlugin>,
}

impl PluginHooks {
    pub fn new(plugins: Vec<LoadedPlugin>) -> Self {
        Self { plugins }
    }

    pub fn load(root: &Path, mode: Mode) -> Self {
        Self::new(load_dir(root, mode))
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// 订阅了某个 hook 的插件，按 priority 升序。
    fn ordered(&self, point: HookPoint) -> Vec<&LoadedPlugin> {
        let mut v: Vec<&LoadedPlugin> = self
            .plugins
            .iter()
            .filter(|p| p.has(point))
            .collect();
        v.sort_by_key(|p| {
            p.subs
                .iter()
                .filter(|(hp, _)| *hp == point)
                .map(|(_, pr)| *pr)
                .min()
                .unwrap_or(0)
        });
        v
    }

    fn sync_call(&self, p: &LoadedPlugin, hook: &str, payload: &str) -> Option<String> {
        // hook 现在是在 tokio async worker 里被同步调用的；必须让出 worker，
        // 否则插件慢会占住 tokio 工作线程（multi_thread runtime 支持 block_in_place）。
        match tokio::runtime::Handle::try_current() {
            Ok(_) => tokio::task::block_in_place(|| p.invoke_sync(hook, payload)),
            Err(_) => p.invoke_sync(hook, payload),
        }
    }

    /// 汇总所有插件的 UI 声明（页面/按钮/设置/命令/替换内置页）。
    pub fn ui_snapshot(&self, values: &crate::ui::SettingsValues) -> Value {
        let trusted = self.plugins.iter().any(|p| p.mode.trusted());
        crate::ui::collect(&self.plugins, values, trusted)
    }

    fn find(&self, plugin: &str) -> Option<&LoadedPlugin> {
        self.plugins.iter().find(|p| p.id == plugin)
    }

    /// 调插件 `onUiRender(page, ctx)`，返回声明式组件树。
    pub fn render(&self, plugin: &str, page: &str, ctx: &Value) -> Option<Value> {
        let p = self.find(plugin)?;
        let payload = json!({ "page": page, "ctx": ctx }).to_string();
        let s = self.sync_call(p, "onUiRender", &payload)?;
        serde_json::from_str::<Value>(&s).ok()
    }

    /// 调插件 `onAction(action, payload)`（仅 trusted）。
    pub fn run_action(&self, plugin: &str, action: &str, payload: &Value) -> Option<Value> {
        let p = self.find(plugin)?;
        if !p.mode.trusted() {
            return None;
        }
        let body = json!({ "action": action, "payload": payload }).to_string();
        let s = self.sync_call(p, "onAction", &body)?;
        serde_json::from_str::<Value>(&s).ok()
    }

    /// 调插件 `onCommand(command)`（仅 trusted）。
    pub fn run_command(&self, plugin: &str, command: &str) -> Option<Value> {
        let p = self.find(plugin)?;
        if !p.mode.trusted() {
            return None;
        }
        let body = json!({ "command": command }).to_string();
        let s = self.sync_call(p, "onCommand", &body)?;
        serde_json::from_str::<Value>(&s).ok()
    }

    /// 是否有任何插件声明了 UI。
    pub fn has_ui(&self) -> bool {
        self.plugins.iter().any(|p| {
            let u = &p.manifest.ui;
            !u.pages.is_empty()
                || !u.actions.is_empty()
                || !u.settings.is_empty()
                || !u.commands.is_empty()
        })
    }
}

fn merge_header(headers: &mut Vec<(String, String)>, k: &str, v: &str) {
    headers.retain(|(hk, _)| !hk.eq_ignore_ascii_case(k));
    headers.push((k.to_string(), v.to_string()));
}

fn apply_header_object(headers: &mut Vec<(String, String)>, obj: Option<&serde_json::Map<String, Value>>) {
    if let Some(obj) = obj {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                merge_header(headers, k, s);
            }
        }
    }
}

impl Hooks for PluginHooks {
    fn on_resolve(&self, url: &str) -> Result<Vec<Candidate>, AdlError> {
        let mut cand = Candidate {
            url: url.to_string(),
            ..Default::default()
        };
        for p in self.ordered(HookPoint::Resolve) {
            let payload = json!({ "url": cand.url }).to_string();
            let Some(s) = self.sync_call(p, "onResolve", &payload) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&s) else {
                continue;
            };
            if v.get("action").and_then(Value::as_str) != Some("rewrite") {
                continue;
            }
            if let Some(u) = v.get("url").and_then(Value::as_str) {
                if !u.is_empty() {
                    cand.url = u.to_string();
                }
            }
            if let Some(ua) = v.get("userAgent").and_then(Value::as_str) {
                cand.user_agent = Some(ua.to_string());
            }
            if let Some(rf) = v.get("referer").and_then(Value::as_str) {
                cand.referer = Some(rf.to_string());
            }
            apply_header_object(&mut cand.headers, v.get("headers").and_then(Value::as_object));
        }
        Ok(vec![cand])
    }

    fn on_plan(&self, plan: &mut DownloadPlan) -> Result<(), AdlError> {
        for p in self.ordered(HookPoint::Plan) {
            let payload = json!({
                "total": plan.total,
                "segments": plan.segments.segments.len(),
                "concurrency": plan.concurrency,
                "userAgent": plan.user_agent,
                "referer": plan.referer,
            })
            .to_string();
            let Some(s) = self.sync_call(p, "onPlan", &payload) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&s) else {
                continue;
            };
            if v.get("action").and_then(Value::as_str) != Some("rewrite") {
                continue;
            }
            if let Some(n) = v.get("concurrency").and_then(Value::as_u64) {
                plan.concurrency = adl_core::clamp_concurrency(n as usize);
            }
            if let Some(ua) = v.get("userAgent").and_then(Value::as_str) {
                if !ua.is_empty() {
                    plan.user_agent = ua.to_string();
                }
            }
            if let Some(rf) = v.get("referer").and_then(Value::as_str) {
                plan.referer = Some(rf.to_string());
            }
            apply_header_object(&mut plan.headers, v.get("headers").and_then(Value::as_object));
        }
        Ok(())
    }

    fn on_before_request(&self, seg_index: usize, start: u64, end: u64) -> Result<Verdict, AdlError> {
        let mut headers: Vec<(String, String)> = Vec::new();
        for p in self.ordered(HookPoint::BeforeRequest) {
            let payload = json!({ "segIndex": seg_index, "start": start, "end": end }).to_string();
            let Some(s) = self.sync_call(p, "onBeforeRequest", &payload) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&s) else {
                continue;
            };
            match v.get("action").and_then(Value::as_str).unwrap_or("pass") {
                "deny" => {
                    let reason = v
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("插件拒绝")
                        .to_string();
                    return Ok(Verdict::Deny { reason });
                }
                "rewrite" => {
                    apply_header_object(&mut headers, v.get("headers").and_then(Value::as_object));
                }
                _ => {}
            }
        }
        if headers.is_empty() {
            Ok(Verdict::Allow)
        } else {
            Ok(Verdict::Rewrite { headers })
        }
    }

    fn on_response(
        &self,
        seg_index: usize,
        status: u16,
        content_type: Option<&str>,
    ) -> Result<ResponseVerdict, AdlError> {
        let mut retry_after: Option<Duration> = None;
        for p in self.ordered(HookPoint::Response) {
            let payload = json!({
                "segIndex": seg_index,
                "status": status,
                "contentType": content_type,
            })
            .to_string();
            let Some(s) = self.sync_call(p, "onResponse", &payload) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&s) else {
                continue;
            };
            match v.get("action").and_then(Value::as_str).unwrap_or("pass") {
                "deny" => {
                    let reason = v
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("插件拒绝响应")
                        .to_string();
                    return Ok(ResponseVerdict::Deny { reason });
                }
                "retry" => {
                    let ms = v.get("afterMs").and_then(Value::as_u64).unwrap_or(1000);
                    let d = Duration::from_millis(ms);
                    retry_after = Some(retry_after.map_or(d, |prev| prev.max(d)));
                }
                _ => {}
            }
        }
        Ok(match retry_after {
            Some(after) => ResponseVerdict::Retry { after },
            None => ResponseVerdict::Allow,
        })
    }

    fn on_progress(&self, p: Progress) {
        let payload = json!({
            "completed": p.completed,
            "total": p.total,
            "speedBps": p.speed_bps,
            "connections": p.connections,
        })
        .to_string();
        for pl in self.ordered(HookPoint::Progress) {
            pl.notify_async("onProgress", &payload);
        }
    }

    fn on_retry(&self, seg_index: usize, attempt: u8, error: &str) {
        let payload = json!({ "segIndex": seg_index, "attempt": attempt, "error": error }).to_string();
        for pl in self.ordered(HookPoint::Retry) {
            pl.notify_async("onRetry", &payload);
        }
    }

    fn on_preempt(&self, seg_index: usize, preempts: u8) {
        let payload = json!({ "segIndex": seg_index, "preempts": preempts }).to_string();
        for pl in self.ordered(HookPoint::Preempt) {
            pl.notify_async("onPreempt", &payload);
        }
    }

    fn on_degrade(&self, concurrency: usize, reason: &str) {
        let payload = json!({ "concurrency": concurrency, "reason": reason }).to_string();
        for pl in self.ordered(HookPoint::Degrade) {
            pl.notify_async("onDegrade", &payload);
        }
    }

    fn on_complete(&self, c: &Completed) -> Result<(), AdlError> {
        let payload = json!({
            "path": c.path.to_string_lossy(),
            "total": c.total,
            "sha256": c.sha256,
        })
        .to_string();
        for p in self.ordered(HookPoint::Complete) {
            // 同步调用：确保插件（如通知）在进程退出前完成。
            let _ = self.sync_call(p, "onComplete", &payload);
        }
        Ok(())
    }
}
