//! 从 zip 压缩包装插件。
//!
//! 分发形态定的是：插件打成 zip 传到 GitHub Releases，ADL 下载回来、
//! 解到 `plugins/<id>/`、**装完把压缩包删掉**（用户 m16664 明确要求）。
//! 所以这里只有一条路径：`install_zip(path, root)`，装完自己删包。
//!
//! 安全约束：
//! - 拒绝绝对路径与 `..`（zip slip；恶意包能靠它写到 `plugins/` 外面）。
//! - 必须能找到 `plugin.toml`，且能解析出 `id`，否则整个安装失败并清理
//!   掉已经解出来的半成品 —— 宁可什么都不装，也别留一个半截目录让
//!   下次启动的加载器报错。

use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// 解出来的插件的目录名（用 manifest 里的 id，不用压缩包里的文件夹名）。
pub struct Installed {
    pub id: String,
    /// 装到哪儿了。界面现在只用 `id` 报状态，留着它是因为"装完打开目录"
    /// 这类动作迟早要（和「插件目录」卡片上的按钮是同一个诉求）。
    #[allow(dead_code)]
    pub dir: PathBuf,
}

/// 从 `zip_path` 安装到 `root/<id>/`，成功后删除 `zip_path`。
pub fn install_zip(zip_path: &Path, root: &Path) -> Result<Installed, String> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| format!("打不开压缩包 {}: {e}", zip_path.display()))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("不是有效的 zip: {e}"))?;

    // 先扫一遍，找出 plugin.toml 在哪一层。GitHub 上打包常常多套一层
    // 目录（`my-plugin-main/plugin.toml`），所以两种都认。
    let mut manifest_at: Option<(usize, String)> = None;
    for i in 0..zip.len() {
        let name = match zip.by_index_raw(i).and_then(|f| Ok(f.name()?.to_string())) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let norm = name.replace('\\', "/");
        if norm == "plugin.toml" {
            manifest_at = Some((i, String::new()));
            break;
        }
        if let Some(rest) = norm.strip_suffix("/plugin.toml") {
            if !rest.contains('/') {
                manifest_at = Some((i, format!("{rest}/")));
                break;
            }
        }
    }
    let (manifest_idx, prefix) = manifest_at.ok_or_else(|| {
        "压缩包里没找到 plugin.toml（应该在根目录或第一层子目录里）".to_string()
    })?;

    // 从那条 manifest 读出 id，用它当目标目录名。
    let id = {
        let mut f = zip
            .by_index(manifest_idx)
            .map_err(|e| format!("读 plugin.toml 失败: {e}"))?;
        let mut text = String::new();
        f.read_to_string(&mut text)
            .map_err(|e| format!("plugin.toml 不是文本: {e}"))?;
        // 借用插件自己的解析器：它已经知道 manifest 的全部必填字段，
        // 在这里再写一遍 TOML 解析只会两边慢慢对不上。
        let m = adl_plugin::Manifest::parse(&text)
            .map_err(|e| format!("plugin.toml 解析失败: {e}"))?;
        m.id.trim().to_string()
    };

    // id 直接当目录名，所以要挡住 `../x` 这类。
    if id.contains('/') || id.contains('\\') || id.contains("..") {
        return Err(format!("插件 id 不合法：{id}"));
    }

    let dest = root.join(&id);
    if dest.exists() {
        return Err(format!(
            "插件目录已存在：{}。先卸载再装。",
            dest.display()
        ));
    }
    std::fs::create_dir_all(&dest).map_err(|e| format!("建目录失败 {}: {e}", dest.display()))?;

    // 解包。任何一项失败都回滚整个目录。
    let result = extract_into(&mut zip, &prefix, &dest);
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&dest);
        return Err(e);
    }

    // 装完删包（用户要求）。删不掉不算安装失败 —— 插件已经能用了，
    // 只是留了个文件，报个错让用户知道可以手动清。
    if let Err(e) = std::fs::remove_file(zip_path) {
        eprintln!(
            "[plugin] 压缩包安装成功但删除失败 {}: {e}",
            zip_path.display()
        );
    }

    Ok(Installed { id, dir: dest })
}

fn extract_into(
    zip: &mut zip::ZipArchive<std::fs::File>,
    prefix: &str,
    dest: &Path,
) -> Result<(), String> {
    for i in 0..zip.len() {
        let mut f = match zip.by_index(i) {
            Ok(f) => f,
            Err(e) => return Err(format!("读取压缩包第 {i} 项失败: {e}")),
        };
        let raw = f.name().map_err(|e| format!("第 {i} 项文件名无效: {e}"))?;
        let raw = raw.replace('\\', "/");
        let rel = match raw.strip_prefix(prefix) {
            Some(r) => r.to_string(),
            None => continue, // 不在目标插件目录下（比如同事带的 README）
        };
        if rel.is_empty() || rel.ends_with('/') {
            continue; // 目录项，create_dir_all 时自然建出来
        }
        let out = match safe_join(dest, &rel) {
            Some(p) => p,
            None => return Err(format!("压缩包里有过不安全的路径：{raw}")),
        };
        if f.is_dir() {
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("建目录失败 {}: {e}", parent.display()))?;
        }
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)
            .map_err(|e| format!("解压 {raw} 失败: {e}"))?;
        std::fs::write(&out, &buf).map_err(|e| format!("写 {} 失败: {e}", out.display()))?;
    }
    Ok(())
}

/// 把 zip 里的相对路径拼到 `base` 下，任何 `..`／绝对路径／盘符都拒绝。
fn safe_join(base: &Path, rel: &str) -> Option<PathBuf> {
    let mut out = base.to_path_buf();
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(s) => out.push(s),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if out.starts_with(base) {
        Some(out)
    } else {
        None
    }
}

/// 把 zip 里所有文件列出来，供"先预览再装"用（现在只在测试里用）。
#[allow(dead_code)]
pub fn list_zip(zip_path: &Path) -> Result<Vec<String>, String> {
    let file = std::fs::File::open(zip_path).map_err(|e| format!("打不开压缩包: {e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("不是有效的 zip: {e}"))?;
    let mut v = Vec::new();
    for i in 0..zip.len() {
        if let Ok(f) = zip.by_index_raw(i) {
            if let Ok(name) = f.name() {
                v.push(name.to_string());
            }
        }
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal() {
        let base = Path::new("C:/root/p");
        assert!(safe_join(base, "a/b.txt").is_some());
        assert!(safe_join(base, "../evil.txt").is_none());
        assert!(safe_join(base, "a/../../evil.txt").is_none());
        assert!(safe_join(base, "C:/Windows/evil.dll").is_none());
        assert!(safe_join(base, "/etc/passwd").is_none());
    }

    #[test]
    fn installs_a_minimal_zip() {
        // 🔴 不能用 `std::env::temp_dir()`：本机 `%TEMP%` 对 Rust 进程是
        // 拒绝访问（Os error 5），测试会在一开始就 unwrap 失败。
        // 统一放到 target/test-tmp 下，和 core 那边的测试一个路数。
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("test-tmp");
        let tmp = base.join("adl-zip-test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let zip_path = tmp.join("p.zip");
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts: zip::write::SimpleFileOptions =
                zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            use std::io::Write;
            w.start_file("demo/plugin.toml", opts).unwrap();
            w.write_all(b"id = \"dev.test.demo\"\nname = \"D\"\nversion = \"1.0.0\"\napi_version = 1\nentry = \"main.js\"\nkind = \"quickjs\"\n")
                .unwrap();
            w.start_file("demo/main.js", opts).unwrap();
            w.write_all(b"adl.onResolve(function(){ return {action:'pass'}; });").unwrap();
            w.finish().unwrap();
        }
        let root = tmp.join("plugins");
        std::fs::create_dir_all(&root).unwrap();
        let got = install_zip(&zip_path, &root).expect("install");
        assert_eq!(got.id, "dev.test.demo");
        assert!(root.join("dev.test.demo/plugin.toml").is_file());
        assert!(root.join("dev.test.demo/main.js").is_file());
        // 装完包要被删掉
        assert!(!zip_path.exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
