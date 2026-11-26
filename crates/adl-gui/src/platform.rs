//! Windows 平台胶水：
//! - 单实例（命名互斥量 `Global\ADL_SingleInstance_v1` + 命名事件唤醒）
//! - 系统休眠/唤醒通知（`PowerRegisterSuspendResumeNotification`，回调方式，无需窗口消息泵）
//! - WebView2 运行时检测（缺失时降级到默认浏览器）
//!
//! 这些 API 都是 Win32，非 Windows 平台上整个模块仍可编译（函数退化为空实现）。

use std::ffi::c_void;

/// 系统电源事件（只保留我们关心的三种）。
#[derive(Clone, Copy, Debug)]
pub enum PowerEvent {
    /// 系统即将休眠（PBT_APMSUSPEND）
    Suspend,
    /// 系统从休眠/睡眠恢复（PBT_APMRESUMESUSPEND / PBT_APMRESUMEAUTOMATIC）
    Resume,
    /// 其它事件，忽略
    Other,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ───────────────────────── 完整性级别（诊断用） ─────────────────────────

/// 报告本进程的完整性级别，形如 `S-1-16-8192 (Medium)`。
///
/// 为什么需要：`Shell_NotifyIconW` 在 **Low 完整性**（`S-1-16-4096`）的进程里
/// 恒定返回 FALSE + `GetLastError()=5`（拒绝访问）—— 托盘图标注册不上，但
/// 代码本身没有 bug。我在这台机器上用脚本启动的每个进程都被降成了 Low，
/// 于是"托盘有没有"这件事只能在用户的真实会话里验证；把 IL 写进启动日志，
/// 从此不用再猜。
#[cfg(windows)]
pub fn integrity_level() -> String {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return format!("查询失败（OpenProcessToken err={}）", std::io::Error::last_os_error());
        }

        // 先问需要多大缓冲，再分配。
        let mut needed: u32 = 0;
        GetTokenInformation(token, TokenIntegrityLevel, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            CloseHandle(token);
            return "查询失败（GetTokenInformation 没给出长度）".to_string();
        }

        let mut buf = vec![0u8; needed as usize];
        let ok = GetTokenInformation(
            token,
            TokenIntegrityLevel,
            buf.as_mut_ptr() as *mut c_void,
            needed,
            &mut needed,
        );
        CloseHandle(token);
        if ok == 0 {
            return format!("查询失败（GetTokenInformation err={}）", std::io::Error::last_os_error());
        }

        let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let sid = label.Label.Sid;
        if sid.is_null() {
            return "查询失败（SID 为空）".to_string();
        }
        let count_ptr = GetSidSubAuthorityCount(sid);
        if count_ptr.is_null() || *count_ptr == 0 {
            return "查询失败（SID 没有子授权）".to_string();
        }
        // 完整性级别的 RID 永远在最后一个子授权里。
        let rid = *GetSidSubAuthority(sid, (*count_ptr - 1) as u32);

        let name = match rid {
            0x0000 => "Untrusted",
            0x1000 => "Low",
            0x2000 => "Medium",
            0x2100 => "Medium Plus",
            0x3000 => "High",
            0x4000 => "System",
            _ => "未知",
        };
        format!("S-1-16-{rid} ({name})")
    }
}

#[cfg(not(windows))]
pub fn integrity_level() -> String {
    "非 Windows，不适用".to_string()
}

// ───────────────────────── 单实例 ─────────────────────────

#[cfg(windows)]
pub struct SingleInstance {
    _mutex: windows_sys::Win32::Foundation::HANDLE,
}

/// 尝试获取单实例锁。
/// - `Some(inst)`：本进程是第一个实例，`inst` 必须活到进程结束（持有互斥量）。
/// - `None`：已有实例在运行，调用方应唤醒旧实例后退出。
#[cfg(windows)]
pub fn acquire_single_instance() -> Option<SingleInstance> {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    unsafe {
        let name = wide("Global\\ADL_SingleInstance_v1");
        let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if h.is_null() {
            // 创建失败（受限令牌等）时放行启动，不挡用户
            return Some(SingleInstance { _mutex: h });
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return None;
        }
        Some(SingleInstance { _mutex: h })
    }
}

#[cfg(not(windows))]
pub struct SingleInstance;

#[cfg(not(windows))]
pub fn acquire_single_instance() -> Option<SingleInstance> {
    Some(SingleInstance)
}

/// 第二个实例调用：通知第一个实例把窗口显示出来。
#[cfg(windows)]
pub fn signal_existing_instance() -> bool {
    use windows_sys::Win32::System::Threading::{OpenEventW, SetEvent};
    const EVENT_MODIFY_STATE: u32 = 0x0002;
    unsafe {
        let name = wide("Global\\ADL_Wakeup_v1");
        let h = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
        if h.is_null() {
            return false;
        }
        SetEvent(h) != 0
    }
}

#[cfg(not(windows))]
pub fn signal_existing_instance() -> bool {
    false
}

/// 第一个实例调用：起一条后台线程等待唤醒事件，收到就执行 `on_wake`。
#[cfg(windows)]
pub fn spawn_wakeup_listener<F>(on_wake: F)
where
    F: Fn() + Send + 'static,
{
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    const INFINITE: u32 = 0xFFFF_FFFF;
    unsafe {
        let name = wide("Global\\ADL_Wakeup_v1");
        // 自动重置事件；已存在则直接打开
        let h = CreateEventW(std::ptr::null(), 0, 0, name.as_ptr());
        if h.is_null() {
            return;
        }
        let hval = h as usize;
        std::thread::spawn(move || loop {
            let h = hval as windows_sys::Win32::Foundation::HANDLE;
            let r = WaitForSingleObject(h, INFINITE);
            if r == 0 {
                on_wake();
            } else {
                break;
            }
        });
    }
}

#[cfg(not(windows))]
pub fn spawn_wakeup_listener<F>(_on_wake: F)
where
    F: Fn() + Send + 'static,
{
}

// ───────────────────────── 休眠 / 唤醒 ─────────────────────────

#[repr(C)]
struct SubsParams {
    callback: Option<unsafe extern "system" fn(*const c_void, u32, *const c_void) -> u32>,
    context: *const c_void,
}

pub struct PowerRegistration {
    _handle: *mut c_void,
    _params: Box<SubsParams>,
}

unsafe extern "system" fn power_callback<F>(
    context: *const c_void,
    type_: u32,
    _setting: *const c_void,
) -> u32
where
    F: Fn(PowerEvent) + Send + Sync + 'static,
{
    let f = &*(context as *const F);
    let ev = match type_ {
        0x0004 => PowerEvent::Suspend, // PBT_APMSUSPEND
        0x0007 => PowerEvent::Resume,  // PBT_APMRESUMESUSPEND
        0x0012 => PowerEvent::Resume,  // PBT_APMRESUMEAUTOMATIC
        _ => PowerEvent::Other,
    };
    f(ev);
    0
}

/// 注册系统休眠/唤醒通知。返回的 `PowerRegistration` 必须活到进程结束。
#[cfg(windows)]
pub fn register_power<F>(f: F) -> Option<PowerRegistration>
where
    F: Fn(PowerEvent) + Send + Sync + 'static,
{
    use windows_sys::Win32::System::Power::PowerRegisterSuspendResumeNotification;
    const DEVICE_NOTIFY_CALLBACK: u32 = 2;
    unsafe {
        let boxed: Box<F> = Box::new(f);
        let context = Box::into_raw(boxed) as *const c_void;
        let cb: unsafe extern "system" fn(*const c_void, u32, *const c_void) -> u32 =
            power_callback::<F>;
        let params = Box::new(SubsParams {
            callback: Some(cb),
            context,
        });
        let mut handle: *mut c_void = std::ptr::null_mut();
        let rc = PowerRegisterSuspendResumeNotification(
            DEVICE_NOTIFY_CALLBACK,
            Box::as_ref(&params) as *const SubsParams as *mut c_void,
            &mut handle,
        );
        if rc != 0 {
            return None;
        }
        Some(PowerRegistration {
            _handle: handle,
            _params: params,
        })
    }
}

#[cfg(not(windows))]
pub fn register_power<F>(_f: F) -> Option<PowerRegistration>
where
    F: Fn(PowerEvent) + Send + Sync + 'static,
{
    None
}

// ───────────────────────── WebView2 检测 / 浏览器降级 ─────────────────────────
//
// 自 v0.2 起 GUI 已换成纯 Rust 的 egui/eframe，不再依赖 WebView2，
// 因此下面两个函数在主程序里已无调用点。保留它们是因为：
//   1. 诊断用途 —— 想知道"这台机器到底有没有 WebView2"时可以直接调；
//   2. 将来若再提供"用浏览器打开 ADL 的 Web UI"这种可选模式（`web/` 目录仍在），
//      可以直接复用，不必重写一遍注册表/目录探测。
// 用 allow(dead_code) 显式标记，避免每次构建都刷两条警告。

/// 检测本机是否安装了 WebView2 运行时（Evergreen 或 Fixed Version 均可）。
/// 先看文件目录，再查注册表（HKLM 32 位视图 / HKLM 64 位视图 / HKCU）。
#[allow(dead_code)]
pub fn webview2_available() -> bool {
    let bases = [
        r"C:\Program Files (x86)\Microsoft\EdgeWebView\Application",
        r"C:\Program Files\Microsoft\EdgeWebView\Application",
    ];
    for base in bases {
        if let Ok(rd) = std::fs::read_dir(base) {
            for e in rd.flatten() {
                if e.path().join("msedgewebview2.exe").is_file() {
                    return true;
                }
            }
        }
    }
    let keys = [
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}",
        r"HKLM\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}",
        r"HKCU\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}",
    ];
    for key in keys {
        if let Ok(out) = std::process::Command::new("reg")
            .args(["query", key, "/v", "pv"])
            .output()
        {
            if out.status.success() {
                return true;
            }
        }
    }
    false
}

/// 用系统默认浏览器打开一个 URL（WebView2 缺失时的降级路径）。
#[allow(dead_code)]
pub fn open_in_browser(url: &str) {
    // 允许用环境变量禁用自动拉起浏览器（无桌面环境/测试时有用）。
    if std::env::var_os("ADL_NO_OPEN_BROWSER").is_some() {
        eprintln!("[gui] 浏览器模式：界面地址 {url}（已按 ADL_NO_OPEN_BROWSER 跳过自动打开）");
        return;
    }
    // `cmd /C start "" "<url>"`：第一个空参数是窗口标题占位，避免 URL 被当成标题。
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
}

/// 用系统文件管理器打开一个目录（插件页的"打开插件目录"）。
///
/// 目录不存在时先建出来：用户第一次点这个按钮时 `plugins/` 往往还没建，
/// 直接 `start` 一个不存在的路径会静默失败，看起来像按钮没反应。
pub fn open_path(dir: &std::path::Path) -> std::io::Result<()> {
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
    }
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &dir.display().to_string()])
            .spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(dir).spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(dir).spawn()?;
    }
    Ok(())
}

/// 弹出系统的「选择文件夹」对话框，返回用户选中的目录。
///
/// 🔴 **这个函数会阻塞到用户选完为止**（`SHBrowseForFolderW` 是模态的）。
/// 调用方必须丢到 `std::thread::spawn` 里再用 channel 收结果，
/// 直接在 egui 的 `update` 里调会让整个界面冻住（连重绘都停）。
///
/// 选的是文件夹不是文件，所以用 `SHBrowseForFolderW` 而不是
/// `GetOpenFileNameW` —— 后者选不了目录。也没引 `rfd`/`native-dialog`
/// 这类第三方依赖：系统自带的就够，还省一个编译单元。
#[cfg(windows)]
pub fn pick_folder(title: &str) -> Option<std::path::PathBuf> {
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED,
        COINIT_DISABLE_OLE1DDE,
    };
    use windows_sys::Win32::UI::Shell::{
        SHBrowseForFolderW, SHGetPathFromIDListW, BROWSEINFOW, BIF_EDITBOX, BIF_NEWDIALOGSTYLE,
        BIF_RETURNONLYFSDIRS,
    };

    unsafe {
        // 这个线程大概率是新建的、还没初始化过 COM。返回 S_FALSE 表示
        // "已经初始化过"（此时不该由我们 CoUninitialize），失败就放弃。
        let hr = CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32);
        if hr < 0 {
            return None;
        }
        let should_uninit = hr == 0;

        let title_w: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        // 对话框自己往里写选中路径的显示名，得给它一块可写缓冲。
        let mut display = [0u16; 260];
        let mut bi: BROWSEINFOW = std::mem::zeroed();
        // hwndOwner 给 null：对话框会当独立窗口弹出，位置交给系统。
        bi.hwndOwner = std::ptr::null_mut();
        bi.pidlRoot = std::ptr::null_mut();
        bi.pszDisplayName = display.as_mut_ptr();
        bi.lpszTitle = title_w.as_ptr();
        // 只要真实目录（不要虚拟文件夹）；带个输入框可以直接粘路径；
        // NEWDIALOGSTYLE 换成 Vista 之后的现代样式（可拖动、可展开）。
        bi.ulFlags = BIF_RETURNONLYFSDIRS | BIF_EDITBOX | BIF_NEWDIALOGSTYLE;
        bi.lpfn = None;
        bi.lParam = 0;

        let pidl = SHBrowseForFolderW(&bi);
        let mut out: Option<std::path::PathBuf> = None;
        if !pidl.is_null() {
            let mut buf = [0u16; 260];
            if SHGetPathFromIDListW(pidl, buf.as_mut_ptr()) != 0 {
                let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                let s = String::from_utf16_lossy(&buf[..len]);
                if !s.is_empty() {
                    out = Some(std::path::PathBuf::from(s));
                }
            }
            // PIDL 是 COM 分配的，必须用 CoTaskMemFree 还，不能 drop
            CoTaskMemFree(pidl as *const c_void);
        }

        if should_uninit {
            CoUninitialize();
        }
        out
    }
}

/// macOS 侧用 `osascript` 调系统的选择目录面板。
///
/// 同样会阻塞（等用户点完），调用方照 Windows 那套丢线程里。
#[cfg(target_os = "macos")]
pub fn pick_folder(_title: &str) -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("osascript")
        .args(["-e", "POSIX path of (choose folder)"])
        .output()
        .ok()?;
    if !out.status.success() {
        // 用户点了取消，osascript 会以非 0 退出，这不是错误。
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(s))
    }
}

/// 其它平台没有现成实现，返回 `None`，界面上退化成手填路径。
#[cfg(all(unix, not(target_os = "macos")))]
pub fn pick_folder(_title: &str) -> Option<std::path::PathBuf> {
    None
}

/// 弹系统的"打开文件"对话框，返回用户选中的文件。
///
/// 和 [`pick_folder`] 一样是**模态阻塞**的 —— 调用方必须丢到
/// `std::thread::spawn` 里再用 channel 收结果，否则整个界面冻住。
///
/// `filter` 是 `(描述, 扩展名不带点)`，比如 `("插件压缩包", "zip")`；
/// 对话框里同时挂一条"所有文件"，免得用户的包扩展名不标准就选不着。
#[cfg(windows)]
pub fn pick_file(title: &str, filter: (&str, &str)) -> Option<std::path::PathBuf> {
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_PATHMUSTEXIST, OPENFILENAMEW,
    };

    unsafe {
        let title_w: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();

        // 过滤器是 "描述\0通配\0描述\0通配\0\0" 这样双终止的宽字符串，
        // 少一个 \0 系统会一路读过界。
        let parts = [
            filter.0.to_string(),
            format!("*.{}", filter.1),
            "所有文件".to_string(),
            "*.*".to_string(),
        ];
        let mut filter_w: Vec<u16> = Vec::new();
        for part in parts.iter() {
            filter_w.extend(part.encode_utf16());
            filter_w.push(0);
        }
        filter_w.push(0);

        // 1024 个 wchar 够放路径了；不用 MAX_PATH 是因为长路径也常见。
        let mut buf = [0u16; 1024];
        let mut ofn: OPENFILENAMEW = std::mem::zeroed();
        ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
        // hwndOwner 给 null：对话框当独立窗口弹出，位置交给系统。
        ofn.hwndOwner = std::ptr::null_mut();
        ofn.lpstrFilter = filter_w.as_ptr();
        ofn.lpstrFile = buf.as_mut_ptr();
        ofn.nMaxFile = buf.len() as u32;
        ofn.lpstrTitle = title_w.as_ptr();
        ofn.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST;

        // 返回 0 = 用户取消（或出错），两种情况我们都当"没选"。
        if GetOpenFileNameW(&mut ofn) == 0 {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let s = String::from_utf16_lossy(&buf[..len]);
        if s.is_empty() {
            None
        } else {
            Some(std::path::PathBuf::from(s))
        }
    }
}

/// macOS 侧用 `osascript` 调系统的选择文件面板（同样阻塞）。
#[cfg(target_os = "macos")]
pub fn pick_file(_title: &str, _filter: (&str, &str)) -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("osascript")
        .args(["-e", "POSIX path of (choose file)"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(s))
    }
}

/// 其它平台没有现成实现，返回 `None`。
#[cfg(all(unix, not(target_os = "macos")))]
pub fn pick_file(_title: &str, _filter: (&str, &str)) -> Option<std::path::PathBuf> {
    None
}

// ───────────────────────── 开机自启动 ─────────────────────────

/// 自启动的注册表值名。固定成 `ADL`：用户关掉自启时我们才能准确删掉那一条，
/// 而不是把 `HKCU\...\Run` 里的其它软件项一起扫一遍。
pub const AUTO_START_VALUE: &str = "ADL";

/// 开机自启命令行：带引号的 exe 绝对路径 + `--auto-start`。
///
/// 路径里的空格必须用引号包住（`"C:\Program Files\ADL\adl-gui.exe"`），
/// 否则 Windows 会把 `C:\Program` 当程序、把剩下的当参数。
/// 带上 `--auto-start` 是为了让启动时能区分"用户双击"和"开机自动拉起"——
/// 开机那次不该弹主窗口，缩到托盘里安静地接着下就行。
#[cfg(windows)]
fn auto_start_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\" --auto-start", exe.display()))
}

/// 读当前是否已经登记了自启动（以注册表实际内容为准，不看配置项）。
///
/// 为什么以注册表为准：用户可能用别的工具（任务管理器「启动」页、
/// CCleaner 之类）把它关了，那些工具只删注册表、不会改我们的 config.toml。
/// 如果只信配置，设置页会显示"开着"但实际不会自启，比不知道更糟。
#[cfg(windows)]
pub fn auto_start_enabled() -> bool {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};

    let sub = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let name = wide(AUTO_START_VALUE);
    let mut size: u32 = 0;

    unsafe {
        // 第一次只问长度；返回值不是 ERROR_SUCCESS 就说明这一项不存在。
        let rc = RegGetValueW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        );
        rc == 0 && size > 2
    }
}

/// 写 / 删自启动项。`None` = 失败原因，`Some(())` = 成功。
///
/// 只动 `HKCU`，不需要管理员权限。
#[cfg(windows)]
pub fn set_auto_start(on: bool) -> Result<(), String> {
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    let sub = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let name = wide(AUTO_START_VALUE);

    unsafe {
        let mut key: HKEY = std::ptr::null_mut();
        let rc = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        );
        if rc != 0 {
            return Err(format!(
                "打不开注册表 Run 项（错误码 {rc}）"
            ));
        }

        let result = if on {
            let Some(cmd) = auto_start_command() else {
                RegCloseKey(key);
                return Err("拿不到本程序的路径，没法登记自启动".to_string());
            };
            // REG_SZ 要连结尾的 NUL 一起写，长度按字节算。
            let data = wide(&cmd);
            let bytes = data.len() * std::mem::size_of::<u16>();
            let rc = RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                bytes as u32,
            );
            if rc == 0 {
                Ok(())
            } else {
                Err(format!("写自启动项失败（错误码 {rc}）"))
            }
        } else {
            let rc = RegDeleteValueW(key, name.as_ptr());
            // 本来就没有这一项也算成功 —— 用户要的是"结果是关的"。
            if rc == 0 || rc == ERROR_FILE_NOT_FOUND {
                Ok(())
            } else {
                Err(format!("删自启动项失败（错误码 {rc}）"))
            }
        };

        RegCloseKey(key);
        result
    }
}

#[cfg(not(windows))]
pub fn auto_start_enabled() -> bool {
    false
}

#[cfg(not(windows))]
pub fn set_auto_start(_on: bool) -> Result<(), String> {
    Err("这个平台还没有实现开机自启动".to_string())
}

/// 本次是不是被开机自启动拉起的（`--auto-start`）。
///
/// 用于决定要不要"静默起"：开机那次不该把主窗口盖到用户脸上。
pub fn launched_by_auto_start() -> bool {
    std::env::args().any(|a| a == "--auto-start")
}
