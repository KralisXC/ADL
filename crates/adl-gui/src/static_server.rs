//! 极简静态文件服务器：把前端 dist 目录挂到 127.0.0.1:0。
//!
//! 为什么不用 file://：WebView2 在 file:// 下对 ES module / fetch 有 CORS 限制，
//! 用本地 HTTP 行为和 vite dev 一致（DESIGN.md §2.5）。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub fn start(root: PathBuf) -> std::io::Result<(String, u16)> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let root = Arc::new(root);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let root = root.clone();
            std::thread::spawn(move || {
                let _ = serve(&mut stream, &root);
            });
        }
    });
    Ok((format!("http://127.0.0.1:{port}"), port))
}

fn serve(stream: &mut std::net::TcpStream, root: &Path) -> std::io::Result<()> {
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf)?;
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
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
        _ => "Not Found",
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
