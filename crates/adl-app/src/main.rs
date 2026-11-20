//! ADL 命令行入口（M4：配置 + 历史 + 子命令）。
//!
//! `adl <URL>`            下载
//! `adl list`             列出历史
//! `adl resume <id>`      继续某条历史（断点续传）
//! `adl remove <id>`      删除历史记录（可加 --files 删文件）
//! `adl clean`            清空已完成历史
//! `adl config [--init]`  查看/生成配置文件

mod config;
mod history;

use adl_core::hooks::Hooks;
use adl_core::{download, AdlError, DownloadOptions, NoopHooks, Progress};
use adl_plugin::{Mode, PluginHooks};
use clap::{Args, Parser, Subcommand};
use history::TaskRecord;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

#[derive(Parser, Debug)]
#[command(
    name = "adl",
    version,
    about = "ADL — 小型 HTTPS 下载器（并发分片 + 断点续传 + 插件）",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    #[command(flatten)]
    dl: DownloadArgs,
}

#[derive(Args, Debug, Default)]
struct DownloadArgs {
    /// 下载地址
    url: Option<String>,

    /// 保存目录（默认取配置）
    #[arg(short = 'd', long)]
    dir: Option<PathBuf>,

    /// 指定文件名（默认按 Content-Disposition / URL 推断）
    #[arg(short = 'o', long)]
    out: Option<String>,

    /// User-Agent（默认取配置或内置浏览器 UA）
    #[arg(long)]
    user_agent: Option<String>,

    /// 额外请求头，可重复：-H "Referer: https://..."
    #[arg(short = 'H', long = "header")]
    headers: Vec<String>,

    /// 不续传，从头下
    #[arg(long)]
    restart: bool,

    /// 期望的最大并发连接数（1–64，默认取配置里的 16）。引擎按服务器实际接受的量自适应。
    #[arg(short = 'x', long)]
    connections: Option<usize>,

    /// 单请求分块大小（支持 K/M/G 后缀）。不填则由引擎按文件大小自动分配
    #[arg(long, value_parser = parse_size)]
    chunk_size: Option<u64>,

    /// 全局限速（支持 K/M/G 后缀，0 = 不限），例如 5M
    #[arg(long, value_parser = parse_size)]
    limit: Option<u64>,

    /// 强制使用 HTTP/1.1（少数站点 h2 不稳，容易断流）
    #[arg(long)]
    http1: bool,

    /// HTTP 代理，例如 http://127.0.0.1:7890
    #[arg(long)]
    proxy: Option<String>,

    /// 插件目录（默认用配置或 exe 同级 plugins/；不存在则不加载插件）
    #[arg(long)]
    plugin_dir: Option<PathBuf>,

    /// 安全模式：插件只能 Observe/Guard，不能改写请求（默认）
    #[arg(long)]
    safe: bool,

    /// 信任模式：允许插件 Transform（改 URL / 请求头 / 分片方案）
    #[arg(long)]
    trusted: bool,

    /// 只打印结果，不打印进度
    #[arg(short = 'q', long)]
    quiet: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 列出下载历史
    List {
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },
    /// 继续一个历史任务（断点续传）
    Resume { id: u64 },
    /// 删除一条历史记录（--files 同时删除已下载文件与 .adl 进度文件）
    Remove {
        id: u64,
        #[arg(long)]
        files: bool,
    },
    /// 清空所有"已完成"的历史记录
    Clean,
    /// 查看配置文件路径与内容（--init 生成默认模板）
    Config {
        #[arg(long)]
        init: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.cmd {
        Some(Cmd::List { limit }) => cmd_list(limit),
        Some(Cmd::Resume { id }) => cmd_resume(id),
        Some(Cmd::Remove { id, files }) => cmd_remove(id, files),
        Some(Cmd::Clean) => cmd_clean(),
        Some(Cmd::Config { init }) => cmd_config(init),
        None => run_download(cli.dl),
    };
    std::process::exit(code);
}

// ─────────────────────────── 下载 ───────────────────────────

fn run_download(args: DownloadArgs) -> i32 {
    let cfg = config::Config::load();

    let Some(url) = args.url.clone() else {
        eprintln!("缺少下载地址。用法：adl <URL>");
        eprintln!("或运行 `adl --help` 查看子命令（list / resume / remove / clean / config）。");
        return 2;
    };

    if args.safe && args.trusted {
        eprintln!("参数冲突：--safe 与 --trusted 只能选一个");
        return 2;
    }

    // 解析 -H "K: V"
    let mut headers = Vec::new();
    for h in &args.headers {
        match h.split_once(':') {
            Some((k, v)) => headers.push((k.trim().to_string(), v.trim().to_string())),
            None => eprintln!("警告：请求头 {h:?} 格式不对，应为 \"Key: Value\"，已忽略"),
        }
    }

    let mut opts = DownloadOptions::default();
    opts.url = url.clone();
    opts.dir = args.dir.clone().unwrap_or_else(|| cfg.dir_path());
    opts.out_name = args.out.clone();
    opts.restart = args.restart;
    opts.headers = headers;

    let conns_in = args.connections.unwrap_or(cfg.connections);
    opts.concurrency = adl_core::clamp_concurrency(conns_in);
    if opts.concurrency != conns_in {
        eprintln!("提示：并发数 {} 超出范围 [1, 64]，已设为 {}", conns_in, opts.concurrency);
    }
    opts.chunk_size = args.chunk_size.unwrap_or(cfg.chunk_size);
    opts.speed_limit = args.limit.unwrap_or(cfg.limit);
    opts.http1_only = args.http1 || cfg.http1;

    opts.proxy = args.proxy.clone().or_else(|| cfg.proxy_opt());
    if let Some(ua) = args.user_agent.clone().or_else(|| cfg.user_agent_opt()) {
        opts.user_agent = ua;
    }
    if let Some((_, v)) = opts
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("referer"))
    {
        opts.referer = Some(v.clone());
    }

    // 插件模式：命令行优先，其次配置。
    let mode = if args.trusted {
        Mode::Trusted
    } else if args.safe {
        Mode::Safe
    } else if cfg.mode_trusted() {
        Mode::Trusted
    } else {
        Mode::Safe
    };
    let plugin_root = args
        .plugin_dir
        .clone()
        .or_else(|| cfg.plugin_dir_opt())
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("plugins")))
        });
    let hooks: Arc<dyn Hooks> = match plugin_root {
        Some(dir) if dir.is_dir() => {
            let ph = PluginHooks::load(&dir, mode);
            if ph.is_empty() {
                eprintln!("[plugin] {} 下没有可用插件", dir.display());
                Arc::new(NoopHooks)
            } else {
                eprintln!(
                    "[plugin] 已加载 {} 个插件（模式：{}）",
                    ph.len(),
                    if mode.trusted() { "trusted" } else { "safe" }
                );
                Arc::new(ph)
            }
        }
        _ => Arc::new(NoopHooks),
    };

    let quiet = args.quiet;
    let conns = opts.concurrency;
    let src_url = opts.url.clone();
    let src_url_hist = src_url.clone();
    let started_unix = history::now_unix();
    let dest_dir = opts.dir.clone();
    let out_name = opts.out_name.clone();
    let history_on = cfg.history_enabled;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("无法创建 tokio runtime");

    let t0 = Instant::now();
    let code = rt.block_on(async move {
        let progress = if quiet {
            None
        } else {
            Some(Arc::new(|p: Progress| render_progress(&p)) as adl_core::ProgressFn)
        };

        match download(opts, hooks, progress).await {
            Ok(r) => {
                if !quiet {
                    eprintln!();
                }
                println!("完成: {}", r.path.display());
                println!("大小: {} 字节", r.total);
                println!(
                    "耗时: {:.2}s（{}/s，{} 连接）",
                    r.elapsed.as_secs_f64(),
                    human_speed(r.speed_bps()),
                    conns
                );
                if r.resumed {
                    println!("续传: 是（已存在 {} 字节）", r.resumed_from);
                }
                println!("SHA256: {}", r.sha256);
                if r.server_sha256.is_some() {
                    println!("校验: 服务器提供了 SHA256，已核对一致");
                }
                if r.server_md5.is_some() {
                    println!("校验: 服务器提供了 MD5，已核对一致");
                }
                if r.total < 4096 {
                    eprintln!(
                        "⚠ 疑似无用文件：仅 {} 字节，可能是错误页/占位文件，请确认是否真的需要（已按你的要求保存）。",
                        r.total
                    );
                }
                (0, Some((r.path.to_string_lossy().to_string(), r.total, Some(r.sha256.clone()))))
            }
            Err(e) => {
                if !quiet {
                    eprintln!();
                }
                eprintln!("失败: {e}");
                eprintln!("错误码（aria2 兼容）: {}", e.aria2_code());
                eprintln!("来源: {src_url}");
                if matches!(e, AdlError::DiskFull) {
                    eprintln!("提示: 磁盘空间不足");
                }
                if matches!(e, AdlError::Stalled) {
                    eprintln!("提示: 服务器可能触发了反爬/限速（或断网）。进度已保存，稍后或换网络后重跑同一命令即可续传。");
                }
                if matches!(e, AdlError::ChecksumMismatch { .. }) {
                    eprintln!("提示: 下载内容与服务器声明的校验值不符，已删除这份损坏的文件与进度，请重试。");
                } else {
                    eprintln!("提示: 进度已保存在下载目录的 .adl 文件中，可用 `adl resume <id>` 或重跑相同命令续传。");
                }
                (exit_code_for(&e), None)
            }
        }
    });

    let (exit_code, success) = code;
    if history_on {
        let mut h = history::History::load();
        let status = if exit_code == 0 {
            "completed"
        } else {
            "failed"
        };
        let (path, total, sha) = match success {
            Some((p, t, s)) => (p, t, s),
            None => {
                // 失败时尽力猜一个路径（目录 + 指定的文件名）
                let p = match &out_name {
                    Some(n) => dest_dir.join(n).to_string_lossy().to_string(),
                    None => String::new(),
                };
                (p, 0, None)
            }
        };
        // 失败时也记下错误码/文案（从最近一次错误里拿不到，就留 None；exit_code 已能看出类别）
        let id = h.add(TaskRecord {
            id: 0,
            url: src_url_hist.clone(),
            path,
            total,
            status: status.to_string(),
            error_code: if exit_code == 0 { None } else { Some(exit_code) },
            error: None,
            started_unix,
            elapsed_ms: t0.elapsed().as_millis() as u64,
            sha256: sha,
        });
        if !quiet {
            eprintln!("（已记录到历史 #{id}）");
        }
    }

    exit_code
}

// ─────────────────────────── 子命令 ───────────────────────────

fn cmd_list(limit: usize) -> i32 {
    let h = history::History::load();
    if h.tasks.is_empty() {
        println!("（暂无下载历史）");
        return 0;
    }
    println!(
        "{:<4} {:<6} {:>10} {:>9} {:<14} {}",
        "ID", "状态", "大小", "耗时", "时间", "文件"
    );
    for t in h.recent(limit) {
        let status = match t.status.as_str() {
            "completed" => "完成",
            "failed" => "失败",
            "cancelled" => "取消",
            other => other,
        };
        println!(
            "{:<4} {:<6} {:>10} {:>9} {:<14} {}",
            t.id,
            status,
            human_size(t.total),
            human_duration(t.elapsed_ms),
            history::format_ago(t.started_unix),
            if t.path.is_empty() { &t.url } else { &t.path }
        );
    }
    0
}

fn cmd_resume(id: u64) -> i32 {
    let h = history::History::load();
    let Some(rec) = h.find(id).cloned() else {
        eprintln!("没有找到历史 #{id}。用 `adl list` 查看。");
        return 1;
    };
    if rec.status == "completed" && !rec.path.is_empty() && std::path::Path::new(&rec.path).exists() {
        println!("历史 #{id} 已经完成，文件在：{}", rec.path);
        println!("如需重新下载，直接运行 `adl \"{}\" --restart`。", rec.url);
        return 0;
    }
    let path = PathBuf::from(&rec.path);
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf());
    let out = path
        .file_name()
        .map(|f| f.to_string_lossy().to_string());
    eprintln!("#{id} 继续下载：{}", rec.url);
    run_download(DownloadArgs {
        url: Some(rec.url.clone()),
        dir,
        out,
        ..Default::default()
    })
}

fn cmd_remove(id: u64, files: bool) -> i32 {
    let mut h = history::History::load();
    let Some(rec) = h.find(id).cloned() else {
        eprintln!("没有找到历史 #{id}。用 `adl list` 查看。");
        return 1;
    };
    if files && !rec.path.is_empty() {
        let p = PathBuf::from(&rec.path);
        match std::fs::remove_file(&p) {
            Ok(_) => println!("已删除文件: {}", p.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("删除文件失败（{}）：{}", p.display(), e),
        }
        let mut ctl_os = p.clone().into_os_string();
        ctl_os.push(".adl");
        let _ = std::fs::remove_file(PathBuf::from(ctl_os));
    }
    h.remove(id);
    println!("已删除历史 #{id}");
    0
}

fn cmd_clean() -> i32 {
    let mut h = history::History::load();
    let n = h.clean_completed();
    println!("已清空 {n} 条已完成的历史记录（失败/取消的保留）。");
    0
}

fn cmd_config(init: bool) -> i32 {
    if init {
        match config::Config::write_template(false) {
            Ok(p) => println!("配置文件: {}（已写入默认模板或已存在）", p.display()),
            Err(e) => {
                eprintln!("写入配置失败: {e}");
                return 1;
            }
        }
        return 0;
    }
    let p = config::config_path();
    println!("配置文件: {}", p.display());
    match std::fs::read_to_string(&p) {
        Ok(text) => {
            println!("{}", "-".repeat(40));
            print!("{text}");
        }
        Err(_) => {
            println!("（文件不存在，当前使用内置默认配置；运行 `adl config --init` 生成模板）");
        }
    }
    0
}

// ─────────────────────────── 工具 ───────────────────────────

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mul) = match s.chars().last() {
        Some('k') | Some('K') => (&s[..s.len() - 1], 1024u64),
        Some('m') | Some('M') => (&s[..s.len() - 1], 1024 * 1024),
        Some('g') | Some('G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| format!("无法解析尺寸: {s:?}"))?;
    n.checked_mul(mul).ok_or_else(|| "尺寸过大".to_string())
}

fn exit_code_for(e: &AdlError) -> i32 {
    match e {
        AdlError::Cancelled => 7,
        AdlError::DiskFull => 34,
        AdlError::AuthRequired(_) => 33,
        AdlError::Network(_)
        | AdlError::ConnectionReset
        | AdlError::NetworkBlocked
        | AdlError::Stalled => 6,
        AdlError::Timeout => 2,
        AdlError::Tls(_) => 1,
        _ => 1,
    }
}

fn render_progress(p: &Progress) {
    let speed = human_speed(p.speed_bps);
    if p.total == 0 {
        eprint!("\r已下载 {} · {speed}/s\x1b[K", human_size(p.completed));
        return;
    }
    let pct = (p.completed as f64 / p.total as f64 * 100.0).min(100.0);
    let filled = (pct / 2.5) as usize;
    let bar: String = "█".repeat(filled) + &"░".repeat(40 - filled);
    eprint!(
        "\r[{bar}] {pct:5.1}%  {}/{}  {speed}/s\x1b[K",
        human_size(p.completed),
        human_size(p.total)
    );
}

fn human_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

fn human_speed(bps: u64) -> String {
    human_size(bps)
}

fn human_duration(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{}s", s / 60, s % 60)
    }
}
