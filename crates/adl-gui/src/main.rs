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
    s_conn: usize,
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
        let mut style = (*cc.egui_ctx.style_of(egui::Theme::Light)).clone();
        style.visuals = egui::Visuals::light();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 6.0);
        cc.egui_ctx.set_style_of(egui::Theme::Light, style.clone());
        cc.egui_ctx.set_style_of(egui::Theme::Dark, style);

        let cfg = gui.cfg.lock().unwrap().clone();
        Self {
            gui,
            tab: Tab::Tasks,
            f_url: String::new(),
            f_out: String::new(),
            s_dir: cfg.dir.clone(),
            s_conn: cfg.connections,
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

        egui::CentralPanel::default().show(ui, |ui| {
            // 顶栏
            ui.horizontal(|ui| {
                ui.heading("ADL");
                ui.label(egui::RichText::new("下载器 · 纯 Rust 原生界面").weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⏻ 退出").clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
            ui.separator();

            // 状态提示（6 秒后消失）
            if let Some((msg, err, at)) = &self.status {
                if at.elapsed() < Duration::from_secs(6) {
                    let color = if *err {
                        egui::Color32::from_rgb(0xB0, 0x30, 0x30)
                    } else {
                        egui::Color32::from_rgb(0x2E, 0x7D, 0x6B)
                    };
                    ui.horizontal(|ui| {
                        ui.colored_label(color, if *err { "⚠" } else { "✓" });
                        ui.colored_label(color, msg.as_str());
                    });
                }
            }

            let avail = ui.available_height();
            ui.horizontal_top(|ui| {
                // 左侧导航
                ui.allocate_ui_with_layout(
                    egui::vec2(180.0, avail),
                    egui::Layout::top_down(egui::Align::LEFT),
                    |ui| self.nav(ui),
                );
                ui.separator();
                // 右侧内容
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        match self.tab.clone() {
                            Tab::Tasks => self.page_tasks(ui),
                            Tab::Add => self.page_add(ui),
                            Tab::Settings => self.page_settings(ui),
                            Tab::Plugin(_, _) => self.page_plugin(ui),
                        }
                    });
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
    fn nav(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        if ui
            .selectable_label(self.tab == Tab::Tasks, "📥  任务列表")
            .clicked()
        {
            self.tab = Tab::Tasks;
        }
        if ui
            .selectable_label(self.tab == Tab::Add, "➕  添加下载")
            .clicked()
        {
            self.tab = Tab::Add;
        }
        if ui
            .selectable_label(self.tab == Tab::Settings, "⚙  设置")
            .clicked()
        {
            self.tab = Tab::Settings;
        }

        // 插件页面
        let pages = self
            .plugin_snapshot
            .get("pages")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        if !pages.is_empty() {
            ui.add_space(10.0);
            ui.label(egui::RichText::new("插件页面").weak().small());
            for p in pages {
                let plugin = p.get("plugin").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let id = p.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let title = p.get("title").and_then(|x| x.as_str()).unwrap_or(&id).to_string();
                let selected = matches!(&self.tab, Tab::Plugin(pl, pg) if pl == &plugin && pg == &id);
                if ui.selectable_label(selected, format!("🧩  {title}")).clicked() {
                    self.tab = Tab::Plugin(plugin.clone(), id.clone());
                    self.plugin_view = None;
                    self.reload_plugin_view();
                }
            }
        }

        // 插件动作（工具栏型）与命令提示
        let trusted = self
            .plugin_snapshot
            .get("trusted")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            ui.add_space(6.0);
            ui.label(egui::RichText::new(format!("模式：{}", if trusted { "信任" } else { "安全" })).weak().small());
            ui.label(egui::RichText::new("Ctrl+K 命令面板").weak().small());
        });
    }

    fn page_tasks(&mut self, ui: &mut egui::Ui) {
        ui.heading("任务列表");
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
                    if ui.button(format!("🔧 {label}")).clicked() {
                        self.run_action(&plugin, &action);
                    }
                }
            });
        }

        let tasks = snapshot(&self.gui);
        if tasks.is_empty() {
            ui.add_space(20.0);
            ui.label(egui::RichText::new("还没有下载任务，去「添加下载」吧。").weak());
            return;
        }
        ui.add_space(6.0);

        let mut pause_id: Option<u64> = None;
        let mut remove_id: Option<u64> = None;
        egui::Grid::new("tasks_grid")
            .num_columns(6)
            .striped(true)
            .spacing([14.0, 8.0])
            .show(ui, |ui| {
                ui.label(egui::RichText::new("状态").strong());
                ui.label(egui::RichText::new("文件").strong());
                ui.label(egui::RichText::new("进度").strong());
                ui.label(egui::RichText::new("速度").strong());
                ui.label(egui::RichText::new("大小").strong());
                ui.label(egui::RichText::new("操作").strong());
                ui.end_row();

                for t in &tasks {
                    let (txt, color) = status_style(&t.status);
                    ui.colored_label(color, txt);
                    let name = t
                        .path
                        .as_deref()
                        .and_then(|p| std::path::Path::new(p).file_name())
                        .and_then(|s| s.to_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| t.url.clone());
                    ui.vertical(|ui| {
                        ui.label(name).on_hover_text(&t.url);
                        if let Some(err) = &t.error {
                            ui.label(
                                egui::RichText::new(err)
                                    .small()
                                    .color(egui::Color32::from_rgb(0xB0, 0x30, 0x30)),
                            );
                        }
                    });
                    let frac = if t.total > 0 {
                        (t.completed as f32 / t.total as f32).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    ui.add(
                        egui::ProgressBar::new(frac)
                            .desired_width(150.0)
                            .show_percentage(),
                    );
                    ui.label(format_speed(t.speed));
                    ui.label(format_bytes(t.total));
                    ui.horizontal(|ui| {
                        if t.status == "downloading" && ui.small_button("暂停").clicked() {
                            pause_id = Some(t.id);
                        }
                        if ui.small_button("删除").clicked() {
                            remove_id = Some(t.id);
                        }
                    });
                    ui.end_row();
                }
            });

        if let Some(id) = pause_id {
            let _ = pause_task(&self.gui, &json!({"id": id}));
        }
        if let Some(id) = remove_id {
            let _ = remove_task(&self.gui, &json!({"id": id, "files": false}));
        }
    }

    fn page_add(&mut self, ui: &mut egui::Ui) {
        ui.heading("添加下载");
        ui.add_space(6.0);
        ui.label("下载地址");
        ui.add(
            egui::TextEdit::singleline(&mut self.f_url)
                .hint_text("https://…")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(6.0);
        ui.label("保存文件名（可选，留空自动从地址/响应推断）");
        ui.add(
            egui::TextEdit::singleline(&mut self.f_out)
                .hint_text("例如 ubuntu.iso")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(10.0);
        if ui.button("开始下载").clicked() {
            self.do_add();
        }
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("浏览器插件接管时会自动带着来源、Cookie 等，无需手动填。")
                .weak()
                .small(),
        );
    }

    fn page_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("设置");
        ui.add_space(6.0);
        egui::Grid::new("settings_grid")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label("默认下载目录");
                ui.add(
                    egui::TextEdit::singleline(&mut self.s_dir).desired_width(360.0),
                );
                ui.end_row();

                ui.label("并发数（1–64，自适应）");
                ui.add(egui::DragValue::new(&mut self.s_conn).range(1..=64));
                ui.end_row();

                ui.label("限速（字节/秒，0=不限）");
                ui.add(egui::DragValue::new(&mut self.s_limit).range(0..=1_000_000_000u64));
                ui.end_row();

                ui.label("HTTP 代理（如 http://127.0.0.1:7890）");
                ui.add(egui::TextEdit::singleline(&mut self.s_proxy).desired_width(360.0));
                ui.end_row();

                ui.label("插件模式");
                egui::ComboBox::from_id_salt("mode")
                    .selected_text(if self.s_mode == "trusted" { "信任（可改写/替换 UI）" } else { "安全（只读/拦截）" })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.s_mode, "safe".to_string(), "安全（只读/拦截）");
                        ui.selectable_value(&mut self.s_mode, "trusted".to_string(), "信任（可改写/替换 UI）");
                    });
                ui.end_row();

                ui.label("强制 HTTP/1.1");
                ui.checkbox(&mut self.s_http1, "某些站点的 h2 有问题时勾选");
                ui.end_row();

                ui.label("插件目录（留空=exe 同级 plugins）");
                ui.add(
                    egui::TextEdit::singleline(&mut self.s_plugin_dir).desired_width(360.0),
                );
                ui.end_row();
            });
        ui.add_space(10.0);
        if ui.button("保存设置").clicked() {
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
            ui.separator();
            ui.heading("插件设置");
            let mut pending: Vec<(String, String, String)> = Vec::new();
            egui::Grid::new("plugin_settings_grid")
                .num_columns(3)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    for s in &settings {
                        let plugin = s.get("plugin").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let key = s.get("key").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        let label = s.get("label").and_then(|x| x.as_str()).unwrap_or(&key).to_string();
                        let kind = s.get("kind").and_then(|x| x.as_str()).unwrap_or("string").to_string();
                        let value = s.get("value").and_then(|x| x.as_str()).map(|v| v.to_string())
                            .or_else(|| s.get("default").and_then(|x| x.as_str()).map(|v| v.to_string()))
                            .unwrap_or_default();
                        let editable = s.get("editable").and_then(|x| x.as_bool()).unwrap_or(false);

                        ui.label(label);
                        match kind.as_str() {
                            "boolean" => {
                                let mut b = value == "true";
                                if ui.add_enabled(editable, egui::Checkbox::new(&mut b, "")).changed() {
                                    pending.push((plugin.clone(), key.clone(), b.to_string()));
                                }
                            }
                            "select" => {
                                let opts: Vec<String> = s
                                    .get("options")
                                    .and_then(|x| x.as_array())
                                    .map(|a| a.iter().filter_map(|o| o.as_str().map(|s| s.to_string())).collect())
                                    .unwrap_or_default();
                                let mut cur = value.clone();
                                ui.add_enabled_ui(editable, |ui| {
                                    egui::ComboBox::from_id_salt(format!("{plugin}.{key}"))
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
                                if ui.add_enabled(editable, egui::TextEdit::singleline(buf).desired_width(220.0)).changed() {
                                    pending.push((plugin.clone(), key.clone(), buf.clone()));
                                }
                            }
                        }
                        ui.label(if editable { "" } else { "（只读）" });
                        ui.end_row();
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
        ui.heading(format!("插件页面 · {plugin}/{page}"));
        ui.add_space(6.0);
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

// ───────────────────────── 插件组件树渲染 ─────────────────────────

fn render_node(ui: &mut egui::Ui, plugin: &str, node: &Value, out: &mut Vec<Dispatch>) {
    let ty = node.get("type").and_then(|x| x.as_str()).unwrap_or("text");
    match ty {
        "text" => {
            ui.label(node.get("value").and_then(|x| x.as_str()).unwrap_or(""));
        }
        "heading" => {
            ui.heading(node.get("value").and_then(|x| x.as_str()).unwrap_or(""));
        }
        "badge" => {
            let v = node.get("value").and_then(|x| x.as_str()).unwrap_or("");
            ui.label(egui::RichText::new(format!("[{v}]")).weak().small());
        }
        "divider" => {
            ui.separator();
        }
        "spacer" => {
            let h = node.get("height").and_then(|x| x.as_f64()).unwrap_or(8.0) as f32;
            ui.add_space(h);
        }
        "row" => {
            ui.horizontal(|ui| {
                if let Some(ch) = node.get("children").and_then(|x| x.as_array()) {
                    for c in ch {
                        render_node(ui, plugin, c, out);
                    }
                }
            });
        }
        "col" => {
            ui.vertical(|ui| {
                if let Some(ch) = node.get("children").and_then(|x| x.as_array()) {
                    for c in ch {
                        render_node(ui, plugin, c, out);
                    }
                }
            });
        }
        "button" => {
            let label = node.get("label").and_then(|x| x.as_str()).unwrap_or("按钮");
            if ui.button(label).clicked() {
                if let Some(action) = node.get("action").and_then(|x| x.as_str()) {
                    out.push(Dispatch::Action {
                        plugin: plugin.to_string(),
                        action: action.to_string(),
                    });
                }
            }
        }
        "progress" => {
            let v = node.get("value").and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
            ui.add(egui::ProgressBar::new(v.clamp(0.0, 1.0)).show_percentage());
        }
        "list" => {
            if let Some(items) = node.get("items").and_then(|x| x.as_array()) {
                for it in items {
                    ui.horizontal(|ui| {
                        ui.label("•");
                        render_node(ui, plugin, it, out);
                    });
                }
            } else if let Some(ch) = node.get("children").and_then(|x| x.as_array()) {
                for c in ch {
                    render_node(ui, plugin, c, out);
                }
            }
        }
        "toggle" => {
            let label = node.get("label").and_then(|x| x.as_str()).unwrap_or("");
            let cur = node.get("value").and_then(|x| x.as_bool()).unwrap_or(false);
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
            let label = node.get("label").and_then(|x| x.as_str()).unwrap_or("");
            let cur = node.get("value").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let opts: Vec<String> = node
                .get("options")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|o| o.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default();
            let mut sel = cur.clone();
            ui.horizontal(|ui| {
                ui.label(label);
                egui::ComboBox::from_id_salt(format!("{plugin}.{label}"))
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
            let label = node.get("label").and_then(|x| x.as_str()).unwrap_or("");
            let val = node.get("value").and_then(|x| x.as_str()).unwrap_or("");
            ui.horizontal(|ui| {
                ui.label(label);
                ui.label(egui::RichText::new(val).weak());
            });
        }
        other => {
            ui.label(format!("[{other}]"));
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
        "wgpu" => eframe::Renderer::Wgpu,
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
            .with_title("ADL 下载器"),
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
