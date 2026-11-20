//! ADL 图形界面（M5 最小可用版）。
//!
//! 结构：tao 事件循环 + wry(WebView2) 窗口 + 本地静态服务器 + 托盘图标。
//! 前端是 Vue SFC（vite 构建产物），通过 `window.ipc.postMessage` 和
//! `window.__ipcResolve/__ipcEmit` 与 Rust 通信（复用上一版验证过的 IPC 协议）。

mod api_server;
mod platform;
mod static_server;

use adl_app::config::Config;
use adl_core::hooks::Hooks;
use adl_core::{download, DownloadOptions, Progress};
use adl_plugin::{Mode, PluginHooks};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Window, WindowBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use wry::{WebView, WebViewBuilder};

enum UserEvent {
    /// 向前端发一个事件：名字 + JSON 载荷
    Emit(&'static str, String),
    /// 第二个实例唤醒本实例，把窗口显示出来
    Show,
    /// 系统即将休眠
    Suspend,
    /// 系统从休眠恢复
    Resume,
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

pub(crate) struct Gui {
    pub(crate) tasks: Mutex<Vec<Task>>,
    pub(crate) next_id: AtomicU64,
    pub(crate) rt: tokio::runtime::Runtime,
    pub(crate) proxy: EventLoopProxy<UserEvent>,
    pub(crate) cfg: Mutex<Config>,
    pub(crate) hooks: Mutex<Arc<PluginHooks>>,
}

fn main() {
    // ── 单实例：已有实例在跑就唤醒它并退出 ──
    let _instance = match platform::acquire_single_instance() {
        Some(i) => i,
        None => {
            let woke = platform::signal_existing_instance();
            if woke {
                eprintln!("ADL 已在运行，已唤醒现有窗口。");
            } else {
                eprintln!("ADL 已在运行。");
            }
            return;
        }
    };

    let cfg = Config::load();
    let initial_hooks = build_hooks(&cfg);

    let web_root = match find_web_root() {
        Some(p) => p,
        None => {
            eprintln!(
                "找不到前端目录（web-dist / web/dist）。请先构建前端：cd web && npm run build"
            );
            std::process::exit(1);
        }
    };

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
        hooks: Mutex::new(initial_hooks),
    });

    // 本地 API：供浏览器扩展把下载转交给 ADL
    let api_port = std::env::var("ADL_API_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(api_server::DEFAULT_PORT);
    match api_server::start(gui.clone(), api_port) {
        Ok(p) => eprintln!("[gui] 本地 API http://127.0.0.1:{}", p),
        Err(e) => eprintln!("[gui] 本地 API 启动失败（{}），浏览器接管不可用", e),
    }

    // 静态服务器（同时提供 POST /ipc，供浏览器降级模式使用）
    let ipc_handler: Arc<dyn Fn(&str) -> String + Send + Sync> = {
        let g = gui.clone();
        Arc::new(move |body: &str| dispatch_ipc_json(&g, body))
    };
    let (url, _port) =
        static_server::start(web_root.clone(), Some(ipc_handler)).expect("无法启动本地静态服务器");
    eprintln!("[gui] 静态服务器 {} (根目录 {})", url, web_root.display());

    // WebView2 检测：缺失/残缺时降级为「浏览器模式」，UI 跑在默认浏览器里，
    // 通过本地 HTTP 的 POST /ipc 与后端通信，功能完整。
    let has_webview2 = std::env::var_os("ADL_FORCE_BROWSER").is_none()
        && platform::webview2_available();
    if !has_webview2 {
        eprintln!(
            "[gui] ⚠ 未检测到 WebView2 运行时，已降级为「浏览器模式」：界面开在你的默认浏览器里。"
        );
        eprintln!(
            "[gui]   安装 WebView2（Edge 自带或微软官方 Runtime）后即可使用原生窗口，功能完全一致。"
        );
    }

    let (ipc_tx, ipc_rx) = std::sync::mpsc::channel::<String>();
    let mut window: Option<Window> = None;
    let mut webview: Option<WebView> = None;
    if has_webview2 {
        let (w, v) = make_window(&event_loop, &url, &ipc_tx);
        window = Some(w);
        webview = Some(v);
    } else {
        platform::open_in_browser(&url);
    }

    // 托盘
    let tray = build_tray();

    // 唤醒监听（单实例第二个进程 → 本进程显示窗口）
    {
        let p = proxy.clone();
        platform::spawn_wakeup_listener(move || {
            let _ = p.send_event(UserEvent::Show);
        });
    }
    // 系统休眠/唤醒通知
    let _power = {
        let p = proxy.clone();
        platform::register_power(move |ev| {
            let ue = match ev {
                platform::PowerEvent::Suspend => UserEvent::Suspend,
                platform::PowerEvent::Resume => UserEvent::Resume,
                platform::PowerEvent::Other => return,
            };
            let _ = p.send_event(ue);
        })
    };
    if _power.is_some() {
        eprintln!("[gui] 已注册系统休眠/唤醒通知");
    } else {
        eprintln!("[gui] 休眠/唤醒通知注册失败（不影响下载，仅少了自动暂停）");
    }

    let url_for_show = url.clone();

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(120));

        match event {
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) | Event::MainEventsCleared => {
                // IPC 请求（在主线程统一处理，保证 webview 调用线程安全）
                while let Ok(body) = ipc_rx.try_recv() {
                    if let Some(wv) = &webview {
                        if let Some(script) = handle_ipc(&gui, &body) {
                            let _ = wv.evaluate_script(&script);
                        }
                    }
                }
                // 托盘菜单
                while let Ok(ev) = MenuEvent::receiver().try_recv() {
                    match ev.id.0.as_str() {
                        "show" => show_window(
                            &mut window,
                            &mut webview,
                            target,
                            &url_for_show,
                            &ipc_tx,
                            has_webview2,
                        ),
                        "quit" => *control_flow = ControlFlow::Exit,
                        _ => {}
                    }
                }
            }
            Event::UserEvent(UserEvent::Emit(name, payload)) => {
                if let Some(wv) = &webview {
                    let script = format!(
                        "window.__ipcEmit({}, {})",
                        serde_json::to_string(name).unwrap_or_else(|_| "\"\"".into()),
                        payload
                    );
                    let _ = wv.evaluate_script(&script);
                }
            }
            Event::UserEvent(UserEvent::Show) => show_window(
                &mut window,
                &mut webview,
                target,
                &url_for_show,
                &ipc_tx,
                has_webview2,
            ),
            Event::UserEvent(UserEvent::Suspend) => {
                eprintln!("[gui] 系统即将休眠：暂停进行中的任务（进度已保存在 .adl 文件）");
                for t in gui.tasks.lock().unwrap().iter_mut() {
                    if let Some(h) = t.handle.take() {
                        h.abort();
                    }
                    if t.status == "downloading" {
                        t.status = "paused".into();
                    }
                }
                if let Some(wv) = &webview {
                    let _ = wv.evaluate_script("window.__ipcEmit('system_suspend', '{}')");
                }
            }
            Event::UserEvent(UserEvent::Resume) => {
                eprintln!("[gui] 系统已恢复：任务保持暂停，请手动继续（避免链接失效）");
                if let Some(wv) = &webview {
                    let _ = wv.evaluate_script("window.__ipcEmit('system_resume', '{}')");
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                // 关闭到托盘：销毁窗口（避免 WebView2 隐藏后白屏），唤醒时重建。
                webview.take();
                window.take();
                eprintln!("[gui] 已最小化到托盘（程序仍在后台运行）");
            }
            _ => {}
        }
        let _ = &tray; // 保持托盘存活
        let _ = &_power; // 保持电源通知注册存活
    });
}

/// 创建窗口 + WebView（首次启动与从托盘唤醒时复用）。
fn make_window(
    target: &EventLoopWindowTarget<UserEvent>,
    url: &str,
    ipc_tx: &std::sync::mpsc::Sender<String>,
) -> (Window, WebView) {
    let window = WindowBuilder::new()
        .with_title("ADL")
        .with_inner_size(LogicalSize::new(1000.0, 700.0))
        .with_min_inner_size(LogicalSize::new(720.0, 520.0))
        .build(target)
        .expect("无法创建窗口");
    let tx = ipc_tx.clone();
    let webview = WebViewBuilder::new()
        .with_url(url)
        .with_ipc_handler(move |req| {
            let _ = tx.send(req.body().clone());
        })
        .build(&window)
        .expect("无法创建 WebView");
    (window, webview)
}

/// 显示主界面：原生模式重建/唤回窗口；浏览器模式重新打开浏览器。
fn show_window(
    window: &mut Option<Window>,
    webview: &mut Option<WebView>,
    target: &EventLoopWindowTarget<UserEvent>,
    url: &str,
    ipc_tx: &std::sync::mpsc::Sender<String>,
    has_webview2: bool,
) {
    if !has_webview2 {
        platform::open_in_browser(url);
        return;
    }
    if webview.is_none() {
        let (w, v) = make_window(target, url, ipc_tx);
        *window = Some(w);
        *webview = Some(v);
        return;
    }
    if let Some(w) = window {
        let _ = w.set_minimized(false);
        w.set_visible(true);
        w.set_focus();
    }
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

fn dispatch_ipc(gui: &Arc<Gui>, body: &str) -> Option<(Value, Value)> {
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
        "ui_snapshot" => {
            let values = load_plugin_settings();
            let hooks = gui.hooks.lock().unwrap().clone();
            Ok(hooks.ui_snapshot(&values))
        }
        "ui_render" => {
            let plugin = args.get("plugin").and_then(|x| x.as_str()).unwrap_or("");
            let page = args.get("page").and_then(|x| x.as_str()).unwrap_or("");
            let ctx = args.get("ctx").cloned().unwrap_or_else(|| json!({}));
            let hooks = gui.hooks.lock().unwrap().clone();
            Ok(hooks
                .render(plugin, page, &ctx)
                .unwrap_or_else(|| json!({"type":"text","value":"（插件没有返回内容）"})))
        }
        "run_action" => {
            let plugin = args.get("plugin").and_then(|x| x.as_str()).unwrap_or("");
            let action = args.get("action").and_then(|x| x.as_str()).unwrap_or("");
            let payload = args.get("payload").cloned().unwrap_or_else(|| json!({}));
            let hooks = gui.hooks.lock().unwrap().clone();
            match hooks.run_action(plugin, action, &payload) {
                Some(v) => Ok(v),
                None => Err("插件动作不可用（需要 trusted 模式）".into()),
            }
        }
        "run_command" => {
            let plugin = args.get("plugin").and_then(|x| x.as_str()).unwrap_or("");
            let command = args.get("command").and_then(|x| x.as_str()).unwrap_or("");
            let hooks = gui.hooks.lock().unwrap().clone();
            match hooks.run_command(plugin, command) {
                Some(v) => Ok(v),
                None => Err("插件命令不可用（需要 trusted 模式）".into()),
            }
        }
        "plugin_setting_set" => {
            let plugin = args.get("plugin").and_then(|x| x.as_str()).unwrap_or("");
            let key = args.get("key").and_then(|x| x.as_str()).unwrap_or("");
            let value = args.get("value").and_then(|x| x.as_str()).unwrap_or("");
            if plugin.is_empty() || key.is_empty() {
                Err("缺少 plugin 或 key".to_string())
            } else {
                save_plugin_setting(plugin, key, value).map(|_| {
                    let values = load_plugin_settings();
                    let hooks = gui.hooks.lock().unwrap().clone();
                    hooks.ui_snapshot(&values)
                })
            }
        }
        other => Err(format!("未知命令: {other}")),
    };

    let payload = match result {
        Ok(v) => v,
        Err(msg) => json!({"__err": {"message": msg}}),
    };
    Some((id, payload))
}

/// wry / WebView2 模式的 IPC：把结果包成调用页面回调的脚本。
fn handle_ipc(gui: &Arc<Gui>, body: &str) -> Option<String> {
    let (id, payload) = dispatch_ipc(gui, body)?;
    Some(format!(
        "window.__ipcResolve({}, {})",
        serde_json::to_string(&id).ok()?,
        serde_json::to_string(&payload).ok()?
    ))
}

/// 浏览器降级模式的 IPC：`POST /ipc` 的响应体，形如 `{"id":..,"payload":..}`。
fn dispatch_ipc_json(gui: &Arc<Gui>, body: &str) -> String {
    match dispatch_ipc(gui, body) {
        Some((id, payload)) => json!({"id": id, "payload": payload}).to_string(),
        None => json!({
            "id": Value::Null,
            "payload": {"__err": {"message": "bad request"}}
        })
        .to_string(),
    }
}

fn plugins_root(cfg: &Config) -> Option<PathBuf> {
    cfg.plugin_dir_opt().or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("plugins")))
    })
}

/// 按当前配置加载插件（含按主配置的 enabled 开关过滤）。
fn build_hooks(cfg: &Config) -> Arc<PluginHooks> {
    let mode = if cfg.mode_trusted() { Mode::Trusted } else { Mode::Safe };
    match plugins_root(cfg) {
        Some(dir) if dir.is_dir() => {
            let all = adl_plugin::load_dir(&dir, mode);
            let kept: Vec<_> = all
                .into_iter()
                .filter(|p| cfg.plugin_enabled(&p.id))
                .collect();
            Arc::new(PluginHooks::new(kept))
        }
        _ => Arc::new(PluginHooks::new(Vec::new())),
    }
}

fn plugin_settings_dir() -> PathBuf {
    adl_app::config::app_data_dir().join("plugin-settings")
}

/// 读所有插件自己的设置文件（`plugin-settings/<id>.json`）。
fn load_plugin_settings() -> adl_plugin::ui::SettingsValues {
    let mut out = BTreeMap::new();
    if let Ok(rd) = std::fs::read_dir(plugin_settings_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = p.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(txt) = std::fs::read_to_string(&p) else {
                continue;
            };
            let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&txt) else {
                continue;
            };
            let mut inner = BTreeMap::new();
            for (k, v) in map {
                let sv = match v {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                inner.insert(k, sv);
            }
            out.insert(id.to_string(), inner);
        }
    }
    out
}

fn save_plugin_setting(plugin: &str, key: &str, value: &str) -> Result<(), String> {
    let dir = plugin_settings_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{plugin}.json"));
    let mut map: BTreeMap<String, Value> = if let Ok(t) = std::fs::read_to_string(&path) {
        serde_json::from_str(&t).unwrap_or_default()
    } else {
        BTreeMap::new()
    };
    map.insert(key.to_string(), Value::String(value.to_string()));
    let txt = serde_json::to_string_pretty(&map).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, txt).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
}

pub(crate) fn gui_snapshot(gui: &Arc<Gui>) -> Value {    let tasks = gui.tasks.lock().unwrap();
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
    let snapshot = settings_json(&cfg);
    let new_cfg = cfg.clone();
    drop(cfg);
    // 设置可能改了插件目录/模式/开关，重建插件实例
    *gui.hooks.lock().unwrap() = build_hooks(&new_cfg);
    Ok(snapshot)
}

pub(crate) fn add_task(gui: &Arc<Gui>, args: &Value) -> Result<Value, String> {
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
    // 浏览器接管时可带过来的额外信息
    if let Some(r) = args.get("referer").and_then(|x| x.as_str()) {
        if !r.is_empty() {
            opts.referer = Some(r.to_string());
        }
    }
    if let Some(ua) = args.get("user_agent").and_then(|x| x.as_str()) {
        if !ua.is_empty() {
            opts.user_agent = ua.to_string();
        }
    }
    if let Some(obj) = args.get("headers").and_then(|x| x.as_object()) {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                opts.headers.push((k.clone(), s.to_string()));
            }
        }
    }
    if let Some(c) = args.get("cookies").and_then(|x| x.as_str()) {
        if !c.is_empty() {
            opts.headers.push(("Cookie".to_string(), c.to_string()));
        }
    }

    let hooks: Arc<dyn Hooks> = gui.hooks.lock().unwrap().clone();

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
