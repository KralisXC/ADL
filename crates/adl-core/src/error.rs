//! 错误类型。错误码沿用 aria2 数值（DESIGN.md §3.7），方便与旧项目/外部工具对齐。

use thiserror::Error;

pub type Result<T> = std::result::Result<T, AdlError>;

#[derive(Debug, Error)]
pub enum AdlError {
    #[error("未知错误: {0}")]
    Unknown(String),

    #[error("连接超时")]
    Timeout,

    #[error("资源不存在（HTTP {0}）")]
    ResourceNotFound(u16),

    #[error("网络错误: {0}")]
    Network(String),

    #[error("服务器不支持断点续传")]
    ResumeNotSupported,

    #[error("控制文件的 piece_length 与当前配置不一致，拒绝续传")]
    PieceLengthChanged,

    #[error("服务器忽略了 Range 请求（期望从 {expected} 开始，返回 200）")]
    RangeIgnored { expected: u64 },

    #[error("Content-Range 与请求不符: {0}")]
    ContentRangeMismatch(String),

    #[error("需要认证（HTTP {0}）")]
    AuthRequired(u16),

    #[error("服务端暂时不可用（HTTP {0}）")]
    ServerBusy(u16),

    #[error("磁盘空间不足")]
    DiskFull,

    #[error("目标是网页而不是文件（Content-Type: {0}）")]
    HtmlInsteadOfFile(String),

    #[error("重定向次数过多")]
    TooManyRedirects,

    #[error("已取消")]
    Cancelled,

    #[error("连接被重置")]
    ConnectionReset,

    #[error("网络被阻断（连续重置超过阈值）")]
    NetworkBlocked,

    #[error("服务器停止发送数据（可能触发了反爬/限速），进度已保存，可稍后手动续传")]
    Stalled,

    #[error("校验失败：服务器声明 {expected}，实际 {actual}")]
    ChecksumMismatch { expected: String, actual: String },

    #[error("TLS 配置失败: {0}")]
    Tls(String),

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP 客户端错误: {0}")]
    Http(#[from] reqwest::Error),

    #[error("URL 解析失败: {0}")]
    Url(#[from] url::ParseError),

    #[error("JSON 序列化失败: {0}")]
    Json(#[from] serde_json::Error),
}

impl AdlError {
    /// 映射到 aria2 的 errorCode 数值。
    pub fn aria2_code(&self) -> u16 {
        match self {
            AdlError::Unknown(_) => 1,
            AdlError::Timeout => 2,
            AdlError::ResourceNotFound(_) => 3,
            AdlError::Network(_) => 6,
            AdlError::ResumeNotSupported => 8,
            AdlError::PieceLengthChanged => 10,
            AdlError::RangeIgnored { .. } => 8,
            AdlError::ContentRangeMismatch(_) => 22,
            AdlError::AuthRequired(_) => 33,
            AdlError::ServerBusy(_) => 29,
            AdlError::DiskFull => 34,
            AdlError::HtmlInsteadOfFile(_) => 22,
            AdlError::TooManyRedirects => 23,
            AdlError::Cancelled => 31,
            AdlError::ConnectionReset => 35,
            AdlError::NetworkBlocked => 36,
            AdlError::Stalled => 5, // aria2：下载因速度过慢/无数据被中止
            AdlError::ChecksumMismatch { .. } => 8, // 语义上接近"无法续传/数据不一致"
            AdlError::Tls(_) => 1,
            AdlError::Io(e) => {
                if is_disk_full(e) {
                    34
                } else {
                    1
                }
            }
            AdlError::Http(e) => {
                if e.is_timeout() {
                    2
                } else if e.is_connect() {
                    6
                } else {
                    1
                }
            }
            AdlError::Url(_) => 1,
            AdlError::Json(_) => 1,
        }
    }

    /// 只重试这几类。4xx 不重试（429 除外，它由降档逻辑处理）。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            AdlError::Timeout
                | AdlError::ConnectionReset
                | AdlError::Network(_)
                | AdlError::ServerBusy(504)
        )
    }

    /// 服务端因并发过高拒绝 → 触发并发降档（DESIGN.md §3.7）。
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, AdlError::ResourceNotFound(403) | AdlError::ServerBusy(429))
    }

    /// 是否是"连不上"级别（握手被拒 / 端口不可达）。
    /// 这类失败多半是服务端并发限额或临时不可达：不该怪到具体分片头上，
    /// 应宽松重试，并把"拒绝接入"当作并发过高的信号去做降档（DESIGN.md §3.8）。
    pub fn is_connect_level(&self) -> bool {
        matches!(self, AdlError::Network(_))
    }

    /// 把 HTTP 状态码转成错误。`200`/`206` 不在此列（由调用方判定语义）。
    pub fn from_status(status: u16) -> AdlError {
        match status {
            401 | 407 => AdlError::AuthRequired(status),
            403 | 404 => AdlError::ResourceNotFound(status),
            429 => AdlError::ServerBusy(429),
            502 | 503 | 504 => AdlError::ServerBusy(status),
            s => AdlError::Unknown(format!("HTTP {s}")),
        }
    }
}

/// Windows `ERROR_DISK_FULL` = 112；POSIX `ENOSPC` = 28。
pub fn is_disk_full(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(112) | Some(28))
}
