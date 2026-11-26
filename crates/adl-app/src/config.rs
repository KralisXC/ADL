//! 配置文件（TOML）。
//!
//! 全部数据都在 **exe 同级的 `data/`**，只有「默认下载目录」跟着系统的
//! 「下载」文件夹走（用户 m21598）。取 exe 失败才退回旧的
//! `%LOCALAPPDATA%\ADL` / `$HOME/.local/share/adl`。
//!
//! 另外兼容两种历史位置：exe 同级的 `config.toml`（早期便携模式，存在就用它）
//! 和 `data/config.toml`。
//!
//! 命令行显式给出的参数总是覆盖配置里的同名项。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// 主配置里每个插件只写一小段（开关等），详细设置在各自的
/// `data/plugin-settings/<id>.json` 里，避免主配置膨胀。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCfg {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Default for PluginCfg {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// 应用数据目录（配置、历史、任务表、消息、插件设置、缓存、更新包都放这）。
///
/// 位置是 **exe 同级的 `data/`**。用户 m21598：「除了默认下载位置是放在
/// win 自动那边，其他的数据全都放我们自己的文件夹里面。」
///
/// 之前是 `%LOCALAPPDATA%\ADL`，问题有三个：
/// 1. 用户找不到——程序装在 D 盘/E 盘，数据却飞到 C 盘用户目录里；
/// 2. 绿色版拷走就丢配置，卸载也留一地垃圾；
/// 3. 某些环境下往用户目录写会被拦（`os error 5`），而这跟"下载器"本身
///    没关系，用户只会觉得软件坏了。
///
/// 但装在 `C:\Program Files` 这类地方时 exe 目录是只读的（非管理员写不进去），
/// 所以这里**先探一下能不能写**：能写就用 `data/`，不能写才退回旧的
/// `%LOCALAPPDATA%\ADL`。探测结果缓存住，不用每次调用都建文件。
pub fn app_data_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let own = dir.join("data");
                if dir_writable(&own) {
                    return own;
                }
                eprintln!(
                    "提示：{} 写不进去，数据改放用户目录（绿色版请放到有写权限的文件夹）",
                    own.display()
                );
            }
        }
        if let Ok(p) = std::env::var("LOCALAPPDATA") {
            if !p.trim().is_empty() {
                return PathBuf::from(p).join("ADL");
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            if !home.trim().is_empty() {
                return PathBuf::from(home).join(".local").join("share").join("adl");
            }
        }
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    })
    .clone()
}

/// 这个目录能不能写（不存在就先建）。用一个探针文件实测，
/// 而不是只看 ACL —— 有 Deny ACE、只读卷、OneDrive 占位文件这些
/// 光看权限位判断不出来的情况太多了。
fn dir_writable(dir: &std::path::Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".adl-write-probe");
    let ok = std::fs::write(&probe, b"1").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// 系统默认的「下载」文件夹。
///
/// 之所以要问系统而不是硬拼 `$HOME/Downloads`：Windows 上这个目录是可以被
/// 用户（或被 OneDrive / 某些优化工具）搬到别处的，注册表里存着真实路径；
/// 硬拼出来的 `C:\Users\x\Downloads` 可能指向一个已经不用的空目录。
///
/// 拿不到就返回 `None`，由调用方回退到 `.`（当前目录）。
pub fn system_download_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::{
            FOLDERID_Downloads, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
        };
        let mut ptr: *mut u16 = std::ptr::null_mut();
        // SAFETY: SHGetKnownFolderPath 成功时把 CoTaskMem 分配的字符串写进 ptr，
        // 失败时不动它（我们预先置空，失败直接返回）。
        let hr = unsafe {
            SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT as u32, std::ptr::null_mut(), &mut ptr)
        };
        if hr < 0 || ptr.is_null() {
            return None;
        }
        let mut len = 0usize;
        // SAFETY: 系统保证这是以 0 结尾的宽字符串。
        unsafe {
            while *ptr.add(len) != 0 {
                len += 1;
            }
        }
        // SAFETY: 上面已量出长度，只读这一段。
        let s = unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len)) };
        // SAFETY: 这个 PIDL/字符串是 COM 分配器给的，必须还给 CoTaskMemFree。
        unsafe { windows_sys::Win32::System::Com::CoTaskMemFree(ptr as *const core::ffi::c_void) };
        if s.trim().is_empty() {
            return None;
        }
        return Some(PathBuf::from(s));
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var("HOME").ok().filter(|h| !h.trim().is_empty())?;
        let home = PathBuf::from(home);
        #[cfg(target_os = "linux")]
        {
            // freedesktop 标准：~/.config/user-dirs.dirs 里 XDG_DOWNLOAD_DIR="$HOME/下载"
            if let Ok(text) = std::fs::read_to_string(home.join(".config/user-dirs.dirs")) {
                for line in text.lines() {
                    let line = line.trim();
                    if let Some(rest) = line.strip_prefix("XDG_DOWNLOAD_DIR=") {
                        let v = rest.trim().trim_matches('"');
                        if !v.is_empty() {
                            return Some(PathBuf::from(v.replace("$HOME", &home.display().to_string())));
                        }
                    }
                }
            }
        }
        Some(home.join("Downloads"))
    }
}

/// 配置文件路径。
///
/// 兼容三种位置，按优先级：
/// 1. exe 同级 `config.toml` —— 早期便携模式，**存在就用**（老用户升级不受影响）；
/// 2. `data/config.toml` —— 现在写在这里（`app_data_dir()` 能写的时候）；
/// 3. 兜底：`app_data_dir()/config.toml`（无法写 exe 目录时就是用户目录）。
pub fn config_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let legacy = dir.join("config.toml");
            if legacy.exists() {
                return legacy;
            }
        }
    }
    app_data_dir().join("config.toml")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 默认保存目录
    pub dir: String,
    /// 期望的最大并发连接数（1–64）
    pub connections: usize,
    /// 单请求分块大小（字节，0 = 按文件大小自动分配）
    pub chunk_size: u64,
    /// 全局限速（字节/秒，0 = 不限）
    pub limit: u64,
    /// User-Agent（空 = 用内置的浏览器 UA）
    pub user_agent: String,
    /// 强制 HTTP/1.1
    pub http1: bool,
    /// HTTP 代理（空 = 不用）
    pub proxy: String,
    /// 插件模式："safe"（默认）或 "trusted"
    pub mode: String,
    /// 是否记录下载历史
    pub history_enabled: bool,
    /// 点窗口关闭按钮时的行为："" = 还没问过用户（下次关闭时弹窗问），
    /// "close" = 直接退出程序，"tray" = 最小化到托盘继续在后台下载。
    pub close_action: String,
    /// 限速显示单位："auto"（按速度自动挑）或 "B/s"/"KB/s"/"MB/s"/"GB/s" 固定。
    pub limit_unit: String,
    /// 侧边栏是否收起（只留图标轨道）。
    pub sidebar_collapsed: bool,
    /// 每个插件的一小段主配置（只用 enabled 开关；详细设置在各插件自己的 json 里）
    pub plugins: BTreeMap<String, PluginCfg>,
    /// 已"卸载"、但要等下次启动才真正删目录的插件 id。
    ///
    /// 用户 m15179：「插件卸载的话可以卸载，但是正常还在跑，要等我们整个软件
    /// 重启后删除（怕用户还在使用）」—— 标记后立即从启用列表里消失、进程里的
    /// `PluginHooks` 也重建掉，但磁盘上的目录留着；下次 `App::new` 启动时
    /// 才 `remove_dir_all`，删完把标记清空。
    pub pending_uninstall: Vec<String>,
    /// 删除任务时是否连本地文件一起删。`None` = 还没问过（弹窗里问），
    /// `Some(true)` = 以后不问、直接删文件，`Some(false)` = 以后不问、只删任务。
    pub delete_files_choice: Option<bool>,
    /// 同时下载的任务数上限（1–16）。超出的进队列，等前面跑完再自动开始。
    ///
    /// 以前是"加上就立刻开跑"，一次粘 20 个链接就有 20 个任务同时抢带宽，
    /// 每个都只能分到一小口，整体反而更慢（用户 m18583："队列"）。
    pub max_concurrent_tasks: usize,
    /// 任务列表默认排序：`status`（进行中在上，默认）/ `name` / `size` / `added`。
    pub task_sort: String,
    /// 任务列表默认筛选：`all` / `downloading` / `completed` / `failed`。
    pub task_filter: String,
    /// 是否自动检查更新（GitHub Releases）。
    pub check_update: bool,
    /// 是否开机自动启动（Windows 写 `HKCU\...\Run`，值名 `ADL`）。
    ///
    /// 用户 m22520：「记得加一个是否自启动的设置」。刻意不做成安装器的
    /// 选项 —— 用户可能一直用绿色版，"装的时候勾一下"根本没机会出现。
    ///
    /// 只在 `HKCU` 下写，**不需要管理员权限**：开机自启是每个用户自己的事，
    /// 写到 HKLM 反而会让普通用户改不动、也让软件被迫提权。
    pub auto_start: bool,
    /// 插件市场索引地址（`index.json` 的完整 URL）。空 = 用内置官方源。
    ///
    /// 之所以做成可配置：官方源是 GitHub raw，国内直连经常抽风；用户
    /// 换成镜像或自己 fork 的仓库时不用改代码。
    pub market_url: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // 默认落到系统的「下载」文件夹，而不是当前工作目录。
            //
            // 用 `"."` 的话，双击 exe 启动时是 exe 所在目录、从终端启动时是
            // 终端的工作目录 —— 同一个软件在两处下到不同地方，用户会觉得
            // "文件丢了"。问系统要真实路径最稳。
            dir: system_download_dir()
                .map(|p| p.display().to_string().replace('\\', "/"))
                .unwrap_or_else(|| ".".to_string()),
            connections: 16,
            chunk_size: 0,
            limit: 0,
            user_agent: String::new(),
            http1: false,
            proxy: String::new(),
            mode: "safe".to_string(),
            history_enabled: true,
            close_action: String::new(),
            limit_unit: "auto".to_string(),
            sidebar_collapsed: false,
            plugins: BTreeMap::new(),
            pending_uninstall: Vec::new(),
            delete_files_choice: None,
            max_concurrent_tasks: 3,
            task_sort: "status".to_string(),
            task_filter: "all".to_string(),
            check_update: true,
            auto_start: false,
            market_url: String::new(),
        }
    }
}

impl Config {
    /// 读取配置。文件不存在 → 默认值；解析失败 → 打警告 + 默认值（不让一个写坏的配置挡住下载）。
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                // 兼容记事本 / PowerShell 写出的 UTF-8 BOM。
                let text = text.strip_prefix('\u{feff}').unwrap_or(&text).to_string();
                match toml::from_str::<Config>(&text) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!(
                            "警告：配置文件 {} 解析失败（{}），已改用默认配置",
                            path.display(),
                            e
                        );
                        Config::default()
                    }
                }
            }
            Err(_) => Config::default(),
        }
    }

    pub fn mode_trusted(&self) -> bool {
        self.mode.eq_ignore_ascii_case("trusted")
    }

    /// 关闭按钮行为是否已经问过用户。
    pub fn close_action_known(&self) -> bool {
        matches!(self.close_action.as_str(), "close" | "tray")
    }

    /// 关闭时是否最小化到托盘（未选择时按"问一次"处理，由 UI 负责弹窗）。
    pub fn close_to_tray(&self) -> bool {
        self.close_action == "tray"
    }

    /// 插件是否启用（主配置里没有该条目时默认启用）。
    pub fn plugin_enabled(&self, id: &str) -> bool {
        self.plugins.get(id).map(|p| p.enabled).unwrap_or(true)
    }

    /// 插件是否启用，缺省值由清单给出。
    ///
    /// `plugin_enabled` 的 `unwrap_or(true)` 意味着"没写过就是开"，
    /// 这对用户自己装的插件是对的；但示例插件（和本体功能重叠的那些，
    /// 比如 notify-on-complete）应该在首次加载时是关的，否则一个文件
    /// 会收到两条"下载完成"。所以缺省值要从 manifest 的
    /// `enabled_by_default` 取，不能写死 true。
    pub fn plugin_enabled_or(&self, id: &str, default: bool) -> bool {
        self.plugins
            .get(id)
            .map(|p| p.enabled)
            .unwrap_or(default)
    }

    pub fn dir_path(&self) -> PathBuf {
        // 空串 = 用系统的「下载」文件夹。模板里默认就是空的，这样用户
        // 不用改配置也能落在系统认为对的地方；取不到再退回 "."。
        if self.dir.trim().is_empty() {
            return system_download_dir().unwrap_or_else(|| PathBuf::from("."));
        }
        PathBuf::from(&self.dir)
    }

    pub fn proxy_opt(&self) -> Option<String> {
        if self.proxy.trim().is_empty() {
            None
        } else {
            Some(self.proxy.clone())
        }
    }

    pub fn user_agent_opt(&self) -> Option<String> {
        if self.user_agent.trim().is_empty() {
            None
        } else {
            Some(self.user_agent.clone())
        }
    }

    /// 把当前配置写回配置文件（覆盖），用于 GUI 的设置页。
    ///
    /// 失败时把**具体路径**带进错误里：以前只回一个 `os error 5`，
    /// 用户和我们都看不出它到底想写哪（用户 m21507 报过这个）。
    ///
    /// `rename` 在少数环境（目录有 Deny ACE 的"删除子项"权限、只读卷、
    /// 杀软占用句柄）会失败，但直接覆盖写往往还是允许的 —— 所以改名失败
    /// 时退回原地写，别让用户看到一个光秃秃的 `拒绝访问`。
    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                std::io::Error::new(e.kind(), format!("建目录 {} 失败: {e}", parent.display()))
            })?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, &text).map_err(|e| {
            std::io::Error::new(e.kind(), format!("写 {} 失败: {e}", tmp.display()))
        })?;
        match std::fs::rename(&tmp, &path) {
            Ok(()) => Ok(()),
            Err(rename_err) => {
                // 退化路径：直接覆盖写目标文件。
                match std::fs::write(&path, &text) {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&tmp);
                        Ok(())
                    }
                    Err(write_err) => Err(std::io::Error::new(
                        write_err.kind(),
                        format!(
                            "保存到 {} 失败: {write_err}（改名也失败: {rename_err}）",
                            path.display()
                        ),
                    )),
                }
            }
        }
    }

    /// 把默认配置（带注释的模板）写到默认位置；已存在则不动。
    /// 返回实际路径。`force=true` 时覆盖。
    pub fn write_template(force: bool) -> std::io::Result<PathBuf> {
        let path = config_path();
        if path.exists() && !force {
            return Ok(path);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, TEMPLATE)?;
        Ok(path)
    }
}

const TEMPLATE: &str = r#"# ADL 配置文件（TOML）
# 命令行参数会覆盖这里同名的设置。

# 默认保存目录（相对路径相对当前工作目录）。
# 新配置默认填系统的「下载」文件夹（Windows 的 FOLDERID_Downloads /
# macOS 的 ~/Downloads / Linux 的 XDG_DOWNLOAD_DIR），这里留空等于用它。
# 路径里的反斜杠要写成双反斜杠或改用正斜杠，例如 "E:\\Downloads" 或 "E:/Downloads"。
dir = ""

# 期望的最大并发连接数（1–64）。引擎会按服务器实际接受的量自适应收敛。
connections = 16

# 单请求分块大小（字节）。0 = 按文件大小自动分配。也支持写 "4M" 这种？
# 注意：TOML 里这里只接受纯数字字节数，例如 4M 请写 4194304。
chunk_size = 0

# 全局限速，字节/秒，0 = 不限。
limit = 0

# User-Agent。留空 = 使用内置的浏览器 UA（冒充浏览器，避免被当爬虫）。
user_agent = ""

# 强制 HTTP/1.1（少数站点 h2 不稳时用）。
http1 = false

# HTTP 代理，例如 "http://127.0.0.1:7890"。留空 = 不用。
proxy = ""

# 插件模式："safe"（默认，只能观察/拦截）或 "trusted"（允许改写请求）。
mode = "safe"

# 是否记录下载历史（history.json）。
history_enabled = true

# 点窗口右上角关闭按钮时的行为：
#   ""      = 还没选，下次关闭时弹窗问一次
#   "close" = 直接退出程序
#   "tray"  = 最小化到托盘，下载继续在后台跑
close_action = ""

# 限速显示单位："auto" 自动挑，或固定成 "B/s" / "KB/s" / "MB/s" / "GB/s"
limit_unit = "auto"

# 侧边栏是否收起（只留一条窄轨道）
sidebar_collapsed = false

# 同时下载的任务数上限（1–16）。多出来的任务排队，等前面跑完自动开始。
max_concurrent_tasks = 3

# 任务列表默认排序："status"（进行中在最上面）/ "name" / "size" / "added"
task_sort = "status"

# 任务列表默认筛选："all" / "downloading" / "completed" / "failed"
task_filter = "all"

# 启动时自动检查 GitHub 新版本
check_update = true

# 开机自动启动（写 HKCU 下的 Run 项，不需要管理员权限）
auto_start = false

# 插件市场索引地址（index.json 的完整 URL）。留空 = 用内置官方源：
#   https://raw.githubusercontent.com/KralisXC/adl-extensions/main/index.json
# 官方源走 GitHub raw，连不上时可以换成镜像或自己 fork 的仓库。
market_url = ""
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_sane() {
        let c = Config::default();
        assert_eq!(c.connections, 16);
        assert_eq!(c.chunk_size, 0);
        assert!(!c.mode_trusted());
        assert!(c.history_enabled);
        assert!(c.proxy_opt().is_none());
        assert!(c.user_agent_opt().is_none());
        assert!(!c.close_action_known());
        assert!(!c.close_to_tray());
        assert_eq!(c.limit_unit, "auto");
        assert!(!c.sidebar_collapsed);
        assert_eq!(c.max_concurrent_tasks, 3);
        assert_eq!(c.task_sort, "status");
        assert!(c.check_update);
        assert!(!c.auto_start);
        assert!(c.market_url.is_empty());
    }

    #[test]
    fn parses_partial_toml() {
        let c: Config = toml::from_str("connections = 4\nmode = \"trusted\"").unwrap();
        assert_eq!(c.connections, 4);
        assert!(c.mode_trusted());
        // 其它字段走 default。`dir` 不再固定是 "."，而是系统的「下载」文件夹
        // （拿不到才回退 "."），所以这里只能断言非空。
        assert!(!c.dir.is_empty());
        assert_eq!(c.chunk_size, 0);
    }

    #[test]
    fn system_download_dir_is_absolute_when_present() {
        // 拿不到允许返回 None；拿到了就必须是绝对路径，否则引擎会在
        // 进程工作目录下建目录，等于又回到 "." 的老问题。
        if let Some(p) = system_download_dir() {
            assert!(p.is_absolute(), "系统下载目录应当是绝对路径: {}", p.display());
        }
    }

    #[test]
    fn template_roundtrips() {
        let c: Config = toml::from_str(TEMPLATE).unwrap();
        assert_eq!(c.connections, 16);
        assert_eq!(c.mode, "safe");
        assert!(c.history_enabled);
    }
}
