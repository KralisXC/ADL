//! Range 探测（DESIGN.md §3.2）。
//!
//! | 响应 | 含义 | 处理 |
//! |---|---|---|
//! | `206` + `Content-Range: bytes 0-0/N` | 支持 Range，总长 N | 分片下载 |
//! | `200` | 不支持 Range | 单流顺序写 |
//! | `416` + `Content-Range: bytes */N` | 空文件或越界 | 取 N，允许**一次**重规划 |
//!
//! 已有完成位图时**不探测** —— 控制文件里的总长就是真相，直接发续传 Range。

use crate::engine::map_reqwest;
use crate::error::{AdlError, Result};
use std::time::Duration;

/// 探测结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// 支持 Range，总长已知
    Ranged { total: u64 },
    /// 不支持 Range，只能单流顺序写。`total` 可能未知（chunked）
    SingleConnection { total: Option<u64> },
    /// 416：从 `Content-Range: bytes */N` 取到总长
    Unsatisfiable { total: u64 },
}

impl Probe {
    pub fn total(&self) -> Option<u64> {
        match self {
            Probe::Ranged { total } | Probe::Unsatisfiable { total } => Some(*total),
            Probe::SingleConnection { total } => *total,
        }
    }

    /// 能不能并发分片。
    pub fn supports_ranges(&self) -> bool {
        matches!(self, Probe::Ranged { .. })
    }
}

/// 探测的元信息，续传时要一起存进控制文件。
#[derive(Debug, Clone, Default)]
pub struct ProbeMeta {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    pub content_disposition: Option<String>,
    pub final_url: String,
    /// 服务器通过 `Digest: sha-256=<base64>` 声明的 SHA256（十六进制）。
    /// 有就下载后核对，没有就不强求（用户 m02040："带就校验，不带不验"）。
    pub digest_sha256: Option<String>,
    /// 服务器通过 `Content-MD5`（或 `Digest: md5=`）声明的 MD5（十六进制）。
    /// 一些盗版游戏/资源站用 MD5 校验，用户 m02182 要求支持。
    pub content_md5: Option<String>,
}

/// 探测阶段的重试上限。探测是"门面"，遇到 429/连不上/5xx 抖动要重试，
/// 否则 16 连接里一次探测被 429 就整单失败（用户 m01800 明确要求）。
const PROBE_MAX_RETRIES: u32 = 5;

/// 确定性的小抖动，避免多任务同时重试形成脉冲。
fn jitter_ms() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);
    let x = C.fetch_add(1, Ordering::Relaxed);
    (x.wrapping_mul(2_654_435_761) >> 8) % 250
}

/// 用 `Range: bytes=0-0` 探测（带重试）。
///
/// 只对"临时性"错误重试：连接失败 / 超时 / 429 / 503 / 504 / 连接重置。
/// 404、401、网页误判、Content-Range 不符等**永久**错误直接返回。
pub async fn probe(client: &reqwest::Client, url: &str) -> Result<(Probe, ProbeMeta)> {
    let mut attempt: u32 = 0;
    loop {
        match probe_once(client, url).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let capacity = e.is_rate_limited() || e.is_connect_level();
                let retryable = e.is_retryable();
                if !(capacity || retryable) || attempt >= PROBE_MAX_RETRIES {
                    return Err(e);
                }
                attempt += 1;
                // 容量类（429/连不上）退避更久，普通抖动短一点。
                let base = if capacity { 800u64 } else { 400u64 };
                let ms = (base << attempt.min(4)).min(5_000) + jitter_ms();
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
        }
    }
}

async fn probe_once(client: &reqwest::Client, url: &str) -> Result<(Probe, ProbeMeta)> {
    let resp = client
        .get(url)
        .header("Range", "bytes=0-0")
        // 探测时不能要压缩：gzip 与 Range 互斥，服务器对 206 回 Content-Encoding: gzip
        // 会让多连接和续传全废且静默（DESIGN.md §7.2）
        .header("Accept-Encoding", "identity")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(map_reqwest)?;

    let status = resp.status().as_u16();
    let mut meta = ProbeMeta {
        etag: header(&resp, "etag"),
        last_modified: header(&resp, "last-modified"),
        content_type: header(&resp, "content-type"),
        content_disposition: header(&resp, "content-disposition"),
        final_url: resp.url().to_string(),
        digest_sha256: parse_digest_sha256(header(&resp, "digest").as_deref())
            .or_else(|| parse_digest_sha256(header(&resp, "content-digest").as_deref())),
        content_md5: parse_content_md5(header(&resp, "content-md5").as_deref())
            .or_else(|| parse_digest_md5(header(&resp, "digest").as_deref()))
            .or_else(|| parse_digest_md5(header(&resp, "content-digest").as_deref())),
    };

    let content_range = header(&resp, "content-range");

    let probe = match status {
        206 => {
            // Content-Range: bytes 0-0/12345
            let total = parse_content_range_total(content_range.as_deref())
                .or_else(|| header(&resp, "content-length").and_then(|v| v.parse().ok()))
                .ok_or_else(|| {
                    AdlError::ContentRangeMismatch(format!(
                        "206 响应缺少可解析的总长，Content-Range={content_range:?}"
                    ))
                })?;
            Probe::Ranged { total }
        }
        200 => {
            // 不支持 Range。Content-Length 可能缺失（chunked）
            let total = header(&resp, "content-length").and_then(|v| v.parse::<u64>().ok());
            Probe::SingleConnection { total }
        }
        416 => {
            // Content-Range: bytes */12345
            let total = parse_content_range_total(content_range.as_deref()).ok_or_else(|| {
                AdlError::ContentRangeMismatch(format!(
                    "416 响应缺少 bytes */N，Content-Range={content_range:?}"
                ))
            })?;
            Probe::Unsatisfiable { total }
        }
        s => return Err(AdlError::from_status(s)),
    };

    // 网页误下载拦截：只在成功响应判（403/429 的错误页留给限流降级）
    if let Some(ct) = &meta.content_type {
        if is_html_body(ct) && !allows_html_download(&meta) {
            return Err(AdlError::HtmlInsteadOfFile(ct.clone()));
        }
    }

    // 让探测响应自己读干净，否则连接无法复用
    let _ = resp.bytes().await;
    meta.final_url = meta.final_url.clone();
    Ok((probe, meta))
}

/// `bytes 0-0/12345` 或 `bytes */12345` → `Some(12345)`
pub fn parse_content_range_total(v: Option<&str>) -> Option<u64> {
    let v = v?;
    let after_slash = v.rsplit('/').next()?;
    let t = after_slash.trim();
    if t == "*" {
        return None;
    }
    t.parse().ok()
}

/// `bytes 100-199/1000` → `Some((100, 199))`
pub fn parse_content_range(v: &str) -> Option<(u64, Option<u64>)> {
    let rest = v.trim().strip_prefix("bytes")?.trim();
    let (range_part, _total) = rest.split_once('/')?;
    let range_part = range_part.trim();
    if range_part == "*" {
        return None;
    }
    let (s, e) = range_part.split_once('-')?;
    let start: u64 = s.trim().parse().ok()?;
    let end: Option<u64> = if e.trim().is_empty() {
        None
    } else {
        Some(e.trim().parse().ok()?)
    };
    Some((start, end))
}

fn header(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

pub fn is_html_body(content_type: &str) -> bool {
    content_type.to_ascii_lowercase().contains("text/html")
}

/// 从 `Digest` / `Content-Digest` 头里取 sha-256 的值并转成十六进制字符串。
///
/// 支持 RFC 3230 `sha-256=<base64>` 与 RFC 9530 `sha-256=:<base64>:`
/// 两种写法，多个算法用逗号分隔。取不到或解码失败都返回 None（不校验）。
pub fn parse_digest_sha256(header: Option<&str>) -> Option<String> {
    use base64::Engine as _;
    let h = header?;
    for part in h.split(',') {
        let part = part.trim();
        let (alg, val) = part.split_once('=')?;
        if !alg.trim().eq_ignore_ascii_case("sha-256") {
            continue;
        }
        // RFC 9530 的形式是 sha-256=:BASE64:
        let val = val.trim().trim_matches(':');
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(val)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(val))
            .ok()?;
        if bytes.len() != 32 {
            return None;
        }
        return Some(hex::encode(bytes));
    }
    None
}

fn decode_b64(s: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(s))
        .ok()
}

/// `Content-MD5: <base64(16 bytes)>` → 十六进制。
pub fn parse_content_md5(header: Option<&str>) -> Option<String> {
    let bytes = decode_b64(header?.trim())?;
    if bytes.len() != 16 {
        return None;
    }
    Some(hex::encode(bytes))
}

/// `Digest: md5=<base64>` / `Digest: md5=:<base64>:` → 十六进制。
pub fn parse_digest_md5(header: Option<&str>) -> Option<String> {
    let h = header?;
    for part in h.split(',') {
        let part = part.trim();
        let Some((alg, val)) = part.split_once('=') else {
            continue;
        };
        if !alg.trim().eq_ignore_ascii_case("md5") {
            continue;
        }
        let bytes = decode_b64(val.trim().trim_matches(':'))?;
        if bytes.len() != 16 {
            return None;
        }
        return Some(hex::encode(bytes));
    }
    None
}

/// 目标本身要 HTML 文件时才放行。
pub fn allows_html_download(meta: &ProbeMeta) -> bool {
    let from_cd = meta
        .content_disposition
        .as_deref()
        .and_then(crate::path::filename_from_content_disposition);
    let name = from_cd.or_else(|| {
        url::Url::parse(&meta.final_url)
            .ok()
            .and_then(|u| crate::path::name_from_url_path(&u))
    });
    match name {
        Some(n) => {
            let ext = n.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
            matches!(ext.as_str(), "html" | "htm" | "mht" | "mhtml")
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_sha256_parses() {
        // RFC 3230: Digest: sha-256=<base64>
        assert_eq!(
            parse_digest_sha256(Some(
                "sha-256=47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="
            )),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string())
        );
        // RFC 9530 冒号形式
        assert_eq!(
            parse_digest_sha256(Some(
                "sha-256=:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=:"
            )),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string())
        );
        assert_eq!(parse_digest_sha256(Some("md5=abc")), None);
        assert_eq!(parse_digest_sha256(None), None);
    }

    #[test]
    fn content_md5_parses() {
        // base64 of md5("abc") = kAFQmDzST7DWlj99KOF/cg==
        assert_eq!(
            parse_content_md5(Some("kAFQmDzST7DWlj99KOF/cg==")),
            Some("900150983cd24fb0d6963f7d28e17f72".to_string())
        );
        assert_eq!(parse_content_md5(Some("not-base64!!")), None);
        assert_eq!(parse_content_md5(None), None);
        // Digest: md5=<base64>
        assert_eq!(
            parse_digest_md5(Some("sha-256=xxxx, md5=kAFQmDzST7DWlj99KOF/cg==")),
            Some("900150983cd24fb0d6963f7d28e17f72".to_string())
        );
    }

    #[test]
    fn parse_total_from_content_range() {
        assert_eq!(parse_content_range_total(Some("bytes 0-0/3303444480")), Some(3303444480));
        assert_eq!(parse_content_range_total(Some("bytes */8388608")), Some(8388608));
        assert_eq!(parse_content_range_total(Some("bytes 0-0/*")), None);
        assert_eq!(parse_content_range_total(None), None);
        assert_eq!(parse_content_range_total(Some("garbage")), None);
    }

    #[test]
    fn parse_range_bounds() {
        assert_eq!(parse_content_range("bytes 100-199/1000"), Some((100, Some(199))));
        assert_eq!(parse_content_range("bytes 100-/1000"), Some((100, None)));
        assert_eq!(parse_content_range("bytes */1000"), None);
        assert_eq!(parse_content_range("nonsense"), None);
    }

    #[test]
    fn html_detection() {
        assert!(is_html_body("text/html; charset=utf-8"));
        assert!(is_html_body("TEXT/HTML"));
        assert!(!is_html_body("application/octet-stream"));
    }

    #[test]
    fn html_allowed_when_target_is_html() {
        let meta = ProbeMeta {
            final_url: "https://x/page.html".into(),
            ..Default::default()
        };
        assert!(allows_html_download(&meta));

        let meta = ProbeMeta {
            final_url: "https://x/file.zip".into(),
            ..Default::default()
        };
        assert!(!allows_html_download(&meta));
    }

    #[test]
    fn html_allowed_via_content_disposition() {
        let meta = ProbeMeta {
            final_url: "https://x/download?id=123".into(),
            content_disposition: Some(r#"attachment; filename="doc.htm""#.into()),
            ..Default::default()
        };
        assert!(allows_html_download(&meta));
    }

    #[test]
    fn probe_helpers() {
        let p = Probe::Ranged { total: 100 };
        assert_eq!(p.total(), Some(100));
        assert!(p.supports_ranges());

        let p = Probe::SingleConnection { total: None };
        assert_eq!(p.total(), None);
        assert!(!p.supports_ranges());

        let p = Probe::Unsatisfiable { total: 50 };
        assert_eq!(p.total(), Some(50));
        assert!(!p.supports_ranges());
    }
}
