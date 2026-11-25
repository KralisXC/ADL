//! 插件清单（manifest.toml，DESIGN.md §4.5）。
//!
//! 格式由用户 m02307 拍板：**TOML**（与 ADL 配置一致）。

use serde::Deserialize;
use std::collections::HashSet;
use std::fmt;

/// ADL 插件 API 版本。插件 `api_version` 不匹配 → 直接拒载（不同版本 hook 语义可能变）。
pub const API_VERSION: u32 = 1;

/// 插件清单文件在插件目录里的固定文件名。
pub const MANIFEST_FILE: &str = "plugin.toml";

/// 九个 hook 点。内部一律用 snake_case 键，manifest 里两种写法都接受。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookPoint {
    Resolve,
    Plan,
    BeforeRequest,
    Response,
    Progress,
    Retry,
    Preempt,
    Degrade,
    Complete,
    /// 用户在界面/终端里点插件声明的按钮时触发。
    ///
    /// 它不在下载管线里，是"用户主动发起"的入口；但照样要走 `[[hooks]]`
    /// 声明，否则宿主的 `active_hooks` 里没有它，插件页会显示"没挂任何 hook"。
    Action,
    /// 用户在终端里敲插件命令时触发，同上。
    Command,
}

impl HookPoint {
    pub fn canonical(self) -> &'static str {
        match self {
            HookPoint::Resolve => "onResolve",
            HookPoint::Plan => "onPlan",
            HookPoint::BeforeRequest => "onBeforeRequest",
            HookPoint::Response => "onResponse",
            HookPoint::Progress => "onProgress",
            HookPoint::Retry => "onRetry",
            HookPoint::Preempt => "onPreempt",
            HookPoint::Degrade => "onDegrade",
            HookPoint::Complete => "onComplete",
            HookPoint::Action => "onAction",
            HookPoint::Command => "onCommand",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let k = s
            .trim()
            .trim_start_matches("on")
            .replace(['_', '-'], "")
            .to_ascii_lowercase();
        match k.as_str() {
            "resolve" => Some(Self::Resolve),
            "plan" => Some(Self::Plan),
            "beforerequest" => Some(Self::BeforeRequest),
            "response" => Some(Self::Response),
            "progress" => Some(Self::Progress),
            "retry" => Some(Self::Retry),
            "preempt" => Some(Self::Preempt),
            "degrade" => Some(Self::Degrade),
            "complete" => Some(Self::Complete),
            "action" => Some(Self::Action),
            "command" => Some(Self::Command),
            _ => None,
        }
    }

    /// 这个 hook 是否需要 Transform 权限（能改写引擎行为，或替用户动手）。
    ///
    /// `Action` / `Command` 也算：它们由用户点击触发，但里面能干的事
    /// （执行外部程序、写文件）比下载管线里的 hook 更重，safe 模式下
    /// 按钮本来就不显示（`ui.collect` 只在 trusted 下输出 actions），
    /// 这里保持一致，否则插件页会显示"挂着 onAction"却点不动。
    pub fn needs_transform(self) -> bool {
        matches!(
            self,
            HookPoint::Resolve
                | HookPoint::Plan
                | HookPoint::BeforeRequest
                | HookPoint::Action
                | HookPoint::Command
        )
    }

    pub fn is_observe(self) -> bool {
        matches!(
            self,
            HookPoint::Progress | HookPoint::Retry | HookPoint::Preempt | HookPoint::Degrade
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HookDecl {
    pub point: String,
    #[serde(default)]
    pub priority: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Permissions {
    /// 允许发起 http 请求（M3 暂未实现实际请求，仅记录）
    #[serde(default)]
    pub net: bool,
    /// 允许读取的目录前缀（空 = 不允许读任何文件）
    #[serde(default)]
    pub fs_read: Vec<String>,
    /// 允许写入的目录前缀
    #[serde(default)]
    pub fs_write: Vec<String>,
    /// 允许执行外部程序（M7 才真正放开）
    #[serde(default)]
    pub exec: bool,
    /// 允许发系统通知
    #[serde(default)]
    pub notify: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Limits {
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "default_memory")]
    pub memory_mb: usize,
}

fn default_timeout() -> u64 {
    3000
}
fn default_memory() -> usize {
    32
}

/// 插件声明的 UI 扩展点（M7，the agent harness 式：插件只声明、不注入代码）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UiDecl {
    #[serde(default)]
    pub pages: Vec<UiPage>,
    #[serde(default)]
    pub actions: Vec<UiAction>,
    #[serde(default)]
    pub settings: Vec<UiSetting>,
    #[serde(default)]
    pub commands: Vec<UiCommand>,
    #[serde(default)]
    pub theme: Option<UiTheme>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UiPage {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub icon: String,
    /// 非空时：trusted 模式下用本页替换内置页（tasks / add / settings）。
    #[serde(default)]
    pub replaces: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UiAction {
    pub id: String,
    pub label: String,
    /// toolbar | task
    #[serde(default)]
    pub place: String,
    #[serde(default)]
    pub icon: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UiSetting {
    pub key: String,
    pub label: String,
    /// boolean | string | number | select
    #[serde(default = "default_setting_kind")]
    pub kind: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub default: String,
}

fn default_setting_kind() -> String {
    "string".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct UiCommand {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UiTheme {
    #[serde(default)]
    pub accent: String,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout_ms: default_timeout(),
            memory_mb: default_memory(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    pub api_version: u32,
    pub entry: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub hooks: Vec<HookDecl>,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub ui: UiDecl,
    /// 首次加载时是否默认启用（缺省 true，保持老 manifest 行为不变）。
    ///
    /// 示例插件（`plugins/` 下那几个）都写 false：它们和本体功能重叠
    /// —— 最典型的是 `notify-on-complete` 和本体的"下载完成"通知，
    /// 两个都开就会给同一个文件发两条提醒（用户 m12538 报的就是这个）。
    #[serde(default = "default_true")]
    pub enabled_by_default: bool,
}

fn default_true() -> bool {
    true
}

fn default_kind() -> String {
    "quickjs".to_string()
}

#[derive(Debug, Clone)]
pub struct ManifestError(pub String);

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ManifestError {}

impl Manifest {
    pub fn load(path: &std::path::Path) -> Result<Self, ManifestError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ManifestError(format!("读不到 {}: {e}", path.display())))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let m: Manifest =
            toml::from_str(text).map_err(|e| ManifestError(format!("manifest 解析失败: {e}")))?;
        m.validate()?;
        Ok(m)
    }

    fn validate(&self) -> Result<(), ManifestError> {
        if self.id.trim().is_empty() {
            return Err(ManifestError("manifest.id 不能为空".into()));
        }
        if self.api_version != API_VERSION {
            return Err(ManifestError(format!(
                "api_version={} 与本机 ADL 支持的 {} 不匹配，拒绝加载",
                self.api_version, API_VERSION
            )));
        }
        if self.kind != "quickjs" {
            return Err(ManifestError(format!(
                "kind={} 暂不支持（M3 只支持 quickjs）",
                self.kind
            )));
        }
        if self.entry.trim().is_empty() {
            return Err(ManifestError("manifest.entry 不能为空".into()));
        }
        Ok(())
    }

    /// 解析出的 hook 订阅（point, priority），跳过无法识别的点。
    pub fn hook_subscriptions(&self) -> Vec<(HookPoint, i64)> {
        let mut v = Vec::new();
        for h in &self.hooks {
            match HookPoint::parse(&h.point) {
                Some(p) => v.push((p, h.priority)),
                None => eprintln!(
                    "[plugin:{}] 未知 hook 点 '{}'，已忽略",
                    self.id, h.point
                ),
            }
        }
        v
    }

    /// 有效订阅集合：按运行模式过滤掉需要 Transform 的 hook（safe 模式只放 Observe+Guard）。
    pub fn active_hooks(&self, trusted: bool) -> HashSet<HookPoint> {
        self.hook_subscriptions()
            .into_iter()
            .filter(|(p, _)| trusted || !p.needs_transform())
            .map(|(p, _)| p)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
id = "com.adl.test"
name = "Test"
version = "1.0.0"
api_version = 1
entry = "main.js"
kind = "quickjs"

[[hooks]]
point = "onBeforeRequest"
priority = 50

[[hooks]]
point = "onComplete"
priority = 10

[permissions]
notify = true
fs_read = ["E:/dl"]
"#;

    #[test]
    fn parses_sample() {
        let m = Manifest::parse(SAMPLE).unwrap();
        assert_eq!(m.id, "com.adl.test");
        assert_eq!(m.hook_subscriptions().len(), 2);
        assert!(m.permissions.notify);
        assert_eq!(m.permissions.fs_read, vec!["E:/dl".to_string()]);
        assert_eq!(m.limits.timeout_ms, 3000);
    }

    #[test]
    fn rejects_wrong_api_version() {
        let bad = SAMPLE.replace("api_version = 1", "api_version = 99");
        assert!(Manifest::parse(&bad).is_err());
    }

    #[test]
    fn hook_point_parsing_is_lenient() {
        assert_eq!(HookPoint::parse("onBeforeRequest"), Some(HookPoint::BeforeRequest));
        assert_eq!(HookPoint::parse("on_before_request"), Some(HookPoint::BeforeRequest));
        assert_eq!(HookPoint::parse("BeforeRequest"), Some(HookPoint::BeforeRequest));
        assert_eq!(HookPoint::parse("onComplete"), Some(HookPoint::Complete));
        assert_eq!(HookPoint::parse("nope"), None);
    }

    #[test]
    fn safe_mode_filters_transform_hooks() {
        let m = Manifest::parse(SAMPLE).unwrap();
        let safe = m.active_hooks(false);
        assert!(!safe.contains(&HookPoint::BeforeRequest));
        assert!(safe.contains(&HookPoint::Complete));
        let trusted = m.active_hooks(true);
        assert!(trusted.contains(&HookPoint::BeforeRequest));
    }
}
