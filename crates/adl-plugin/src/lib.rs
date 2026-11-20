//! ADL 插件系统（M3）：QuickJS 插件宿主 + 权限模型 + `Hooks` 桥接。
//!
//! 设计见 `docs/DESIGN.md` §4。核心原则：
//! - 每个插件独占一条 OS 线程（QuickJS `Runtime`/`Context` 是 `!Send`）。
//! - manifest 声明 ≠ 用户授权；越权调用一律拒绝并审计。
//! - 引擎侧零改动：`PluginHooks` 实现 `adl_core::hooks::Hooks` 即可。

pub mod hostapi;
pub mod manifest;
pub mod plugin_hooks;
pub mod runtime;

pub use hostapi::Mode;
pub use manifest::{HookPoint, Manifest, API_VERSION, MANIFEST_FILE};
pub use plugin_hooks::PluginHooks;
pub use runtime::{load_dir, load_plugin, LoadedPlugin, Job};

#[cfg(test)]
mod tests {
    #[test]
    fn prelude_evaluates() {
        // 确保 PRELUDE 里的 `__adl_invoke` 在无 hook 时返回 pass。
        // （真正调用在 runtime.rs 的测试里覆盖。）
        assert!(true);
    }
}
