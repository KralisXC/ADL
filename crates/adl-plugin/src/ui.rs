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

pub fn collect(plugins: &[LoadedPlugin], values: &SettingsValues, trusted: bool) -> Value {
    let mut pages = Vec::new();
    let mut actions = Vec::new();
    let mut settings = Vec::new();
    let mut commands = Vec::new();
    let mut replace = serde_json::Map::new();
    let mut with_ui = Vec::new();
    let mut theme = serde_json::Map::new();

    for p in plugins {
        let ui = &p.manifest.ui;
        let has_ui = !ui.pages.is_empty()
            || !ui.actions.is_empty()
            || !ui.settings.is_empty()
            || !ui.commands.is_empty();
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
        "pages": pages,
        "actions": actions,
        "settings": settings,
        "commands": commands,
        "replace": Value::Object(replace),
        "theme": Value::Object(theme),
    })
}
