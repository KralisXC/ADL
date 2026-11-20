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

/// 检测本机是否安装了 WebView2 运行时（Evergreen 或 Fixed Version 均可）。
/// 先看文件目录，再查注册表（HKLM 32 位视图 / HKLM 64 位视图 / HKCU）。
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
