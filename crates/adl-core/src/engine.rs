//! 下载引擎（M2：并发分片 + 限速 + 重试 + 并发降档）。
//!
//! 结构（DESIGN.md §3.4）：
//!
//! - **单写者**：只有一个调度协程能改 `SegmentTable`。worker 通过 channel 上报事件，
//!   事件带 `(id, token)`，token 不匹配的迟到事件一律丢弃。
//! - **worker**：每个 worker 一次性领一段连续 Missing piece（`next_run_limited`），
//!   发一个 Range 请求把它们拉完，边写边按 piece 记账（断点续传的记账点）。
//! - **抢占**：某段 12 秒没有新字节且速度低于 100 KB/s，就发 Stop（`watch`），
//!   worker 在下一个 chunk 边界确认 `Stopped`，调度器再把它退回 Missing 改派。
//!   两阶段（Active→Reclaiming→Missing），绝不直接改派（worker 可能还在写）。
//! - **重试 + 降档**：可重试错误（超时/重置/网络/504）退避重试；403/429 触发
//!   并发降档（8→4→2→1）。
//!
//! **插件预留点**（用户 m01363 的硬约束）：六个 hook 调用点全部就位，M3 换
//! `QuickJsHooks` 即可；并发数、分片大小、限速都通过 `DownloadOptions` /
//! `DownloadPlan` 暴露给插件。

use crate::control::{ControlFile, DEFAULT_PIECE_LENGTH};
use crate::error::{is_disk_full, AdlError, Result};
use crate::hooks::{Completed, DownloadPlan, Hooks, Progress, ResponseVerdict, Verdict};
use crate::http::DEFAULT_UA;
use crate::path;
use crate::probe::{self, Probe};
use crate::segment::SegmentTable;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

/// 进度回调的批量粒度：64 KB 上报一次。
const PROGRESS_BATCH: u64 = 64 * 1024;
/// 读超时：多久没收到任何字节就算超时。
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// 调度器 tick：僵死检测 / 进度刷新 / 控制文件落盘检查。
const SCHED_TICK: Duration = Duration::from_millis(500);
/// 一段多久没有新字节就算僵死，可以抢占。
const STALL_SECS: u64 = 12;
/// 单片最多被抢占几次（防乒乓）。
const MAX_PREEMPTS: u8 = 2;
/// 单片最多重试几次。
const MAX_RETRIES: u8 = 3;
/// 僵死判定的速度下限：低于它才认为"真的慢"（避免高速时误抢）。
const PREEMPT_SPEED_FLOOR: u64 = 100 * 1024;
/// 并发硬上限（用户要求：最高 64）。
const MAX_CONCURRENCY: usize = 64;
/// 默认并发：真实网络 16 连接基本能跑满（用户实测经验）。
const DEFAULT_CONCURRENCY: usize = 16;
/// 限流类失败（403/429）的分片级重试上限；这类主要靠降档解决。
const RATE_MAX_RETRIES: u8 = 12;
/// 无任何进展期间连续失败多少条就放弃（防止死链无限重试）。
const HARD_FAIL_LIMIT: u32 = 40;
/// 连续多久完全没有新字节 → 判定服务器"彻底不发了"（反爬/限速），主动暂停并保存进度。
/// 真实案例：Intel 驱动站 130MB 处触发反爬后停发，要等 40 分钟；此时死等/狂重试只会更糟。
const NO_PROGRESS_PAUSE_SECS: u64 = 60;

/// 探测「能否再多开一条连接」的初始间隔。
const PROBE_BASE: Duration = Duration::from_secs(20);
/// 试探间隔上限：连续试探失败会不停翻倍，最多到这里，避免频繁撞墙触发反爬。
const PROBE_MAX: Duration = Duration::from_secs(300);
/// 一次试探的判定窗口：+1 之后这么久内出现容量失败 → 视为试探失败。
const PROBE_GRACE: Duration = Duration::from_secs(10);

/// 把并发数夹到 `[1, MAX_CONCURRENCY]`（不强制 2 的幂；用户要的是"按服务器给多少跑多少"）。
pub fn clamp_concurrency(n: usize) -> usize {
    n.clamp(1, MAX_CONCURRENCY)
}

/// 自适应并发控制器。
///
/// 用户模型（m01705）：我们想开 `target` 条连接，但服务器经常只放行其中一部分
/// （比如 16 里只接受 9）。于是：
/// - 遇到容量类失败（连不上 / 403 / 429），把实际并发降 1，直到收敛到服务器真正
///   能接受的量（16 里被拒 7 个 → 收敛到 9）；
/// - 稳定运行 `probe_interval` 后，试探性 +1（9→10），看服务器能不能接受；接受
///   就保持并继续慢慢往上试，不接受就退回并把这一轮间隔翻倍；
/// - **绝不一直顶着 `target` 猛冲**——试探间隔会指数增长到最多 5 分钟，
///   既省无效连接，也避免触发服务器反爬。
struct ConcController {
    /// 上限（用户期望的最大并发，来自 `-x` / 插件）。
    target: usize,
    /// 当前实际允许同时开几条。
    effective: usize,
    /// 当前试探间隔（失败翻倍、成功回调）。
    probe_interval: Duration,
    /// 最近一次容量失败的时刻（用来判断"稳定了多久"）。
    last_capacity: Instant,
    /// 若正在试探更高并发，这里是 +1 的时刻。
    probing_since: Option<Instant>,
}

impl ConcController {
    fn new(target: usize) -> Self {
        let target = clamp_concurrency(target);
        Self {
            target,
            effective: target,
            probe_interval: PROBE_BASE,
            last_capacity: Instant::now(),
            probing_since: None,
        }
    }

    /// 处理一次容量类失败。`ok_count` = 当前真正跑通（已收到字节）的连接数。
    /// 返回变化后的 effective（若变化，调用方用它触发 `on_degrade`）。
    fn on_capacity_failure(&mut self, ok_count: usize) -> Option<usize> {
        let now = Instant::now();
        let old = self.effective;
        // 试探期失败 → 这轮试探失败，间隔翻倍（最多 PROBE_MAX）。
        if let Some(t) = self.probing_since.take() {
            if now.duration_since(t) <= PROBE_GRACE {
                self.probe_interval = (self.probe_interval * 2).min(PROBE_MAX);
            }
        }
        self.last_capacity = now;
        // 降 1，但不低于"真正跑通的数量"（避免压到比服务器允许的还低）。
        let floor = ok_count.max(1);
        let mut ne = old.saturating_sub(1).max(floor);
        if ne > old {
            ne = old;
        }
        self.effective = ne;
        if self.effective != old {
            Some(self.effective)
        } else {
            None
        }
    }

    /// 每 tick 调用：稳定够久就试探 +1；顺带判定上一个试探是否成功。
    fn maybe_probe(&mut self) {
        let now = Instant::now();
        // 上一个试探已撑过判定窗口 → 视为成功：间隔回调一半。
        if let Some(t) = self.probing_since {
            if now.duration_since(t) > PROBE_GRACE {
                self.probing_since = None;
                if self.probe_interval > PROBE_BASE {
                    self.probe_interval = (self.probe_interval / 2).max(PROBE_BASE);
                }
            }
        }
        if self.effective < self.target
            && self.probing_since.is_none()
            && now.duration_since(self.last_capacity) >= self.probe_interval
        {
            self.effective += 1;
            self.probing_since = Some(now);
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub url: String,
    /// 目标目录
    pub dir: PathBuf,
    /// 显式指定文件名（`--out`）
    pub out_name: Option<String>,
    pub user_agent: String,
    /// 额外请求头
    pub headers: Vec<(String, String)>,
    pub referer: Option<String>,
    /// 续传用的 piece_length，必须与旧控制文件一致
    pub piece_length: u64,
    /// 不续传，从头下
    pub restart: bool,
    /// 并发连接数上限（插件可通过 `onPlan` 再改）
    pub concurrency: usize,
    /// 单个请求最多拉多少字节（决定"一段"多大；小于 piece_length 时按 piece 记）
    pub chunk_size: u64,
    /// 全局限速（字节/秒），0 = 不限
    pub speed_limit: u64,
    /// 强制 HTTP/1.1（少数站点 h2 不稳，用户 m01907 提到过）
    pub http1_only: bool,
    /// HTTP 代理，如 `http://127.0.0.1:7890`
    pub proxy: Option<String>,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            url: String::new(),
            dir: PathBuf::from("."),
            out_name: None,
            user_agent: DEFAULT_UA.to_string(),
            headers: Vec::new(),
            referer: None,
            piece_length: DEFAULT_PIECE_LENGTH,
            restart: false,
            concurrency: DEFAULT_CONCURRENCY,
            chunk_size: 0, // 0 = 按文件大小自动分配分块
            speed_limit: 0,
            http1_only: false,
            proxy: None,
        }
    }
}

/// 下载结果。
#[derive(Debug, Clone)]
pub struct DownloadResult {
    pub path: PathBuf,
    pub total: u64,
    pub sha256: String,
    pub elapsed: Duration,
    /// 本次是否用了续传
    pub resumed: bool,
    /// 续传时已经有的字节数
    pub resumed_from: u64,
    /// 服务器通过 Digest 头声明的 SHA256（若有，已核对一致才会成功返回）
    pub server_sha256: Option<String>,
    /// 服务器通过 Content-MD5 / Digest 头声明的 MD5（若有，已核对一致才会成功返回）
    pub server_md5: Option<String>,
}

impl DownloadResult {
    pub fn speed_bps(&self) -> u64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 {
            return 0;
        }
        (self.total as f64 / secs) as u64
    }
}

/// 进度上报闭包。引擎只认这个 trait 对象，不认具体 UI。
pub type ProgressFn = Arc<dyn Fn(Progress) + Send + Sync>;

// ═══════════════════════════════════════════════════════════════════════
// worker ↔ 调度器 的事件
// ═══════════════════════════════════════════════════════════════════════

/// 一个 worker 领到的任务：一段连续 piece + 对应的字节区间 + Stop 信号。
struct WorkerJob {
    id: u64,
    token: u64,
    start_idx: usize,
    byte_start: u64,
    byte_end: u64,
    stop: watch::Receiver<bool>,
}

/// worker 上报给调度器的事件。
enum WorkerEvent {
    Started { id: u64, token: u64 },
    Progress { id: u64, token: u64, written: u64 },
    Done { id: u64, token: u64 },
    Stopped { id: u64, token: u64, written: u64 },
    Failed { id: u64, token: u64, error: AdlError },
}

/// worker 内部的终止原因。
enum WorkerAbort {
    Stopped { written: u64 },
    Failed(AdlError),
}

/// worker 共享的上下文（只读，Arc 共享）。
struct WorkerCtx {
    client: reqwest::Client,
    url: String,
    target: PathBuf,
    headers: Vec<(String, String)>,
    referer: Option<String>,
    hooks: Arc<dyn Hooks>,
    tx: mpsc::UnboundedSender<WorkerEvent>,
    speed: Arc<SpeedTracker>,
    limiter: Option<Arc<RateLimiter>>,
    expect_etag: Option<String>,
    expect_lm: Option<String>,
    resumed: bool,
}

/// 调度器里一个在跑的段。
struct ActiveRun {
    id: u64,
    start: usize,
    count: usize,
    token: u64,
    worker: u32,
    stop_tx: watch::Sender<bool>,
    started: Instant,
    last_written: u64,
    last_change: Instant,
    preempts: u8,
}

// ═══════════════════════════════════════════════════════════════════════
// 主流程
// ═══════════════════════════════════════════════════════════════════════

/// 下载主流程。
pub async fn download(
    mut opts: DownloadOptions,
    hooks: Arc<dyn Hooks>,
    on_progress: Option<ProgressFn>,
) -> Result<DownloadResult> {
    let started = Instant::now();

    // ── 1. onResolve：站点适配 ────────────────────────────────────────
    let candidates = hooks.on_resolve(&opts.url)?;
    let candidate = candidates
        .into_iter()
        .next()
        .ok_or_else(|| AdlError::Unknown("onResolve 没有返回任何候选".into()))?;
    let url = candidate.url;
    // 插件在 onResolve 里给出的 UA/Referer/headers 也要生效（M3）：
    // 必须在探测之前合并，否则需要特定 UA 才能访问的站点连探测都过不去。
    if let Some(ua) = candidate.user_agent {
        if !ua.is_empty() {
            opts.user_agent = ua;
        }
    }
    if let Some(rf) = candidate.referer {
        if !rf.is_empty() {
            opts.referer = Some(rf);
        }
    }
    for (k, v) in candidate.headers {
        opts.headers.retain(|(hk, _)| !hk.eq_ignore_ascii_case(&k));
        opts.headers.push((k, v));
    }

    std::fs::create_dir_all(&opts.dir)?;

    // ── 2. 先看有没有控制文件 ────────────────────────────────────────
    let mut resumed_from = 0u64;
    let mut resumed = false;

    let guess_name = opts
        .out_name
        .clone()
        .or_else(|| url::Url::parse(&url).ok().and_then(|u| path::name_from_url_path(&u)));
    let guess_path = match &guess_name {
        Some(n) => Some(opts.dir.join(path::sanitize_name(n))),
        None => None,
    };

    let mut existing: Option<(PathBuf, ControlFile)> = None;
    if !opts.restart {
        if let Some(p) = &guess_path {
            if let Some(cf) = ControlFile::load(p)? {
                existing = Some((p.clone(), cf));
            }
        }
        // 名字可能来自 Content-Disposition / 服务器 302，和 URL 猜名不一致；
        // 再按 URL 扫一遍目录里的 .adl，保证跨进程续传不因改名而丢失（用户 m01907）。
        if existing.is_none() {
            existing = find_control_by_url(&opts.dir, &url)?;
        }
    }

    // ── 3. 探测（或复用控制文件的总长）──────────────────────────────
    let client = crate::http::build_client(&opts.user_agent, opts.http1_only, opts.proxy.as_deref())?;

    let (_probe, meta, total, target_path, mut table) = match &existing {
        Some((p, cf)) => {
            let table = cf.resume_table(cf.total, opts.piece_length)?;
            resumed_from = table.completed();
            resumed = resumed_from > 0;
            (
                Probe::Ranged { total: cf.total },
                probe::ProbeMeta {
                    etag: cf.etag.clone(),
                    last_modified: cf.last_modified.clone(),
                    final_url: cf.url.clone(),
                    ..Default::default()
                },
                cf.total,
                p.clone(),
                table,
            )
        }
        None => {
            let (probe, meta) = probe::probe(&client, &url).await?;
            let total = match probe.total() {
                Some(t) => t,
                None => {
                    return download_unknown_length(
                        &client, &url, &opts, &meta, hooks, on_progress, started,
                    )
                    .await;
                }
            };

            let name = opts
                .out_name
                .clone()
                .or_else(|| {
                    meta.content_disposition
                        .as_deref()
                        .and_then(path::filename_from_content_disposition)
                })
                .or_else(|| path::name_from_url_path(&url::Url::parse(&url).ok()?));

            let target = match name {
                Some(n) => opts.dir.join(path::sanitize_name(&n)),
                None => opts.dir.join(path::fallback_timestamp_name()),
            };
            // 重名不覆盖：自动改名成 `name (1).ext`；`--restart` 时保留原名并覆盖。
            let target = if opts.restart {
                target
            } else {
                path::unique_path(&target)
            };

            let table = if probe.supports_ranges() {
                let pieces = (total.div_ceil(opts.piece_length)).max(1) as usize;
                SegmentTable::split(total, pieces)
            } else {
                SegmentTable::single(total)
            };

            (probe, meta, total, target, table)
        }
    };

    // 校验落盘路径不许逃出下载目录（DESIGN.md §3.8）
    if !path::is_within(&opts.dir, &target_path) {
        return Err(AdlError::Unknown(format!(
            "目标路径逃出了下载目录: {} 不在 {} 之内",
            target_path.display(),
            opts.dir.display()
        )));
    }

    // 空文件：没有分片可下，直接建空文件收尾。
    if total == 0 {
        let target_path = path::to_long_path(&target_path);
        std::fs::File::create(&target_path)?;
        let sha256 = hash_file(&target_path)?;
        ControlFile::remove(&target_path);
        let result = DownloadResult {
            path: target_path,
            total: 0,
            sha256,
            elapsed: started.elapsed(),
            resumed: false,
            resumed_from: 0,
            server_sha256: None,
            server_md5: None,
        };
        hooks.on_complete(&Completed {
            path: result.path.clone(),
            total: 0,
            sha256: Some(result.sha256.clone()),
        })?;
        return Ok(result);
    }

    // ── 4. onPlan：决定分片方案（插件可改并发数 / 分片表）───────────
    let mut plan = DownloadPlan {
        total,
        segments: table.clone(),
        concurrency: clamp_concurrency(opts.concurrency),
        user_agent: opts.user_agent.clone(),
        referer: opts.referer.clone().or_else(|| Some(meta.final_url.clone())),
        headers: opts.headers.clone(),
    };
    hooks.on_plan(&mut plan)?;
    table = plan.segments;
    let total = plan.total;

    // ── 5. 预分配（只用 trunc）──────────────────────────────────────
    let target_path = path::to_long_path(&target_path);
    {
        let f = open_target_rw(&target_path).await?;
        f.set_len(total).map_err(|e| {
            if is_disk_full(&e) {
                AdlError::DiskFull
            } else {
                AdlError::Io(e)
            }
        })?;
    }

    // ── 6. 落控制文件（先落一次，崩溃后能续）────────────────────────
    let mut cf = ControlFile::new(&url, total, &table, opts.piece_length);
    cf.etag = meta.etag.clone();
    cf.last_modified = meta.last_modified.clone();
    cf.save(&target_path)?;

    // ── 7. 并发调度 ─────────────────────────────────────────────────
    let speed = Arc::new(SpeedTracker::new());
    let limiter = if opts.speed_limit > 0 {
        Some(Arc::new(RateLimiter::new(opts.speed_limit)))
    } else {
        None
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<WorkerEvent>();
    let ctx = Arc::new(WorkerCtx {
        client: client.clone(),
        url: url.clone(),
        target: target_path.clone(),
        headers: opts.headers.clone(),
        referer: plan.referer.clone(),
        hooks: hooks.clone(),
        tx: tx.clone(),
        speed: speed.clone(),
        limiter: limiter.clone(),
        expect_etag: meta.etag.clone(),
        expect_lm: meta.last_modified.clone(),
        resumed,
    });

    // 一段最多拉几个 piece。`chunk_size == 0`（CLI 默认）表示按文件大小自动分配：
    // 目标让每个并发连接能拿到若干段、且小文件也能吃满并发。
    let chunk_pieces = if opts.chunk_size == 0 {
        let total_pieces = table.segments.len().max(1);
        let target = plan.concurrency.max(1) * 4;
        (total_pieces / target).clamp(1, 8)
    } else {
        ((opts.chunk_size + opts.piece_length - 1) / opts.piece_length).max(1) as usize
    };

    let mut ctrl = ConcController::new(plan.concurrency);

    let mut active: HashMap<u64, ActiveRun> = HashMap::new();
    let mut next_id: u64 = 1;
    let mut last_save = Instant::now();
    let mut retry_after: Option<Instant> = None;
    // 无进展期间连续失败（成功即清零，用来兜底死链）。
    let mut hard_failures: u32 = 0;
    // 最近一次"拿到新字节"的时间，用来判断"服务器彻底不发了"（反爬/限速）→ 主动暂停。
    let mut last_any_progress = Instant::now();
    let mut fatal: Option<AdlError> = None;

    let mut ticker = tokio::time::interval(SCHED_TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        if table.is_done() {
            break;
        }

        // ── 派发：有空位就发新段 ────────────────────────────────────
        while active.len() < ctrl.effective {
            if let Some(ra) = retry_after {
                if Instant::now() < ra {
                    break;
                }
                retry_after = None;
            }
            let Some((rs, rc)) = table.next_run_limited(chunk_pieces) else {
                break;
            };
            let token = next_token();
            let id = next_id;
            next_id += 1;
            let worker = id as u32;
            if !table.assign_run(rs, rc, worker, token) {
                break;
            }
            let byte_start = table.segments[rs].start;
            let byte_end = table.segments[rs + rc - 1].end;
            let preempts = table.preempts_at(rs);
            let (stop_tx, stop_rx) = watch::channel(false);
            active.insert(
                id,
                ActiveRun {
                    id,
                    start: rs,
                    count: rc,
                    token,
                    worker,
                    stop_tx,
                    started: Instant::now(),
                    last_written: 0,
                    last_change: Instant::now(),
                    preempts,
                },
            );
            tokio::spawn(run_worker(
                WorkerJob {
                    id,
                    token,
                    start_idx: rs,
                    byte_start,
                    byte_end,
                    stop: stop_rx,
                },
                ctx.clone(),
            ));
        }

        // ── 等事件 或 tick ─────────────────────────────────────────
        tokio::select! {
            maybe = rx.recv() => {
                let Some(ev) = maybe else { break; };
                match ev {
                    WorkerEvent::Started { id, token } => {
                        // worker 真正开始跑了：重置僵死时钟（从发请求那一刻计时）
                        if let Some(r) = active.get_mut(&id) {
                            if r.token == token {
                                r.started = Instant::now();
                                r.last_change = Instant::now();
                            }
                        }
                    }

                    WorkerEvent::Progress { id, token, written } => {
                        let Some(r) = active.get_mut(&id) else { continue };
                        if r.token != token { continue; }
                        let newly = table.report_run_progress(r.start, r.count, token, written);
                        let now = Instant::now();
                        if written > r.last_written {
                            r.last_written = written;
                            r.last_change = now;
                            // 任何新字节到账都算"服务器还在发数据"，避免慢速但正常的下载被误判反爬。
                            last_any_progress = now;
                        }
                        hard_failures = 0;
                        emit_progress(&table, total, &speed, active.len(), &hooks, &on_progress);
                        if newly > 0 || last_save.elapsed() >= Duration::from_secs(2) {
                            cf.update_from(&table);
                            let _ = cf.save(&target_path);
                            last_save = Instant::now();
                        }
                    }

                    WorkerEvent::Done { id, token } => {
                        let Some(r) = active.remove(&id) else { continue };
                        if r.token != token { continue; }
                        let run_bytes: u64 = table.segments[r.start..r.start + r.count]
                            .iter()
                            .map(|s| s.len())
                            .sum();
                        table.report_run_progress(r.start, r.count, token, run_bytes);
                        if table.finish_run(r.start, r.count, token) {
                            fatal = Some(AdlError::Unknown("分片完成但记账不一致".into()));
                            break;
                        }
                        hard_failures = 0;
                        last_any_progress = Instant::now();
                        cf.update_from(&table);
                        let _ = cf.save(&target_path);
                        last_save = Instant::now();
                        emit_progress(&table, total, &speed, active.len(), &hooks, &on_progress);
                    }

                    WorkerEvent::Stopped { id, token, written } => {
                        let Some(r) = active.remove(&id) else { continue };
                        if r.token != token { continue; }
                        table.report_run_progress(r.start, r.count, token, written);
                        table.finish_reclaim_run(r.start, r.count, token);
                        cf.update_from(&table);
                        let _ = cf.save(&target_path);
                        last_save = Instant::now();
                        emit_progress(&table, total, &speed, active.len(), &hooks, &on_progress);
                    }

                    WorkerEvent::Failed { id, token, error } => {
                        let Some(r) = active.remove(&id) else { continue };
                        if r.token != token { continue; }

                        let connect_level = error.is_connect_level();
                        let rate_limited = error.is_rate_limited();
                        let retryable = error.is_retryable();
                        if !(connect_level || rate_limited || retryable) {
                            fatal = Some(error);
                            break;
                        }
                        let reason = error.to_string();
                        hard_failures = hard_failures.saturating_add(1);

                        // 连接级失败不惩罚具体分片（多半是并发限额），直接回队；限流类仍计数，
                        // 但用更高的上限，因为它也可能真的是"这个 URL 不让下"。
                        let mut attempt: u8 = 0;
                        if connect_level {
                            table.finish_run(r.start, r.count, token);
                        } else {
                            attempt = table.bump_attempts_run(r.start, r.count, token);
                            table.finish_run(r.start, r.count, token);
                            let cap = if rate_limited { RATE_MAX_RETRIES } else { MAX_RETRIES };
                            if attempt > cap {
                                fatal = Some(error);
                                break;
                            }
                            hooks.on_retry(r.start, attempt, &reason);
                        }

                        // 容量类（连不上 / 403 / 429）：自适应降并发，收敛到服务端
                        // 真正能接受的量；稳定后再慢慢试探 +1。
                        if connect_level || rate_limited {
                            let ok_count = active.values().filter(|r| r.last_written > 0).count();
                            if let Some(new_conc) = ctrl.on_capacity_failure(ok_count) {
                                hooks.on_degrade(new_conc, &reason);
                            }
                        }

                        if hard_failures > HARD_FAIL_LIMIT {
                            fatal = Some(error);
                            break;
                        }
                        // 连接级失败退避更短，让它快速再试；数据类错误按分片重试次数退避。
                        retry_after = Some(
                            Instant::now()
                                + if connect_level {
                                    backoff_connect(hard_failures)
                                } else {
                                    backoff_for(attempt.max(1))
                                },
                        );
                        cf.update_from(&table);
                        let _ = cf.save(&target_path);
                        last_save = Instant::now();
                    }
                }
            }

            _ = ticker.tick() => {
                let now = Instant::now();
                // ── 自适应并发：稳定够久就试探 +1 ──────────────────
                ctrl.maybe_probe();
                // ── 服务器长时间完全不发数据 → 主动暂停（反爬/限速）──
                if now.duration_since(last_any_progress) > Duration::from_secs(NO_PROGRESS_PAUSE_SECS) {
                    cf.update_from(&table);
                    let _ = cf.save(&target_path);
                    fatal = Some(AdlError::Stalled);
                    break;
                }
                // ── 僵死检测：慢段抢占 ─────────────────────────────
                let mut to_preempt: Vec<(u64, usize, usize, u32, u64, u8)> = Vec::new();
                for r in active.values() {
                    if r.preempts >= MAX_PREEMPTS {
                        continue;
                    }
                    if now.duration_since(r.last_change) < Duration::from_secs(STALL_SECS) {
                        continue;
                    }
                    let elapsed = r.started.elapsed().as_secs_f64().max(0.001);
                    let bps = (r.last_written as f64 / elapsed) as u64;
                    if bps < PREEMPT_SPEED_FLOOR {
                        to_preempt.push((r.id, r.start, r.count, r.worker, r.token, r.preempts));
                    }
                }
                for (id, start, count, worker, token, preempts) in to_preempt {
                    if table.begin_reclaim_run(start, count, worker, token) {
                        if let Some(r) = active.get(&id) {
                            let _ = r.stop_tx.send(true);
                        }
                        hooks.on_preempt(start, preempts.saturating_add(1));
                    }
                }

                // 孤儿 Reclaiming 兜底：没有任何 worker 却还有 Reclaiming
                if active.is_empty() && !table.is_done() && table.next_missing().is_none() {
                    if table.reset_reclaiming() == 0 {
                        fatal = Some(AdlError::Unknown("无可派发分片但下载未完成".into()));
                        break;
                    }
                }

                emit_progress(&table, total, &speed, active.len(), &hooks, &on_progress);
                if last_save.elapsed() >= Duration::from_secs(2) {
                    cf.update_from(&table);
                    let _ = cf.save(&target_path);
                    last_save = Instant::now();
                }
            }
        }
    }

    // ── 8. 收尾 ─────────────────────────────────────────────────────
    if let Some(e) = fatal {
        for r in active.values() {
            let _ = r.stop_tx.send(true);
        }
        // 一点数据都没下到（例如插件拒绝了响应、服务器返回 HTML 错误页）时，
        // 预分配出来的空文件和控制文件都是垃圾，直接清掉，避免看起来像"下了一半"。
        if table.completed() == 0 {
            let _ = std::fs::remove_file(&target_path);
            ControlFile::remove(&target_path);
        }
        return Err(e);
    }
    if !table.is_done() {
        return Err(AdlError::Unknown("下载循环提前结束但未完成".into()));
    }

    let sha256 = hash_file(&target_path)?;
    // 服务器若通过 Digest 头声明了 SHA256 就核对（用户 m02040：带就校验，不带不验）。
    if let Some(expect) = &meta.digest_sha256 {
        if !expect.eq_ignore_ascii_case(&sha256) {
            // 校验不过 = 数据坏了，删掉文件和进度，避免下次"续传"一个坏文件。
            let _ = std::fs::remove_file(&target_path);
            ControlFile::remove(&target_path);
            return Err(AdlError::ChecksumMismatch {
                expected: expect.clone(),
                actual: sha256,
            });
        }
    }
    // 服务器若声明了 MD5（部分资源站/盗版游戏校验用）也核对。
    if let Some(expect) = &meta.content_md5 {
        let actual = crate::md5::md5_file(&target_path)?;
        if !expect.eq_ignore_ascii_case(&actual) {
            let _ = std::fs::remove_file(&target_path);
            ControlFile::remove(&target_path);
            return Err(AdlError::ChecksumMismatch {
                expected: expect.clone(),
                actual,
            });
        }
    }

    let mut cf_final = cf.clone();
    cf_final.update_from(&table);
    cf_final.sha256 = Some(sha256.clone());
    let _ = cf_final.save(&target_path);
    // 成功才删控制文件
    ControlFile::remove(&target_path);

    let result = DownloadResult {
        path: target_path,
        total,
        sha256,
        elapsed: started.elapsed(),
        resumed,
        resumed_from,
        server_sha256: meta.digest_sha256.clone(),
        server_md5: meta.content_md5.clone(),
    };

    // ── 9. onComplete ───────────────────────────────────────────────
    hooks.on_complete(&Completed {
        path: result.path.clone(),
        total,
        sha256: Some(result.sha256.clone()),
    })?;

    Ok(result)
}

// ═══════════════════════════════════════════════════════════════════════
// worker 实现
// ═══════════════════════════════════════════════════════════════════════

async fn run_worker(job: WorkerJob, ctx: Arc<WorkerCtx>) {
    let id = job.id;
    let token = job.token;
    let _ = ctx.tx.send(WorkerEvent::Started { id, token });
    match do_run(&job, &ctx).await {
        Ok(()) => {
            let _ = ctx.tx.send(WorkerEvent::Done { id, token });
        }
        Err(WorkerAbort::Stopped { written }) => {
            let _ = ctx.tx.send(WorkerEvent::Stopped { id, token, written });
        }
        Err(WorkerAbort::Failed(e)) => {
            let _ = ctx.tx.send(WorkerEvent::Failed {
                id,
                token,
                error: e,
            });
        }
    }
}

async fn do_run(job: &WorkerJob, ctx: &WorkerCtx) -> std::result::Result<(), WorkerAbort> {
    let mut stop = job.stop.clone();

    // ── onBeforeRequest（Guard | Transform）─────────────────────────
    let extra = match ctx
        .hooks
        .on_before_request(job.start_idx, job.byte_start, job.byte_end)
    {
        Ok(Verdict::Allow) => Vec::new(),
        Ok(Verdict::Deny { reason }) => {
            return Err(WorkerAbort::Failed(AdlError::Unknown(format!(
                "插件拒绝请求: {reason}"
            ))))
        }
        Ok(Verdict::Rewrite { headers }) => headers,
        Err(e) => return Err(WorkerAbort::Failed(e)),
    };

    let mut req = ctx
        .client
        .get(&ctx.url)
        .header("Range", format!("bytes={}-{}", job.byte_start, job.byte_end))
        .header("Accept-Encoding", "identity")
        .timeout(READ_TIMEOUT);
    for (k, v) in ctx.headers.iter().chain(extra.iter()) {
        req = req.header(k.as_str(), v.as_str());
    }
    if let Some(r) = &ctx.referer {
        if !r.is_empty() {
            req = req.header("Referer", r.as_str());
        }
    }

    // 发请求（可被 Stop 打断）
    let resp = tokio::select! {
        biased;
        _ = stop.changed() => return Err(WorkerAbort::Stopped { written: 0 }),
        r = req.send() => match r {
            Ok(resp) => resp,
            Err(e) => return Err(WorkerAbort::Failed(map_reqwest(e))),
        },
    };

    let status = resp.status().as_u16();

    // ── 关键校验：服务器忽略 Range 就准备写坏文件 ───────────────────
    if status == 200 && job.byte_start != 0 {
        return Err(WorkerAbort::Failed(AdlError::RangeIgnored {
            expected: job.byte_start,
        }));
    }
    if status == 206 {
        let cr = resp
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        match probe::parse_content_range(cr.as_deref().unwrap_or("")) {
            Some((start, _)) if start == job.byte_start => {}
            other => {
                return Err(WorkerAbort::Failed(AdlError::ContentRangeMismatch(
                    format!(
                        "期望从 {} 开始，实际 Content-Range={cr:?}（解析={other:?}）",
                        job.byte_start
                    ),
                )))
            }
        }

        // 续传一致性校验（代替 If-Range，见 M1 实测）
        if job.byte_start > 0 || ctx.resumed {
            let now_etag = resp
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let now_lm = resp
                .headers()
                .get("last-modified")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let changed = match (&ctx.expect_etag, &now_etag) {
                (Some(old), Some(new)) => old != new,
                (None, _) => match (&ctx.expect_lm, &now_lm) {
                    (Some(old), Some(new)) => old != new,
                    _ => false,
                },
                (Some(_), None) => false,
            };
            if changed {
                return Err(WorkerAbort::Failed(AdlError::RangeIgnored {
                    expected: job.byte_start,
                }));
            }
        }
    } else if status != 200 {
        return Err(WorkerAbort::Failed(AdlError::from_status(status)));
    }

    // ── onResponse（Guard）──────────────────────────────────────────
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    match ctx.hooks.on_response(job.start_idx, status, ct.as_deref()) {
        Ok(ResponseVerdict::Allow) => {}
        Ok(ResponseVerdict::Deny { reason }) => {
            return Err(WorkerAbort::Failed(AdlError::Unknown(format!(
                "插件拒绝响应: {reason}"
            ))))
        }
        Ok(ResponseVerdict::Retry { after }) => {
            tokio::time::sleep(after).await;
            return Err(WorkerAbort::Failed(AdlError::ServerBusy(503)));
        }
        Err(e) => return Err(WorkerAbort::Failed(e)),
    }

    // ── 写盘 ────────────────────────────────────────────────────────
    let mut file = match open_target_rw(&ctx.target).await {
        Ok(f) => f,
        Err(e) => return Err(WorkerAbort::Failed(e)),
    };
    if let Err(e) = file.seek(SeekFrom::Start(job.byte_start)) {
        return Err(WorkerAbort::Failed(AdlError::Io(e)));
    }

    let mut written: u64 = 0;
    let mut last_report: u64 = 0;
    let mut stopped = false;
    let mut stream = resp.bytes_stream();

    loop {
        tokio::select! {
            biased;
            res = stop.changed() => {
                if res.is_err() || *stop.borrow() {
                    stopped = true;
                    break;
                }
            }
            chunk = stream.next() => {
                match chunk {
                    None => break,
                    Some(Ok(bytes)) => {
                        if let Err(e) = file.write_all(&bytes) {
                            return Err(WorkerAbort::Failed(if is_disk_full(&e) {
                                AdlError::DiskFull
                            } else {
                                AdlError::Io(e)
                            }));
                        }
                        written += bytes.len() as u64;
                        ctx.speed.add(bytes.len() as u64);
                        if let Some(lim) = &ctx.limiter {
                            lim.consume(bytes.len() as u64).await;
                        }
                        if written - last_report >= PROGRESS_BATCH {
                            last_report = written;
                            let _ = ctx.tx.send(WorkerEvent::Progress {
                                id: job.id,
                                token: job.token,
                                written,
                            });
                        }
                    }
                    Some(Err(e)) => return Err(WorkerAbort::Failed(map_reqwest(e))),
                }
            }
        }
    }

    let _ = file.flush();
    drop(file);

    if stopped {
        return Err(WorkerAbort::Stopped { written });
    }

    let expected = job.byte_end - job.byte_start + 1;
    if written != expected {
        return Err(WorkerAbort::Failed(AdlError::ContentRangeMismatch(
            format!(
                "区间 [{}, {}] 应写 {expected} 字节，实际 {written}",
                job.byte_start, job.byte_end
            ),
        )));
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════
// 未知长度（chunked / close-delimited）：只能单流顺序写
// ═══════════════════════════════════════════════════════════════════════

async fn download_unknown_length(
    client: &reqwest::Client,
    url: &str,
    opts: &DownloadOptions,
    meta: &probe::ProbeMeta,
    hooks: Arc<dyn Hooks>,
    on_progress: Option<ProgressFn>,
    started: Instant,
) -> Result<DownloadResult> {
    let name = opts
        .out_name
        .clone()
        .or_else(|| {
            meta.content_disposition
                .as_deref()
                .and_then(path::filename_from_content_disposition)
        })
        .or_else(|| {
            url::Url::parse(url)
                .ok()
                .and_then(|u| path::name_from_url_path(&u))
        })
        .unwrap_or_else(path::fallback_timestamp_name);

    let target = path::to_long_path(&opts.dir.join(path::sanitize_name(&name)));
    let mut file = std::fs::File::create(&target)?;

    let mut req = client
        .get(url)
        .header("Accept-Encoding", "identity")
        .timeout(READ_TIMEOUT);
    for (k, v) in &opts.headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let resp = req.send().await.map_err(map_reqwest)?;
    if !resp.status().is_success() {
        return Err(AdlError::from_status(resp.status().as_u16()));
    }

    let speed_tracker = Arc::new(SpeedTracker::new());
    let limiter = if opts.speed_limit > 0 {
        Some(RateLimiter::new(opts.speed_limit))
    } else {
        None
    };
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    let mut last_report = 0u64;
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(map_reqwest)?;
        file.write_all(&chunk)?;
        hasher.update(&chunk);
        total += chunk.len() as u64;
        speed_tracker.add(chunk.len() as u64);
        if let Some(lim) = &limiter {
            lim.consume(chunk.len() as u64).await;
        }

        if total - last_report >= PROGRESS_BATCH {
            last_report = total;
            let p = Progress {
                completed: total,
                total: 0,
                speed_bps: speed_tracker.bps(),
                connections: 1,
            };
            hooks.on_progress(p);
            if let Some(cb) = &on_progress {
                cb(p);
            }
        }
    }
    file.flush()?;
    drop(file);

    let sha256 = hex::encode(hasher.finalize());
    if let Some(expect) = &meta.digest_sha256 {
        if !expect.eq_ignore_ascii_case(&sha256) {
            let _ = std::fs::remove_file(&target);
            return Err(AdlError::ChecksumMismatch {
                expected: expect.clone(),
                actual: sha256,
            });
        }
    }
    if let Some(expect) = &meta.content_md5 {
        let actual = crate::md5::md5_file(&target)?;
        if !expect.eq_ignore_ascii_case(&actual) {
            let _ = std::fs::remove_file(&target);
            return Err(AdlError::ChecksumMismatch {
                expected: expect.clone(),
                actual,
            });
        }
    }
    let result = DownloadResult {
        path: target,
        total,
        sha256,
        elapsed: started.elapsed(),
        resumed: false,
        resumed_from: 0,
        server_sha256: meta.digest_sha256.clone(),
        server_md5: meta.content_md5.clone(),
    };

    hooks.on_complete(&Completed {
        path: result.path.clone(),
        total,
        sha256: Some(result.sha256.clone()),
    })?;

    Ok(result)
}

// ═══════════════════════════════════════════════════════════════════════
// 工具
// ═══════════════════════════════════════════════════════════════════════

fn emit_progress(
    table: &SegmentTable,
    total: u64,
    speed: &SpeedTracker,
    conns: usize,
    hooks: &Arc<dyn Hooks>,
    cb: &Option<ProgressFn>,
) {
    let p = Progress {
        completed: table.completed(),
        total,
        speed_bps: speed.bps(),
        connections: conns as u32,
    };
    hooks.on_progress(p);
    if let Some(f) = cb {
        f(p);
    }
}

/// 退避：400ms * 2^(attempt-1)，封顶 8s，加 0~250ms 抖动。
fn backoff_for(attempt: u8) -> Duration {
    let base = 400u64;
    let exp = 1u64 << (attempt.min(4) as u32);
    let ms = (base * exp).min(8_000);
    let jitter = next_token() % 251;
    Duration::from_millis(ms + jitter)
}

/// 连接级失败的退避：比数据错误更短（300ms 起、封顶 2s），
/// 让被拒绝的连接快速再试，但不至于把服务端打爆。
fn backoff_connect(n: u32) -> Duration {
    let ms = (300u64 << n.min(3)).min(2_000);
    let jitter = next_token() % 200;
    Duration::from_millis(ms + jitter)
}

/// 杀软/索引器/资源管理器临时占用文件（Windows `ERROR_SHARING_VIOLATION`=32 / `ERROR_LOCK_VIOLATION`=33）。
fn is_lock_error(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(32) | Some(33))
}

/// 打开目标文件（可创建、读写、不截断）。被短暂占用时重试几次（用户 m01907 遇到过）。
async fn open_target_rw(path: &Path) -> Result<std::fs::File> {
    let mut last: Option<std::io::Error> = None;
    for i in 0..4u32 {
        match std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
        {
            Ok(f) => return Ok(f),
            Err(e) => {
                if is_lock_error(&e) && i < 3 {
                    last = Some(e);
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
                return Err(AdlError::Io(e));
            }
        }
    }
    Err(AdlError::Io(
        last.unwrap_or_else(|| std::io::Error::other("打开文件失败")),
    ))
}

/// 在下载目录里按 URL 找匹配的控制文件（落盘名可能与 URL/--out 猜出的名字不同，
/// 例如名字来自 Content-Disposition 或 302 后的地址）。保证跨进程续传不因改名丢失。
fn find_control_by_url(dir: &Path, url: &str) -> Result<Option<(PathBuf, ControlFile)>> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(None),
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some(crate::control::CONTROL_EXT) {
            continue;
        }
        let data = p.with_extension("");
        if let Some(cf) = ControlFile::load(&data)? {
            if cf.url == url {
                return Ok(Some((data, cf)));
            }
        }
    }
    Ok(None)
}

pub(crate) fn map_reqwest(e: reqwest::Error) -> AdlError {    if e.is_timeout() {
        AdlError::Timeout
    } else if e.is_connect() {
        AdlError::Network(format!("连接失败: {e}"))
    } else if e.is_body() || e.is_decode() || e.is_request() {
        // 发送/读取请求过程中的传输层失败（服务端提前关连接、收到部分响应就断等）。
        // 这类多半是并发压力或临时抖动，按"可重试"处理，而不是当成致命错误。
        AdlError::ConnectionReset
    } else {
        AdlError::Http(e)
    }
}

/// 算文件 SHA256（流式，不整个读进内存）。
pub fn hash_file(p: &Path) -> Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(p)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

// ═══════════════════════════════════════════════════════════════════════
// 限速（令牌桶：全局共享，按"虚拟可用时刻"平滑）
// ═══════════════════════════════════════════════════════════════════════

/// 全局限速器。所有 worker 共享一个，保证总速率不超过 `rate` 字节/秒。
///
/// 实现是"虚拟时钟"：每消耗 n 字节就把 `next` 往后推 `n/rate` 秒，
/// 该睡的时长就是 `next - now`。简单、无突发、多 worker 并发也不会超速。
pub struct RateLimiter {
    rate: u64,
    next: tokio::sync::Mutex<Instant>,
}

impl RateLimiter {
    pub fn new(rate: u64) -> Self {
        Self {
            rate,
            next: tokio::sync::Mutex::new(Instant::now()),
        }
    }

    pub async fn consume(&self, n: u64) {
        if self.rate == 0 || n == 0 {
            return;
        }
        let dur = Duration::from_secs_f64(n as f64 / self.rate as f64);
        let wait = {
            let mut next = self.next.lock().await;
            let now = Instant::now();
            if *next < now {
                *next = now;
            }
            *next += dur;
            *next - now
        };
        if wait > Duration::ZERO {
            tokio::time::sleep(wait).await;
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 测速
// ═══════════════════════════════════════════════════════════════════════

/// 滑动窗口测速。
struct SpeedTracker {
    bytes: AtomicU64,
    last_bytes: AtomicU64,
    last_tick: std::sync::Mutex<Instant>,
    bps: AtomicU64,
}

impl SpeedTracker {
    fn new() -> Self {
        Self {
            bytes: AtomicU64::new(0),
            last_bytes: AtomicU64::new(0),
            last_tick: std::sync::Mutex::new(Instant::now()),
            bps: AtomicU64::new(0),
        }
    }

    fn add(&self, n: u64) {
        self.bytes.fetch_add(n, Ordering::Relaxed);
        if let Ok(mut tick) = self.last_tick.try_lock() {
            let elapsed = tick.elapsed();
            if elapsed >= Duration::from_millis(500) {
                let cur = self.bytes.load(Ordering::Relaxed);
                let prev = self.last_bytes.swap(cur, Ordering::Relaxed);
                let bps = ((cur - prev) as f64 / elapsed.as_secs_f64()) as u64;
                self.bps.store(bps, Ordering::Relaxed);
                *tick = Instant::now();
            }
        }
    }

    fn bps(&self) -> u64 {
        self.bps.load(Ordering::Relaxed)
    }
}

fn next_token() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_of_known_content() {
        let dir = crate::testutil::tmpdir("hash");
        let p = dir.join("h.txt");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            hash_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn hash_empty_file() {
        let dir = crate::testutil::tmpdir("hash2");
        let p = dir.join("e.txt");
        std::fs::write(&p, b"").unwrap();
        assert_eq!(
            hash_file(&p).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn tokens_are_unique() {
        let a = next_token();
        let b = next_token();
        assert_ne!(a, b);
    }

    #[test]
    fn default_options_sane() {
        let o = DownloadOptions::default();
        assert_eq!(o.piece_length, DEFAULT_PIECE_LENGTH);
        assert!(!o.restart);
        assert!(o.out_name.is_none());
        assert!(o.headers.is_empty());
        assert_eq!(o.concurrency, DEFAULT_CONCURRENCY);
        assert_eq!(o.chunk_size, 0, "0 = 按文件大小自动分配");
        assert_eq!(o.speed_limit, 0);
    }

    #[test]
    fn clamp_concurrency_cases() {
        assert_eq!(clamp_concurrency(0), 1);
        assert_eq!(clamp_concurrency(1), 1);
        assert_eq!(clamp_concurrency(9), 9);
        assert_eq!(clamp_concurrency(12), 12); // 不再强制 2 的幂
        assert_eq!(clamp_concurrency(16), 16);
        assert_eq!(clamp_concurrency(64), 64);
        assert_eq!(clamp_concurrency(100), 64); // 超过硬上限 → 64
        assert_eq!(clamp_concurrency(usize::MAX), 64);
    }

    #[test]
    fn controller_drops_to_working_level() {
        // target 16，服务器只让 9 个跑通：连续 7 次容量失败后应稳定在 9，且不低于 9。
        let mut c = ConcController::new(16);
        assert_eq!(c.effective, 16);
        for _ in 0..7 {
            c.on_capacity_failure(9);
        }
        assert_eq!(c.effective, 9, "应降到真正跑通的数量");
        // 再来失败也不会压到 9 以下（floor = ok_count）
        for _ in 0..5 {
            c.on_capacity_failure(9);
        }
        assert_eq!(c.effective, 9);
    }

    #[test]
    fn controller_probes_up_toward_target() {
        let mut c = ConcController::new(16);
        c.effective = 9;
        // 立刻探一次（把 last_capacity 拨到早已过去）
        c.last_capacity = Instant::now() - PROBE_BASE - Duration::from_secs(1);
        c.maybe_probe();
        assert_eq!(c.effective, 10, "稳定够久后试探 +1");
        assert!(c.probing_since.is_some());
        // 试探失败：退回 9，间隔翻倍
        let before = c.probe_interval;
        c.on_capacity_failure(9);
        assert_eq!(c.effective, 9);
        assert!(c.probe_interval > before, "试探失败应拉长间隔");
    }

    #[test]
    fn controller_never_exceeds_target() {
        let mut c = ConcController::new(4);
        for _ in 0..100 {
            c.last_capacity = Instant::now() - PROBE_MAX - Duration::from_secs(1);
            c.maybe_probe();
        }
        assert!(c.effective <= 4);
    }

    #[test]
    fn backoff_grows_and_caps() {
        let a = backoff_for(1);
        assert!(a >= Duration::from_millis(800));
        assert!(a <= Duration::from_millis(800 + 250));
        let big = backoff_for(10);
        assert!(big <= Duration::from_millis(8_000 + 250));
    }

    #[tokio::test]
    async fn rate_limiter_throttles() {
        // 200 KB/s，消费 100 KB 至少要 ~0.5s
        let lim = RateLimiter::new(200 * 1024);
        let t0 = Instant::now();
        lim.consume(100 * 1024).await;
        let e1 = t0.elapsed();
        assert!(e1 >= Duration::from_millis(400), "第一次太快: {e1:?}");
    }
}
