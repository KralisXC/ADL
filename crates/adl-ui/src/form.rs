//! 声明式表单：把"标签 + 控件 + 说明"这组三件套从重复劳动里抽出来。
//!
//! 原来设置页是 201 行 `Grid::new(..).show(|ui| { label_cell(..); ui.add_sized(..); ui.end_row(); })`
//! 重复七遍。用 `form()` + `Field` 之后同样内容约 30 行，且新增一行只需加一个 `Field`。

use egui::{Align, Layout, Ui, vec2};

use super::theme::Theme;
use super::widgets;

/// 表单字段：每个变体自带"怎么画控件"。
///
/// 故意不引入 trait object（`Box<dyn Field>`）—— 那样调用方每行都要 `Box::new`。
/// 枚举足够，且编译器能保证穷尽性。
pub enum Field<'a> {
    /// 单行文本
    Text {
        label: &'a str,
        value: &'a mut String,
        hint: &'a str,
        note: &'a str,
        /// 输入框右边挂的小按钮：(文案, 动作 id)。
        ///
        /// 用来放"选择文件夹…"这类一次性动作 —— 它们和字段强相关，
        /// 单独在 form 外面再画一行反而会跟标签列对不齐。
        action: Option<(&'a str, &'a str)>,
    },
    /// 整数（拖动 / 键入）
    Number {
        label: &'a str,
        value: &'a mut u64,
        range: std::ops::RangeInclusive<u64>,
        note: &'a str,
    },
    /// 复选（右侧带说明）
    Check {
        label: &'a str,
        value: &'a mut bool,
        text: &'a str,
    },
    /// 下拉选择
    Select {
        label: &'a str,
        value: &'a mut String,
        options: &'a [(&'a str, &'a str)],
        note: &'a str,
    },
}

impl<'a> Field<'a> {
    pub fn text(label: &'a str, value: &'a mut String) -> Self {
        Self::Text {
            label,
            value,
            hint: "",
            note: "",
            action: None,
        }
    }

    pub fn number(
        label: &'a str,
        value: &'a mut u64,
        range: std::ops::RangeInclusive<u64>,
    ) -> Self {
        Self::Number {
            label,
            value,
            range,
            note: "",
        }
    }

    pub fn checkbox(label: &'a str, value: &'a mut bool, text: &'a str) -> Self {
        Self::Check { label, value, text }
    }

    pub fn select(
        label: &'a str,
        value: &'a mut String,
        options: &'a [(&'a str, &'a str)],
    ) -> Self {
        Self::Select {
            label,
            value,
            options,
            note: "",
        }
    }

    pub fn hint(mut self, hint: &'a str) -> Self {
        if let Self::Text { hint: h, .. } = &mut self {
            *h = hint;
        }
        self
    }

    /// 在输入框右边挂一个小按钮。点中后动作 id 会出现在 `form` 的返回值里，
    /// 由调用方决定要做什么（例如弹系统目录选择框）。
    pub fn action(mut self, text: &'a str, id: &'a str) -> Self {
        if let Self::Text { action: a, .. } = &mut self {
            *a = Some((text, id));
        }
        self
    }

    pub fn note(mut self, note: &'a str) -> Self {
        match &mut self {
            Self::Text { note: n, .. }
            | Self::Number { note: n, .. }
            | Self::Select { note: n, .. } => *n = note,
            Self::Check { .. } => {}
        }
        self
    }

    fn label(&self) -> &str {
        match self {
            Self::Text { label, .. }
            | Self::Number { label, .. }
            | Self::Check { label, .. }
            | Self::Select { label, .. } => label,
        }
    }
}

/// 画一个表单卡片。`title` 为空时不画分组标题。
///
/// 返回本帧被点中的"后缀按钮动作 id"（`Field::action`），没有就是 `None`。
/// 注意**不是**布尔：一个表单里可能挂多个后缀按钮（目录、UA），
/// 光知道"某个被点了"没法分辨是哪个。
pub fn form(ui: &mut Ui, title: &str, fields: &mut [Field<'_>]) -> Option<String> {
    let th = Theme::from_ui(ui);
    let label_w = th.spacing.form_label_w;
    let ctrl_w = th.spacing.form_ctrl_w;
    let field_h = th.spacing.field_h;

    let mut action_hit: Option<String> = None;

    widgets::card(ui, |ui| {
        ui.add_space(2.0);
        if !title.is_empty() {
            widgets::section_title(ui, title);
            ui.add_space(8.0);
        }

        for f in fields.iter_mut() {
            let label = f.label().to_string();
            ui.horizontal(|ui| {
                // 左：标签（固定宽度、左对齐、垂直居中）
                ui.allocate_ui_with_layout(
                    vec2(label_w, field_h),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        ui.label(&label);
                    },
                );

                // 右：控件（固定宽度起，可被内容撑高）
                match f {
                    Field::Text {
                        value,
                        hint,
                        note,
                        action,
                        ..
                    } => {
                        // `value` 是 `&mut &mut String`（枚举被 &mut 借用后再解构），
                        // egui 的 TextBuffer 要的是 `&mut String`，所以解两层。
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.add_sized(
                                    [ctrl_w, field_h],
                                    egui::TextEdit::singleline(&mut **value).hint_text(*hint),
                                );
                                if let Some((text, id)) = action {
                                    if ui.button(*text).clicked() {
                                        action_hit = Some((*id).to_string());
                                    }
                                }
                            });
                            if !note.is_empty() {
                                widgets::hint(ui, note);
                            }
                        });
                    }
                    Field::Number {
                        value, range, note, ..
                    } => {
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::DragValue::new(&mut **value)
                                        .range(range.clone())
                                        .speed(1.0),
                                );
                                if !note.is_empty() {
                                    widgets::hint(ui, note);
                                }
                            });
                        });
                    }
                    Field::Check { value, text, .. } => {
                        ui.vertical(|ui| {
                            ui.add_space(3.0);
                            ui.checkbox(&mut **value, *text);
                        });
                    }
                    Field::Select {
                        value, options, note, ..
                    } => {
                        ui.vertical(|ui| {
                            let cur = options
                                .iter()
                                .find(|(k, _)| k == value)
                                .map(|(_, v)| *v)
                                .unwrap_or(value.as_str())
                                .to_string();
                            // value 是 `&mut &mut String`（枚举被 &mut 借用后又解构），
                            // 所以要解两层；`selectable_value` 要的是 `&mut String`。
                            egui::ComboBox::from_id_salt(format!("adl_form_{label}"))
                                .width(ctrl_w)
                                .selected_text(cur)
                                .show_ui(ui, |ui| {
                                    for (k, v) in options.iter() {
                                        ui.selectable_value(&mut **value, (*k).to_string(), *v);
                                    }
                                });
                            if !note.is_empty() {
                                widgets::hint(ui, note);
                            }
                        });
                    }
                }
            });
            ui.add_space(10.0);
        }
    });

    action_hit
}

/// 表单 + 底部主按钮的组合（设置页那种"改完点保存"）。
///
/// 返回 `true` 表示点了保存。
pub fn form_with_save(
    ui: &mut Ui,
    title: &str,
    fields: &mut [Field<'_>],
    save_label: &str,
) -> bool {
    let _ = form(ui, title, fields);
    ui.add_space(14.0);
    let mut clicked = false;
    widgets::card(ui, |ui| {
        ui.horizontal(|ui| {
            if ui.button(save_label).clicked() {
                clicked = true;
            }
        });
    });
    clicked
}
