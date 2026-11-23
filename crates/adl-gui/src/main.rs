//! ADL 图形界面：纯 Rust 原生 UI（egui / eframe），**不依赖 WebView2**。
//! 跨平台：Windows / Linux / macOS 均可编译运行。
//!
//! 本文件只负责“壳”（窗口与交互）；下载引擎在 `adl-core`，插件在 `adl-plugin`，
//! 配置与历史在 `adl-app`。浏览器接管用的本地 HTTP API 仍由 `api_server` 提供。

mod api_server;
mod notice;
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
    /// 后台（下载任务、插件线程）要弹给用户的消息。界面每帧取走，
    /// 因为那些线程拿不到 `App`，只能往这里塞。
    pub(crate) pending_notices: Mutex<Vec<(notice::Level, String, String)>>,
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
    /// 常驻的「插件」页：安装 / 启用停用 / 查看声明。
    ///
    /// 和下面的 `Plugin(..)` 是两件事：`Plugin` 是**插件自己声明的页面**
    /// （由插件返回组件树来画），而 `Plugins` 是宿主自己的管理页，插件没装
    /// 也能进。
    Plugins,
    /// 消息中心：下载完成 / 失败 / 插件通知留档在这里。
    Notices,
    /// 终端：命令行界面，替代原来的 Ctrl+K 浮层命令面板。
    ///
    /// 做成独立页面而不是浮层，是因为浮层每次只活一次、没有历史，
    /// 想翻上一条命令的输出就得重新跑一遍。终端页能留下滚动历史，
    /// 也更接近用户熟悉的 PowerShell。
    Terminal,
    Settings,
    Plugin(String, String),
}

/// 插件页的两个分段（对应 DSH 的"已安装 / 插件市场"）。
///
/// "插件市场"目前只有骨架：用户说以后要接自己的 GitHub 插件源，
/// 先把入口和文案占住，免得以后加进来时整页要重排。
/// （原先还有"发现"，用户 m11341 说用不上，已删除。）
#[derive(Clone, Copy, PartialEq, Eq)]
enum PluginsTab {
    Installed,
    Market,
}

/// 终端里一行输出的级别（只影响颜色）。
///
/// PowerShell 把输出分成几条流（Output / Warning / Error / Verbose …），
/// 我们只保留四条：警告单独一条流很有用 —— “插件被 safe 模式挡住了”
/// 既不是正常输出也不是错误，用红色会像出事了。
#[derive(Clone, Copy, PartialEq, Eq)]
enum LineKind {
    /// 启动横幅（比正文亮一档，和 PowerShell 的 banner 一个地位）
    Banner,
    /// 用户敲的命令回显
    Echo,
    /// 正常输出
    Out,
    /// 提示 / 帮助
    Info,
    /// 警告：能继续，但有事要说
    Warn,
    /// 错误
    Err,
}

/// 终端页的状态：一屏历史 + 当前输入 + 上下翻历史。
struct Terminal {
    lines: Vec<(LineKind, String)>,
    input: String,
    /// 按 ↑ 翻的历史（最新的在末尾）
    history: Vec<String>,
    /// 当前翻到第几条；`None` = 正在编辑新行
    hist_pos: Option<usize>,
    /// 请求焦点：刚按 Ctrl+K 进来、或者执行完一条命令后，光标要留在输入框
    focus: bool,
    /// Tab 补全状态。PowerShell 的 Tab 是**循环**的：按住 Tab 依次换下一个
    /// 候选，所以我们得记住这一轮是从哪一行开始的。
    comp: Option<Completion>,
}

/// 一轮 Tab 补全。
///
/// PowerShell 的 Tab 是**循环**的：按住不放依次换下一个候选。所以候选在
/// 第一次按 Tab 时就算好存下来，后面只推进 `idx`。候选存的是**整行**，
/// 不是片段 —— 这样二级命令（`plugin list`）也能一次替换干净。
struct Completion {
    cands: Vec<String>,
    idx: usize,
}

/// 终端输入框的固定 id。
///
/// 必须自己钉死：`TextEdit` 默认用"父布局 id + 序号"派生 id，虽然这里
/// 每帧结果一致，但我们要在**控件之外**（补全、翻历史）改它的光标，
/// 就得能拿到同一个 id 去 `TextEdit::load_state/store_state`。
const TERM_INPUT_ID: &str = "adl_term_input";

/// 把终端输入框的光标挪到行尾。
///
/// 补全/翻历史都是直接替换 `term.input` 整个字符串，光标状态却还留在
/// 旧位置：`h` 按 Tab 变成 `help` 之后，光标还卡在字符索引 1
/// （用户 m13765 报的就是这个）。所以每次程序化改内容都要手工复位。
fn term_caret_to_end(ctx: &egui::Context, text: &str) {
    let id = egui::Id::unique(TERM_INPUT_ID);
    if let Some(mut st) = egui::TextEdit::load_state(ctx, id) {
        // `CCursor` 的 index 是**字符**下标不是字节下标，
        // 所以中文目录/命令也不会把光标切在半个字上。
        st.cursor.set_char_range(Some(egui::text::CCursorRange::one(
            egui::text::CCursor::end_of_str(text),
        )));
        st.store(ctx, id);
    }
}

/// 终端第一级命令（Tab 补全和 help 共用一份，免得两处对不上）。
const TERM_COMMANDS: &[&str] = &[
    "help", "clear", "tasks", "add", "pause", "resume", "rm", "open", "settings", "set", "cd",
    "mode", "plugin", "history", "ver",
];

/// 算出 Tab 补全的候选（整行，不是片段）。
///
/// 只做"补全当前这个词"这一件事：补出来的都是 `前缀 + 完整候选`，
/// 所以 `plugin l` 补成 `plugin list` 而不会把 `plugin` 吃掉。
fn term_candidates(input: &str) -> Vec<String> {
    let line = input;
    // 行尾有空格 = 正在敲下一个词，当前词是空串
    let ends_space = line.ends_with(' ');
    let mut words: Vec<&str> = line.split_whitespace().collect();
    let cur = if ends_space {
        ""
    } else {
        words.pop().unwrap_or("")
    };
    let prefix = if words.is_empty() {
        String::new()
    } else {
        format!("{} ", words.join(" "))
    };

    let pool: Vec<&str> = match words.first().copied() {
        None => TERM_COMMANDS.to_vec(),
        Some("plugin") if words.len() == 1 => vec!["list", "run"],
        Some("set") if words.len() == 1 => vec!["dir", "connections", "limit"],
        Some("mode") if words.len() == 1 => vec!["safe", "trusted"],
        Some("pause") if words.len() == 1 => vec!["all"],
        // `tasks ls` 这种别名也补一下，省得记两套
        Some("tasks") if words.len() == 1 => vec!["ls"],
        // 第二层之后没什么可补的（插件 id、任务 id 都是动态的）
        Some(_) => Vec::new(),
    };

    pool.into_iter()
        .filter(|c| c.starts_with(cur))
        .map(|c| format!("{prefix}{c}"))
        .collect()
}

/// 终端输入行的实时着色。
///
/// 这就是 PSReadLine 干的事：**不是**等回车之后再去染输出，而是在输入框
/// 自己还不认识这行字的时候就按词法给它上色。挂到 `TextEdit::layouter`
/// 上，每敲一个字符都会被调一次。
///
/// 着色密度刻意保持很低 —— 真 PowerShell 的 prompt 行里只有命令名是绿的，
/// 参数和普通文本一个颜色。搞成彩虹反而不像终端了。
fn term_layouter(
    ui: &egui::Ui,
    text: &str,
    wrap_width: f32,
    ink_echo: egui::Color32,
    ink_out: egui::Color32,
    ink_path: egui::Color32,
) -> std::sync::Arc<egui::Galley> {
    let ink_warn = egui::Color32::from_rgb(0xE8, 0xC4, 0x6F);
    let font = egui::FontId::monospace(12.5);
    let fmt = |c: egui::Color32| egui::TextFormat {
        font_id: font.clone(),
        color: c,
        ..Default::default()
    };

    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = wrap_width;

    // 第二级子命令：`plugin list`、`set dir`、`mode safe` 这类。
    // 和 Tab 补全用的是同一套词，两处一起改才不会补得出命令却染不上色。
    const SUB: &[&str] = &[
        "list", "run", "dir", "connections", "limit", "safe", "trusted", "all", "ls",
    ];

    let mut idx = 0usize;
    for (wi, word) in text.split_whitespace().enumerate() {
        let start = match text[idx..].find(word) {
            Some(p) => idx + p,
            None => idx,
        };
        // 词与词之间的空白原样保留，否则缩进和多个空格会被着色吃掉
        if start > idx {
            job.append(&text[idx..start], 0.0, fmt(ink_out));
        }
        let color = if wi == 0 {
            if TERM_COMMANDS.contains(&word) {
                ink_echo
            } else {
                // 还不认识的命令先按正文画。真 PowerShell 也是执行之后才报红。
                ink_out
            }
        } else if word.starts_with('-') {
            ink_warn
        } else if wi == 1 && SUB.contains(&word) {
            ink_path
        } else {
            ink_out
        };
        job.append(word, 0.0, fmt(color));
        idx = start + word.len();
    }
    if idx < text.len() {
        job.append(&text[idx..], 0.0, fmt(ink_out));
    }
    // `ui.fonts()` 给的是 `&Fonts`，而 `layout_job` 要 `&mut`（它要写
    // 字形缓存）。`Painter::layout_job` 内部走 `fonts_mut`，是这里唯一
    // 能拿到可变引用的入口。
    ui.painter().layout_job(job)
}

impl Default for Terminal {
    fn default() -> Self {
        let mut lines = Vec::new();
        // 两行 banner，形状照 PowerShell 的启动横幅来：
        //   Windows PowerShell 5.1
        //   Copyright (C) Microsoft Corporation. All rights reserved.
        // 我们没版权行可写，第二行改成快捷键提示（同样两行、同样长短）。
        lines.push((LineKind::Banner, format!("ADL 终端 {}", env!("CARGO_PKG_VERSION"))));
        lines.push((
            LineKind::Info,
            "输入 help 看可用命令 · Tab 补全 · ↑/↓ 翻历史".to_string(),
        ));
        Self {
            lines,
            input: String::new(),
            history: Vec::new(),
            hist_pos: None,
            focus: false,
            comp: None,
        }
    }
}

impl Terminal {
    fn push(&mut self, kind: LineKind, text: impl Into<String>) {
        for line in text.into().lines() {
            self.lines.push((kind, line.to_string()));
        }
        // 只留最近 500 行：终端页没有分页，多了就纯占内存。
        if self.lines.len() > 500 {
            let cut = self.lines.len() - 500;
            self.lines.drain(0..cut);
        }
    }
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
    /// 单次下载的 User-Agent 覆盖；空 = 用设置里的默认值
    f_ua: String,
    // 设置表单
    s_dir: String,
    s_conn: u64,
    s_limit: u64,
    s_proxy: String,
    s_ua: String,
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
    /// 终端页状态（Ctrl+K 进入）
    term: Terminal,
    /// 插件页当前分段（已安装 / 插件市场 / 发现）
    plugins_tab: PluginsTab,
    // 托盘图标：持有即可保活（Windows 上 tray-icon 自己带消息线程）
    _tray: Option<TrayIcon>,
    // 自绘标题栏用的最大化状态（从 ViewportInfo 同步，点击时本地翻转以即时响应）
    maximized: bool,
    /// "正在退出"闸门：用户确认退出后放行 Close，避免被自己的关闭拦截挡回去。
    closing: bool,
    /// 弹窗淡入计时器
    fade: Fade,
    /// 侧栏竖条此刻画在哪（`None` = 还没初始化，首帧直接吸附到目标位置）
    nav_slide: Option<f32>,
    /// 换页时刻：驱动底色擦入与页面下刷。页签一变就重置。
    page_start: Option<Instant>,
    /// 上一帧的页签，用来检测换页
    page_last_tab: Option<Tab>,
    /// 本帧的换页进度（0.0..=1.0，缓出）。每帧在 `ui()` 开头算一次，
    /// 侧栏擦入和页面下刷共用同一个值，两者才像同一次切换。
    page_fill: f32,
    /// 消息中心（持久化到 `notices.json`）。
    notices: notice::Store,
    /// 右下角 toast：当前显示的那条 + 它出现的时间。
    toast: Option<(notice::Notice, Instant)>,
    /// 正在等系统目录选择框的结果。
    ///
    /// `SHBrowseForFolderW` 是模态阻塞调用，不能在有绘制的时候跑，
    /// 所以丢进独立线程、把结果从这个 channel 收回来。
    pick_rx: Option<std::sync::mpsc::Receiver<Option<std::path::PathBuf>>>,
}

/// 首次点关闭时问用户"退出还是最小化"。
struct CloseAsk {
    /// 勾选"记住我的选择"
    remember: bool,
}

/// 起一个线程去弹系统的「选择文件夹」对话框，立刻返回收结果的 channel。
///
/// `SHBrowseForFolderW` / `osascript choose folder` 都是**模态阻塞**的：
/// 直接在 `update` 里调用，整个界面会连重绘都停住（用户看到的是一张卡死的
/// 旧画面，连"取消"都点不了）。所以必须离开主线程，等结果回来再写回字段。
fn start_pick_folder() -> std::sync::mpsc::Receiver<Option<std::path::PathBuf>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let picked = platform::pick_folder("选择默认下载目录");
        // 接收端可能已经丢了（用户切换页面把状态清掉），忽略发送失败。
        let _ = tx.send(picked);
    });
    rx
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

/// 侧栏竖条跨行滑动时长。比弹窗稍长一点，滑得太快就看不出"滑"了。
const NAV_SLIDE_SECS: f32 = 0.18;

/// 选中底色淡入的时长，也用于页面内容的下刷淡入。
const NAV_FILL_SECS: f32 = 0.16;

/// 侧栏收起 / 展开的时长。
///
/// 比换页动效长一些：位移 212px 这种幅度，0.16 秒看完只会觉得"闪了一下"，
/// 0.22 秒才看得出是"被推出去"。
const SIDEBAR_SECS: f32 = 0.22;

/// 状态提示条显示时长与"末尾淡出"区间。
const STATUS_SECS: f32 = 6.0;
const STATUS_FADE: f32 = 0.45;

/// 右下角 toast 停留时长与末尾淡出区间。
///
/// 比状态条短：toast 盖在内容上，挡久了烦；重要的事都进「消息」页了。
const TOAST_SECS: f32 = 4.0;
const TOAST_FADE: f32 = 0.35;

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
        //   home | tasks | plugins | notices | terminal | settings | add | plugin:<pluginId>:<pageId>
        // `add` 现在打开弹窗而不是切页。
        let mut add_open = false;
        let initial_tab = match std::env::var("ADL_TAB").unwrap_or_default().as_str() {
            "add" => {
                add_open = true;
                Tab::Tasks
            }
            "home" => Tab::Home,
            "plugins" => Tab::Plugins,
            "notices" => Tab::Notices,
            "terminal" => Tab::Terminal,
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
            f_ua: String::new(),
            s_dir: cfg.dir.clone(),
            s_conn: cfg.connections as u64,
            s_limit: cfg.limit,
            s_proxy: cfg.proxy.clone(),
            s_ua: cfg.user_agent.clone(),
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
            term: Terminal::default(),
            plugins_tab: PluginsTab::Installed,
            _tray: tray,
            maximized: false,
            closing: false,
            fade: Fade::default(),
            nav_slide: None,
            page_start: None,
            page_last_tab: None,
            page_fill: 1.0,
            notices: notice::Store::load(),
            toast: None,
            pick_rx: None,
        }
    }

    fn set_status(&mut self, msg: impl Into<String>, err: bool) {
        self.status = Some((msg.into(), err, Instant::now()));
    }

    /// 记一条消息：落盘 + 立刻弹 toast。
    ///
    /// `set_status` 那条 6 秒就消失，重要事件（下完 / 失败 / 插件通知）
    /// 走这里才翻得回来。
    fn notify(&mut self, level: notice::Level, title: impl Into<String>, body: impl Into<String>) {
        let title = title.into();
        let body = body.into();
        self.notices.push(level, title.clone(), body.clone());
        self.notices.save();
        self.toast = Some((
            notice::Notice {
                id: 0,
                level,
                title,
                body,
                at: adl_app::history::now_unix(),
                read: false,
            },
            Instant::now(),
        ));
    }

    fn refresh_plugin_snapshot(&mut self) {
        if self.plugin_snapshot_at.elapsed() < Duration::from_millis(1200) {
            return;
        }
        let hooks = self.gui.hooks.lock().unwrap().clone();
        self.plugin_snapshot = hooks.ui_snapshot(&load_plugin_settings());
        if std::env::var_os("ADL_DEBUG_PLUGINS").is_some() {
            eprintln!(
                "[dbg] all = {}",
                serde_json::to_string(&self.plugin_snapshot.get("all")).unwrap_or_default()
            );
        }
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
        let ua = self.f_ua.trim();
        let args = json!({
            "url": self.f_url.trim(),
            "out": if out.is_empty() { Value::Null } else { json!(out) },
            // 留空 = Value::Null，引擎会退回设置里的默认 UA
            "user_agent": if ua.is_empty() { Value::Null } else { json!(ua) },
        });
        match add_task(&self.gui, &args) {
            Ok(v) => {
                let id = v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
                self.set_status(format!("已添加任务 #{id}"), false);
                self.f_url.clear();
                self.f_out.clear();
                self.f_ua.clear();
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
            // 空字符串 = 回到内置默认 UA（引擎侧 user_agent_opt() 返回 None）
            "user_agent": self.s_ua,
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
/// 侧栏左右内边距，必须与 `sidebar_frame()` 的 `Margin::symmetric(12, 14)`
/// 保持一致：面板内的可用宽度已经是 `sidebar_w - 2*PAD`，内容再按
/// `sidebar_w` 定宽就会溢出 24px，导航项的右侧圆角正好被面板裁平。
const SIDEBAR_PAD: f32 = 12.0;
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
                "quit" => {
                    // 托盘「退出」是用户的明确意图，直接放行。
                    // 不设 closing 的话，Close 会再被上面那段拦截抓住、弹一次询问框
                    // —— 表现就是"点了退出没反应，还得再点 X"。
                    self.closing = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }

                _ => {}
            }
        }

        // Ctrl+K：跳到终端页（不再是浮层命令面板）。
        //
        // 再按一次回到原来的页面 —— 但只在"当前就在终端"时才回退，
        // 否则从别的页面按 Ctrl+K 会被误判成"关闭终端"而什么也没发生。
        if ctx.input(|i| i.key_pressed(egui::Key::K) && i.modifiers.ctrl) {
            if self.tab == Tab::Terminal {
                self.tab = Tab::Tasks;
            } else {
                self.tab = Tab::Terminal;
                self.term.focus = true;
            }
        }

        self.refresh_plugin_snapshot();

        // 目录选择框的结果（用户可能还在选，`try_recv` 没东西就是还没选完）。
        // 选完就回填到设置页的输入框里 —— 这里只改草稿 `s_dir`，
        // 还得点「保存设置」才真正落盘，跟手打路径一个待遇。
        if let Some(rx) = &self.pick_rx {
            if let Ok(picked) = rx.try_recv() {
                if let Some(p) = picked {
                    // 存成带正斜杠的路径：TOML 里反斜杠是转义符，
                    // 用户手改配置文件时最容易在这里踩坑。
                    self.s_dir = p.display().to_string().replace('\\', "/");
                }
                self.pick_rx = None;
            } else {
                // 对话框开着的时候界面也得继续呼吸（不然用户以为程序死了）
                ctx.request_repaint_after(std::time::Duration::from_millis(120));
            }
        }

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
        // 折叠键。
        //
        // 但"硬切"会闪一下：侧栏整块消失的同时外壳横移 212px，观感像窗口
        // 抖了一下。所以宽度改成插值 —— 面板一直存在，只是 `exact_size`
        // 从 `sidebar_w()` 收到 0。
        //
        // 关键：**面板内容按原宽排版、靠裁剪裁掉溢出**。如果让宽度真去挤压
        // 内部布局，导航文字会在中途换行、按钮被压扁，动起来像坏的。
        //
        // 换页进度也在这一层算（而不是在 `sidebar` 里），因为侧栏收起时页面
        // 照样要下刷 —— 时钟必须独立于侧栏是否存在。
        self.page_fill = self.tick_page(&ctx);

        let target_w = if self.sidebar_collapsed { 0.0 } else { sidebar_w() };
        let mut w = ctx.animate_value_with_time(
            egui::Id::unique("adl_sidebar_width"),
            target_w,
            SIDEBAR_SECS,
        );
        // `animate_value_with_time` 在动画结束后仍返回目标值，这里夹一下防止
        // 浮点残留让面板留一条 0.4px 的缝。
        if (w - target_w).abs() < 0.5 {
            w = target_w;
        } else {
            ctx.request_repaint();
        }
        if w > 0.5 {
            let reveal = (w / sidebar_w()).clamp(0.0, 1.0);
            egui::Panel::left("adl_sidebar")
                .exact_size(w)
                .resizable(false)
                .show_separator_line(false)
                .frame(sidebar_frame())
                .show(ui, |ui| {
                    // 裁剪 + 定宽：内容始终按完整侧栏宽排版，多出来的部分被
                    // 面板边缘切掉，于是看起来像"侧栏被推出去/拉进来"。
                    ui.set_clip_rect(ui.max_rect());
                    // 必须减掉两侧内边距（sidebar_frame 是 symmetric(12,_)）：
                    // 直接写 sidebar_w() 会让内容比可用宽度宽 24px，导航项
                    // 右侧圆角正好被面板裁掉，看着像"高亮条被切平了"。
                    ui.set_width(sidebar_w() - SIDEBAR_PAD * 2.0);
                    // 宽度只剩一半时内容还完整，只用透明度跟着收，避免
                    // 出现"半截文字卡在边上"。
                    ui.set_opacity((reveal * 1.6).clamp(0.0, 1.0));
                    self.sidebar(ui);
                });
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

                // 换页淡入：只做透明度，**不做位移**。
                //
                // 早先这里还有一句 `ui.add_space((1.0 - pf) * 8.0)` 想让内容
                // "从上往下落"。结果每帧布局高度都在变，内容整体位置每帧挪
                // 零点几像素，配合半透明就是满屏抖 —— 用户报的"淡入淡出会
                // 闪烁"就是它。8px 的方向感换不来这么明显的瑕疵，删掉。
                let pf = self.page_fill.clamp(0.0, 1.0);
                ui.scope(|ui| {
                    // 这三个页面自己管滚动，外面**不能**再套页面级 ScrollArea：
                    //  - 终端：输入行必须永远停在看得见的地方，套两层会被视口切掉一半；
                    //  - 任务列表 / 消息：用 show_rows 只渲染视口内的行，
                    //    它需要自己拿到滚动区才能知道视口位置（见 adl_ui::virtual_table）。
                    //
                    // 而且这三页**不参与淡入**：终端是一整块深色底，半透明时
                    // 会透出下面的浅色页面变成一块灰板；虚拟列表首帧还在
                    // 量尺寸，跟着一起半透明就会闪。
                    if matches!(
                        self.tab,
                        Tab::Terminal | Tab::Tasks | Tab::Notices
                    ) {
                        match self.tab.clone() {
                            Tab::Terminal => self.page_terminal(ui),
                            Tab::Tasks => self.page_tasks(ui),
                            Tab::Notices => self.page_notices(ui),
                            _ => {}
                        }
                        return;
                    }
                    ui.set_opacity(pf);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            match self.tab.clone() {
                                Tab::Home => self.page_home(ui),
                                Tab::Plugins => self.page_plugins(ui),
                                Tab::Settings => self.page_settings(ui),
                                Tab::Plugin(_, _) => self.page_plugin(ui),
                                // 上面已经 return 了，这里只是让 match 穷尽。
                                Tab::Tasks | Tab::Notices | Tab::Terminal => {}
                            }
                        });
                });
            });

        // 后台线程（下载任务、插件）投递过来的消息：每帧取空。
        // 放在弹窗之前取，这样同帧就能弹 toast。
        let incoming: Vec<(notice::Level, String, String)> = {
            let mut q = self.gui.pending_notices.lock().unwrap();
            std::mem::take(&mut *q)
        };
        for (level, title, body) in incoming {
            self.notify(level, title, body);
        }

        // 右下角 toast：4 秒后自己淡出消失，点了跳「消息」页。
        self.show_toast(&ctx);

        // 弹窗淡入：先根据"当前开着哪个 overlay"确定身份，再决定透明度。
        let overlay = if self.add_open {
            1
        } else if self.close_ask.is_some() {
            2
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

        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

impl App {
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
            self.f_ua.clear();
            self.add_open = true;
        }
        ui.add_space(12.0);

        // ── 主导航 ──
        //
        // 这一段写法有点绕，原因是"小绿条要跨行滑动"：竖条不能由每一行自己画
        // —— 切页那一帧旧选中行已经不渲染了，竖条会原地消失再在下一行冒出来，
        // 就是用户说的"点任务列表时直接出现"。所以这里只记录选中行在哪，
        // 等整栏画完再用 `nav_bar` 统一画一次。
        let fill = self.page_fill;
        let mut sel_geom: Option<(f32, f32, f32)> = None;
        let mut track = |ui: &mut egui::Ui, sel: bool, label: &str| {
            let r = adl_ui::widgets::nav_item_anim(ui, sel, label, fill);
            if sel {
                sel_geom = Some((r.rect.left(), r.rect.top(), r.rect.height()));
            }
            r.clicked()
        };

        if track(ui, self.tab == Tab::Home, "主页") {
            self.tab = Tab::Home;
        }
        ui.add_space(2.0);
        if track(ui, self.tab == Tab::Tasks, "任务列表") {
            self.tab = Tab::Tasks;
        }
        ui.add_space(2.0);
        // 常驻插件页：装了什么、启用没启用、声明了哪些 hook，都在这儿看。
        // 它上面的"插件页面"分组是插件自己声明的页，两者不冲突。
        if track(ui, self.tab == Tab::Plugins, "插件") {
            self.tab = Tab::Plugins;
        }
        ui.add_space(2.0);
        // 终端：原 Ctrl+K 命令面板改成的独立页面，有滚动历史。
        if track(ui, self.tab == Tab::Terminal, "终端") {
            self.tab = Tab::Terminal;
            self.term.focus = true;
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
            // 分隔只留一条极淡的线，不再写"插件页面"这种小字分组标题。
            //
            // 原来那行灰色小字既难看清，又把插件页压成了"二级入口"，而用户
            // 的心智是"插件页也是页面"，与主页/任务列表平级。现在插件声明的
            // 页面直接排进主导航，靠一条 `hairline` 做视觉分隔就够了。
            ui.add_space(10.0);
            adl_ui::widgets::hairline(ui);
            ui.add_space(8.0);
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
                if track(ui, selected, &title) {
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
                if track(ui, selected, &label) {
                    self.tab = Tab::Plugin("adl".to_string(), tab_id);
                }
                ui.add_space(2.0);
            }
            if has_more {
                if track(ui, false, "更多设置…") {
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
            // bottom_up 从下往上排，这里加的就是"距离窗口底边"的留白。
            // 只给 2 会贴底：模式胶囊的下半圈圆角正好压在窗口边缘上。
            ui.add_space(10.0);
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
            if track(ui, self.tab == Tab::Settings, "设置") {
                self.tab = Tab::Settings;
            }
            ui.add_space(2.0);
            // 消息紧挨在设置上方：`bottom_up` 里后加的在上，所以放在设置之后。
            // 未读数直接写进标题，不然用户永远不会想到点进来。
            let unread = self.notices.unread();
            let label = if unread > 0 {
                format!("消息 ({unread})")
            } else {
                "消息".to_string()
            };
            if track(ui, self.tab == Tab::Notices, &label) {
                self.tab = Tab::Notices;
            }
            ui.add_space(4.0);
        });

        // 整栏画完，最后补画竖条 —— 它要跨行滑动，所以必须由这里统一决定位置。
        //
        // `slide` 是插值后的 y（朝 `top` 缓动）。首帧 `nav_slide` 是 `None`，
        // 直接吸附到目标位置，避免进侧栏时竖条从顶上滑下来。
        if let Some((left, top, h)) = sel_geom {
            let ctx = ui.ctx();
            let slide = match self.nav_slide {
                Some(_) => {
                    let t = ctx.animate_value_with_time(
                        egui::Id::unique("adl_nav_slide"),
                        top,
                        NAV_SLIDE_SECS,
                    );
                    if (t - top).abs() > 0.5 {
                        ctx.request_repaint();
                    }
                    t
                }
                None => top,
            };
            self.nav_slide = Some(slide);
            adl_ui::widgets::nav_bar(ui, left, slide, h);
        }
    }

    /// 换页进度：0.0..=1.0，缓出。
    ///
    /// 页签一变就重置起点，于是选中底色重新从左到右铺一遍、页面内容同时
    /// 从上往下刷出来。两者共用这一个值，才像"一次切换"而不是两件事。
    fn tick_page(&mut self, ctx: &egui::Context) -> f32 {
        if self.page_last_tab.as_ref() != Some(&self.tab) {
            self.page_last_tab = Some(self.tab.clone());
            self.page_start = Some(Instant::now());
        }
        let Some(t0) = self.page_start else {
            return 1.0;
        };
        let t = (t0.elapsed().as_secs_f32() / NAV_FILL_SECS).clamp(0.0, 1.0);
        if t < 1.0 {
            ctx.request_repaint();
        }
        1.0 - (1.0 - t) * (1.0 - t)
    }

    fn page_title(&self) -> String {
        match &self.tab {
            Tab::Home => "主页".to_string(),
            Tab::Tasks => "任务列表".to_string(),
            Tab::Plugins => "插件".to_string(),
            Tab::Notices => "消息".to_string(),
            Tab::Terminal => "终端".to_string(),
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

        // 侧栏收起后标题栏不再补画 "ADL"（用户要求）。
        // 收起时整个界面只剩折叠键与窗口三键，品牌字跟着侧栏一起消失，
        // 这样"收起"才是真的收干净，而不是换个地方继续占位。

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

        // 行数可能上千（重装系统后补历史时就是），整表渲染会让每帧都画全部行。
        // virtual_table 自带 ScrollArea + show_rows，只画视口里的那几十行。
        adl_ui::table::virtual_table(ui, &cols, tasks.len(), |ui, i, w| {
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
            // 背板也要跟着淡。`ui.set_opacity` 管不到它 —— 背板是在
            // `Area::show` 里、进内容闭包之前就画好的。不传这个参数的话
            // 观感是"背板啪一下变暗，内容慢慢浮出来"，像分两段渲染。
            .backdrop_color(fade_color(egui::Color32::from_black_alpha(100), a))
            .show(ctx, |ui| {
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

            ui.add_space(12.0);
            ui.label("User-Agent");
            ui.add_space(4.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.f_ua)
                    .hint_text("可选，留空用设置里的默认 UA（已伪装成 Chrome）")
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(4.0);
            adl_ui::widgets::hint(
                ui,
                "有些站点只认特定客户端（比如某网盘、某论坛的专用 UA），这里填了这一单就只用它。",
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
            .backdrop_color(fade_color(egui::Color32::from_black_alpha(100), a))
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
            ui.add_space(12.0);

            // 勾选框在上、按钮在下 —— 和 Windows 自己的确认框一致。
            // 原来按钮在上、勾选框在下，读起来像"先做决定再看到选项"。
            if let Some(ask) = self.close_ask.as_mut() {
                ui.checkbox(&mut ask.remember, "记住我的选择（以后不再询问）");
            }
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
            }
            // 不管勾没勾"记住"，选完之后都要**立刻把这一步做完**。
            //
            // 这里的坑：`ui()` 开头那段关闭拦截看到 `close_requested()` 时，
            // 只要 `closing` 还是 false 就会再弹一次框（并且 CancelClose）。
            // 原来"退出程序"只是发了个 Close 就把弹窗关了，于是
            // 关窗 → 又弹一次 → 再点一次才能真退。用户说的"要点两遍"
            // 就是这个；勾了"记住"的路径更糟 —— 只写了配置，连 Close 都没发。
            if action == "tray" {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else {
                self.closing = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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

        // ── 其它：目录、代理与 UA ──
        // 两个后缀按钮：
        //  - `pick_dir` 弹系统目录选择框（手打 Windows 路径很容易漏反斜杠）
        //  - `fill_ua`   一键填入内置 Chrome 串，用户在这个基础上改，
        //                比"从 blank 开始自己拼一整串 UA"现实得多
        let mut fields = vec![
            adl_ui::Field::text("默认下载目录", &mut self.s_dir)
                .hint("留空 = 当前目录")
                .action("选择…", "pick_dir"),
            adl_ui::Field::text("HTTP 代理", &mut self.s_proxy)
                .hint("http://127.0.0.1:7890（留空 = 直连）"),
            adl_ui::Field::text("User-Agent", &mut self.s_ua)
                .hint("留空 = 内置的 Chrome 串（推荐）")
                .action("填入 Chrome", "fill_ua")
                .note(
                    "有些站点只认特定客户端，可以在这里填整串 UA；\
                     单次覆盖在「新建下载」里填",
                ),
        ];
        if let Some(hit) = adl_ui::form(ui, "其它", &mut fields) {
            match hit.as_str() {
                // 目录选择是模态的，会阻塞到用户选完 —— 必须丢线程里，
                // 否则整个界面（包括"取消"按钮）都没反应。
                "pick_dir" => {
                    self.pick_rx = Some(start_pick_folder());
                }
                "fill_ua" => {
                    self.s_ua = adl_core::http::DEFAULT_UA.to_string();
                }
                _ => {}
            }
        }

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

    /// 常驻的「插件」页：装了什么、启用没启用、声明了哪些 hook。
    ///
    /// 这一版只做骨架（用户原话"先做大概，不用全部功能做了先"）：能看清单、
    /// 能开关、能打开插件目录、能刷新。**没有**在线安装/卸载/更新。
    fn page_plugins(&mut self, ui: &mut egui::Ui) {
        ui.add_space(2.0);

        let mut all: Vec<Value> = self
            .plugin_snapshot
            .get("all")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();

        // 顶部：目录位置 + 两个动作
        let root = {
            let cfg = self.gui.cfg.lock().unwrap();
            plugins_root(&cfg)
        };

        // 停用的插件不在快照里（没被加载），得从目录补回来，否则停用一次
        // 就再也点不到「启用」了。
        {
            let known: Vec<String> = all
                .iter()
                .filter_map(|p| p.get("id").and_then(|x| x.as_str()).map(String::from))
                .collect();
            all.extend(disabled_plugin_entries(root.as_ref(), &known));

            // 启用的排前面，同组按 id 排，列表才不会因为开关一次就跳动。
            let enabled_map: Vec<bool> = {
                let cfg = self.gui.cfg.lock().unwrap();
                all.iter()
                    .map(|p| {
                        let id = p.get("id").and_then(|x| x.as_str()).unwrap_or("");
                        cfg.plugin_enabled(id)
                    })
                    .collect()
            };
            let mut idx: Vec<usize> = (0..all.len()).collect();
            idx.sort_by(|&a, &b| {
                let id = |i: usize| {
                    all[i]
                        .get("id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string()
                };
                (!enabled_map[a], id(a)).cmp(&(!enabled_map[b], id(b)))
            });
            all = idx.into_iter().map(|i| all[i].clone()).collect();
        }
        let mut open_dir = false;
        let mut refresh = false;
        let mut open_market_cfg = false;

        // ── 顶部：说明 + 右侧刷新（"添加插件"已去掉：添加就是往目录里放
        // 文件夹，按钮只能把目录打开，插件目录卡片里已经有同一个按钮了）──
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("安装、启用和配置插件")
                    .small()
                    .color(muted()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("刷新").clicked() {
                    refresh = true;
                }
            });
        });
        ui.add_space(10.0);

        // ── 两段切换 ──
        let tabs = [
            (PluginsTab::Installed, "已安装"),
            (PluginsTab::Market, "插件市场"),
        ];
        if let Some(t) = adl_ui::widgets::segmented(ui, self.plugins_tab, &tabs) {
            self.plugins_tab = t;
        }
        ui.add_space(12.0);

        match self.plugins_tab {
            PluginsTab::Installed => {
        adl_ui::widgets::card(ui, |ui| {
            adl_ui::widgets::section_title(ui, "插件目录");
            ui.add_space(4.0);
            // 没有插件目录时 build_hooks 会退化成空集合，这里如实说清楚
            // 目录在哪（也可能是"还没建出来"）。
            let shown = root
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "（未配置）".to_string());
            ui.label(
                egui::RichText::new(shown)
                    .font(egui::FontId::monospace(12.5))
                    .color(pal().ink),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("打开插件目录").clicked() {
                    open_dir = true;
                }
                if ui.button("重新扫描").clicked() {
                    refresh = true;
                }
            });
            ui.add_space(6.0);
            adl_ui::widgets::hint(
                ui,
                "把插件文件夹（含 plugin.toml）放进这个目录，再点「重新扫描」就会出现在下面。\
                 插件只在启动/重扫时加载，运行中的任务用不上新加载的插件。",
            );
        });

        if open_dir {
            if let Some(dir) = root.as_ref() {
                match platform::open_path(dir) {
                    Ok(()) => {}
                    Err(e) => self.set_status(format!("打开目录失败: {e}"), true),
                }
            }
        }
        if refresh {
            // 重建 hooks + 让快照立刻失效，否则页面要等 1.2 秒才更新。
            let new_cfg = self.gui.cfg.lock().unwrap().clone();
            *self.gui.hooks.lock().unwrap() = build_hooks(&new_cfg);
            self.plugin_snapshot_at = Instant::now() - Duration::from_secs(3600);
            self.set_status("已重新扫描插件目录", false);
        }

        ui.add_space(14.0);

        if all.is_empty() {
            adl_ui::widgets::empty_state(
                ui,
                "还没装任何插件。内置的 ua-fixer / auto-concurrency / notify-on-complete / \
                 block-html 在程序目录的 plugins/ 下。",
            );
            return;
        }

        // 启停要等所有卡片画完再落盘：卡片闭包里已经借了 self.gui 的锁，
        // 在闭包里写配置容易死锁（同一把 Mutex 重复 lock）。
        let mut toggle: Option<(String, bool)> = None;

        // 分两组显示：内置插件是软件自带的功能（id 都以 com.adl. 开头），
        // 在插件页里只能看、不能关 —— 关掉 ua-fixer 之类的没有意义，
        // 只会让人以为程序坏了。用户自己装的才给开关。
        let builtin: Vec<Value> = all.iter().filter(|p| is_builtin(p)).cloned().collect();
        let external: Vec<Value> = all.iter().filter(|p| !is_builtin(p)).cloned().collect();

        for (group, list, can_toggle) in [
            ("内置插件", &builtin, false),
            ("已安装插件", &external, true),
        ] {
            if list.is_empty() {
                continue;
            }
            adl_ui::widgets::section_title(ui, group);
            ui.add_space(2.0);
            adl_ui::widgets::hint(
                ui,
                if can_toggle {
                    "你自己放进插件目录的插件，可以随时启用或停用。"
                } else {
                    "程序自带的功能，已经按默认调好，这里只做展示。"
                },
            );
            ui.add_space(8.0);
            for p in list {
                let id = p.get("id").and_then(|x| x.as_str()).unwrap_or("");
                if id.is_empty() {
                    continue;
                }
                let name = p.get("name").and_then(|x| x.as_str()).unwrap_or(id);
                let version = p.get("version").and_then(|x| x.as_str()).unwrap_or("");
                let desc = p.get("description").and_then(|x| x.as_str()).unwrap_or("");
                let has_ui = p.get("has_ui").and_then(|x| x.as_bool()).unwrap_or(false);
                let trusted = p.get("trusted").and_then(|x| x.as_bool()).unwrap_or(false);
                let enabled = {
                    let d = p
                        .get("enabled_by_default")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(true);
                    self.gui.cfg.lock().unwrap().plugin_enabled_or(id, d)
                };
                let hooks = str_list(p.get("hooks"));
                let active = str_list(p.get("active_hooks"));
                // 权限是"声明"，不等于授权：safe 模式下 Transform 类 hook 不生效，
                // 所以同时显示"声明了几个"和"当前生效几个"。
                let perms = permission_labels(p.get("permissions"));

                adl_ui::widgets::card(ui, |ui| {
                    ui.horizontal(|ui| {
                        // 必须显式给色：`.strong()` 走 `visuals.strong_text_color()`，
                        // 也就是 `widgets.active.text_color()` —— 我们的主题把 active 设为
                        // 白色（青绿按钮上要白字），于是不带 `.color()` 的 strong 文本
                        // 会画成白字，在白卡上整行消失。
                        let title_color = if enabled { ink() } else { muted() };
                        ui.label(
                            egui::RichText::new(name)
                                .size(14.5)
                                .strong()
                                .color(title_color),
                        );
                        if !version.is_empty() {
                            ui.label(
                                egui::RichText::new(format!("v{version}"))
                                    .small()
                                    .color(muted()),
                            );
                        }
                        ui.add_space(6.0);
                        if !enabled {
                            adl_ui::widgets::badge(ui, "已停用", pal().muted);
                        } else if trusted {
                            adl_ui::widgets::badge(ui, "信任模式", pal().warn);
                        } else {
                            adl_ui::widgets::badge(ui, "安全模式", pal().accent);
                        }
                        if has_ui {
                            adl_ui::widgets::badge(ui, "有界面", pal().accent);
                        }
                        // 开关放最右：和 DSH 的插件页一致（名称在左、开关在右），
                        // 一排扫下去就能看出哪些开着。
                        // 内置插件照常显示整张卡片，只是右边**什么都不画** ——
                        // 原来这里放一个「内置」badge，用户 m11933 说看着像
                        // 另一种东西，不如让它们和普通插件长一样、单纯没有开关。
                        if can_toggle {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if adl_ui::widgets::switch(ui, enabled).clicked() {
                                        toggle = Some((id.to_string(), !enabled));
                                    }
                                },
                            );
                        }
                    });

                    if !desc.is_empty() {
                        ui.add_space(3.0);
                        ui.label(
                            egui::RichText::new(desc)
                                .size(12.5)
                                .color(if enabled { pal().ink } else { muted() }),
                        );
                    }

                    ui.add_space(6.0);
                    // hook 行：生效的用主色，被安全模式挡住的用灰色并标注。
                    if !hooks.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            ui.label(egui::RichText::new("hook").small().color(muted()));
                            for h in &hooks {
                                let on = active.iter().any(|a| a == h);
                                let short = h.trim_start_matches("on");
                                if on {
                                    adl_ui::widgets::badge(ui, short, pal().accent);
                                } else {
                                    adl_ui::widgets::badge(ui, short, pal().muted);
                                }
                            }
                        });
                    }
                    if !perms.is_empty() {
                        ui.add_space(4.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            ui.label(egui::RichText::new("权限").small().color(muted()));
                            for (label, on) in &perms {
                                if *on {
                                    adl_ui::widgets::badge(ui, label, pal().warn);
                                } else {
                                    adl_ui::widgets::badge(ui, label, pal().muted);
                                }
                            }
                        });
                    }

                });
                ui.add_space(8.0);
            }
            ui.add_space(4.0);
        }

        if let Some((id, want)) = toggle {
            let mut cfg = self.gui.cfg.lock().unwrap();
            cfg.plugins
                .insert(id.clone(), adl_app::config::PluginCfg { enabled: want });
            if let Err(e) = cfg.save() {
                drop(cfg);
                self.set_status(format!("保存插件开关失败: {e}"), true);
            } else {
                let new_cfg = cfg.clone();
                drop(cfg);
                *self.gui.hooks.lock().unwrap() = build_hooks(&new_cfg);
                self.plugin_snapshot_at = Instant::now() - Duration::from_secs(3600);
                self.set_status(
                    if want {
                        format!("已启用插件 {id}")
                    } else {
                        format!("已停用插件 {id}")
                    },
                    false,
                );
            }
        }
            }
            PluginsTab::Market => {
                // 插件市场目前是空壳：源地址还没定（用户计划接自己的 GitHub
                // 仓库），所以这里只把"将来会有什么"说清楚，不做假列表 ——
                // 假数据会让人以为已经能装了。
                adl_ui::widgets::card(ui, |ui| {
                    adl_ui::widgets::section_title(ui, "插件市场");
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(
                            "还没有配置插件市场源。\n\
                             之后会接一个从 GitHub 拉取插件索引的官方源，在这里浏览并一键安装。",
                        )
                        .size(12.5)
                        .color(muted()),
                    );
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("来源配置").clicked() {
                            // 现在唯一能配的"来源"就是本地插件目录，直接带用户去，
                            // 比留一个点了没反应的按钮好。
                            open_market_cfg = true;
                        }
                    });
                    ui.add_space(8.0);
                    adl_ui::widgets::hint(
                        ui,
                        "插件市场需要插件源索引（JSON）与签名校验，等官方源就绪后开放。",
                    );
                });
            }
        }

        if open_market_cfg {
            // 直接给本地插件目录：目前"来源"只有这一个。
            if let Some(dir) = root.as_ref() {
                if let Err(e) = platform::open_path(dir) {
                    self.set_status(format!("打开目录失败: {e}"), true);
                }
            }
        }
    }

    /// 消息中心：下载完成 / 失败 / 插件通知留档在这里。
    ///
    /// 时间倒序（新的在上面）：追事件的时候人总是想知道"刚才发生了什么"。
    fn page_notices(&mut self, ui: &mut egui::Ui) {
        let unread = self.notices.unread();
        let total = self.notices.items.len();

        let mut mark_read = false;
        let mut clear = false;
        adl_ui::widgets::card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(if total == 0 {
                        "还没有消息".to_string()
                    } else if unread > 0 {
                        format!("共 {total} 条，{unread} 条未读")
                    } else {
                        format!("共 {total} 条，全部已读")
                    })
                    .size(13.0)
                    .color(ink()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(total > 0, egui::Button::new("清空")).clicked() {
                        clear = true;
                    }
                    if ui
                        .add_enabled(unread > 0, egui::Button::new("全部已读"))
                        .clicked()
                    {
                        mark_read = true;
                    }
                });
            });
            ui.add_space(4.0);
            adl_ui::widgets::hint(
                ui,
                "下载完成、下载失败、插件通过 notify 权限发来的提醒都会记在这里。\
                 最多保留最近 200 条。",
            );
        });

        if clear {
            self.notices.clear();
            self.notices.save();
            self.toast = None;
        } else if mark_read {
            self.notices.mark_all_read();
            self.notices.save();
            self.toast = None;
        }

        if total == 0 {
            ui.add_space(10.0);
            adl_ui::widgets::empty_state(ui, "消息是空的。下完一个文件、或者某个插件提醒你的时候，会出现在这里。");
            return;
        }

        ui.add_space(10.0);
        // 消息条数被 Store 卡在 200 条以内，量级可控；这里用普通 ScrollArea
        // 就够了（不为它单独做变高虚拟化）。但滚动区必须在这一层建：
        // 外层页面级 ScrollArea 对 Tab::Notices 已经关掉了，否则
        // 表头那张卡会跟着消息一起滚走。
        let items: Vec<notice::Notice> = self.notices.items.iter().rev().cloned().collect();
        egui::ScrollArea::vertical()
            .id_salt("adl_notices_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
        for n in &items {
            // 用 alert 组件的样式（底色块 + 左侧色条）而不是白卡 + 圆点：
            // 一屏十几条时，底色比一个小圆点更快区分"完成/注意/失败"。
            let (bar, bg) = match n.level {
                notice::Level::Info => (pal().accent, pal().accent_soft),
                notice::Level::Ok => (egui::Color32::from_rgb(0x2E, 0x7D, 0x6B), pal().ok_bg),
                notice::Level::Warn => (pal().warn, pal().warn_bg),
                notice::Level::Error => (pal().danger, pal().err_bg),
            };
            // 已读的只把标题压淡，底色和正文保持原样：整条一起乘 alpha
            // 会让旧记录彻底看不清（实测 0.5 时正文灰到读不出来），
            // 而这里的目的是"扫一眼能分出读没读"，不是"藏起来"。
            let resp = egui::Frame::new()
                .fill(bg)
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin::symmetric(14, 9))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(&n.title)
                                .size(13.5)
                                .strong()
                                .color(if n.read { muted() } else { ink() }),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.label(
                                    egui::RichText::new(notice::ago(n.at))
                                        .small()
                                        .color(muted()),
                                );
                                ui.add_space(8.0);
                                adl_ui::widgets::badge(ui, n.level.label(), bar);
                            },
                        );
                    });
                    if !n.body.is_empty() {
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new(&n.body).size(12.5).color(pal().ink));
                    }
                });
            // 左侧色条画在 Frame 之外，避免和内容抢布局（同 alert 组件）。
            let r = resp.response.rect;
            ui.painter().rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(r.left() + 5.0, r.top() + 8.0),
                    egui::vec2(3.0, (r.height() - 16.0).max(0.0)),
                ),
                egui::CornerRadius::same(2),
                if n.read { fade_color(bar, 0.45) } else { bar },
            );
            ui.add_space(8.0);
        }
            });
    }

    /// 右下角 toast：4 秒后淡出。
    ///
    /// 刻意不拦截点击（点别处照常生效），但点它本身会跳到「消息」页 ——
    /// 那种"刚才好像弹了个什么"的时候，一口气就能翻到全部。
    fn show_toast(&mut self, ctx: &egui::Context) {
        let Some((n, at)) = self.toast.clone() else {
            return;
        };
        let el = at.elapsed().as_secs_f32();
        if el > TOAST_SECS {
            self.toast = None;
            return;
        }
        // 最后 0.35 秒淡出；淡出期间要持续请求重绘，否则窗口静止时会停在半透明。
        let alpha = ((TOAST_SECS - el) / TOAST_FADE).clamp(0.0, 1.0);
        if alpha < 1.0 {
            ctx.request_repaint();
        }

        let (dot, tone) = match n.level {
            notice::Level::Info => (pal().accent, pal().accent),
            notice::Level::Ok => (egui::Color32::from_rgb(0x2E, 0x7D, 0x6B), pal().accent),
            notice::Level::Warn => (pal().warn, pal().warn),
            notice::Level::Error => (pal().danger, pal().danger),
        };

        let mut clicked = false;
        egui::Area::new(egui::Id::unique("adl_toast"))
            .order(egui::Order::Tooltip)
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-18.0, -18.0))
            // `Area` 自带 fade_in，会和这里的 alpha 叠乘；toast 只活 4 秒，
            // 再叠一层入场淡入会显得发虚，所以关掉。
            .fade_in(false)
            .show(ctx, |ui| {
                ui.set_opacity(alpha);
                let mut frame = egui::Frame::popup(&ctx.style_of(egui::Theme::Light));
                frame.fill = fade_color(frame.fill, alpha);
                frame.stroke.color = fade_color(frame.stroke.color, alpha);
                frame.shadow.color = fade_color(frame.shadow.color, alpha);
                let resp = frame
                    .inner_margin(egui::Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        ui.set_width(300.0);
                        ui.horizontal(|ui| {
                            let dot_at =
                                egui::pos2(ui.cursor().left() + 4.0, ui.cursor().top() + 8.0);
                            ui.painter().circle_filled(dot_at, 3.6, dot);
                            ui.add_space(14.0);
                            ui.label(
                                egui::RichText::new(&n.title)
                                    .size(13.0)
                                    .strong()
                                    .color(ink()),
                            );
                        });
                        if !n.body.is_empty() {
                            ui.add_space(3.0);
                            ui.horizontal(|ui| {
                                ui.add_space(14.0);
                                ui.label(
                                    egui::RichText::new(&n.body).size(12.0).color(muted()),
                                );
                            });
                        }
                        ui.add_space(3.0);
                        ui.horizontal(|ui| {
                            ui.add_space(14.0);
                            adl_ui::widgets::badge(ui, n.level.label(), tone);
                            ui.label(
                                egui::RichText::new("点击查看全部")
                                    .small()
                                    .color(muted()),
                            );
                        });
                    })
                    .response;
                if resp.interact(egui::Sense::click()).clicked() {
                    clicked = true;
                }
            });

        if clicked {
            self.tab = Tab::Notices;
            self.toast = None;
            self.notices.mark_all_read();
            self.notices.save();
        }
    }

    /// 终端页：一个能留历史的命令行（取代原来的 Ctrl+K 浮层命令面板）。
    ///
    /// 为什么做成页面而不是浮层：浮层关掉就没了，想翻上一条命令的输出只能
    /// 重跑；而且用户明确说想要"终端那种（win 的 powershell）"。
    fn page_terminal(&mut self, ui: &mut egui::Ui) {
        // 终端配色不走主题：这是"深色控制台"这一个孤岛，主题是浅色的。
        //
        // 用户说原来的样子"有些简陋、看着有分界感"，所以：
        // ① 去掉 1px 描边 —— 有描边就像另贴上去的一块黑板；
        // ② 圆角与卡片一致（10），内边距也调成卡片的量级；
        // ③ 提示符抄 PowerShell 的样子：`PS <目录>` + 尖括号，目录用强调色，
        //    一眼能看出"现在在哪个目录下操作"。
        let bg = egui::Color32::from_rgb(0x17, 0x1D, 0x1C);
        let ink_out = egui::Color32::from_rgb(0xDC, 0xE3, 0xE1);
        let ink_info = egui::Color32::from_rgb(0x77, 0x86, 0x83);
        let ink_echo = egui::Color32::from_rgb(0x8F, 0xD3, 0xC1);
        let ink_path = egui::Color32::from_rgb(0x6F, 0xA8, 0xE8);
        let ink_warn = egui::Color32::from_rgb(0xE8, 0xC4, 0x6F);
        let ink_err = egui::Color32::from_rgb(0xE3, 0x86, 0x86);
        // 横幅比正文再亮一档，但不是彩色 —— 真 PowerShell 的 banner
        // 和正文同色系，只是更干净（见 m12534 那张截图）。
        let ink_banner = egui::Color32::from_rgb(0xE8, 0xEE, 0xEC);
        let color_of = |k: LineKind| match k {
            LineKind::Banner => ink_banner,
            LineKind::Echo => ink_echo,
            LineKind::Out => ink_out,
            LineKind::Info => ink_info,
            LineKind::Warn => ink_warn,
            LineKind::Err => ink_err,
        };

        // 提示符里的目录：太长就从左边截，保留尾部（尾部的层次才是
        // 人真正关心的部分，跟 PowerShell 折行时的效果接近）。
        //
        // 默认配置里 `dir = "."`，直接显示就是个光秃秃的 `.`，看着像坏了；
        // PowerShell 的提示符永远是绝对路径，所以这里也换算成绝对路径再截。
        let cwd = {
            let raw = self.gui.cfg.lock().unwrap().dir.clone();
            let abs = std::path::Path::new(&raw)
                .canonicalize()
                .map(|p| p.display().to_string().replace('\\', "/"))
                .unwrap_or_else(|_| raw.clone());
            let n = abs.chars().count();
            if n > 52 {
                let tail: String = abs.chars().skip(n - 49).collect();
                format!("…{tail}")
            } else {
                abs
            }
        };

        // 大框占满可用高度。输入行**不**钉在底部，而是跟在历史输出的最后
        // 一行后面，和历史共用一个滚动区 —— 这才是真终端的样子（在 `PS>`
        // 后面就地打字），而不是底下永远浮着一条输入栏。
        //
        // 高度要先把 Frame 的内边距（symmetric(14,12) → 上下各 12）扣掉，
        // 否则内容比框高 24px，`PS>` 那一行会被裁在框外看不见。
        let full_h = ui.available_height().max(200.0);
        let inner_h = (full_h - 24.0).max(160.0);

        let mut submit = false;

        egui::Frame::new()
            .fill(bg)
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(egui::Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.set_height(inner_h);

                // ── 历史输出 + 当前输入行（同一个滚动区）──
                //
                // 把输入行放进滚动区里，而不是放在框外：真终端就是"在最后
                // 一行 PS> 后面接着打"，滚历史时输入行跟着一起走。靠
                // `stick_to_bottom(true)` 保证它永远停在看得见的最底下。
                egui::ScrollArea::vertical()
                    .id_salt("adl_term_log")
                    .max_height(inner_h)
                    .auto_shrink([false, false])
                    // 新输出总是把视图顶到底部，省得每次执行完还要手动滚
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        for (kind, text) in &self.term.lines {
                            match kind {
                                LineKind::Echo => {
                                    // 回显要把整条提示符留住，不能只剩命令名 ——
                                    // 真 PowerShell 里敲过的行是原样留在屏幕上的，
                                    // 只有 "PS 命令" 没有目录和 `>` 会看着像丢了东西。
                                    // 用 `\t` 把"当时的目录"和"命令"分开存：
                                    // 目录用路径色、命令用正文色，两者还对得上。
                                    let (where_at, cmd) = text
                                        .split_once('\t')
                                        .unwrap_or(("", text.as_str()));
                                    ui.horizontal_wrapped(|ui| {
                                        // 和下面正在输入的那一行用完全相同的
                                        // 间距：`PS ` + `目录>` + 6px + 内容。
                                        // 两处只要差一个空格，输入的字就不会
                                        // 和上面那些回显行左边对起来。
                                        ui.spacing_mut().item_spacing.x = 0.0;
                                        ui.label(
                                            egui::RichText::new("PS ")
                                                .monospace()
                                                .size(12.5)
                                                .color(ink_echo),
                                        );
                                        if !where_at.is_empty() {
                                            ui.label(
                                                egui::RichText::new(where_at)
                                                    .monospace()
                                                    .size(12.5)
                                                    .color(ink_path),
                                            );
                                        }
                                        ui.add_space(6.0);
                                        ui.label(
                                            egui::RichText::new(cmd)
                                                .monospace()
                                                .size(12.5)
                                                .color(ink_out),
                                        );
                                    });
                                }
                                k => {
                                    let c = color_of(*k);
                                    ui.label(
                                        egui::RichText::new(text).monospace().size(12.5).color(c),
                                    );
                                }
                            }
                        }

                        // ── 当前输入行（行内）──
                        // Enter/Tab 必须在建 TextEdit **之前**读：单行 TextEdit 收到回车
                        // 会 `consume_key(Enter)` 并 surrender_focus，等它返回之后
                        // 再 `key_pressed(Enter)` 永远是 false（实测第一次回车丢键）。
                        let (enter, tab, up, down, esc) = ui.input(|i| {
                            (
                                i.key_pressed(egui::Key::Enter),
                                i.key_pressed(egui::Key::Tab),
                                i.key_pressed(egui::Key::ArrowUp),
                                i.key_pressed(egui::Key::ArrowDown),
                                i.key_pressed(egui::Key::Escape),
                            )
                        });
                        // 补全/翻历史是程序化替换整行内容，替换完要把光标
                        // 拎到行尾（否则 `h` → `help` 之后光标还卡在索引 1）。
                        // 这里只记一个标记，等 TextEdit 画完、状态落盘之后再改。
                        let mut caret_to_end = false;
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            ui.label(
                                egui::RichText::new("PS ")
                                    .monospace()
                                    .size(12.5)
                                    .color(ink_echo),
                            );
                            ui.label(
                                egui::RichText::new(format!("{cwd}>"))
                                    .monospace()
                                    .size(12.5)
                                    .color(ink_path),
                            );
                            ui.add_space(6.0);
                            // 边打边着色：把当前这行词法过一遍，命令名/子命令
                            // 用提示符那支绿，参数保持正文色。这是 PSReadLine
                            // 的做法 —— 它也是在自己接管输入之后边打边给 token
                            // 上色，而不是等回车之后再染。
                            let mut layouter =
                                |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
                                    term_layouter(ui, buf.as_str(), wrap, ink_echo, ink_out, ink_info)
                                };
                            let te = ui.add(
                                egui::TextEdit::singleline(&mut self.term.input)
                                    // 固定 id：补全/翻历史要在控件之外改光标，
                                    // 必须能拿到同一个 id（见 `term_caret_to_end`）。
                                    .id(egui::Id::unique(TERM_INPUT_ID))
                                    // 必须显式给字号，不能用 `TextStyle::Monospace`：
                                    // 主题里只覆盖了 Body/Button/Heading/Small，
                                    // Monospace 还是 egui 默认的 14.0，而提示符标签
                                    // 是 12.5。两边 galley 高度不同，TextEdit 在
                                    // `ui.horizontal` 的 Align::Center 里居中后，
                                    // 它的字整体比提示符高出一截（实测差 25 逻辑像素）。
                                    .font(egui::FontId::monospace(12.5))
                                    .desired_width(f32::INFINITY)
                                    // TextEdit 的默认对齐是 LEFT_TOP，而 `interact_size.y`
                                    // 把它的框撑到 30；文字因此贴在自己的框顶，而同排的
                                    // 提示符标签是在框里居中 —— 实测输入的字比 `PS 路径>`
                                    // 高 7.3 逻辑像素。显式给 LEFT_CENTER 让两边同基线。
                                    .align(egui::Align2::LEFT_CENTER)
                                    .text_color(ink_out)
                                    // 输入框和面板同底色、无描边：中间不再有一块深色
                                    // 方块，也没有聚焦时才冒出来的圆角框，光标直接落
                                    // 在面板上 —— 这才是终端，不是表单。
                                    .frame(
                                        egui::Frame::new()
                                            .fill(bg)
                                            .stroke(egui::Stroke::NONE)
                                            .corner_radius(egui::CornerRadius::ZERO),
                                    )
                                    .margin(egui::Margin::symmetric(0, 4))
                                    // 不放行 Tab：默认单行 TextEdit 会把 Tab 当成"切焦点"，
                                    // 一按就跳到别的控件上去。这里自己接管做补全。
                                    .lock_focus(true)
                                    .layouter(&mut layouter),
                            );
                            // 空输入时的灰字提示自己画，不用 `.hint_text(..)`。
                            //
                            // 原因：hint 会作为一个 atom 参与这一行的布局，它的
                            // galley 高度由**汉字回退字体**的行高决定（比 12.5
                            // 号 monospace 高 4~5px），而光标竖线的高度来自
                            // TextEdit 自己的 font —— 两者各自在行里居中，于是
                            // 空输入时光标看着比灰字高一截（用户 m13765）。
                            // 自己用 `Align2::LEFT_CENTER` 画就只有一个居中基准。
                            // 往右让 12px 是为了不压住 index 0 处的光标。
                            if self.term.input.is_empty() {
                                ui.painter().text(
                                    egui::pos2(te.rect.left() + 12.0, te.rect.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    "输入 help 看命令",
                                    egui::FontId::monospace(12.5),
                                    ink_info,
                                );
                            }
                            if self.term.focus {
                                te.request_focus();
                                self.term.focus = false;
                            }
                            // 只有输入框有焦点时才拦方向键/Esc，否则在别的页面按 ↑
                            // 也会翻终端历史（虽然看不见，但会莫名其妙改掉输入）。
                            // `lost_focus()` 也要算：回车那一帧焦点已经交还了。
                            let active = te.has_focus() || te.lost_focus();
                            if active {
                                // 手动改过内容（打字、粘贴）就作废上一轮补全，
                                // 否则再按 Tab 会拿旧候选把输入顶掉。
                                if te.changed() {
                                    self.term.comp = None;
                                }
                                if esc {
                                    self.term.input.clear();
                                    self.term.hist_pos = None;
                                    self.term.comp = None;
                                    caret_to_end = true;
                                }
                                if up && !self.term.history.is_empty() {
                                    // 第一次按 ↑ 从最新一条往上翻；翻到头就停在最老那条。
                                    let next = match self.term.hist_pos {
                                        None => self.term.history.len() - 1,
                                        Some(0) => 0,
                                        Some(i) => i - 1,
                                    };
                                    self.term.hist_pos = Some(next);
                                    self.term.input = self.term.history[next].clone();
                                    self.term.comp = None;
                                    caret_to_end = true;
                                }
                                if down {
                                    match self.term.hist_pos {
                                        // 翻回底部 = 回到正在编辑的空行
                                        Some(i) if i + 1 >= self.term.history.len() => {
                                            self.term.hist_pos = None;
                                            self.term.input.clear();
                                        }
                                        Some(i) => {
                                            self.term.hist_pos = Some(i + 1);
                                            self.term.input = self.term.history[i + 1].clone();
                                        }
                                        None => {}
                                    }
                                    self.term.comp = None;
                                    caret_to_end = true;
                                }
                                if tab {
                                    self.term_complete();
                                    // 真 PowerShell 的 Tab 补完，光标跟在词尾，
                                    // 接着打参数就行。循环换候选也一样。
                                    caret_to_end = true;
                                }
                                if enter {
                                    submit = true;
                                }
                            }
                        });
                        // 光标复位必须在 TextEdit 已经跑完（它这一帧的状态是
                        // 最后写进 memory 的那份）之后再做，否则会被覆盖回去。
                        if caret_to_end {
                            term_caret_to_end(ui.ctx(), &self.term.input);
                        }
                    });
            });

        if submit {
            let line = self.term.input.trim().to_string();
            self.term.input.clear();
            self.term.hist_pos = None;
            if !line.is_empty() {
                self.term.history.push(line.clone());
                // 回显要把"当时在哪个目录下敲的"一起留下：真 PowerShell
                // 里敲过的行是整条 `PS C:\xxx> 命令` 原样留在屏幕上的。
                // 用 `\t` 分隔目录与命令，渲染时各用各的颜色。
                self.term
                    .push(LineKind::Echo, format!("{cwd}>\t{line}"));
                self.term_exec(&line);
            }
            // 执行完把光标留在输入框，连敲才顺手
            self.term.focus = true;
        }
    }

    /// Tab 补全：第一次算出候选并套用第一个，之后每次推进到下一个。
    ///
    /// 之所以要自己接管：单行 `TextEdit` 默认把 Tab 当"切焦点"用
    /// （`event_filter.tab == false`），一按就跳到别的控件上去了。
    fn term_complete(&mut self) {
        if let Some(c) = self.term.comp.as_mut() {
            if !c.cands.is_empty() {
                c.idx = (c.idx + 1) % c.cands.len();
                self.term.input = c.cands[c.idx].clone();
                self.term.focus = true;
                return;
            }
        }
        let cands = term_candidates(&self.term.input);
        if cands.is_empty() {
            // 没候选就什么都不做（真终端这时会响一声，我们没有声音）
            self.term.comp = None;
            return;
        }
        self.term.input = cands[0].clone();
        self.term.comp = Some(Completion { cands, idx: 0 });
        self.term.focus = true;
    }

    /// 执行终端里的一行命令。
    ///
    /// 所有输出都写进 `self.term`（不弹 toast、不写状态条）—— 终端页自己
    /// 就是输出面板，再飘一条 6 秒消失的提示反而找不到。
    fn term_exec(&mut self, line: &str) {
        let mut it = line.split_whitespace();
        let cmd = it.next().unwrap_or("").to_ascii_lowercase();
        let args: Vec<String> = it.map(|s| s.to_string()).collect();

        match cmd.as_str() {
            "help" | "?" | "" => {
                // 形状照 PowerShell 的 Get-Help 输出：几个小节标题（主题 /
                // 简短说明 / 详细说明），正文两空格缩进、同色系、没有色块。
                // 命令表从 `TERM_COMMANDS` 生成，Tab 补全与 help 共用一份，
                // 免得两处各写一遍、改一处忘一处。
                self.term.push(LineKind::Banner, "主题");
                self.term.push(LineKind::Out, "  ADL 命令行");
                self.term.push(LineKind::Out, "");
                self.term.push(LineKind::Banner, "简短说明");
                self.term
                    .push(LineKind::Out, "  在这里管理下载任务、改设置、跑插件命令。");
                self.term.push(LineKind::Out, "");
                self.term.push(LineKind::Banner, "详细说明");
                for c in TERM_COMMANDS {
                    self.term.push(LineKind::Out, format!("  {c}"));
                }
                self.term.push(LineKind::Out, "");
                self.term.push(LineKind::Banner, "参数");
                for line in [
                    "  add <url> [文件名]              新建下载",
                    "  pause <id> | pause all          暂停",
                    "  resume <id>                     继续（按 URL 从控制文件续传）",
                    "  rm <id>                         从列表移除（不删文件）",
                    "  open <id>                       在资源管理器里定位",
                    "  cd <目录>                       改默认下载目录（不带参数=显示当前）",
                    "  set dir <路径>                   改默认下载目录",
                    "  set connections <1-64>          改并发上限",
                    "  set limit <字节/秒，0=不限>       改限速",
                    "  mode safe|trusted               切换插件模式",
                    "  plugin list                     列出已加载插件",
                    "  plugin run <插件> <命令>         跑插件声明的命令",
                ] {
                    self.term.push(LineKind::Out, line);
                }
                self.term.push(LineKind::Out, "");
                self.term.push(LineKind::Banner, "联机帮助");
                self.term.push(
                    LineKind::Out,
                    "  Tab 补全、↑/↓ 翻历史、Esc 清空当前行。",
                );
            }
            "clear" | "cls" => {
                // 把开头的两行横幅留下，只清后面的输出 —— 清成全黑会让人以为卡死
                self.term.lines.truncate(2);
            }
            "tasks" | "ls" => {
                let tasks = snapshot(&self.gui);
                if tasks.is_empty() {
                    self.term.push(LineKind::Info, "还没有任务");
                } else {
                    let mut rows = String::new();
                    for t in &tasks {
                        let (label, _) = status_style(&t.status);
                        let prog = if t.total > 0 {
                            format!("{:.1}%", t.completed as f64 * 100.0 / t.total as f64)
                        } else {
                            "-".to_string()
                        };
                        rows.push_str(&format!(
                            "  #{:<3} {:<6} {:>7}  {:<10}  {}\n",
                            t.id,
                            label,
                            prog,
                            format_speed(t.speed),
                            display_name(t)
                        ));
                    }
                    self.term.push(
                        LineKind::Out,
                        format!("  ID   状态      进度    速度        文件\n{rows}"),
                    );
                }
            }
            "add" => {
                if args.is_empty() {
                    self.term
                        .push(LineKind::Err, "用法：add <url> [文件名]");
                    return;
                }
                let mut payload = json!({ "url": args[0] });
                if let Some(name) = args.get(1) {
                    payload["out"] = json!(name);
                }
                match add_task(&self.gui, &payload) {
                    Ok(v) => {
                        let id = v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
                        self.term
                            .push(LineKind::Out, format!("已添加任务 #{id}：{}", args[0]));
                    }
                    Err(e) => self.term.push(LineKind::Err, format!("添加失败：{e}")),
                }
            }
            "pause" => {
                if args.first().map(|s| s.as_str()) == Some("all") {
                    pause_all(&self.gui);
                    self.term.push(LineKind::Out, "已暂停全部任务");
                } else if let Some(id) = args.first().and_then(|s| s.parse::<u64>().ok()) {
                    match pause_task(&self.gui, &json!({ "id": id })) {
                        Ok(_) => self.term.push(LineKind::Out, format!("已暂停 #{id}")),
                        Err(e) => self.term.push(LineKind::Err, e),
                    }
                } else {
                    self.term.push(LineKind::Err, "用法：pause <id> | pause all");
                }
            }
            "resume" => {
                let Some(id) = args.first().and_then(|s| s.parse::<u64>().ok()) else {
                    self.term.push(LineKind::Err, "用法：resume <id>");
                    return;
                };
                let url = snapshot(&self.gui)
                    .into_iter()
                    .find(|t| t.id == id)
                    .map(|t| t.url);
                match url {
                    None => self.term.push(LineKind::Err, format!("#{id} 不存在")),
                    Some(url) => {
                        // 续传走的是"重新 add 同一条 URL"：引擎会找到同名
                        // `.adl` 控制文件，从断点接着下，而不是从头开始。
                        match add_task(&self.gui, &json!({ "url": url })) {
                            Ok(v) => {
                                let nid =
                                    v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
                                self.term.push(
                                    LineKind::Out,
                                    format!("已继续 #{id}（新任务 #{nid}，从断点续传）"),
                                );
                            }
                            Err(e) => self.term.push(LineKind::Err, format!("继续失败：{e}")),
                        }
                    }
                }
            }
            "rm" => {
                let Some(id) = args.first().and_then(|s| s.parse::<u64>().ok()) else {
                    self.term.push(LineKind::Err, "用法：rm <id>");
                    return;
                };
                match remove_task(&self.gui, &json!({ "id": id, "files": false })) {
                    Ok(_) => self.term.push(LineKind::Out, format!("已移除 #{id}（文件保留）")),
                    Err(e) => self.term.push(LineKind::Err, e),
                }
            }
            "open" => {
                let Some(id) = args.first().and_then(|s| s.parse::<u64>().ok()) else {
                    self.term.push(LineKind::Err, "用法：open <id>");
                    return;
                };
                let path = snapshot(&self.gui)
                    .into_iter()
                    .find(|t| t.id == id)
                    .and_then(|t| t.path);
                match path {
                    Some(p) if std::path::Path::new(&p).exists() => {
                        match platform::open_path(std::path::Path::new(&p)) {
                            Ok(_) => self.term.push(LineKind::Out, format!("已打开 {p}")),
                            Err(e) => self.term.push(LineKind::Err, format!("打开失败：{e}")),
                        }
                    }
                    Some(p) => self
                        .term
                        .push(LineKind::Err, format!("文件还不存在：{p}")),
                    None => self
                        .term
                        .push(LineKind::Err, format!("#{id} 还没落盘")),
                }
            }
            "cd" => {
                // 只改"默认下载目录"，不改进程工作目录 —— 下载器没有 shell
                // 意义上的 cwd，这个命令的语义就是"以后文件放这儿"。
                let Some(p) = args.first() else {
                    let dir = self.gui.cfg.lock().unwrap().dir.clone();
                    self.term.push(LineKind::Out, dir);
                    return;
                };
                let dir = p.replace('/', "\\");
                let pb = std::path::Path::new(&dir);
                if !pb.is_dir() {
                    self.term
                        .push(LineKind::Err, format!("目录不存在：{dir}"));
                    return;
                }
                let mut cfg = self.gui.cfg.lock().unwrap();
                cfg.dir = dir.clone();
                let _ = cfg.save();
                drop(cfg);
                self.s_dir = dir.clone();
                self.term.push(LineKind::Out, format!("下载目录 → {dir}"));
            }
            "history" => {
                if self.term.history.is_empty() {
                    self.term.push(LineKind::Info, "还没有历史命令");
                } else {
                    let mut body = String::new();
                    for (i, h) in self.term.history.iter().enumerate() {
                        body.push_str(&format!("  {:>3}  {h}\n", i + 1));
                    }
                    self.term.push(LineKind::Out, body.trim_end().to_string());
                }
            }
            "ver" => {
                self.term.push(
                    LineKind::Out,
                    format!(
                        "ADL {} · {} · 插件 {}/{}",
                        env!("CARGO_PKG_VERSION"),
                        std::env::consts::OS,
                        {
                            let cfg = self.gui.cfg.lock().unwrap().clone();
                            if cfg.mode_trusted() { "trusted" } else { "safe" }
                        },
                        self.plugin_snapshot
                            .get("all")
                            .and_then(|x| x.as_array())
                            .map(|a| a.len())
                            .unwrap_or(0)
                    ),
                );
            }
            "settings" => {
                let cfg = self.gui.cfg.lock().unwrap().clone();
                self.term.push(
                    LineKind::Out,
                    format!(
                        "  下载目录  {}\n  并发上限  {}\n  限速      {}\n  代理      {}\n  插件模式  {}\n  插件目录  {}",
                        cfg.dir,
                        cfg.connections,
                        if cfg.limit == 0 {
                            "不限速".to_string()
                        } else {
                            format_speed(cfg.limit)
                        },
                        if cfg.proxy.is_empty() {
                            "直连".to_string()
                        } else {
                            cfg.proxy.clone()
                        },
                        cfg.mode,
                        if cfg.plugin_dir.is_empty() {
                            "exe 同级 plugins".to_string()
                        } else {
                            cfg.plugin_dir.clone()
                        },
                    ),
                );
            }
            "set" => {
                let key = args.first().map(|s| s.to_ascii_lowercase()).unwrap_or_default();
                let value = args.get(1).cloned().unwrap_or_default();
                match key.as_str() {
                    "dir" => {
                        if value.is_empty() {
                            self.term.push(LineKind::Err, "用法：set dir <路径>");
                            return;
                        }
                        // `/` 统一换成 `\`：终端里手打路径常常是斜杠，
                        // 而 Windows 上混着用会让后续 `PathBuf` 拼名字很别扭。
                        let dir = value.replace('/', "\\");
                        let mut cfg = self.gui.cfg.lock().unwrap();
                        cfg.dir = dir.clone();
                        let _ = cfg.save();
                        drop(cfg);
                        self.s_dir = dir.clone();
                        self.term.push(LineKind::Out, format!("下载目录 → {dir}"));
                    }
                    "connections" | "conn" => {
                        match value.parse::<usize>() {
                            Ok(n) => {
                                let n = adl_core::clamp_concurrency(n);
                                let mut cfg = self.gui.cfg.lock().unwrap();
                                cfg.connections = n;
                                let _ = cfg.save();
                                drop(cfg);
                                self.s_conn = n as u64;
                                self.term
                                    .push(LineKind::Out, format!("并发上限 → {n}"));
                            }
                            Err(_) => self
                                .term
                                .push(LineKind::Err, "并发要填数字（1-64）"),
                        }
                    }
                    "limit" => match value.parse::<u64>() {
                        Ok(n) => {
                            let mut cfg = self.gui.cfg.lock().unwrap();
                            cfg.limit = n;
                            let _ = cfg.save();
                            drop(cfg);
                            self.s_limit = n;
                            self.term.push(
                                LineKind::Out,
                                if n == 0 {
                                    "限速 → 不限速".to_string()
                                } else {
                                    format!("限速 → {}", format_speed(n))
                                },
                            );
                        }
                        Err(_) => self
                            .term
                            .push(LineKind::Err, "限速要填字节/秒（0 = 不限）"),
                    },
                    _ => self
                        .term
                        .push(LineKind::Err, "用法：set dir|connections|limit <值>"),
                }
            }
            "mode" => {
                let m = args.first().map(|s| s.to_ascii_lowercase()).unwrap_or_default();
                if m != "safe" && m != "trusted" {
                    self.term.push(LineKind::Err, "用法：mode safe | mode trusted");
                    return;
                }
                let mut cfg = self.gui.cfg.lock().unwrap();
                cfg.mode = m.clone();
                let _ = cfg.save();
                let new_cfg = cfg.clone();
                drop(cfg);
                *self.gui.hooks.lock().unwrap() = build_hooks(&new_cfg);
                self.s_mode = m.clone();
                self.plugin_snapshot_at = Instant::now() - Duration::from_secs(3600);
                self.term.push(
                    LineKind::Out,
                    format!(
                        "插件模式 → {m}{}",
                        if m == "safe" {
                            "（只读观察 / 拦截，Transform 类 hook 不生效）"
                        } else {
                            "（允许改写请求、注册按钮、替换页面）"
                        }
                    ),
                );
            }
            "plugin" => {
                let sub = args.first().map(|s| s.to_ascii_lowercase()).unwrap_or_default();
                match sub.as_str() {
                    "list" | "ls" => {
                        let all = self
                            .plugin_snapshot
                            .get("all")
                            .and_then(|x| x.as_array())
                            .cloned()
                            .unwrap_or_default();
                        if all.is_empty() {
                            self.term.push(LineKind::Info, "没有插件");
                        } else {
                            let mut rows = String::new();
                            for p in &all {
                                let id = p.get("id").and_then(|x| x.as_str()).unwrap_or("?");
                                let ver = p.get("version").and_then(|x| x.as_str()).unwrap_or("");
                                let trusted = p
                                    .get("trusted")
                                    .and_then(|x| x.as_bool())
                                    .unwrap_or(false);
                                rows.push_str(&format!(
                                    "  {:<26} {:<8} {}\n",
                                    id,
                                    ver,
                                    if trusted { "信任" } else { "安全" }
                                ));
                            }
                            self.term
                                .push(LineKind::Out, format!("  插件                       版本     模式\n{rows}"));
                        }
                    }
                    "run" => {
                        let (Some(p), Some(c)) = (args.get(1), args.get(2)) else {
                            self.term
                                .push(LineKind::Err, "用法：plugin run <插件> <命令>");
                            return;
                        };
                        let hooks = self.gui.hooks.lock().unwrap().clone();
                        match hooks.run_command(p, c) {
                            Some(v) => {
                                let msg = v
                                    .get("message")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("命令已执行")
                                    .to_string();
                                self.term.push(LineKind::Out, msg);
                            }
                            None => self.term.push(
                                // 警告而不是错误：命令没跑，但程序没坏，
                                // 只是当前模式不允许 —— 一屏红字会像出事了。
                                LineKind::Warn,
                                "命令不可用（要用 trusted 模式，且插件确实声明了该命令）",
                            ),
                        }
                    }
                    _ => self
                        .term
                        .push(LineKind::Err, "用法：plugin list | plugin run <插件> <命令>"),
                }
            }
            other => self.term.push(
                LineKind::Err,
                format!("未知命令：{other}（输入 help 看可用命令）"),
            ),
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
    display_name_of_url(&t.url)
}

/// 从 URL 猜一个显示名：去掉 query/fragment，取最后一段路径。
fn display_name_of_url(url: &str) -> String {
    let no_query = url.split(['?', '#']).next().unwrap_or(url);
    let tail = no_query.trim_end_matches('/');
    let last = tail.rsplit('/').find(|s| !s.is_empty());
    match last {
        Some(s) => s.to_string(),
        None => url.to_string(),
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

/// 判断一条插件记录是不是"软件自带"的。
///
/// 内置插件的 id 统一是 `com.adl.xxx`（见 `plugins/` 下各 plugin.toml）。
/// 用户自己装的插件就算起名也带 com.adl 前缀，也是他们自己的责任；
/// 这里只按约定前缀分组，目的是在插件页把"程序自带功能"和"用户安装"
/// 分开显示 —— 前者不可在插件页调整。
fn is_builtin(v: &Value) -> bool {
    v.get("id")
        .and_then(|x| x.as_str())
        .map(|id| id.starts_with("com.adl."))
        .unwrap_or(false)
}

/// 从 `ui_snapshot["all"][i]["hooks"]` 这类 JSON 数组里取字符串列表。
fn str_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// 把 `permissions` 对象翻成 [(显示名, 是否声明)]，空的权限不显示。
fn permission_labels(v: Option<&Value>) -> Vec<(String, bool)> {
    let Some(obj) = v.and_then(|x| x.as_object()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (key, name) in [
        ("net", "网络"),
        ("exec", "执行"),
        ("notify", "通知"),
    ] {
        if obj.get(key).and_then(|x| x.as_bool()).unwrap_or(false) {
            out.push((name.to_string(), true));
        }
    }
    // 文件权限是数组，非空才算声明过。
    for (key, name) in [("fs_read", "读文件"), ("fs_write", "写文件")] {
        let n = obj
            .get(key)
            .and_then(|x| x.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        if n > 0 {
            out.push((format!("{name}×{n}"), true));
        }
    }
    out
}

fn plugins_root(cfg: &Config) -> Option<PathBuf> {
    cfg.plugin_dir_opt().or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("plugins")))
    })
}

/// 补上「已停用」的插件条目。
///
/// 快照里的 `all` 来自**已加载**的插件，而 `build_hooks` 在加载阶段就把停用的
/// 插件过滤掉了 —— 于是停用一次，这个插件就会从清单里彻底消失，再也点不到
/// 「启用」。这里直接扫目录读 `plugin.toml`，把清单里没有的补成停用条目。
fn disabled_plugin_entries(root: Option<&PathBuf>, known: &[String]) -> Vec<Value> {
    let Some(root) = root else {
        return Vec::new();
    };
    let Ok(rd) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let dir = e.path();
        if !dir.is_dir() {
            continue;
        }
        let manifest = dir.join(adl_plugin::MANIFEST_FILE);
        if !manifest.is_file() {
            continue;
        }
        // 坏 manifest 直接跳过：这个页面只是清单，不该因为一个坏插件整页报错。
        let Ok(m) = adl_plugin::Manifest::load(&manifest) else {
            continue;
        };
        if known.iter().any(|k| k == &m.id) {
            continue;
        }
        let mut hooks: Vec<String> = m
            .hook_subscriptions()
            .into_iter()
            .map(|(hp, _)| hp.canonical().to_string())
            .collect();
        hooks.sort();
        let has_ui = !(m.ui.pages.is_empty()
            && m.ui.actions.is_empty()
            && m.ui.settings.is_empty()
            && m.ui.commands.is_empty());
        out.push(json!({
            "id": m.id,
            "name": m.name,
            "version": m.version,
            "description": m.description,
            "api_version": m.api_version,
            "kind": m.kind,
            "dir": dir.display().to_string(),
            "trusted": false,
            "has_ui": has_ui,
            // 停用的插件也照常带上缺省值：列表里那一行的开关要能算对
            // "下次清掉配置后该是开还是关"。
            "enabled_by_default": m.enabled_by_default,
            "hooks": hooks,
            // 停用的插件没被加载，谈不上"生效的 hook"，留空即可。
            "active_hooks": Vec::<String>::new(),
            "pages": m.ui.pages.len(),
            "actions": m.ui.actions.len(),
            "settings": m.ui.settings.len(),
            "commands": m.ui.commands.len(),
            "permissions": json!({
                "net": m.permissions.net,
                "exec": m.permissions.exec,
                "notify": m.permissions.notify,
                "fs_read": m.permissions.fs_read,
                "fs_write": m.permissions.fs_write,
            }),
        }));
    }
    out
}

fn build_hooks(cfg: &Config) -> Arc<PluginHooks> {
    let mode = if cfg.mode_trusted() { Mode::Trusted } else { Mode::Safe };
    match plugins_root(cfg) {
        Some(dir) if dir.is_dir() => {
            let all = adl_plugin::load_dir(&dir, mode);
            let kept: Vec<_> = all
                .into_iter()
                // 缺省值取清单里的 enabled_by_default：示例插件写 false，
                // 免得和一个文件的两条"下载完成"叠在一起（用户 m12538）。
                .filter(|p| {
                    let d = p.manifest.enabled_by_default;
                    cfg.plugin_enabled_or(&p.id, d)
                })
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
    // 空字符串表示"用内置默认 UA"，不是"没有 UA" —— 引擎侧
    // `user_agent_opt()` 会把空串当 None，再退回默认 Chrome 串。
    if let Some(ua) = args.get("user_agent").and_then(|x| x.as_str()) {
        cfg.user_agent = ua.trim().to_string();
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
        pending_notices: Mutex::new(Vec::new()),
    });

    // 插件线程调 `adl.notify()` 时往 pending_notices 里塞，界面每帧取走。
    // 插件跑在各自的 OS 线程上，拿不到 `App`，只能走这个通道。
    {
        let g = gui.clone();
        adl_plugin::hostapi::set_notify_sink(Some(std::sync::Arc::new(
            move |plugin: &str, title: &str, body: &str| {
                if let Ok(mut q) = g.pending_notices.lock() {
                    q.push((
                        notice::Level::Info,
                        if title.is_empty() {
                            format!("插件 {plugin}")
                        } else {
                            title.to_string()
                        },
                        body.to_string(),
                    ));
                }
            },
        )));
    }

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
