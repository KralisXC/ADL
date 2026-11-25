// ───────────────────────── 队列 + 持久化 ─────────────────────────

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};

use adl_core::hooks::Hooks;
use adl_core::{download, DownloadOptions, Progress};

use crate::{display_name_of_url, format_bytes, notice, Gui, Task};

/// 把内存里的任务表整份写回 `tasks.json`。
///
/// 只在"用户看得见的状态变化"上调用：加任务、开始/暂停/继续/删除、完成/失败。
/// 不跟着进度每秒写 —— 进度本来就能从 `.adl` 控制文件恢复，丢最后一点点
/// 进度无所谓，而下载时每秒写几十次盘是纯浪费。
pub(crate) fn save_tasks(gui: &Arc<Gui>) {
    let next_id = gui.next_id.load(Ordering::SeqCst);
    let store = {
        let tasks = gui.tasks.lock().unwrap();
        adl_app::tasks::TaskStore {
            next_id,
            tasks: tasks
                .iter()
                .map(|t| adl_app::tasks::SavedTask {
                    id: t.id,
                    url: t.url.clone(),
                    out_name: t.out_name.clone(),
                    dir: t.dir.clone(),
                    user_agent: t.user_agent.clone(),
                    referer: t.referer.clone(),
                    headers: t.headers.clone(),
                    cookies: t.cookies.clone(),
                    status: t.status.clone(),
                    completed: t.completed,
                    total: t.total,
                    path: t.path.clone(),
                    error: t.error.clone(),
                    added_unix: t.added_unix,
                })
                .collect(),
        }
    };
    let _ = store.save();
}

/// 同时下载上限（至少 1，配置写 0 也不能把队列焊死）。
fn max_active(gui: &Arc<Gui>) -> usize {
    gui.cfg
        .lock()
        .unwrap()
        .max_concurrent_tasks
        .clamp(1, 16)
}

/// 有空位就把排队最久的任务拉起来，直到占满上限或队列空。
///
/// 必须在**不持有任务锁**的时候调用（`launch_task` 里还要加锁）。
pub(crate) fn pump_queue(gui: &Arc<Gui>) {
    loop {
        let next = {
            let tasks = gui.tasks.lock().unwrap();
            let max = max_active(gui);
            let active = tasks.iter().filter(|t| t.status == "downloading").count();
            if active >= max {
                return;
            }
            // 按 id（= 入队顺序）取最早的一条，队列才像队列。
            tasks
                .iter()
                .filter(|t| t.status == "queued")
                .map(|t| t.id)
                .min()
        };
        match next {
            Some(id) => {
                {
                    let mut tasks = gui.tasks.lock().unwrap();
                    if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
                        // 先改状态再拉起：否则循环第二次还会挑到它。
                        t.status = "downloading".into();
                        t.error = None;
                    } else {
                        return;
                    }
                }
                launch_task(gui, id);
            }
            None => return,
        }
    }
}

/// 真正把一条任务丢给 tokio 跑。
///
/// 从 `Task` 里读参数而不是从 `args` 读：排队中的任务被拉起时早就不记得
/// 当初那个 `Value` 了，而单任务的 UA / Referer / headers 都必须原样复用。
fn launch_task(gui: &Arc<Gui>, id: u64) {
    let (url, opts_fields) = {
        let tasks = gui.tasks.lock().unwrap();
        let Some(t) = tasks.iter().find(|t| t.id == id) else {
            return;
        };
        (
            t.url.clone(),
            (
                t.dir.clone(),
                t.out_name.clone(),
                t.user_agent.clone(),
                t.referer.clone(),
                t.headers.clone(),
                t.cookies.clone(),
            ),
        )
    };
    let (dir, out_name, user_agent, referer, headers, cookies) = opts_fields;

    let cfg = gui.cfg.lock().unwrap().clone();
    let mut opts = DownloadOptions::default();
    opts.url = url.clone();
    opts.dir = dir
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| cfg.dir_path());
    opts.out_name = out_name;
    opts.concurrency = adl_core::clamp_concurrency(cfg.connections);
    opts.speed_limit = cfg.limit;
    opts.http1_only = cfg.http1;
    opts.proxy = cfg.proxy_opt();
    if let Some(ua) = cfg.user_agent_opt() {
        opts.user_agent = ua;
    }
    if let Some(ua) = user_agent.filter(|s| !s.is_empty()) {
        opts.user_agent = ua;
    }
    if let Some(r) = referer.filter(|s| !s.is_empty()) {
        opts.referer = Some(r);
    }
    opts.headers.extend(headers);
    if let Some(c) = cookies.filter(|s| !s.is_empty()) {
        opts.headers.push(("Cookie".to_string(), c));
    }

    let hooks: Arc<dyn Hooks> = gui.hooks.lock().unwrap().clone();

    let gui2 = gui.clone();
    let handle = gui.rt.spawn(async move {
        let gui3 = gui2.clone();
        let start = Instant::now();
        let last_emit = Arc::new(AtomicU64::new(0));
        let last_emit2 = last_emit.clone();
        let progress: adl_core::ProgressFn = Arc::new(move |p: Progress| {
            let ms = start.elapsed().as_millis() as u64;
            if ms.saturating_sub(last_emit2.load(Ordering::Relaxed)) >= 150 {
                last_emit2.store(ms, Ordering::Relaxed);
                if let Ok(mut tasks) = gui3.tasks.lock() {
                    if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
                        t.completed = p.completed;
                        t.total = p.total;
                        t.speed = p.speed_bps;
                        t.connections = p.connections;
                        t.runs = p.runs.clone();
                    }
                }
            }
        });

        let res = download(opts, hooks, Some(progress)).await;

        // 完成 / 失败都记一条消息：状态条 6 秒就没了，用户切走页面就看不到
        // "那个到底下完没有"。
        match &res {
            Ok(r) => {
                let name = r
                    .path
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| display_name_of_url(&url));
                if let Ok(mut q) = gui2.pending_notices.lock() {
                    q.push((
                        notice::Level::Ok,
                        format!("下载完成：{name}"),
                        format!(
                            "{} · 用时 {:.1} 秒",
                            format_bytes(r.total),
                            start.elapsed().as_secs_f64()
                        ),
                    ));
                }
            }
            Err(e) => {
                if let Ok(mut q) = gui2.pending_notices.lock() {
                    q.push((
                        notice::Level::Error,
                        format!("下载失败：{}", display_name_of_url(&url)),
                        e.to_string(),
                    ));
                }
            }
        }

        {
            let mut tasks = gui2.tasks.lock().unwrap();
            if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
                // 收尾时一定要把"正在跑的线路"清掉：引擎在最后一批 worker
                // 退出之后就不再 emit_progress 了，这里不清的话详情页会
                // 一直挂着几十条残留区间（已写入 0 B 也照样列出来）。
                t.runs.clear();
                match &res {
                    Ok(r) => {
                        t.status = "completed".into();
                        t.completed = r.total;
                        t.total = r.total;
                        t.path = Some(r.path.to_string_lossy().to_string());
                        t.speed = r.speed_bps();
                        t.connections = 0;
                    }
                    Err(e) => {
                        t.status = "failed".into();
                        t.error = Some(e.to_string());
                        t.connections = 0;
                    }
                }
            }
        }

        // 一条跑完 = 空出一个位置：拉下一条排队的，再把状态存下来。
        save_tasks(&gui2);
        pump_queue(&gui2);
    });

    {
        let mut tasks = gui.tasks.lock().unwrap();
        if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
            t.handle = Some(handle);
        }
    }
}

/// 新建一条任务。超出并发上限的进队列，返回里 `queued=true`。
pub(crate) fn add_task(gui: &Arc<Gui>, args: &Value) -> Result<Value, String> {
    let url = args
        .get("url")
        .and_then(|x| x.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or("缺少下载地址")?
        .trim()
        .to_string();
    // 只接受 http(s)：拖进来一段随便的文字不该变成一条必然失败的任务。
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("不是 http/https 地址：{url}"));
    }
    let out = args
        .get("out")
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let dir = args
        .get("dir")
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let user_agent = args
        .get("user_agent")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    let referer = args
        .get("referer")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    let cookies = args
        .get("cookies")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(obj) = args.get("headers").and_then(|x| x.as_object()) {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                headers.push((k.clone(), s.to_string()));
            }
        }
    }

    let cfg = gui.cfg.lock().unwrap().clone();
    let resolved_dir = dir
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| cfg.dir_path());

    let id = gui.next_id.fetch_add(1, Ordering::SeqCst);
    {
        let mut tasks = gui.tasks.lock().unwrap();
        tasks.push(Task {
            id,
            url: url.clone(),
            // 一律先记成「排队中」，再让 `pump_queue` 决定能不能立刻开跑 ——
            // 直接写成 downloading 的话它不会被 pump 挑到，永远不启动。
            status: "queued".into(),
            completed: 0,
            total: 0,
            speed: 0,
            connections: 0,
            runs: Vec::new(),
            dir: Some(resolved_dir.to_string_lossy().to_string()),
            path: None,
            error: None,
            out_name: out,
            user_agent,
            referer,
            headers,
            cookies,
            added_unix: adl_app::history::now_unix(),
            handle: None,
        });
    }

    // 有空位就立刻开跑；没有就老实排队。
    pump_queue(gui);
    let queued = {
        let tasks = gui.tasks.lock().unwrap();
        tasks
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.status == "queued")
            .unwrap_or(false)
    };
    save_tasks(gui);
    Ok(json!({"ok": true, "id": id, "queued": queued}))
}

/// 批量添加：一次解析多行文本，逐条入队。
///
/// 用户 m18583：「批量添加的话可以添加一下多行黏贴」。每行一条 URL，
/// 空行与重复行跳过 —— 粘贴回来的列表里重复项很常见，重复入队会下两遍。
pub(crate) fn add_batch(gui: &Arc<Gui>, text: &str) -> (usize, Vec<String>) {
    let mut seen: Vec<String> = Vec::new();
    let mut ok = 0usize;
    let mut errs: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if seen.iter().any(|s| s == line) {
            continue;
        }
        seen.push(line.to_string());
        match add_task(gui, &json!({"url": line})) {
            Ok(_) => ok += 1,
            Err(e) => errs.push(e),
        }
    }
    (ok, errs)
}

/// 暂停一条：下载中的停下并让出名额，排队中的直接下队（也停在"已暂停"）。
pub(crate) fn pause_task(gui: &Arc<Gui>, args: &Value) -> Result<Value, String> {
    let id = args.get("id").and_then(|x| x.as_u64()).ok_or("缺少 id")?;
    {
        let mut tasks = gui.tasks.lock().unwrap();
        let Some(t) = tasks.iter_mut().find(|t| t.id == id) else {
            return Err("任务不存在".into());
        };
        if let Some(h) = t.handle.take() {
            h.abort();
        }
        if matches!(t.status.as_str(), "downloading" | "queued") {
            t.status = "paused".into();
        }
        // 停下来的任务没有"正在跑的线路"了，残留的区间留着会让人
        // 以为它还在下（详情页照样会列出来）。
        t.runs.clear();
        t.speed = 0;
        t.connections = 0;
    }
    save_tasks(gui);
    // 让出的名额给排队的任务。
    pump_queue(gui);
    Ok(json!({"ok": true}))
}

/// 继续一条：放回队列，由 `pump_queue` 决定什么时候真开跑。
///
/// 以前这里的做法是"按同 id 的 url 重新 add 一条新任务"，于是列表里会
/// 多出一行、原行还挂着「已暂停」—— 看着像下了两遍。
pub(crate) fn resume_task(gui: &Arc<Gui>, args: &Value) -> Result<Value, String> {
    let id = args.get("id").and_then(|x| x.as_u64()).ok_or("缺少 id")?;
    {
        let mut tasks = gui.tasks.lock().unwrap();
        let Some(t) = tasks.iter_mut().find(|t| t.id == id) else {
            return Err("任务不存在".into());
        };
        if !matches!(t.status.as_str(), "paused" | "failed" | "queued") {
            return Ok(json!({"ok": true, "skipped": true}));
        }
        t.status = "queued".into();
        t.error = None;
    }
    save_tasks(gui);
    pump_queue(gui);
    Ok(json!({"ok": true}))
}

pub(crate) fn pause_all(gui: &Arc<Gui>) {
    {
        let mut tasks = gui.tasks.lock().unwrap();
        for t in tasks.iter_mut() {
            if matches!(t.status.as_str(), "downloading" | "queued") {
                if let Some(h) = t.handle.take() {
                    h.abort();
                }
                t.status = "paused".into();
                t.runs.clear();
                t.speed = 0;
                t.connections = 0;
            }
        }
    }
    save_tasks(gui);
}

pub(crate) fn resume_all(gui: &Arc<Gui>) {
    {
        let mut tasks = gui.tasks.lock().unwrap();
        for t in tasks.iter_mut() {
            if matches!(t.status.as_str(), "paused" | "failed") {
                t.status = "queued".into();
                t.error = None;
            }
        }
    }
    save_tasks(gui);
    pump_queue(gui);
}

pub(crate) fn remove_task(gui: &Arc<Gui>, args: &Value) -> Result<Value, String> {
    let id = args.get("id").and_then(|x| x.as_u64()).ok_or("缺少 id")?;
    let delete_files = args.get("files").and_then(|x| x.as_bool()).unwrap_or(false);
    {
        let mut tasks = gui.tasks.lock().unwrap();
        let Some(pos) = tasks.iter().position(|t| t.id == id) else {
            return Err("任务不存在".into());
        };
        let t = tasks.remove(pos);
        if let Some(h) = t.handle {
            h.abort();
        }
        if delete_files {
            if let Some(p) = &t.path {
                let path = PathBuf::from(p);
                let _ = std::fs::remove_file(&path);
                let mut ctl = path.into_os_string();
                ctl.push(".adl");
                let _ = std::fs::remove_file(PathBuf::from(ctl));
            }
        }
    }
    save_tasks(gui);
    // 删掉一条正在跑的任务也等于空出一个名额。
    pump_queue(gui);
    Ok(json!({"ok": true}))
}

/// 启动时把 `tasks.json` 读回来。
///
/// 规则：
/// - 已完成 / 失败的照原样进列表（能点"打开文件所在位置"）；
/// - 上次在跑、或排着队、或用户手动暂停的，一律改成 `queued` 交给队列 ——
///   重启时不该一次拉起十几条把刚开机的网络打满；
/// - `next_id` 取文件里的 `next_id` 与 `max_id+1` 的较大者，避免复用旧 id。
pub(crate) fn restore_tasks(gui: &Arc<Gui>) -> usize {
    let store = adl_app::tasks::TaskStore::load();
    if store.tasks.is_empty() {
        return 0;
    }
    let mut restored = 0usize;
    {
        let mut tasks = gui.tasks.lock().unwrap();
        for s in store.tasks {
            let status = match s.status.as_str() {
                "completed" => "completed",
                "failed" => "failed",
                // downloading / queued / paused / 未知 → 排队等名额
                _ => "queued",
            };
            restored += 1;
            tasks.push(Task {
                id: s.id,
                url: s.url,
                status: status.into(),
                completed: s.completed,
                total: s.total,
                speed: 0,
                connections: 0,
                runs: Vec::new(),
                dir: s.dir,
                path: s.path,
                error: s.error,
                out_name: s.out_name,
                user_agent: s.user_agent,
                referer: s.referer,
                headers: s.headers,
                cookies: s.cookies,
                added_unix: s.added_unix,
                handle: None,
            });
        }
    }
    let next = store.next_id.max({
        gui.tasks
            .lock()
            .unwrap()
            .iter()
            .map(|t| t.id)
            .max()
            .unwrap_or(0)
            + 1
    });
    gui.next_id.store(next, Ordering::SeqCst);
    // 恢复完立刻推一次队列：用户拉开程序就看见"在下了"，
    // 而不是所有行都停在"排队中"等他手动点。
    pump_queue(gui);
    restored
}
