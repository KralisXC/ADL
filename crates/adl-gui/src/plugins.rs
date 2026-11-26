//! 插件页：已安装清单、启用开关、单个插件的设置页。
//!
//! 从 `main.rs` 拆出来（用户 m14683：「我们的插件页面也可以从 main 里面单独
//! 拎出来变成一个 rs」）。和 `terminal.rs` 同一个路数：自由函数放上面，
//! `impl App` 块放下面，`main.rs` 只留 `mod plugins;`。
//!
//! 这里还带了两件跟插件管理绑得很紧的东西：`plugins_root`（目录解析）和
//! `disabled_plugin_entries`（停用的插件不在快照里，得扫目录补回来，否则
//! 停用一次就再也点不到「启用」）。

use super::*;
use crate::market;


/// 插件页的两个分段（对应 DSH 的"已安装 / 插件市场"）。
///
/// "插件市场"目前只有骨架：用户说以后要接自己的 GitHub 插件源，
/// 先把入口和文案占住，免得以后加进来时整页要重排。
/// （原先还有"发现"，用户 m11341 说用不上，已删除。）
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PluginsTab {
    Installed,
    Market,
}

// ------------------------- helper functions -------------------------

/// 判断一条插件记录是不是"软件自带"的。
///
/// 内置插件的 id 统一是 `com.adl.xxx`（见 `plugins/` 下各 plugin.toml）。
/// 用户自己装的插件就算起名也带 com.adl 前缀，也是他们自己的责任；
/// 这里只按约定前缀分组，目的是在插件页把"程序自带功能"和"用户安装"
/// 分开显示 —— 前者不可在插件页调整。
pub(crate) fn is_builtin(v: &Value) -> bool {
    v.get("id")
        .and_then(|x| x.as_str())
        .map(|id| id.starts_with("com.adl."))
        .unwrap_or(false)
}

/// 从 `ui_snapshot["all"][i]["hooks"]` 这类 JSON 数组里取字符串列表。
fn str_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// 把 `permissions` 对象翻成 [(显示名, 是否声明)]，空的权限不显示。
fn permission_labels(v: Option<&Value>) -> Vec<(String, bool)> {
    let Some(obj) = v.and_then(|x| x.as_object()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (key, name) in [
        ("net", "网络"),
        ("exec", "执行"),
        ("notify", "通知"),
    ] {
        if obj.get(key).and_then(|x| x.as_bool()).unwrap_or(false) {
            out.push((name.to_string(), true));
        }
    }
    // 文件权限是数组，非空才算声明过。
    for (key, name) in [("fs_read", "读文件"), ("fs_write", "写文件")] {
        let n = obj
            .get(key)
            .and_then(|x| x.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        if n > 0 {
            out.push((format!("{name}×{n}"), true));
        }
    }
    out
}

/// 插件根目录：**永远**是 exe 同级的 `plugins/`。
///
/// 用户 m19629：「插件储存位置还是直接固定吧（放在 exe 同根目录的 plugins
/// 文件夹（每个插件记得分类））」。以前这里允许配置里改一个 `plugin_dir`，
/// 结果同一个软件在两台机器上插件放在完全不同的地方，排错时还得先问
/// "你的插件目录指哪儿了"。现在固定下来，每个插件占一个子目录，名字就是
/// manifest 里的 id（安装器与卸载都按这个走）。
pub(crate) fn plugins_root() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("plugins")))
}

/// 补上「已停用」的插件条目。
///
/// 快照里的 `all` 来自**已加载**的插件，而 `build_hooks` 在加载阶段就把停用的
/// 插件过滤掉了 —— 于是停用一次，这个插件就会从清单里彻底消失，再也点不到
/// 「启用」。这里直接扫目录读 `plugin.toml`，把清单里没有的补成停用条目。
fn disabled_plugin_entries(root: Option<&PathBuf>, known: &[String]) -> Vec<Value> {
    let Some(root) = root else {
        return Vec::new();
    };
    let Ok(rd) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let dir = e.path();
        if !dir.is_dir() {
            continue;
        }
        let manifest = dir.join(adl_plugin::MANIFEST_FILE);
        if !manifest.is_file() {
            continue;
        }
        // 坏 manifest 直接跳过：这个页面只是清单，不该因为一个坏插件整页报错。
        let Ok(m) = adl_plugin::Manifest::load(&manifest) else {
            continue;
        };
        if known.iter().any(|k| k == &m.id) {
            continue;
        }
        let mut hooks: Vec<String> = m
            .hook_subscriptions()
            .into_iter()
            .map(|(hp, _)| hp.canonical().to_string())
            .collect();
        hooks.sort();
        let has_ui = !(m.ui.pages.is_empty()
            && m.ui.actions.is_empty()
            && m.ui.settings.is_empty()
            && m.ui.commands.is_empty());
        out.push(json!({
            "id": m.id,
            "name": m.name,
            "version": m.version,
            "description": m.description,
            "api_version": m.api_version,
            "kind": m.kind,
            "dir": dir.display().to_string(),
            "trusted": false,
            "has_ui": has_ui,
            // 停用的插件也照常带上缺省值：列表里那一行的开关要能算对
            // "下次清掉配置后该是开还是关"。
            "enabled_by_default": m.enabled_by_default,
            "hooks": hooks,
            // 停用的插件没被加载，谈不上"生效的 hook"，留空即可。
            "active_hooks": Vec::<String>::new(),
            "pages": m.ui.pages.len(),
            "actions": m.ui.actions.len(),
            "settings": m.ui.settings.len(),
            "commands": m.ui.commands.len(),
            "permissions": json!({
                "net": m.permissions.net,
                "exec": m.permissions.exec,
                "notify": m.permissions.notify,
                "fs_read": m.permissions.fs_read,
                "fs_write": m.permissions.fs_write,
            }),
        }));
    }
    out
}

// ------------------------- pages -------------------------

impl App {
    /// 常驻的「插件」页：装了什么、启用没启用、声明了哪些 hook。
    ///
    /// 这一版只做骨架（用户原话"先做大概，不用全部功能做了先"）：能看清单、
    /// 能开关、能打开插件目录、能刷新。**没有**在线安装/卸载/更新。
    pub(crate) fn page_plugins(&mut self, ui: &mut egui::Ui) {
        ui.add_space(2.0);

        let mut all: Vec<Value> = self
            .plugin_snapshot
            .get("all")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();

        // 内置功能在插件页里"假装自己是插件"：终端没有 plugin.toml，也不会
        // 走 QuickJS，但它是程序自带的能力，放在「内置插件」组里能让人一眼
        // 看全 ADL 有哪些自带功能，不用去用户装的插件里翻。
        //
        // id 用 `com.adl.` 前缀，于是 `is_builtin()` 自然把它分到内置组，
        // 也就自动没有右侧开关（终端是页面，不是能开关的东西）。
        all.push(json!({
            "id": "com.adl.terminal",
            "name": "终端",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "仿 PowerShell 的交互式命令行：敲 help 看命令，Tab 补全、↑/↓ 翻历史。",
            "has_ui": true,
            // 终端不是 QuickJS 插件，没有"信任模式"这回事；给 false 让它
            // 和别的内置插件一样显示「安全模式」，不会多出一个容易误读的标。
            "trusted": false,
            "hooks": Vec::<String>::new(),
            "active_hooks": Vec::<String>::new(),
            "permissions": json!({
                "net": false, "exec": false, "notify": false,
                "fs_read": Vec::<String>::new(), "fs_write": Vec::<String>::new()
            }),
            "enabled_by_default": true,
        }));

        // 顶部：目录位置 + 两个动作
        let root = plugins_root();

        // 停用的插件不在快照里（没被加载），得从目录补回来，否则停用一次
        // 就再也点不到「启用」了。
        {
            let known: Vec<String> = all
                .iter()
                .filter_map(|p| p.get("id").and_then(|x| x.as_str()).map(String::from))
                .collect();
            all.extend(disabled_plugin_entries(root.as_ref(), &known));

            // 启用的排前面，同组按 id 排，列表才不会因为开关一次就跳动。
            let enabled_map: Vec<bool> = {
                let cfg = self.gui.cfg.lock().unwrap();
                all.iter()
                    .map(|p| {
                        let id = p.get("id").and_then(|x| x.as_str()).unwrap_or("");
                        cfg.plugin_enabled(id)
                    })
                    .collect()
            };
            let mut idx: Vec<usize> = (0..all.len()).collect();
            idx.sort_by(|&a, &b| {
                let id = |i: usize| {
                    all[i]
                        .get("id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string()
                };
                (!enabled_map[a], id(a)).cmp(&(!enabled_map[b], id(b)))
            });
            all = idx.into_iter().map(|i| all[i].clone()).collect();
        }
        let mut open_dir = false;
        let mut refresh = false;
        let mut open_market_cfg = false;
        // 从 zip 装插件：文件选择对话框是模态的，必须丢子线程，否则整个
        // 窗口冻住（和 `pick_folder` 一个道理）。
        let mut pick_zip = false;

        // ── 顶部：说明 + 右侧刷新 / 从压缩包安装 ──
        //
        // 「从压缩包安装」对的是插件分发形态：扩展中心把插件打成 zip 传
        // 到 GitHub Releases，用户下回来点这里装。装完压缩包会被删掉。
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("安装、启用和配置插件")
                    .small()
                    .color(muted()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("刷新").clicked() {
                    refresh = true;
                }
                if ui.button("从压缩包安装…").clicked() {
                    pick_zip = true;
                }
            });
        });
        ui.add_space(10.0);

        // ── 两段切换 ──
        let tabs = [
            (PluginsTab::Installed, "已安装"),
            (PluginsTab::Market, "插件市场"),
        ];
        let mut enter_market = false;
        if let Some(t) = adl_ui::widgets::segmented(ui, "plugins_tab", self.plugins_tab, &tabs) {
            // 切进「插件市场」时自动拉一次清单：市场页本来就是"给我看有什么可装的"，
            // 进来还空着让人再点一次「刷新清单」是多余的。只在这一刻触发，
            // 不每帧拉（否则每帧都打 GitHub）。
            enter_market = t == PluginsTab::Market && self.plugins_tab != t;
            self.plugins_tab = t;
        }
        if enter_market && self.market_rx.is_none() {
            self.market_note = None;
            self.market_rx = Some(market::start_fetch(&self.gui, self.market_url.clone()));
        }
        ui.add_space(12.0);

        match self.plugins_tab {
            PluginsTab::Installed => {
        adl_ui::widgets::card(ui, |ui| {
            adl_ui::widgets::section_title(ui, "插件目录");
            ui.add_space(4.0);
            // 没有插件目录时 build_hooks 会退化成空集合，这里如实说清楚
            // 目录在哪（也可能是"还没建出来"）。
            let shown = root
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "（未配置）".to_string());
            ui.label(
                egui::RichText::new(shown)
                    .font(egui::FontId::monospace(12.5))
                    .color(pal().ink),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("打开插件目录").clicked() {
                    open_dir = true;
                }
                if ui.button("重新扫描").clicked() {
                    refresh = true;
                }
            });
            ui.add_space(6.0);
            adl_ui::widgets::hint(
                ui,
                "把插件文件夹（含 plugin.toml）放进这个目录，再点「重新扫描」就会出现在下面。\
                 插件只在启动/重扫时加载，运行中的任务用不上新加载的插件。",
            );
        });

        if open_dir {
            if let Some(dir) = root.as_ref() {
                match platform::open_path(dir) {
                    Ok(()) => {}
                    Err(e) => self.set_status(format!("打开目录失败: {e}"), true),
                }
            }
        }
        if refresh {
            // 重建 hooks + 让快照立刻失效，否则页面要等 1.2 秒才更新。
            let new_cfg = self.gui.cfg.lock().unwrap().clone();
            *self.gui.hooks.lock().unwrap() = build_hooks(&new_cfg);
            self.plugin_snapshot_at = Instant::now() - Duration::from_secs(3600);
            self.set_status("已重新扫描插件目录", false);
        }
        if pick_zip {
            match root.as_ref() {
                Some(dir) => {
                    // 整条链路（选文件 + 解包）丢子线程，结果从 `zip_rx` 回来，
                    // 那边顺带重建 hooks。
                    self.zip_rx = Some(start_install_zip(dir.clone()));
                    self.set_status("正在安装插件…", false);
                }
                None => self.set_status("还没有插件目录，先用「打开插件目录」建一个", true),
            }
        }

        ui.add_space(14.0);

        if all.is_empty() {
            adl_ui::widgets::empty_state(
                ui,
                "还没装任何插件。内置的 ua-fixer / auto-concurrency / notify-on-complete / \
                 block-html 在程序目录的 plugins/ 下。",
            );
            return;
        }

        // 启停要等所有卡片画完再落盘：卡片闭包里已经借了 self.gui 的锁，
        // 在闭包里写配置容易死锁（同一把 Mutex 重复 lock）。
        let mut toggle: Option<(String, bool)> = None;
        // 卸载同理：先记下 id，卡片全部画完再动配置和磁盘。
        let mut uninstall: Option<String> = None;

        // 分两组显示：内置插件是软件自带的功能（id 都以 com.adl. 开头），
        // 在插件页里只能看、不能关 —— 关掉 ua-fixer 之类的没有意义，
        // 只会让人以为程序坏了。用户自己装的才给开关。
        let builtin: Vec<Value> = all.iter().filter(|p| is_builtin(p)).cloned().collect();
        let external: Vec<Value> = all.iter().filter(|p| !is_builtin(p)).cloned().collect();

        for (group, list, can_toggle) in [
            ("内置插件", &builtin, false),
            ("已安装插件", &external, true),
        ] {
            if list.is_empty() {
                continue;
            }
            adl_ui::widgets::section_title(ui, group);
            ui.add_space(2.0);
            adl_ui::widgets::hint(
                ui,
                if can_toggle {
                    "你自己放进插件目录的插件，可以随时启用或停用。"
                } else {
                    "程序自带的功能，已经按默认调好，这里只做展示。"
                },
            );
            ui.add_space(8.0);
            for p in list {
                let id = p.get("id").and_then(|x| x.as_str()).unwrap_or("");
                if id.is_empty() {
                    continue;
                }
                let name = p.get("name").and_then(|x| x.as_str()).unwrap_or(id);
                let version = p.get("version").and_then(|x| x.as_str()).unwrap_or("");
                let desc = p.get("description").and_then(|x| x.as_str()).unwrap_or("");
                let has_ui = p.get("has_ui").and_then(|x| x.as_bool()).unwrap_or(false);
                let trusted = p.get("trusted").and_then(|x| x.as_bool()).unwrap_or(false);
                let enabled = {
                    let d = p
                        .get("enabled_by_default")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(true);
                    self.gui.cfg.lock().unwrap().plugin_enabled_or(id, d)
                };
                let hooks = str_list(p.get("hooks"));
                let active = str_list(p.get("active_hooks"));
                // 权限是"声明"，不等于授权：safe 模式下 Transform 类 hook 不生效，
                // 所以同时显示"声明了几个"和"当前生效几个"。
                let perms = permission_labels(p.get("permissions"));

                adl_ui::widgets::card(ui, |ui| {
                    ui.horizontal(|ui| {
                        // 必须显式给色：`.strong()` 走 `visuals.strong_text_color()`，
                        // 也就是 `widgets.active.text_color()` —— 我们的主题把 active 设为
                        // 白色（青绿按钮上要白字），于是不带 `.color()` 的 strong 文本
                        // 会画成白字，在白卡上整行消失。
                        let title_color = if enabled { ink() } else { muted() };
                        ui.label(
                            egui::RichText::new(name)
                                .size(14.5)
                                .strong()
                                .color(title_color),
                        );
                        if !version.is_empty() {
                            ui.label(
                                egui::RichText::new(format!("v{version}"))
                                    .small()
                                    .color(muted()),
                            );
                        }
                        ui.add_space(6.0);
                        if !enabled {
                            adl_ui::widgets::badge(ui, "已停用", pal().muted);
                        } else if trusted {
                            adl_ui::widgets::badge(ui, "信任模式", pal().warn);
                        } else {
                            adl_ui::widgets::badge(ui, "安全模式", pal().accent);
                        }
                        if has_ui {
                            adl_ui::widgets::badge(ui, "有界面", pal().accent);
                        }
                        // 开关放最右：和 DSH 的插件页一致（名称在左、开关在右），
                        // 一排扫下去就能看出哪些开着。
                        // 内置插件照常显示整张卡片，只是右边**什么都不画** ——
                        // 原来这里放一个「内置」badge，用户 m11933 说看着像
                        // 另一种东西，不如让它们和普通插件长一样、单纯没有开关。
                        if can_toggle {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if adl_ui::widgets::switch(ui, enabled).clicked() {
                                        toggle = Some((id.to_string(), !enabled));
                                    }
                                },
                            );
                        }
                    });

                    if !desc.is_empty() {
                        ui.add_space(3.0);
                        ui.label(
                            egui::RichText::new(desc)
                                .size(12.5)
                                .color(if enabled { pal().ink } else { muted() }),
                        );
                    }

                    ui.add_space(6.0);
                    // hook 行：生效的用主色，被安全模式挡住的用灰色并标注。
                    if !hooks.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            ui.label(egui::RichText::new("hook").small().color(muted()));
                            for h in &hooks {
                                let on = active.iter().any(|a| a == h);
                                let short = h.trim_start_matches("on");
                                if on {
                                    adl_ui::widgets::badge(ui, short, pal().accent);
                                } else {
                                    adl_ui::widgets::badge(ui, short, pal().muted);
                                }
                            }
                        });
                    }
                    if !perms.is_empty() {
                        ui.add_space(4.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            ui.label(egui::RichText::new("权限").small().color(muted()));
                            for (label, on) in &perms {
                                if *on {
                                    adl_ui::widgets::badge(ui, label, pal().warn);
                                } else {
                                    adl_ui::widgets::badge(ui, label, pal().muted);
                                }
                            }
                        });
                    }

                    // 卸载：只对用户自己装的插件开放（内置的删了程序就残了）。
                    // 点下去**不会立刻删目录** —— 用户 m15179 明确要求等重启，
                    // 怕它此刻还在跑。这里只打标记，下次启动统一清。
                    if can_toggle {
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui
                                .small_button("卸载…")
                                .on_hover_text("立刻停用，下次启动 ADL 时删除插件目录")
                                .clicked()
                            {
                                uninstall = Some(id.to_string());
                            }
                            adl_ui::widgets::hint(
                                ui,
                                "卸载后立即停用，插件目录会在下次启动时删除。",
                            );
                        });
                    }

                });
                ui.add_space(8.0);
            }
            ui.add_space(4.0);
        }

        if let Some((id, want)) = toggle {
            let mut cfg = self.gui.cfg.lock().unwrap();
            cfg.plugins
                .insert(id.clone(), adl_app::config::PluginCfg { enabled: want });
            if let Err(e) = cfg.save() {
                drop(cfg);
                self.set_status(format!("保存插件开关失败: {e}"), true);
            } else {
                let new_cfg = cfg.clone();
                drop(cfg);
                *self.gui.hooks.lock().unwrap() = build_hooks(&new_cfg);
                self.plugin_snapshot_at = Instant::now() - Duration::from_secs(3600);
                self.set_status(
                    if want {
                        format!("已启用插件 {id}")
                    } else {
                        format!("已停用插件 {id}")
                    },
                    false,
                );
            }
        }

        // 卸载：立刻停用 + 记进 `pending_uninstall`，磁盘等下次启动再动。
        if let Some(id) = uninstall {
            let mut cfg = self.gui.cfg.lock().unwrap();
            // 停用 + 记标记：停用让它这一轮就不再跑，标记让下次启动删目录。
            cfg.plugins
                .insert(id.clone(), adl_app::config::PluginCfg { enabled: false });
            if !cfg.pending_uninstall.iter().any(|x| x == &id) {
                cfg.pending_uninstall.push(id.clone());
            }
            if let Err(e) = cfg.save() {
                drop(cfg);
                self.set_status(format!("保存卸载标记失败: {e}"), true);
            } else {
                let new_cfg = cfg.clone();
                drop(cfg);
                // 重建 hooks：这个插件从这一刻起就不在进程里了。
                *self.gui.hooks.lock().unwrap() = build_hooks(&new_cfg);
                self.plugin_snapshot_at = Instant::now() - Duration::from_secs(3600);
                self.set_status(
                    format!("已卸载插件 {id}（目录将在下次启动时删除）"),
                    false,
                );
            }
        }
            }
            PluginsTab::Market => {
                // 市场：从 `index.json` 拉清单，点「安装」就地下载 + 校验 sha256
                // + 解包。装的链路复用 `install::install_zip`，所以"装完删包"、
                // zip slip 防护、"目标目录已存在就拒"这些行为跟本地装包一致。
                let mut fetch_now = false;
                let mut install_id: Option<String> = None;

                adl_ui::widgets::card(ui, |ui| {
                    adl_ui::widgets::section_title(ui, "插件市场");
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(&self.market_url)
                            .font(egui::FontId::monospace(12.0))
                            .color(muted()),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let busy = self.market_rx.is_some();
                        let label = if busy { "正在拉取…" } else { "刷新清单" };
                        if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                            fetch_now = true;
                        }
                        if ui.button("换源…").clicked() {
                            open_market_cfg = true;
                        }
                    });
                    ui.add_space(6.0);
                    adl_ui::widgets::hint(
                        ui,
                        "清单来自扩展中心仓库的 index.json。每个包都会比对 sha256，\
                         对不上就丢弃、绝不解包。",
                    );
                    if let Some((msg, err)) = &self.market_note {
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(msg)
                                .size(12.0)
                                .color(if *err { pal().danger } else { muted() }),
                        );
                    }
                });

                if fetch_now {
                    self.market_note = None;
                    self.market_rx = Some(market::start_fetch(&self.gui, self.market_url.clone()));
                }

                ui.add_space(12.0);
                if self.market_entries.is_empty() {
                    let msg = if self.market_rx.is_some() {
                        "正在从扩展中心拉取插件清单…"
                    } else {
                        "还没有清单。点「刷新清单」从扩展中心获取。"
                    };
                    adl_ui::widgets::empty_state(ui, msg);
                } else {
                    // 已经装了的 id：市场行上显示「已安装」、按钮不出现，
                    // 否则用户会反复点、每次都被"目录已存在"拒一次。
                    let installed: Vec<String> = {
                        let cfg = self.gui.cfg.lock().unwrap();
                        let mut v: Vec<String> = cfg.plugins.keys().cloned().collect();
                        v.extend(cfg.pending_uninstall.iter().cloned());
                        v
                    };
                    let entries = self.market_entries.clone();
                    for e in &entries {
                        let is_installed = installed.iter().any(|x| x == &e.id);
                        let installing = self.market_installing.as_deref() == Some(e.id.as_str());
                        adl_ui::widgets::card(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(if e.name.is_empty() {
                                        e.id.as_str()
                                    } else {
                                        e.name.as_str()
                                    })
                                    .strong()
                                    .color(pal().ink),
                                );
                                ui.label(
                                    egui::RichText::new(format!("v{}", e.version))
                                        .small()
                                        .color(muted()),
                                );
                                if e.needs_trusted {
                                    adl_ui::widgets::badge(ui, "需信任模式", pal().warn);
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if is_installed {
                                            adl_ui::widgets::badge(ui, "已安装", pal().accent);
                                        } else if installing {
                                            ui.label(
                                                egui::RichText::new("安装中…").small().color(muted()),
                                            );
                                        } else if ui
                                            .add_enabled(
                                                self.market_installing.is_none(),
                                                egui::Button::new("安装"),
                                            )
                                            .clicked()
                                        {
                                            install_id = Some(e.id.clone());
                                        }
                                    },
                                );
                            });
                            if !e.description.is_empty() {
                                ui.add_space(3.0);
                                ui.label(
                                    egui::RichText::new(&e.description)
                                        .size(12.5)
                                        .color(pal().ink),
                                );
                            }
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} · {:.1} MB · API {}+",
                                    e.id, e.size_mb, e.min_api
                                ))
                                .small()
                                .color(muted()),
                            );
                        });
                    }

                    if let Some(id) = install_id {
                        let entry = entries.iter().find(|e| e.id == id).cloned();
                        if let (Some(entry), Some(dir)) = (entry, root.clone()) {
                            self.market_installing = Some(id);
                            self.market_install_rx =
                                Some(market::start_install(&self.gui, entry, dir));
                        } else if root.is_none() {
                            self.set_status("还没有插件目录，先用「打开插件目录」建一个", true);
                        }
                    }
                }
            }
        }

        if open_market_cfg {
            // 「换源…」：市场源地址得在设置页改（那里才有输入框），
            // 直接把用户送过去，别在这儿留一个点了没反应的按钮。
            self.tab = Tab::Settings;
            self.set_status("在「设置」里可以改插件市场索引地址", false);
        }
    }

    /// 终端页：一个能留历史的命令行（取代原来的 Ctrl+K 浮层命令面板）。
    ///
    /// 为什么做成页面而不是浮层：浮层关掉就没了，想翻上一条命令的输出只能
    /// 重跑；而且用户明确说想要"终端那种（win 的 powershell）"。
    pub(crate) fn page_plugin(&mut self, ui: &mut egui::Ui) {
        let (plugin, page) = match self.tab.clone() {
            Tab::Plugin(p, g) => (p, g),
            _ => return,
        };

        // 侧栏"插件设置"入口用的是伪页面 `adl` + `settings:<pluginId>`：
        // 它不是插件自己渲染的页，而是宿主把该插件的 settings 项单独列出来。
        if plugin == "adl" {
            if let Some(pid) = page.strip_prefix("settings:") {
                self.page_plugin_settings(ui, pid);
                return;
            }
        }

        ui.add_space(2.0);
        let mut dispatch: Vec<Dispatch> = Vec::new();
        if self.plugin_view.is_none() {
            self.reload_plugin_view();
        }
        if let Some(tree) = self.plugin_view.clone() {
            if tree.get("type").and_then(|x| x.as_str()) == Some("error") {
                let msg = tree.get("error").and_then(|x| x.as_str()).unwrap_or("插件出错");
                ui.colored_label(egui::Color32::from_rgb(0xB0, 0x30, 0x30), msg);
            } else {
                render_node(ui, &plugin, &tree, &mut dispatch);
            }
        } else {
            ui.label("（插件没有返回内容）");
        }
        for d in dispatch {
            match d {
                Dispatch::Action { plugin, action } => self.run_action(&plugin, &action),
                Dispatch::Setting { plugin, key, value } => self.save_plugin_setting(&plugin, &key, &value),
            }
        }
    }

    /// 单个插件的设置页（侧栏入口点进来的那个）。
    pub(crate) fn page_plugin_settings(&mut self, ui: &mut egui::Ui, plugin_id: &str) {
        ui.add_space(2.0);
        let settings: Vec<Value> = self
            .plugin_snapshot
            .get("settings")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter(|s| s.get("plugin").and_then(|x| x.as_str()) == Some(plugin_id))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        if settings.is_empty() {
            adl_ui::widgets::empty_state(ui, "这个插件没有声明可配置项。");
            return;
        }

        let mut pending: Vec<(String, String, String)> = Vec::new();
        adl_ui::widgets::card(ui, |ui| {
            for s in &settings {
                let plugin = s.get("plugin").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let key = s.get("key").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let label = s.get("label").and_then(|x| x.as_str()).unwrap_or(&key).to_string();
                let kind = s.get("kind").and_then(|x| x.as_str()).unwrap_or("string").to_string();
                let value = s
                    .get("value")
                    .and_then(|x| x.as_str())
                    .map(|v| v.to_string())
                    .or_else(|| s.get("default").and_then(|x| x.as_str()).map(|v| v.to_string()))
                    .unwrap_or_default();
                let editable = s.get("editable").and_then(|x| x.as_bool()).unwrap_or(false);
                let note = s.get("note").and_then(|x| x.as_str()).map(|v| v.to_string());

                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;
                    adl_ui::widgets::label_cell(ui, 210.0, &label);
                    adl_ui::widgets::cell(ui, 280.0, |ui| match kind.as_str() {
                        "boolean" => {
                            let mut b = value == "true";
                            if ui
                                .add_enabled(editable, egui::Checkbox::new(&mut b, ""))
                                .changed()
                            {
                                pending.push((plugin.clone(), key.clone(), b.to_string()));
                            }
                        }
                        "select" => {
                            let opts: Vec<String> = s
                                .get("options")
                                .and_then(|x| x.as_array())
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|o| o.as_str().map(|s| s.to_string()))
                                        .collect()
                                })
                                .unwrap_or_default();
                            let mut cur = value.clone();
                            ui.add_enabled_ui(editable, |ui| {
                                egui::ComboBox::from_id_salt(format!("page.{plugin}.{key}"))
                                    .width(260.0)
                                    .selected_text(cur.clone())
                                    .show_ui(ui, |ui| {
                                        for o in &opts {
                                            ui.selectable_value(&mut cur, o.clone(), o.clone());
                                        }
                                    });
                            });
                            if cur != value {
                                pending.push((plugin.clone(), key.clone(), cur));
                            }
                        }
                        _ => {
                            let buf = self
                                .plugin_edit
                                .entry((plugin.clone(), key.clone()))
                                .or_insert_with(|| value.clone());
                            if ui
                                .add_enabled(
                                    editable,
                                    egui::TextEdit::singleline(buf).desired_width(260.0),
                                )
                                .changed()
                            {
                                pending.push((plugin.clone(), key.clone(), buf.clone()));
                            }
                        }
                    });
                    if !editable {
                        adl_ui::widgets::hint(ui, "（信任模式下才可改）");
                    }
                });
                if let Some(n) = note {
                    ui.horizontal(|ui| {
                        adl_ui::widgets::label_cell(ui, 210.0, "");
                        adl_ui::widgets::hint(ui, &n);
                    });
                }
                ui.add_space(8.0);
            }
        });

        for (p, k, v) in pending {
            self.save_plugin_setting(&p, &k, &v);
        }
    }

    pub(crate) fn save_plugin_setting(&mut self, plugin: &str, key: &str, value: &str) {
        if let Err(e) = save_plugin_setting(plugin, key, value) {
            self.set_status(e, true);
            return;
        }
        self.plugin_snapshot_at = Instant::now() - Duration::from_secs(3600);
    }
}