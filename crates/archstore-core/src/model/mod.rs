//! 统一数据模型与事务计划（core 与 helper 共享的唯一数据契约）。

pub mod package;
pub mod plan;

pub use package::{
    DepKind, DependencyInfo, DetailExtra, IconRef, Installed, PackageDetail, PackageId,
    PackageSource, PackageSummary, UpdateInfo, human_size,
};
