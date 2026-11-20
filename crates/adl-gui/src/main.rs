//! ADL 图形界面（M5 最小可用版）。
//!
//! 结构：tao 事件循环 + wry(WebView2) 窗口 + 本地静态服务器 + 托盘图标。
//! 前端是 Vue SFC（vite 构建产物），通过 `window.ipc.postMessage` 和
//! `window.__ipcResolve/__ipcEmit` 与 Rust 通信（复用上一版验证过的 IPC 协议）。

mod static_server;

use adl_app::config::Config;
use adl_core::hooks::Hooks;
use adl_core::{download, DownloadOptions, Progress};
use adl_plugin::{Mode, PluginHooks};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::WindowBuilder;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use wry::WebViewBuilder;

enum UserEvent {
    /// 向前端发一个事件：名字 + JSON 载荷
    Emit(&'static str, String),
}

struct Task {
    id: u64,
    url: String,
    status: String, // downloading | paused | completed | failed
    completed: u64,
    total: u64,
    speed: u64,
    path: Option<String>,
    error: Option<String>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

struct Gui {
    tasks: Mutex<Vec<Task>>,
    next_id: AtomicU64,
    rt: tokio::runtime::Runtime,
    proxy: EventLoopProxy<UserEvent>,
    cfg: Mutex<Config>,
}

fn main() {
    let cfg = Config::load();

    let web_root = match find_web_root() {
        Some(p) => p,
        None => {
            eprintln!(
                "找不到前端目录（web-dist / web/dist）。请先构建前端：cd web && npm run build"
            );
            std::process::exit(1);
        }
    };
    let (url, port) = static_server::start(web_root.clone()).expect("无法启动本地静态服务器");
    eprintln!("[gui] 静态服务器 {} (根目录 {})", url, web_root.display());

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("无法创建 tokio runtime");

    let gui = Arc::new(Gui {
        tasks: Mutex::new(Vec::new()),
        next_id: AtomicU64::new(1),
        rt,
        proxy: proxy.clone(),
        cfg: Mutex::new(cfg),
    });

    let window = WindowBuilder::new()
        .with_title("ADL")
        .with_inner_size(LogicalSize::new(1000.0, 700.0))
        .with_min_inner_size(LogicalSize::new(720.0, 520.0))
        .build(&event_loop)
        .expect("无法创建窗口");

    let (ipc_tx, ipc_rx) = std::sync::mpsc::channel::<String>();
    let webview = WebViewBuilder::new()
        .with_url(&url)
        .with_ipc_handler(move |req| {
            let _ = ipc_tx.send(req.body().clone());
        })
        .build(&window)
        .expect("无法创建 WebView");

    // 托盘
    let tray = build_tray();

    let _ = port;

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(120));

        match event {
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) | Event::MainEventsCleared => {
                // IPC 请求（在主线程统一处理，保证 webview 调用线程安全）
                while let Ok(body) = ipc_rx.try_recv() {
                    if let Some(script) = handle_ipc(&gui, &body) {
                        let _ = webview.evaluate_script(&script);
                    }
                }
                // 托盘菜单
                while let Ok(ev) = MenuEvent::receiver().try_recv() {
                    match ev.id.0.as_str() {
                        "show" => {
                            window.set_visible(true);
                            window.set_focus();
                        }
                        "quit" => *control_flow = ControlFlow::Exit,
                        _ => {}
                    }
                }
            }
            Event::UserEvent(UserEvent::Emit(name, payload)) => {
                let script = format!(
                    "window.__ipcEmit({}, {})",
                    serde_json::to_string(name).unwrap_or_else(|_| "\"\"".into()),
                    payload
                );
                let _ = webview.evaluate_script(&script);
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
        let _ = &tray; // 保持托盘存活
    });
}

fn build_tray() -> Option<tray_icon::TrayIcon> {
    let menu = Menu::new();
    let show = MenuItem::with_id("show", "显示主界面", true, None);
    let quit = MenuItem::with_id("quit", "退出", true, None);
    let _ = menu.append_items(&[&show, &quit]);
    let icon = make_icon();
    let mut builder = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("ADL 下载器");
    if let Some(ic) = icon {
        builder = builder.with_icon(ic);
    }
    builder.build().ok()
}

fn make_icon() -> Option<Icon> {
    // 32x32 的青色方块图标（无需外部资源文件）
    let (w, h) = (32u32, 32u32);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let border = x < 3 || y < 3 || x >= w - 3 || y >= h - 3;
            let (r, g, b, a) = if border {
                (0x2e, 0x6e, 0x6a, 255)
            } else if (x as i32 - 16).abs() + (y as i32 - 16).abs() < 10 {
                (0x4a, 0x9e, 0x9e, 255)
            } else {
                (0x12, 0x18, 0x1a, 255)
            };
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }
    Icon::from_rgba(rgba, w, h).ok()
}

fn find_web_root() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ADL_WEB_ROOT") {
        let p = PathBuf::from(p);
        if p.is_dir() {
            return Some(p);
        }
    }
    let mut cands = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("web-dist"));
            cands.push(dir.join("web"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        cands.push(cwd.join("web").join("dist"));
        cands.push(cwd.join("web"));
        cands.push(cwd.join("web-dist"));
    }
    cands.into_iter().find(|p| p.join("index.html").is_file())
}

// ─────────────────────────── IPC ───────────────────────────

fn handle_ipc(gui: &Arc<Gui>, body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    let cmd = v.get("cmd").and_then(|c| c.as_str()).unwrap_or("");
    let args = v.get("args").cloned().unwrap_or(Value::Object(Default::default()));

    let result: Result<Value, String> = match cmd {
        "ping" => Ok(json!({"ok": true, "pong": true})),
        "get_tasks" => Ok(gui_snapshot(gui)),
        "get_settings" => {
            let cfg = gui.cfg.lock().unwrap();
            Ok(settings_json(&cfg))
        }
        "set_settings" => set_settings(gui, &args),
        "add" => add_task(gui, &args),
        "pause" => pause_task(gui, &args),
        "remove" => remove_task(gui, &args),
        "open_dir" => Ok(json!({"ok": true})),
        other => Err(format!("未知命令: {other}")),
    };

    let payload = match result {
        Ok(v) => v,
        Err(msg) => json!({"__err": {"message": msg}}),
    };
    Some(format!(
        "window.__ipcResolve({}, {})",
        serde_json::to_string(&id).ok()?,
        serde_json::to_string(&payload).ok()?
    ))
}

fn gui_snapshot(gui: &Arc<Gui>) -> Value {
    let tasks = gui.tasks.lock().unwrap();
    let arr: Vec<Value> = tasks
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "url": t.url,
                "status": t.status,
                "completed": t.completed,
                "total": t.total,
                "speed": t.speed,
                "path": t.path,
                "error": t.error,
            })
        })
        .collect();
    json!(arr)
}

fn settings_json(cfg: &Config) -> Value {
    json!({
        "dir": cfg.dir,
        "connections": cfg.connections,
        "limit": cfg.limit,
        "proxy": cfg.proxy,
        "mode": cfg.mode,
        "http1": cfg.http1,
        "plugin_dir": cfg.plugin_dir,
    })
}

fn set_settings(gui: &Arc<Gui>, args: &Value) -> Result<Value, String> {
    let mut cfg = gui.cfg.lock().unwrap();
    if let Some(d) = args.get("dir").and_then(|x| x.as_str()) {
        cfg.dir = d.to_string();
    }
    if let Some(c) = args.get("connections").and_then(|x| x.as_u64()) {
        cfg.connections = adl_core::clamp_concurrency(c as usize);
    }
    if let Some(l) = args.get("limit").and_then(|x| x.as_u64()) {
        cfg.limit = l;
    }
    if let Some(p) = args.get("proxy").and_then(|x| x.as_str()) {
        cfg.proxy = p.to_string();
    }
    if let Some(m) = args.get("mode").and_then(|x| x.as_str()) {
        cfg.mode = if m.eq_ignore_ascii_case("trusted") { "trusted".into() } else { "safe".into() };
    }
    if let Some(h) = args.get("http1").and_then(|x| x.as_bool()) {
        cfg.http1 = h;
    }
    if let Some(pd) = args.get("plugin_dir").and_then(|x| x.as_str()) {
        cfg.plugin_dir = pd.to_string();
    }
    cfg.save().map_err(|e| format!("保存配置失败: {e}"))?;
    Ok(settings_json(&cfg))
}

fn add_task(gui: &Arc<Gui>, args: &Value) -> Result<Value, String> {
    let url = args
        .get("url")
        .and_then(|x| x.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or("缺少下载地址")?
        .to_string();
    let out = args.get("out").and_then(|x| x.as_str()).map(|s| s.to_string());

    let cfg = gui.cfg.lock().unwrap().clone();

    let mut opts = DownloadOptions::default();
    opts.url = url.clone();
    opts.dir = args
        .get("dir")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| cfg.dir_path());
    opts.out_name = out.clone();
    opts.concurrency = adl_core::clamp_concurrency(cfg.connections);
    opts.speed_limit = cfg.limit;
    opts.http1_only = cfg.http1;
    opts.proxy = cfg.proxy_opt();
    if let Some(ua) = cfg.user_agent_opt() {
        opts.user_agent = ua;
    }

    let mode = if cfg.mode_trusted() { Mode::Trusted } else { Mode::Safe };
    let plugin_root = cfg.plugin_dir_opt().or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("plugins")))
    });
    let hooks: Arc<dyn Hooks> = match plugin_root {
        Some(dir) if dir.is_dir() => {
            let ph = PluginHooks::load(&dir, mode);
            if ph.is_empty() {
                Arc::new(adl_core::NoopHooks)
            } else {
                Arc::new(ph)
            }
        }
        _ => Arc::new(adl_core::NoopHooks),
    };

    let id = gui.next_id.fetch_add(1, Ordering::SeqCst);
    {
        let mut tasks = gui.tasks.lock().unwrap();
        tasks.push(Task {
            id,
            url: url.clone(),
            status: "downloading".into(),
            completed: 0,
            total: 0,
            speed: 0,
            path: None,
            error: None,
            handle: None,
        });
    }

    let gui2 = gui.clone();
    let proxy = gui.proxy.clone();
    let handle = gui.rt.spawn(async move {
        let proxy_p = proxy.clone();
        let start = Instant::now();
        let last_emit = Arc::new(AtomicU64::new(0));
        let last_emit2 = last_emit.clone();
        let progress: adl_core::ProgressFn = Arc::new(move |p: Progress| {
            let ms = start.elapsed().as_millis() as u64;
            if ms.saturating_sub(last_emit2.load(Ordering::Relaxed)) >= 150 {
                last_emit2.store(ms, Ordering::Relaxed);
                let payload = json!({
                    "id": id,
                    "completed": p.completed,
                    "total": p.total,
                    "speed": p.speed_bps,
                });
                let _ = proxy_p.send_event(UserEvent::Emit("progress", payload.to_string()));
            }
        });

        let res = download(opts, hooks, Some(progress)).await;

        let mut tasks = gui2.tasks.lock().unwrap();
        if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
            match &res {
                Ok(r) => {
                    t.status = "completed".into();
                    t.completed = r.total;
                    t.total = r.total;
                    t.path = Some(r.path.to_string_lossy().to_string());
                    t.speed = r.speed_bps();
                    let payload = json!({
                        "id": id,
                        "status": "completed",
                        "path": t.path,
                        "total": r.total,
                        "sha256": r.sha256,
                    });
                    let _ = proxy.send_event(UserEvent::Emit("task_done", payload.to_string()));
                }
                Err(e) => {
                    t.status = "failed".into();
                    t.error = Some(e.to_string());
                    let payload = json!({
                        "id": id,
                        "status": "failed",
                        "error": e.to_string(),
                        "code": e.aria2_code(),
                    });
                    let _ = proxy.send_event(UserEvent::Emit("task_done", payload.to_string()));
                }
            }
        }
    });

    {
        let mut tasks = gui.tasks.lock().unwrap();
        if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
            t.handle = Some(handle);
        }
    }

    Ok(json!({"ok": true, "id": id}))
}

fn pause_task(gui: &Arc<Gui>, args: &Value) -> Result<Value, String> {
    let id = args.get("id").and_then(|x| x.as_u64()).ok_or("缺少 id")?;
    let mut tasks = gui.tasks.lock().unwrap();
    if let Some(t) = tasks.iter_mut().find(|t| t.id == id) {
        if let Some(h) = t.handle.take() {
            h.abort(); // 中止下载；已下进度保存在 .adl，重新添加可续传
        }
        if t.status == "downloading" {
            t.status = "paused".into();
        }
        Ok(json!({"ok": true}))
    } else {
        Err("任务不存在".into())
    }
}

fn remove_task(gui: &Arc<Gui>, args: &Value) -> Result<Value, String> {
    let id = args.get("id").and_then(|x| x.as_u64()).ok_or("缺少 id")?;
    let delete_files = args.get("files").and_then(|x| x.as_bool()).unwrap_or(false);
    let mut tasks = gui.tasks.lock().unwrap();
    if let Some(pos) = tasks.iter().position(|t| t.id == id) {
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
        Ok(json!({"ok": true}))
    } else {
        Err("任务不存在".into())
    }
}
