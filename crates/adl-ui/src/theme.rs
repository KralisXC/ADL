//! 主题令牌化：所有颜色、间距、圆角、字号集中在这里。
//!
//! 目的：换肤（浅色/深色/赛博风）时只改这一个文件，不用在几千行 UI 代码里
//! 找 `Color32::from_rgb(...)`。以前 `C_TEAL`/`C_BG`/... 散在 `main.rs` 里，
//! 想加深色模式得改几十处。
//!
//! 存储：进程级 `RwLock<Theme>` 单例。为什么不放 egui 的临时数据？
//! `Id::new` 在 0.37 已弃用，而主题是一个"全局常量"性质的东西，
//! 每个进程只有一套主题，静态变量更直接（也就省下了层层传 `&Theme`）。

use std::sync::{OnceLock, RwLock};

use egui::{Color32, CornerRadius, FontId, Margin, Style, TextStyle};

/// 调色板：Motrix 清爽风（浅青底 + 白卡 + 青绿强调）。
#[derive(Clone, Debug)]
pub struct Palette {
    /// 强调色：选中态、进度条、主按钮
    pub accent: Color32,
    /// 强调色的浅底：悬停、选中背景
    pub accent_soft: Color32,
    /// 主按钮悬停
    pub accent_hover: Color32,
    /// 页面底色
    pub bg: Color32,
    /// 卡片/侧栏/顶栏底色
    pub card: Color32,
    /// 分割线、卡片描边
    pub line: Color32,
    /// 极细线：只在"内容被截断/滚动"处出现（表头下沿、滚动区边缘）。
    /// 比 `line` 更淡，肉眼几乎看不出是一条线，只当作边界暗示。
    pub hair: Color32,
    /// 次要文字（表头、说明、速度）
    pub muted: Color32,
    /// 正文
    pub ink: Color32,
    /// 悬停浅底（中性）
    pub hover_bg: Color32,
    /// 错误红字
    pub danger: Color32,
    /// 警告棕字（信任模式胶囊）
    pub warn: Color32,
    /// 警告黄底
    pub warn_bg: Color32,
    /// 成功浅底（状态条）
    pub ok_bg: Color32,
    /// 错误浅底（状态条）
    pub err_bg: Color32,
}

impl Default for Palette {
    fn default() -> Self {
        Self::light()
    }
}

impl Palette {
    pub fn light() -> Self {
        Self {
            accent: Color32::from_rgb(0x3D, 0x8B, 0x7A),
            accent_soft: Color32::from_rgb(0xE2, 0xF1, 0xED),
            accent_hover: Color32::from_rgb(0x33, 0x7A, 0x6B),
            bg: Color32::from_rgb(0xF4, 0xF8, 0xF8),
            card: Color32::WHITE,
            line: Color32::from_rgb(0xE4, 0xED, 0xEC),
            hair: Color32::from_rgb(0xEE, 0xF3, 0xF2),
            muted: Color32::from_rgb(0x8A, 0x9A, 0x98),
            ink: Color32::from_rgb(0x2A, 0x33, 0x33),
            hover_bg: Color32::from_rgb(0xEC, 0xF3, 0xF2),
            danger: Color32::from_rgb(0xB0, 0x30, 0x30),
            warn: Color32::from_rgb(0x8B, 0x5A, 0x10),
            warn_bg: Color32::from_rgb(0xFD, 0xF2, 0xDC),
            ok_bg: Color32::from_rgb(0xEC, 0xF7, 0xF4),
            err_bg: Color32::from_rgb(0xFD, 0xEF, 0xEF),
        }
    }
}

/// 间距与尺寸令牌。
#[derive(Clone, Debug)]
pub struct Spacing {
    /// 侧边栏宽度
    pub sidebar_w: f32,
    /// 侧边栏收起后的宽度（只留一条窄轨道）
    pub sidebar_w_collapsed: f32,
    /// 自绘标题栏高度（窗口无系统边框时用）
    pub titlebar_h: f32,
    /// 顶栏高度
    pub topbar_h: f32,
    /// 导航项高度
    pub nav_h: f32,
    /// 表格行高
    pub row_h: f32,
    /// 表单控件高度
    pub field_h: f32,
    /// 表单左侧标签列宽
    pub form_label_w: f32,
    /// 表单右侧控件列宽
    pub form_ctrl_w: f32,
    /// 表格列间距
    pub col_gap: f32,
    /// 卡片内边距
    pub card_pad: i8,
    /// 内容区内边距（水平, 垂直）
    pub content_pad: (i8, i8),
}

impl Default for Spacing {
    fn default() -> Self {
        Self {
            sidebar_w: 212.0,
            sidebar_w_collapsed: 56.0,
            titlebar_h: 40.0,
            topbar_h: 54.0,
            nav_h: 36.0,
            row_h: 30.0,
            field_h: 26.0,
            form_label_w: 210.0,
            form_ctrl_w: 380.0,
            col_gap: 10.0,
            card_pad: 16,
            content_pad: (20, 16),
        }
    }
}

/// 圆角令牌。
#[derive(Clone, Debug)]
pub struct Radius {
    pub card: u8,
    pub button: u8,
    pub nav: u8,
    pub pill: u8,
    pub input: u8,
}

impl Default for Radius {
    fn default() -> Self {
        Self {
            card: 12,
            button: 7,
            nav: 8,
            pill: 11,
            input: 8,
        }
    }
}

/// 字号令牌。
#[derive(Clone, Debug)]
pub struct TextSize {
    pub body: f32,
    pub small: f32,
    pub hint: f32,
    pub brand: f32,
    pub title: f32,
    pub section: f32,
    pub button: f32,
}

impl Default for TextSize {
    fn default() -> Self {
        Self {
            body: 14.5,
            small: 12.0,
            hint: 11.0,
            brand: 20.0,
            title: 16.0,
            section: 13.0,
            button: 13.5,
        }
    }
}

/// 完整主题。
#[derive(Clone, Debug, Default)]
pub struct Theme {
    pub palette: Palette,
    pub spacing: Spacing,
    pub radius: Radius,
    pub text: TextSize,
}

static THEME: OnceLock<RwLock<Theme>> = OnceLock::new();

fn slot() -> &'static RwLock<Theme> {
    THEME.get_or_init(|| RwLock::new(Theme::light()))
}

impl Theme {
    pub fn light() -> Self {
        Self {
            palette: Palette::light(),
            spacing: Spacing::default(),
            radius: Radius::default(),
            text: TextSize::default(),
        }
    }

    /// 替换当前主题（进程级）。
    pub fn install(&self) {
        if let Ok(mut w) = slot().write() {
            *w = self.clone();
        }
    }

    /// 读取当前主题。
    pub fn get() -> Self {
        slot()
            .read()
            .map(|r| r.clone())
            .unwrap_or_else(|_| Theme::light())
    }

    /// 从 Ui 取主题（保留此签名，方便将来做"每个窗口不同主题"）。
    pub fn from_ui(_ui: &egui::Ui) -> Self {
        Self::get()
    }

    /// 从 Context 取主题。
    pub fn of(_ctx: &egui::Context) -> Self {
        Self::get()
    }

    /// 把令牌翻译成 egui 的 `Style`（全局视觉 + 字号）。
    ///
    /// 这一步等价于原来 `App::new` 里那一大段 `Visuals` 手工赋值，
    /// 但颜色全部来自 `self.palette`，换肤只需换 `Palette`。
    pub fn apply(&self, style: &mut Style) {
        let p = &self.palette;
        let r = &self.radius;
        let t = &self.text;

        let mut v = egui::Visuals::light();
        v.panel_fill = p.bg;
        v.window_fill = p.card;
        v.extreme_bg_color = p.bg;
        v.faint_bg_color = p.accent_soft;
        v.override_text_color = Some(p.ink);
        v.hyperlink_color = p.accent;
        v.selection.bg_fill = p.accent_soft;
        v.selection.stroke = egui::Stroke::new(1.0, p.accent);
        v.window_corner_radius = CornerRadius::same(r.card + 2);
        v.menu_corner_radius = CornerRadius::same(r.card);

        let cr = CornerRadius::same(r.input);
        v.widgets.noninteractive.corner_radius = cr;
        v.widgets.inactive.corner_radius = cr;
        v.widgets.hovered.corner_radius = cr;
        v.widgets.active.corner_radius = cr;
        v.widgets.open.corner_radius = cr;

        v.widgets.noninteractive.bg_fill = p.card;
        v.widgets.noninteractive.weak_bg_fill = p.card;
        v.widgets.inactive.bg_fill = p.accent_soft;
        v.widgets.inactive.weak_bg_fill = p.accent_soft;
        v.widgets.hovered.bg_fill = p.accent_soft;
        v.widgets.hovered.weak_bg_fill = p.accent_soft;
        v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, p.accent);
        v.widgets.active.bg_fill = p.accent;
        v.widgets.active.weak_bg_fill = p.accent;
        v.widgets.active.fg_stroke = egui::Stroke::new(1.0, Color32::WHITE);
        style.visuals = v;

        style.spacing.item_spacing = egui::vec2(10.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 7.0);
        style.spacing.interact_size.y = 30.0;
        style.spacing.scroll.bar_width = 10.0;

        style
            .text_styles
            .insert(TextStyle::Body, FontId::proportional(t.body));
        style
            .text_styles
            .insert(TextStyle::Button, FontId::proportional(t.body));
        style
            .text_styles
            .insert(TextStyle::Heading, FontId::proportional(t.body + 7.0));
        style
            .text_styles
            .insert(TextStyle::Small, FontId::proportional(t.small));
    }

    /// 一次性装好主题 + 样式（`App::new` 里调用）。
    pub fn install_and_apply(&self, ctx: &egui::Context) {
        self.install();
        let mut style = (*ctx.style_of(egui::Theme::Light)).clone();
        self.apply(&mut style);
        ctx.set_style_of(egui::Theme::Light, style.clone());
        ctx.set_style_of(egui::Theme::Dark, style);
    }

    // ── 便捷取值 ──

    /// 卡片：白底 + 圆角 + 极细描边 + 内边距
    ///
    /// 描边用 `hair` 而不是 `line`：卡片靠"白底浮在浅底上"产生层次，
    /// 描边只是防止完全融在一起，不该有"框"的感觉。
    pub fn card_frame(&self) -> egui::Frame {
        egui::Frame::new()
            .fill(self.palette.card)
            .corner_radius(CornerRadius::same(self.radius.card))
            .stroke(egui::Stroke::new(1.0, self.palette.hair))
            .inner_margin(Margin::same(self.spacing.card_pad))
    }

    /// 自绘标题栏：白底，无描边（分隔靠下面的顶栏同色、内容区浅底）。
    pub fn titlebar_frame(&self) -> egui::Frame {
        egui::Frame::new()
            .fill(self.palette.card)
            .inner_margin(Margin::symmetric(10, 0))
    }

    /// 左侧整高边栏：白底
    pub fn sidebar_frame(&self) -> egui::Frame {
        egui::Frame::new()
            .fill(self.palette.card)
            .inner_margin(Margin::symmetric(12, 14))
    }

    /// 内容区：浅底 + 内边距（保留给"独立卡片式内容区"的旧用法，
    /// 现在主界面走 `shell_frame()`）
    pub fn content_frame(&self) -> egui::Frame {
        let (x, y) = self.spacing.content_pad;
        egui::Frame::new()
            .fill(self.palette.bg)
            .inner_margin(Margin::symmetric(x, y))
    }

    /// 右侧主区外壳：浅底 + **左上圆角**，把顶栏 chips 与任务列表包成同一块。
    ///
    /// 为什么要圆角：侧栏与标题栏是白底，主区是浅底，两者交界处如果是个
    /// 直角，看起来像两块贴在一起的纸；左上角切一个圆角，主区就像一张
    /// 卡片压在白色外壳上，层次自然出来（用户明确要求"左上角做圆角设计"）。
    ///
    /// 只切左上（`nw`），其余三角保持直角：另外三个角贴着窗口边缘，
    /// 切了反而会在边缘露出白边。
    pub fn shell_frame(&self) -> egui::Frame {
        let (x, _) = self.spacing.content_pad;
        egui::Frame::new()
            .fill(self.palette.bg)
            .corner_radius(CornerRadius {
                nw: self.radius.card + 4,
                ne: 0,
                sw: 0,
                se: 0,
            })
            .inner_margin(Margin {
                left: x,
                right: x,
                top: 8,
                bottom: 0,
            })
    }

    /// 顶栏（chips 那一行）：**透明**，让下面的浅底透出来。
    ///
    /// 以前这里是白底 `topbar_frame()`，结果侧栏白底 + 顶栏白底 + 内容浅底
    /// 之间出现一条横向的白/灰分界带。现在顶栏并进主区的浅底里，
    /// 只靠间距与 chips 本身区分层级。
    pub fn topbar_frame(&self) -> egui::Frame {
        egui::Frame::new().inner_margin(Margin::symmetric(0, 6))
    }
}
