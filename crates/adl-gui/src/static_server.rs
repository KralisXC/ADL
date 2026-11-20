//! 极简静态文件服务器：把前端 dist 目录挂到 127.0.0.1:0。
//!
//! 为什么不用 file://：WebView2 在 file:// 下对 ES module / fetch 有 CORS 限制，
//! 用本地 HTTP 行为和 vite dev 一致（DESIGN.md §2.5）。
//!
//! 额外承担一个职责：`POST /ipc`。当 WebView2 缺失、UI 跑在普通浏览器里时，
//! 前端没有 `window.ipc.postMessage`，就用 fetch 打这个端点，实现与原生窗口
//! 完全相同的 IPC（同一套 `handle_ipc` 后端）。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;

type IpcHandler = Arc<dyn Fn(&str) -> String + Send + Sync>;

pub fn start(root: PathBuf, ipc: Option<IpcHandler>) -> std::io::Result<(String, u16)> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let root = Arc::new(root);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let root = root.clone();
            let ipc = ipc.clone();
            std::thread::spawn(move || {
                let _ = serve(&mut stream, &root, ipc.as_deref());
            });
        }
    });
    Ok((format!("http://127.0.0.1:{port}"), port))
}

/// 读完整请求（头 + 按 Content-Length 读 body），返回 (方法, 路径, body)。
fn read_request(stream: &mut std::net::TcpStream) -> std::io::Result<(String, String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    // 先读到头部结束
    let header_end = loop {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_bytes(&buf, b"\r\n\r\n") {
            break Some(pos + 4);
        }
        if buf.len() > 64 * 1024 {
            break None;
        }
    };
    let Some(header_end) = header_end else {
        return Ok((String::new(), String::new(), Vec::new()));
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut method = String::new();
    let mut path = String::new();
    if let Some(line) = head.lines().next() {
        let mut it = line.split_whitespace();
        method = it.next().unwrap_or("").to_string();
        path = it.next().unwrap_or("/").to_string();
    }
    // Content-Length
    let mut content_len = 0usize;
    for line in head.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                content_len = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_len {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_len);
    Ok((method, path, body))
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn serve(
    stream: &mut std::net::TcpStream,
    root: &Path,
    ipc: Option<&(dyn Fn(&str) -> String + Send + Sync)>,
) -> std::io::Result<()> {
    let (method, path, body) = read_request(stream)?;

    // ── IPC 端点（浏览器降级模式）──
    if path.split('?').next() == Some("/ipc") && method.eq_ignore_ascii_case("POST") {
        if let Some(handler) = ipc {
            let req = String::from_utf8_lossy(&body).to_string();
            let resp = handler(&req);
            return respond(stream, 200, "application/json; charset=utf-8", resp.as_bytes());
        }
        return respond(stream, 404, "text/plain", b"ipc disabled");
    }
    if method.eq_ignore_ascii_case("POST") {
        return respond(stream, 405, "text/plain", b"method not allowed");
    }

    let mut rel = path.split('?').next().unwrap_or("/").trim_start_matches('/').to_string();
    if rel.is_empty() {
        rel = "index.html".to_string();
    }
    // 安全：拒绝 .. 穿越
    if rel.split('/').any(|c| c == "..") {
        return respond(stream, 400, "text/plain", b"bad path");
    }
    let mut file = root.join(&rel);
    if !file.is_file() {
        // SPA 回退：没有扩展名的路径一律回 index.html
        if Path::new(&rel).extension().is_none() {
            file = root.join("index.html");
        }
    }
    match std::fs::read(&file) {
        Ok(bytes) => {
            let ct = content_type(&file);
            respond(stream, 200, ct, &bytes)
        }
        Err(_) => respond(stream, 404, "text/plain", b"not found"),
    }
}

fn respond(
    stream: &mut std::net::TcpStream,
    status: u16,
    ct: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|s| s.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}
