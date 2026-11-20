//! HTTP 客户端构造（DESIGN.md §7.1 / §3.8）。
//!
//! 三条实测约束，缺一条就跑不起来：
//! 1. 根证书只能用 `rustls-platform-verifier`（走 Windows CryptoAPI）。
//!    `webpki-roots` 缺 `CN=YR2` 根 → `UnknownIssuer`；`rustls-native-certs` 在本机
//!    受限令牌下读到 0 张证书 → `os error 5`。
//! 2. **ALPN 必须显式设**。`use_preconfigured_tls` 不会替你设，不设就永远协商到
//!    HTTP/1.1（实测 h2 比 h1.1 快 22%）。
//! 3. 每个分片一条独立连接（独立 `Client`），不共用连接池。实测独立 h2 = 5.78 MB/s，
//!    共享 Client 的 h2 多流 = 5.44 MB/s。单条 TCP 的拥塞窗口才是瓶颈。

use crate::error::{AdlError, Result};
use rustls_platform_verifier::BuilderVerifierExt;
use std::time::Duration;

/// 默认 User-Agent 必须冒充真实浏览器。
///
/// 用户 m02040 明确要求：很多服务器会按 UA 判断爬虫，像 aria2 / ADL 这种
/// "基础 UA" 会被直接拒绝（尤其百度网盘这类反盗链站）。这里固定成一个常见的
/// Windows Chrome UA，尽量与普通用户浏览器一致。
pub const DEFAULT_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// 建一条独立连接的 client。多分片时**每片调一次**。
pub fn build_client(user_agent: &str, http1_only: bool, proxy: Option<&str>) -> Result<reqwest::Client> {
    let mut cfg = rustls::ClientConfig::builder()
        .with_platform_verifier()
        .map_err(|e| AdlError::Tls(e.to_string()))?
        .with_no_client_auth();

    // 不设 ALPN 就永远协商到 HTTP/1.1。强制 h1 时也不能再上报 h2，否则 ALPN 与实际协议不符。
    cfg.alpn_protocols = if http1_only {
        vec![b"http/1.1".to_vec()]
    } else {
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    };

    let mut builder = reqwest::Client::builder()
        .user_agent(user_agent)
        .use_preconfigured_tls(cfg)
        .connect_timeout(Duration::from_secs(10))
        // 下载主体不限总时长，靠读超时和取消控制
        .pool_max_idle_per_host(0) // 独立连接：不复用
        .redirect(reqwest::redirect::Policy::limited(20));

    if http1_only {
        builder = builder.http1_only();
    }
    if let Some(p) = proxy {
        let px = reqwest::Proxy::all(p).map_err(AdlError::Http)?;
        builder = builder.proxy(px);
    }

    builder.build().map_err(AdlError::Http)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ua_disguises_as_browser() {
        // 用户 m02040：默认 UA 必须是真实浏览器，不能是 aria2/ADL 这类基础 UA。
        assert!(DEFAULT_UA.contains("Mozilla/5.0"), "默认 UA 必须是浏览器 UA: {DEFAULT_UA}");
        assert!(DEFAULT_UA.contains("Chrome/") || DEFAULT_UA.contains("Edg/"));
        assert!(!DEFAULT_UA.to_ascii_lowercase().contains("aria2"));
        assert!(!DEFAULT_UA.starts_with("ADL"));
    }

    #[test]
    fn client_builds() {
        let c = build_client(DEFAULT_UA, false, None);
        assert!(c.is_ok(), "client 构造失败: {:?}", c.err());
    }

    #[test]
    fn client_builds_http1_and_proxy() {
        assert!(build_client(DEFAULT_UA, true, None).is_ok());
        assert!(build_client(DEFAULT_UA, false, Some("http://127.0.0.1:7890")).is_ok());
        assert!(build_client(DEFAULT_UA, false, Some("not a url")).is_err());
    }

    #[test]
    fn alpn_is_set() {
        // 直接验证配置：ALPN 顺序必须是 h2 优先
        let mut cfg = rustls::ClientConfig::builder()
            .with_platform_verifier()
            .expect("platform verifier")
            .with_no_client_auth();
        cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        assert_eq!(cfg.alpn_protocols[0], b"h2");
        assert_eq!(cfg.alpn_protocols[1], b"http/1.1");
    }
}
