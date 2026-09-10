//! 统一数据模型：PackageId / PackageSource / Installed / PackageSummary / PackageDetail。
//!
//! 设计要点（见 project.md §4.1）：
//! - IconRef 只描述"从哪里取图标"，不在数据模型里持有 gdk::Texture（否则 core 会依赖 GTK）。
//! - Installed 用枚举而非 Option，避免"已安装但版本未知"这类非法状态。
//! - 所有公共结构体派生 Clone；跨线程只传拥有所有权的数据。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 软件来源。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PackageSource {
    /// core / extra / multilib / 自定义仓库。
    Official { repo: String },
    /// AUR。
    Aur,
    /// Flatpak 远程仓库（flathub / 其他）。
    Flatpak { remote: String },
}

impl PackageSource {
    /// 用于 UI 徽章与后端分流的短标识："pacman" | "aur" | "flatpak"。
    pub fn kind(&self) -> &'static str {
        match self {
            PackageSource::Official { .. } => "pacman",
            PackageSource::Aur => "aur",
            PackageSource::Flatpak { .. } => "flatpak",
        }
    }

    /// 面向用户的中文来源名。
    pub fn display(&self) -> String {
        match self {
            PackageSource::Official { repo } => format!("官方仓库 {repo}"),
            PackageSource::Aur => "AUR".to_string(),
            PackageSource::Flatpak { remote } => format!("Flatpak {remote}"),
        }
    }

    /// 仓库/远程名（Official 与 Flatpak 有，AUR 无）。
    pub fn repo_name(&self) -> Option<&str> {
        match self {
            PackageSource::Official { repo } => Some(repo),
            PackageSource::Flatpak { remote } => Some(remote),
            PackageSource::Aur => None,
        }
    }
}

/// 包的唯一标识（来源 + 名称）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PackageId {
    pub source: PackageSource,
    /// Official/Aur: 包名；Flatpak: application id（如 org.mozilla.firefox）
    pub name: String,
}

impl PackageId {
    pub fn official(repo: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            source: PackageSource::Official { repo: repo.into() },
            name: name.into(),
        }
    }

    pub fn aur(name: impl Into<String>) -> Self {
        Self {
            source: PackageSource::Aur,
            name: name.into(),
        }
    }

    pub fn flatpak(remote: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            source: PackageSource::Flatpak {
                remote: remote.into(),
            },
            name: name.into(),
        }
    }

    pub fn kind(&self) -> &'static str {
        self.source.kind()
    }
}

impl std::fmt::Display for PackageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.source {
            PackageSource::Official { repo } => write!(f, "{}/{}", repo, self.name),
            PackageSource::Aur => write!(f, "aur/{}", self.name),
            PackageSource::Flatpak { remote } => write!(f, "{}:{}", remote, self.name),
        }
    }
}

/// 安装状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Installed {
    No,
    /// version: 已安装版本；explicit: 是否用户显式安装（false = 依赖带入）
    Yes {
        version: String,
        explicit: bool,
    },
}

impl Installed {
    pub fn is_yes(&self) -> bool {
        matches!(self, Installed::Yes { .. })
    }

    pub fn version(&self) -> Option<&str> {
        match self {
            Installed::Yes { version, .. } => Some(version),
            Installed::No => None,
        }
    }

    /// 是否"依赖带入"（非显式安装）。
    pub fn is_dependency(&self) -> bool {
        matches!(
            self,
            Installed::Yes {
                explicit: false,
                ..
            }
        )
    }

    pub fn is_explicit(&self) -> bool {
        matches!(self, Installed::Yes { explicit: true, .. })
    }
}

/// 图标引用：只描述来源，不持有位图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum IconRef {
    /// 本地图标名（主题图标，如 "firefox"）
    IconName(String),
    /// 本地已缓存的图标文件（下载完成后填入）
    CachedFile(PathBuf),
    /// 远程 URL（详情页懒下载）
    Remote(String),
    #[default]
    Missing,
}

/// 可用更新信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub current: String,
    pub candidate: String,
    pub download_size: Option<u64>,
}

/// 列表页展示的一行数据：字段少、可缓存、无网络依赖。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PackageSummary {
    pub id: PackageId,
    pub display_name: String,
    /// 单行描述（<= 120 字符，超出截断）
    pub summary: String,
    pub version: Option<String>,
    pub installed: Installed,
    /// 有可用更新时填充
    pub update: Option<UpdateInfo>,
    pub icon: IconRef,
    /// AUR Popularity
    pub popularity: Option<f64>,
    /// AUR NumVotes
    pub votes: Option<u32>,
    /// AUR OutOfDate / Flatpak EOL
    pub out_of_date: bool,
}

impl PackageSummary {
    /// 构造一个最小可用的摘要（其余字段填充默认值）。
    pub fn minimal(id: PackageId, display_name: impl Into<String>) -> Self {
        Self {
            id,
            display_name: display_name.into(),
            summary: String::new(),
            version: None,
            installed: Installed::No,
            update: None,
            icon: IconRef::Missing,
            popularity: None,
            votes: None,
            out_of_date: false,
        }
    }

    /// 描述截断到 120 字符（按字符边界，不会切碎 UTF-8）。
    pub fn set_summary(&mut self, text: &str) {
        const MAX: usize = 120;
        let one_line = text.replace(['\n', '\r'], " ");
        let trimmed = one_line.trim();
        if trimmed.chars().count() <= MAX {
            self.summary = trimmed.to_string();
        } else {
            let mut s: String = trimmed.chars().take(MAX - 1).collect();
            s.push('…');
            self.summary = s;
        }
    }

    pub fn is_installed(&self) -> bool {
        self.installed.is_yes()
    }

    pub fn has_update(&self) -> bool {
        self.update.is_some()
    }
}

/// 依赖种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DepKind {
    /// 运行时依赖
    Runtime,
    /// 构建依赖（AUR makedepends）
    Make,
    /// 测试依赖（checkdepends）
    Check,
    /// 可选依赖（optdepends）
    Optional,
    /// Flatpak runtime
    RuntimeRef,
    /// Flatpak 扩展
    Extension,
}

impl DepKind {
    pub fn label(&self) -> &'static str {
        match self {
            DepKind::Runtime => "运行时依赖",
            DepKind::Make => "构建依赖",
            DepKind::Check => "测试依赖",
            DepKind::Optional => "可选依赖",
            DepKind::RuntimeRef => "运行时",
            DepKind::Extension => "扩展",
        }
    }

    /// 是否属于"仅构建期需要"（UI 用灰底标注并可提示构建后移除）。
    pub fn is_build_only(&self) -> bool {
        matches!(self, DepKind::Make | DepKind::Check)
    }
}

/// 单条依赖信息（展示与计划构建共用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyInfo {
    /// 原始依赖表达式，如 "gtk3>=3.24"
    pub name: String,
    pub kind: DepKind,
    /// optdepends 的用途说明
    pub description: Option<String>,
    /// 已满足时指向提供者（含 virtual provides）
    pub satisfied_by: Option<PackageId>,
    pub missing: bool,
    /// 需要下载时的体积
    pub size: Option<u64>,
    /// 可选依赖中的"推荐"项
    pub recommended: bool,
}

impl DependencyInfo {
    /// 解析依赖表达式中的包名（去掉版本约束与描述）。
    ///
    /// 支持的形式：name、name>=1.2、name=1.2、name<1.2、optdepends 的 "name: description"。
    pub fn parse(expr: &str) -> (String, Option<String>) {
        let (head, desc) = match expr.split_once(':') {
            Some((h, d)) => (h.trim(), Some(d.trim().to_string())),
            None => (expr.trim(), None),
        };
        let end = head.find(['<', '>', '=']).unwrap_or(head.len());
        (head[..end].trim().to_string(), desc)
    }

    /// 从表达式构造一条"未检查"的依赖记录。
    pub fn from_expr(expr: &str, kind: DepKind) -> Self {
        let (name, description) = Self::parse(expr);
        Self {
            name,
            kind,
            description,
            satisfied_by: None,
            missing: true,
            size: None,
            recommended: false,
        }
    }
}

/// 后端特有字段的只读展示（键值对列表）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetailExtra(pub Vec<(String, String)>);

impl DetailExtra {
    pub fn push(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.0.push((key.into(), value.into()));
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// 详情页数据：摘要 + 可能来自网络的完整字段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PackageDetail {
    pub summary: PackageSummary,
    /// 长描述（可含 Pango markup，渲染前必须校验）
    pub description: String,
    pub licenses: Vec<String>,
    pub homepage: Option<String>,
    pub download_size: Option<u64>,
    pub installed_size: Option<u64>,
    pub maintainer: Option<String>,
    pub screenshots: Vec<String>,
    /// 0.0..=5.0
    pub rating: Option<f32>,
    pub review_count: Option<u32>,
    /// 仅 Flatpak：文件系统/总线权限
    pub permissions: Vec<String>,
    pub dependencies: Vec<DependencyInfo>,
    pub extra: DetailExtra,
}

impl PackageDetail {
    pub fn from_summary(summary: PackageSummary) -> Self {
        Self {
            summary,
            description: String::new(),
            licenses: Vec::new(),
            homepage: None,
            download_size: None,
            installed_size: None,
            maintainer: None,
            screenshots: Vec::new(),
            rating: None,
            review_count: None,
            permissions: Vec::new(),
            dependencies: Vec::new(),
            extra: DetailExtra::default(),
        }
    }
}

/// 人类可读的字节数（1024 进制，与 pacman/GNOME Software 的展示习惯一致）。
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_expr_parsing() {
        assert_eq!(DependencyInfo::parse("gtk3>=3.24").0, "gtk3");
        assert_eq!(DependencyInfo::parse("glibc").0, "glibc");
        let (n, d) = DependencyInfo::parse("ffmpeg: 视频解码支持");
        assert_eq!(n, "ffmpeg");
        assert_eq!(d.as_deref(), Some("视频解码支持"));
    }

    #[test]
    fn summary_truncation_is_char_safe() {
        let mut s = PackageSummary::minimal(PackageId::aur("x"), "x");
        s.set_summary(&"中".repeat(200));
        assert_eq!(s.summary.chars().count(), 120);
        assert!(s.summary.ends_with('…'));
    }

    #[test]
    fn human_size_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(2048), "2.0 KiB");
        assert_eq!(human_size(1024 * 1024 * 3 / 2), "1.5 MiB");
    }

    #[test]
    fn source_kinds() {
        assert_eq!(PackageId::aur("yay").kind(), "aur");
        assert_eq!(PackageId::official("extra", "vim").kind(), "pacman");
        assert_eq!(PackageId::flatpak("flathub", "org.x.Y").kind(), "flatpak");
    }

    #[test]
    fn package_id_json_roundtrip() {
        for id in [
            PackageId::aur("yay"),
            PackageId::official("extra", "firefox"),
            PackageId::flatpak("flathub", "org.mozilla.firefox"),
        ] {
            let json = serde_json::to_string(&id).expect("serialize");
            let back: PackageId = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(id, back);
        }
    }
}
