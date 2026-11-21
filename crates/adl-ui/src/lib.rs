//! ADL 自研 UI 组件库。
//!
//! 三层：
//! - [`theme`] 主题令牌（颜色/间距/圆角/字号）——换肤只改这里
//! - [`widgets`] egui 表达力不足、需要反复手写的控件
//! - [`form`] / [`table`] 声明式表单与表格——把"标签+控件+说明"和
//!   "表头+定宽列"的样板收进去
//!
//! 本体 GUI（`adl-gui`）与插件渲染器共用这一套，所以插件的声明式组件树
//! 与本体的界面长得一模一样，而不是"插件的东西看着像另一个软件"。

pub mod form;
pub mod table;
pub mod theme;
pub mod widgets;

pub use form::{Field, form, form_with_save};
pub use table::{Column, Width, table, truncated, virtual_table};
pub use theme::{Palette, Radius, Spacing, TextSize, Theme};
pub use widgets::{
    WindowButton, badge, card, cell, chip, empty_state, hairline, heatmap, hint, label_cell,
    mode_pill, nav_item, section_title, segmented, sidebar_toggle, small_button, stat_card,
    window_button,
};
