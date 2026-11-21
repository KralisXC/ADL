//! 基础控件：egui 表达力不足或需要反复手写的那几个，收进来一次写好。
//!
//! 这里只放"长得不能只靠 `ui.add(...)` 一句话说清"的控件。
//! 普通按钮、输入框不必包 —— 包的越多，反而离 egui 越远。

use egui::{Align, Color32, CornerRadius, FontId, Layout, Rect, Response, Sense, Ui, vec2};

use super::theme::Theme;

/// 固定宽度、左对齐、垂直居中的标签格。
///
/// **不能**直接用 `ui.add_sized([w, h], Label::new(..).halign(Align::LEFT))`：
/// 那样拿到的是"居中"的观感（各标签看起来像右对齐），必须显式建左对齐布局。
pub fn label_cell(ui: &mut Ui, w: f32, text: &str) {
    let h = Theme::from_ui(ui).spacing.field_h;
    ui.allocate_ui_with_layout(vec2(w, h), Layout::left_to_right(Align::Center), |ui| {
        ui.label(text);
    });
}

/// 固定宽度、左对齐、垂直居中的单元格。
///
/// `set_min_width` 是关键：`allocate_ui_with_layout` 只提供 max_rect，
/// 水平布局前进时用的是子 Ui 的"已用宽度"。表头文字很窄，不强制撑满
/// 就会各列挤在一起、与数据行对不齐（实测过）。
pub fn cell(ui: &mut Ui, w: f32, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(vec2(w, 26.0), Layout::left_to_right(Align::Center), |ui| {
        ui.set_min_width(w);
        ui.set_max_width(w);
        add(ui);
    });
}

/// 侧边栏导航项：整行可点，选中态浅青底 + 左侧 3px 竖条。
pub fn nav_item(ui: &mut Ui, selected: bool, label: &str) -> bool {
    let th = Theme::from_ui(ui);
    let p = &th.palette;
    let h = th.spacing.nav_h;
    let w = ui.available_width();

    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());

    if selected || resp.hovered() {
        let bg = if selected { p.accent_soft } else { p.hover_bg };
        ui.painter()
            .rect_filled(rect, CornerRadius::same(th.radius.nav), bg);
    }
    if selected {
        let bar = Rect::from_min_size(
            egui::pos2(rect.left(), rect.top() + 8.0),
            vec2(3.0, rect.height() - 16.0),
        );
        ui.painter()
            .rect_filled(bar, CornerRadius::same(2), p.accent);
    }

    let color = if selected { p.accent } else { p.ink };
    ui.painter().text(
        egui::pos2(rect.left() + 14.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::proportional(th.text.body),
        color,
    );

    resp.clicked()
}

/// 自绘小按钮（顶栏用）。
///
/// 不用 `with_layout(right_to_left)` 或 `egui::Sides`：在 `Panel::top` 的
/// 内边距里会被裁掉或跑到面板外面（实测过）。这里直接按矩形画。
pub fn small_button(ui: &mut Ui, rect: Rect, label: &str, primary: bool) -> bool {
    let th = Theme::from_ui(ui);
    let p = &th.palette;

    // 用 scope_id 派生唯一 id：`ui.id()` 在 0.37 已弃用。
    let resp = ui.interact(rect, ui.scope_id().with(label), Sense::click());

    let (bg, fg) = if primary {
        let bg = if resp.hovered() { p.accent_hover } else { p.accent };
        (bg, Color32::WHITE)
    } else {
        let bg = if resp.hovered() {
            p.line
        } else {
            p.hover_bg
        };
        (bg, p.ink)
    };

    ui.painter()
        .rect_filled(rect, CornerRadius::same(th.radius.button), bg);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        FontId::proportional(th.text.button),
        fg,
    );

    resp.clicked()
}

/// 状态胶囊（"安全模式" / "信任模式"）。
pub fn mode_pill(ui: &mut Ui, trusted: bool) {
    let th = Theme::from_ui(ui);
    let p = &th.palette;

    let (text, fg, bg) = if trusted {
        ("信任模式", p.warn, p.warn_bg)
    } else {
        ("安全模式", p.accent, p.accent_soft)
    };

    let (rect, _) = ui.allocate_exact_size(vec2(76.0, 22.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(th.radius.pill), bg);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        FontId::proportional(th.text.hint),
        fg,
    );
}

/// 小徽章（状态标记）。
pub fn badge(ui: &mut Ui, text: &str, color: Color32) -> Response {
    let th = Theme::from_ui(ui);
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        FontId::proportional(th.text.hint),
        color,
    );
    let pad = vec2(7.0, 3.0);
    let (rect, resp) =
        ui.allocate_exact_size(galley.size() + pad * 2.0, Sense::hover());
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(th.radius.pill),
        color.gamma_multiply(0.12),
    );
    ui.painter().galley(
        rect.center() - galley.size() / 2.0,
        galley,
        color,
    );
    resp
}

/// 卡片容器（白底 + 圆角 + 细描边 + 内边距），并让内容撑满可用宽度。
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let th = Theme::from_ui(ui);
    th.card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        add(ui)
    })
    .inner
}

/// 小节标题（卡片内的"下载" / "插件" 分组标题）。
pub fn section_title(ui: &mut Ui, text: &str) {
    let th = Theme::from_ui(ui);
    ui.label(
        egui::RichText::new(text)
            .size(th.text.section)
            .strong()
            .color(th.palette.muted),
    );
}

/// 说明性小字（控件右侧或下方的注释）。
pub fn hint(ui: &mut Ui, text: &str) {
    let th = Theme::from_ui(ui);
    ui.label(
        egui::RichText::new(text)
            .small()
            .color(th.palette.muted),
    );
}

/// 空状态提示（列表无数据时）。
pub fn empty_state(ui: &mut Ui, text: &str) {
    let th = Theme::from_ui(ui);
    ui.add_space(20.0);
    ui.label(
        egui::RichText::new(text)
            .color(th.palette.muted)
            .size(th.text.body),
    );
}

/// 极细分割线（1 物理像素观感）。
///
/// **只在"内容被截断/滚动"的地方用**：表头下沿、可滚动区域的上/下边缘。
/// 分区本身靠底色差（白卡 vs 浅底）表达，不要用线去划每个面板 —— 那样
/// 界面会被一堆框裹住（用户明确说过"分隔线太明显了"）。
///
/// 用 `hair` 令牌而不是 `line`：`line` 是卡片描边强度，做面板分隔太重。
pub fn hairline(ui: &mut Ui) {
    let th = Theme::from_ui(ui);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
    // 只画中间 0.5 高的一根线，让它在 2x DPI 下也不至于变成一条 2px 粗线。
    let y = rect.center().y;
    ui.painter().line_segment(
        [
            egui::pos2(rect.left(), y),
            egui::pos2(rect.right(), y),
        ],
        egui::Stroke::new(0.6, th.palette.hair),
    );
}

/// 自绘标题栏上的窗口按钮。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowButton {
    Minimize,
    Maximize,
    Restore,
    Close,
}

/// 窗口控制按钮（最小化 / 最大化·还原 / 关闭）。
///
/// 关闭按钮悬停时变红底白叉（Windows 习惯），其余悬停浅灰底。
/// 图标全部用 `line_segment` 画，不依赖字体里的几何符号 ——
/// 中文字体（simhei）没有 `✕ ▢ ─` 这些字形，会渲染成方块。
pub fn window_button(ui: &mut Ui, rect: Rect, kind: WindowButton) -> bool {
    let th = Theme::from_ui(ui);
    let p = &th.palette;

    let resp = ui.interact(
        rect,
        ui.scope_id().with(("winbtn", kind as u8)),
        Sense::click(),
    );

    let is_close = kind == WindowButton::Close;
    if resp.hovered() {
        let bg = if is_close {
            Color32::from_rgb(0xC4, 0x2B, 0x1C)
        } else {
            p.hover_bg
        };
        ui.painter()
            .rect_filled(rect, CornerRadius::same(6), bg);
    }
    let fg = if resp.hovered() && is_close {
        Color32::WHITE
    } else if resp.hovered() {
        p.ink
    } else {
        p.muted
    };

    let c = rect.center();
    let s = 4.5; // 半边长
    let stroke = egui::Stroke::new(1.2, fg);
    let painter = ui.painter();
    match kind {
        WindowButton::Minimize => {
            painter.line_segment(
                [egui::pos2(c.x - s, c.y + 0.5), egui::pos2(c.x + s, c.y + 0.5)],
                stroke,
            );
        }
        WindowButton::Maximize => {
            let r = Rect::from_center_size(c, vec2(s * 2.0, s * 2.0));
            painter.rect_stroke(
                r,
                CornerRadius::same(1),
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        WindowButton::Restore => {
            // 两个叠起来的小方框（错开 2px）
            let r1 = Rect::from_center_size(
                egui::pos2(c.x - 1.0, c.y + 1.0),
                vec2(s * 1.8, s * 1.8),
            );
            let r2 = Rect::from_center_size(
                egui::pos2(c.x + 1.0, c.y - 1.0),
                vec2(s * 1.8, s * 1.8),
            );
            painter.rect_stroke(r2, CornerRadius::same(1), stroke, egui::StrokeKind::Inside);
            painter.rect_filled(r1, CornerRadius::same(1), p.card);
            painter.rect_stroke(r1, CornerRadius::same(1), stroke, egui::StrokeKind::Inside);
        }
        WindowButton::Close => {
            painter.line_segment(
                [egui::pos2(c.x - s, c.y - s), egui::pos2(c.x + s, c.y + s)],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(c.x - s, c.y + s), egui::pos2(c.x + s, c.y - s)],
                stroke,
            );
        }
    }

    resp.clicked()
}
