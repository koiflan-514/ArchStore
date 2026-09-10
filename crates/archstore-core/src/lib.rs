//! ArchStore 业务核心：无 GTK 依赖的模型、后端、缓存、配置与事务计划。
//!
//! 依赖方向约束（§3.2）：archstore-gui -> archstore-core <- archstore-helper。
//! 本 crate 的依赖里不得出现 gtk4/libadwaita（CI 用 cargo tree 校验）。

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]
#![warn(missing_debug_implementations)]

pub mod aur_run;
pub mod backend;
pub mod cache;
pub mod config;
pub mod desktop_icons;
pub mod env;
pub mod error;
pub mod flathub;
pub mod i18n;
pub mod icons;
pub mod model;
pub mod net;
pub mod plan;
pub mod translate;

pub use error::{CoreError, CoreResult};

/// crate 版本（用于 --version 与日志）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 应用 ID（desktop 文件、polkit action、gschema 共用）。
pub const APP_ID: &str = "io.github.archstore.ArchStore";

/// 提权用的 polkit action id。
pub const POLKIT_ACTION: &str = "io.github.archstore.ArchStore.transaction";
