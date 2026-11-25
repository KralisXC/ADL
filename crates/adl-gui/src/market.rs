//! 插件市场：从 `index.json` 拉插件清单，按条目下载 zip、校验 sha256、交给
//! `install::install_zip` 解开。
//!
//! 索引与包都托管在 `https://github.com/KralisXC/adl-extensions`
//! （`index.json` 在 main 分支，包在 Releases）。用户 m18583 的原话是
//! 「插件市场仍是空壳，我们不是已经差不多弄好了嘛」—— 装的这条链路
//! （下载 → 校验 → 解包 → 删包）在 m16664 里已经打通了，缺的只有
//! 「清单从哪来」和「点了怎么下」。这个文件补的就是这两块。
//!
//! 三条硬约束：
//! 1. **必须校验 sha256**。插件能 `exec`，索引是走明文 HTTP 拿的，不校验
//!    等于把"随便谁改一下 index.json 就能让用户装木马"写进设计里。
//!    哈希对不上就整包丢掉，绝不解包。
//! 2. **`min_api` 要挡**。宿主 API 每加一个能力就 +1，老宿主跑新插件会在
//!    调用不存在的方法时炸在 QuickJS 里，报错还看不懂。宁可在这儿拒绝。
//! 3. 网络动作全部 async，由调用方丢到 tokio 上跑；UI 只收结果，不阻塞。

use std::path::PathBuf;

use serde::Deserialize;

/// 官方索引地址。GitHub raw 会带 `Cache-Control`，但 reqwest 这边不缓存，
/// 每次点刷新都是真请求。
pub const DEFAULT_INDEX_URL: &str =
    "https://raw.githubusercontent.com/KralisXC/adl-extensions/main/index.json";

/// 宿主支持的插件 API 版本（与 `adl_plugin::manifest` 里的 `API_VERSION` 对齐）。
pub const HOST_API: u32 = adl_plugin::manifest::API_VERSION;

#[derive(Deserialize)]
struct RawIndex {
    #[serde(default)]
    api_version: u32,
    #[serde(default)]
    plugins: Vec<RawEntry>,
}

#[derive(Deserialize)]
struct RawEntry {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    zip: String,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    size_mb: f64,
    #[serde(default = "default_api")]
    min_api: u32,
    #[serde(default)]
    needs_trusted: bool,
}

fn default_api() -> u32 {
    1
}

/// 一条市场条目（已校验过必填字段，UI 直接拿去画）。
#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub zip: String,
    pub sha256: String,
    pub size_mb: f64,
    pub min_api: u32,
    /// 需要信任模式才能用（声明了 exec / 改写请求这类权限）。
    pub needs_trusted: bool,
}

/// 解析索引文本。
///
/// 逐条校验：缺 `id` / `zip` / `sha256` 的条目直接跳过 —— 宁可市场里少一行，
/// 也别让用户点一个注定装不上的东西。`min_api` 高于宿主的也跳过，但会
/// 在返回的错误列表里说清楚是哪个插件要求太新。
pub fn parse_index(text: &str) -> Result<(Vec<Entry>, Vec<String>), String> {
    let raw: RawIndex = serde_json::from_str(text).map_err(|e| format!("索引不是合法 JSON: {e}"))?;
    if raw.api_version > HOST_API {
        return Err(format!(
            "索引要求的 API 版本 {} 比本程序支持的 {} 新，请先更新 ADL",
            raw.api_version, HOST_API
        ));
    }
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    for e in raw.plugins {
        if e.id.is_empty() || e.zip.is_empty() || e.sha256.is_empty() {
            skipped.push(format!(
                "{}：缺少 id / zip / sha256",
                if e.id.is_empty() { "(未命名)" } else { &e.id }
            ));
            continue;
        }
        if e.min_api > HOST_API {
            skipped.push(format!(
                "{}：需要 API {}（本程序是 {}）",
                e.id, e.min_api, HOST_API
            ));
            continue;
        }
        out.push(Entry {
            id: e.id,
            name: if e.name.is_empty() {
                String::new()
            } else {
                e.name
            },
            version: e.version,
            description: e.description,
            zip: e.zip,
            sha256: e.sha256.to_ascii_lowercase(),
            size_mb: e.size_mb,
            min_api: e.min_api,
            needs_trusted: e.needs_trusted,
        });
    }
    Ok((out, skipped))
}

/// 拉索引。返回 `(条目, 被跳过的条目说明)`。
pub async fn fetch_index(client: &reqwest::Client, url: &str) -> Result<(Vec<Entry>, Vec<String>), String> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("连不上插件源: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("插件源返回 HTTP {}", resp.status().as_u16()));
    }
    let text = resp.text().await.map_err(|e| format!("读取索引失败: {e}"))?;
    parse_index(&text)
}

/// 下载 → 校验 → 解包。成功返回插件的 `id`。
///
/// 中间那个 zip 落在 `app_data/cache/<id>-<version>.zip`，`install_zip` 装完
/// 自己会把它删掉；校验失败则由这里负责清理，免得盘上留一堆下坏了的包。
pub async fn download_and_install(
    client: &reqwest::Client,
    entry: &Entry,
    root: &std::path::Path,
) -> Result<String, String> {
    let cache = cache_dir();
    std::fs::create_dir_all(&cache).map_err(|e| format!("建缓存目录失败: {e}"))?;
    let safe_id: String = entry
        .id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let zip_path = cache.join(format!("{safe_id}-{}.zip", entry.version));

    let resp = client
        .get(&entry.zip)
        .send()
        .await
        .map_err(|e| format!("下载插件包失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("插件包返回 HTTP {}", resp.status().as_u16()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("读取插件包失败: {e}"))?;
    std::fs::write(&zip_path, &bytes).map_err(|e| format!("写入临时文件失败: {e}"))?;

    // 校验在解包之前。哈希不对说明包被换过（或者下坏了），两种都不能装。
    let got = match adl_core::hash_file(&zip_path) {
        Ok(h) => h,
        Err(e) => {
            let _ = std::fs::remove_file(&zip_path);
            return Err(format!("校验哈希失败: {e}"));
        }
    };
    if got != entry.sha256 {
        let _ = std::fs::remove_file(&zip_path);
        return Err(format!(
            "插件包校验失败（期望 {}…，实际 {}…），已丢弃",
            &entry.sha256.chars().take(12).collect::<String>(),
            &got.chars().take(12).collect::<String>()
        ));
    }

    match crate::install::install_zip(&zip_path, root) {
        Ok(inst) => Ok(inst.id),
        Err(e) => {
            let _ = std::fs::remove_file(&zip_path);
            Err(e)
        }
    }
}

/// 下载用的缓存目录（`%LOCALAPPDATA%/ADL/cache`）。
///
/// 不能用 `std::env::temp_dir()`：本机受限令牌下 Rust 进程对 `%TEMP%` 是
/// 拒绝访问的（`os error 5`），这一点在单测里已经踩过。
fn cache_dir() -> PathBuf {
    adl_app::config::app_data_dir().join("cache")
}

// ───────────────────────── 与界面的接线 ─────────────────────────
//
// 网络全部丢在 tokio 上跑，UI 只从 channel 取结果。为什么连"拉索引"也要
// 丢线程：reqwest 的第一次 TLS 握手在本机能到几百毫秒，同步写在 `fn ui`
// 里就是肉眼可见的卡顿。

use std::sync::mpsc::Receiver;
use std::sync::Arc;

use crate::Gui;

/// 按当前配置造一个给市场用的 client（跟下载走同一套 UA/代理/HTTP1 选择）。
fn market_client(gui: &Arc<Gui>) -> Result<reqwest::Client, String> {
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

/// 后台拉索引，结果从返回的 channel 里取。
pub fn start_fetch(gui: &Arc<Gui>, url: String) -> Receiver<Result<(Vec<Entry>, Vec<String>), String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    // 两个 Arc：一个用来拿 runtime（`spawn` 会借它），一个 move 进任务里读配置。
    let rt_gui = Arc::clone(gui);
    let cfg_gui = Arc::clone(gui);
    rt_gui.rt.spawn(async move {
        let out = match market_client(&cfg_gui) {
            Ok(client) => fetch_index(&client, &url).await,
            Err(e) => Err(e),
        };
        let _ = tx.send(out);
    });
    rx
}

/// 后台装某个市场条目（下载 + 校验 + 解包），成功时回插件 id。
pub fn start_install(
    gui: &Arc<Gui>,
    entry: Entry,
    root: PathBuf,
) -> Receiver<Result<String, String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let rt_gui = Arc::clone(gui);
    let cfg_gui = Arc::clone(gui);
    rt_gui.rt.spawn(async move {
        let out = match market_client(&cfg_gui) {
            Ok(client) => download_and_install(&client, &entry, &root).await,
            Err(e) => Err(e),
        };
        let _ = tx.send(out);
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_index_and_skips_incomplete() {
        let text = r#"{
            "api_version": 1,
            "plugins": [
                {"id":"a.b","name":"甲","version":"1.0.0","description":"d",
                 "zip":"https://x/a.zip","sha256":"ABC","size_mb":1.5},
                {"id":"","name":"坏","version":"1.0.0","zip":"u","sha256":"h"},
                {"id":"c.d","version":"2.0.0","zip":"https://x/c.zip","sha256":"def",
                 "min_api": 99}
            ]
        }"#;
        let (entries, skipped) = parse_index(text).expect("parse");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "a.b");
        // 哈希统一按小写比较，索引里写大写也能对上。
        assert_eq!(entries[0].sha256, "abc");
        assert_eq!(skipped.len(), 2);
        assert!(skipped[1].contains("99"));
    }

    #[test]
    fn rejects_newer_index() {
        let err = parse_index(r#"{"api_version":9,"plugins":[]}"#).unwrap_err();
        assert!(err.contains("比本程序支持的"), "{err}");
    }
}
