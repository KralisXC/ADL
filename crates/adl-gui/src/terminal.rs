//! 终端页：仿 PowerShell 的交互式命令行。
//!
//! 这里装的是**渲染 + 输入处理 + 命令分发**三件事：`page_terminal` 画界面，
//! `term_complete` 管 Tab 循环补全，`term_exec` 是命令分发表。
//!
//! 参考的是 `E:\ADL\PowerShell-master` 里的 ConsoleHost：
//! `ConsoleHostUserInterface.cs` 只提供兜底的 `ReadLine`，真正的行内编辑、
//! 语法高亮、历史是 PSReadLine 模块接管（`ConsoleHost.cs:1788` 的
//! `ImportPSModule(new[]{"PSReadLine"})`）。egui 侧的等价物就是
//! `TextEdit::layouter` + 自己处理按键，所以这里的结构和它一致：
//! 输入框负责编辑，`term_layouter` 负责边打字边着色。

use super::*;
/// 终端里一行输出的级别（只影响颜色）。
///
/// PowerShell 把输出分成几条流（Output / Warning / Error / Verbose …），
/// 我们只保留四条：警告单独一条流很有用 —— “插件被 safe 模式挡住了”
/// 既不是正常输出也不是错误，用红色会像出事了。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineKind {
    /// 启动横幅（比正文亮一档，和 PowerShell 的 banner 一个地位）
    Banner,
    /// 用户敲的命令回显
    Echo,
    /// 正常输出
    Out,
    /// 提示 / 帮助
    Info,
    /// 警告：能继续，但有事要说
    Warn,
    /// 错误
    Err,
}

/// 终端页的状态：一屏历史 + 当前输入 + 上下翻历史。
pub(crate) struct TerminalState {
    pub(crate) lines: Vec<(LineKind, String)>,
    pub(crate) input: String,
    /// 按 ↑ 翻的历史（最新的在末尾）
    pub(crate) history: Vec<String>,
    /// 当前翻到第几条；`None` = 正在编辑新行
    pub(crate) hist_pos: Option<usize>,
    /// 请求焦点：刚按 Ctrl+K 进来、或者执行完一条命令后，光标要留在输入框
    pub(crate) focus: bool,
    /// Tab 补全状态。PowerShell 的 Tab 是**循环**的：按住 Tab 依次换下一个
    /// 候选，所以我们得记住这一轮是从哪一行开始的。
    pub(crate) comp: Option<Completion>,
}

/// 一轮 Tab 补全。
///
/// PowerShell 的 Tab 是**循环**的：按住不放依次换下一个候选。所以候选在
/// 第一次按 Tab 时就算好存下来，后面只推进 `idx`。候选存的是**整行**，
/// 不是片段 —— 这样二级命令（`plugin list`）也能一次替换干净。
pub(crate) struct Completion {
    pub(crate) cands: Vec<String>,
    pub(crate) idx: usize,
}

/// 终端输入框的固定 id。
///
/// 必须自己钉死：`TextEdit` 默认用"父布局 id + 序号"派生 id，虽然这里
/// 每帧结果一致，但我们要在**控件之外**（补全、翻历史）改它的光标，
/// 就得能拿到同一个 id 去 `TextEdit::load_state/store_state`。
const TERM_INPUT_ID: &str = "adl_term_input";

/// 把终端输入框的光标挪到行尾。
///
/// 补全/翻历史都是直接替换 `term.input` 整个字符串，光标状态却还留在
/// 旧位置：`h` 按 Tab 变成 `help` 之后，光标还卡在字符索引 1
/// （用户 m13765 报的就是这个）。所以每次程序化改内容都要手工复位。
fn term_caret_to_end(ctx: &egui::Context, text: &str) {
    let id = egui::Id::unique(TERM_INPUT_ID);
    if let Some(mut st) = egui::TextEdit::load_state(ctx, id) {
        // `CCursor` 的 index 是**字符**下标不是字节下标，
        // 所以中文目录/命令也不会把光标切在半个字上。
        st.cursor.set_char_range(Some(egui::text::CCursorRange::one(
            egui::text::CCursor::end_of_str(text),
        )));
        st.store(ctx, id);
    }
}

/// 终端第一级命令（Tab 补全和 help 共用一份，免得两处对不上）。
const TERM_COMMANDS: &[&str] = &[
    "help", "clear", "tasks", "add", "pause", "resume", "rm", "open", "settings", "set", "cd",
    "mode", "plugin", "history", "ver",
];

/// 算出 Tab 补全的候选（整行，不是片段）。
///
/// 只做"补全当前这个词"这一件事：补出来的都是 `前缀 + 完整候选`，
/// 所以 `plugin l` 补成 `plugin list` 而不会把 `plugin` 吃掉。
fn term_candidates(input: &str) -> Vec<String> {
    let line = input;
    // 行尾有空格 = 正在敲下一个词，当前词是空串
    let ends_space = line.ends_with(' ');
    let mut words: Vec<&str> = line.split_whitespace().collect();
    let cur = if ends_space {
        ""
    } else {
        words.pop().unwrap_or("")
    };
    let prefix = if words.is_empty() {
        String::new()
    } else {
        format!("{} ", words.join(" "))
    };

    let pool: Vec<&str> = match words.first().copied() {
        None => TERM_COMMANDS.to_vec(),
        Some("plugin") if words.len() == 1 => vec!["list", "run"],
        Some("set") if words.len() == 1 => vec!["dir", "connections", "limit"],
        Some("mode") if words.len() == 1 => vec!["safe", "trusted"],
        Some("pause") if words.len() == 1 => vec!["all"],
        // `tasks ls` 这种别名也补一下，省得记两套
        Some("tasks") if words.len() == 1 => vec!["ls"],
        // 第二层之后没什么可补的（插件 id、任务 id 都是动态的）
        Some(_) => Vec::new(),
    };

    pool.into_iter()
        .filter(|c| c.starts_with(cur))
        .map(|c| format!("{prefix}{c}"))
        .collect()
}

/// 终端输入行的实时着色。
///
/// 这就是 PSReadLine 干的事：**不是**等回车之后再去染输出，而是在输入框
/// 自己还不认识这行字的时候就按词法给它上色。挂到 `TextEdit::layouter`
/// 上，每敲一个字符都会被调一次。
///
/// 着色密度刻意保持很低 —— 真 PowerShell 的 prompt 行里只有命令名是绿的，
/// 参数和普通文本一个颜色。搞成彩虹反而不像终端了。
fn term_layouter(
    ui: &egui::Ui,
    text: &str,
    wrap_width: f32,
    ink_echo: egui::Color32,
    ink_out: egui::Color32,
    ink_path: egui::Color32,
) -> std::sync::Arc<egui::Galley> {
    let ink_warn = egui::Color32::from_rgb(0xE8, 0xC4, 0x6F);
    let font = egui::FontId::monospace(12.5);
    let fmt = |c: egui::Color32| egui::TextFormat {
        font_id: font.clone(),
        color: c,
        ..Default::default()
    };

    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = wrap_width;

    // 🔴 空串必须补一个零宽 section，否则光标会掉半行。
    //
    // `LayoutJob::sections` 为空时 epaint 的 `layout()` 里 `layout_section`
    // 一次都不会跑，于是 `paragraph.empty_paragraph_height` 停在初始值 0
    // （`text_layout.rs:81`），galley 高度就是 0。而光标的位置是
    // `cursor_rect()` 算的：`cursor_pos.max.y = cursor_pos.max.y.at_least(
    // cursor_pos.min.y + row_height)`（`text_cursor_state.rs:497`）——
    // 拿 0 高度的 galley 在 `inner_rect` 里居中，左上角正好落在**行中线**上，
    // 然后往下画满一个 `row_height`，于是空输入时的光标比同一行的提示符
    // 低了半行（用户 m14409 的截图就是这么来的；有字时 galley 有高度、
    // 居中后顶部对齐，所以看着正常）。
    //
    // 补一个空字符串的 section，`layout_section` 就会被调用一次，
    // 那里会执行 `paragraph.empty_paragraph_height = line_height`
    // （`text_layout.rs:541`），空 galley 于是也有了一行的高度。
    // `line_height` 显式钉成 `row_height`，和光标用的那个值完全一致。
    if text.is_empty() {
        let row_h = ui.painter().fonts_mut(|f| f.row_height(&font));
        job.append(
            "",
            0.0,
            egui::TextFormat {
                font_id: font.clone(),
                color: ink_out,
                line_height: Some(row_h),
                ..Default::default()
            },
        );
        return ui.painter().layout_job(job);
    }

    // 第二级子命令：`plugin list`、`set dir`、`mode safe` 这类。
    // 和 Tab 补全用的是同一套词，两处一起改才不会补得出命令却染不上色。
    const SUB: &[&str] = &[
        "list", "run", "dir", "connections", "limit", "safe", "trusted", "all", "ls",
    ];

    let mut idx = 0usize;
    for (wi, word) in text.split_whitespace().enumerate() {
        let start = match text[idx..].find(word) {
            Some(p) => idx + p,
            None => idx,
        };
        // 词与词之间的空白原样保留，否则缩进和多个空格会被着色吃掉
        if start > idx {
            job.append(&text[idx..start], 0.0, fmt(ink_out));
        }
        let color = if wi == 0 {
            if TERM_COMMANDS.contains(&word) {
                ink_echo
            } else {
                // 还不认识的命令先按正文画。真 PowerShell 也是执行之后才报红。
                ink_out
            }
        } else if word.starts_with('-') {
            ink_warn
        } else if wi == 1 && SUB.contains(&word) {
            ink_path
        } else {
            ink_out
        };
        job.append(word, 0.0, fmt(color));
        idx = start + word.len();
    }
    if idx < text.len() {
        job.append(&text[idx..], 0.0, fmt(ink_out));
    }
    // `ui.fonts()` 给的是 `&Fonts`，而 `layout_job` 要 `&mut`（它要写
    // 字形缓存）。`Painter::layout_job` 内部走 `fonts_mut`，是这里唯一
    // 能拿到可变引用的入口。
    ui.painter().layout_job(job)
}

impl Default for TerminalState {
    fn default() -> Self {
        let mut lines = Vec::new();
        // 两行 banner，形状照 PowerShell 的启动横幅来：
        //   Windows PowerShell 5.1
        //   Copyright (C) Microsoft Corporation. All rights reserved.
        // 我们没版权行可写，第二行改成快捷键提示（同样两行、同样长短）。
        lines.push((LineKind::Banner, format!("ADL 终端 {}", env!("CARGO_PKG_VERSION"))));
        lines.push((
            LineKind::Info,
            "输入 help 看可用命令 · Tab 补全 · ↑/↓ 翻历史".to_string(),
        ));
        Self {
            lines,
            input: String::new(),
            history: Vec::new(),
            hist_pos: None,
            focus: false,
            comp: None,
        }
    }
}

impl TerminalState {
    pub(crate) fn push(&mut self, kind: LineKind, text: impl Into<String>) {
        for line in text.into().lines() {
            self.lines.push((kind, line.to_string()));
        }
        // 只留最近 500 行：终端页没有分页，多了就纯占内存。
        if self.lines.len() > 500 {
            let cut = self.lines.len() - 500;
            self.lines.drain(0..cut);
        }
    }
}

impl App {
    pub(crate) fn page_terminal(&mut self, ui: &mut egui::Ui) {
        // 终端配色不走主题：这是"深色控制台"这一个孤岛，主题是浅色的。
        //
        // 用户说原来的样子"有些简陋、看着有分界感"，所以：
        // ① 去掉 1px 描边 —— 有描边就像另贴上去的一块黑板；
        // ② 圆角与卡片一致（10），内边距也调成卡片的量级；
        // ③ 提示符抄 PowerShell 的样子：`PS <目录>` + 尖括号，目录用强调色，
        //    一眼能看出"现在在哪个目录下操作"。
        let bg = egui::Color32::from_rgb(0x17, 0x1D, 0x1C);
        let ink_out = egui::Color32::from_rgb(0xDC, 0xE3, 0xE1);
        let ink_info = egui::Color32::from_rgb(0x77, 0x86, 0x83);
        let ink_echo = egui::Color32::from_rgb(0x8F, 0xD3, 0xC1);
        let ink_path = egui::Color32::from_rgb(0x6F, 0xA8, 0xE8);
        let ink_warn = egui::Color32::from_rgb(0xE8, 0xC4, 0x6F);
        let ink_err = egui::Color32::from_rgb(0xE3, 0x86, 0x86);
        // 横幅比正文再亮一档，但不是彩色 —— 真 PowerShell 的 banner
        // 和正文同色系，只是更干净（见 m12534 那张截图）。
        let ink_banner = egui::Color32::from_rgb(0xE8, 0xEE, 0xEC);
        let color_of = |k: LineKind| match k {
            LineKind::Banner => ink_banner,
            LineKind::Echo => ink_echo,
            LineKind::Out => ink_out,
            LineKind::Info => ink_info,
            LineKind::Warn => ink_warn,
            LineKind::Err => ink_err,
        };

        // 提示符里的目录：太长就从左边截，保留尾部（尾部的层次才是
        // 人真正关心的部分，跟 PowerShell 折行时的效果接近）。
        //
        // 默认配置里 `dir = "."`，直接显示就是个光秃秃的 `.`，看着像坏了；
        // PowerShell 的提示符永远是绝对路径，所以这里也换算成绝对路径再截。
        let cwd = {
            let raw = self.gui.cfg.lock().unwrap().dir.clone();
            let abs = std::path::Path::new(&raw)
                .canonicalize()
                .map(|p| p.display().to_string().replace('\\', "/"))
                .unwrap_or_else(|_| raw.clone());
            let n = abs.chars().count();
            if n > 52 {
                let tail: String = abs.chars().skip(n - 49).collect();
                format!("…{tail}")
            } else {
                abs
            }
        };

        // 大框占满可用高度。输入行**不**钉在底部，而是跟在历史输出的最后
        // 一行后面，和历史共用一个滚动区 —— 这才是真终端的样子（在 `PS>`
        // 后面就地打字），而不是底下永远浮着一条输入栏。
        //
        // 高度要先把 Frame 的内边距（symmetric(14,12) → 上下各 12）扣掉，
        // 否则内容比框高 24px，`PS>` 那一行会被裁在框外看不见。
        let full_h = ui.available_height().max(200.0);
        let inner_h = (full_h - 24.0).max(160.0);

        let mut submit = false;

        egui::Frame::new()
            .fill(bg)
            .corner_radius(egui::CornerRadius::same(10))
            .inner_margin(egui::Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.set_height(inner_h);

                // ── 历史输出 + 当前输入行（同一个滚动区）──
                //
                // 把输入行放进滚动区里，而不是放在框外：真终端就是"在最后
                // 一行 PS> 后面接着打"，滚历史时输入行跟着一起走。靠
                // `stick_to_bottom(true)` 保证它永远停在看得见的最底下。
                egui::ScrollArea::vertical()
                    .id_salt("adl_term_log")
                    .max_height(inner_h)
                    .auto_shrink([false, false])
                    // 新输出总是把视图顶到底部，省得每次执行完还要手动滚
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        for (kind, text) in &self.term.lines {
                            match kind {
                                LineKind::Echo => {
                                    // 回显要把整条提示符留住，不能只剩命令名 ——
                                    // 真 PowerShell 里敲过的行是原样留在屏幕上的，
                                    // 只有 "PS 命令" 没有目录和 `>` 会看着像丢了东西。
                                    // 用 `\t` 把"当时的目录"和"命令"分开存：
                                    // 目录用路径色、命令用正文色，两者还对得上。
                                    let (where_at, cmd) = text
                                        .split_once('\t')
                                        .unwrap_or(("", text.as_str()));
                                    ui.horizontal_wrapped(|ui| {
                                        // 和下面正在输入的那一行用完全相同的
                                        // 间距：`PS ` + `目录>` + 6px + 内容。
                                        // 两处只要差一个空格，输入的字就不会
                                        // 和上面那些回显行左边对起来。
                                        ui.spacing_mut().item_spacing.x = 0.0;
                                        ui.label(
                                            egui::RichText::new("PS ")
                                                .monospace()
                                                .size(12.5)
                                                .color(ink_echo),
                                        );
                                        if !where_at.is_empty() {
                                            ui.label(
                                                egui::RichText::new(where_at)
                                                    .monospace()
                                                    .size(12.5)
                                                    .color(ink_path),
                                            );
                                        }
                                        ui.add_space(6.0);
                                        ui.label(
                                            egui::RichText::new(cmd)
                                                .monospace()
                                                .size(12.5)
                                                .color(ink_out),
                                        );
                                    });
                                }
                                k => {
                                    let c = color_of(*k);
                                    ui.label(
                                        egui::RichText::new(text).monospace().size(12.5).color(c),
                                    );
                                }
                            }
                        }

                        // ── 当前输入行（行内）──
                        // Enter/Tab 必须在建 TextEdit **之前**读：单行 TextEdit 收到回车
                        // 会 `consume_key(Enter)` 并 surrender_focus，等它返回之后
                        // 再 `key_pressed(Enter)` 永远是 false（实测第一次回车丢键）。
                        let (enter, tab, up, down, esc) = ui.input(|i| {
                            (
                                i.key_pressed(egui::Key::Enter),
                                i.key_pressed(egui::Key::Tab),
                                i.key_pressed(egui::Key::ArrowUp),
                                i.key_pressed(egui::Key::ArrowDown),
                                i.key_pressed(egui::Key::Escape),
                            )
                        });
                        // 补全/翻历史是程序化替换整行内容，替换完要把光标
                        // 拎到行尾（否则 `h` → `help` 之后光标还卡在索引 1）。
                        // 这里只记一个标记，等 TextEdit 画完、状态落盘之后再改。
                        let mut caret_to_end = false;
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            ui.label(
                                egui::RichText::new("PS ")
                                    .monospace()
                                    .size(12.5)
                                    .color(ink_echo),
                            );
                            ui.label(
                                egui::RichText::new(format!("{cwd}>"))
                                    .monospace()
                                    .size(12.5)
                                    .color(ink_path),
                            );
                            ui.add_space(6.0);
                            // 边打边着色：把当前这行词法过一遍，命令名/子命令
                            // 用提示符那支绿，参数保持正文色。这是 PSReadLine
                            // 的做法 —— 它也是在自己接管输入之后边打边给 token
                            // 上色，而不是等回车之后再染。
                            let mut layouter =
                                |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
                                    term_layouter(ui, buf.as_str(), wrap, ink_echo, ink_out, ink_info)
                                };
                            let te = ui.add(
                                egui::TextEdit::singleline(&mut self.term.input)
                                    // 固定 id：补全/翻历史要在控件之外改光标，
                                    // 必须能拿到同一个 id（见 `term_caret_to_end`）。
                                    .id(egui::Id::unique(TERM_INPUT_ID))
                                    // 必须显式给字号，不能用 `TextStyle::Monospace`：
                                    // 主题里只覆盖了 Body/Button/Heading/Small，
                                    // Monospace 还是 egui 默认的 14.0，而提示符标签
                                    // 是 12.5。两边 galley 高度不同，TextEdit 在
                                    // `ui.horizontal` 的 Align::Center 里居中后，
                                    // 它的字整体比提示符高出一截（实测差 25 逻辑像素）。
                                    .font(egui::FontId::monospace(12.5))
                                    .desired_width(f32::INFINITY)
                                    // TextEdit 的默认对齐是 LEFT_TOP，而 `interact_size.y`
                                    // 把它的框撑到 30；文字因此贴在自己的框顶，而同排的
                                    // 提示符标签是在框里居中 —— 实测输入的字比 `PS 路径>`
                                    // 高 7.3 逻辑像素。显式给 LEFT_CENTER 让两边同基线。
                                    .align(egui::Align2::LEFT_CENTER)
                                    .text_color(ink_out)
                                    // 输入框和面板同底色、无描边：中间不再有一块深色
                                    // 方块，也没有聚焦时才冒出来的圆角框，光标直接落
                                    // 在面板上 —— 这才是终端，不是表单。
                                    .frame(
                                        egui::Frame::new()
                                            .fill(bg)
                                            .stroke(egui::Stroke::NONE)
                                            .corner_radius(egui::CornerRadius::ZERO),
                                    )
                                    .margin(egui::Margin::symmetric(0, 4))
                                    // 不放行 Tab：默认单行 TextEdit 会把 Tab 当成"切焦点"，
                                    // 一按就跳到别的控件上去。这里自己接管做补全。
                                    .lock_focus(true)
                                    .layouter(&mut layouter),
                            );
                            if self.term.focus {
                                te.request_focus();
                                self.term.focus = false;
                            }
                            // 空输入时的灰字提示：**只在没有焦点时画**。
                            //
                            // 之前试过 `.hint_text(..)`、也试过自己 `painter.text`
                            // 再钉 `TextFormat::line_height` + 手工上抬 0.7px，
                            // 每次都要跟光标比基线 —— 因为灰字和光标是两套 galley：
                            // 光标高度取 TextEdit 自己的 `row_height`，灰字高度取
                            // 汉字回退字体的行高，谁也没法在不改源码的前提下压成
                            // 一条线（用户 m13765 / m14064 / m14285 连着报了三轮）。
                            //
                            // 换个思路：**这两样东西本来就不需要同时出现**。
                            // 真 PowerShell 里光标所在的空行也不显示任何提示。
                            // 没焦点时（刚切到终端页、或者点走了）画灰字告诉用户
                            // 能敲什么；一旦有焦点就只剩光标，谁也不跟谁比。
                            // 想留提示可以看上面那两行 banner，那里已经写全了。
                            if self.term.input.is_empty() && !te.has_focus() {
                                ui.painter().text(
                                    egui::pos2(te.rect.left() + 2.0, te.rect.center().y),
                                    egui::Align2::LEFT_CENTER,
                                    "输入 help 看命令",
                                    egui::FontId::monospace(12.5),
                                    ink_info,
                                );
                            }
                            // 只有输入框有焦点时才拦方向键/Esc，否则在别的页面按 ↑
                            // 也会翻终端历史（虽然看不见，但会莫名其妙改掉输入）。
                            // `lost_focus()` 也要算：回车那一帧焦点已经交还了。
                            let active = te.has_focus() || te.lost_focus();
                            if active {
                                // 手动改过内容（打字、粘贴）就作废上一轮补全，
                                // 否则再按 Tab 会拿旧候选把输入顶掉。
                                if te.changed() {
                                    self.term.comp = None;
                                }
                                if esc {
                                    self.term.input.clear();
                                    self.term.hist_pos = None;
                                    self.term.comp = None;
                                    caret_to_end = true;
                                }
                                if up && !self.term.history.is_empty() {
                                    // 第一次按 ↑ 从最新一条往上翻；翻到头就停在最老那条。
                                    let next = match self.term.hist_pos {
                                        None => self.term.history.len() - 1,
                                        Some(0) => 0,
                                        Some(i) => i - 1,
                                    };
                                    self.term.hist_pos = Some(next);
                                    self.term.input = self.term.history[next].clone();
                                    self.term.comp = None;
                                    caret_to_end = true;
                                }
                                if down {
                                    match self.term.hist_pos {
                                        // 翻回底部 = 回到正在编辑的空行
                                        Some(i) if i + 1 >= self.term.history.len() => {
                                            self.term.hist_pos = None;
                                            self.term.input.clear();
                                        }
                                        Some(i) => {
                                            self.term.hist_pos = Some(i + 1);
                                            self.term.input = self.term.history[i + 1].clone();
                                        }
                                        None => {}
                                    }
                                    self.term.comp = None;
                                    caret_to_end = true;
                                }
                                if tab {
                                    self.term_complete();
                                    // 真 PowerShell 的 Tab 补完，光标跟在词尾，
                                    // 接着打参数就行。循环换候选也一样。
                                    caret_to_end = true;
                                }
                                if enter {
                                    submit = true;
                                }
                            }
                        });
                        // 光标复位必须在 TextEdit 已经跑完（它这一帧的状态是
                        // 最后写进 memory 的那份）之后再做，否则会被覆盖回去。
                        if caret_to_end {
                            term_caret_to_end(ui.ctx(), &self.term.input);
                        }
                    });
            });

        if submit {
            let line = self.term.input.trim().to_string();
            self.term.input.clear();
            self.term.hist_pos = None;
            if !line.is_empty() {
                self.term.history.push(line.clone());
                // 回显要把"当时在哪个目录下敲的"一起留下：真 PowerShell
                // 里敲过的行是整条 `PS C:\xxx> 命令` 原样留在屏幕上的。
                // 用 `\t` 分隔目录与命令，渲染时各用各的颜色。
                self.term
                    .push(LineKind::Echo, format!("{cwd}>\t{line}"));
                self.term_exec(&line);
            }
            // 执行完把光标留在输入框，连敲才顺手
            self.term.focus = true;
        }
    }

    /// Tab 补全：第一次算出候选并套用第一个，之后每次推进到下一个。
    ///
    /// 之所以要自己接管：单行 `TextEdit` 默认把 Tab 当"切焦点"用
    /// （`event_filter.tab == false`），一按就跳到别的控件上去了。
    fn term_complete(&mut self) {
        if let Some(c) = self.term.comp.as_mut() {
            if !c.cands.is_empty() {
                c.idx = (c.idx + 1) % c.cands.len();
                self.term.input = c.cands[c.idx].clone();
                self.term.focus = true;
                return;
            }
        }
        let cands = term_candidates(&self.term.input);
        if cands.is_empty() {
            // 没候选就什么都不做（真终端这时会响一声，我们没有声音）
            self.term.comp = None;
            return;
        }
        self.term.input = cands[0].clone();
        self.term.comp = Some(Completion { cands, idx: 0 });
        self.term.focus = true;
    }

    /// 执行终端里的一行命令。
    ///
    /// 所有输出都写进 `self.term`（不弹 toast、不写状态条）—— 终端页自己
    /// 就是输出面板，再飘一条 6 秒消失的提示反而找不到。
    fn term_exec(&mut self, line: &str) {
        let mut it = line.split_whitespace();
        let cmd = it.next().unwrap_or("").to_ascii_lowercase();
        let args: Vec<String> = it.map(|s| s.to_string()).collect();

        match cmd.as_str() {
            "help" | "?" | "" => {
                // 形状照 PowerShell 的 Get-Help 输出：几个小节标题（主题 /
                // 简短说明 / 详细说明），正文两空格缩进、同色系、没有色块。
                // 命令表从 `TERM_COMMANDS` 生成，Tab 补全与 help 共用一份，
                // 免得两处各写一遍、改一处忘一处。
                self.term.push(LineKind::Banner, "主题");
                self.term.push(LineKind::Out, "  ADL 命令行");
                self.term.push(LineKind::Out, "");
                self.term.push(LineKind::Banner, "简短说明");
                self.term
                    .push(LineKind::Out, "  在这里管理下载任务、改设置、跑插件命令。");
                self.term.push(LineKind::Out, "");
                self.term.push(LineKind::Banner, "详细说明");
                for c in TERM_COMMANDS {
                    self.term.push(LineKind::Out, format!("  {c}"));
                }
                self.term.push(LineKind::Out, "");
                self.term.push(LineKind::Banner, "参数");
                for line in [
                    "  add <url> [文件名]              新建下载",
                    "  pause <id> | pause all          暂停",
                    "  resume <id>                     继续（按 URL 从控制文件续传）",
                    "  rm <id>                         从列表移除（不删文件）",
                    "  open <id>                       在资源管理器里定位",
                    "  cd <目录>                       改默认下载目录（不带参数=显示当前）",
                    "  set dir <路径>                   改默认下载目录",
                    "  set connections <1-64>          改并发上限",
                    "  set limit <字节/秒，0=不限>       改限速",
                    "  mode safe|trusted               切换插件模式",
                    "  plugin list                     列出已加载插件",
                    "  plugin run <插件> <命令>         跑插件声明的命令",
                ] {
                    self.term.push(LineKind::Out, line);
                }
                self.term.push(LineKind::Out, "");
                self.term.push(LineKind::Banner, "联机帮助");
                self.term.push(
                    LineKind::Out,
                    "  Tab 补全、↑/↓ 翻历史、Esc 清空当前行。",
                );
            }
            "clear" | "cls" => {
                // 把开头的两行横幅留下，只清后面的输出 —— 清成全黑会让人以为卡死
                self.term.lines.truncate(2);
            }
            "tasks" | "ls" => {
                let tasks = snapshot(&self.gui);
                if tasks.is_empty() {
                    self.term.push(LineKind::Info, "还没有任务");
                } else {
                    let mut rows = String::new();
                    for t in &tasks {
                        let (label, _) = status_style(&t.status);
                        let prog = if t.total > 0 {
                            format!("{:.1}%", t.completed as f64 * 100.0 / t.total as f64)
                        } else {
                            "-".to_string()
                        };
                        rows.push_str(&format!(
                            "  #{:<3} {:<6} {:>7}  {:<10}  {}\n",
                            t.id,
                            label,
                            prog,
                            format_speed(t.speed),
                            display_name(t)
                        ));
                    }
                    self.term.push(
                        LineKind::Out,
                        format!("  ID   状态      进度    速度        文件\n{rows}"),
                    );
                }
            }
            "add" => {
                if args.is_empty() {
                    self.term
                        .push(LineKind::Err, "用法：add <url> [文件名]");
                    return;
                }
                let mut payload = json!({ "url": args[0] });
                if let Some(name) = args.get(1) {
                    payload["out"] = json!(name);
                }
                match add_task(&self.gui, &payload) {
                    Ok(v) => {
                        let id = v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
                        self.term
                            .push(LineKind::Out, format!("已添加任务 #{id}：{}", args[0]));
                    }
                    Err(e) => self.term.push(LineKind::Err, format!("添加失败：{e}")),
                }
            }
            "pause" => {
                if args.first().map(|s| s.as_str()) == Some("all") {
                    pause_all(&self.gui);
                    self.term.push(LineKind::Out, "已暂停全部任务");
                } else if let Some(id) = args.first().and_then(|s| s.parse::<u64>().ok()) {
                    match pause_task(&self.gui, &json!({ "id": id })) {
                        Ok(_) => self.term.push(LineKind::Out, format!("已暂停 #{id}")),
                        Err(e) => self.term.push(LineKind::Err, e),
                    }
                } else {
                    self.term.push(LineKind::Err, "用法：pause <id> | pause all");
                }
            }
            "resume" => {
                let Some(id) = args.first().and_then(|s| s.parse::<u64>().ok()) else {
                    self.term.push(LineKind::Err, "用法：resume <id>");
                    return;
                };
                let url = snapshot(&self.gui)
                    .into_iter()
                    .find(|t| t.id == id)
                    .map(|t| t.url);
                match url {
                    None => self.term.push(LineKind::Err, format!("#{id} 不存在")),
                    Some(url) => {
                        // 续传走的是"重新 add 同一条 URL"：引擎会找到同名
                        // `.adl` 控制文件，从断点接着下，而不是从头开始。
                        match add_task(&self.gui, &json!({ "url": url })) {
                            Ok(v) => {
                                let nid =
                                    v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
                                self.term.push(
                                    LineKind::Out,
                                    format!("已继续 #{id}（新任务 #{nid}，从断点续传）"),
                                );
                            }
                            Err(e) => self.term.push(LineKind::Err, format!("继续失败：{e}")),
                        }
                    }
                }
            }
            "rm" => {
                let Some(id) = args.first().and_then(|s| s.parse::<u64>().ok()) else {
                    self.term.push(LineKind::Err, "用法：rm <id>");
                    return;
                };
                match remove_task(&self.gui, &json!({ "id": id, "files": false })) {
                    Ok(_) => self.term.push(LineKind::Out, format!("已移除 #{id}（文件保留）")),
                    Err(e) => self.term.push(LineKind::Err, e),
                }
            }
            "open" => {
                let Some(id) = args.first().and_then(|s| s.parse::<u64>().ok()) else {
                    self.term.push(LineKind::Err, "用法：open <id>");
                    return;
                };
                let path = snapshot(&self.gui)
                    .into_iter()
                    .find(|t| t.id == id)
                    .and_then(|t| t.path);
                match path {
                    Some(p) if std::path::Path::new(&p).exists() => {
                        match platform::open_path(std::path::Path::new(&p)) {
                            Ok(_) => self.term.push(LineKind::Out, format!("已打开 {p}")),
                            Err(e) => self.term.push(LineKind::Err, format!("打开失败：{e}")),
                        }
                    }
                    Some(p) => self
                        .term
                        .push(LineKind::Err, format!("文件还不存在：{p}")),
                    None => self
                        .term
                        .push(LineKind::Err, format!("#{id} 还没落盘")),
                }
            }
            "cd" => {
                // 只改"默认下载目录"，不改进程工作目录 —— 下载器没有 shell
                // 意义上的 cwd，这个命令的语义就是"以后文件放这儿"。
                let Some(p) = args.first() else {
                    let dir = self.gui.cfg.lock().unwrap().dir.clone();
                    self.term.push(LineKind::Out, dir);
                    return;
                };
                let dir = p.replace('/', "\\");
                let pb = std::path::Path::new(&dir);
                if !pb.is_dir() {
                    self.term
                        .push(LineKind::Err, format!("目录不存在：{dir}"));
                    return;
                }
                let mut cfg = self.gui.cfg.lock().unwrap();
                cfg.dir = dir.clone();
                let _ = cfg.save();
                drop(cfg);
                self.s_dir = dir.clone();
                self.term.push(LineKind::Out, format!("下载目录 → {dir}"));
            }
            "history" => {
                if self.term.history.is_empty() {
                    self.term.push(LineKind::Info, "还没有历史命令");
                } else {
                    let mut body = String::new();
                    for (i, h) in self.term.history.iter().enumerate() {
                        body.push_str(&format!("  {:>3}  {h}\n", i + 1));
                    }
                    self.term.push(LineKind::Out, body.trim_end().to_string());
                }
            }
            "ver" => {
                self.term.push(
                    LineKind::Out,
                    format!(
                        "ADL {} · {} · 插件 {}/{}",
                        env!("CARGO_PKG_VERSION"),
                        std::env::consts::OS,
                        {
                            let cfg = self.gui.cfg.lock().unwrap().clone();
                            if cfg.mode_trusted() { "trusted" } else { "safe" }
                        },
                        self.plugin_snapshot
                            .get("all")
                            .and_then(|x| x.as_array())
                            .map(|a| a.len())
                            .unwrap_or(0)
                    ),
                );
            }
            "settings" => {
                let cfg = self.gui.cfg.lock().unwrap().clone();
                self.term.push(
                    LineKind::Out,
                    format!(
                        "  下载目录  {}\n  并发上限  {}\n  限速      {}\n  代理      {}\n  插件模式  {}\n  插件目录  {}",
                        cfg.dir,
                        cfg.connections,
                        if cfg.limit == 0 {
                            "不限速".to_string()
                        } else {
                            format_speed(cfg.limit)
                        },
                        if cfg.proxy.is_empty() {
                            "直连".to_string()
                        } else {
                            cfg.proxy.clone()
                        },
                        cfg.mode,
                        if cfg.plugin_dir.is_empty() {
                            "exe 同级 plugins".to_string()
                        } else {
                            cfg.plugin_dir.clone()
                        },
                    ),
                );
            }
            "set" => {
                let key = args.first().map(|s| s.to_ascii_lowercase()).unwrap_or_default();
                let value = args.get(1).cloned().unwrap_or_default();
                match key.as_str() {
                    "dir" => {
                        if value.is_empty() {
                            self.term.push(LineKind::Err, "用法：set dir <路径>");
                            return;
                        }
                        // `/` 统一换成 `\`：终端里手打路径常常是斜杠，
                        // 而 Windows 上混着用会让后续 `PathBuf` 拼名字很别扭。
                        let dir = value.replace('/', "\\");
                        let mut cfg = self.gui.cfg.lock().unwrap();
                        cfg.dir = dir.clone();
                        let _ = cfg.save();
                        drop(cfg);
                        self.s_dir = dir.clone();
                        self.term.push(LineKind::Out, format!("下载目录 → {dir}"));
                    }
                    "connections" | "conn" => {
                        match value.parse::<usize>() {
                            Ok(n) => {
                                let n = adl_core::clamp_concurrency(n);
                                let mut cfg = self.gui.cfg.lock().unwrap();
                                cfg.connections = n;
                                let _ = cfg.save();
                                drop(cfg);
                                self.s_conn = n as u64;
                                self.term
                                    .push(LineKind::Out, format!("并发上限 → {n}"));
                            }
                            Err(_) => self
                                .term
                                .push(LineKind::Err, "并发要填数字（1-64）"),
                        }
                    }
                    "limit" => match value.parse::<u64>() {
                        Ok(n) => {
                            let mut cfg = self.gui.cfg.lock().unwrap();
                            cfg.limit = n;
                            let _ = cfg.save();
                            drop(cfg);
                            self.s_limit = n;
                            self.term.push(
                                LineKind::Out,
                                if n == 0 {
                                    "限速 → 不限速".to_string()
                                } else {
                                    format!("限速 → {}", format_speed(n))
                                },
                            );
                        }
                        Err(_) => self
                            .term
                            .push(LineKind::Err, "限速要填字节/秒（0 = 不限）"),
                    },
                    _ => self
                        .term
                        .push(LineKind::Err, "用法：set dir|connections|limit <值>"),
                }
            }
            "mode" => {
                let m = args.first().map(|s| s.to_ascii_lowercase()).unwrap_or_default();
                if m != "safe" && m != "trusted" {
                    self.term.push(LineKind::Err, "用法：mode safe | mode trusted");
                    return;
                }
                let mut cfg = self.gui.cfg.lock().unwrap();
                cfg.mode = m.clone();
                let _ = cfg.save();
                let new_cfg = cfg.clone();
                drop(cfg);
                *self.gui.hooks.lock().unwrap() = build_hooks(&new_cfg);
                self.s_mode = m.clone();
                self.plugin_snapshot_at = Instant::now() - Duration::from_secs(3600);
                self.term.push(
                    LineKind::Out,
                    format!(
                        "插件模式 → {m}{}",
                        if m == "safe" {
                            "（只读观察 / 拦截，Transform 类 hook 不生效）"
                        } else {
                            "（允许改写请求、注册按钮、替换页面）"
                        }
                    ),
                );
            }
            "plugin" => {
                let sub = args.first().map(|s| s.to_ascii_lowercase()).unwrap_or_default();
                match sub.as_str() {
                    "list" | "ls" => {
                        let all = self
                            .plugin_snapshot
                            .get("all")
                            .and_then(|x| x.as_array())
                            .cloned()
                            .unwrap_or_default();
                        if all.is_empty() {
                            self.term.push(LineKind::Info, "没有插件");
                        } else {
                            let mut rows = String::new();
                            for p in &all {
                                let id = p.get("id").and_then(|x| x.as_str()).unwrap_or("?");
                                let ver = p.get("version").and_then(|x| x.as_str()).unwrap_or("");
                                let trusted = p
                                    .get("trusted")
                                    .and_then(|x| x.as_bool())
                                    .unwrap_or(false);
                                rows.push_str(&format!(
                                    "  {:<26} {:<8} {}\n",
                                    id,
                                    ver,
                                    if trusted { "信任" } else { "安全" }
                                ));
                            }
                            self.term
                                .push(LineKind::Out, format!("  插件                       版本     模式\n{rows}"));
                        }
                    }
                    "run" => {
                        let (Some(p), Some(c)) = (args.get(1), args.get(2)) else {
                            self.term
                                .push(LineKind::Err, "用法：plugin run <插件> <命令>");
                            return;
                        };
                        let hooks = self.gui.hooks.lock().unwrap().clone();
                        match hooks.run_command(p, c) {
                            Some(v) => {
                                let msg = v
                                    .get("message")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("命令已执行")
                                    .to_string();
                                self.term.push(LineKind::Out, msg);
                            }
                            None => self.term.push(
                                // 警告而不是错误：命令没跑，但程序没坏，
                                // 只是当前模式不允许 —— 一屏红字会像出事了。
                                LineKind::Warn,
                                "命令不可用（要用 trusted 模式，且插件确实声明了该命令）",
                            ),
                        }
                    }
                    _ => self
                        .term
                        .push(LineKind::Err, "用法：plugin list | plugin run <插件> <命令>"),
                }
            }
            other => self.term.push(
                LineKind::Err,
                format!("未知命令：{other}（输入 help 看可用命令）"),
            ),
        }
    }

}
