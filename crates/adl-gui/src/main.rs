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

// ───────────────────────── 共享状态 ─────────────────────────

pub(crate) struct Task {
    pub(crate) id: u64,
    pub(crate) url: String,
    pub(crate) status: String,
    pub(crate) completed: u64,
    pub(crate) total: u64,
    pub(crate) speed: u64,
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
            path: t.path.clone(),
            error: t.error.clone(),
        })
        .collect()
}

// ───────────────────────── 应用 ─────────────────────────

#[derive(Clone, PartialEq)]
enum Tab {
    Tasks,
    Add,
    Settings,
    Plugin(String, String),
}

enum Dispatch {
    Action { plugin: String, action: String },
    Setting { plugin: String, key: String, value: String },
}

struct App {
    gui: Arc<Gui>,
    tab: Tab,
    // 添加下载表单
    f_url: String,
    f_out: String,
    // 设置表单
    s_dir: String,
    s_conn: u64,
    s_limit: u64,
    s_proxy: String,
    s_mode: String,
    s_http1: bool,
    s_plugin_dir: String,
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
        //   tasks | add | settings | plugin:<pluginId>:<pageId>
        let initial_tab = match std::env::var("ADL_TAB").unwrap_or_default().as_str() {
            "add" => Tab::Add,
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
            f_url: String::new(),
            f_out: String::new(),
            s_dir: cfg.dir.clone(),
            s_conn: cfg.connections as u64,
            s_limit: cfg.limit,
            s_proxy: cfg.proxy.clone(),
            s_mode: if cfg.mode_trusted() { "trusted".into() } else { "safe".into() },
            s_http1: cfg.http1,
            s_plugin_dir: cfg.plugin_dir.clone(),
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
                self.tab = Tab::Tasks;
            }
            Err(e) => self.set_status(e, true),
        }
    }

    fn do_save_settings(&mut self) {
        let args = json!({
            "dir": self.s_dir,
            "connections": self.s_conn,
            "limit": self.s_limit,
            "proxy": self.s_proxy,
            "mode": self.s_mode,
            "http1": self.s_http1,
            "plugin_dir": self.s_plugin_dir,
        });
        match set_settings(&self.gui, &args) {
            Ok(_) => {
                self.set_status("已保存设置", false);
                self.plugin_snapshot_at = Instant::now() - Duration::from_secs(3600);
            }
            Err(e) => self.set_status(e, true),
        }
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

/// 顶栏：白底
fn topbar_frame() -> egui::Frame {
    theme().topbar_frame()
}

/// 内容区：浅底 + 内边距
fn content_frame() -> egui::Frame {
    theme().content_frame()
}

/// 设置页左侧标签格、表格单元格、导航项、顶栏按钮等已统一迁到
/// `adl_ui::widgets` / `adl_ui::table`，本文件只保留页面组装逻辑。

impl eframe::App for App {
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

        // 关闭到托盘：拦截关闭，改为隐藏窗口（不销毁，egui 无白屏问题）
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
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

        // ===== 整体布局：最外层自绘标题栏 + 左侧整高侧边栏 + 右侧顶栏/内容 =====
        // Panel 之间的调用顺序决定占位顺序，所以最外层不要先开 CentralPanel。
        egui::Panel::top("adl_titlebar")
            .exact_size(titlebar_h())
            .resizable(false)
            .show_separator_line(false)
            .frame(titlebar_frame())
            .show(ui, |ui| self.titlebar(ui));

        egui::Panel::left("adl_sidebar")
            .exact_size(sidebar_w())
            .resizable(false)
            .show_separator_line(false)
            .frame(sidebar_frame())
            .show(ui, |ui| self.sidebar(ui));

        egui::Panel::top("adl_topbar")
            .exact_size(topbar_h())
            .resizable(false)
            .show_separator_line(false)
            .frame(topbar_frame())
            .show(ui, |ui| self.topbar(ui));

        // 状态提示条（6 秒后消失）
        if let Some((msg, err, at)) = &self.status {
            if at.elapsed() < Duration::from_secs(6) {
                let color = if *err {
                    egui::Color32::from_rgb(0xB0, 0x30, 0x30)
                } else {
                    egui::Color32::from_rgb(0x2E, 0x7D, 0x6B)
                };
                let fill = if *err {
                    egui::Color32::from_rgb(0xFD, 0xEF, 0xEF)
                } else {
                    egui::Color32::from_rgb(0xEC, 0xF7, 0xF4)
                };
                egui::Panel::top("adl_statusbar")
                    .resizable(false)
                    .show_separator_line(false)
                    .frame(
                        egui::Frame::new()
                            .fill(fill)
                            .inner_margin(egui::Margin::symmetric(20, 6)),
                    )
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.colored_label(color, if *err { "⚠" } else { "✓" });
                            ui.colored_label(color, msg.as_str());
                        });
                    });
            }
        }

        egui::CentralPanel::default()
            .frame(content_frame())
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        match self.tab.clone() {
                            Tab::Tasks => self.page_tasks(ui),
                            Tab::Add => self.page_add(ui),
                            Tab::Settings => self.page_settings(ui),
                            Tab::Plugin(_, _) => self.page_plugin(ui),
                        }
                    });
            });

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

    /// 左侧整高边栏：导航 + 底部状态
    ///
    /// 品牌字（ADL / 下载器）已在自绘标题栏里，这里不再重复 ——
    /// 否则窗口顶部会出现两次"ADL 下载器"。
    fn sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.label(egui::RichText::new("导航").size(11.5).color(muted()));
        ui.add_space(6.0);

        if self.nav_item(ui, self.tab == Tab::Tasks, "任务列表") {
            self.tab = Tab::Tasks;
        }
        ui.add_space(2.0);
        if self.nav_item(ui, self.tab == Tab::Add, "添加下载") {
            self.tab = Tab::Add;
        }
        ui.add_space(2.0);
        if self.nav_item(ui, self.tab == Tab::Settings, "设置") {
            self.tab = Tab::Settings;
        }

        let pages = self
            .plugin_snapshot
            .get("pages")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        if !pages.is_empty() {
            ui.add_space(18.0);
            ui.label(egui::RichText::new("插件页面").size(11.5).color(muted()));
            ui.add_space(6.0);
            for p in pages {
                let plugin = p.get("plugin").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let id = p.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let title = p.get("title").and_then(|x| x.as_str()).unwrap_or(&id).to_string();
                let selected =
                    matches!(&self.tab, Tab::Plugin(pl, pg) if pl == &plugin && pg == &id);
                if self.nav_item(ui, selected, &title) {
                    self.tab = Tab::Plugin(plugin.clone(), id.clone());
                    self.plugin_view = None;
                    self.reload_plugin_view();
                }
                ui.add_space(2.0);
            }
        }

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
            ui.add_space(8.0);
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
            ui.add_space(4.0);
        });
    }

    fn page_title(&self) -> String {
        match &self.tab {
            Tab::Tasks => "任务列表".to_string(),
            Tab::Add => "添加下载".to_string(),
            Tab::Settings => "设置".to_string(),
            Tab::Plugin(plugin, page) => {
                let p = page.clone();
                let pl = plugin.clone();
                self.plugin_snapshot
                    .get("pages")
                    .and_then(|x| x.as_array())
                    .and_then(|arr| {
                        arr.iter().find_map(|n| {
                            let same_plugin =
                                n.get("plugin").and_then(|x| x.as_str()) == Some(pl.as_str());
                            let same_page = n.get("id").and_then(|x| x.as_str()) == Some(p.as_str());
                            if same_plugin && same_page {
                                n.get("title").and_then(|x| x.as_str()).map(|s| s.to_string())
                            } else {
                                None
                            }
                        })
                    })
                    .unwrap_or_else(|| "插件页面".to_string())
            }
        }
    }

    /// 顶栏按钮：自绘（不依赖布局容器，位置绝对可控）
    fn topbar_button(ui: &mut egui::Ui, rect: egui::Rect, label: &str, primary: bool) -> bool {
        adl_ui::widgets::small_button(ui, rect, label, primary)
    }

    /// 自绘标题栏：品牌 + 可拖动空白区 + 最小化/最大化/关闭。
    ///
    /// 窗口用 `.with_decorations(false)` 关掉了系统标题栏，所以这里的拖动与
    /// 双击最大化必须自己实现（`ViewportCommand::StartDrag` / `Maximized`）。
    /// 拖动区只覆盖标题栏里除按钮外的部分，否则点按钮会变成拖窗口。
    fn titlebar(&mut self, ui: &mut egui::Ui) {
        // 跟随系统状态（用户用快捷键 Win+↑ 时也要同步）
        if let Some(m) = ui.ctx().input(|i| i.viewport().maximized) {
            self.maximized = m;
        }

        let band = ui.available_rect_before_wrap();
        let cy = band.center().y;

        // 右侧三个窗口按钮，从右缘往左排
        let bw = 46.0;
        let bh = band.height().min(34.0);
        let close_rect = egui::Rect::from_center_size(
            egui::pos2(band.right() - bw / 2.0, cy),
            egui::vec2(bw, bh),
        );
        let max_rect = close_rect.translate(egui::vec2(-bw, 0.0));
        let min_rect = max_rect.translate(egui::vec2(-bw, 0.0));

        // 品牌与拖动区（覆盖到最小化按钮左侧，留 6px 空隙）
        let drag_rect = egui::Rect::from_min_max(
            band.left_top(),
            egui::pos2(min_rect.left() - 6.0, band.bottom()),
        );
        let drag = ui.interact(drag_rect, ui.scope_id().with("adl_titlebar_drag"), egui::Sense::click_and_drag());
        if drag.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if drag.double_clicked() {
            self.toggle_maximize(ui.ctx());
        }

        ui.painter().text(
            egui::pos2(drag_rect.left() + 4.0, cy),
            egui::Align2::LEFT_CENTER,
            "ADL",
            egui::FontId::proportional(14.0),
            pal().accent,
        );
        ui.painter().text(
            egui::pos2(drag_rect.left() + 42.0, cy),
            egui::Align2::LEFT_CENTER,
            "下载器",
            egui::FontId::proportional(12.0),
            pal().muted,
        );

        if adl_ui::widgets::window_button(ui, min_rect, adl_ui::WindowButton::Minimize) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Minimized(true));
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

    /// 顶栏：页标题 + 右侧操作（自绘按钮，绝对定位到面板右缘）
    ///
    /// 不放"退出"按钮了：标题栏右上角已经有系统级关闭键，重复摆两个关闭
    /// 入口会让人犹豫"这两个有什么区别"。
    fn topbar(&mut self, ui: &mut egui::Ui) {
        let title = self.page_title();
        let band = ui.available_rect_before_wrap();
        let cy = band.center().y;
        let bh = 28.0;

        // 左侧标题
        ui.painter().text(
            egui::pos2(band.left() + 2.0, cy),
            egui::Align2::LEFT_CENTER,
            &title,
            egui::FontId::proportional(16.0),
            ink(),
        );

        // 右侧按钮，从右边缘往左排
        let w_add = 96.0;
        let right = band.right();
        let add_rect = egui::Rect::from_center_size(
            egui::pos2(right - w_add / 2.0, cy),
            egui::vec2(w_add, bh),
        );

        if Self::topbar_button(ui, add_rect, "+ 添加下载", true) {
            self.tab = Tab::Add;
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
            adl_ui::widgets::empty_state(ui, "还没有下载任务，去「添加下载」吧。");
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

            let name = t
                .path
                .as_deref()
                .and_then(|p| std::path::Path::new(p).file_name())
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| t.url.clone());
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

    fn page_add(&mut self, ui: &mut egui::Ui) {
        // 标题已在顶栏显示
        ui.add_space(2.0);
        // 这里的输入框要撑满宽度（URL 很长），所以不适合 form() 的定宽控件格，
        // 但仍复用 card / section_title / hint 保持与设置页一致的观感。
        let mut start = false;
        adl_ui::widgets::card(ui, |ui| {
            adl_ui::widgets::section_title(ui, "新建任务");
            ui.add_space(10.0);

            ui.label("下载地址");
            ui.add_space(4.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.f_url)
                    .hint_text("https://…")
                    .desired_width(f32::INFINITY),
            );

            ui.add_space(12.0);
            ui.label("保存文件名");
            ui.add_space(4.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.f_out)
                    .hint_text("可选，留空自动从地址 / 响应头推断，例如 ubuntu.iso")
                    .desired_width(f32::INFINITY),
            );

            ui.add_space(14.0);
            if ui.button("开始下载").clicked() {
                start = true;
            }
            ui.add_space(6.0);
            adl_ui::widgets::hint(
                ui,
                "浏览器插件接管时会自动带着来源、Cookie 等，无需手动填。",
            );
        });
        if start {
            self.do_add();
        }
    }

    fn page_settings(&mut self, ui: &mut egui::Ui) {
        // 标题已在顶栏显示
        ui.add_space(2.0);

        // 以前这里是 200 行 Grid::new(..) + label_cell + add_sized + end_row 的
        // 重复样板，现在每个字段一行 `Field`。新增一项设置只需加一行。
        let mode_opts: [(&str, &str); 2] = [
            ("safe", "安全（只读观察 / 拦截）"),
            ("trusted", "信任（可改写请求 / 替换界面）"),
        ];
        let mut fields = vec![
            adl_ui::Field::text("默认下载目录", &mut self.s_dir).hint("留空 = 当前目录"),
            adl_ui::Field::number("并发数（1–64，自适应）", &mut self.s_conn, 1..=64)
                .note("服务器不给满会自动降，稳定后偶尔试探 +1"),
            adl_ui::Field::number("限速（字节/秒，0 = 不限）", &mut self.s_limit, 0..=1_000_000_000),
            adl_ui::Field::text("HTTP 代理", &mut self.s_proxy)
                .hint("http://127.0.0.1:7890（留空 = 直连）"),
            adl_ui::Field::checkbox(
                "HTTP/1.1",
                &mut self.s_http1,
                "强制使用 HTTP/1.1（某些站点 h2 有问题时）",
            ),
        ];
        adl_ui::form(ui, "下载", &mut fields);

        ui.add_space(10.0);
        let mut plug_fields = vec![
            adl_ui::Field::select("插件模式", &mut self.s_mode, &mode_opts)
                .note("信任模式下插件才能改写请求、注册按钮与替换内置页面"),
            adl_ui::Field::text("插件目录", &mut self.s_plugin_dir)
                .hint("留空 = exe 同级 plugins"),
        ];
        adl_ui::form(ui, "插件", &mut plug_fields);

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
        let plugin = match self.tab.clone() {
            Tab::Plugin(p, _) => p,
            _ => return,
        };
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
            let rows: Vec<Vec<String>> = node
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
                                            // 单元格可以是纯字符串，也可以是 {value,tone}
                                            Value::String(s) => s.clone(),
                                            other => s_of(other, "value").to_string(),
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
                        let v = row.get(i).cloned().unwrap_or_default();
                        adl_ui::widgets::cell(ui, *cw, |ui| {
                            adl_ui::truncated(ui, &v, &v);
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
            // 等宽只读文本（日志/哈希）
            ui.label(
                egui::RichText::new(s_of(node, "value"))
                    .monospace()
                    .small()
                    .color(pal().muted),
            );
        }
        "timestamp" => {
            let v = s_of(node, "value");
            ui.label(egui::RichText::new(v).small().color(pal().muted));
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
                }
                Err(e) => {
                    t.status = "failed".into();
                    t.error = Some(e.to_string());
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
