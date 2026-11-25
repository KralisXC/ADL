//! 软件自动更新：走 GitHub Releases。
//!
//! 用户 m18583：「软件自动更新的话就接入我们的github自动更新吧」。
//! 仓库是 `https://github.com/KralisXC/ADL`，所以清单直接用
//! GitHub 的 `releases/latest` 接口，不需要自己维护一份版本文件。
//!
//! 三条设计取舍：
//! 1. **只认 asset 里的 `.exe`**。没有 CI 的时候一个 release 可能只挂了
//!    源码包，这时候自动换二进制是不可能成功的 —— 退化成"打开下载页"，
//!    比下载一个 zip 然后不知道怎么装要好。
//! 2. **换文件用"先改名再写回"**。Windows 不允许删除正在运行的 exe，
//!    但**允许改名**（`MoveFileEx` 语义）：把当前 exe 改成 `.old`，把新的
//!    拷到原路径，再拉起新进程、自己退出。万一拷贝失败就把 `.old` 改回来，
//!    不能把用户搞成"双击没反应"。
//! 3. 检查更新只读 release 的元数据，不读正文以外的任何东西；正式下载和
//!    替换动作全部由用户点按钮触发，不做静默更新。

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// 最新 release 的元数据接口。
pub const DEFAULT_RELEASES_API: &str = "https://github.com/KralisXC/ADL/releases/latest";

/// 当前版本（编进二进制的 crate 版本）。
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Deserialize)]
struct RawRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<RawAsset>,
}

#[derive(Deserialize)]
struct RawAsset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// 一个已解析的 release。
#[derive(Clone, Debug)]
pub struct ReleaseInfo {
    /// 去掉前缀 `v` 的版本号，例如 `0.2.0`。
    pub version: String,
    pub title: String,
    pub notes: String,
    /// 发布页地址（没有可直接替换的 exe 时用它兜底）。
    pub page: String,
    /// `(文件名, 下载地址, 字节数)`。只有 Windows 的 `.exe` 才会被识别成可用。
    pub exe: Option<(String, String, u64)>,
}

/// 解析 GitHub 的 release JSON。
///
/// 注意 GitHub 的 `releases/latest` 走的是 302 跳转，reqwest 默认跟随，
/// 这里拿到的已经是最终 JSON。
pub fn parse_release(text: &str) -> Result<ReleaseInfo, String> {
    let raw: RawRelease =
        serde_json::from_str(text).map_err(|e| format!("发布信息不是合法 JSON: {e}"))?;
    let version = raw.tag_name.trim_start_matches('v').trim().to_string();
    if version.is_empty() {
        return Err("发布信息里没有 tag_name".to_string());
    }
    // 挑 Windows 可执行文件。不用 `ends_with(".exe")` 之外的条件：
    // 我们的打包约定就是"一个 exe"，不搞安装器。
    let exe = raw
        .assets
        .iter()
        .find(|a| a.name.to_ascii_lowercase().ends_with(".exe"))
        .map(|a| (a.name.clone(), a.browser_download_url.clone(), a.size));
    Ok(ReleaseInfo {
        version,
        title: if raw.name.is_empty() {
            raw.tag_name.clone()
        } else {
            raw.name.clone()
        },
        notes: raw.body,
        page: raw.html_url,
        exe,
    })
}

/// 比较两个版本号，`latest` 是否比 `current` 新。
///
/// 只按点分段做数值比较，忽略非数字后缀（`0.2.0-beta.1` 的 `beta` 段按 0 处理）。
/// 这不是完整的 semver —— 我们自己发版只用 `x.y.z`，够用且不会误判。
pub fn is_newer(current: &str, latest: &str) -> bool {
    fn parts(s: &str) -> Vec<u64> {
        // 先砍掉预发布/构建元数据：`0.2.0-beta.1` 只取 `0.2.0`。
        // 不砍的话 `beta.1` 的 `1` 会被当成第四段版本号，把预发布版
        // 判成比正式版还新。
        let core = s
            .trim()
            .trim_start_matches('v')
            .split(['-', '+'])
            .next()
            .unwrap_or("");
        core.split('.')
            .map(|p| {
                p.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse::<u64>()
                    .unwrap_or(0)
            })
            .collect()
    }
    let (a, b) = (parts(current), parts(latest));
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return y > x;
        }
    }
    false
}

/// 拉最新 release。`client` 由调用方按设置的 UA/代理建好。
pub async fn fetch_latest(client: &reqwest::Client, url: &str) -> Result<ReleaseInfo, String> {
    let resp = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("连不上 GitHub: {e}"))?;
    if resp.status().as_u16() == 404 {
        return Err("这个仓库还没有发布任何 release".to_string());
    }
    if !resp.status().is_success() {
        return Err(format!("GitHub 返回 HTTP {}", resp.status().as_u16()));
    }
    let text = resp.text().await.map_err(|e| format!("读取发布信息失败: {e}"))?;
    parse_release(&text)
}

/// 更新文件放哪儿：`app_data/update/`。
fn update_dir() -> PathBuf {
    adl_app::config::app_data_dir().join("update")
}

/// 下载新版 exe 到更新目录，返回落盘路径。
pub async fn download_exe(
    client: &reqwest::Client,
    url: &str,
    expect_size: u64,
) -> Result<PathBuf, String> {
    let dir = update_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("建更新目录失败: {e}"))?;
    let dest = dir.join("adl-gui.new.exe");

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("下载新版本失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("下载地址返回 HTTP {}", resp.status().as_u16()));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("读取安装包失败: {e}"))?;
    // 大小对不上通常意味着下到一半断了，或者被 HTTP 中间层塞了错误页。
    if expect_size > 0 && bytes.len() as u64 != expect_size {
        return Err(format!(
            "安装包大小不符（期望 {} 字节，实际 {} 字节），已丢弃",
            expect_size,
            bytes.len()
        ));
    }
    std::fs::write(&dest, &bytes).map_err(|e| format!("写入新版本失败: {e}"))?;
    Ok(dest)
}

/// 把 `new_exe` 换成当前正在运行的可执行文件，然后由调用方退出程序。
///
/// 顺序是"先改名、再拷、失败就改回来"。Windows 上不能删正在运行的 exe，
/// 但能改名，所以这条路是可行的；Linux/macOS 上改名同样没问题。
///
/// 返回新 exe 的路径（调用方用它拉起新进程）。调用方**必须**在成功后尽快
/// `exit`，否则新旧两个进程会抢同一个单实例锁。
pub fn apply_update(new_exe: &Path) -> Result<PathBuf, String> {
    let cur = std::env::current_exe().map_err(|e| format!("拿不到当前程序路径: {e}"))?;
    let mut old = cur.clone();
    let name = cur
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "adl-gui.exe".to_string());
    old.set_file_name(format!("{name}.old"));

    // 上一次更新留下的 .old（那时它还占着旧 exe，现在旧 exe 已经没人用了）
    let _ = std::fs::remove_file(&old);

    std::fs::rename(&cur, &old).map_err(|e| format!("替换失败（无法重命名旧程序）: {e}"))?;
    if let Err(e) = std::fs::copy(new_exe, &cur) {
        // 回滚：把旧程序放回原处，否则用户下次双击就找不到 exe 了。
        let _ = std::fs::rename(&old, &cur);
        return Err(format!("替换失败（写入新程序出错，已回滚）: {e}"));
    }
    // 新的写好了，旧的（还锁着）留到下次启动再删 —— 见 `cleanup_old`。
    Ok(cur)
}

/// 启动时清掉上一次更新留下的 `.old`。
///
/// 为什么不干脆在 `apply_update` 里删：那一刻它还是"正在运行的旧程序"，
/// Windows 会拒绝；只有**下一次**启动（那时跑的是新 exe）才删得掉。
pub fn cleanup_old() {
    let Ok(cur) = std::env::current_exe() else {
        return;
    };
    let name = cur
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut old = cur.clone();
    old.set_file_name(format!("{name}.old"));
    if old.exists() {
        let _ = std::fs::remove_file(&old);
    }
}

// ───────────────────────── 与界面的接线 ─────────────────────────
//
// 和 `market.rs` 同一套路：网络请求全部丢到 tokio 上，UI 只从 channel 取
// 结果。检查更新是用户点一次才发生一次，不做后台轮询 —— 一个下载器没必要
// 常驻一个每小时的 GitHub 请求。

use std::sync::mpsc::Receiver;
use std::sync::Arc;

use crate::Gui;

/// 按当前设置造 client（UA 为空时用内置 Chrome 串，跟下载保持一致）。
fn update_client(gui: &Arc<Gui>) -> Result<reqwest::Client, String> {
    let cfg = gui.cfg.lock().unwrap().clone();
    let ua = if cfg.user_agent.trim().is_empty() {
        adl_core::http::DEFAULT_UA.to_string()
    } else {
        cfg.user_agent.clone()
    };
    let proxy = if cfg.proxy.trim().is_empty() {
        None
    } else {
        Some(cfg.proxy.as_str())
    };
    adl_core::http::build_client(&ua, cfg.http1, proxy).map_err(|e| format!("建 HTTP 客户端失败: {e}"))
}

/// 后台查最新版本。地址为空时用 `DEFAULT_RELEASES_API`。
pub fn start_check(gui: &Arc<Gui>, api: String) -> Receiver<Result<ReleaseInfo, String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let rt_gui = Arc::clone(gui);
    let cfg_gui = Arc::clone(gui);
    rt_gui.rt.spawn(async move {
        let url = if api.trim().is_empty() {
            DEFAULT_RELEASES_API.to_string()
        } else {
            api
        };
        let out = match update_client(&cfg_gui) {
            Ok(client) => fetch_latest(&client, &url).await,
            Err(e) => Err(e),
        };
        let _ = tx.send(out);
    });
    rx
}

/// 后台下载新版 exe。
pub fn start_download(
    gui: &Arc<Gui>,
    url: String,
    size: u64,
) -> Receiver<Result<PathBuf, String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let rt_gui = Arc::clone(gui);
    let cfg_gui = Arc::clone(gui);
    rt_gui.rt.spawn(async move {
        let out = match update_client(&cfg_gui) {
            Ok(client) => download_exe(&client, &url, size).await,
            Err(e) => Err(e),
        };
        let _ = tx.send(out);
    });
    rx
}

/// 把已下好的新 exe 换上去，然后拉起新进程。
///
/// 这里**不**调用 `std::process::exit`：调用方（`App::ui`）还要跳过一次
/// 自己的 `closing` 闸门、存好配置，由它来退出更安全。这个函数只负责
/// "文件换好 + 新进程起来"。
pub fn swap_and_spawn(new_exe: &Path) -> Result<PathBuf, String> {
    let target = apply_update(new_exe)?;
    std::process::Command::new(&target)
        .spawn()
        .map_err(|e| format!("新版本已就位，但拉起失败（请手动重启）: {e}"))?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.1.0", "0.1.1"));
        assert!(is_newer("0.1.0", "1.0.0"));
        assert!(is_newer("v0.1.0", "0.2"));
        assert!(!is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        // 预发布后缀按 0 处理，不会被判成新版本。
        assert!(!is_newer("0.2.0", "0.2.0-beta.1"));
    }

    #[test]
    fn parses_release_with_exe_asset() {
        // 用 r####"…"#### 而不是 r#"…"#：正文里有 `"###` 这种序列，
        // 它本身就包含 `"##`，会把 r##" 提前终止掉。
        let text = r####"{
            "tag_name": "v0.2.0",
            "name": "ADL 0.2.0",
            "body": "### 新增\n- 任务队列",
            "html_url": "https://github.com/KralisXC/ADL/releases/tag/v0.2.0",
            "assets": [
                {"name": "source.zip", "browser_download_url": "https://x/s.zip", "size": 10},
                {"name": "adl-gui.exe", "browser_download_url": "https://x/a.exe", "size": 1024}
            ]
        }"####;
        let r = parse_release(text).expect("parse");
        assert_eq!(r.version, "0.2.0");
        assert_eq!(r.title, "ADL 0.2.0");
        let (name, url, size) = r.exe.expect("应该识别到 exe");
        assert_eq!(name, "adl-gui.exe");
        assert_eq!(url, "https://x/a.exe");
        assert_eq!(size, 1024);
    }

    #[test]
    fn release_without_exe_has_no_install_target() {
        let text = r#"{"tag_name":"v0.3.0","assets":[{"name":"src.tgz","browser_download_url":"u","size":1}]}"#;
        let r = parse_release(text).expect("parse");
        assert!(r.exe.is_none());
    }
}
