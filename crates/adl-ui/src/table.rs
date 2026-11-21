//! 声明式表格：列定义 + 行渲染闭包，替掉手写的 `table_cell` 胶水。
//!
//! 为什么不用 `egui::Grid`：Grid 按"本帧内容宽度"分配列宽，文件名一长就会
//! 把整行宽度抢走，其余列被挤成竖排（实测过：`test8m (2).bin` 会一个字一行）。
//! 为什么不用 `egui_extras::TableBuilder`：它为列宽拖拽付出了额外状态与
//! 依赖（enum-map），而我们要的是"固定列宽 + 吃剩余宽 + 截断"这种最朴素的
//! 语义，自己三十行更可控。
//!
//! 虚拟化：行数很多时只画视口内的行（`ScrollArea::show_rows`），
//! 不引入任何额外依赖。

use egui::{Align, Layout, Response, Ui, vec2};

use super::theme::Theme;

/// 列宽策略。
#[derive(Clone, Copy, Debug)]
pub enum Width {
    /// 固定像素
    Fixed(f32),
    /// 吃掉剩余宽度（最多一列）
    Remainder,
}

/// 一列的定义。
#[derive(Clone, Debug)]
pub struct Column {
    pub title: &'static str,
    pub width: Width,
}

impl Column {
    pub fn new(title: &'static str, width: f32) -> Self {
        Self {
            title,
            width: Width::Fixed(width),
        }
    }

    pub fn remainder(title: &'static str) -> Self {
        Self {
            title,
            width: Width::Remainder,
        }
    }

    fn px(&self, remainder_px: f32) -> f32 {
        match self.width {
            Width::Fixed(w) => w,
            Width::Remainder => remainder_px,
        }
    }
}

/// 表头绘制状态：调用方拿不到它，但内部两处（表头/数据行）要算一样的列宽。
struct Layout2 {
    /// 每列的实际像素宽（含余量列）
    widths: Vec<f32>,
}

fn compute(columns: &[Column], available: f32, gap: f32) -> Layout2 {
    let n = columns.len();
    let gaps = gap * (n.saturating_sub(1)) as f32;
    let fixed: f32 = columns
        .iter()
        .filter_map(|c| match c.width {
            Width::Fixed(w) => Some(w),
            Width::Remainder => None,
        })
        .sum();
    // 减去 16px 安全边距，避免出现横向滚动条盖住最右列
    let remainder = (available - fixed - gaps - 16.0).max(120.0);
    let widths = columns.iter().map(|c| c.px(remainder)).collect();
    Layout2 { widths }
}

/// 固定宽度单元格。
fn cell(ui: &mut Ui, w: f32, h: f32, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(vec2(w, h), Layout::left_to_right(Align::Center), |ui| {
        // set_min_width 是关键：否则窄文字会让各列挤在一起、与数据行对不齐。
        ui.set_min_width(w);
        ui.set_max_width(w);
        add(ui);
    });
}

/// 画表头，返回计算好的列宽（供数据行复用）。
fn header(ui: &mut Ui, columns: &[Column], lay: &Layout2, gap: f32, muted: egui::Color32) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        for (c, w) in columns.iter().zip(lay.widths.iter()) {
            cell(ui, *w, 22.0, |ui| {
                ui.label(egui::RichText::new(c.title).small().color(muted));
            });
        }
    });
    ui.add_space(4.0);
    ui.separator();
    ui.add_space(4.0);
}

/// 画整张表（非虚拟化；行数少时够用，语义最简单）。
///
/// `row` 收到行下标，返回它画出来的东西（一般是最后的 `Response`，可忽略）。
pub fn table<R>(
    ui: &mut Ui,
    columns: &[Column],
    row_count: usize,
    mut row: impl FnMut(&mut Ui, usize, &[f32]) -> R,
) {
    let th = Theme::from_ui(ui);
    let gap = th.spacing.col_gap;
    let row_h = th.spacing.row_h;
    let lay = compute(columns, ui.available_width(), gap);

    header(ui, columns, &lay, gap, th.palette.muted);

    for i in 0..row_count {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            ui.set_height(row_h);
            row(ui, i, &lay.widths);
        });
        ui.add_space(2.0);
        ui.separator();
        ui.add_space(2.0);
    }
}

/// 画一张虚拟化表（行数可能上千时用）：只渲染视口内的行。
///
/// 需要表格自己撑起滚动区，所以调用方不要再在外面套 `ScrollArea`。
pub fn virtual_table<R>(
    ui: &mut Ui,
    columns: &[Column],
    row_count: usize,
    mut row: impl FnMut(&mut Ui, usize, &[f32]) -> R,
) {
    let th = Theme::from_ui(ui);
    let gap = th.spacing.col_gap;
    let row_h = th.spacing.row_h;
    let lay = compute(columns, ui.available_width(), gap);

    header(ui, columns, &lay, gap, th.palette.muted);

    // 行高 + 行间 2+2 间距（与 table() 的视觉一致）
    let pitch = row_h + 4.0;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, pitch, row_count, |ui, range| {
            for i in range {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    ui.set_height(row_h);
                    row(ui, i, &lay.widths);
                });
                ui.add_space(4.0);
            }
        });
}

/// 单元格里常用：单行截断 + 悬停显示全文。
pub fn truncated(ui: &mut Ui, text: &str, full: &str) -> Response {
    ui.add(egui::Label::new(text).truncate().sense(egui::Sense::hover()))
        .on_hover_text(full)
}
