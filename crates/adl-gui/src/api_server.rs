//! 本地 HTTP API —— 供浏览器扩展把下载转交给 ADL（M6）。
//!
//! 安全模型（缺一即漏洞）：
//!   1. 只绑 127.0.0.1，外部网络不可达；
//!   2. 会改变状态的接口（/add /probe /check /notify）必须带正确 token；
//!   3. 校验 Origin：空（MV3 Service Worker 直发不带 Origin）或 chrome-extension://
//!      / moz-extension:// 才放行；普通网页无法省略 Origin，因此拿不到 token、也调不动。
//!
//! 只读的 `/ping` `/bridge` 不要 token：扩展刚启动、ADL 刚被拉起时还不知道
//! token，但那时候正需要这两个接口来"问地址、问配置"。
//!
//! 注意：token 由 `/auto-token` 下发给扩展，该接口同样做 Origin 校验。

use crate::{add_task, gui_snapshot, Gui};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

pub const DEFAULT_PORT: u16 = 17800;

/// `/bridge` 的接口版本。扩展拿这个判断自己还能不能用，
/// 将来协议不兼容时加一，扩展发现版本过大就只保留"浏览器原生下载"。
pub const BRIDGE_API: u32 = 1;

fn gen_token() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut a = std::collections::hash_map::RandomState::new().build_hasher();
    a.write_u128(now);
    a.write_u32(std::process::id());
    let mut b = std::collections::hash_map::RandomState::new().build_hasher();
    b.write_u128(now ^ 0x9e37_79b9_7f4a_7c15);
    b.write_u64(0xdead_beef_cafe_babe);
    format!("{:016x}{:016x}", a.finish(), b.finish())
}

/// `/bridge` 里报给扩展的那几项配置快照。
///
/// 单独拷一份而不是每次请求都读 `gui.cfg`：扩展问 bridge 是热路径（每次
/// 下载都会问），而 `cfg` 是 Mutex，没必要为几个 bool 去抢锁。
#[derive(Clone)]
struct BridgeCfg {
    real_port: u16,
    send_cookies: bool,
    min_mb: u64,
    notify_fail: bool,
    allow_launch: bool,
}

/// 启动 API 服务，返回实际监听端口。
///
/// 除了配置端口，端口不是 17800 时还会在 17800 上再绑一个"发现端口"：
/// 浏览器扩展是硬编码往 17800 问的，用户改了端口之后它得能问出真实端口。
/// 两个端口共用同一套 handler，只有 `/bridge` 里报的 `port` 可能不同。
pub fn start(gui: Arc<Gui>, port: u16) -> std::io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let actual = listener.local_addr()?.port();
    let token = Arc::new(gen_token());
    let bridge = {
        let cfg = gui.cfg.lock().unwrap();
        BridgeCfg {
            real_port: actual,
            send_cookies: cfg.browser_send_cookies,
            min_mb: cfg.browser_min_mb,
            notify_fail: cfg.browser_notify_fail,
            allow_launch: cfg.browser_allow_launch,
        }
    };
    spawn_listener(listener, gui.clone(), token.clone(), bridge.clone());
    if actual != DEFAULT_PORT {
        match TcpListener::bind(("127.0.0.1", DEFAULT_PORT)) {
            Ok(alias) => spawn_listener(alias, gui, token, bridge),
            // 端口被别的程序占了不算致命：用户手动把扩展指到真实端口也能用。
            Err(e) => eprintln!(
                "[api] {DEFAULT_PORT} 上绑不上发现端口（{e}），扩展需要手动填端口 {actual}"
            ),
        }
    }
    Ok(actual)
}

fn spawn_listener(
    listener: TcpListener,
    gui: Arc<Gui>,
    token: Arc<String>,
    bridge: BridgeCfg,
) {
    let _ = thread::Builder::new()
        .name("adl-api".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => {
                        let g = gui.clone();
                        let t = token.clone();
                        let b = bridge.clone();
                        let _ = thread::Builder::new()
                            .name("adl-api-conn".into())
                            .spawn(move || {
                                let _ = handle_conn(s, &g, &t, &b);
                            });
                    }
                    Err(_) => continue,
                }
            }
        });
}

struct Req {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn read_req(stream: &mut TcpStream) -> std::io::Result<Req> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_sub(&buf, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            let mut lines = head.split("\r\n");
            let first = lines.next().unwrap_or("");
            let mut parts = first.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let raw_target = parts.next().unwrap_or("/").to_string();
            let (path, query) = parse_target(&raw_target);
            let mut headers = HashMap::new();
            for line in lines {
                if let Some((k, v)) = line.split_once(':') {
                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                }
            }
            let clen: usize = headers
                .get("content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let need = pos + 4 + clen;
            while buf.len() < need {
                let n = stream.read(&mut tmp)?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let body = buf[pos + 4..].to_vec();
            return Ok(Req { method, path, query, headers, body });
        }
        if buf.len() > 1 << 20 {
            break;
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "bad request",
    ))
}

fn parse_target(target: &str) -> (String, HashMap<String, String>) {
    let mut q = HashMap::new();
    let (path, qs) = match target.split_once('?') {
        Some((p, s)) => (p.to_string(), s.to_string()),
        None => (target.to_string(), String::new()),
    };
    for pair in qs.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            q.insert(k.to_string(), v.to_string());
        }
    }
    (path, q)
}

fn origin_allowed(origin: &str) -> bool {
    origin.is_empty()
        || origin.starts_with("chrome-extension://")
        || origin.starts_with("moz-extension://")
}

fn handle_conn(
    mut stream: TcpStream,
    gui: &Arc<Gui>,
    token: &str,
    bridge: &BridgeCfg,
) -> std::io::Result<()> {
    let req = match read_req(&mut stream) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    let origin = req.headers.get("origin").cloned().unwrap_or_default();
    let tok = req
        .headers
        .get("x-adl-token")
        .cloned()
        .or_else(|| req.query.get("token").cloned())
        .unwrap_or_default();

    let (status, body): (u16, String) = match (req.method.as_str(), req.path.as_str()) {
        ("OPTIONS", _) => (204, String::new()),
        ("GET", "/ping") => (200, json!({"ok": true, "pong": true}).to_string()),
        // 扩展的"问路"接口：不要 token（ADL 刚被拉起来时扩展还没拿到 token），
        // 但要校验 Origin —— 普通网页拿不到这个响应里的任何敏感信息，
        // 最多知道"这台机器上装了 ADL"。
        ("GET", "/bridge") => {
            if !origin_allowed(&origin) {
                (403, json!({"ok": false, "error": "forbidden origin"}).to_string())
            } else {
                (
                    200,
                    json!({
                        "ok": true,
                        "api": BRIDGE_API,
                        "version": env!("CARGO_PKG_VERSION"),
                        "port": bridge.real_port,
                        "send_cookies": bridge.send_cookies,
                        "min_mb": bridge.min_mb,
                        "notify_fail": bridge.notify_fail,
                        "allow_launch": bridge.allow_launch,
                    })
                    .to_string(),
                )
            }
        }
        ("GET", "/status") => (
            200,
            json!({
                "ok": true,
                "version": env!("CARGO_PKG_VERSION"),
                "tasks": gui_snapshot(gui),
            })
            .to_string(),
        ),
        ("GET", "/auto-token") => {
            if origin_allowed(&origin) {
                (200, json!({"ok": true, "token": token}).to_string())
            } else {
                (403, json!({"ok": false, "error": "forbidden origin"}).to_string())
            }
        }
        ("POST", "/add") | ("POST", "/probe") | ("POST", "/notify") | ("GET", "/check") => {
            if !origin_allowed(&origin) {
                (403, json!({"ok": false, "error": "forbidden origin"}).to_string())
            } else if tok != token {
                (401, json!({"ok": false, "error": "bad token"}).to_string())
            } else if req.path == "/check" {
                (200, json!({"ok": true}).to_string())
            } else if req.path == "/notify" {
                // 扩展接管失败（连不上、ADL 太旧、参数被拒）时让桌面端弹一条
                // 消息，而不是只在扩展的 console 里 warn —— 用户根本看不到
                // console。`cfg.browser_notify_fail` 关掉时静默丢弃。
                let v: Value = serde_json::from_slice(&req.body).unwrap_or_else(|_| json!({}));
                let msg = v.get("message").and_then(|x| x.as_str()).unwrap_or("");
                let err = v.get("error").and_then(|x| x.as_bool()).unwrap_or(true);
                if bridge.notify_fail && !msg.is_empty() {
                    crate::push_notice(
                        gui,
                        if err {
                            crate::notice::Level::Warn
                        } else {
                            crate::notice::Level::Info
                        },
                        "浏览器接管".to_string(),
                        msg.to_string(),
                    );
                }
                (200, json!({"ok": true}).to_string())
            } else if req.path == "/add" {
                let v: Value = serde_json::from_slice(&req.body).unwrap_or_else(|_| json!({}));
                match add_task(gui, &v) {
                    Ok(r) => (200, r.to_string()),
                    Err(e) => (400, json!({"ok": false, "error": e}).to_string()),
                }
            } else {
                // /probe：最小实现，回显 URL 是否可用（M6 暂不深探）
                let v: Value = serde_json::from_slice(&req.body).unwrap_or_else(|_| json!({}));
                let url = v.get("url").cloned().unwrap_or(Value::Null);
                (200, json!({"ok": true, "url": url}).to_string())
            }
        }
        _ => (404, json!({"ok": false, "error": "not found"}).to_string()),
    };

    write_resp(&mut stream, status, &body, &origin)
}

fn write_resp(
    stream: &mut TcpStream,
    status: u16,
    body: &str,
    origin: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "OK",
    };
    let acao = if origin.starts_with("chrome-extension://") || origin.starts_with("moz-extension://")
    {
        origin.to_string()
    } else {
        "*".to_string()
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: {acao}\r\n\
         Access-Control-Allow-Headers: content-type, x-adl-token\r\n\
         Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n\r\n",
        body.as_bytes().len()
    );
    stream.write_all(head.as_bytes())?;
    if status != 204 {
        stream.write_all(body.as_bytes())?;
    }
    stream.flush()?;
    Ok(())
}
