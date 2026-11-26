//! 极简启动日志。
//!
//! 为什么需要它：release 版带 `windows_subsystem = "windows"`，进程没有控制台，
//! `eprintln!` 写出去的东西**没有任何人能看到**。于是"托盘图标到底注册上没有"
//! 这种问题，用户双击启动后我们一无所知，只能靠猜。
//!
//! 这里把关键几行同时写进 `data/startup.log`（跟着 `app_data_dir()` 走，默认
//! 就在 exe 同级的 `data/` 下）。用户报问题时把那个文件发过来即可。
//!
//! 刻意做得很小：不引日志框架、不做轮转、不碰全局 logger。每次启动截断重写
//! —— 我们要的是"这一次启动发生了什么"，不是历史归档。

use std::io::Write;
use std::sync::Mutex;
use std::sync::OnceLock;

/// 日志文件句柄。`None` = 初始化失败（比如目录不可写），此后退化成只写 stderr。
static SINK: OnceLock<Mutex<Option<std::fs::File>>> = OnceLock::new();

/// 日志文件路径（供 UI 上"打开日志"按钮或排查时提示用）。
pub fn path() -> std::path::PathBuf {
    adl_app::config::app_data_dir().join("startup.log")
}

/// 在 `main()` 最开头调用一次：建目录、截断旧日志、写一行抬头。
pub fn init() {
    let file = std::fs::File::create(path()).ok();
    let sink = SINK.get_or_init(|| Mutex::new(file));
    if let Ok(mut guard) = sink.lock() {
        if let Some(f) = guard.as_mut() {
            let _ = writeln!(
                f,
                "=== ADL {} 启动 {} ===",
                env!("CARGO_PKG_VERSION"),
                timestamp()
            );
            let _ = f.flush();
        }
    }
}

/// 写一行日志：同时进 stderr（debug 下能看见）与日志文件。
pub fn log(msg: &str) {
    eprintln!("{msg}");
    let Some(sink) = SINK.get() else { return };
    let Ok(mut guard) = sink.lock() else { return };
    if let Some(f) = guard.as_mut() {
        let _ = writeln!(f, "[{}] {}", timestamp(), msg);
        let _ = f.flush();
    }
}

/// 本地时间，格式 `HH:MM:SS`。只用来给日志行排序，不做精密用途。
fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 秒数换算成本地时间要时区信息；这里只取 UTC 的时分秒，
    // 够用来对齐"哪几行是同一次启动里的"。差 8 小时不影响排查顺序。
    let secs = now % 86_400;
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

/// 把日志写成文件方便导出（当前未用，留给"发送诊断信息"按钮）。
#[allow(dead_code)]
pub fn read_all() -> String {
    std::fs::read_to_string(path()).unwrap_or_default()
}
