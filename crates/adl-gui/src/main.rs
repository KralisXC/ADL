//! ADL 图形界面：纯 Rust 原生 UI（egui / eframe），**不依赖 WebView2**。
//! 跨平台：Windows / Linux / macOS 均可编译运行。
//!
//! 本文件只负责“壳”（窗口与交互）；下载引擎在 `adl-core`，插件在 `adl-plugin`，
//! 配置与历史在 `adl-app`。浏览器接管用的本地 HTTP API 仍由 `api_server` 提供。

mod api_server;
mod platform;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use serde_json::{json, Value};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use adl_app::config::Config;
use adl_core::hooks::Hooks;
use adl_core::{download, DownloadOptions, Progress};
use adl_plugin::{Mode, PluginHooks};
use adl_ui::ChipIcon;

// ───────────────────────── 共享状态 ─────────────────────────

pub(crate) struct Task {
    pub(crate) id: u64,
    pub(crate) url: String,
    pub(crate) status: String,
    pub(crate) completed: u64,
    pub(crate) total: u64,
    pub(crate) speed: u64,
    /// 当前活跃连接数（引擎每帧通过 `Progress::connections` 报上来）。
    /// 顶栏的"并行线路"chip 就是把所有进行中任务的这个值加起来。
    pub(crate) connections: u32,
    pub(crate) path: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) handle: Option<tokio::task::JoinHandle<()>>,
}

pub(crate) struct Gui {
    pub(crate) tasks: Mutex<Vec<Task>>,
    pub(crate) next_id: AtomicU64,
    pub(crate) rt: tokio::runtime::Runtime,
    pub(crate) cfg: Mutex<Config>,
    pub(crate) hooks: Mutex<Arc<PluginHooks>>,
}

#[derive(Clone)]
struct TaskView {
    id: u64,
    url: String,
    status: String,
    completed: u64,
    total: u64,
    speed: u64,
    connections: u32,
    path: Option<String>,
    error: Option<String>,
}

fn snapshot(gui: &Gui) -> Vec<TaskView> {
    gui.tasks
        .lock()
        .unwrap()
        .iter()
        .map(|t| TaskView {
            id: t.id,
            url: t.url.clone(),
            status: t.status.clone(),
            completed: t.completed,
            total: t.total,
            speed: t.speed,
            connections: t.connections,
            path: t.path.clone(),
            error: t.error.clone(),
        })
        .collect()
}

// ───────────────────────── 应用 ─────────────────────────

#[derive(Clone, PartialEq)]
enum Tab {
    Home,
    Tasks,
    Settings,
    Plugin(String, String),
}

/// 主页热力图的时间跨度。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Range {
    Week,
    Month,
    Year,
}

enum Dispatch {
    Action { plugin: String, action: String },
    Setting { plugin: String, key: String, value: String },
}

struct App {
    gui: Arc<Gui>,
    tab: Tab,
    // 添加下载表单（现在是弹窗，不再是页面）
    add_open: bool,
    f_url: String,
    f_out: String,
    // 设置表单
    s_dir: String,
    s_conn: u64,
    s_limit: u64,
    s_proxy: String,
    s_mode: String,
    s_plugin_dir: String,
    // 限速显示单位（"auto" 或固定单位）
    limit_unit: String,
    // 侧栏收起
    sidebar_collapsed: bool,
    // 主页热力图范围
    home_range: Range,
    // 关闭行为询问弹窗（None = 不显示）
    close_ask: Option<CloseAsk>,
    // 提示
    status: Option<(String, bool, Instant)>,
    // 插件 UI 缓存
    plugin_snapshot: Value,
    plugin_snapshot_at: Instant,
    plugin_view: Option<Value>,
    plugin_edit: BTreeMap<(String, String), String>,
    // 唤醒/电源标志
    wake: Arc<AtomicBool>,
    suspend: Arc<AtomicBool>,
    resume: Arc<AtomicBool>,
    cmd_open: bool,
    cmd_query: String,
    // 托盘图标：持有即可保活（Windows 上 tray-icon 自己带消息线程）
    _tray: Option<TrayIcon>,
    // 自绘标题栏用的最大化状态（从 ViewportInfo 同步，点击时本地翻转以即时响应）
    maximized: bool,
    /// "正在退出"闸门：用户确认退出后放行 Close，避免被自己的关闭拦截挡回去。
    closing: bool,
    /// 弹窗淡入计时器
    fade: Fade,
}

/// 首次点关闭时问用户"退出还是最小化"。
struct CloseAsk {
    /// 勾选"记住我的选择"
    remember: bool,
}

/// 按比例缩放一个颜色的 alpha（保留 RGB）。
///
/// 不能直接用 `gamma_multiply`：它把 RGB 也一起乘，白色卡片淡到一半会变成
/// **灰色**卡片，看起来像蒙了层脏东西，而不是"半透明地浮出来"。
fn fade_color(c: egui::Color32, a: f32) -> egui::Color32 {
    let a = (c.a() as f32 * a.clamp(0.0, 1.0)).round() as u8;
    egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

/// 弹窗淡入用的 `Frame`：底色 / 描边 / 阴影一起跟着 `a` 走。
///
/// 为什么要显式给 `Modal` 传 frame：`Modal` 内部是 `Frame::popup(style)` 在
/// **内容画完之后**才 `paint` 卡片，`ui.set_opacity` 只影响"之后画的图元"，
/// 管不到它。不这么写的话，文字在淡入、白卡却是"啪"地一下出现的。
fn fade_frame(ctx: &egui::Context, a: f32) -> egui::Frame {
    // `Context` 没有 `style()`，只有按主题取的 `style_of`；本项目 Light/Dark
    // 装的是同一份样式，取 Light 即可。
    let mut f = egui::Frame::popup(&ctx.style_of(egui::Theme::Light));
    f.fill = fade_color(f.fill, a);
    f.stroke.color = fade_color(f.stroke.color, a);
    f.shadow.color = fade_color(f.shadow.color, a);
    f
}

/// 弹窗淡入时长（秒）。0.14 和 `style.animation_time` 对齐，切换时不打架。
const FADE_SECS: f32 = 0.14;

/// 状态提示条显示时长与"末尾淡出"区间。
const STATUS_SECS: f32 = 6.0;
const STATUS_FADE: f32 = 0.45;

/// 弹窗淡入计时器。
///
/// 为什么不用 `ctx.animate_bool`：egui 的动画管理器在**首次**遇到某个 id 时
/// 直接把 `last_value` 初始化成目标值并原样返回，也就是"第一帧就是全不透明"，
/// 根本不会播动画（`animation_manager.rs` 里 `None => insert(...); end`）。
/// 弹窗恰恰每次都换 id、每帧只在打开期间存在，正好踩中这个分支。
///
/// 所以这里自己记时间戳：overlay 身份一变就重置起点，`alpha()` 按经过时间
/// 给出 0→1 的缓出曲线。
struct Fade {
    /// 当前 overlay 的身份（0 = 没有弹窗，1/2/3 = 添加/关闭/命令面板）
    key: u8,
    /// 淡入起点
    start: Option<Instant>,
}

impl Default for Fade {
    fn default() -> Self {
        Self {
            key: 0,
            start: None,
        }
    }
}

impl Fade {
    /// 每帧绘制前调用：overlay 换了就重置起点。
    fn track(&mut self, key: u8) {
        if self.key != key {
            self.key = key;
            self.start = if key == 0 { None } else { Some(Instant::now()) };
        }
    }

    /// 0.0（刚打开）→ 1.0（完全不透明），`dur` 秒内走完，缓出曲线。
    fn alpha(&self, dur: f32) -> f32 {
        let Some(t0) = self.start else {
            return 1.0;
        };
        let t = (t0.elapsed().as_secs_f32() / dur.max(0.001)).clamp(0.0, 1.0);
        // 缓出：开头快、结尾慢，观感比线性"软"。
        1.0 - (1.0 - t) * (1.0 - t)
    }

    /// 是否还在淡入（用来决定要不要继续请求重绘）。
    fn running(&self, dur: f32) -> bool {
        self.start.is_some() && self.alpha(dur) < 1.0
    }
}

impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        gui: Arc<Gui>,
        wake: Arc<AtomicBool>,
        suspend: Arc<AtomicBool>,
        resume: Arc<AtomicBool>,
        tray: Option<TrayIcon>,
    ) -> Self {
        install_cjk_font(&cc.egui_ctx);
        // 主题令牌与全局 Visuals 全部由 adl-ui 提供：这里不再手工赋值颜色，
        // 换肤只需改 `adl_ui::Palette`。
        adl_ui::Theme::light().install_and_apply(&cc.egui_ctx);

        let cfg = gui.cfg.lock().unwrap().clone();
        // ADL_TAB 仅用于开发/截图脚本直达某个页面，不写进文档：
        //   home | tasks | settings | add | plugin:<pluginId>:<pageId>
        // `add` 现在打开弹窗而不是切页。
        let mut add_open = false;
        let initial_tab = match std::env::var("ADL_TAB").unwrap_or_default().as_str() {
            "add" => {
                add_open = true;
                Tab::Tasks
            }
            "home" => Tab::Home,
            "settings" => Tab::Settings,
            s if s.starts_with("plugin:") => {
                let mut it = s.trim_start_matches("plugin:").splitn(2, ':');
                let p = it.next().unwrap_or("").to_string();
                let g = it.next().unwrap_or("").to_string();
                Tab::Plugin(p, g)
            }
            _ => Tab::Tasks,
        };
        Self {
            gui,
            tab: initial_tab,
            add_open,
            f_url: String::new(),
            f_out: String::new(),
            s_dir: cfg.dir.clone(),
            s_conn: cfg.connections as u64,
            s_limit: cfg.limit,
            s_proxy: cfg.proxy.clone(),
            s_mode: if cfg.mode_trusted() { "trusted".into() } else { "safe".into() },
            s_plugin_dir: cfg.plugin_dir.clone(),
            limit_unit: if cfg.limit_unit.trim().is_empty() {
                "auto".to_string()
            } else {
                cfg.limit_unit.clone()
            },
            sidebar_collapsed: cfg.sidebar_collapsed,
            home_range: Range::Month,
            close_ask: None,
            status: None,
            plugin_snapshot: json!({}),
            plugin_snapshot_at: Instant::now() - Duration::from_secs(3600),
            plugin_view: None,
            plugin_edit: BTreeMap::new(),
            wake,
            suspend,
            resume,
            cmd_open: false,
            cmd_query: String::new(),
            _tray: tray,
            maximized: false,
            closing: false,
            fade: Fade::default(),
        }
    }

    fn set_status(&mut self, msg: impl Into<String>, err: bool) {
        self.status = Some((msg.into(), err, Instant::now()));
    }

    fn refresh_plugin_snapshot(&mut self) {
        if self.plugin_snapshot_at.elapsed() < Duration::from_millis(1200) {
            return;
        }
        let hooks = self.gui.hooks.lock().unwrap().clone();
        self.plugin_snapshot = hooks.ui_snapshot(&load_plugin_settings());
        self.plugin_snapshot_at = Instant::now();
    }

    fn reload_plugin_view(&mut self) {
        if let Tab::Plugin(plugin, page) = self.tab.clone() {
            let hooks = self.gui.hooks.lock().unwrap().clone();
            let ctx = json!({});
            self.plugin_view = hooks.render(&plugin, &page, &ctx);
        }
    }

    fn do_add(&mut self) {
        if self.f_url.trim().is_empty() {
            self.set_status("请填写下载地址", true);
            return;
        }
        let out = self.f_out.trim();
        let args = json!({
            "url": self.f_url.trim(),
            "out": if out.is_empty() { Value::Null } else { json!(out) },
        });
        match add_task(&self.gui, &args) {
            Ok(v) => {
                let id = v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
                self.set_status(format!("已添加任务 #{id}"), false);
                self.f_url.clear();
                self.f_out.clear();
                self.add_open = false;
                self.tab = Tab::Tasks;
            }
            Err(e) => self.set_status(e, true),
        }
    }

    fn do_save_settings(&mut self) {
        // `http1` 不再暴露给用户（自动协商失败会自己退回 HTTP/1.1），
        // 这里仍原样带上当前配置值，避免保存时把它重置。
        let http1 = self.gui.cfg.lock().unwrap().http1;
        let args = json!({
            "dir": self.s_dir,
            "connections": self.s_conn,
            "limit": self.s_limit,
            "proxy": self.s_proxy,
            "mode": self.s_mode,
            "http1": http1,
            "plugin_dir": self.s_plugin_dir,
            "limit_unit": self.limit_unit,
        });
        match set_settings(&self.gui, &args) {
            Ok(_) => {
                self.set_status("已保存设置", false);
                self.plugin_snapshot_at = Instant::now() - Duration::from_secs(3600);
            }
            Err(e) => self.set_status(e, true),
        }
    }

    /// 关闭行为：记住用户的选择并落盘。
    fn remember_close_action(&mut self, action: &str) {
        {
            let mut cfg = self.gui.cfg.lock().unwrap();
            cfg.close_action = action.to_string();
            let _ = cfg.save();
        }
        self.close_ask = None;
        if action == "tray" {
            self.set_status("已记住：关闭时最小化到托盘（下载继续）", false);
        } else {
            self.set_status("已记住：关闭时退出程序", false);
        }
    }

    /// 移除所有已完成的任务（只从列表里删，不碰磁盘文件）。
    ///
    /// 顶栏"后台任务"下拉里的「清空已完成」用它 —— 磁盘上的文件是用户
    /// 自己下的东西，清理界面不该顺手删掉。
    fn clear_finished(&mut self) {
        let mut tasks = self.gui.tasks.lock().unwrap();
        tasks.retain(|t| t.status != "completed");
    }

    fn run_action(&mut self, plugin: &str, action: &str) {
        let hooks = self.gui.hooks.lock().unwrap().clone();
        match hooks.run_action(plugin, action, &json!({})) {
            Some(v) => {
                let msg = v
                    .get("message")
                    .and_then(|x| x.as_str())
                    .unwrap_or("插件动作已执行")
                    .to_string();
                self.set_status(msg, false);
                self.reload_plugin_view();
            }
            None => self.set_status("插件动作不可用（需要 trusted 模式）", true),
        }
    }

    fn run_command(&mut self, plugin: &str, command: &str) {
        let hooks = self.gui.hooks.lock().unwrap().clone();
        match hooks.run_command(plugin, command) {
            Some(v) => {
                let msg = v
                    .get("message")
                    .and_then(|x| x.as_str())
                    .unwrap_or("命令已执行")
                    .to_string();
                self.set_status(msg, false);
            }
            None => self.set_status("插件命令不可用（需要 trusted 模式）", true),
        }
        self.cmd_open = false;
    }

    fn save_plugin_setting(&mut self, plugin: &str, key: &str, value: &str) {
        if let Err(e) = save_plugin_setting(plugin, key, value) {
            self.set_status(e, true);
            return;
        }
        self.plugin_snapshot_at = Instant::now() - Duration::from_secs(3600);
    }
}

// ───────────────────────── 主题（来自 adl-ui） ─────────────────────────

/// 主题令牌的唯一来源是 `adl_ui::theme`，这里只做短名转发。
///
/// 以前这些是散在本文件里的 `const teal(): Color32 = ...`，想加深色模式
/// 或换配色得改几十处；现在换 `adl_ui::Palette` 一处即可。
fn theme() -> adl_ui::Theme {
    adl_ui::Theme::get()
}
fn pal() -> adl_ui::Palette {
    theme().palette
}
fn teal() -> egui::Color32 {
    pal().accent
}
fn muted() -> egui::Color32 {
    pal().muted
}
fn ink() -> egui::Color32 {
    pal().ink
}
fn sidebar_w() -> f32 {
    theme().spacing.sidebar_w
}
// 顶栏已并进主区外壳（不再是独立的 Panel），这个高度只作为
// chips 行的视觉参考值保留，避免主题里删字段引起大范围改动；
// 目前没有调用点，留着说明"这里曾经有一条顶栏"。
#[allow(dead_code)]
fn topbar_h() -> f32 {
    theme().spacing.topbar_h
}
fn titlebar_h() -> f32 {
    theme().spacing.titlebar_h
}

/// 自绘标题栏（白底、无描边）
fn titlebar_frame() -> egui::Frame {
    theme().titlebar_frame()
}

/// 整高侧边栏：白底
fn sidebar_frame() -> egui::Frame {
    theme().sidebar_frame()
}

/// 右侧主区外壳：浅底 + 左上圆角
fn shell_frame() -> egui::Frame {
    theme().shell_frame()
}

/// 设置页左侧标签格、表格单元格、导航项、顶栏按钮等已统一迁到
/// `adl_ui::widgets` / `adl_ui::table`，本文件只保留页面组装逻辑。

impl eframe::App for App {
    /// 窗口清屏色。
    ///
    /// 主区左上角是圆角（`shell_frame` 只切 `nw`），圆角外侧露出来的就是
    /// 这个清屏色。eframe 默认是半透明深色 `rgba(12,12,12,180)`，在浅色
    /// 界面里会变成一块黑三角 —— 所以必须显式给白，和侧栏/标题栏同色。
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        let c = pal().card;
        [
            c.r() as f32 / 255.0,
            c.g() as f32 / 255.0,
            c.b() as f32 / 255.0,
            1.0,
        ]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // 处理唤醒 / 电源事件
        if self.wake.swap(false, Ordering::SeqCst) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        if self.suspend.swap(false, Ordering::SeqCst) {
            pause_all(&self.gui);
        }
        if self.resume.swap(false, Ordering::SeqCst) {
            self.set_status("系统已从休眠恢复，如需继续请点「继续」", false);
        }

        // 关闭拦截：行为由配置决定（首次点关闭时问一次，可以记住）
        //
        // `closing` 是个"放行闸"——用户选了"退出程序"之后我们要再发一次 Close，
        // 如果不放行就会在这里被自己拦回去，永远关不掉。
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.closing {
                // 已在退出流程里，不再拦截
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                let action = self.gui.cfg.lock().unwrap().close_action.clone();
                match action.as_str() {
                    "close" => {
                        self.closing = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    "tray" => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                    }
                    _ => {
                        // 还没问过：弹窗让用户选，并允许记住
                        self.close_ask = Some(CloseAsk { remember: true });
                    }
                }
            }
        }

        // 托盘菜单事件（显示主界面 / 退出）
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            match ev.id.0.as_str() {
                "show" => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                "quit" => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                _ => {}
            }
        }

        // Ctrl+K 命令面板
        if ctx.input(|i| i.key_pressed(egui::Key::K) && i.modifiers.ctrl) {
            self.cmd_open = !self.cmd_open;
        }

        self.refresh_plugin_snapshot();

        // 无边框窗口：系统不再提供边缘拖拽，八向热区要自己铺。
        resize_borders(ui);

        // ===== 整体布局 =====
        // 自绘标题栏（整宽，含侧栏折叠按钮与三个窗口键）
        // → 左侧整高侧边栏
        // → 右侧一整块浅底"外壳"（左上圆角），里面是 chips 行 + 内容区。
        // 以前顶栏是白底独立一条，结果侧栏白 / 顶栏白 / 内容浅灰之间出现
        // 一条横向的白灰分界带；现在顶栏并进外壳的浅底里，只靠间距分层。
        egui::Panel::top("adl_titlebar")
            .exact_size(titlebar_h())
            .resizable(false)
            .show_separator_line(false)
            .frame(titlebar_frame())
            .show(ui, |ui| self.titlebar(ui));

        // 侧栏：收起时**整个隐藏**，不保留一条窄轨。
        //
        // 一开始做成"收窄到 56px 只剩首字"，实测那样很怪（首字导航既认不出
        // 也不好看）。现在收起 = 真的不要侧栏，重新展开靠标题栏最左上角那个
        // 汉堡键。
        if !self.sidebar_collapsed {
            egui::Panel::left("adl_sidebar")
                .exact_size(sidebar_w())
                .resizable(false)
                .show_separator_line(false)
                .frame(sidebar_frame())
                .show(ui, |ui| self.sidebar(ui));
        }

        // 状态提示条（6 秒后消失）——放在外壳正上方，不参与圆角
        //
        // 最后 0.45 秒做淡出，否则一条通栏色带会"啪"地消失，很突兀。
        // 淡出期间继续请求重绘，不然静止的窗口只会重绘一次就停住。
        if let Some((msg, err, at)) = &self.status {
            let el = at.elapsed().as_secs_f32();
            if el < STATUS_SECS {
                let p = pal();
                let (color, fill) = if *err {
                    (p.danger, p.err_bg)
                } else {
                    (egui::Color32::from_rgb(0x2E, 0x7D, 0x6B), p.ok_bg)
                };
                let alpha = ((STATUS_SECS - el) / STATUS_FADE).clamp(0.0, 1.0);
                if alpha < 1.0 {
                    ctx.request_repaint();
                }
                let text = msg.clone();
                let is_err = *err;
                egui::Panel::top("adl_statusbar")
                    .resizable(false)
                    .show_separator_line(false)
                    .frame(
                        egui::Frame::new()
                            .fill(fill.gamma_multiply(alpha))
                            .inner_margin(egui::Margin::symmetric(20, 6)),
                    )
                    .show(ui, |ui| {
                        ui.set_opacity(alpha);
                        ui.horizontal(|ui| {
                            ui.colored_label(color, if is_err { "!" } else { "OK" });
                            ui.colored_label(color, text);
                        });
                    });
            }
        }

        egui::CentralPanel::default()
            .frame(shell_frame())
            .show(ui, |ui| {
                self.topbar(ui);
                // 标题行与内容区之间一条浅黑细线。
                //
                // 全站别处都不用线分区（靠底色差），但这里内容会从上往下滚，
                // 需要一条看得见的边界提示"上面这一行是固定的"。用低透明度
                // 黑而不是主题灰绿 —— 灰绿在这层浅底上几乎看不见。
                ui.add_space(4.0);
                adl_ui::widgets::rule(ui);
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        match self.tab.clone() {
                            Tab::Home => self.page_home(ui),
                            Tab::Tasks => self.page_tasks(ui),
                            Tab::Settings => self.page_settings(ui),
                            Tab::Plugin(_, _) => self.page_plugin(ui),
                        }
                    });
            });

        // 弹窗淡入：先根据"当前开着哪个 overlay"确定身份，再决定透明度。
        let overlay = if self.add_open {
            1
        } else if self.close_ask.is_some() {
            2
        } else if self.cmd_open {
            3
        } else {
            0
        };
        self.fade.track(overlay);
        if self.fade.running(FADE_SECS) {
            ctx.request_repaint();
        }

        // 添加下载弹窗
        if self.add_open {
            self.show_add_modal(&ctx);
        }

        // 关闭行为首次询问
        if self.close_ask.is_some() {
            self.show_close_modal(&ctx);
        }

        // 命令面板
        if self.cmd_open {
            self.show_command_palette(&ctx);
        }

        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

impl App {
    /// 单个导航项：整宽、圆角、选中/悬停高亮（不用 emoji —— 中文字体里没有表情字形，会显示成方块）
    fn nav_item(&mut self, ui: &mut egui::Ui, selected: bool, label: &str) -> bool {
        adl_ui::widgets::nav_item(ui, selected, label)
    }

    /// 左侧整高边栏。
    ///
    /// 布局参考 DSH 的侧栏：最上面是 `ADL` 品牌字，接着是主操作按钮（新建下载），
    /// 然后是主导航 + 插件分组，最后把"设置"钉在左下角。
    /// 收起时整个侧栏不渲染（`Panel::left` 外面那层 `if` 决定），
    /// 品牌字由 `titlebar()` 在折叠键右侧补画，所以这里不处理收起态。
    fn sidebar(&mut self, ui: &mut egui::Ui) {
        // ── 品牌：只写 ADL ──
        // 早先是个青绿方块加一个字母 A，再配 "ADL / 下载器" 两行小字；
        // 用户觉得那个假图标不好看，改成纯文字。
        ui.add_space(8.0);
        ui.label(egui::RichText::new("ADL").size(19.0).strong().color(teal()));
        ui.add_space(10.0);

        // ── 主操作：新建下载（取代原来的"添加下载"导航项）──
        let btn_h = 34.0;
        let (btn_rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), btn_h), egui::Sense::hover());
        if adl_ui::widgets::small_button(ui, btn_rect, "+ 新建下载", true) {
            self.f_url.clear();
            self.f_out.clear();
            self.add_open = true;
        }
        ui.add_space(12.0);

        // ── 主导航 ──
        if self.nav_item(ui, self.tab == Tab::Home, "主页") {
            self.tab = Tab::Home;
        }
        ui.add_space(2.0);
        if self.nav_item(ui, self.tab == Tab::Tasks, "任务列表") {
            self.tab = Tab::Tasks;
        }

        // ── 插件：页面 + 最多 5 个设置入口，超出的走"更多设置" ──
        let pages = self
            .plugin_snapshot
            .get("pages")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        // 有设置的插件各算一个入口；最多在侧栏放 5 个，其余收进"更多"
        let mut setting_plugins: Vec<(String, String)> = Vec::new();
        if let Some(arr) = self.plugin_snapshot.get("settings").and_then(|x| x.as_array()) {
            for s in arr {
                let plugin = s.get("plugin").and_then(|x| x.as_str()).unwrap_or("");
                if plugin.is_empty() {
                    continue;
                }
                if !setting_plugins.iter().any(|(p, _)| p == plugin) {
                    setting_plugins.push((plugin.to_string(), plugin.to_string()));
                }
            }
        }
        let shown = setting_plugins.len().min(5);
        let has_more = setting_plugins.len() > shown;

        if !pages.is_empty() || !setting_plugins.is_empty() {
            ui.add_space(14.0);
            ui.label(egui::RichText::new("插件").size(11.5).color(muted()));
            ui.add_space(6.0);
            for p in pages {
                let plugin = p
                    .get("plugin")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let id = p.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let title = p
                    .get("title")
                    .and_then(|x| x.as_str())
                    .unwrap_or(&id)
                    .to_string();
                let selected =
                    matches!(&self.tab, Tab::Plugin(pl, pg) if pl == &plugin && pg == &id);
                if self.nav_item(ui, selected, &title) {
                    self.tab = Tab::Plugin(plugin.clone(), id.clone());
                    self.plugin_view = None;
                    self.reload_plugin_view();
                }
                ui.add_space(2.0);
            }
            for (plugin, name) in setting_plugins.iter().take(shown) {
                let tab_id = format!("settings:{plugin}");
                let selected =
                    matches!(&self.tab, Tab::Plugin(pl, pg) if pl == "adl" && pg == &tab_id);
                let label = format!("{name} 设置");
                if self.nav_item(ui, selected, &label) {
                    self.tab = Tab::Plugin("adl".to_string(), tab_id);
                }
                ui.add_space(2.0);
            }
            if has_more {
                if self.nav_item(ui, false, "更多设置…") {
                    self.tab = Tab::Settings;
                }
                ui.add_space(2.0);
            }
        }

        // ── 底部：设置 + 模式胶囊 + 快捷键提示 ──
        let trusted = self
            .plugin_snapshot
            .get("trusted")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new("Ctrl+K 命令面板")
                    .size(11.0)
                    .color(muted()),
            );
            ui.add_space(6.0);
            let (txt, fg, bg) = if trusted {
                (
                    "信任模式",
                    egui::Color32::from_rgb(0x8B, 0x5A, 0x10),
                    egui::Color32::from_rgb(0xFD, 0xF2, 0xDC),
                )
            } else {
                ("安全模式", teal(), egui::Color32::from_rgb(0xDD, 0xEE, 0xE9))
            };
            let (rect, _) = ui.allocate_exact_size(egui::vec2(76.0, 22.0), egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, egui::CornerRadius::same(11), bg);
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                txt,
                egui::FontId::proportional(12.0),
                fg,
            );
            ui.add_space(8.0);
            // 设置钉在左下角
            if self.nav_item(ui, self.tab == Tab::Settings, "设置") {
                self.tab = Tab::Settings;
            }
            ui.add_space(4.0);
        });
    }

    fn page_title(&self) -> String {
        match &self.tab {
            Tab::Home => "主页".to_string(),
            Tab::Tasks => "任务列表".to_string(),
            Tab::Settings => "设置".to_string(),
            Tab::Plugin(plugin, page) => {
                // 插件设置伪页面：`adl` + `settings:<pluginId>`
                if plugin == "adl" {
                    if let Some(pid) = page.strip_prefix("settings:") {
                        return format!("{pid} · 设置");
                    }
                }
                let p = page.clone();
                let pl = plugin.clone();
                self.plugin_snapshot
                    .get("pages")
                    .and_then(|x| x.as_array())
                    .and_then(|arr| {
                        arr.iter().find_map(|n| {
                            let same_plugin =
                                n.get("plugin").and_then(|x| x.as_str()) == Some(pl.as_str());
                            let same_page =
                                n.get("id").and_then(|x| x.as_str()) == Some(p.as_str());
                            if same_plugin && same_page {
                                n.get("title")
                                    .and_then(|x| x.as_str())
                                    .map(|s| s.to_string())
                            } else {
                                None
                            }
                        })
                    })
                    .unwrap_or_else(|| "插件页面".to_string())
            }
        }
    }

    /// 自绘标题栏：侧栏显隐键 + 可拖动空白区 + 最小化/最大化/关闭。
    ///
    /// 窗口用 `.with_decorations(false)` 关掉了系统标题栏，所以这里的拖动与
    /// 双击最大化必须自己实现（`ViewportCommand::StartDrag` / `Maximized`）。
    /// 拖动区只覆盖标题栏里除按钮外的部分，否则点按钮会变成拖窗口。
    ///
    /// 显隐键放最左上角（与用户要的第 5 张参考图一致）：这一栏与窗口三键
    /// 本该是同一个横条，而不是让侧栏顶部再单独占一行。
    fn titlebar(&mut self, ui: &mut egui::Ui) {
        // 跟随系统状态（用户用快捷键 Win+↑ 时也要同步）
        if let Some(m) = ui.ctx().input(|i| i.viewport().maximized) {
            self.maximized = m;
        }

        let band = ui.available_rect_before_wrap();
        let cy = band.center().y;

        // 折叠/展开侧栏
        let tw = 34.0;
        let toggle_rect = egui::Rect::from_center_size(
            egui::pos2(band.left() + tw / 2.0, cy),
            egui::vec2(tw, band.height().min(34.0)),
        );
        if adl_ui::widgets::sidebar_toggle(ui, toggle_rect, self.sidebar_collapsed) {
            self.sidebar_collapsed = !self.sidebar_collapsed;
            let mut cfg = self.gui.cfg.lock().unwrap();
            cfg.sidebar_collapsed = self.sidebar_collapsed;
            let _ = cfg.save();
        }

        // 侧栏隐藏时品牌字会跟着消失，所以在标题栏里补一个 "ADL"。
        // 侧栏显示时不画 —— 否则顶部会出现两个 ADL。
        if self.sidebar_collapsed {
            ui.painter().text(
                egui::pos2(toggle_rect.right() + 10.0, cy),
                egui::Align2::LEFT_CENTER,
                "ADL",
                egui::FontId::proportional(14.0),
                teal(),
            );
        }

        // 右侧三个窗口按钮，从右缘往左排
        let bw = 46.0;
        let bh = band.height().min(34.0);
        let close_rect = egui::Rect::from_center_size(
            egui::pos2(band.right() - bw / 2.0, cy),
            egui::vec2(bw, bh),
        );
        let max_rect = close_rect.translate(egui::vec2(-bw, 0.0));
        let min_rect = max_rect.translate(egui::vec2(-bw, 0.0));

        // 品牌与拖动区（折叠按钮右侧 → 最小化按钮左侧，留 6px 空隙）
        let drag_rect = egui::Rect::from_min_max(
            egui::pos2(toggle_rect.right() + 4.0, band.top()),
            egui::pos2(min_rect.left() - 6.0, band.bottom()),
        );
        let drag = ui.interact(
            drag_rect,
            ui.scope_id().with("adl_titlebar_drag"),
            egui::Sense::click_and_drag(),
        );
        if drag.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if drag.double_clicked() {
            self.toggle_maximize(ui.ctx());
        }

        // 品牌字：侧栏显示时在侧栏里，隐藏时在标题栏补一个（见上）
        let _ = drag_rect;

        if adl_ui::widgets::window_button(ui, min_rect, adl_ui::WindowButton::Minimize) {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        let kind = if self.maximized {
            adl_ui::WindowButton::Restore
        } else {
            adl_ui::WindowButton::Maximize
        };
        if adl_ui::widgets::window_button(ui, max_rect, kind) {
            self.toggle_maximize(ui.ctx());
        }
        if adl_ui::widgets::window_button(ui, close_rect, adl_ui::WindowButton::Close) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    fn toggle_maximize(&mut self, ctx: &egui::Context) {
        self.maximized = !self.maximized;
        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(self.maximized));
    }

    /// 顶栏：左侧页面标题，右侧一排状态 chip（参考 DSH 的顶栏）。
    ///
    /// 这些 chip 都是"可点的状态指示"，不是纯装饰：
    ///   任务列表 → 回任务页（当前就在任务页时高亮）
    ///   限速档位 → 弹菜单选预设，直接改 `cfg.limit`
    ///   并行线路 → 所有进行中任务的活跃连接数之和（有流量时点是青绿）
    ///   下载进度 → 剩余/已完成，悬停可看细节
    ///
    /// 不放"退出"按钮：标题栏右上角已经有系统级关闭键，重复摆两个关闭
    /// 入口会让人犹豫"这两个有什么区别"。
    fn topbar(&mut self, ui: &mut egui::Ui) {
        let title = self.page_title();
        let tasks = snapshot(&self.gui);

        // 限速：从配置读当前档位，chip 上直接显示
        let (limit, unit) = {
            let cfg = self.gui.cfg.lock().unwrap();
            (cfg.limit, cfg.limit_unit.clone())
        };
        let limit_label = if limit == 0 {
            // 不限速时叫"标准模式"，读起来比"不限速"更像一个档位名
            "标准模式".to_string()
        } else {
            format!("限速 {}", format_speed_unit(limit, &unit))
        };
        let conns: u32 = tasks
            .iter()
            .filter(|t| t.status == "downloading")
            .map(|t| t.connections)
            .sum();
        let running = tasks.iter().filter(|t| t.status == "downloading").count();
        let done = tasks.iter().filter(|t| t.status == "completed").count();
        let progress_label = if running > 0 {
            format!("剩余 {running} / 完成 {done}")
        } else {
            format!("完成 {done} 总计 {}", tasks.len())
        };

        let mut go_tasks = false;
        let mut picked_limit: Option<u64> = None;
        let mut clear_finished = false;

        ui.horizontal(|ui| {
            ui.add_space(2.0);
            ui.label(egui::RichText::new(title).size(16.0).color(ink()));

            // chip 靠右：`right_to_left` 里第一个 add 落在最右边，
            // 所以视觉顺序"限速 → 线路 → 总计"要**倒着 add**。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(2.0);

                // 总计 / 后台任务（点开是任务卡片列表）
                let pr =
                    adl_ui::widgets::icon_chip(ui, &progress_label, ChipIcon::Stack, false);
                pr.clone()
                    .on_hover_text(format!("进行中 {running}，已完成 {done}"));
                egui::Popup::menu(&pr).show(|ui| {
                    ui.set_min_width(340.0);
                    if tasks.is_empty() {
                        adl_ui::widgets::hint(ui, "还没有任务");
                        return;
                    }
                    // 未完成 / 失败的排前面，已完成的沉底
                    let rank = |s: &str| match s {
                        "downloading" => 0,
                        "paused" | "failed" => 1,
                        _ => 2,
                    };
                    let mut list: Vec<&TaskView> = tasks.iter().collect();
                    list.sort_by_key(|t| (rank(&t.status), t.id));
                    ui.horizontal(|ui| {
                        adl_ui::widgets::section_title(ui, "任务");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if done > 0 && ui.small_button("清空已完成").clicked() {
                                clear_finished = true;
                                ui.close();
                            }
                        });
                    });
                    for t in list.iter().take(12) {
                        if task_row(ui, t) {
                            go_tasks = true;
                            ui.close();
                        }
                    }
                    if list.len() > 12 {
                        adl_ui::widgets::hint(
                            ui,
                            &format!("还有 {} 个，去任务列表看", list.len() - 12),
                        );
                    }
                });

                // 并行线路
                let lines_label = format!("{conns} 条线路");
                adl_ui::widgets::icon_chip(ui, &lines_label, ChipIcon::Lines, false)
                    .on_hover_text("当前所有下载任务加起来的活跃连接数（服务器会限流）");

                // 限速档位 + 菜单
                let lr = adl_ui::widgets::icon_chip(ui, &limit_label, ChipIcon::Gauge, false);
                lr.clone()
                    .on_hover_text("点这里切换限速档位（也可以去设置里改精确值）");
                egui::Popup::menu(&lr).show(|ui| {
                    ui.set_min_width(160.0);
                    for (v, label) in [
                        (0u64, "不限速"),
                        (1 << 20, "1 MB/s"),
                        (2 << 20, "2 MB/s"),
                        (5 << 20, "5 MB/s"),
                        (10 << 20, "10 MB/s"),
                    ] {
                        if ui.selectable_label(limit == v, label).clicked() {
                            picked_limit = Some(v);
                            ui.close();
                        }
                    }
                });
            });
        });

        if go_tasks {
            self.tab = Tab::Tasks;
        }
        if clear_finished {
            self.clear_finished();
            self.set_status("已清空已完成的任务", false);
        }
        if let Some(v) = picked_limit {
            {
                let mut cfg = self.gui.cfg.lock().unwrap();
                cfg.limit = v;
                let _ = cfg.save();
            }
            self.s_limit = v;
            self.set_status(
                if v == 0 {
                    "已切换为不限速".to_string()
                } else {
                    format!("限速已设为 {}", format_speed_unit(v, &unit))
                },
                false,
            );
        }
    }

    fn page_tasks(&mut self, ui: &mut egui::Ui) {
        ui.add_space(2.0);
        // 工具栏动作（插件）
        let actions: Vec<(String, String, String)> = self
            .plugin_snapshot
            .get("actions")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter(|a| a.get("place").and_then(|x| x.as_str()) == Some("toolbar"))
                    .filter_map(|a| {
                        Some((
                            a.get("plugin")?.as_str()?.to_string(),
                            a.get("id")?.as_str()?.to_string(),
                            a.get("label")?.as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !actions.is_empty() {
            ui.horizontal(|ui| {
                for (plugin, action, label) in actions {
                    if ui.button(label).clicked() {
                        self.run_action(&plugin, &action);
                    }
                }
            });
        }

        let tasks = snapshot(&self.gui);
        if tasks.is_empty() {
            adl_ui::widgets::empty_state(ui, "还没有下载任务，点左上角「+ 新建下载」开始。");
            return;
        }
        ui.add_space(8.0);

        // 列定义搬到 adl-ui：固定宽 + 一列吃剩余 + 单行截断。
        // 以前这里 170 行手写水平布局与 table_cell(..) 胶水，现在只有表与行。
        let cols = [
            adl_ui::Column::new("状态", 72.0),
            adl_ui::Column::remainder("文件"),
            adl_ui::Column::new("进度", 150.0),
            adl_ui::Column::new("速度", 92.0),
            adl_ui::Column::new("大小", 82.0),
            adl_ui::Column::new("操作", 104.0),
        ];

        let mut pause_id: Option<u64> = None;
        let mut remove_id: Option<u64> = None;

        adl_ui::table(ui, &cols, tasks.len(), |ui, i, w| {
            let t = &tasks[i];
            let (txt, color) = status_style(&t.status);

            adl_ui::widgets::cell(ui, w[0], |ui| {
                let r = ui.label(egui::RichText::new(txt).color(color));
                if t.status == "failed" {
                    let msg = t.error.as_deref().unwrap_or("失败");
                    r.on_hover_text(msg);
                }
            });

            let name = display_name(t);
            adl_ui::widgets::cell(ui, w[1], |ui| {
                adl_ui::table::truncated(ui, &name, &t.url);
            });

            let frac = if t.total > 0 {
                (t.completed as f32 / t.total as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };
            // 失败原因不塞进"进度"列（150px 装不下，实测被截成"资源不存在（HTTP 4…"），
            // 改成在进度列给一行短标记，完整原因挂到状态列的悬停提示上。
            adl_ui::widgets::cell(ui, w[2], |ui| {
                if t.status == "completed" {
                    ui.label(egui::RichText::new("已完成").color(teal()));
                } else if t.status == "failed" {
                    ui.label(
                        egui::RichText::new("已中断")
                            .color(egui::Color32::from_rgb(0xB0, 0x30, 0x30)),
                    );
                } else {
                    ui.add(
                        egui::ProgressBar::new(frac)
                            .desired_width(w[2] - 8.0)
                            .fill(teal())
                            .show_percentage(),
                    );
                }
            });

            adl_ui::widgets::cell(ui, w[3], |ui| {
                let s = if t.speed > 0 {
                    format_speed(t.speed)
                } else {
                    "—".to_string()
                };
                ui.label(egui::RichText::new(s).color(muted()));
            });

            adl_ui::widgets::cell(ui, w[4], |ui| {
                ui.label(format_bytes(t.total));
            });

            adl_ui::widgets::cell(ui, w[5], |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if t.status == "downloading" && ui.small_button("暂停").clicked() {
                        pause_id = Some(t.id);
                    }
                    if ui.small_button("删除").clicked() {
                        remove_id = Some(t.id);
                    }
                });
            });
        });

        if let Some(id) = pause_id {
            let _ = pause_task(&self.gui, &json!({"id": id}));
        }
        if let Some(id) = remove_id {
            let _ = remove_task(&self.gui, &json!({"id": id, "files": false}));
        }
    }

    /// 添加下载：改成居中弹窗（取代原来的整页表单）。
    ///
    /// 弹窗里两个输入框撑满宽度（URL 很长，不适合 form() 的定宽控件格），
    /// 回车（或在地址框里 Enter）直接开始；Esc / 点背板 = 取消。
    fn show_add_modal(&mut self, ctx: &egui::Context) {
        let mut start = false;
        let mut cancel = false;

        let a = self.fade.alpha(FADE_SECS);
        let resp = egui::Modal::new(egui::Id::unique("adl_add_modal"))
            .frame(fade_frame(ctx, a))
            .show(ctx, |ui| {
            // 淡入：`Modal` 自己没有透明度参数，而且背板是在 `Area::show` 里
            // 最先画的（`set_opacity` 只影响之后画的图元），所以这里只能让
            // 内容本身淡入 —— 观感上足够了，背板瞬间变暗反而像"已经打开"。
            ui.set_opacity(a);
            ui.set_width(520.0);
            adl_ui::widgets::section_title(ui, "新建下载");
            ui.add_space(12.0);

            ui.label("下载地址");
            ui.add_space(4.0);
            let url_edit = ui.add(
                egui::TextEdit::singleline(&mut self.f_url)
                    .hint_text("https://…")
                    .desired_width(f32::INFINITY),
            );
            if url_edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                start = true;
            }

            ui.add_space(12.0);
            ui.label("保存文件名");
            ui.add_space(4.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.f_out)
                    .hint_text("可选，留空自动从地址 / 响应头推断，例如 ubuntu.iso")
                    .desired_width(f32::INFINITY),
            );

            ui.add_space(16.0);
            ui.horizontal(|ui| {
                if ui.button("开始下载").clicked() {
                    start = true;
                }
                if ui.button("取消").clicked() {
                    cancel = true;
                }
            });
            ui.add_space(8.0);
            adl_ui::widgets::hint(
                ui,
                "浏览器插件接管时会自动带着来源、Cookie 等，无需手动填。",
            );
        });

        if resp.should_close() {
            cancel = true;
        }
        if start {
            self.do_add();
        } else if cancel {
            self.add_open = false;
        }
    }

    /// 首次点关闭时问一次"退出还是最小化到托盘"。
    ///
    /// 用户明确要求：必须能记住选择，否则每次都弹窗会很烦。
    /// 勾了"记住"就写进 config.toml；不勾则只对本次生效。
    fn show_close_modal(&mut self, ctx: &egui::Context) {
        let mut choice: Option<&str> = None;
        let mut cancel = false;

        let a = self.fade.alpha(FADE_SECS);
        let resp = egui::Modal::new(egui::Id::unique("adl_close_modal"))
            .frame(fade_frame(ctx, a))
            .show(ctx, |ui| {
            ui.set_opacity(a);
            ui.set_width(400.0);
            adl_ui::widgets::section_title(ui, "关闭窗口时怎么做？");
            ui.add_space(10.0);
            adl_ui::widgets::hint(
                ui,
                "你现在还有没下完的任务。选「最小化到托盘」下载会继续跑；\
                 选「退出程序」会先保存进度再退出，下次可以接着下。",
            );
            ui.add_space(14.0);

            ui.horizontal(|ui| {
                if ui.button("最小化到托盘").clicked() {
                    choice = Some("tray");
                }
                if ui.button("退出程序").clicked() {
                    choice = Some("close");
                }
                if ui.button("取消").clicked() {
                    cancel = true;
                }
            });

            ui.add_space(10.0);
            if let Some(ask) = self.close_ask.as_mut() {
                ui.checkbox(&mut ask.remember, "记住我的选择（以后不再询问）");
            }
        });

        if resp.should_close() {
            cancel = true;
        }
        if cancel {
            self.close_ask = None;
            return;
        }
        if let Some(action) = choice {
            let remember = self.close_ask.as_ref().map(|a| a.remember).unwrap_or(false);
            if remember {
                self.remember_close_action(action);
            } else {
                self.close_ask = None;
                if action == "tray" {
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                } else {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    /// 主页：四个统计卡片 + 下载热力图（周 / 月 / 年）。
    ///
    /// 数据源是 `history.json` 的 `started_unix`，所以只统计"走完整个流程"
    /// 的任务（包括失败的）—— 中断后没记账的不在内，这与任务列表口径一致。
    fn page_home(&mut self, ui: &mut egui::Ui) {
        ui.add_space(2.0);
        let tasks = snapshot(&self.gui);
        let total = tasks.len();
        let done = tasks.iter().filter(|t| t.status == "completed").count();
        let failed = tasks.iter().filter(|t| t.status == "failed").count();
        let bytes: u64 = tasks.iter().filter(|t| t.status == "completed").map(|t| t.total).sum();

        // 四张统计卡：宽度必须"刚好填满一行"。
        //
        // 之前写成 `(avail - gap*3)/4` 然后又每张后面 `add_space(gap)`，
        // 于是 4 张卡吃了 4 份间距而不是 3 份，最右那张被裁掉半个。
        // 这里改成把间距交给 `item_spacing`：horizontal 只会在**元素之间**
        // 插间距，n 个元素天然只有 n-1 个间隙。
        let gap = 10.0;
        let n = 4usize;
        let avail = ui.available_width();
        let w = ((avail - gap * (n as f32 - 1.0)) / n as f32 - 1.0).max(80.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (val, label, tone) in [
                (format!("{total}"), "任务总数", ink()),
                (format!("{done}"), "已完成", teal()),
                (
                    format!("{failed}"),
                    "失败",
                    egui::Color32::from_rgb(0xB0, 0x30, 0x30),
                ),
                (format_bytes(bytes), "累计下载", ink()),
            ] {
                adl_ui::widgets::cell(ui, w, |ui| {
                    adl_ui::widgets::stat_card(ui, &val, label, tone);
                });
            }
        });

        ui.add_space(16.0);
        adl_ui::widgets::card(ui, |ui| {
            ui.horizontal(|ui| {
                adl_ui::widgets::section_title(ui, "下载热力图");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let items = [
                        (Range::Year, "年"),
                        (Range::Month, "月"),
                        (Range::Week, "周"),
                    ];
                    if let Some(r) = adl_ui::widgets::segmented(ui, self.home_range, &items) {
                        self.home_range = r;
                    }
                });
            });
            ui.add_space(10.0);

            // 历史里每个任务落在"哪一天"：按范围铺格子
            let hist = adl_app::history::History::load();
            let now = adl_app::history::now_unix();
            let day = 86_400u64;
            let cols = match self.home_range {
                Range::Week => 7,
                Range::Month => 7,
                Range::Year => 53,
            };
            let rows = match self.home_range {
                Range::Week => 1,
                Range::Month => 5,
                Range::Year => 7,
            };
            let start_days_ago = (cols * rows) as u64 - 1;
            // 先按网格顺序铺满 0，再用历史里的 started_unix 累加计数
            let mut counts = vec![0u32; cols * rows];
            for t in &hist.tasks {
                if t.started_unix == 0 {
                    continue;
                }
                let ago_days = now.saturating_sub(t.started_unix) / day;
                if ago_days > start_days_ago {
                    continue;
                }
                let idx = (start_days_ago - ago_days) as usize; // 0 = 最早
                // 年视图：一列一周、一行一天；周/月视图：一行一周、一格一天
                let i = if matches!(self.home_range, Range::Year) {
                    (idx / 7) * rows + (idx % 7)
                } else {
                    idx
                };
                if i < counts.len() {
                    counts[i] += 1;
                }
            }
            let cells: Vec<(usize, usize, u8)> = counts
                .iter()
                .enumerate()
                .map(|(i, &n)| {
                    let level = match n {
                        0 => 0,
                        1 => 1,
                        2..=3 => 2,
                        4..=7 => 3,
                        _ => 4,
                    };
                    if matches!(self.home_range, Range::Year) {
                        (i / rows, i % rows, level)
                    } else {
                        (i % cols, i / cols, level)
                    }
                })
                .collect();
            adl_ui::widgets::heatmap(ui, &cells, cols, rows);

            ui.add_space(8.0);
            let label = match self.home_range {
                Range::Week => "最近 7 天",
                Range::Month => "最近 5 周",
                Range::Year => "最近 1 年",
            };
            adl_ui::widgets::hint(
                ui,
                &format!("{label}每天启动的下载数量，颜色越深越多（数据来自下载历史）。"),
            );
        });
    }

    fn page_settings(&mut self, ui: &mut egui::Ui) {
        // 标题已在顶栏显示
        ui.add_space(2.0);

        // ── 并发：只给档位，不给自由输入 ──
        // 用户要求"最大值 + 固定 1/2/4/8/16/32/64，本质还是自适应"：
        // 这里选的是**上限**，引擎会按服务器实际放行量自动收敛。
        let mut save_conn = false;
        adl_ui::widgets::card(ui, |ui| {
            adl_ui::widgets::section_title(ui, "并发上限");
            ui.add_space(4.0);
            let opts: [(u64, &str); 7] = [
                (1, "1"),
                (2, "2"),
                (4, "4"),
                (8, "8"),
                (16, "16"),
                (32, "32"),
                (64, "64"),
            ];
            if let Some(v) = adl_ui::widgets::segmented(ui, self.s_conn, &opts) {
                self.s_conn = v;
                save_conn = true;
            }
            ui.add_space(6.0);
            adl_ui::widgets::hint(
                ui,
                "服务器通常不会给满（常见 16 只放 9）。引擎会按实际放行量自动收敛，\
                 稳定后偶尔试探 +1，被拒就退回并把试探间隔翻倍，避免触发反爬。",
            );
        });
        if save_conn {
            let mut cfg = self.gui.cfg.lock().unwrap();
            cfg.connections = adl_core::clamp_concurrency(self.s_conn as usize);
            let _ = cfg.save();
        }

        ui.add_space(12.0);

        // ── 限速：数值 + 单位选择（英文单位，中文读起来别扭）──
        let mut save_limit = false;
        adl_ui::widgets::card(ui, |ui| {
            adl_ui::widgets::section_title(ui, "限速");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let unit_scale = unit_scale_of(&self.limit_unit);
                let mut shown = self.s_limit / unit_scale.max(1) as u64;
                let resp = ui.add(
                    egui::DragValue::new(&mut shown)
                        .speed(1.0)
                        .range(0..=10_000u64),
                );
                if resp.changed() {
                    self.s_limit = shown.saturating_mul(unit_scale.max(1) as u64);
                    save_limit = true;
                }
                let units: [(&str, &str); 5] = [
                    ("auto", "自动"),
                    ("B/s", "B/s"),
                    ("KB/s", "KB/s"),
                    ("MB/s", "MB/s"),
                    ("GB/s", "GB/s"),
                ];
                let mut cur = self.limit_unit.clone();
                if cur.is_empty() {
                    cur = "auto".to_string();
                }
                egui::ComboBox::from_id_salt("adl_limit_unit")
                    .width(96.0)
                    .selected_text(units.iter().find(|(v, _)| *v == cur).map(|(_, l)| *l).unwrap_or("自动"))
                    .show_ui(ui, |ui| {
                        for (v, l) in units {
                            if ui.selectable_value(&mut cur, v.to_string(), l).clicked() {
                                self.limit_unit = v.to_string();
                                save_limit = true;
                            }
                        }
                    });
                ui.label(
                    egui::RichText::new(if self.s_limit == 0 {
                        "不限速".to_string()
                    } else {
                        format_speed_unit(self.s_limit, &self.limit_unit)
                    })
                    .color(muted()),
                );
            });
            ui.add_space(6.0);
            adl_ui::widgets::hint(
                ui,
                "0 = 不限速。限速是全局上限，所有任务共享同一个令牌桶。",
            );
        });
        if save_limit {
            let mut cfg = self.gui.cfg.lock().unwrap();
            cfg.limit = self.s_limit;
            cfg.limit_unit = self.limit_unit.clone();
            let _ = cfg.save();
        }

        ui.add_space(12.0);

        // ── 其它：目录与代理 ──
        let mut fields = vec![
            adl_ui::Field::text("默认下载目录", &mut self.s_dir).hint("留空 = 当前目录"),
            adl_ui::Field::text("HTTP 代理", &mut self.s_proxy)
                .hint("http://127.0.0.1:7890（留空 = 直连）"),
        ];
        adl_ui::form(ui, "其它", &mut fields);

        ui.add_space(10.0);
        let mode_opts: [(&str, &str); 2] = [
            ("safe", "安全（只读观察 / 拦截）"),
            ("trusted", "信任（可改写请求 / 替换界面）"),
        ];
        let mut plug_fields = vec![
            adl_ui::Field::select("插件模式", &mut self.s_mode, &mode_opts)
                .note("信任模式下插件才能改写请求、注册按钮与替换内置页面"),
            adl_ui::Field::text("插件目录", &mut self.s_plugin_dir)
                .hint("留空 = exe 同级 plugins"),
        ];
        adl_ui::form(ui, "插件", &mut plug_fields);

        // ── 关闭行为 ──
        // 只有两个选项：问过一次之后就定下来了，"每次询问"不再是可选状态
        // （用户明确要求：选了就不能调回询问）。
        ui.add_space(10.0);
        adl_ui::widgets::card(ui, |ui| {
            adl_ui::widgets::section_title(ui, "关闭窗口时");
            ui.add_space(6.0);
            let cur = self.gui.cfg.lock().unwrap().close_action.clone();
            let known = matches!(cur.as_str(), "tray" | "close");
            let cur_s = if known { cur.clone() } else { "tray".to_string() };
            let opts: [(&str, &str); 2] = [("tray", "最小化到托盘"), ("close", "退出程序")];
            let mut picked: Option<String> = None;
            ui.horizontal(|ui| {
                for (v, l) in opts {
                    if ui.selectable_label(cur_s == v, l).clicked() {
                        picked = Some(v.to_string());
                    }
                }
            });
            if let Some(v) = picked {
                if !known || v != cur_s {
                    let mut cfg = self.gui.cfg.lock().unwrap();
                    cfg.close_action = v;
                    let _ = cfg.save();
                }
            }
            ui.add_space(6.0);
            adl_ui::widgets::hint(
                ui,
                if known {
                    "第一次点关闭时问过一次，之后按这里的设置走。选「最小化到托盘」下载继续跑；\
                     选「退出程序」会先存进度，下次可续传。"
                } else {
                    "第一次点关闭时会让你选一次并记住；在那之前这里先显示默认值「最小化到托盘」。"
                },
            );
        });

        ui.add_space(14.0);
        let mut save = false;
        adl_ui::widgets::card(ui, |ui| {
            if ui.button("保存设置").clicked() {
                save = true;
            }
        });
        if save {
            self.do_save_settings();
        }

        // 插件设置区
        let settings = self
            .plugin_snapshot
            .get("settings")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        if !settings.is_empty() {
            ui.add_space(16.0);
            let mut pending: Vec<(String, String, String)> = Vec::new();
            adl_ui::widgets::card(ui, |ui| {
                ui.label(
                    egui::RichText::new("插件设置")
                        .size(15.0)
                        .strong()
                        .color(teal()),
                );
                ui.add_space(8.0);
                // 每行：标签格 + 控件格 + 只读标记，全部走 adl-ui 的定宽单元格，
                // 不再手写 Grid + end_row。
                for s in &settings {
                    let plugin = s.get("plugin").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    let key = s.get("key").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    let label = s.get("label").and_then(|x| x.as_str()).unwrap_or(&key).to_string();
                    let kind = s.get("kind").and_then(|x| x.as_str()).unwrap_or("string").to_string();
                    let value = s
                        .get("value")
                        .and_then(|x| x.as_str())
                        .map(|v| v.to_string())
                        .or_else(|| s.get("default").and_then(|x| x.as_str()).map(|v| v.to_string()))
                        .unwrap_or_default();
                    let editable = s.get("editable").and_then(|x| x.as_bool()).unwrap_or(false);

                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 12.0;
                        adl_ui::widgets::label_cell(ui, 210.0, &label);
                        adl_ui::widgets::cell(ui, 260.0, |ui| match kind.as_str() {
                            "boolean" => {
                                let mut b = value == "true";
                                if ui
                                    .add_enabled(editable, egui::Checkbox::new(&mut b, ""))
                                    .changed()
                                {
                                    pending.push((plugin.clone(), key.clone(), b.to_string()));
                                }
                            }
                            "select" => {
                                let opts: Vec<String> = s
                                    .get("options")
                                    .and_then(|x| x.as_array())
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|o| o.as_str().map(|s| s.to_string()))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                let mut cur = value.clone();
                                ui.add_enabled_ui(editable, |ui| {
                                    egui::ComboBox::from_id_salt(format!("{plugin}.{key}"))
                                        .width(240.0)
                                        .selected_text(cur.clone())
                                        .show_ui(ui, |ui| {
                                            for o in &opts {
                                                ui.selectable_value(&mut cur, o.clone(), o.clone());
                                            }
                                        });
                                });
                                if cur != value {
                                    pending.push((plugin.clone(), key.clone(), cur));
                                }
                            }
                            _ => {
                                let buf = self
                                    .plugin_edit
                                    .entry((plugin.clone(), key.clone()))
                                    .or_insert_with(|| value.clone());
                                if ui
                                    .add_enabled(
                                        editable,
                                        egui::TextEdit::singleline(buf).desired_width(240.0),
                                    )
                                    .changed()
                                {
                                    pending.push((plugin.clone(), key.clone(), buf.clone()));
                                }
                            }
                        });
                        if !editable {
                            adl_ui::widgets::hint(ui, "（只读）");
                        }
                    });
                }
            });
            for (p, k, v) in pending {
                self.save_plugin_setting(&p, &k, &v);
            }
        }
    }

    fn page_plugin(&mut self, ui: &mut egui::Ui) {
        let (plugin, page) = match self.tab.clone() {
            Tab::Plugin(p, g) => (p, g),
            _ => return,
        };

        // 侧栏"插件设置"入口用的是伪页面 `adl` + `settings:<pluginId>`：
        // 它不是插件自己渲染的页，而是宿主把该插件的 settings 项单独列出来。
        if plugin == "adl" {
            if let Some(pid) = page.strip_prefix("settings:") {
                self.page_plugin_settings(ui, pid);
                return;
            }
        }

        ui.add_space(2.0);
        let mut dispatch: Vec<Dispatch> = Vec::new();
        if self.plugin_view.is_none() {
            self.reload_plugin_view();
        }
        if let Some(tree) = self.plugin_view.clone() {
            if tree.get("type").and_then(|x| x.as_str()) == Some("error") {
                let msg = tree.get("error").and_then(|x| x.as_str()).unwrap_or("插件出错");
                ui.colored_label(egui::Color32::from_rgb(0xB0, 0x30, 0x30), msg);
            } else {
                render_node(ui, &plugin, &tree, &mut dispatch);
            }
        } else {
            ui.label("（插件没有返回内容）");
        }
        for d in dispatch {
            match d {
                Dispatch::Action { plugin, action } => self.run_action(&plugin, &action),
                Dispatch::Setting { plugin, key, value } => self.save_plugin_setting(&plugin, &key, &value),
            }
        }
    }

    /// 单个插件的设置页（侧栏入口点进来的那个）。
    fn page_plugin_settings(&mut self, ui: &mut egui::Ui, plugin_id: &str) {
        ui.add_space(2.0);
        let settings: Vec<Value> = self
            .plugin_snapshot
            .get("settings")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter(|s| s.get("plugin").and_then(|x| x.as_str()) == Some(plugin_id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        if settings.is_empty() {
            adl_ui::widgets::empty_state(ui, "这个插件没有声明可配置项。");
            return;
        }

        let mut pending: Vec<(String, String, String)> = Vec::new();
        adl_ui::widgets::card(ui, |ui| {
            for s in &settings {
                let plugin = s.get("plugin").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let key = s.get("key").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let label = s.get("label").and_then(|x| x.as_str()).unwrap_or(&key).to_string();
                let kind = s.get("kind").and_then(|x| x.as_str()).unwrap_or("string").to_string();
                let value = s
                    .get("value")
                    .and_then(|x| x.as_str())
                    .map(|v| v.to_string())
                    .or_else(|| s.get("default").and_then(|x| x.as_str()).map(|v| v.to_string()))
                    .unwrap_or_default();
                let editable = s.get("editable").and_then(|x| x.as_bool()).unwrap_or(false);
                let note = s.get("note").and_then(|x| x.as_str()).map(|v| v.to_string());

                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;
                    adl_ui::widgets::label_cell(ui, 210.0, &label);
                    adl_ui::widgets::cell(ui, 280.0, |ui| match kind.as_str() {
                        "boolean" => {
                            let mut b = value == "true";
                            if ui
                                .add_enabled(editable, egui::Checkbox::new(&mut b, ""))
                                .changed()
                            {
                                pending.push((plugin.clone(), key.clone(), b.to_string()));
                            }
                        }
                        "select" => {
                            let opts: Vec<String> = s
                                .get("options")
                                .and_then(|x| x.as_array())
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|o| o.as_str().map(|s| s.to_string()))
                                        .collect()
                                })
                                .unwrap_or_default();
                            let mut cur = value.clone();
                            ui.add_enabled_ui(editable, |ui| {
                                egui::ComboBox::from_id_salt(format!("page.{plugin}.{key}"))
                                    .width(260.0)
                                    .selected_text(cur.clone())
                                    .show_ui(ui, |ui| {
                                        for o in &opts {
                                            ui.selectable_value(&mut cur, o.clone(), o.clone());
                                        }
                                    });
                            });
                            if cur != value {
                                pending.push((plugin.clone(), key.clone(), cur));
                            }
                        }
                        _ => {
                            let buf = self
                                .plugin_edit
                                .entry((plugin.clone(), key.clone()))
                                .or_insert_with(|| value.clone());
                            if ui
                                .add_enabled(
                                    editable,
                                    egui::TextEdit::singleline(buf).desired_width(260.0),
                                )
                                .changed()
                            {
                                pending.push((plugin.clone(), key.clone(), buf.clone()));
                            }
                        }
                    });
                    if !editable {
                        adl_ui::widgets::hint(ui, "（信任模式下才可改）");
                    }
                });
                if let Some(n) = note {
                    ui.horizontal(|ui| {
                        adl_ui::widgets::label_cell(ui, 210.0, "");
                        adl_ui::widgets::hint(ui, &n);
                    });
                }
                ui.add_space(8.0);
            }
        });

        for (p, k, v) in pending {
            self.save_plugin_setting(&p, &k, &v);
        }
    }

    fn show_command_palette(&mut self, ctx: &egui::Context) {
        let commands: Vec<(String, String, String)> = self
            .plugin_snapshot
            .get("commands")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| {
                        Some((
                            c.get("plugin")?.as_str()?.to_string(),
                            c.get("id")?.as_str()?.to_string(),
                            c.get("title")?.as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut open = self.cmd_open;
        let mut chosen: Option<(String, String)> = None;
        egui::Window::new("命令面板")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 120.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.set_opacity(self.fade.alpha(FADE_SECS));
                ui.add(
                    egui::TextEdit::singleline(&mut self.cmd_query)
                        .hint_text("搜索命令…")
                        .desired_width(320.0),
                );
                let q = self.cmd_query.to_lowercase();
                ui.separator();
                for (plugin, id, title) in &commands {
                    if !q.is_empty() && !title.to_lowercase().contains(&q) {
                        continue;
                    }
                    if ui.button(title).clicked() {
                        chosen = Some((plugin.clone(), id.clone()));
                    }
                }
                if commands.is_empty() {
                    ui.label(egui::RichText::new("当前没有插件命令（trusted 模式可用）").weak());
                }
            });
        self.cmd_open = open;
        if let Some((p, id)) = chosen {
            self.run_command(&p, &id);
        }
    }
}

// ───────────────────────── 无边框窗口的边缘缩放 ─────────────────────────

/// 无边框窗口的边缘缩放热区。
///
/// `with_decorations(false)` 之后，Windows 不再提供 `WS_SIZEBOX` 那种边缘命中
/// 测试（winit 的 `WM_NCCALCSIZE` 把客户区铺满整个非客户区），所以八个方向的
/// 缩放必须自己铺：沿四边画不可见的热区，按住时发 `BeginResize`。
///
/// 热区宽度取 5px：再宽会挡住内容区的边缘控件，再窄鼠标不好瞄。
fn resize_borders(ui: &mut egui::Ui) {
    const G: f32 = 5.0;
    use egui::viewport::ResizeDirection as Dir;

    let full = ui.ctx().input(|i| i.viewport_rect());

    // (矩形, 方向, 光标)
    let items: [(egui::Rect, Dir, egui::CursorIcon); 8] = [
        (
            egui::Rect::from_min_max(full.left_top(), egui::pos2(full.left() + G, full.bottom())),
            Dir::West,
            egui::CursorIcon::ResizeHorizontal,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(full.right() - G, full.top()),
                full.right_bottom(),
            ),
            Dir::East,
            egui::CursorIcon::ResizeHorizontal,
        ),
        (
            egui::Rect::from_min_max(full.left_top(), egui::pos2(full.right(), full.top() + G)),
            Dir::North,
            egui::CursorIcon::ResizeVertical,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(full.left(), full.bottom() - G),
                full.right_bottom(),
            ),
            Dir::South,
            egui::CursorIcon::ResizeVertical,
        ),
        (
            egui::Rect::from_min_max(full.left_top(), egui::pos2(full.left() + G, full.top() + G)),
            Dir::NorthWest,
            egui::CursorIcon::ResizeNorthWest,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(full.right() - G, full.top()),
                egui::pos2(full.right(), full.top() + G),
            ),
            Dir::NorthEast,
            egui::CursorIcon::ResizeNorthEast,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(full.left(), full.bottom() - G),
                egui::pos2(full.left() + G, full.bottom()),
            ),
            Dir::SouthWest,
            egui::CursorIcon::ResizeSouthWest,
        ),
        (
            egui::Rect::from_min_max(
                egui::pos2(full.right() - G, full.bottom() - G),
                full.right_bottom(),
            ),
            Dir::SouthEast,
            egui::CursorIcon::ResizeSouthEast,
        ),
    ];

    for (i, (rect, dir, cursor)) in items.into_iter().enumerate() {
        let resp = ui.interact(
            rect,
            ui.make_persistent_id(("adl_resize_hot", i)),
            egui::Sense::drag(),
        );
        if resp.hovered() {
            ui.ctx().set_cursor_icon(cursor);
        }
        if resp.drag_started() {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::BeginResize(dir));
        }
    }
}

// ───────────────────────── 插件组件树渲染 ─────────────────────────

/// 取字符串字段（缺失/类型不符都当空串）。
fn s_of<'a>(node: &'a Value, k: &str) -> &'a str {
    node.get(k).and_then(|x| x.as_str()).unwrap_or("")
}
/// 取数字字段。
fn f_of(node: &Value, k: &str) -> Option<f64> {
    node.get(k).and_then(|x| x.as_f64())
}
/// 取布尔字段。
fn b_of(node: &Value, k: &str, default: bool) -> bool {
    node.get(k).and_then(|x| x.as_bool()).unwrap_or(default)
}
/// 取字符串数组。
fn sa_of(node: &Value, k: &str) -> Vec<String> {
    node.get(k)
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|o| o.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// 语义色：把插件写的 `tone` 名字翻成主题色。
///
/// 插件**不能**直接传 RGB —— 那样换主题/深色模式时插件会变成一块突兀的
/// 硬编码色。只允许语义名，颜色归主题管。
fn tone_color(tone: &str) -> egui::Color32 {
    let p = pal();
    match tone {
        "accent" | "primary" | "ok" | "completed" => p.accent,
        "warn" | "paused" => p.warn,
        "danger" | "error" | "failed" => p.danger,
        _ => p.muted,
    }
}

/// 插件声明的组件树 → egui。
///
/// 设计约束（与 DESIGN.md §5.2 一致）：插件只能返回**声明式 JSON**，
/// 绝不返回 HTML/JS。这里每个 `type` 都是白名单里的一项，未识别的一律
/// 画成 `[xxx]` 而不是报错 —— 这样新版插件在旧版 ADL 上只是少画点东西，
/// 不会整页崩掉。
fn render_node(ui: &mut egui::Ui, plugin: &str, node: &Value, out: &mut Vec<Dispatch>) {
    let ty = node.get("type").and_then(|x| x.as_str()).unwrap_or("text");
    match ty {
        "text" => {
            let v = s_of(node, "value");
            let dim = b_of(node, "dim", false);
            let r = egui::RichText::new(v);
            ui.label(if dim { r.color(pal().muted) } else { r });
        }
        "heading" => {
            ui.label(
                egui::RichText::new(s_of(node, "value"))
                    .size(17.0)
                    .strong()
                    .color(pal().ink),
            );
        }
        "label" => {
            // 表单左侧的小标题
            ui.label(
                egui::RichText::new(s_of(node, "value"))
                    .small()
                    .color(pal().muted),
            );
        }
        "badge" => {
            let v = s_of(node, "value");
            if v.is_empty() {
                return;
            }
            adl_ui::badge(ui, v, tone_color(s_of(node, "tone")));
        }
        "status" => {
            // = badge + 圆点，用于"运行中/已完成"这类状态
            let v = s_of(node, "value");
            let c = tone_color(s_of(node, "tone"));
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 3.5, c);
                ui.label(egui::RichText::new(v).color(c));
            });
        }
        "divider" => {
            // 极细线：插件也可能想分段，但不要粗沟
            adl_ui::hairline(ui);
        }
        "spacer" => {
            let h = f_of(node, "height").unwrap_or(8.0) as f32;
            ui.add_space(h);
        }
        "row" => {
            let gap = f_of(node, "gap").unwrap_or(8.0) as f32;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                if let Some(ch) = node.get("children").and_then(|x| x.as_array()) {
                    for c in ch {
                        render_node(ui, plugin, c, out);
                    }
                }
            });
        }
        "col" => {
            let gap = f_of(node, "gap").unwrap_or(6.0) as f32;
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = gap;
                if let Some(ch) = node.get("children").and_then(|x| x.as_array()) {
                    for c in ch {
                        render_node(ui, plugin, c, out);
                    }
                }
            });
        }
        "card" => {
            // 插件页里也能有卡片，视觉与本体一致
            adl_ui::card(ui, |ui| {
                if let Some(ch) = node.get("children").and_then(|x| x.as_array()) {
                    for c in ch {
                        render_node(ui, plugin, c, out);
                    }
                }
            });
        }
        "button" => {
            let label = node.get("label").and_then(|x| x.as_str()).unwrap_or("按钮");
            let kind = s_of(node, "kind");
            let clicked = if kind == "primary" {
                ui.add(egui::Button::new(
                    egui::RichText::new(label).color(egui::Color32::WHITE),
                )
                .fill(pal().accent))
                .clicked()
            } else if kind == "ghost" {
                ui.add(egui::Button::new(label).frame(false)).clicked()
            } else {
                ui.button(label).clicked()
            };
            if clicked {
                if let Some(action) = node.get("action").and_then(|x| x.as_str()) {
                    out.push(Dispatch::Action {
                        plugin: plugin.to_string(),
                        action: action.to_string(),
                    });
                }
            }
        }
        "progress" => {
            // 两种写法：value 是 0..1 比例，或 value/total 是绝对量
            let total = f_of(node, "total");
            let ratio = match total {
                Some(t) if t > 0.0 => (f_of(node, "value").unwrap_or(0.0) / t) as f32,
                _ => f_of(node, "value").unwrap_or(0.0) as f32,
            };
            let mut pb = egui::ProgressBar::new(ratio.clamp(0.0, 1.0)).fill(pal().accent);
            let label = s_of(node, "label");
            pb = if let Some(t) = total {
                pb.text(format!(
                    "{} / {}",
                    format_bytes(f_of(node, "value").unwrap_or(0.0) as u64),
                    format_bytes(t as u64)
                ))
            } else if !label.is_empty() {
                pb.text(label)
            } else {
                pb.show_percentage()
            };
            let width = ui.available_width();
            ui.add(pb.desired_width(width));
        }
        "list" => {
            let items = node.get("items").and_then(|x| x.as_array()).cloned();
            if let Some(items) = items {
                for it in items.iter() {
                    ui.horizontal(|ui| {
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(6.0, 6.0), egui::Sense::hover());
                        ui.painter().circle_filled(rect.center(), 2.0, pal().muted);
                        ui.add_space(2.0);
                        if it.is_string() {
                            ui.label(it.as_str().unwrap_or(""));
                        } else {
                            render_node(ui, plugin, it, out);
                        }
                    });
                }
            } else if let Some(ch) = node.get("children").and_then(|x| x.as_array()) {
                for c in ch {
                    render_node(ui, plugin, c, out);
                }
            }
        }
        "kv" => {
            // 键值对：左侧灰标签定宽，右侧值
            let rows = node.get("items").and_then(|x| x.as_array()).cloned();
            let lw = f_of(node, "labelWidth").unwrap_or(110.0) as f32;
            if let Some(rows) = rows {
                for r in rows.iter() {
                    let k = s_of(r, "key");
                    let v = s_of(r, "value");
                    let tone = s_of(r, "tone");
                    ui.horizontal(|ui| {
                        // 键用弱色、值用正文色：一眼能分出"标签/内容"
                        ui.allocate_ui_with_layout(
                            egui::vec2(lw, 22.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.label(egui::RichText::new(k).color(pal().muted));
                            },
                        );
                        let text = egui::RichText::new(v);
                        if tone.is_empty() {
                            ui.label(text);
                        } else {
                            ui.label(text.color(tone_color(tone)));
                        }
                    });
                }
            }
        }
        "stat" => {
            // 大数字卡片：value 为主，label 为辅
            adl_ui::card(ui, |ui| {
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(s_of(node, "value"))
                            .size(22.0)
                            .strong()
                            .color(pal().ink),
                    );
                    let sub = s_of(node, "label");
                    if !sub.is_empty() {
                        ui.label(egui::RichText::new(sub).small().color(pal().muted));
                    }
                });
            });
        }
        "grid" => {
            // 等宽网格：按 columns 把 children 分行排（用于统计卡片墙）
            let cols = f_of(node, "columns").unwrap_or(3.0).max(1.0) as usize;
            let children: Vec<Value> = node
                .get("children")
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default();
            let gap = f_of(node, "gap").unwrap_or(10.0) as f32;
            let avail = ui.available_width();
            let cell_w = ((avail - gap * (cols as f32 - 1.0)) / cols as f32).max(80.0);
            for chunk in children.chunks(cols) {
                // horizontal_top：否则各行高度不同时卡片会像阶梯一样上下错开
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for c in chunk {
                        ui.allocate_ui_with_layout(
                            egui::vec2(cell_w, 10.0),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                ui.set_min_width(cell_w);
                                ui.set_max_width(cell_w);
                                render_node(ui, plugin, c, out);
                            },
                        );
                    }
                });
                ui.add_space(gap);
            }
        }
        "table" => {
            // 插件表格：columns=[{title,width?}]，rows=[[cell,...]]
            let titles: Vec<String> = node
                .get("columns")
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .map(|c| {
                            if c.is_string() {
                                c.as_str().unwrap_or("").to_string()
                            } else {
                                s_of(c, "title").to_string()
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            if titles.is_empty() {
                return;
            }
            // rows 的单元格可以是纯字符串，也可以是 {value, tone}。
            // tone 以前被解析后丢弃，现在真的上色 —— 插件表格里
            // "已完成/失败"这类列终于能和本体一样有颜色。
            let rows: Vec<Vec<(String, String)>> = node
                .get("rows")
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .map(|r| {
                            r.as_array()
                                .map(|cells| {
                                    cells
                                        .iter()
                                        .map(|c| match c {
                                            Value::String(s) => (s.clone(), String::new()),
                                            other => (
                                                s_of(other, "value").to_string(),
                                                s_of(other, "tone").to_string(),
                                            ),
                                        })
                                        .collect()
                                })
                                .unwrap_or_default()
                        })
                        .collect()
                })
                .unwrap_or_default();
            // 手写表头（列宽均分，插件不参与列宽计算，避免布局失控）
            let n = titles.len();
            let gap = 10.0;
            let avail = ui.available_width();
            let w = ((avail - gap * (n as f32 - 1.0) - 16.0) / n as f32).max(60.0);
            let widths: Vec<f32> = vec![w; n];
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for t in &titles {
                    adl_ui::widgets::cell(ui, w, |ui| {
                        ui.label(egui::RichText::new(t).small().color(pal().muted));
                    });
                }
            });
            ui.add_space(4.0);
            adl_ui::hairline(ui);
            ui.add_space(4.0);
            for row in rows {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for (i, cw) in widths.iter().enumerate() {
                        let (v, tone) = row
                            .get(i)
                            .cloned()
                            .unwrap_or_else(|| (String::new(), String::new()));
                        adl_ui::widgets::cell(ui, *cw, |ui| {
                            if tone.is_empty() {
                                adl_ui::truncated(ui, &v, &v);
                            } else {
                                let r = egui::RichText::new(&v).color(tone_color(&tone));
                                ui.add(egui::Label::new(r).truncate()).on_hover_text(&v);
                            }
                        });
                    }
                });
                ui.add_space(4.0);
            }
        }
        "link" => {
            let label = s_of(node, "label");
            let url = s_of(node, "url");
            if url.is_empty() {
                ui.label(label);
            } else {
                ui.hyperlink_to(label, url);
            }
        }
        "toggle" => {
            let label = s_of(node, "label");
            let cur = b_of(node, "value", false);
            let mut b = cur;
            if ui.checkbox(&mut b, label).changed() {
                if let Some(key) = node.get("setting").and_then(|x| x.as_str()) {
                    out.push(Dispatch::Setting {
                        plugin: plugin.to_string(),
                        key: key.to_string(),
                        value: b.to_string(),
                    });
                }
            }
        }
        "select" => {
            let label = s_of(node, "label");
            let cur = s_of(node, "value").to_string();
            let opts = sa_of(node, "options");
            let mut sel = cur.clone();
            ui.horizontal(|ui| {
                if !label.is_empty() {
                    ui.label(label);
                }
                egui::ComboBox::from_id_salt(format!("{plugin}.{label}"))
                    .width(200.0)
                    .selected_text(sel.clone())
                    .show_ui(ui, |ui| {
                        for o in &opts {
                            ui.selectable_value(&mut sel, o.clone(), o.clone());
                        }
                    });
            });
            if sel != cur {
                if let Some(key) = node.get("setting").and_then(|x| x.as_str()) {
                    out.push(Dispatch::Setting {
                        plugin: plugin.to_string(),
                        key: key.to_string(),
                        value: sel,
                    });
                }
            }
        }
        "input" => {
            // 只读：插件不能往宿主抢焦点，输入类交回本体页面处理
            let label = s_of(node, "label");
            let val = s_of(node, "value");
            ui.horizontal(|ui| {
                if !label.is_empty() {
                    ui.label(label);
                }
                ui.label(egui::RichText::new(val).color(pal().ink));
            });
        }
        "code" => {
            // 等宽只读文本（日志/哈希）。
            //
            // 全站默认关掉了"拖选文字"（见 `Theme::apply`），但哈希、日志、
            // 错误堆栈这类内容用户真的会想复制，所以这里显式放开。
            ui.add(
                egui::Label::new(
                    egui::RichText::new(s_of(node, "value"))
                        .monospace()
                        .small()
                        .color(pal().muted),
                )
                .selectable(true),
            );
        }
        "timestamp" => {
            let v = s_of(node, "value");
            ui.label(egui::RichText::new(v).small().color(pal().muted));
        }
        "copy" => {
            // 一键复制：值本身只读展示（等宽，方便核对哈希/路径），
            // 真正写剪贴板的是宿主 —— 插件拿不到剪贴板 API。
            let v = s_of(node, "value").to_string();
            let label = s_of(node, "label");
            let btn = if label.is_empty() { "复制" } else { label };
            ui.horizontal(|ui| {
                if !v.is_empty() {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&v)
                                .monospace()
                                .small()
                                .color(pal().muted),
                        )
                        .selectable(true),
                    );
                }
                if ui
                    .add(egui::Button::new(egui::RichText::new(btn).small()).frame(false))
                    .on_hover_text("复制到剪贴板")
                    .clicked()
                {
                    ui.ctx().copy_text(v.clone());
                }
            });
        }
        "spinner" => {
            // 加载中：插件页在等异步结果时给个活物，别让用户以为卡死
            let size = f_of(node, "size").unwrap_or(16.0).clamp(8.0, 48.0) as f32;
            let label = s_of(node, "label");
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(size).color(pal().accent));
                if !label.is_empty() {
                    ui.label(egui::RichText::new(label).color(pal().muted));
                }
            });
        }
        "alert" => {
            // 提示条：底色块 + 左侧细色条，比 badge 更能承载一段话
            let p = pal();
            let tone = s_of(node, "tone");
            let c = tone_color(tone);
            let bg = match tone {
                "danger" | "error" | "failed" => p.err_bg,
                "warn" | "paused" => p.warn_bg,
                _ => p.accent_soft,
            };
            let title = s_of(node, "title");
            let text = s_of(node, "value");
            let resp = egui::Frame::new()
                .fill(bg)
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin::symmetric(14, 9))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.vertical(|ui| {
                        if !title.is_empty() {
                            ui.label(egui::RichText::new(title).strong().color(c));
                        }
                        if !text.is_empty() {
                            ui.label(egui::RichText::new(text).color(p.ink));
                        }
                    });
                });
            // 左侧色条画在 Frame 之外，避免和内容抢布局
            let r = resp.response.rect;
            ui.painter().rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(r.left() + 5.0, r.top() + 8.0),
                    egui::vec2(3.0, (r.height() - 16.0).max(0.0)),
                ),
                egui::CornerRadius::same(2),
                c,
            );
        }
        "collapse" => {
            // 折叠面板：长页面把次要信息收起来
            let t = s_of(node, "title");
            let title = if t.is_empty() { s_of(node, "label") } else { t };
            let open = b_of(node, "open", true);
            let hdr = egui::CollapsingHeader::new(
                egui::RichText::new(title).strong().color(pal().ink),
            )
            .default_open(open)
            .show_background(false)
            .id_salt(format!("{plugin}.collapse.{title}"));
            hdr.show(ui, |ui| {
                if let Some(ch) = node.get("children").and_then(|x| x.as_array()) {
                    for c in ch {
                        render_node(ui, plugin, c, out);
                    }
                }
            });
        }
        "scroll" => {
            // 限高滚动：插件塞长列表时不把整页顶出屏幕
            let h = f_of(node, "maxHeight").unwrap_or(240.0).clamp(40.0, 2000.0) as f32;
            let id = s_of(node, "id");
            let mut sa = egui::ScrollArea::vertical()
                .max_height(h)
                .auto_shrink([false, false]);
            if !id.is_empty() {
                sa = sa.id_salt(format!("{plugin}.scroll.{id}"));
            }
            sa.show(ui, |ui| {
                if let Some(ch) = node.get("children").and_then(|x| x.as_array()) {
                    for c in ch {
                        render_node(ui, plugin, c, out);
                    }
                }
            });
        }
        "steps" => {
            // 步骤条：current 之前的算已完成（实心），当前步用 tone，之后是灰
            let cur = f_of(node, "current").unwrap_or(0.0).max(0.0) as usize;
            let items: Vec<Value> = node
                .get("items")
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default();
            let p = pal();
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                for (i, it) in items.iter().enumerate() {
                    let (label, tone) = if it.is_string() {
                        (it.as_str().unwrap_or("").to_string(), String::new())
                    } else {
                        (
                            s_of(it, "label").to_string(),
                            s_of(it, "tone").to_string(),
                        )
                    };
                    let c = if i == cur {
                        tone_color(if tone.is_empty() { "accent" } else { tone.as_str() })
                    } else if i < cur {
                        p.accent
                    } else {
                        p.muted
                    };
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
                    // 已完成和当前步都是实心圆，靠"当前步多一圈光晕"区分 ——
                    // 否则两步同色，用户看不出走到哪了。
                    if i == cur {
                        ui.painter().circle_filled(rect.center(), 9.0, c.gamma_multiply(0.22));
                    }
                    ui.painter().circle_filled(
                        rect.center(),
                        if i == cur { 7.0 } else { 9.0 },
                        if i <= cur { c } else { p.hair },
                    );
                    if i < cur {
                        // 已完成：手画对勾。simhei 没有 ✓ 的字形，用字体符号会
                        // 渲染成空方块（顶栏图标踩过这个坑）。
                        let ctr = rect.center();
                        let s = 4.2;
                        let stroke = egui::Stroke::new(1.6, egui::Color32::WHITE);
                        ui.painter().line_segment(
                            [
                                egui::pos2(ctr.x - s * 0.9, ctr.y + s * 0.05),
                                egui::pos2(ctr.x - s * 0.2, ctr.y + s * 0.75),
                            ],
                            stroke,
                        );
                        ui.painter().line_segment(
                            [
                                egui::pos2(ctr.x - s * 0.2, ctr.y + s * 0.75),
                                egui::pos2(ctr.x + s * 0.95, ctr.y - s * 0.8),
                            ],
                            stroke,
                        );
                    } else {
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            format!("{}", i + 1),
                            egui::FontId::proportional(10.0),
                            if i <= cur {
                                egui::Color32::WHITE
                            } else {
                                p.muted
                            },
                        );
                    }
                    if !label.is_empty() {
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(label)
                                .small()
                                .color(if i <= cur { p.ink } else { p.muted }),
                        );
                        ui.add_space(6.0);
                    }
                    if i + 1 < items.len() {
                        let (lr, _) = ui
                            .allocate_exact_size(egui::vec2(24.0, 18.0), egui::Sense::hover());
                        ui.painter().hline(
                            lr.x_range(),
                            lr.center().y,
                            egui::Stroke::new(1.0, p.hair),
                        );
                    }
                }
            });
        }
        other => {
            // 未知类型：画占位而不是 panic —— 新版插件在旧版宿主上要能降级
            ui.label(
                egui::RichText::new(format!("[{other}]"))
                    .small()
                    .color(pal().muted),
            );
        }
    }
}

// ───────────────────────── 辅助 ─────────────────────────

fn install_cjk_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates = [
        "C:/Windows/Fonts/simhei.ttf",
        "C:/Windows/Fonts/Deng.ttf",
        "C:/Windows/Fonts/msyh.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(
                "cjk".to_owned(),
                std::sync::Arc::new(egui::FontData::from_owned(bytes)),
            );
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "cjk".to_owned());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("cjk".to_owned());
            break;
        }
    }
    ctx.set_fonts(fonts);
}

fn status_style(status: &str) -> (&'static str, egui::Color32) {
    match status {
        "completed" => ("完成", egui::Color32::from_rgb(0x2E, 0x7D, 0x6B)),
        "failed" => ("失败", egui::Color32::from_rgb(0xB0, 0x30, 0x30)),
        "paused" => ("已暂停", egui::Color32::from_rgb(0x8A, 0x6D, 0x1F)),
        _ => ("下载中", egui::Color32::from_rgb(0x1F, 0x5F, 0x8A)),
    }
}

/// 顶栏"后台任务"下拉里的一行：状态点 + 文件名 + 进度/大小 + 速度。
///
/// 刻意做得比任务列表松（高约 30px、不带操作按钮）—— 它只是"瞥一眼"的
/// 面板，真正的操作在任务列表页。
/// 任务在界面上的显示名：优先落盘文件名；还没落盘（失败/刚入队）就退到
/// URL 的最后一段（很多失败任务是 0 字节，`path` 为空，直接把整条 URL
/// 当名字会把弹出面板撑得很宽）。
fn display_name(t: &TaskView) -> String {
    if let Some(name) = t
        .path
        .as_deref()
        .and_then(|p| std::path::Path::new(p).file_name())
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
    {
        return name.to_string();
    }
    let no_query = t.url.split(['?', '#']).next().unwrap_or(&t.url);
    let tail = no_query.trim_end_matches('/');
    let last = tail.rsplit('/').find(|s| !s.is_empty());
    match last {
        Some(s) => s.to_string(),
        None => t.url.clone(),
    }
}

/// 顶栏"后台任务"下拉里的一行：状态点 + 文件名 + 进度/大小。
///
/// 刻意做得比任务列表松（不带操作按钮）—— 它只是"瞥一眼"的面板，
/// 真正的操作在任务列表页。返回 true 表示这一行被点了。
fn task_row(ui: &mut egui::Ui, t: &TaskView) -> bool {
    let (_, color) = status_style(&t.status);
    let name = display_name(t);

    let row = ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 18.0), egui::Sense::hover());
        ui.painter().circle_filled(
            rect.center(),
            3.2,
            if t.status == "downloading" {
                teal()
            } else {
                color
            },
        );

        // 文件名（宽度自适应，右侧留固定宽度给"进度/大小"）
        let right_w = 118.0;
        let name_w = (ui.available_width() - right_w).max(80.0);
        adl_ui::widgets::cell(ui, name_w, |ui| {
            adl_ui::table::truncated(ui, &name, &t.url);
        });

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let tail = if t.status == "downloading" {
                if t.speed > 0 {
                    format!("{}  {}", format_bytes(t.completed), format_speed(t.speed))
                } else {
                    format_bytes(t.completed)
                }
            } else if t.status == "failed" {
                "已中断".to_string()
            } else {
                format_bytes(t.total.max(t.completed))
            };
            ui.label(egui::RichText::new(tail).small().color(muted()));
        });
    });

    row.response.interact(egui::Sense::click()).clicked()
}

fn format_bytes(n: u64) -> String {
    const U: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

fn format_speed(bps: u64) -> String {
    if bps == 0 {
        "-".to_string()
    } else {
        format!("{}/s", format_bytes(bps))
    }
}

/// 单位字符串 → 每个单位多少字节；`auto` 返回 0（表示让 `format_speed` 自己挑）。
fn unit_scale_of(unit: &str) -> u32 {
    match unit {
        "B/s" => 1,
        "KB/s" => 1024,
        "MB/s" => 1024 * 1024,
        "GB/s" => 1024 * 1024 * 1024,
        _ => 0, // auto
    }
}

/// 按用户选的单位显示速度。
///
/// 中文"兆字节/秒"读起来别扭，这里统一走 `MB/s` 这类英文单位；
/// `auto` 时退化为自适应（沿用 `format_speed`）。
fn format_speed_unit(bps: u64, unit: &str) -> String {
    if bps == 0 {
        return "0 B/s".to_string();
    }
    match unit_scale_of(unit) {
        0 => format_speed(bps),
        scale => {
            let v = bps as f64 / scale as f64;
            if scale == 1 {
                format!("{bps} B/s")
            } else {
                format!("{v:.1} {}", unit)
            }
        }
    }
}

// ───────────────────────── 与后端交互（复用原有逻辑） ─────────────────────────

fn plugins_root(cfg: &Config) -> Option<PathBuf> {
    cfg.plugin_dir_opt().or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("plugins")))
    })
}

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

pub(crate) fn gui_snapshot(gui: &Arc<Gui>) -> Value {
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
    let snapshot = settings_json(&cfg);
    let new_cfg = cfg.clone();
    drop(cfg);
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
            connections: 0,
            path: None,
            error: None,
            handle: None,
        });
    }

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
                    }
                }
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
                    t.connections = 0;
                }
                Err(e) => {
                    t.status = "failed".into();
                    t.error = Some(e.to_string());
                    t.connections = 0;
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
            h.abort();
        }
        if t.status == "downloading" {
            t.status = "paused".into();
        }
        Ok(json!({"ok": true}))
    } else {
        Err("任务不存在".into())
    }
}

fn pause_all(gui: &Arc<Gui>) {
    let mut tasks = gui.tasks.lock().unwrap();
    for t in tasks.iter_mut() {
        if t.status == "downloading" {
            if let Some(h) = t.handle.take() {
                h.abort();
            }
            t.status = "paused".into();
        }
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

// ───────────────────────── 入口 ─────────────────────────

fn make_icon() -> Option<Icon> {
    let (w, h) = (32u32, 32u32);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let edge = x < 2 || y < 2 || x >= w - 2 || y >= h - 2;
            let px = if edge {
                [0x2Eu8, 0x7D, 0x6B, 0xFF]
            } else {
                [0x4Au8, 0x9E, 0x9E, 0xFF]
            };
            rgba.extend_from_slice(&px);
        }
    }
    Icon::from_rgba(rgba, w, h).ok()
}

fn build_tray() -> Option<TrayIcon> {
    let menu = Menu::new();
    let show = MenuItem::with_id("show", "显示主界面", true, None);
    let quit = MenuItem::with_id("quit", "退出", true, None);
    let _ = menu.append(&show);
    let _ = menu.append(&quit);
    let mut b = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("ADL 下载器");
    if let Some(ic) = make_icon() {
        b = b.with_icon(ic);
    }
    b.build().ok()
}

fn main() -> eframe::Result {
    // 单实例：已有实例则唤醒它并退出
    let _single = platform::acquire_single_instance();
    if _single.is_none() {
        platform::signal_existing_instance();
        return Ok(());
    }

    let cfg = Config::load();
    let gui = Arc::new(Gui {
        tasks: Mutex::new(Vec::new()),
        next_id: AtomicU64::new(1),
        rt: tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("构建 tokio 运行时失败"),
        cfg: Mutex::new(cfg.clone()),
        hooks: Mutex::new(build_hooks(&cfg)),
    });

    // 浏览器接管用的本地 API（与 UI 同进程共享状态）
    let api_port = std::env::var("ADL_API_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(api_server::DEFAULT_PORT);
    match api_server::start(gui.clone(), api_port) {
        Ok(p) => eprintln!("[api] 浏览器接管 API 监听 127.0.0.1:{p}"),
        Err(e) => eprintln!("[api] 启动失败: {e}"),
    }

    // 休眠 / 唤醒
    let suspend = Arc::new(AtomicBool::new(false));
    let resume = Arc::new(AtomicBool::new(false));
    {
        let s = suspend.clone();
        let r = resume.clone();
        let _ = platform::register_power(move |ev| match ev {
            platform::PowerEvent::Suspend => s.store(true, Ordering::SeqCst),
            platform::PowerEvent::Resume => r.store(true, Ordering::SeqCst),
            platform::PowerEvent::Other => {}
        });
    }

    // 第二实例唤醒
    let wake = Arc::new(AtomicBool::new(false));
    {
        let w = wake.clone();
        platform::spawn_wakeup_listener(move || w.store(true, Ordering::SeqCst));
    }

    // 渲染后端选择：默认 glow(OpenGL)；ADL_RENDERER=wgpu 走 wgpu(Vulkan/DX12/Metal)；
    // ADL_RENDERER=vulkan 等价于 wgpu 且强制 WGPU_BACKEND=vulkan。
    let renderer = match std::env::var("ADL_RENDERER")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "wgpu" => {
            // 实测：本机 wgpu 自动选后端会优先拿到 Vulkan，而 Intel Vulkan 驱动在
            // 远程/无独显会话下 swapchain 会在数秒内静默崩溃；DX12 稳定。所以
            // 未显式指定 WGPU_BACKEND 时，Windows 上默认走 dx12。
            if cfg!(windows) && std::env::var_os("WGPU_BACKEND").is_none() {
                std::env::set_var("WGPU_BACKEND", "dx12");
            }
            eframe::Renderer::Wgpu
        }
        "vulkan" => {
            std::env::set_var("WGPU_BACKEND", "vulkan");
            eframe::Renderer::Wgpu
        }
        _ => eframe::Renderer::Glow,
    };
    eprintln!("[gui] 渲染后端: {:?}", renderer);

    let opts = eframe::NativeOptions {
        renderer,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1040.0, 700.0])
            .with_min_inner_size([760.0, 520.0])
            .with_title("ADL 下载器")
            // 关掉系统标题栏，自己画（见 App::titlebar）。
            // 代价：系统不再提供边缘缩放，八向热区由 resize_borders() 铺。
            .with_decorations(false),
        ..Default::default()
    };

    let gui2 = gui.clone();
    let tray = build_tray();
    eframe::run_native(
        "ADL",
        opts,
        Box::new(move |cc| Ok(Box::new(App::new(cc, gui2, wake, suspend, resume, tray)))),
    )
}
