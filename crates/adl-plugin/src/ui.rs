//! 插件 UI 扩展点的聚合层（M7）。
//!
//! 只做「把各插件在 manifest 里声明的 UI + 它们的设置值，汇总成一个 JSON 描述」。
//! 真正的组件树由插件在 `onUiRender()` 时返回（前端用白名单渲染器画），
//! 这里不产生任何 HTML/JS。
//!
//! 分级（安全模式的基础）：`safe` 只暴露页面（只读展示）与设置项，**不暴露**
//! 动作/命令/替换内置页；`trusted` 才暴露全部（含「替换内置 UI」）。

use crate::runtime::LoadedPlugin;
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// plugin_id -> (setting_key -> value)
pub type SettingsValues = BTreeMap<String, BTreeMap<String, String>>;

fn setting_value(values: &SettingsValues, plugin: &str, key: &str, default: &str) -> String {
    values
        .get(plugin)
        .and_then(|m| m.get(key))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

/// manifest 里声明的全部 hook 名（`onResolve` 这种规范写法）。
fn hook_names(p: &LoadedPlugin) -> Vec<String> {
    p.manifest
        .hook_subscriptions()
        .into_iter()
        .map(|(hp, _)| hp.canonical().to_string())
        .collect()
}

/// 当前模式下**实际生效**的 hook 名。
///
/// 和 `hook_names` 分开是因为 safe 模式会过滤掉需要 Transform 的点：
/// 插件页要能告诉用户"你声明了 onPlan，但安全模式下它没跑"。
fn active_hook_names(p: &LoadedPlugin) -> Vec<String> {
    let mut v: Vec<String> = p
        .manifest
        .active_hooks(p.mode.trusted())
        .into_iter()
        .map(|hp| hp.canonical().to_string())
        .collect();
    v.sort();
    v
}

pub fn collect(plugins: &[LoadedPlugin], values: &SettingsValues, trusted: bool) -> Value {
    let mut pages = Vec::new();
    let mut actions = Vec::new();
    let mut settings = Vec::new();
    let mut commands = Vec::new();
    let mut replace = serde_json::Map::new();
    let mut with_ui = Vec::new();
    let mut all = Vec::new();
    let mut theme = serde_json::Map::new();

    for p in plugins {
        let ui = &p.manifest.ui;
        let has_ui = !ui.pages.is_empty()
            || !ui.actions.is_empty()
            || !ui.settings.is_empty()
            || !ui.commands.is_empty();

        // 「全部插件」清单：常驻插件页要显示**没声明 UI 的插件**（它们正是
        // 干活的那批，比如 ua-fixer / block-html），所以这一份必须在下面的
        // `continue` 之前收集，不能复用 `with_ui`。
        all.push(json!({
            "id": p.id,
            "name": p.manifest.name,
            "version": p.manifest.version,
            "description": p.manifest.description,
            "api_version": p.manifest.api_version,
            "kind": p.manifest.kind,
            "dir": p.dir.display().to_string(),
            "trusted": p.mode.trusted(),
            "has_ui": has_ui,
            "hooks": hook_names(p),
            "active_hooks": active_hook_names(p),
            "pages": ui.pages.len(),
            "actions": ui.actions.len(),
            "settings": ui.settings.len(),
            "commands": ui.commands.len(),
            "permissions": json!({
                "net": p.manifest.permissions.net,
                "exec": p.manifest.permissions.exec,
                "notify": p.manifest.permissions.notify,
                "fs_read": p.manifest.permissions.fs_read,
                "fs_write": p.manifest.permissions.fs_write,
            }),
        }));

        if !has_ui {
            continue;
        }
        with_ui.push(json!(p.id));

        for page in &ui.pages {
            pages.push(json!({
                "plugin": p.id,
                "id": page.id,
                "title": page.title,
                "icon": page.icon,
                "replaces": page.replaces,
            }));
            if trusted && !page.replaces.is_empty() {
                replace.insert(
                    page.replaces.clone(),
                    json!({ "plugin": p.id, "page": page.id }),
                );
            }
        }

        if trusted {
            for a in &ui.actions {
                actions.push(json!({
                    "plugin": p.id,
                    "id": a.id,
                    "label": a.label,
                    "place": if a.place.is_empty() { "toolbar" } else { &a.place },
                    "icon": a.icon,
                }));
            }
            for c in &ui.commands {
                commands.push(json!({
                    "plugin": p.id,
                    "id": c.id,
                    "title": c.title,
                }));
            }
        }

        for s in &ui.settings {
            settings.push(json!({
                "plugin": p.id,
                "key": s.key,
                "label": s.label,
                "kind": if s.kind.is_empty() { "string" } else { &s.kind },
                "options": s.options,
                "default": s.default,
                "value": setting_value(values, &p.id, &s.key, &s.default),
                "editable": trusted,
            }));
        }

        if let Some(th) = &ui.theme {
            if !th.accent.is_empty() {
                theme.insert(p.id.clone(), json!(th.accent));
            }
        }
    }

    json!({
        "trusted": trusted,
        "plugins": with_ui,
        // 全部已加载插件（含没有 UI 的）。侧栏「插件」分组用 `plugins`，
        // 常驻插件页用 `all` —— 后者才是"装了哪些东西"的完整视图。
        "all": all,
        "pages": pages,
        "actions": actions,
        "settings": settings,
        "commands": commands,
        "replace": Value::Object(replace),
        "theme": Value::Object(theme),
    })
}
