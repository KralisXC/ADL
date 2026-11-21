//! Hook 管线（DESIGN.md §4.3）。
//!
//! **M1 阶段这些 hook 全部是空实现，但调用点必须留出来。**
//! 这是 DESIGN.md 里唯一一条"现在不做、后面会痛很久"的事：
//! 插件系统是 M3 才做，但引擎代码里没有 hook 调用点的话，M3 就得返工重写引擎主流程。
//!
//! 分两类：
//! - **同步阻塞**：必须等结果才能继续（`onResolve` / `onPlan` / `onBeforeRequest` /
//!   `onResponse` / `onComplete`）。必须带超时，M3 用 QuickJS 的
//!   `set_interrupt_handler` 强制中断（实测 301ms 生效）。
//! - **异步丢弃**：`onProgress`。每 256 KB 都停下来等插件会慢十倍，所以走队列，
//!   队列满直接丢，绝不阻塞下载。

use crate::error::AdlError;
use crate::segment::SegmentTable;
use std::time::Duration;

/// 一个下载候选（`onResolve` 的产物）。
#[derive(Debug, Clone, Default)]
pub struct Candidate {
    pub url: String,
    /// 额外请求头
    pub headers: Vec<(String, String)>,
    pub user_agent: Option<String>,
    pub referer: Option<String>,
}

/// 下载计划（`onPlan` 的产物）。
#[derive(Debug, Clone)]
pub struct DownloadPlan {
    pub total: u64,
    pub segments: SegmentTable,
    pub concurrency: usize,
    pub user_agent: String,
    pub referer: Option<String>,
    pub headers: Vec<(String, String)>,
}

/// `onBeforeRequest` 的判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// 直接拒绝并给出理由
    Deny { reason: String },
    /// 改写这次请求
    Rewrite { headers: Vec<(String, String)> },
}

/// `onResponse` 的判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseVerdict {
    Allow,
    Deny { reason: String },
    /// 重试这一片（例如服务器返回了 HTML 错误页）
    Retry { after: Duration },
}

/// 进度事件（`onProgress` 的入参）。
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub completed: u64,
    pub total: u64,
    pub speed_bps: u64,
    /// 当前活跃的连接数（已派发且未结束的分片）。UI 用它显示"并行线路"。
    pub connections: u32,
}

/// 完成事件（`onComplete` 的入参）。
#[derive(Debug, Clone)]
pub struct Completed {
    pub path: std::path::PathBuf,
    pub total: u64,
    pub sha256: Option<String>,
}

/// 插件 hook 接口。
///
/// M1 只有 `NoopHooks`。M3 加 `QuickJsHooks`，M7 加 `WasmHooks` —— 实现这个 trait 即可，
/// 引擎主流程一行不改。
///
/// 所有方法都是 `&self`（不是 `&mut self`）：hook 可能被多个 worker 并发调用，
/// 内部可变性由实现方自己管（QuickJS 侧靠"每个插件一条专属线程 + 消息通道"）。
pub trait Hooks: Send + Sync {
    /// Transform · 3s。站点适配：改写 URL / headers / cookie。
    fn on_resolve(&self, url: &str) -> Result<Vec<Candidate>, AdlError> {
        Ok(vec![Candidate {
            url: url.to_string(),
            ..Default::default()
        }])
    }

    /// Transform · 3s。决定分片方案。
    fn on_plan(&self, plan: &mut DownloadPlan) -> Result<(), AdlError> {
        let _ = plan;
        Ok(())
    }

    /// Guard | Transform · 3s。每个分片请求发出前调用。
    fn on_before_request(
        &self,
        _seg_index: usize,
        _start: u64,
        _end: u64,
    ) -> Result<Verdict, AdlError> {
        Ok(Verdict::Allow)
    }

    /// Guard · 3s。收到响应头后、写任何字节前调用。
    fn on_response(
        &self,
        _seg_index: usize,
        _status: u16,
        _content_type: Option<&str>,
    ) -> Result<ResponseVerdict, AdlError> {
        Ok(ResponseVerdict::Allow)
    }

    /// Observe · 异步丢弃。**绝不能阻塞**。
    fn on_progress(&self, _p: Progress) {}

    /// Observe · 异步丢弃。某个分片请求失败、即将重试时调用。
    ///
    /// M3 插件可据此做"坏镜像剔除/换源"等策略；M2 只留调用点。
    fn on_retry(&self, _seg_index: usize, _attempt: u8, _error: &str) {}

    /// Observe · 异步丢弃。某个分片因速度过慢被抢占、改派给别的连接时调用。
    fn on_preempt(&self, _seg_index: usize, _preempts: u8) {}

    /// Observe · 异步丢弃。因限速/服务端拒绝触发并发降档时调用。
    ///
    /// `concurrency` 是降档后的并发数。插件可以据此提示用户或调整策略。
    fn on_degrade(&self, _concurrency: usize, _reason: &str) {}

    /// Observe | Transform。下载完成后调用。
    fn on_complete(&self, _c: &Completed) -> Result<(), AdlError> {
        Ok(())
    }
}

/// M1 的空实现：所有 hook 走 trait 默认值。
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopHooks;

impl Hooks for NoopHooks {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_allows_everything() {
        let h = NoopHooks;
        assert_eq!(h.on_resolve("https://x/").unwrap().len(), 1);
        assert_eq!(h.on_before_request(0, 0, 99).unwrap(), Verdict::Allow);
        assert_eq!(h.on_response(0, 206, None).unwrap(), ResponseVerdict::Allow);
        h.on_progress(Progress {
            completed: 1,
            total: 2,
            speed_bps: 3,
            connections: 4,
        });
        assert!(h
            .on_complete(&Completed {
                path: std::path::PathBuf::from("x"),
                total: 1,
                sha256: None,
            })
            .is_ok());
    }

    #[test]
    fn custom_hooks_can_deny() {
        struct Deny;
        impl Hooks for Deny {
            fn on_before_request(&self, _: usize, _: u64, _: u64) -> Result<Verdict, AdlError> {
                Ok(Verdict::Deny {
                    reason: "test".into(),
                })
            }
        }
        let v = Deny.on_before_request(0, 0, 1).unwrap();
        assert!(matches!(v, Verdict::Deny { .. }));
    }

    #[test]
    fn hooks_are_object_safe() {
        // 引擎要持 `Arc<dyn Hooks>`，所以必须 object safe
        let h: std::sync::Arc<dyn Hooks> = std::sync::Arc::new(NoopHooks);
        assert!(h.on_resolve("u").is_ok());
    }
}
