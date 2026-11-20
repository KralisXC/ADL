//! 配置文件（TOML）。
//!
//! 位置优先级：
//! 1. exe 同级的 `config.toml`（便携模式：存在就用它）；
//! 2. `%LOCALAPPDATA%\ADL\config.toml`（Windows）或 `$HOME/.local/share/adl/config.toml`。
//!
//! 命令行显式给出的参数总是覆盖配置里的同名项。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// 主配置里每个插件只写一小段（开关等），详细设置在各自的
/// `%LOCALAPPDATA%\ADL\plugin-settings\<id>.json` 里，避免主配置膨胀。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCfg {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Default for PluginCfg {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// 应用数据目录（历史记录、默认配置都放这）。
pub fn app_data_dir() -> PathBuf {
    if let Ok(p) = std::env::var("LOCALAPPDATA") {
        if !p.trim().is_empty() {
            return PathBuf::from(p).join("ADL");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home).join(".local").join("share").join("adl");
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// 配置文件路径：优先 exe 同级的（便携），否则应用数据目录。
pub fn config_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("config.toml");
            if p.exists() {
                return p;
            }
        }
    }
    app_data_dir().join("config.toml")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 默认保存目录
    pub dir: String,
    /// 期望的最大并发连接数（1–64）
    pub connections: usize,
    /// 单请求分块大小（字节，0 = 按文件大小自动分配）
    pub chunk_size: u64,
    /// 全局限速（字节/秒，0 = 不限）
    pub limit: u64,
    /// User-Agent（空 = 用内置的浏览器 UA）
    pub user_agent: String,
    /// 强制 HTTP/1.1
    pub http1: bool,
    /// HTTP 代理（空 = 不用）
    pub proxy: String,
    /// 插件模式："safe"（默认）或 "trusted"
    pub mode: String,
    /// 插件目录（空 = exe 同级 plugins/）
    pub plugin_dir: String,
    /// 是否记录下载历史
    pub history_enabled: bool,
    /// 每个插件的一小段主配置（只用 enabled 开关；详细设置在各插件自己的 json 里）
    pub plugins: BTreeMap<String, PluginCfg>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dir: ".".to_string(),
            connections: 16,
            chunk_size: 0,
            limit: 0,
            user_agent: String::new(),
            http1: false,
            proxy: String::new(),
            mode: "safe".to_string(),
            plugin_dir: String::new(),
            history_enabled: true,
            plugins: BTreeMap::new(),
        }
    }
}

impl Config {
    /// 读取配置。文件不存在 → 默认值；解析失败 → 打警告 + 默认值（不让一个写坏的配置挡住下载）。
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                // 兼容记事本 / PowerShell 写出的 UTF-8 BOM。
                let text = text.strip_prefix('\u{feff}').unwrap_or(&text).to_string();
                match toml::from_str::<Config>(&text) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!(
                            "警告：配置文件 {} 解析失败（{}），已改用默认配置",
                            path.display(),
                            e
                        );
                        Config::default()
                    }
                }
            }
            Err(_) => Config::default(),
        }
    }

    pub fn mode_trusted(&self) -> bool {
        self.mode.eq_ignore_ascii_case("trusted")
    }

    /// 插件是否启用（主配置里没有该条目时默认启用）。
    pub fn plugin_enabled(&self, id: &str) -> bool {
        self.plugins.get(id).map(|p| p.enabled).unwrap_or(true)
    }

    pub fn dir_path(&self) -> PathBuf {
        PathBuf::from(&self.dir)
    }

    pub fn proxy_opt(&self) -> Option<String> {
        if self.proxy.trim().is_empty() {
            None
        } else {
            Some(self.proxy.clone())
        }
    }

    pub fn user_agent_opt(&self) -> Option<String> {
        if self.user_agent.trim().is_empty() {
            None
        } else {
            Some(self.user_agent.clone())
        }
    }

    pub fn plugin_dir_opt(&self) -> Option<PathBuf> {
        if self.plugin_dir.trim().is_empty() {
            None
        } else {
            Some(PathBuf::from(&self.plugin_dir))
        }
    }

    /// 把当前配置写回配置文件（覆盖），用于 GUI 的设置页。
    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// 把默认配置（带注释的模板）写到默认位置；已存在则不动。
    /// 返回实际路径。`force=true` 时覆盖。
    pub fn write_template(force: bool) -> std::io::Result<PathBuf> {
        let path = config_path();
        if path.exists() && !force {
            return Ok(path);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, TEMPLATE)?;
        Ok(path)
    }
}

const TEMPLATE: &str = r#"# ADL 配置文件（TOML）
# 命令行参数会覆盖这里同名的设置。

# 默认保存目录（相对路径相对当前工作目录）。
# 路径里的反斜杠要写成双反斜杠或改用正斜杠，例如 "E:\\Downloads" 或 "E:/Downloads"。
dir = "."

# 期望的最大并发连接数（1–64）。引擎会按服务器实际接受的量自适应收敛。
connections = 16

# 单请求分块大小（字节）。0 = 按文件大小自动分配。也支持写 "4M" 这种？
# 注意：TOML 里这里只接受纯数字字节数，例如 4M 请写 4194304。
chunk_size = 0

# 全局限速，字节/秒，0 = 不限。
limit = 0

# User-Agent。留空 = 使用内置的浏览器 UA（冒充浏览器，避免被当爬虫）。
user_agent = ""

# 强制 HTTP/1.1（少数站点 h2 不稳时用）。
http1 = false

# HTTP 代理，例如 "http://127.0.0.1:7890"。留空 = 不用。
proxy = ""

# 插件模式："safe"（默认，只能观察/拦截）或 "trusted"（允许改写请求）。
mode = "safe"

# 插件目录。留空 = 使用 adl.exe 同级的 plugins/。
plugin_dir = ""

# 是否记录下载历史（history.json）。
history_enabled = true
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_sane() {
        let c = Config::default();
        assert_eq!(c.connections, 16);
        assert_eq!(c.chunk_size, 0);
        assert!(!c.mode_trusted());
        assert!(c.history_enabled);
        assert!(c.proxy_opt().is_none());
        assert!(c.user_agent_opt().is_none());
    }

    #[test]
    fn parses_partial_toml() {
        let c: Config = toml::from_str("connections = 4\nmode = \"trusted\"").unwrap();
        assert_eq!(c.connections, 4);
        assert!(c.mode_trusted());
        // 其它字段走 default
        assert_eq!(c.dir, ".");
        assert_eq!(c.chunk_size, 0);
    }

    #[test]
    fn template_roundtrips() {
        let c: Config = toml::from_str(TEMPLATE).unwrap();
        assert_eq!(c.connections, 16);
        assert_eq!(c.mode, "safe");
        assert!(c.history_enabled);
    }
}
