//! ADL 图形界面：纯 Rust 原生 UI（egui / eframe），**不依赖 WebView2**。
//! 跨平台：Windows / Linux / macOS 均可编译运行。
//!
//! 本文件只负责“壳”（窗口与交互）；下载引擎在 `adl-core`，插件在 `adl-plugin`，
//! 配置与历史在 `adl-app`。浏览器接管用的本地 HTTP API 仍由 `api_server` 提供。

// 🔴 不加这一行，Windows 会给这个 exe 分配一个控制台窗口 —— 用户双击
// 启动时看到的那些「闪一下的 PowerShell 黑框」就是它（严格说是 conhost
// 承载的 console 窗口，宿主进程就是我们自己）。这是 exe 自己的 PE 头属性，
// 跟打包工具无关：只要在 release 里加了，打包后也一样没有窗口。
//
// 用 `cfg_attr(not(debug_assertions))`：debug 下保留控制台，`eprintln!`
// 打的插件加载日志、panic 回溯都能直接在终端里看到；release 下才藏起来。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api_server;
mod install;
mod logfile;
mod market;
mod notice;
mod plugins;
mod queue_fn;
mod terminal;
mod update;

use plugins::{plugins_root, PluginsTab};
use queue_fn::{add_batch, add_task, pause_all, pause_task, remove_task, restore_tasks, resume_all,
               resume_task, save_tasks};
use terminal::TerminalState;
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
use adl_plugin::{Mode, PluginHooks};
use adl_ui::ChipIcon;

// ───────────────────────── 共享状态 ─────────────────────────

pub(crate) struct Task {
    pub(crate) id: u64,
    pub(crate) url: String,
    /// "downloading" | "queued" | "paused" | "completed" | "failed"
    pub(crate) status: String,
    pub(crate) completed: u64,
    pub(crate) total: u64,
    pub(crate) speed: u64,
    /// 当前活跃连接数（引擎每帧通过 `Progress::connections` 报上来）。
    /// 顶栏的"并行线路"chip 就是把所有进行中任务的这个值加起来。
    pub(crate) connections: u32,
    /// 每条活跃线路的区间与已写字节（任务详情页的热力矩阵就画这个）。
    /// 由引擎 `Progress::runs` 每 64 KB 上报一次；空 `Vec` = 没有在跑的线路。
    pub(crate) runs: Vec<adl_core::hooks::RunInfo>,
    /// 下载目录。控制文件在任务跑完之前就存在（`<目标>.adl`），但目标
    /// 文件名要探测完才知道，所以在下载过程中 `path` 是空的。详情页要
    /// 画分片图就得靠这个目录 + URL 去反查控制文件（用户 m15179）。
    pub(crate) dir: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) error: Option<String>,
    /// 单任务下载参数。以前只活在 `add_task` 的局部变量里，重启/排队后
    /// 就再也没人知道"这条当初是用哪个 UA 要的"（用户 m18583 要求持久化）。
    pub(crate) out_name: Option<String>,
    pub(crate) user_agent: Option<String>,
    pub(crate) referer: Option<String>,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) cookies: Option<String>,
    /// 入队时刻（unix 秒），排序用。
    pub(crate) added_unix: u64,
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
    runs: Vec<adl_core::hooks::RunInfo>,
    dir: Option<String>,
    path: Option<String>,
    error: Option<String>,
    added_unix: u64,
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
            runs: t.runs.clone(),
            dir: t.dir.clone(),
            path: t.path.clone(),
            error: t.error.clone(),
            added_unix: t.added_unix,
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
    /// 单个任务的详情页（从任务行的「⋯ → 查看详情…」进）。
    ///
    /// 带任务 id：详情页只读 `snapshot()` 里同 id 的那条，任务被删掉时
    /// 页面显示"任务已删除"而不是崩。
    TaskDetail(u64),
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
    /// 单次下载的 User-Agent 覆盖；空 = 用设置里的默认值
    f_ua: String,
    /// 单次下载的保存目录覆盖；空 = 用设置里的默认下载目录。
    /// 后端 `add_task` 早就认 `args["dir"]`，这里只是把入口补上
    /// （用户 m15179："每个文件单独的下载目录调整（新建下载页面）"）。
    f_dir: String,
    // 设置表单
    s_dir: String,
    s_conn: u64,
    s_limit: u64,
    s_proxy: String,
    s_ua: String,
    s_mode: String,
    // 队列上限（同时下载几条任务）—— 草稿值，点「保存设置」才落盘
    s_max_tasks: u64,
    // 启动时检查更新 —— 草稿值
    s_check_update: bool,
    // 开机自启动 —— 草稿值。真相在注册表里（用户可能用任务管理器的
    // 「启动」页关掉它，那种工具只删注册表、不改我们的配置），
    // 所以这个值每次都从 `platform::auto_start_enabled()` 读回来。
    s_auto_start: bool,
    // 关闭窗口时的行为 —— 草稿值（"tray" / "close"）
    s_close_action: String,
    // 限速显示单位（"auto" 或固定单位）
    limit_unit: String,
    // 侧栏收起
    sidebar_collapsed: bool,
    // 主页热力图范围
    home_range: Range,
    // 关闭行为询问弹窗（None = 不显示）
    close_ask: Option<CloseAsk>,
    // 删除任务确认弹窗（None = 不显示）
    del_ask: Option<DeleteAsk>,
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
    term: TerminalState,
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
    /// 上面那个接收端当前服务的是谁：`true` = 新建下载弹窗的目录，
    /// `false` = 设置页的默认下载目录。两处共用一个 `pick_rx`。
    pick_to_add: bool,
    /// 正在进行的"从压缩包安装插件"。
    ///
    /// 选文件 + 解包都是阻塞动作（系统对话框要等人点，解包要读磁盘），
    /// 所以整条链路丢进一个线程，只把最终结果（成功说装了什么、失败说
    /// 为什么）从这个 channel 送回来。
    zip_rx: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    /// 任务列表的搜索词（匹配文件名与 URL）。
    task_search: String,
    /// 任务列表状态筛选：all / downloading / queued / paused / completed / failed。
    /// 初值来自 `cfg.task_filter`，改动即落盘。
    task_filter: String,
    /// 任务列表排序：status（默认，进行中在上）/ added / name / size / progress。
    task_sort: String,
    /// 插件市场的索引结果（后台拉，UI 这里只负责取）。
    ///
    /// `None` = 还没点过「刷新」。`Some(Err(_))` 也要留住，否则"拉失败了"
    /// 每一帧都会重拉一次，变成死循环打 GitHub。
    market_rx: Option<std::sync::mpsc::Receiver<Result<(Vec<market::Entry>, Vec<String>), String>>>,
    /// 已经拿到的市场清单（空 Vec = 拉到了但一条能用的都没有）。
    market_entries: Vec<market::Entry>,
    /// 上次拉取失败/跳过条目的说明，直接显示在界面上。
    market_note: Option<(String, bool)>,
    /// 正在装的市场条目 id（同一时间只装一个，装的时候按钮禁用）。
    market_installing: Option<String>,
    /// 市场安装的结果通道。
    market_install_rx: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    /// 市场索引地址（用户可以在设置里换镜像）。
    market_url: String,

    // ── 软件自动更新（走 GitHub Releases）──
    // 只在用户点「检查更新」时发一次请求，不常驻轮询：下载器没必要隔一小时
    // 打一次 GitHub。`update_rx` 拿到结果后必须置 None（同 market_rx）。
    update_rx: Option<std::sync::mpsc::Receiver<Result<update::ReleaseInfo, String>>>,
    /// 已经查到的最新版本信息（`None` = 还没查过）。
    update_info: Option<update::ReleaseInfo>,
    /// 检查更新的结果说明（错误或"已是最新"）。
    update_note: Option<(String, bool)>,
    /// 正在下载的安装包通道。
    update_dl_rx: Option<std::sync::mpsc::Receiver<Result<std::path::PathBuf, String>>>,
    /// 下载进度用的提示文字（`None` = 没在下载）。
    update_downloading: bool,
    /// 已下好的新 exe，等用户点「重启并安装」。
    update_staged: Option<std::path::PathBuf>,

    /// 托盘图标的注册结果，一句话（"已注册（任务栏可见）" / "没注册上：…"）。
    ///
    /// 用户 m22507 报"没有托盘"，而 release 版是 `windows_subsystem="windows"`，
    /// stderr 全丢、界面上又什么都不说 —— 只能让用户去翻 startup.log。
    /// 现在把这句话原样显示在设置页「启动」卡里。
    tray_status: String,

    /// 本次是被开机自启拉起的（`--auto-start`）。第一帧要悄悄缩到托盘。
    ///
    /// 单独存一个字段而不是每帧读 `args()`：一来重复解析命令行没必要，
    /// 二来这个动作只该做一次（做完置 false），否则用户点托盘"显示主界面"
    /// 之后下一帧又会被自己藏回去。
    boot_hidden: bool,
}

/// 首次点关闭时问用户"退出还是最小化"。
struct CloseAsk {
    /// 勾选"记住我的选择"
    remember: bool,
}

/// 删除任务前的确认：问一句"本地文件要不要一起删"。
///
/// 用户 m15179：「删除文件，是否删除本地文件（还有记住选项）」——
/// `remember` 勾上就把这次的选择写进 `cfg.delete_files_choice`，
/// 之后删除不再弹这个框。
struct DeleteAsk {
    id: u64,
    /// 显示用（文件名或 URL 尾段）
    name: String,
    /// 这个任务有没有落盘路径；没有的话"删文件"这个勾选框没意义
    has_file: bool,
    /// 当前是否勾着"同时删除本地文件"
    delete_files: bool,
    /// 是否记住这次选择
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
        let picked = platform::pick_folder("选择下载目录");
        // 接收端可能已经丢了（用户切换页面把状态清掉），忽略发送失败。
        let _ = tx.send(picked);
    });
    rx
}

/// 「从压缩包安装插件」的整条链路：选文件 → 解包 → 删包。
///
/// 三件事都是阻塞的（系统对话框要等人点、解包要读磁盘），所以整段丢进
/// 一个线程，只把最终结论送回来 —— `Ok(插件 id)` 或 `Err(给人看的理由)`。
/// 成功之后调用方还要重建 `PluginHooks`、让快照失效，那两步需要 `self`，
/// 不能在这里做。
fn start_install_zip(root: std::path::PathBuf) -> std::sync::mpsc::Receiver<Result<String, String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let picked = platform::pick_file("选择插件压缩包", ("插件压缩包", "zip"));
        let Some(zip_path) = picked else {
            // 用户取消：发个空结果让界面把「安装中」的状态收掉。
            let _ = tx.send(Err(String::new()));
            return;
        };
        let res = install::install_zip(&zip_path, &root).map(|inst| inst.id);
        let _ = tx.send(res);
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

/// 托盘图标的固定身份。
///
/// Windows 用 GUID（而不是 exe 路径 + 进程序号）来记住这个图标在任务栏上的
/// 位置：用户把它从溢出区拖出来之后，下次启动还在原地。随便定一个常量即可，
/// 只要**永远不变**就行 —— 换了 GUID 等于换了一个新图标，位置又重置了。
const TRAY_GUID: u128 = 0x4144_4c31_0000_4000_8000_0000_0000_0001;

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
        tray_status: String,
    ) -> Self {
        install_cjk_font(&cc.egui_ctx);
        // 主题令牌与全局 Visuals 全部由 adl-ui 提供：这里不再手工赋值颜色，
        // 换肤只需改 `adl_ui::Palette`。
        adl_ui::Theme::light().install_and_apply(&cc.egui_ctx);

        let cfg = gui.cfg.lock().unwrap().clone();

        // 上次标记"卸载"的插件：这一刻才真正删目录。
        //
        // 为什么不点按钮就删：卸载时插件可能正在跑（onProgress 每 64 KB 调
        // 一次），当场 remove_dir_all 会把它脚下的文件抽走。用户 m15179 的
        // 要求就是"等整个软件重启后再删"。这里是启动路径，插件还没加载，
        // 删起来安全。
        run_pending_uninstall(&gui);

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

        // 启动时查一次更新（设置里可关）。这里先起，免得在 `Self { .. }`
        // 里再借 `gui` —— 那时 `gui` 已经被移进结构体了。
        let boot_update = if cfg.check_update {
            Some(update::start_check(&gui, String::new()))
        } else {
            None
        };

        Self {
            gui,
            tab: initial_tab,
            add_open,
            f_url: String::new(),
            f_out: String::new(),
            f_ua: String::new(),
            f_dir: String::new(),
            s_dir: cfg.dir.clone(),
            s_conn: cfg.connections as u64,
            s_limit: cfg.limit,
            s_proxy: cfg.proxy.clone(),
            s_ua: cfg.user_agent.clone(),
            s_mode: if cfg.mode_trusted() { "trusted".into() } else { "safe".into() },
            s_max_tasks: cfg.max_concurrent_tasks as u64,
            s_check_update: cfg.check_update,
            // 自启动以注册表为准，不信配置：用户可能在任务管理器的「启动」
            // 页把它关了，那种操作只删注册表项、不会改 config.toml。
            s_auto_start: platform::auto_start_enabled(),
            s_close_action: if cfg.close_action.trim().is_empty() {
                "tray".to_string()
            } else {
                cfg.close_action.clone()
            },
            limit_unit: if cfg.limit_unit.trim().is_empty() {
                "auto".to_string()
            } else {
                cfg.limit_unit.clone()
            },
            sidebar_collapsed: cfg.sidebar_collapsed,
            home_range: Range::Month,
            close_ask: None,
            del_ask: None,
            status: None,
            plugin_snapshot: json!({}),
            plugin_snapshot_at: Instant::now() - Duration::from_secs(3600),
            plugin_view: None,
            plugin_edit: BTreeMap::new(),
            wake,
            suspend,
            resume,
            term: TerminalState::default(),
            plugins_tab: PluginsTab::Installed,
            _tray: tray,
            tray_status,
            boot_hidden: platform::launched_by_auto_start(),
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
            pick_to_add: false,
            zip_rx: None,
            task_search: String::new(),
            task_filter: cfg.task_filter.clone(),
            task_sort: cfg.task_sort.clone(),
            market_rx: None,
            market_entries: Vec::new(),
            market_note: None,
            market_installing: None,
            market_install_rx: None,
            market_url: if cfg.market_url.trim().is_empty() {
                market::DEFAULT_INDEX_URL.to_string()
            } else {
                cfg.market_url.clone()
            },
            update_rx: boot_update,
            update_info: None,
            update_note: None,
            update_dl_rx: None,
            update_downloading: false,
            update_staged: None,
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
        // 多行粘贴 = 批量添加。文件名/UA/目录只对单条有意义，批量时忽略。
        if self.f_url.contains('\n') {
            let (ok, errs) = add_batch(&self.gui, &self.f_url);
            if ok == 0 && !errs.is_empty() {
                self.set_status(format!("一条都没加进去：{}", errs[0]), true);
                return;
            }
            let msg = if errs.is_empty() {
                format!("已添加 {ok} 条任务")
            } else {
                format!("已添加 {ok} 条，跳过 {} 条（{}）", errs.len(), errs[0])
            };
            self.set_status(msg, false);
            self.f_url.clear();
            self.f_out.clear();
            self.f_ua.clear();
            self.f_dir.clear();
            self.add_open = false;
            self.tab = Tab::Tasks;
            return;
        }
        let out = self.f_out.trim();
        let ua = self.f_ua.trim();
        let dir = self.f_dir.trim();
        let args = json!({
            "url": self.f_url.trim(),
            "out": if out.is_empty() { Value::Null } else { json!(out) },
            // 留空 = Value::Null，引擎会退回设置里的默认 UA
            "user_agent": if ua.is_empty() { Value::Null } else { json!(ua) },
            // 留空 = 用设置里的默认下载目录（add_task 里 `.filter(!is_empty)`）
            "dir": if dir.is_empty() { Value::Null } else { json!(dir) },
        });
        match add_task(&self.gui, &args) {
            Ok(v) => {
                let id = v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
                let queued = v.get("queued").and_then(|x| x.as_bool()).unwrap_or(false);
                if queued {
                    self.set_status(format!("已加入队列 #{id}（同时在下的任务已满）"), false);
                } else {
                    self.set_status(format!("已添加任务 #{id}"), false);
                }
                self.f_url.clear();
                self.f_out.clear();
                self.f_ua.clear();
                self.f_dir.clear();
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
            "limit_unit": self.limit_unit,
            // 空字符串 = 回到内置默认 UA（引擎侧 user_agent_opt() 返回 None）
            "user_agent": self.s_ua,
            // 下面几项以前是"一改就落盘"，现在和别的设置一样走草稿
            "max_concurrent_tasks": self.s_max_tasks,
            "check_update": self.s_check_update,
            "auto_start": self.s_auto_start,
            "close_action": self.s_close_action,
        });
        match set_settings(&self.gui, &args) {
            Ok(_) => {
                // 自启动真正的动作在注册表里，配置项只是个记录。写失败
                // （组策略禁用 Run 项、注册表被锁）必须当场说出来，
                // 否则用户以为开了、重启一看没自启。
                if let Err(e) = platform::set_auto_start(self.s_auto_start) {
                    self.set_status(format!("设置已保存，但自启动没改成: {e}"), true);
                } else {
                    self.set_status("已保存设置", false);
                }
                // 队列上限可能被调大了，立刻看一眼有没有空位可以补。
                queue_fn::pump_queue(&self.gui);
                self.plugin_snapshot_at = Instant::now() - Duration::from_secs(3600);
            }
            Err(e) => self.set_status(e, true),
        }
    }

    /// 把草稿字段全部重置回已保存的配置（「取消」按钮）。
    fn reset_settings_draft(&mut self) {
        let cfg = self.gui.cfg.lock().unwrap().clone();
        self.s_dir = cfg.dir.clone();
        self.s_conn = cfg.connections as u64;
        self.s_limit = cfg.limit;
        self.s_proxy = cfg.proxy.clone();
        self.s_ua = cfg.user_agent.clone();
        self.s_mode = if cfg.mode_trusted() {
            "trusted".into()
        } else {
            "safe".into()
        };
        self.s_max_tasks = cfg.max_concurrent_tasks as u64;
        self.s_check_update = cfg.check_update;
        // 「取消」把自启动也退回注册表的当前实际状态（不是配置里的值）：
        // 用户改了开关但没保存，取消就该回到"现在真的会怎样"。
        self.s_auto_start = platform::auto_start_enabled();
        self.s_close_action = if cfg.close_action.trim().is_empty() {
            "tray".to_string()
        } else {
            cfg.close_action.clone()
        };
        self.limit_unit = if cfg.limit_unit.trim().is_empty() {
            "auto".to_string()
        } else {
            cfg.limit_unit.clone()
        };
    }

    /// 设置页的草稿与已保存配置差了几项。
    ///
    /// 每帧现算，**不缓存脏标记**：用户把一个值改回去时计数必须跟着减，
    /// 脏标记做不到这件事（除非每次编辑都反查原值，那就等于现算了）。
    fn settings_dirty_count(&self) -> usize {
        let cfg = self.gui.cfg.lock().unwrap();
        let cfg_limit_unit = if cfg.limit_unit.trim().is_empty() {
            "auto".to_string()
        } else {
            cfg.limit_unit.clone()
        };
        let cfg_mode = if cfg.mode_trusted() {
            "trusted".to_string()
        } else {
            "safe".to_string()
        };
        let cfg_close = if cfg.close_action.trim().is_empty() {
            "tray".to_string()
        } else {
            cfg.close_action.clone()
        };
        let mut n = 0usize;
        if self.s_conn as usize != cfg.connections {
            n += 1;
        }
        if self.s_limit != cfg.limit {
            n += 1;
        }
        if self.limit_unit != cfg_limit_unit {
            n += 1;
        }
        if self.s_dir.trim() != cfg.dir.trim() {
            n += 1;
        }
        if self.s_proxy.trim() != cfg.proxy.trim() {
            n += 1;
        }
        if self.s_ua.trim() != cfg.user_agent.trim() {
            n += 1;
        }
        if self.s_mode != cfg_mode {
            n += 1;
        }
        if self.s_max_tasks as usize != cfg.max_concurrent_tasks {
            n += 1;
        }
        if self.s_check_update != cfg.check_update {
            n += 1;
        }
        // 自启动跟**注册表实际状态**比，不跟配置比：任务管理器里关掉自启动
        // 只删注册表项，配置里还留着 true，拿配置比会永远显示"已更改"。
        if self.s_auto_start != platform::auto_start_enabled() {
            n += 1;
        }
        if self.s_close_action != cfg_close {
            n += 1;
        }
        n
    }

    /// 设置页顶部那条固定工具条：保存 / 取消 / 已更改 N 项。
    ///
    /// 它必须画在页面级 `ScrollArea` **外面**（用户 m19629：把「保存设置」
    /// 从页面底部挪到「设置」标题下面），否则一滚动就跟着往上跑，设置页
    /// 又有十来张卡片，改完最后一项还得滚回底部才能保存。
    fn settings_bar(&mut self, ui: &mut egui::Ui) -> bool {
        let dirty = self.settings_dirty_count();
        let mut save = false;
        let mut cancel = false;
        adl_ui::widgets::card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                if dirty == 0 {
                    ui.label(
                        egui::RichText::new("没有未保存的更改")
                            .size(12.5)
                            .color(muted()),
                    );
                } else {
                    adl_ui::widgets::badge(
                        ui,
                        &format!("已更改 {dirty} 项"),
                        pal().warn,
                    );
                    ui.label(
                        egui::RichText::new("改动只是预演，点「保存设置」才生效")
                            .size(12.0)
                            .color(muted()),
                    );
                }
                // 按钮靠右：`right_to_left` 里第一个 add 落在最右边，
                // 所以视觉顺序"取消 → 保存"要倒着 add。
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if ui.button("保存设置").clicked() {
                        save = true;
                    }
                    if ui
                        .add_enabled(dirty > 0, egui::Button::new("取消"))
                        .clicked()
                    {
                        cancel = true;
                    }
                });
            });
        });
        if cancel {
            self.reset_settings_draft();
            self.set_status("已撤销未保存的更改", false);
        }
        save
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
        {
            let mut tasks = self.gui.tasks.lock().unwrap();
            tasks.retain(|t| t.status != "completed");
        }
        // 列表变了就等于用户改过任务表：落盘，否则重启后已完成的任务
        // 又"复活"回列表里。
        save_tasks(&self.gui);
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

    /// 执行「作用于单个任务」的插件动作，把任务的文件信息塞进 payload。
    ///
    /// 插件拿不到全局任务表（`getTask` 在宿主里还是空实现），所以这里
    /// 主动把 `path` / `dir` / `url` / `name` 一起递过去 —— 媒体插件至少
    /// 得知道要转哪个文件。
    ///
    /// 🔴 **必须丢到子线程**：转码一个 1 GB 的视频要几分钟，而 `run_action`
    /// 是同步等待 JS 返回的。在 UI 线程上直接调，整个窗口会卡死到转完为止。
    /// 结果从 `pending_notices` 回来，走的是和后台下载任务同一条通道。
    fn run_task_action(&mut self, plugin: &str, action: &str, id: u64) {
        let task = snapshot(&self.gui).into_iter().find(|t| t.id == id);
        let Some(t) = task else {
            self.set_status(format!("任务 #{id} 已经不在了"), true);
            return;
        };
        if t.path.as_ref().map(|p| p.is_empty()).unwrap_or(true) {
            self.set_status("这个任务还没有落盘文件，等下载完再操作", true);
            return;
        }
        let name = display_name(&t);
        let payload = json!({
            "taskId": t.id,
            "url": t.url,
            "name": name,
            "path": t.path,
            "dir": t.dir,
            "status": t.status,
            "total": t.total,
        });
        let hooks = self.gui.hooks.lock().unwrap().clone();
        let gui = self.gui.clone();
        let plugin = plugin.to_string();
        let action = action.to_string();
        self.set_status(format!("已交给插件处理：{name}"), false);
        std::thread::spawn(move || {
            let res = hooks.run_action(&plugin, &action, &payload);
            let (level, title, body) = match res {
                Some(v) => {
                    let ok = v.get("ok").and_then(|x| x.as_bool()).unwrap_or(true);
                    let msg = v
                        .get("message")
                        .and_then(|x| x.as_str())
                        .unwrap_or(if ok { "插件动作已完成" } else { "插件动作失败" })
                        .to_string();
                    let lv = if ok {
                        crate::notice::Level::Ok
                    } else {
                        crate::notice::Level::Error
                    };
                    (lv, format!("{plugin} · {name}"), msg)
                }
                None => (
                    crate::notice::Level::Warn,
                    format!("{plugin} · {name}"),
                    "插件动作不可用（需要信任模式）".to_string(),
                ),
            };
            if let Ok(mut q) = gui.pending_notices.lock() {
                q.push((level, title, body));
            }
        });
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

/// 设置卡的小节标题：`dirty == true` 时用强调色，提示"这张卡里改过东西"。
///
/// 用户 m19629 要求"改了就是绿色的"，指的就是这种即时反馈：
/// 草稿改过但还没保存的项是绿的，下面再用 `hint_was` 把原值用黄字标出来。
fn dirty_title(ui: &mut egui::Ui, title: &str, dirty: bool) {
    let color = if dirty { pal().accent } else { pal().ink };
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(
            egui::RichText::new(title)
                .size(theme().text.section)
                .strong()
                .color(color),
        );
        if dirty {
            adl_ui::widgets::badge(ui, "已修改", pal().accent);
        }
    });
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

        // 开机自启拉起的那一次：缩到托盘、不弹主窗口。
        //
        // 只在第一帧做一次（`boot_hidden` 立刻置 false），否则用户从托盘点
        // "显示主界面"之后下一帧又会被这里藏回去。
        //
        // 🔴 前提是托盘真的注册上了：托盘没起来的时候藏窗口 = 用户从此
        // 再也叫不出界面（任务管理器里才找得到）。所以这里必须看
        // `_tray` 在不在，不在就当普通双击处理，宁可弹窗。
        if self.boot_hidden {
            self.boot_hidden = false;
            if self._tray.is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                logfile::log("[boot] 已缩到托盘");
            } else {
                logfile::log("[boot] 托盘不可用，取消静默启动、正常显示主窗口");
            }
        }

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
                    let s = p.display().to_string().replace('\\', "/");
                    // 同一个接收端服务两处入口：设置页的默认目录、
                    // 新建下载弹窗里的单次目录。`pick_to_add` 记住是谁发起的。
                    if self.pick_to_add {
                        self.f_dir = s;
                    } else {
                        self.s_dir = s;
                    }
                }
                self.pick_rx = None;
                self.pick_to_add = false;
            } else {
                // 对话框开着的时候界面也得继续呼吸（不然用户以为程序死了）
                ctx.request_repaint_after(std::time::Duration::from_millis(120));
            }
        }

        // 「从压缩包安装」的结果：真正的解包在子线程做完了，这里只负责
        // 重建 hooks + 让插件快照立刻失效 + 报一句结果。
        if let Some(rx) = &self.zip_rx {
            match rx.try_recv() {
                Ok(Ok(id)) => {
                    self.zip_rx = None;
                    let new_cfg = self.gui.cfg.lock().unwrap().clone();
                    *self.gui.hooks.lock().unwrap() = build_hooks(&new_cfg);
                    self.plugin_snapshot_at = Instant::now() - Duration::from_secs(3600);
                    self.set_status(format!("已安装插件 {id}（压缩包已删除）"), false);
                }
                Ok(Err(e)) => {
                    self.zip_rx = None;
                    // 空串 = 用户在对话框里点了取消，什么都不用说。
                    if !e.is_empty() {
                        self.set_status(format!("安装失败：{e}"), true);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(120));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.zip_rx = None;
                }
            }
        }

        // 插件市场的两个后台任务：拉索引、装某个条目。
        //
        // 这里的 `market_rx` 在拿到结果后置 None —— 拉失败也不会每帧重试，
        // 否则连不上 GitHub 时会变成一秒几次的无效请求。
        if let Some(rx) = &self.market_rx {
            match rx.try_recv() {
                Ok(Ok((entries, skipped))) => {
                    self.market_rx = None;
                    self.market_entries = entries;
                    self.market_note = if skipped.is_empty() {
                        None
                    } else {
                        Some((format!("跳过 {} 条：{}", skipped.len(), skipped.join("；")), true))
                    };
                }
                Ok(Err(e)) => {
                    self.market_rx = None;
                    self.market_note = Some((e, true));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(120));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.market_rx = None;
                    self.market_note = Some(("拉取插件索引的连接断了".to_string(), true));
                }
            }
        }
        if let Some(rx) = &self.market_install_rx {
            match rx.try_recv() {
                Ok(Ok(id)) => {
                    self.market_install_rx = None;
                    self.market_installing = None;
                    let new_cfg = self.gui.cfg.lock().unwrap().clone();
                    *self.gui.hooks.lock().unwrap() = build_hooks(&new_cfg);
                    self.plugin_snapshot_at = Instant::now() - Duration::from_secs(3600);
                    self.notify(
                        notice::Level::Ok,
                        format!("插件已安装：{id}"),
                        "可以到「已安装」里打开它的开关。",
                    );
                }
                Ok(Err(e)) => {
                    self.market_install_rx = None;
                    self.market_installing = None;
                    self.set_status(format!("安装失败：{e}"), true);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(120));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.market_install_rx = None;
                    self.market_installing = None;
                }
            }
        }

        // 自动更新：查版本 → 下载安装包。两段都拿到结果就清空通道，
        // 否则连不上 GitHub 时会每帧重试。
        if let Some(rx) = &self.update_rx {
            match rx.try_recv() {
                Ok(Ok(info)) => {
                    self.update_rx = None;
                    let newer = update::is_newer(update::CURRENT_VERSION, &info.version);
                    self.update_note = if newer {
                        Some((format!("发现新版本 {}", info.version), false))
                    } else {
                        Some((format!("已是最新（{}）", update::CURRENT_VERSION), false))
                    };
                    self.update_info = Some(info);
                }
                Ok(Err(e)) => {
                    self.update_rx = None;
                    self.update_note = Some((e, true));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(150));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.update_rx = None;
                    self.update_note = Some(("检查更新的连接断了".to_string(), true));
                }
            }
        }
        if let Some(rx) = &self.update_dl_rx {
            match rx.try_recv() {
                Ok(Ok(path)) => {
                    self.update_dl_rx = None;
                    self.update_downloading = false;
                    self.update_staged = Some(path);
                    self.update_note = Some(("安装包已下好，点「重启并安装」".to_string(), false));
                }
                Ok(Err(e)) => {
                    self.update_dl_rx = None;
                    self.update_downloading = false;
                    self.update_note = Some((e, true));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(150));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.update_dl_rx = None;
                    self.update_downloading = false;
                    self.update_note = Some(("下载安装包时连接断了".to_string(), true));
                }
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

        // 状态提示条（6 秒后消失）——浮在内容上方的圆角小条，不占布局
        //
        // 原来是一条通栏色带（`Panel::top` + 满宽底色），压在标题栏下面：
        // 方角、通宽、和周围全是圆角卡片的界面格格不入（用户说"突兀、
        // 不适配"）。改成和右下角 toast 同一套形态：圆角 + 描边 + 左侧色点，
        // 只是锚在顶部居中，宽度随文字自适应、上限 520。
        //
        // 最后 0.45 秒做淡出，否则会"啪"地消失；淡出期间持续请求重绘，
        // 不然静止的窗口只会重绘一次就停在半透明。
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
                egui::Area::new(egui::Id::unique("adl_statusbar"))
                    .order(egui::Order::Middle)
                    .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 54.0))
                    // `Area` 自带 fade_in 会和这里的 alpha 叠乘，显得发虚。
                    .fade_in(false)
                    .interactable(false)
                    .show(&ctx, |ui| {
                        ui.set_opacity(alpha);
                        egui::Frame::new()
                            .fill(fade_color(fill, alpha))
                            // 描边用同色系但更重一档，弱到只在浅底上勾出边界；
                            // 不用 `card` 的白底 + 阴影，那是给可交互卡片的。
                            .stroke(egui::Stroke::new(1.0, fade_color(color.gamma_multiply(0.35), alpha)))
                            .corner_radius(egui::CornerRadius::same(8))
                            .inner_margin(egui::Margin::symmetric(14, 8))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let dot_at = egui::pos2(
                                        ui.cursor().left() + 4.0,
                                        ui.cursor().top() + 8.0,
                                    );
                                    ui.painter().circle_filled(dot_at, 3.4, fade_color(color, alpha));
                                    ui.add_space(14.0);
                                    ui.label(
                                        egui::RichText::new(if is_err { "失败" } else { "完成" })
                                            .size(12.5)
                                            .strong()
                                            .color(fade_color(color, alpha)),
                                    );
                                    ui.add_space(6.0);
                                    ui.label(
                                        egui::RichText::new(text)
                                            .size(12.5)
                                            .color(fade_color(ink(), alpha)),
                                    );
                                });
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

                // 设置页的「保存 / 取消 / 已更改 N 项」固定在标题下面。
                //
                // 必须放在页面级 ScrollArea **外面**：设置页有十来张卡片，
                // 保存按钮原来在页面最底部，改完靠上的某项还得一路滚下去。
                if self.tab == Tab::Settings {
                    if self.settings_bar(ui) {
                        self.do_save_settings();
                    }
                    ui.add_space(8.0);
                }

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
                                Tab::TaskDetail(id) => self.page_task_detail(ui, id),
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
        } else if self.del_ask.is_some() {
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

        // 删除任务确认
        if self.del_ask.is_some() {
            self.show_delete_modal(&ctx);
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
            Tab::TaskDetail(id) => format!("任务 #{id}"),
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

    /// 单个任务的详情页：热力矩阵 + 每条线路的下载量。
    ///
    /// 数据来源分两块：
    /// - **已完成的分片**来自落盘的 `.adl` 控制文件里的位图（1 bit = 1 片，
    ///   片长 `DEFAULT_PIECE_LENGTH` = 1 MiB）—— 这是唯一能拿到"整张图"的地方；
    /// - **正在跑的分片**来自 `TaskView.runs`（引擎每 64 KB 上报一次，
    ///   见 `adl_core::hooks::RunInfo`），它带真实区间与已写字节。
    ///
    /// 两条合起来才画得出"哪块已经好了、哪块正在动、哪块还没开始"。
    fn page_task_detail(&mut self, ui: &mut egui::Ui, id: u64) {
        ui.add_space(2.0);
        let tasks = snapshot(&self.gui);
        let Some(t) = tasks.iter().find(|t| t.id == id) else {
            adl_ui::widgets::card(ui, |ui| {
                adl_ui::widgets::section_title(ui, "任务已不存在");
                ui.add_space(8.0);
                adl_ui::widgets::hint(ui, "它可能已经被删掉了。");
                ui.add_space(10.0);
                if ui.button("← 回到任务列表").clicked() {
                    self.tab = Tab::Tasks;
                }
            });
            return;
        };

        let name = display_name(t);
        let (status_txt, status_color) = status_style(&t.status);
        let frac = if t.total > 0 {
            (t.completed as f32 / t.total as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };

        // ── 顶部：名字 + 状态 + 主要动作 ──
        adl_ui::widgets::card(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("←").on_hover_text("回到任务列表").clicked() {
                    self.tab = Tab::Tasks;
                }
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(&name)
                        .size(15.0)
                        .strong()
                        .color(pal().ink),
                );
                ui.add_space(8.0);
                adl_ui::widgets::badge(ui, status_txt, status_color);

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("删除…").clicked() {
                        self.del_ask = Some(DeleteAsk {
                            id: t.id,
                            name: name.clone(),
                            has_file: t.path.as_ref().map(|p| !p.is_empty()).unwrap_or(false),
                            delete_files: t.path.is_some(),
                            remember: false,
                        });
                    }
                    if matches!(t.status.as_str(), "paused" | "failed") {
                        if ui.button("继续").clicked() {
                            let _ = resume_task(&self.gui, &json!({"id": id}));
                        }
                    } else if matches!(t.status.as_str(), "downloading" | "queued") {
                        if ui.button("暂停").clicked() {
                            let _ = pause_task(&self.gui, &json!({"id": id}));
                        }
                    }
                    if ui.button("打开目录").clicked() {
                        if let Some(p) = t.path.as_ref().filter(|p| !p.is_empty()) {
                            let dir = std::path::Path::new(p)
                                .parent()
                                .map(|d| d.to_path_buf())
                                .unwrap_or_else(|| std::path::PathBuf::from(p));
                            let _ = platform::open_path(&dir);
                        } else {
                            self.set_status("这个任务还没有落盘路径", true);
                        }
                    }
                });
            });

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let bar_w = (ui.available_width() - 90.0).max(80.0);
                ui.add(
                    egui::ProgressBar::new(frac)
                        .desired_width(bar_w)
                        .desired_height(8.0)
                        .corner_radius(egui::CornerRadius::same(3))
                        .fill(teal()),
                );
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(format!("{:.1}%", frac * 100.0))
                        .small()
                        .color(muted()),
                );
            });

            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                let kv = |ui: &mut egui::Ui, k: &str, v: String| {
                    ui.label(egui::RichText::new(k).small().color(muted()));
                    ui.label(egui::RichText::new(v).small().color(pal().ink));
                    ui.add_space(14.0);
                };
                kv(ui, "已下载", format_bytes(t.completed));
                kv(ui, "总大小", format_bytes(t.total));
                kv(
                    ui,
                    "速度",
                    if t.speed > 0 { format_speed(t.speed) } else { "—".into() },
                );
                kv(ui, "活跃线路", format!("{} / {}", t.connections, t.runs.len()));
            });

            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("地址").small().color(muted()));
                ui.label(egui::RichText::new(&t.url).small().color(pal().ink));
            });
            if let Some(p) = t.path.as_ref().filter(|p| !p.is_empty()) {
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new("保存到").small().color(muted()));
                    ui.label(egui::RichText::new(p).small().color(pal().ink));
                });
            }
            if let Some(e) = t.error.as_ref().filter(|e| !e.is_empty()) {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(e).small().color(pal().danger));
            }
        });

        // ── 分片热力矩阵 ──
        ui.add_space(10.0);
        adl_ui::widgets::card(ui, |ui| {
            ui.horizontal(|ui| {
                adl_ui::widgets::section_title(ui, "分片");
                ui.add_space(8.0);
                adl_ui::widgets::hint(
                    ui,
                    "一格 = 1 MiB。绿=已完成，橙=正在下载，灰=还没开始。",
                );
            });
            ui.add_space(10.0);

            match piece_states(t) {
                Some((pieces, done, active)) => {
                    piece_matrix(ui, pieces, &done, &active);
                    ui.add_space(8.0);
                    let done_n = done.iter().filter(|b| **b).count();
                    adl_ui::widgets::hint(
                        ui,
                        &format!(
                            "共 {pieces} 片，已完成 {done_n} 片，正在下载 {} 片。",
                            active.len()
                        ),
                    );
                }
                None => {
                    adl_ui::widgets::hint(
                        ui,
                        "还没有控制文件（任务刚建立或已清理），暂时画不出分片图。",
                    );
                }
            }
        });

        // ── 每条线路的下载量 ──
        ui.add_space(10.0);
        adl_ui::widgets::card(ui, |ui| {
            ui.horizontal(|ui| {
                adl_ui::widgets::section_title(ui, "活跃线路");
                ui.add_space(8.0);
                adl_ui::widgets::hint(
                    ui,
                    "引擎此刻正在跑的分片区间与各自写入的字节数。",
                );
            });
            ui.add_space(10.0);

            if t.runs.is_empty() {
                adl_ui::widgets::hint(
                    ui,
                    if t.status == "downloading" {
                        "这一帧没有在跑的线路（可能正在重试或等待调度）。"
                    } else {
                        "任务没在跑，没有活跃线路。"
                    },
                );
                return;
            }

            let mut runs: Vec<&adl_core::hooks::RunInfo> = t.runs.iter().collect();
            runs.sort_by_key(|r| r.start);
            for (i, r) in runs.iter().enumerate() {
                let span = r.end.saturating_sub(r.start) + 1;
                let ratio = if span > 0 {
                    (r.written as f32 / span as f32).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("#{}", i + 1))
                            .small()
                            .color(muted()),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "{:>12} – {:>12}",
                            format_bytes(r.start),
                            format_bytes(r.end)
                        ))
                        .monospace()
                        .small()
                        .color(pal().ink),
                    );
                    ui.add_space(8.0);
                    let w = (ui.available_width() - 150.0).max(60.0);
                    ui.add(
                        egui::ProgressBar::new(ratio)
                            .desired_width(w)
                            .desired_height(6.0)
                            .corner_radius(egui::CornerRadius::same(3))
                            .fill(pal().warn),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "{} / {}",
                            format_bytes(r.written),
                            format_bytes(span)
                        ))
                        .small()
                        .color(muted()),
                    );
                });
                ui.add_space(4.0);
            }
        });
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

        let mut tasks = snapshot(&self.gui);

        // ── 搜索 / 筛选 / 排序（用户 m18583）───────────────
        //
        // 先过滤再排序：`total` 与 `completed` 都是过滤后的子集，计数才对得上。
        // 默认排序是 `status`，它把"正在下载"顶到最上面、已完成的沉到最下面 ——
        // 用户明确要求过这一点（原来按 id 排，新下的反而在最下面）。
        let mut filter_changed = false;
        // Some(true) = 点了「全部继续」；Some(false) = 点了「全部暂停」。
        let mut bulk_action: Option<bool> = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let search_w = 220.0;
            ui.add(
                egui::TextEdit::singleline(&mut self.task_search)
                    .hint_text("搜索文件名 / 地址")
                    .desired_width(search_w),
            );
            egui::ComboBox::from_id_salt("adl_task_filter")
                .width(96.0)
                .selected_text(match self.task_filter.as_str() {
                    "downloading" => "下载中",
                    "queued" => "排队中",
                    "paused" => "已暂停",
                    "completed" => "已完成",
                    "failed" => "失败",
                    _ => "全部",
                })
                .show_ui(ui, |ui| {
                    for (v, label) in [
                        ("all", "全部"),
                        ("downloading", "下载中"),
                        ("queued", "排队中"),
                        ("paused", "已暂停"),
                        ("completed", "已完成"),
                        ("failed", "失败"),
                    ] {
                        if ui.selectable_label(self.task_filter == v, label).clicked() {
                            self.task_filter = v.to_string();
                            filter_changed = true;
                        }
                    }
                });
            egui::ComboBox::from_id_salt("adl_task_sort")
                .width(104.0)
                .selected_text(match self.task_sort.as_str() {
                    "added" => "按添加时间",
                    "name" => "按文件名",
                    "size" => "按大小",
                    "progress" => "按进度",
                    _ => "按状态",
                })
                .show_ui(ui, |ui| {
                    for (v, label) in [
                        ("status", "按状态"),
                        ("added", "按添加时间"),
                        ("name", "按文件名"),
                        ("size", "按大小"),
                        ("progress", "按进度"),
                    ] {
                        if ui.selectable_label(self.task_sort == v, label).clicked() {
                            self.task_sort = v.to_string();
                            filter_changed = true;
                        }
                    }
                });
            ui.label(
                egui::RichText::new(format!("{} 条", tasks.len()))
                    .small()
                    .color(muted()),
            );
            // 批量控制放在右边：任务一多，逐行点暂停很烦。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("全部继续").clicked() {
                    bulk_action = Some(true);
                }
                if ui.small_button("全部暂停").clicked() {
                    bulk_action = Some(false);
                }
            });
        });
        if filter_changed {
            // 筛选/排序是用户偏好，下次打开还要在，所以落配置里。
            let mut cfg = self.gui.cfg.lock().unwrap();
            cfg.task_filter = self.task_filter.clone();
            cfg.task_sort = self.task_sort.clone();
            let _ = cfg.save();
        }
        match bulk_action {
            Some(true) => {
                resume_all(&self.gui);
                self.set_status("已把暂停/失败的任务全部放回队列", false);
            }
            Some(false) => {
                pause_all(&self.gui);
                self.set_status("已暂停全部任务", false);
            }
            None => {}
        }

        let needle = self.task_search.trim().to_lowercase();
        if !needle.is_empty() {
            tasks.retain(|t| {
                t.url.to_lowercase().contains(&needle)
                    || display_name(t).to_lowercase().contains(&needle)
            });
        }
        if self.task_filter != "all" {
            tasks.retain(|t| t.status == self.task_filter);
        }
        // 状态排序：下载中 → 排队中 → 暂停/失败 → 完成，同档内按 id 正序
        // （先加的在上面，符合"队列"的直觉）。
        fn rank(s: &str) -> u8 {
            match s {
                "downloading" => 0,
                "queued" => 1,
                "paused" | "failed" => 2,
                _ => 3,
            }
        }
        match self.task_sort.as_str() {
            "added" => tasks.sort_by_key(|t| (std::cmp::Reverse(t.added_unix), t.id)),
            "name" => tasks.sort_by(|a, b| display_name(a).cmp(&display_name(b))),
            "size" => tasks.sort_by_key(|t| (std::cmp::Reverse(t.total), t.id)),
            "progress" => tasks.sort_by(|a, b| {
                let fa = if b.total > 0 { a.completed as f64 / b.total as f64 } else { 0.0 };
                let fb = if b.total > 0 { b.completed as f64 / b.total as f64 } else { 0.0 };
                fb.partial_cmp(&fa).unwrap_or(std::cmp::Ordering::Equal)
            }),
            // 默认：状态档优先，再按 id（先加的在上）
            _ => tasks.sort_by_key(|t| (rank(&t.status), t.id)),
        }

        if tasks.is_empty() {
            if snapshot(&self.gui).is_empty() {
                adl_ui::widgets::empty_state(ui, "还没有下载任务，点左上角「+ 新建下载」开始。");
            } else {
                adl_ui::widgets::empty_state(ui, "没有符合当前筛选条件的任务。");
            }
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
            // 152 而不是 104：多了一个「复制」。三个两字按钮 + 间距刚好放下，
            // 挤到 104 的时候「删除」会被表右边裁掉（虚拟表格不给横向滚动）。
            adl_ui::Column::new("操作", 152.0),
        ];

        let mut pause_id: Option<u64> = None;
        let mut resume_id: Option<u64> = None;
        let mut remove_id: Option<u64> = None;
        let mut open_dir_id: Option<u64> = None;
        let mut detail_id: Option<u64> = None;
        let mut copy_url: Option<String> = None;
        // 插件声明的「作用于单个任务」的动作，以及这一帧用户点了哪一个。
        let task_actions: Vec<(String, String, String)> = self
            .plugin_snapshot
            .get("actions")
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter(|a| a.get("place").and_then(|x| x.as_str()) == Some("task"))
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
        let mut task_action: Option<(String, String, u64)> = None;

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
                    // 圆角矩形进度条，不是胶囊更不是圆点。
                    //
                    // 原来只给了 `desired_width`，高度就落到主题的
                    // `interact_size.y = 30`；而 ProgressBar 的默认圆角是
                    // **高度的一半**（`progress_bar.rs:137-138`），于是
                    // 20% 时填充宽度被 `min_width = 2*圆角 = 30` 兜住，
                    // 画出来是个 30×30 的实心圆（用户 m15179 截图）。
                    // 现在把高度压到 8、圆角给 3、最小填充宽度自然变成 6，
                    // 才是「细长圆角条」。
                    ui.horizontal(|ui| {
                        let bar_w = (w[2] - 52.0).max(40.0);
                        ui.add(
                            egui::ProgressBar::new(frac)
                                .desired_width(bar_w)
                                .desired_height(8.0)
                                .corner_radius(egui::CornerRadius::same(3))
                                .fill(teal()),
                        );
                        ui.label(
                            egui::RichText::new(format!("{:.0}%", frac * 100.0))
                                .small()
                                .color(muted()),
                        );
                    });
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
                    // 只有下载中的任务能暂停，其余状态这里留空会显得缺一块，
                    // 所以已完成/失败/暂停的给一个「继续」。
                    if t.status == "downloading" {
                        if ui.small_button("暂停").clicked() {
                            pause_id = Some(t.id);
                        }
                    } else if t.status == "paused" {
                        if ui
                            .small_button("继续")
                            .on_hover_text("从断点继续下载")
                            .clicked()
                        {
                            resume_id = Some(t.id);
                        }
                    }
                    // 其余操作收进「⋯」：三个两字按钮挤在 152px 里既挤又
                    // 容易点错（用户 m15179：「这些按钮有点挤和奇怪」）。
                    // 菜单里的项都带完整文字，读起来比挤成两个字清楚。
                    let more = adl_ui::widgets::more_button(ui)
                        .on_hover_text("更多操作");
                    egui::Popup::menu(&more).show(|ui| {
                        ui.set_min_width(180.0);
                        let has_path = t
                            .path
                            .as_ref()
                            .map(|p| !p.is_empty())
                            .unwrap_or(false);
                        if ui
                            .add_enabled(
                                has_path,
                                egui::Button::new("打开文件所在位置").frame(false),
                            )
                            .clicked()
                        {
                            open_dir_id = Some(t.id);
                            ui.close();
                        }
                        if ui
                            .add(egui::Button::new("查看详情…").frame(false))
                            .clicked()
                        {
                            detail_id = Some(t.id);
                            ui.close();
                        }
                        if ui
                            .add(egui::Button::new("复制下载地址").frame(false))
                            .clicked()
                        {
                            copy_url = Some(t.url.clone());
                            ui.close();
                        }
                        // 插件声明的、作用于单个任务的动作（`place = "task"`）。
                        // 放在这里而不是工具栏：转码这类动作天然要指定"哪一个文件"。
                        for (plugin, action, label) in task_actions.iter() {
                            if ui
                                .add(egui::Button::new(label.as_str()).frame(false))
                                .clicked()
                            {
                                task_action = Some((plugin.clone(), action.clone(), t.id));
                                ui.close();
                            }
                        }
                        ui.separator();
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("删除任务…")
                                        .color(egui::Color32::from_rgb(0xB0, 0x30, 0x30)),
                                )
                                .frame(false),
                            )
                            .clicked()
                        {
                            remove_id = Some(t.id);
                            ui.close();
                        }
                    });
                });
            });
        });

        if let Some(id) = pause_id {
            let _ = pause_task(&self.gui, &json!({"id": id}));
        }
        if let Some(id) = resume_id {
            // 继续 = 把这一条放回队列（原行复用），不再新开一条。
            match resume_task(&self.gui, &json!({"id": id})) {
                Ok(_) => self.set_status(format!("已把 #{id} 放回队列"), false),
                Err(e) => self.set_status(e, true),
            }
        }
        if let Some(id) = open_dir_id {
            // 打开文件所在目录，不打开文件本身 —— 用户想干的是"看一眼""拷走"。
            if let Some(t) = tasks.iter().find(|t| t.id == id) {
                if let Some(p) = t.path.as_ref().filter(|p| !p.is_empty()) {
                    let dir = std::path::Path::new(p)
                        .parent()
                        .map(|d| d.to_path_buf())
                        .unwrap_or_else(|| std::path::PathBuf::from(p));
                    if let Err(e) = platform::open_path(&dir) {
                        self.set_status(format!("打开目录失败: {e}"), true);
                    }
                } else {
                    self.set_status("这个任务还没有落盘路径", true);
                }
            }
        }
        if let Some(id) = detail_id {
            self.tab = Tab::TaskDetail(id);
        }
        if let Some(id) = remove_id {
            // 删除前先问一句"本地文件要不要一起删"。用户 m15179 要求的
            // "记住选项"存在 cfg.delete_files_choice 里，问过一次就不再问。
            let remembered = self.gui.cfg.lock().unwrap().delete_files_choice;
            if let Some(del_files) = remembered {
                self.do_remove_task(id, del_files);
            } else if let Some(t) = tasks.iter().find(|t| t.id == id) {
                self.del_ask = Some(DeleteAsk {
                    id,
                    name: display_name(t),
                    has_file: t.path.as_ref().map(|p| !p.is_empty()).unwrap_or(false),
                    delete_files: t.path.is_some(),
                    remember: false,
                });
            }
        }
        if let Some(url) = copy_url {
            ui.ctx().copy_text(url);
            self.set_status("已复制下载地址", false);
        }
        if let Some((plugin, action, id)) = task_action {
            self.run_task_action(&plugin, &action, id);
        }
    }

    /// 真正执行删除。`files` = 是否连本地文件与控制文件一起删。
    fn do_remove_task(&mut self, id: u64, files: bool) {
        match remove_task(&self.gui, &json!({"id": id, "files": files})) {
            Ok(_) => self.set_status(
                if files { format!("已删除任务 #{id} 和本地文件") } else { format!("已删除任务 #{id}") },
                false,
            ),
            Err(e) => self.set_status(e, true),
        }
    }

    /// 删除确认弹窗：问一句"本地文件要不要一起删"，可勾"记住"。
    fn show_delete_modal(&mut self, ctx: &egui::Context) {
        let mut go: Option<bool> = None;
        let mut cancel = false;

        let a = self.fade.alpha(FADE_SECS);
        let resp = egui::Modal::new(egui::Id::unique("adl_delete_modal"))
            .frame(fade_frame(ctx, a))
            .backdrop_color(fade_color(egui::Color32::from_black_alpha(100), a))
            .show(ctx, |ui| {
            ui.set_opacity(a);
            ui.set_width(420.0);
            adl_ui::widgets::section_title(ui, "删除任务");
            ui.add_space(10.0);
            if let Some(ask) = self.del_ask.as_ref() {
                adl_ui::widgets::hint(ui, &format!("将删除任务「{}」。", ask.name));
            }
            ui.add_space(10.0);

            if let Some(ask) = self.del_ask.as_mut() {
                if !ask.has_file {
                    adl_ui::widgets::hint(ui, "这个任务还没有落盘文件，只会删掉任务记录。");
                } else {
                    ui.checkbox(&mut ask.delete_files, "同时删除本地文件和控制文件（不可恢复）");
                }
                ui.add_space(8.0);
                ui.checkbox(&mut ask.remember, "记住我的选择（以后不再询问）");
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    let del_files = ask.delete_files && ask.has_file;
                    if ui.button("删除").clicked() {
                        go = Some(del_files);
                    }
                    if ui.button("取消").clicked() {
                        cancel = true;
                    }
                });
            }
        });

        if resp.should_close() {
            cancel = true;
        }
        if cancel {
            self.del_ask = None;
            return;
        }
        if let Some(del_files) = go {
            let (id, remember) = match self.del_ask.take() {
                Some(a) => (a.id, a.remember),
                None => return,
            };
            if remember {
                // 记住 = 以后所有删除都按这次的选择，不再弹这个框。
                let mut cfg = self.gui.cfg.lock().unwrap();
                cfg.delete_files_choice = Some(del_files);
                let _ = cfg.save();
            }
            self.do_remove_task(id, del_files);
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
            // 多行 = 批量添加。用户 m18583：「批量添加的话可以添加一下多行黏贴」。
            // 单行时行为和以前完全一样（回车即开始），多行时按行拆开逐条入队。
            let multiline = self.f_url.contains('\n');
            let url_edit = ui.add(
                egui::TextEdit::multiline(&mut self.f_url)
                    .hint_text("https://…（粘贴多行可以一次添加多条）")
                    .desired_width(f32::INFINITY)
                    .desired_rows(if multiline { 5 } else { 1 }),
            );
            if !multiline
                && url_edit.lost_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter))
            {
                start = true;
            }
            if multiline {
                let n = self
                    .f_url
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .count();
                adl_ui::widgets::hint(ui, &format!("已识别 {n} 条地址，会一起入队。"));
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
            ui.label("保存到目录");
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let pick_w = 92.0;
                let edit_w = (ui.available_width() - pick_w - 8.0).max(80.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.f_dir)
                        .hint_text("可选，留空用设置里的默认下载目录")
                        .desired_width(edit_w),
                );
                if ui.button("选择…").clicked() {
                    self.pick_rx = Some(start_pick_folder());
                    self.pick_to_add = true;
                }
            });

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
                    if let Some(r) = adl_ui::widgets::segmented(ui, "home_range", self.home_range, &items) {
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

        // 草稿模式下每张卡都要回答两个问题："这项改了吗"、"原来是什么"。
        // 这里一次性把已保存的值抄出来，下面各卡直接比、直接显示原值。
        let (
            was_conn,
            was_limit,
            was_limit_unit,
            was_dir,
            was_proxy,
            was_ua,
            was_mode,
            was_max_tasks,
            was_check_update,
            was_close,
        ) = {
            let cfg = self.gui.cfg.lock().unwrap();
            (
                cfg.connections as u64,
                cfg.limit,
                if cfg.limit_unit.trim().is_empty() {
                    "auto".to_string()
                } else {
                    cfg.limit_unit.clone()
                },
                cfg.dir.clone(),
                cfg.proxy.clone(),
                cfg.user_agent.clone(),
                if cfg.mode_trusted() {
                    "trusted".to_string()
                } else {
                    "safe".to_string()
                },
                cfg.max_concurrent_tasks as u64,
                cfg.check_update,
                if cfg.close_action.trim().is_empty() {
                    "tray".to_string()
                } else {
                    cfg.close_action.clone()
                },
            )
        };
        // 自启动的"原值"不取配置，取注册表实际状态（任务管理器关掉后
        // 配置里还是 true，拿配置比会永远显示"已更改"）。
        let was_auto_start = platform::auto_start_enabled();
        // 字符串项的原值只在真的改过时才传给 `Field::mark`（`None` = 没改，
        // 不画那一行黄字）。空串也算一个合法原值，所以不能拿"非空"当条件。
        let was_dir = (self.s_dir.trim() != was_dir.trim()).then_some(was_dir);
        let was_proxy = (self.s_proxy.trim() != was_proxy.trim()).then_some(was_proxy);
        let was_ua = (self.s_ua.trim() != was_ua.trim()).then_some(was_ua);

        // ── 并发：只给档位，不给自由输入 ──
        // 用户要求"最大值 + 固定 1/2/4/8/16/32/64，本质还是自适应"：
        // 这里选的是**上限**，引擎会按服务器实际放行量自动收敛。
        //
        // 从 m19629 起，这一项也是草稿：改动只动 `self.s_conn`，点「保存设置」
        // 才写进配置。以前是"点一下立刻落盘"，用户想反悔只能自己记住原值再点回去。
        adl_ui::widgets::card(ui, |ui| {
            dirty_title(ui, "并发上限", self.s_conn != was_conn);
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
            if let Some(v) = adl_ui::widgets::segmented(ui, "conn_limit", self.s_conn, &opts) {
                self.s_conn = v;
            }
            if self.s_conn != was_conn {
                adl_ui::widgets::hint_was(ui, &format!("原：{}", was_conn));
            }
            ui.add_space(6.0);
            adl_ui::widgets::hint(
                ui,
                "服务器通常不会给满（常见 16 只放 9）。引擎会按实际放行量自动收敛，\
                 稳定后偶尔试探 +1，被拒就退回并把试探间隔翻倍，避免触发反爬。",
            );
        });

        ui.add_space(12.0);

        // ── 限速：数值 + 单位选择（英文单位，中文读起来别扭）──
        adl_ui::widgets::card(ui, |ui| {
            let dirty = self.s_limit != was_limit || self.limit_unit != was_limit_unit;
            dirty_title(ui, "限速", dirty);
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
            if dirty {
                adl_ui::widgets::hint_was(
                    ui,
                    &format!(
                        "原：{}（{}）",
                        if was_limit == 0 {
                            "不限速".to_string()
                        } else {
                            format_speed_unit(was_limit, &was_limit_unit)
                        },
                        was_limit_unit
                    ),
                );
                ui.add_space(4.0);
            }
            adl_ui::widgets::hint(
                ui,
                "0 = 不限速。限速是全局上限，所有任务共享同一个令牌桶。",
            );
        });

        ui.add_space(12.0);

        // ── 同时下载的任务数（队列上限）──
        // 这是"同时跑几条任务"，和上面的"并发上限"（一条任务开几条连接）是两回事：
        // 前者决定带宽怎么分给不同文件，后者决定单个文件能不能吃满带宽。
        adl_ui::widgets::card(ui, |ui| {
            dirty_title(ui, "同时下载", self.s_max_tasks != was_max_tasks);
            ui.add_space(4.0);
            let opts: [(u64, &str); 7] = [
                (1, "1"),
                (2, "2"),
                (3, "3"),
                (5, "5"),
                (8, "8"),
                (12, "12"),
                (16, "16"),
            ];
            if let Some(v) = adl_ui::widgets::segmented(ui, "max_tasks", self.s_max_tasks, &opts) {
                self.s_max_tasks = v;
            }
            if self.s_max_tasks != was_max_tasks {
                adl_ui::widgets::hint_was(ui, &format!("原：{}", was_max_tasks));
            }
            ui.add_space(6.0);
            adl_ui::widgets::hint(
                ui,
                "超过这个数量的任务会排在队里等着，前面的下完自动补位。\
                 想先下某个文件，可以在任务列表里把它「继续」——它会被放回队首。",
            );
        });

        ui.add_space(12.0);

        // ── 其它：目录、代理与 UA ──
        // 两个后缀按钮：
        //  - `pick_dir` 弹系统目录选择框（手打 Windows 路径很容易漏反斜杠）
        //  - `fill_ua`   一键填入内置 Chrome 串，用户在这个基础上改，
        //                比"从 blank 开始自己拼一整串 UA"现实得多
        let mut fields = vec![
            adl_ui::Field::text("默认下载目录", &mut self.s_dir)
                .hint("留空 = 系统的「下载」文件夹")
                .action("选择…", "pick_dir")
                .mark(was_dir.as_deref()),
            adl_ui::Field::text("HTTP 代理", &mut self.s_proxy)
                .hint("http://127.0.0.1:7890（留空 = 直连）")
                .mark(was_proxy.as_deref()),
            adl_ui::Field::text("User-Agent", &mut self.s_ua)
                .hint("留空 = 内置的 Chrome 串（推荐）")
                .action("填入 Chrome", "fill_ua")
                .mark(was_ua.as_deref())
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
        // 先算好"改没改"，再借 `self.s_mode`：写成
        // `.mark((self.s_mode != was_mode).then_some(...))` 会和
        // 同一个表达式里的 `&mut self.s_mode` 冲突（E0502）。
        let mode_mark = (self.s_mode != was_mode).then_some(was_mode.as_str());
        let mut plug_fields = vec![
            adl_ui::Field::select("插件模式", &mut self.s_mode, &mode_opts)
                .mark(mode_mark)
                .note("信任模式下插件才能改写请求、注册按钮与替换内置页面"),
        ];
        adl_ui::form(ui, "插件", &mut plug_fields);
        // 插件目录不再可配（用户 m19629）：永远在 exe 同级的 plugins/ 下，
        // 这里只说清楚位置，不给输入框。
        if let Some(root) = plugins_root() {
            adl_ui::widgets::hint(ui, &format!("插件目录：{}", root.display()));
        }

        // ── 软件更新 ──
        // 只认 release 里挂的 .exe：没有 CI 的时候发布可能只带源码包，
        // 那种情况下退化成"打开下载页"，不做自动替换。
        ui.add_space(10.0);
        let mut check_now = false;
        let mut download_now = false;
        let mut apply_now = false;
        adl_ui::widgets::card(ui, |ui| {
            let dirty = self.s_check_update != was_check_update;
            dirty_title(ui, "软件更新", dirty);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let mut on = self.s_check_update;
                if ui.checkbox(&mut on, "启动时检查更新").changed() {
                    self.s_check_update = on;
                }
                if dirty {
                    adl_ui::widgets::badge(
                        ui,
                        &format!(
                            "原：{}",
                            if was_check_update { "开启" } else { "关闭" }
                        ),
                        pal().warn,
                    );
                }
                ui.add_space(6.0);
                let busy = self.update_rx.is_some();
                if ui
                    .add_enabled(!busy, egui::Button::new(if busy { "正在检查…" } else { "检查更新" }))
                    .clicked()
                {
                    check_now = true;
                }
            });
            adl_ui::widgets::hint(
                ui,
                &format!(
                    "当前版本 {}。只检查，不自动装 —— 换二进制要你点一下。",
                    update::CURRENT_VERSION
                ),
            );
            if let Some((msg, err)) = &self.update_note {
                ui.add_space(4.0);
                let color = if *err { pal().danger } else { pal().muted };
                ui.label(egui::RichText::new(msg).size(12.5).color(color));
            }
            // 发布名（release 的 `name`）：GitHub 上常写成「ADL 0.2.0」，
            // 比 tag 好读，直接拿来当标题行。
            if self
                .update_info
                .as_ref()
                .map(|i| update::is_newer(update::CURRENT_VERSION, &i.version))
                .unwrap_or(false)
            {
                if let Some(info) = &self.update_info {
                    if !info.title.trim().is_empty() {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(&info.title)
                                .size(13.0)
                                .strong()
                                .color(pal().ink),
                        );
                    }
                }
            }
            // 查到新版本：给出下载按钮；没挂 exe 就退化成打开发布页。
            let newer = self
                .update_info
                .as_ref()
                .map(|i| update::is_newer(update::CURRENT_VERSION, &i.version))
                .unwrap_or(false);
            if newer {
                if let Some(info) = &self.update_info {
                    ui.add_space(6.0);
                    if !info.notes.trim().is_empty() {
                        // 更新说明只显示前 12 行：GitHub 上的正文可能很长，
                        // 塞满设置页反而把按钮挤下去。
                        let short: String = info
                            .notes
                            .lines()
                            .take(12)
                            .collect::<Vec<_>>()
                            .join("\n");
                        ui.label(egui::RichText::new(short).size(12.0).color(pal().muted));
                        ui.add_space(4.0);
                    }
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 10.0;
                        if info.exe.is_some() {
                            let label = if self.update_downloading {
                                "正在下载…"
                            } else if self.update_staged.is_some() {
                                "已下载"
                            } else {
                                "下载更新"
                            };
                            if ui
                                .add_enabled(
                                    !self.update_downloading && self.update_staged.is_none(),
                                    egui::Button::new(label),
                                )
                                .clicked()
                            {
                                download_now = true;
                            }
                            if self.update_staged.is_some()
                                && ui.button("重启并安装").clicked()
                            {
                                apply_now = true;
                            }
                        } else {
                            // 没挂 exe：只能让用户自己去发布页拿包。
                            if ui.button("打开发布页").clicked() {
                                platform::open_in_browser(&info.page);
                            }
                        }
                    });
                }
            }
        });
        if check_now {
            self.update_note = None;
            self.update_staged = None;
            self.update_rx = Some(update::start_check(&self.gui, String::new()));
        }
        if download_now {
            if let Some(info) = &self.update_info {
                if let Some((_, url, size)) = &info.exe {
                    self.update_downloading = true;
                    self.update_dl_rx = Some(update::start_download(
                        &self.gui,
                        url.clone(),
                        *size,
                    ));
                }
            }
        }
        if apply_now {
            if let Some(path) = self.update_staged.clone() {
                match update::swap_and_spawn(&path) {
                    Ok(_) => {
                        // 替换成功、新进程已拉起。这里必须走正常退出路径：
                        // 直接 `exit` 会跳过配置落盘，还可能和新进程抢单实例锁。
                        self.closing = true;
                        ui.ctx()
                            .send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    Err(e) => self.set_status(e, true),
                }
            }
        }

        // ── 启动 ──
        // 用户 m22520：「记得加一个是否自启动的设置」。刻意做成设置项而不是
        // 安装器选项 —— 绿色版用户根本没机会看到安装界面。
        //
        // 真相在注册表：这个开关的当前状态每次都从 `HKCU\...\Run` 读，
        // 不信 config.toml。任务管理器「启动」页关掉自启时只删注册表项，
        // 只信配置的话界面会显示"开着"但实际不会自启。
        ui.add_space(10.0);
        adl_ui::widgets::card(ui, |ui| {
            let dirty = self.s_auto_start != was_auto_start;
            dirty_title(ui, "启动", dirty);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let mut on = self.s_auto_start;
                if ui.checkbox(&mut on, "开机自动启动").changed() {
                    self.s_auto_start = on;
                }
                if dirty {
                    adl_ui::widgets::badge(
                        ui,
                        &format!("原：{}", if was_auto_start { "开启" } else { "关闭" }),
                        pal().warn,
                    );
                }
            });
            ui.add_space(4.0);
            adl_ui::widgets::hint(
                ui,
                "开机那次直接缩到托盘、不弹主窗口，接着下上次没下完的。\
                 不想让它开机跑，也可以在任务管理器的「启动」页关掉。",
            );
            // 托盘状态：注册失败时用户只知道"没有图标"，不知道原因。
            // 这一行直接摊开说，免得再走"翻 startup.log"那一步。
            let tray_ok = self._tray.is_some() && !self.tray_status.contains("没注册上")
                && !self.tray_status.contains("创建失败");
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let color = if tray_ok { pal().muted } else { pal().danger };
                ui.label(
                    egui::RichText::new(format!("托盘图标：{}", self.tray_status))
                        .size(12.0)
                        .color(color),
                );
            });
        });

        // ── 关闭行为 ──
        // 只有两个选项：问过一次之后就定下来了，"每次询问"不再是可选状态
        // （用户明确要求：选了就不能调回询问）。
        //
        // 从 m19629 起也是草稿：改动只动 `self.s_close_action`，点「保存设置」
        // 才写进配置。真关窗口时的分派逻辑读的是配置，所以必须保存才生效 ——
        // 这正是"先预演、再提交"该有的样子。
        ui.add_space(10.0);
        adl_ui::widgets::card(ui, |ui| {
            let dirty = self.s_close_action != was_close;
            dirty_title(ui, "关闭窗口时", dirty);
            ui.add_space(6.0);
            let cfg_close = was_close.clone();
            let known = matches!(cfg_close.as_str(), "tray" | "close");
            let opts: [(&str, &str); 2] = [("tray", "最小化到托盘"), ("close", "退出程序")];
            ui.horizontal(|ui| {
                for (v, l) in opts {
                    if ui.selectable_label(self.s_close_action == v, l).clicked() {
                        self.s_close_action = v.to_string();
                    }
                }
            });
            if dirty {
                let was_label = if cfg_close == "close" {
                    "退出程序"
                } else {
                    "最小化到托盘"
                };
                adl_ui::widgets::hint_was(ui, &format!("原：{was_label}"));
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

        // 底部不再放「保存设置」——它已经固定到页面顶部了（见 `settings_bar`）。
        // 这里只留一点空白，和上面的插件设置区隔开。

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
        // 排队等名额的状态必须看得见：否则用户以为点了没反应。
        "queued" => ("排队中", egui::Color32::from_rgb(0x5E, 0x6D, 0x6B)),
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

/// 从任务的 `.adl` 控制文件读回"已完成哪些片"。
///
/// 返回 `(总片数, 每片是否完成, 正在跑的分片索引)`；返回 `None` 表示
/// 拿不到控制文件（任务刚建立、已完成被清理、或路径还没落盘），
/// 这时详情页只显示一句说明，不画假的图。
///
/// 关键：**下载过程中 `path` 是空的** —— 目标文件名要探测完才知道，
/// 引擎是在拿到响应头之后才定下来的。但控制文件早就写在下载目录里了，
/// 所以这里先用 `path` 试，失败再按 `dir` + URL 扫一遍 `.adl`
/// （和引擎 `find_control_by_url` 同一个思路）。
fn piece_states(t: &TaskView) -> Option<(usize, Vec<bool>, Vec<usize>)> {
    let cf = control_of(t)?;
    let piece_len = cf.piece_length.max(1);
    let pieces = (cf.total.div_ceil(piece_len)).max(1) as usize;
    let bits = hex::decode(&cf.bitfield).ok()?;
    let mut done = vec![false; pieces];
    for (p, slot) in done.iter_mut().enumerate() {
        *slot = bits
            .get(p / 8)
            .map(|b| b & (1 << (p % 8)) != 0)
            .unwrap_or(false);
    }
    // 正在跑的分片：把 `runs` 的字节区间映射到片号。
    let mut active: Vec<usize> = Vec::new();
    for r in &t.runs {
        let first = (r.start / piece_len) as usize;
        let last = (r.end / piece_len) as usize;
        for p in first..=last.min(pieces.saturating_sub(1)) {
            if !active.contains(&p) {
                active.push(p);
            }
        }
    }
    Some((pieces, done, active))
}

/// 找到这个任务对应的控制文件：先按落盘路径，再按下载目录 + URL 扫。
fn control_of(t: &TaskView) -> Option<adl_core::ControlFile> {
    if let Some(p) = t.path.as_ref().filter(|p| !p.is_empty()) {
        if let Ok(Some(cf)) = adl_core::ControlFile::load(std::path::Path::new(p)) {
            return Some(cf);
        }
    }
    let dir = t.dir.as_ref().filter(|d| !d.is_empty())?;
    let rd = std::fs::read_dir(dir).ok()?;
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some(adl_core::control::CONTROL_EXT) {
            continue;
        }
        // `.adl` 是 `<目标>.adl`，而 `ControlFile::load` 吃的是目标路径。
        let data = p.with_extension("");
        if let Ok(Some(cf)) = adl_core::ControlFile::load(&data) {
            if cf.url == t.url {
                return Some(cf);
            }
        }
    }
    None
}

/// 画分片热力矩阵：每格一片，绿=完成，橙=正在下载，灰=未开始。
///
/// 分片可能上万（几 GB 的文件），所以列数按可用宽度自适应，
/// 行数 = 向上取整。格子最小 3px，再小就一片糊成一块。
fn piece_matrix(ui: &mut egui::Ui, pieces: usize, done: &[bool], active: &[usize]) {
    let pw = pal();
    let gap = 2.0;
    let avail = ui.available_width();
    // 12px 一格起，最多铺满一行；行数随之变化。
    let cell = 12.0;
    let cols = (((avail + gap) / (cell + gap)).floor() as usize).max(1);
    let rows = pieces.div_ceil(cols);
    let w = cols as f32 * cell + (cols as f32 - 1.0) * gap;
    let h = rows as f32 * cell + (rows as f32 - 1.0) * gap;
    // 太高的矩阵截断到 260px，避免把页面撑得滚不完 —— 但这时只剩
    // "前若干片"，所以下面那句统计仍然报全量。
    let max_h = 260.0;
    let (slot, _) = ui.allocate_exact_size(egui::vec2(w, h.min(max_h)), egui::Sense::hover());
    let clip = slot.intersect(egui::Rect::from_min_size(
        slot.min,
        egui::vec2(w, max_h),
    ));
    let mut pnt = ui.painter().clone();
    pnt.set_clip_rect(pnt.clip_rect().intersect(clip));

    let active_set: std::collections::HashSet<usize> = active.iter().copied().collect();
    for i in 0..pieces {
        let c = i % cols;
        let r = i / cols;
        let x = slot.left() + c as f32 * (cell + gap);
        let y = slot.top() + r as f32 * (cell + gap);
        if y > clip.bottom() {
            break;
        }
        let color = if active_set.contains(&i) {
            pw.warn
        } else if done.get(i).copied().unwrap_or(false) {
            pw.accent
        } else {
            pw.hover_bg
        };
        pnt.rect_filled(
            egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(cell, cell)),
            egui::CornerRadius::same(2),
            color,
        );
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

/// 把上次标记为"卸载"的插件目录真正删掉，然后清空标记。
///
/// 在 `App::new` 里调用：那时插件还没被加载进 `PluginHooks`，
/// 删目录不会踩到正在执行它的 QuickJS 线程。
///
/// 删不掉的（被占用 / 权限）保留标记，下次启动再试 —— 宁可多留一轮，
/// 也不要静默把标记丢掉、让插件变成"半卸载"状态。
fn run_pending_uninstall(gui: &Arc<Gui>) {
    let (ids, root) = {
        let cfg = gui.cfg.lock().unwrap();
        if cfg.pending_uninstall.is_empty() {
            return;
        }
        (cfg.pending_uninstall.clone(), plugins_root())
    };
    let Some(root) = root else {
        return;
    };

    let mut still: Vec<String> = Vec::new();
    for id in ids {
        // 目录名不一定等于插件 id（manifest 里 id 和文件夹名可以不一样），
        // 所以扫一遍目录、按各自的 plugin.toml 里的 id 匹配。
        let dir = find_plugin_dir(&root, &id).unwrap_or_else(|| root.join(&id));
        if !dir.exists() {
            continue;
        }
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            eprintln!("[plugin] 删除插件目录失败 {}: {e}", dir.display());
            still.push(id);
        }
    }

    let mut cfg = gui.cfg.lock().unwrap();
    cfg.pending_uninstall = still;
    let _ = cfg.save();
}

/// 在插件根目录里按 manifest 的 id 找对应文件夹。
fn find_plugin_dir(root: &std::path::Path, id: &str) -> Option<PathBuf> {
    let rd = std::fs::read_dir(root).ok()?;
    for e in rd.flatten() {
        let dir = e.path();
        if !dir.is_dir() {
            continue;
        }
        let mf = dir.join(adl_plugin::MANIFEST_FILE);
        if !mf.is_file() {
            continue;
        }
        if let Ok(m) = adl_plugin::Manifest::load(&mf) {
            if m.id == id {
                return Some(dir);
            }
        }
    }
    None
}

fn build_hooks(cfg: &Config) -> Arc<PluginHooks> {
    let mode = if cfg.mode_trusted() { Mode::Trusted } else { Mode::Safe };
    match plugins_root() {
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
        "limit_unit": cfg.limit_unit,
        "proxy": cfg.proxy,
        "mode": cfg.mode,
        "http1": cfg.http1,
        "user_agent": cfg.user_agent,
        "max_concurrent_tasks": cfg.max_concurrent_tasks,
        "check_update": cfg.check_update,
        "auto_start": cfg.auto_start,
        "close_action": cfg.close_action,
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
    // 空字符串表示"用内置默认 UA"，不是"没有 UA" —— 引擎侧
    // `user_agent_opt()` 会把空串当 None，再退回默认 Chrome 串。
    if let Some(ua) = args.get("user_agent").and_then(|x| x.as_str()) {
        cfg.user_agent = ua.trim().to_string();
    }
    if let Some(n) = args.get("max_concurrent_tasks").and_then(|x| x.as_u64()) {
        cfg.max_concurrent_tasks = (n as usize).clamp(1, 16);
    }
    if let Some(b) = args.get("check_update").and_then(|x| x.as_bool()) {
        cfg.check_update = b;
    }
    // 注意：这里只记下用户的意愿，真正的动作（写/删注册表 Run 项）由
    // 调用方 `do_save_settings` 做 —— 因为 `set_settings` 也走命令行/API，
    // 而命令行改配置不该悄悄动注册表。
    if let Some(b) = args.get("auto_start").and_then(|x| x.as_bool()) {
        cfg.auto_start = b;
    }
    if let Some(a) = args.get("close_action").and_then(|x| x.as_str()) {
        cfg.close_action = a.to_string();
    }
    if let Some(u) = args.get("limit_unit").and_then(|x| x.as_str()) {
        cfg.limit_unit = u.to_string();
    }
    cfg.save().map_err(|e| format!("保存配置失败: {e}"))?;
    let snapshot = settings_json(&cfg);
    let new_cfg = cfg.clone();
    drop(cfg);
    *gui.hooks.lock().unwrap() = build_hooks(&new_cfg);
    Ok(snapshot)
}


// ───────────────────────── 入口 ─────────────────────────

/// 画图标像素：圆角方块 + 中间一个向下的箭头。
///
/// 早先这里画的是**全不透明**的方块：alpha 恒 `0xFF`、从边到边铺满，于是
/// 在托盘里就是一块纯色方砖 —— 用户报的"托盘还没有"，一半原因就是这个
/// 形状根本看不出是个图标，另一半是下面 `build_tray` 把错误 `.ok()` 掉了，
/// 就算注册失败界面上也毫无提示。
///
/// 现在按圆角矩形的符号距离场算覆盖率：圆角外是真透明（`alpha = 0`），
/// 边缘留 1px 抗锯齿。托盘图标在 100% 缩放下实际只有 16×16，圆角给到 7
/// 大了会糊成一个点，小了又和方砖没区别 —— 所以各尺寸都按边长比例算，
/// 16、32、64 看起来才是同一个图标。
fn icon_rgba(size: u32) -> Vec<u8> {
    let n = size as f32;
    let half = n * 0.5;
    // 以下比例都从 32×32 的调参值折算而来，见函数上方注释。
    let r = n * 0.21875;
    let stem_hw = n * 0.08125;
    let stem_top = -n * 0.21875;
    let stem_bot = n * 0.03125;
    let head_top = n * 0.015625;
    let head_bot = n * 0.25;

    // 底色用主题的青绿（#2E7D6B），箭头用白 —— 深色/浅色任务栏上都看得清。
    const BG: [f32; 3] = [0x2E as f32, 0x7D as f32, 0x6B as f32];
    const FG: [f32; 3] = [0xF2 as f32, 0xF7 as f32, 0xF6 as f32];

    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            // 像素中心，坐标系原点在图标正中。
            let px = x as f32 + 0.5 - half;
            let py = y as f32 + 0.5 - half;

            // 圆角矩形 SDF：到"内缩 r 的矩形"的距离再减 r。
            let qx = (px.abs() - (half - r)).max(0.0);
            let qy = (py.abs() - (half - r)).max(0.0);
            let d_rect = (qx * qx + qy * qy).sqrt() - r;
            let cover = (0.5 - d_rect).clamp(0.0, 1.0);
            if cover <= 0.0 {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }

            // 箭头 = 竖杆 + 三角头（两者取并集，即 SDF 取 min）。
            let d_stem = (px.abs() - stem_hw).max(stem_top - py).max(py - stem_bot);
            let head_hw = ((head_bot - py) * 0.95).max(0.0);
            let d_head = (px.abs() - head_hw).max(head_top - py).max(py - head_bot);
            let d_arrow = d_stem.min(d_head);
            let arrow = (0.5 - d_arrow).clamp(0.0, 1.0);

            let mut rgb = [0u8; 3];
            for i in 0..3 {
                rgb[i] = (BG[i] + (FG[i] - BG[i]) * arrow).round() as u8;
            }
            rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], (cover * 255.0).round() as u8]);
        }
    }
    rgba
}

/// 托盘用的 32×32 图标。
fn make_icon() -> Option<Icon> {
    Icon::from_rgba(icon_rgba(32), 32, 32).ok()
}

/// 窗口图标（任务栏 / Alt+Tab 用）。
///
/// 和托盘图标同一套几何，但画大一点：`with_icon` 给的是原图，系统按需缩放，
/// 32×32 直接放大到 150% 缩放的任务栏会发虚，给 64×64 更稳。
fn make_window_icon() -> egui::IconData {
    let size = 64;
    egui::IconData {
        rgba: icon_rgba(size),
        width: size,
        height: size,
    }
}

/// 确认图标是不是真的进了任务栏。
///
/// 🔴 以前这里用 `TrayIcon::rect()` 当判据，那是**错的**：`rect()` 内部走
/// `Shell_NotifyIconGetRect`，而图标被折叠进「溢出区」（任务栏那个 `^`
/// 后面）时它会直接返 `E_FAIL`，于是注册明明成功却被判成失败 —— 接着
/// `build_tray` 把刚建好的实例丢掉（`Drop` 会 `NIM_DELETE` 把图标删掉），
/// 再建一个无 GUID 的，用户永远看不到图标。
///
/// 换成 `set_tooltip`：它走 `NIM_MODIFY`，图标没登记时必然返回
/// `os error 5 (拒绝访问)`，登记了就一定成功，跟图标摆在哪无关。
fn tray_registered(t: &TrayIcon) -> Result<(), String> {
    t.set_tooltip(Some("ADL 下载器"))
        .map_err(|e| format!("{e}"))
}

fn build_tray() -> (Option<TrayIcon>, String) {
    // 先按"带 GUID"建一次：GUID 能让 Windows 记住用户把图标摆在溢出区还是
    // 钉在任务栏上。注册不上才退回不带 GUID 的老式 `hWnd + uID` 身份 ——
    // 宁可下次启动图标归回溢出区，也不要干脆没有图标。
    //
    // 顺便把结论返回给界面：用户 m22507 报"没有托盘"，而 release 版没有
    // 控制台、stderr 全丢，界面上又什么都不说 —— 只能在日志里找。现在
    // 设置页直接显示这一行状态。
    match build_tray_inner(true) {
        Some(t) => match tray_registered(&t) {
            Ok(()) => {
                logfile::log("[tray] 托盘图标已注册（GUID 身份）");
                return (Some(t), "已注册（固定在任务栏的位置会被记住）".to_string());
            }
            Err(e) => {
                logfile::log(&format!("[tray] GUID 身份没注册上（{e}），改试老式身份"));
                // 先 drop，让它的 NIM_DELETE 把残留清掉，再建第二个。
            }
        },
        None => logfile::log("[tray] GUID 身份创建失败，改试老式身份"),
    }
    match build_tray_inner(false) {
        Some(t) => {
            match tray_registered(&t) {
                Ok(()) => {
                    // 注册成功但 rect() 仍可能问不到位置（在溢出区里），
                    // 这条只作参考，不当失败判据。
                    let where_at = match t.rect() {
                        Some(_) => "任务栏可见",
                        None => "可能在溢出区（点任务栏的 ^ 看看）",
                    };
                    logfile::log(&format!("[tray] 托盘图标已注册（hWnd + uID 身份，{where_at}）"));
                    (Some(t), format!("已注册（{where_at}）"))
                }
                Err(e) => {
                    // 真正注册失败最常见的原因是本进程跑在 Low 完整性级别
                    // （沙箱/受限令牌）：那种情况下 Shell_NotifyIconW 直接
                    // 返回 FALSE + 5。把原始错误写进日志和界面，别让用户猜。
                    logfile::log(&format!("[tray] 托盘图标没能注册到任务栏（{e}）"));
                    (
                        Some(t),
                        format!("没注册上：{e}（进程完整性级别低时会被系统拒绝）"),
                    )
                }
            }
        }
        None => (
            None,
            "创建失败，托盘不可用（详细原因见 data/startup.log）".to_string(),
        ),
    }
}

fn build_tray_inner(use_guid: bool) -> Option<TrayIcon> {
    let menu = Menu::new();
    let show = MenuItem::with_id("show", "显示主界面", true, None);
    let quit = MenuItem::with_id("quit", "退出", true, None);
    let _ = menu.append(&show);
    let _ = menu.append(&quit);
    let mut b = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("ADL 下载器");
    if use_guid {
        // 固定身份：Windows 用 GUID 记住"固定到任务栏 / 收进溢出区"。
        // 不给 GUID 时身份是「exe 路径 + 进程序号」，每次启动都是新图标，
        // 用户把它拖出溢出区之后下次启动又回去了。
        b = b.with_guid(TRAY_GUID);
    }
    if let Some(ic) = make_icon() {
        b = b.with_icon(ic);
    }
    match b.build() {
        Ok(t) => Some(t),
        Err(e) => {
            // 以前这里是 `.ok()`：注册失败静默返回 None，界面上完全看不出
            // 托盘没起来。至少留一行日志，排查时能知道是这一步挂的。
            logfile::log(&format!("[tray] 托盘图标创建失败（use_guid={use_guid}）: {e}"));
            None
        }
    }
}

fn main() -> eframe::Result {
    // 日志最先初始化：release 版没有控制台，eprintln 全丢。写一份到
    // data/startup.log，用户报"没有托盘图标"时能直接看是哪一步挂的。
    logfile::init();
    logfile::log("main 开始");
    // 完整性级别：`Shell_NotifyIconW` 只在 Medium 及以上才肯登记图标，
    // Low 完整性（沙箱、受限令牌）下恒定失败且错误码是 5。先记下来，
    // 免得以后又把"环境限制"当成"代码 bug"查半天。
    logfile::log(&format!("进程完整性级别: {}", platform::integrity_level()));

    // 单实例：已有实例则唤醒它并退出
    let _single = platform::acquire_single_instance();
    if _single.is_none() {
        logfile::log("已有实例在运行，唤醒它并退出（本次不建窗口、不建托盘）");
        platform::signal_existing_instance();
        return Ok(());
    }
    logfile::log("单实例锁已拿到");

    // 上一次自动更新留下的 `<exe>.old`：那一刻它还是正在运行的旧程序，
    // Windows 不让删；现在跑的是新 exe，可以清了。
    update::cleanup_old();

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

    // 把上次退出时的任务表读回来。未完成的（含上次还在下的、排队的、
    // 用户手动暂停的）一律转成 queued 交给队列按上限慢慢拉起 ——
    // 开机一次拉起十几条会把刚连上的网络打满（用户 m18583）。
    // 已完成/失败的照原样进列表，方便直接「打开文件所在位置」。
    let restored = restore_tasks(&gui);
    if restored > 0 {
        eprintln!("[tasks] 从 tasks.json 恢复了 {restored} 条任务");
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
            // 任务栏 / Alt+Tab 的窗口图标。exe 文件本身的图标在 PE 资源里
            // （assets/adl.rc，由 build.rs 编译进去），两者是分开的两处。
            .with_icon(make_window_icon())
            // 关掉系统标题栏，自己画（见 App::titlebar）。
            // 代价：系统不再提供边缘缩放，八向热区由 resize_borders() 铺。
            .with_decorations(false),
        ..Default::default()
    };
    let gui2 = gui.clone();    logfile::log(&format!(
        "数据目录: {}",
        adl_app::config::app_data_dir().display()
    ));
    logfile::log(&format!(
        "插件目录: {}",
        plugins_root().map(|p| p.display().to_string()).unwrap_or_else(|| "（无）".into())
    ));
    let (tray, tray_status) = build_tray();
    logfile::log(&format!("[tray] {tray_status}"));
    eframe::run_native(
        "ADL",
        opts,
        Box::new(move |cc| {
            Ok(Box::new(App::new(
                cc,
                gui2,
                wake,
                suspend,
                resume,
                tray,
                tray_status,
            )))
        }),
    )
}
