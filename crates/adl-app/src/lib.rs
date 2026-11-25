//! ADL 的 CLI 共享逻辑（配置 + 历史 + 任务表），供 `adl` 二进制与 `adl-gui` 复用。
pub mod config;
pub mod history;
pub mod tasks;
