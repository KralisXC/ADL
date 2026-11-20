//! ADL 核心引擎。
//!
//! 分层：`error` / `path` 是底座，`segment` / `control` 是分片账本，
//! `http` / `probe` 是传输，`hooks` 是插件契约，`engine` 把它们串起来。
//!
//! M1 只做单流 + 断点续传，但分片表与 hook 调用点全部就位，
//! M2 加并发时不需要改这里的分层。

pub mod control;
pub mod engine;
pub mod error;
pub mod hooks;
pub mod http;
pub mod md5;
pub mod path;
pub mod probe;
pub mod segment;

pub use control::{ControlFile, DEFAULT_PIECE_LENGTH};
pub use engine::{clamp_concurrency, download, hash_file, DownloadOptions, DownloadResult, ProgressFn};
pub use error::{AdlError, Result};
pub use md5::{md5_file, md5_hex};
pub use hooks::{
    Candidate, Completed, DownloadPlan, Hooks, NoopHooks, Progress, ResponseVerdict, Verdict,
};
pub use segment::{SegState, Segment, SegmentTable};

/// 测试用临时目录。
///
/// 不能直接用 `std::env::temp_dir()`：本机（以及不少受限 Windows 环境）下
/// `%TEMP%` 对普通进程是拒绝访问的（`os error 5`），测试会全部假失败。
/// 这里先试系统临时目录，失败就退回 workspace 的 `target/test-tmp`。
#[cfg(test)]
pub(crate) mod testutil {
    use std::path::PathBuf;

    pub fn tmpdir(tag: &str) -> PathBuf {
        let candidates = [
            std::env::temp_dir(),
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
                .join("test-tmp"),
        ];
        for base in candidates {
            let p = base.join(format!("adl-{tag}-{}", std::process::id()));
            if std::fs::create_dir_all(&p).is_ok() {
                return p;
            }
        }
        panic!("找不到可写的临时目录（%TEMP% 与 target/test-tmp 都不可写）");
    }
}
