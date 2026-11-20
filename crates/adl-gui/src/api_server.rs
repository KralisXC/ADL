//! 本地 HTTP API —— 供浏览器扩展把下载转交给 ADL（M6）。
//!
//! 安全模型（缺一即漏洞）：
//!   1. 只绑 127.0.0.1，外部网络不可达；
//!   2. 会改变状态的接口（/add /probe /check）必须带正确 token；
//!   3. 校验 Origin：空（MV3 Service Worker 直发不带 Origin）或 chrome-extension://
//!      / moz-extension:// 才放行；普通网页无法省略 Origin，因此拿不到 token、也调不动。
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

/// 启动 API 服务，返回实际监听端口。
pub fn start(gui: Arc<Gui>, port: u16) -> std::io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let actual = listener.local_addr()?.port();
    let token = Arc::new(gen_token());
    thread::Builder::new()
        .name("adl-api".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => {
                        let g = gui.clone();
                        let t = token.clone();
                        let _ = thread::Builder::new()
                            .name("adl-api-conn".into())
                            .spawn(move || {
                                let _ = handle_conn(s, &g, &t);
                            });
                    }
                    Err(_) => continue,
                }
            }
        })?;
    Ok(actual)
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

fn handle_conn(mut stream: TcpStream, gui: &Arc<Gui>, token: &str) -> std::io::Result<()> {
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
        ("POST", "/add") | ("POST", "/probe") | ("GET", "/check") => {
            if !origin_allowed(&origin) {
                (403, json!({"ok": false, "error": "forbidden origin"}).to_string())
            } else if tok != token {
                (401, json!({"ok": false, "error": "bad token"}).to_string())
            } else if req.path == "/check" {
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
