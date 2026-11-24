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
pub fn cell<R>(ui: &mut Ui, w: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.allocate_ui_with_layout(vec2(w, 26.0), Layout::left_to_right(Align::Center), |ui| {
        ui.set_min_width(w);
        ui.set_max_width(w);
        add(ui)
    })
    .inner
}

/// 侧边栏导航项：整行可点，选中态浅青底 + 左侧 3px 竖条。
///
/// 静态版本（插件页等首次进入的场景直接用这个）。
pub fn nav_item(ui: &mut Ui, selected: bool, label: &str) -> bool {
    nav_item_anim(ui, selected, label, 1.0).clicked()
}

/// 带淡入的导航项，返回整行的 `Response`。
///
/// `fill` 是选中底色的不透明度（0.0..=1.0）。早先这里是"从左到右擦入"，
/// 用户觉得那块底色扫过来太抢眼，改成整体淡入。
/// 左侧那根 3px 竖条**不在这里画** —— 它要在整栏画完之后单独画一次
/// （见 `nav_bar`），否则切换页面时竖条只能"瞬间跳行"，做不出滑动。
pub fn nav_item_anim(ui: &mut Ui, selected: bool, label: &str, fill: f32) -> egui::Response {
    let th = Theme::from_ui(ui);
    let p = &th.palette;
    let h = th.spacing.nav_h;
    let w = ui.available_width();

    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());

    if resp.hovered() && !selected {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(th.radius.nav), p.hover_bg);
    }
    if selected {
        // 只改 alpha、不改 RGB：整块 `gamma_multiply` 会连颜色一起压暗，
        // 淡到一半会变成脏青色，而不是"浅底慢慢浮出来"。
        let a = (fill.clamp(0.0, 1.0) * 255.0).round() as u8;
        if a > 0 {
            let c = p.accent_soft;
            let fill = Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a);
            ui.painter()
                .rect_filled(rect, CornerRadius::same(th.radius.nav), fill);
        }
    }

    let color = if selected { p.accent } else { p.ink };
    ui.painter().text(
        egui::pos2(rect.left() + 14.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::proportional(th.text.body),
        color,
    );

    resp
}

/// 侧栏选中项的 3px 竖条，画在任意 y 上（供滑动动画跨行使用）。
///
/// `top` 是竖条顶端在**屏幕坐标**里的 y（调用方负责把行内偏移换算好）。
/// 竖条本身比行高短 16px，所以这里再往下让 8px 居中。
pub fn nav_bar(ui: &Ui, left: f32, top: f32, h: f32) {
    let th = Theme::from_ui(ui);
    let bar = Rect::from_min_size(egui::pos2(left, top + 8.0), vec2(3.0, h - 16.0));
    ui.painter()
        .rect_filled(bar, CornerRadius::same(2), th.palette.accent);
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

/// 「⋯」更多操作按钮：三个点**手画**，不用字符。
///
/// 不能用 `small_button(ui, rect, "⋯", false)`：simhei 没有 U+22EF 这个字形，
/// 渲染出来是个空方块（用户 m15179 的截图里那一列就是个空白方框）。
/// 和标题栏的窗口按钮一样，几何形状一律自己画。
pub fn more_button(ui: &mut Ui) -> Response {
    let th = Theme::from_ui(ui);
    let p = &th.palette;

    let size = vec2(30.0, 24.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(th.radius.button), p.hover_bg);
    }
    let fg = if resp.hovered() { p.ink } else { p.muted };
    let c = rect.center();
    for i in -1..=1 {
        ui.painter()
            .circle_filled(egui::pos2(c.x + i as f32 * 4.6, c.y), 1.5, fg);
    }
    resp
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

/// 开关（插件启停、布尔型设置）。
///
/// egui 自带的 `Checkbox` 是方形勾选框；在"启用/停用某个东西"的语境里，
/// 长条滑块（左边关/右边开）比打勾更直观 —— 所以自己画一个。
///
/// 圆点靠 `animate_bool_responsive` 平移：切一次开关会看到它滑过去，
/// 而不是"啪"地跳一下。
pub fn switch(ui: &mut Ui, on: bool) -> Response {
    let th = Theme::from_ui(ui);
    let p = &th.palette;

    let (rect, resp) = ui.allocate_exact_size(vec2(38.0, 21.0), Sense::click());
    let t = ui.ctx().animate_bool_responsive(resp.id, on);

    // 轨道颜色也跟着淡：`line`（关）→ `accent`（开）
    let track = mix_color(p.line, p.accent, t);
    ui.painter()
        .rect_filled(rect, CornerRadius::same(11), track);

    let knob_r = 8.0;
    let travel = rect.width() - 2.0 * (knob_r + 2.5);
    let cx = rect.left() + knob_r + 2.5 + travel * t;
    ui.painter()
        .circle_filled(egui::pos2(cx, rect.center().y), knob_r, Color32::WHITE);

    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// 两色之间按 `t`（0..=1）插值。
///
/// `Color32` 没有现成的 lerp：`gamma_multiply` 只是整体缩放亮度，
/// 从浅灰到青绿这种换色必须逐通道算。
fn mix_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        f(a.r(), b.r()),
        f(a.g(), b.g()),
        f(a.b(), b.b()),
        f(a.a(), b.a()),
    )
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
///
/// 用 `ink` 而不是 `muted`：小节标题是卡片内的第一层信息，本该比正文更重；
/// 之前的浅灰在小字号下几乎糊在白底里。
pub fn section_title(ui: &mut Ui, text: &str) {
    let th = Theme::from_ui(ui);
    ui.label(
        egui::RichText::new(text)
            .size(th.text.section)
            .strong()
            .color(th.palette.ink),
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

/// 比 [`hairline`] 更明确的一条分隔线：**低透明度黑**，不是主题色。
///
/// 用在"标题行与内容区交界"这种真需要一条视觉边界的地方。`hair` 在这里
/// 太淡（几乎看不见），`ui.separator()` 又像一道沟。
pub fn rule(ui: &mut Ui) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
    let y = rect.center().y;
    ui.painter().line_segment(
        [
            egui::pos2(rect.left(), y),
            egui::pos2(rect.right(), y),
        ],
        egui::Stroke::new(0.8, Color32::from_black_alpha(22)),
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

// ═══════════════════════════════════════════════════════════════════════
// 顶栏 chip / 分段选择 / 热力图 / 统计卡 / 侧栏折叠
// ═══════════════════════════════════════════════════════════════════════

/// 胶囊块的左侧图标。
///
/// 全部用线段/圆弧手画，**不依赖字体符号** —— 中文字体（simhei）里没有
/// 这些几何字形，`⌘ ⚡ ⋮` 之类会渲染成空方块。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChipIcon {
    /// 限速：小表盘 + 指针
    Gauge,
    /// 并行线路：一台机架服务器（几层硬盘抽屉）
    Lines,
    /// 后台任务：上面一颗菱形 + 下面堆叠层
    Stack,
}

fn paint_chip_icon(ui: &Ui, c: egui::Pos2, icon: ChipIcon, color: Color32) {
    let p = ui.painter();
    let stroke = egui::Stroke::new(1.15, color);
    match icon {
        ChipIcon::Gauge => {
            // 表盘：一段开口圆弧 + 一根指针。egui 没有 arc 图元，
            // 用 12 段短折线拼（这个尺寸下肉眼看不出棱角）。
            let r = 5.3f32;
            let (a0, a1) = (0.85f32, 0.85 + 4.4); // 留出右下缺口
            let mut pts = Vec::with_capacity(13);
            for i in 0..=12 {
                let a = a0 + (a1 - a0) * (i as f32 / 12.0);
                pts.push(c + vec2(a.cos(), a.sin()) * r);
            }
            p.add(egui::Shape::line(pts, stroke));
            let a = -1.15f32; // 指针指向左上
            p.line_segment(
                [c, c + vec2(a.cos(), a.sin()) * (r - 1.2)],
                egui::Stroke::new(1.3, color),
            );
            p.circle_filled(c, 1.15, color);
        }
        ChipIcon::Lines => {
            // 机架服务器：一个竖长方形外框，里面两条分隔线 + 左侧状态点。
            // 只用线条，厚度统一，12px 下仍然认得出是"一台机器"。
            let outer = Rect::from_center_size(c, vec2(10.4, 12.0));
            p.rect_stroke(
                outer,
                CornerRadius::same(1),
                stroke,
                egui::StrokeKind::Inside,
            );
            for f in [1.0f32 / 3.0, 2.0 / 3.0] {
                let y = outer.top() + outer.height() * f;
                p.line_segment(
                    [egui::pos2(outer.left() + 1.0, y), egui::pos2(outer.right() - 1.0, y)],
                    egui::Stroke::new(0.9, color),
                );
            }
            for i in 0..3 {
                let y = outer.top() + outer.height() * ((i as f32 + 0.5) / 3.0);
                p.circle_filled(egui::pos2(outer.left() + 2.6, y), 1.0, color);
            }
        }
        ChipIcon::Stack => {
            // 三层堆叠（按用户给的那张 23×31 参考图重画）。
            //
            // 参考图的真实几何（逐像素测量）：菱形半宽 7 / 半高 4，
            // 层与层的中线间距 4 —— 正好是半个菱形高，于是上层菱形的
            // **下顶点**恰好落在下层 V 的两个**左右顶点**连线上，看着才像
            // 一摞叠起来的平面。最上面一层画完整菱形，下面两层只画下半截
            // 的 V（上半截被上一层挡住，画出来会变成一团糊）。
            //
            // 这里按参考图实测的 7:4 画（半宽 = 半高 × 1.75）：先用 2:1
            // 试过，12px 下顶点是利落些，但总宽变成 4h，比左边的机架图标
            // 宽出一截，三个 chip 的图标看着不一样大。1.75 的斜率在这个
            // 尺寸下抗锯齿完全够用。
            //
            // 尺寸取 h=2.75 → 总宽 2w=9.6、总高 4h=11（含描边约 16×19
            // 物理像素，dpi=144），与左边"机架"图标实测 16×18 视觉等大。
            let h = 2.75f32; // 菱形半高
            let w = h * 1.75; // 半宽（参考图实测 7:4）
            let y0 = c.y - h; // 最上层菱形中线
            p.add(egui::Shape::closed_line(
                vec![
                    egui::pos2(c.x, y0 - h),
                    egui::pos2(c.x + w, y0),
                    egui::pos2(c.x, y0 + h),
                    egui::pos2(c.x - w, y0),
                ],
                stroke,
            ));
            for i in 1..3 {
                let y = y0 + h * i as f32;
                p.add(egui::Shape::line(
                    vec![
                        egui::pos2(c.x - w, y),
                        egui::pos2(c.x, y + h),
                        egui::pos2(c.x + w, y),
                    ],
                    stroke,
                ));
            }
        }
    }
}

/// 顶栏那种"胶囊信息块"（左边可带一个状态圆点）。
///
/// 返回值是 `Response`：调用方拿它去 `Popup::menu(&resp)` 弹卡片，
/// 或者直接判 `clicked()`。宽度按文字自适应，所以几个 chip 排在一起
/// 不会像固定宽度那样留下大片空白。
pub fn chip(ui: &mut Ui, label: &str, dot: Option<Color32>, active: bool) -> Response {
    chip_impl(ui, label, dot, None, active)
}

/// 带图标的胶囊块（顶栏那排用它）。
pub fn icon_chip(ui: &mut Ui, label: &str, icon: ChipIcon, active: bool) -> Response {
    chip_impl(ui, label, None, Some(icon), active)
}

fn chip_impl(
    ui: &mut Ui,
    label: &str,
    dot: Option<Color32>,
    icon: Option<ChipIcon>,
    active: bool,
) -> Response {
    let th = Theme::from_ui(ui);
    let p = &th.palette;

    let font = FontId::proportional(th.text.small);
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, p.ink);

    let pad_x = 10.0;
    // 圆点占 10px；图标占 16px（含右侧 4px 空隙）
    let lead_w = match (dot.is_some(), icon.is_some()) {
        (true, _) => 12.0,
        (_, true) => 17.0,
        _ => 0.0,
    };
    let w = pad_x * 2.0 + lead_w + galley.size().x;
    let h = 26.0;

    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());

    let bg = if active {
        p.accent_soft
    } else if resp.hovered() {
        p.hover_bg
    } else {
        Color32::TRANSPARENT
    };
    if bg != Color32::TRANSPARENT {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(th.radius.pill), bg);
    }

    let color = if active { p.accent } else { p.muted };
    let mut tx = rect.left() + pad_x;
    if let Some(c) = dot {
        ui.painter()
            .circle_filled(egui::pos2(tx + 3.0, rect.center().y), 3.2, c);
        tx += lead_w;
    } else if let Some(ic) = icon {
        paint_chip_icon(ui, egui::pos2(tx + 6.6, rect.center().y), ic, color);
        tx += lead_w;
    }
    ui.painter().galley(
        egui::pos2(tx, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );

    resp
}

/// 分段选择器（并发档位、热力图范围切换）。
///
/// 返回 `Some(值)` 表示本帧用户点选了另一个选项。
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, current: T, items: &[(T, &str)]) -> Option<T> {
    let th = Theme::from_ui(ui);
    let p = &th.palette;
    let font = FontId::proportional(th.text.small);

    let pad_x = 12.0;
    let h = 28.0;
    let widths: Vec<f32> = items
        .iter()
        .map(|(_, label)| {
            let g = ui
                .painter()
                .layout_no_wrap((*label).to_owned(), font.clone(), p.ink);
            g.size().x + pad_x * 2.0
        })
        .collect();
    let total: f32 = widths.iter().sum();
    let (bg_rect, _) = ui.allocate_exact_size(vec2(total + 4.0, h + 4.0), Sense::hover());
    ui.painter()
        .rect_filled(bg_rect, CornerRadius::same(th.radius.pill), p.hover_bg);

    let mut picked = None;
    let mut x = bg_rect.left() + 2.0;
    for (i, (value, label)) in items.iter().enumerate() {
        let rect = Rect::from_min_size(egui::pos2(x, bg_rect.top() + 2.0), vec2(widths[i], h));
        let resp = ui.interact(rect, ui.scope_id().with(("seg", i)), Sense::click());
        let on = *value == current;
        if on {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(th.radius.pill), p.card);
        }
        let color = if on {
            p.accent
        } else if resp.hovered() {
            p.ink
        } else {
            p.muted
        };
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            *label,
            font.clone(),
            color,
        );
        if resp.clicked() && !on {
            picked = Some(*value);
        }
        x += widths[i];
    }
    picked
}

/// 热力图（GitHub 贡献图那种）：`cells` 是 `(列, 行, 强度 0..=4)`。
///
/// 只负责画格子与 5 档配色；日期与统计由调用方算好传进来 ——
/// 这样周/月/年三种视图都能复用同一个控件。
///
/// 格子整体**水平居中**：周视图只有 7 格，靠左放会显得整张卡片空得莫名其妙。
pub fn heatmap(ui: &mut Ui, cells: &[(usize, usize, u8)], cols: usize, rows: usize) {
    let th = Theme::from_ui(ui);
    let p = &th.palette;

    let gap = 3.0;
    let cell = ((ui.available_width() - gap * (cols as f32 - 1.0)) / cols.max(1) as f32)
        .clamp(6.0, 18.0);
    let h = rows as f32 * cell + gap * (rows as f32 - 1.0);
    let w = cols as f32 * cell + gap * (cols as f32 - 1.0);
    let full_w = ui.available_width();
    let (slot, _) = ui.allocate_exact_size(vec2(full_w, h), Sense::hover());
    let rect = Rect::from_min_size(
        egui::pos2(slot.left() + ((full_w - w) * 0.5).max(0.0), slot.top()),
        vec2(w, h),
    );

    let levels = [
        p.hover_bg,
        p.accent.gamma_multiply(0.22),
        p.accent.gamma_multiply(0.42),
        p.accent.gamma_multiply(0.66),
        p.accent,
    ];

    for (c, r, level) in cells {
        if *c >= cols || *r >= rows {
            continue;
        }
        let x = rect.left() + *c as f32 * (cell + gap);
        let y = rect.top() + *r as f32 * (cell + gap);
        let cr = Rect::from_min_size(egui::pos2(x, y), vec2(cell, cell));
        ui.painter().rect_filled(
            cr,
            CornerRadius::same(2),
            levels[(*level as usize).min(4)],
        );
    }
}

/// 主页统计卡片：大数字 + 小标题。
pub fn stat_card(ui: &mut Ui, value: &str, label: &str, tone: Color32) {
    let th = Theme::from_ui(ui);
    card(ui, |ui| {
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(value).size(22.0).strong().color(tone));
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(label)
                    .size(th.text.small)
                    .color(th.palette.muted),
            );
        });
    });
}

/// 侧栏显隐按钮。
///
/// 图标**固定不变**：一个带竖向分隔线的圆角矩形（左边那块 = 侧栏）。
///
/// 早先画的是"点下去会变成什么"——展开时画带分隔线的方框、收起时画三条
/// 横线。用户反馈"收起来之后图标变了个样，像换了个按钮"，所以改成始终
/// 画同一个符号。`_will_expand` 保留在签名里是为了让调用方继续表达意图，
/// 图标本身不再依赖它。
pub fn sidebar_toggle(ui: &mut Ui, rect: Rect, _will_expand: bool) -> bool {
    let th = Theme::from_ui(ui);
    let p = &th.palette;
    let resp = ui.interact(
        rect,
        ui.scope_id().with("adl_sidebar_toggle"),
        Sense::click(),
    );
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(6), p.hover_bg);
    }
    let fg = if resp.hovered() { p.ink } else { p.muted };
    let stroke = egui::Stroke::new(1.2, fg);
    let c = rect.center();

    let outer = Rect::from_center_size(c, vec2(14.0, 11.2));
    ui.painter()
        .rect_stroke(outer, CornerRadius::same(2), stroke, egui::StrokeKind::Inside);
    let x = outer.left() + 4.6;
    ui.painter().line_segment(
        [
            egui::pos2(x, outer.top() + 1.0),
            egui::pos2(x, outer.bottom() - 1.0),
        ],
        stroke,
    );

    resp.clicked()
}

