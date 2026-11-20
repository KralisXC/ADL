//! 文件名与路径处理（DESIGN.md §3.8）。
//!
//! 两件事必须分开：
//! - **清洗**：把服务器给的名字变成合法文件名（不依赖目录，纯字符串）
//! - **裁决**：决定最终落盘路径，必须校验不许逃出下载根目录

use std::path::{Path, PathBuf};

pub const MAX_COMPONENT: usize = 255;
/// Windows MAX_PATH 不含结尾 NUL。超过就加 `\\?\` 前缀。
pub const MAX_PATH: usize = 260;
pub const MAX_EXT: usize = 16;

/// Windows 保留设备名。即便带扩展名（`CON.txt`）也仍然保留。
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 把任意字符串变成合法的单段文件名。
///
/// - 替换 `<>:"/\|?*` 与控制字符为 `_`
/// - 剥掉尾部的 `.` 和空格（Windows 不允许，同时防 `.` / `..` 穿越）
/// - 空结果回退到 `download`
/// - 保留设备名加 `_` 前缀
pub fn sanitize_name(raw: &str) -> String {
    let mut s: String = raw
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();

    // 尾部点和空格在 Windows 上会被静默吃掉，而且 "." / ".." 会变成目录穿越
    s = s.trim().trim_end_matches(['.', ' ']).to_string();

    // 截断到 MAX_COMPONENT，但不要切断 UTF-8
    if s.len() > MAX_COMPONENT {
        let mut end = MAX_COMPONENT;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s = s.trim_end_matches(['.', ' ']).to_string();
    }

    if s.is_empty() {
        return "download".to_string();
    }

    // 保留设备名：CON / CON.txt 都不行
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        s = format!("_{s}");
    }

    s
}

/// 看起来像"不透明名字"：无扩展名、够长、只由 `[0-9a-zA-Z_-]` 组成。
///
/// 典型来源是 `?id=8f3a9c2e1b4d...` 这类 CDN 链接，直接拿来当文件名对人类毫无意义。
pub fn is_opaque_name(name: &str) -> bool {
    if name.len() < 16 {
        return false;
    }
    if Path::new(name).extension().is_some() {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 从 URL 路径的最后一段提取文件名（已百分号解码）。
pub fn name_from_url_path(u: &url::Url) -> Option<String> {
    let last = u.path_segments()?.next_back()?;
    if last.is_empty() {
        return None;
    }
    let decoded = percent_decode(last);
    let n = sanitize_name(&decoded);
    if n.is_empty() {
        None
    } else {
        Some(n)
    }
}

/// 从 `Content-Disposition` 里取文件名。
///
/// 优先 RFC 5987 的 `filename*=UTF-8''xxx`，其次裸 `filename=`。
pub fn filename_from_content_disposition(value: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();

    // 1) filename*=charset''percent-encoded
    if let Some(pos) = lower.find("filename*=") {
        let rest = &value[pos + "filename*=".len()..];
        let token = rest.split(';').next()?.trim();
        // charset''value
        let mut it = token.splitn(3, '\'');
        let _charset = it.next();
        let _lang = it.next();
        if let Some(enc) = it.next() {
            let decoded = percent_decode(enc);
            let n = sanitize_name(&decoded);
            if !n.is_empty() {
                return Some(n);
            }
        }
    }

    // 2) 裸 filename=
    if let Some(pos) = lower.find("filename=") {
        let rest = &value[pos + "filename=".len()..];
        let token = rest.split(';').next()?.trim();
        let unquoted = token.trim_matches('"').trim();
        if !unquoted.is_empty() {
            let n = sanitize_name(&percent_decode(unquoted));
            if !n.is_empty() {
                return Some(n);
            }
        }
    }

    None
}

/// 最小的百分号解码。非法序列原样保留，不 panic。
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = (b[i + 1] as char).to_digit(16);
            let lo = (b[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 兜底名字：`Download_<毫秒时间戳>`。
pub fn fallback_timestamp_name() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("Download_{ms}")
}

/// 单一裁决点：给定目录和候选名，算出最终路径。
///
/// 候选优先级：显式 `--out` > `Content-Disposition` > URL 路径 > 时间戳兜底。
pub fn resolve_out_name(dir: &Path, candidates: &[Option<String>]) -> PathBuf {
    for c in candidates.iter().flatten() {
        let n = sanitize_name(c);
        if !n.is_empty() && n != "download" {
            return dir.join(n);
        }
    }
    dir.join(fallback_timestamp_name())
}

/// 若要落盘的文件已存在，自动改名成 `name (1).ext`、`name (2).ext`…（用户 m01907 要求）。
///
/// `--restart` 时调用方**不**该调本函数，让原有文件被显式覆盖。
pub fn unique_path(base: &Path) -> PathBuf {
    if !base.exists() {
        return base.to_path_buf();
    }
    let parent = base.parent().unwrap_or_else(|| Path::new("."));
    let stem = base
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("download");
    let ext = base.extension().and_then(|s| s.to_str());
    for i in 1..10_000u32 {
        let name = match ext {
            Some(e) => format!("{stem} ({i}).{e}"),
            None => format!("{stem} ({i})"),
        };
        let cand = parent.join(name);
        if !cand.exists() {
            return cand;
        }
    }
    base.to_path_buf()
}

/// 判断路径是否在根目录之内（按路径分量比对，不是字符串前缀）。
///
/// `C:\dl2` 不是 `C:\dl` 的子目录 —— 字符串前缀比对会误判。
pub fn is_within(root: &Path, target: &Path) -> bool {
    let mut r = root.components();
    let mut t = target.components();
    loop {
        match (r.next(), t.next()) {
            (None, _) => return true,
            (Some(a), Some(b)) => {
                if !components_eq(a, b) {
                    return false;
                }
            }
            (Some(_), None) => return false,
        }
    }
}

fn components_eq(
    a: std::path::Component<'_>,
    b: std::path::Component<'_>,
) -> bool {
    use std::path::Component::*;
    match (a, b) {
        (Prefix(x), Prefix(y)) => x.as_os_str().eq_ignore_ascii_case(y.as_os_str()),
        (RootDir, RootDir) => true,
        (CurDir, CurDir) => true,
        (ParentDir, ParentDir) => true,
        (Normal(x), Normal(y)) => x.eq_ignore_ascii_case(y),
        _ => false,
    }
}

/// 剥掉 `canonicalize()` 产生的 `\\?\` 前缀。
///
/// `\\?\E:\a` → `E:\a`；`\\?\UNC\server\share` → `\\server\share`。
pub fn strip_verbatim(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest.to_string());
    }
    p.to_path_buf()
}

/// 太长就加 `\\?\` 前缀。**只在确定是文件路径处调用**，
/// 不能改通用的 UTF-8→宽字符转换（那个还给 `CreateProcessW` 用）。
pub fn to_long_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if s.starts_with(r"\\?\") || s.starts_with(r"\\.\") {
        return p.to_path_buf();
    }
    if s.len() < MAX_PATH {
        return p.to_path_buf();
    }
    // 已经是绝对路径
    if s.len() >= 2 && s.as_bytes()[1] == b':' {
        return PathBuf::from(format!(r"\\?\{s}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\") {
        return PathBuf::from(format!(r"\\?\UNC\{rest}"));
    }
    p.to_path_buf()
}

/// 按 `total` 和期望段数切分区间。**最后一段吃掉余数**。
///
/// `base = total / n` 直接切会丢字节（实测 64 MiB / 6 段丢 4 字节）。
pub fn split_segments(total: u64, n: usize) -> Vec<(u64, u64)> {
    if total == 0 || n == 0 {
        return Vec::new();
    }
    let n = n as u64;
    let base = total / n;
    if base == 0 {
        return vec![(0, total - 1)];
    }
    let mut out = Vec::with_capacity(n as usize);
    let mut start = 0u64;
    for i in 0..n {
        let end = if i == n - 1 {
            total - 1 // 最后一段吃掉余数
        } else {
            start + base - 1
        };
        out.push((start, end));
        start = end + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_illegal() {
        assert_eq!(sanitize_name(r#"a<b>c:d"e/f\g|h?i*j"#), "a_b_c_d_e_f_g_h_i_j");
    }

    #[test]
    fn sanitize_strips_trailing_dots_and_spaces() {
        assert_eq!(sanitize_name("file.  "), "file");
        assert_eq!(sanitize_name("..."), "download");
        assert_eq!(sanitize_name(""), "download");
    }

    #[test]
    fn sanitize_escapes_reserved() {
        assert_eq!(sanitize_name("CON"), "_CON");
        assert_eq!(sanitize_name("con.txt"), "_con.txt");
        assert_eq!(sanitize_name("COM1"), "_COM1");
        // CONSOLE 不是保留名
        assert_eq!(sanitize_name("CONSOLE"), "CONSOLE");
    }

    #[test]
    fn sanitize_truncates_on_char_boundary() {
        let long = "中".repeat(200); // 600 字节
        let n = sanitize_name(&long);
        assert!(n.len() <= MAX_COMPONENT);
        assert!(n.chars().all(|c| c == '中')); // 没切坏
    }

    #[test]
    fn unique_path_renames_on_conflict() {
        let d = crate::testutil::tmpdir("unique");
        let base = d.join("file.zip");
        assert_eq!(unique_path(&base), base.clone());
        std::fs::write(&base, b"x").unwrap();
        let p1 = unique_path(&base);
        assert_eq!(p1.file_name().unwrap(), "file (1).zip");
        std::fs::write(&p1, b"x").unwrap();
        let p2 = unique_path(&base);
        assert_eq!(p2.file_name().unwrap(), "file (2).zip");

        // 无扩展名
        let n = d.join("plain");
        std::fs::write(&n, b"x").unwrap();
        assert_eq!(unique_path(&n).file_name().unwrap(), "plain (1)");
    }

    #[test]
    fn opaque_name_detection() {        assert!(is_opaque_name("8f3a9c2e1b4d5f6a7b8c"));
        assert!(!is_opaque_name("short"));
        assert!(!is_opaque_name("file.txt")); // 有扩展名
        assert!(!is_opaque_name("has space in name"));
    }

    #[test]
    fn split_segments_no_byte_loss() {
        for total in [1u64, 5, 100, 64 * 1024 * 1024, 3_303_444_480] {
            for n in 1..=16usize {
                let segs = split_segments(total, n);
                // 1) 覆盖完整且不重叠
                assert_eq!(segs[0].0, 0, "total={total} n={n}");
                assert_eq!(segs.last().unwrap().1, total - 1, "total={total} n={n}");
                for w in segs.windows(2) {
                    assert_eq!(w[0].1 + 1, w[1].0, "total={total} n={n} 段不连续");
                }
                // 2) 长度之和精确等于 total
                let sum: u64 = segs.iter().map(|(s, e)| e - s + 1).sum();
                assert_eq!(sum, total, "total={total} n={n} 丢字节");
            }
        }
    }

    #[test]
    fn split_segments_handles_tiny_total() {
        // total < n 时只能给一段
        let segs = split_segments(3, 10);
        assert_eq!(segs, vec![(0, 2)]);
    }

    #[test]
    fn content_disposition_rfc5987() {
        let v = "attachment; filename=\"fallback.txt\"; filename*=UTF-8''%E4%B8%AD%E6%96%87.zip";
        assert_eq!(filename_from_content_disposition(v).as_deref(), Some("中文.zip"));
    }

    #[test]
    fn content_disposition_plain() {
        let v = r#"attachment; filename="report.pdf""#;
        assert_eq!(filename_from_content_disposition(v).as_deref(), Some("report.pdf"));
    }

    #[test]
    fn content_disposition_none() {
        assert_eq!(filename_from_content_disposition("inline"), None);
    }

    #[test]
    fn is_within_component_wise() {
        let root = Path::new(r"C:\dl");
        assert!(is_within(root, Path::new(r"C:\dl\a\b.bin")));
        assert!(is_within(root, Path::new(r"C:\dl")));
        // 关键：dl2 不是 dl 的子目录
        assert!(!is_within(root, Path::new(r"C:\dl2\a.bin")));
        assert!(!is_within(root, Path::new(r"C:\other")));
    }

    #[test]
    fn strip_verbatim_works() {
        assert_eq!(strip_verbatim(Path::new(r"\\?\E:\a\b")), PathBuf::from(r"E:\a\b"));
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\UNC\srv\share\x")),
            PathBuf::from(r"\\srv\share\x")
        );
        // 没有前缀就原样返回
        assert_eq!(strip_verbatim(Path::new(r"E:\a")), PathBuf::from(r"E:\a"));
    }

    #[test]
    fn name_from_url() {
        let u = url::Url::parse("https://example.com/path/file%20name.zip?x=1").unwrap();
        assert_eq!(name_from_url_path(&u).as_deref(), Some("file name.zip"));
    }

    #[test]
    fn name_from_url_root() {
        let u = url::Url::parse("https://example.com/").unwrap();
        assert_eq!(name_from_url_path(&u), None);
    }
}
