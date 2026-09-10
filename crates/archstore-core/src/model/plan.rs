//! 事务计划（TransactionPlan）：GUI 与 root 之间唯一的数据契约（project.md §5.1）。
//!
//! 安全红线：
//! 1. 绝不把用户输入拼进 shell 字符串，也不使用 sh -c。所有子进程用参数数组调用。
//! 2. 绝不通过命令行传递包名列表给 root；GUI 把计划写成临时文件，只把文件路径传给 helper。
//! 3. helper 不信任计划内容：对每个 PlanItem.name 重新执行 validate_name，
//!    并重新向系统确认该包存在于对应源。
//! 4. 不信任 reason 与依赖清单：helper 自己用 libalpm check_deps 重新计算依赖。
//! 5. 不做部分执行：校验失败则拒绝全部，退出码非 0，不执行任何子命令。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};

/// 当前计划 schema 版本。helper 遇到未知 schema 直接拒绝。
pub const PLAN_SCHEMA: u32 = 1;

/// 单个包名的最大长度。
const NAME_MAX_LEN: usize = 255;

/// 包名白名单：拒绝任何可能被解释为选项或路径的内容。
///
/// 允许：ASCII 字母数字与 `.` `_` `+` `-` `@`；
/// 拒绝：空串、超长、以 `-` 开头（会被当成命令行选项）、任何 `/`（路径分隔符）。
pub fn validate_name(name: &str) -> CoreResult<()> {
    let ok = !name.is_empty()
        && name.len() <= NAME_MAX_LEN
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-' | b'@'));
    if ok {
        Ok(())
    } else {
        Err(CoreError::InvalidName(name.to_string()))
    }
}

/// 事务种类。命令行取值见 as_str/parse。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanKind {
    /// 安装/更新官方仓库包（pacman -S / -Syu）
    PacmanSync,
    /// 卸载官方仓库包（pacman -Rns）
    PacmanRemove,
    /// Flatpak 安装
    FlatpakInstall,
    /// Flatpak 卸载
    FlatpakUninstall,
    /// Flatpak 更新
    FlatpakUpdate,
}

impl PlanKind {
    /// helper 命令行 --kind 的取值。
    pub fn as_str(&self) -> &'static str {
        match self {
            PlanKind::PacmanSync => "pacman-sync",
            PlanKind::PacmanRemove => "pacman-remove",
            PlanKind::FlatpakInstall => "flatpak-install",
            PlanKind::FlatpakUninstall => "flatpak-uninstall",
            PlanKind::FlatpakUpdate => "flatpak-update",
        }
    }

    pub fn parse(s: &str) -> CoreResult<Self> {
        match s {
            "pacman-sync" => Ok(PlanKind::PacmanSync),
            "pacman-remove" => Ok(PlanKind::PacmanRemove),
            "flatpak-install" => Ok(PlanKind::FlatpakInstall),
            "flatpak-uninstall" => Ok(PlanKind::FlatpakUninstall),
            "flatpak-update" => Ok(PlanKind::FlatpakUpdate),
            other => Err(CoreError::PlanRejected {
                reason: format!("未知的 --kind 取值：{other}"),
            }),
        }
    }

    /// 面向用户的中文名。
    pub fn label(&self) -> &'static str {
        match self {
            PlanKind::PacmanSync => "安装 / 更新官方仓库软件",
            PlanKind::PacmanRemove => "卸载官方仓库软件",
            PlanKind::FlatpakInstall => "安装 Flatpak 应用",
            PlanKind::FlatpakUninstall => "卸载 Flatpak 应用",
            PlanKind::FlatpakUpdate => "更新 Flatpak 应用",
        }
    }

    /// 该种类是否是"删除"操作。
    pub fn is_remove(&self) -> bool {
        matches!(self, PlanKind::PacmanRemove | PlanKind::FlatpakUninstall)
    }

    /// 该种类是否是 Flatpak 操作。
    pub fn is_flatpak(&self) -> bool {
        matches!(
            self,
            PlanKind::FlatpakInstall | PlanKind::FlatpakUninstall | PlanKind::FlatpakUpdate
        )
    }
}

/// Flatpak 安装位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Installation {
    System,
    User,
}

impl Installation {
    pub fn as_str(&self) -> &'static str {
        match self {
            Installation::System => "system",
            Installation::User => "user",
        }
    }

    pub fn parse(s: &str) -> CoreResult<Self> {
        match s {
            "system" => Ok(Installation::System),
            "user" => Ok(Installation::User),
            other => Err(CoreError::PlanRejected {
                reason: format!("未知的 installation 取值：{other}"),
            }),
        }
    }

    /// flatpak CLI 对应的全局参数。
    pub fn flag(&self) -> &'static str {
        match self {
            Installation::System => "--system",
            Installation::User => "--user",
        }
    }
}

/// 计划项的来源（比 PackageSource 更严格：卸载计划必须指明仓库/远程）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PlanSource {
    /// 官方仓库包。repo 仅用于校验与展示，实际执行由 pacman 依 pacman.conf 解析。
    Official { repo: String },
    /// Flatpak 应用。
    Flatpak {
        remote: String,
        installation: Installation,
    },
}

impl PlanSource {
    pub fn kind_str(&self) -> &'static str {
        match self {
            PlanSource::Official { .. } => "pacman",
            PlanSource::Flatpak { .. } => "flatpak",
        }
    }
}

/// 计划项被加入的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlanItemReason {
    /// 用户显式选择
    Explicit,
    /// 依赖带入
    Dependency,
    /// 构建依赖（AUR）
    BuildDependency,
}

impl PlanItemReason {
    pub fn label(&self) -> &'static str {
        match self {
            PlanItemReason::Explicit => "显式",
            PlanItemReason::Dependency => "依赖",
            PlanItemReason::BuildDependency => "构建依赖",
        }
    }
}

/// 计划中的一项。helper 只信任 name 的形式，不信任其存在性（会重新确认）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    pub source: PlanSource,
    /// 严格校验：见 validate_name
    pub name: String,
    pub target_version: Option<String>,
    pub reason: PlanItemReason,
}

impl PlanItem {
    /// 构造一个官方仓库计划项。
    pub fn official(repo: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            source: PlanSource::Official { repo: repo.into() },
            name: name.into(),
            target_version: None,
            reason: PlanItemReason::Explicit,
        }
    }

    /// 构造一个 Flatpak 计划项。
    pub fn flatpak(
        remote: impl Into<String>,
        installation: Installation,
        name: impl Into<String>,
    ) -> Self {
        Self {
            source: PlanSource::Flatpak {
                remote: remote.into(),
                installation,
            },
            name: name.into(),
            target_version: None,
            reason: PlanItemReason::Explicit,
        }
    }

    pub fn with_reason(mut self, reason: PlanItemReason) -> Self {
        self.reason = reason;
        self
    }

    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.target_version = Some(version.into());
        self
    }

    /// 单条项目的自我校验（helper 与 GUI 共用）。
    pub fn validate(&self, kind: PlanKind) -> CoreResult<()> {
        validate_name(&self.name)?;
        match (&self.source, kind.is_flatpak()) {
            (PlanSource::Official { repo }, false) => {
                // 仓库名来自 pacman.conf，同样不允许出现路径分隔符或前导横线
                validate_name(repo).map_err(|_| CoreError::PlanRejected {
                    reason: format!("仓库名不合法：{repo:?}"),
                })
            }
            (PlanSource::Flatpak { remote, .. }, true) => {
                // 远程名可以是 "flathub" 或 "flathub:https://..."（flatpak remotes 的输出形式）
                let head = remote.split(':').next().unwrap_or(remote);
                validate_name(head).map_err(|_| CoreError::PlanRejected {
                    reason: format!("Flatpak 远程名不合法：{remote:?}"),
                })
            }
            (PlanSource::Official { .. }, true) => Err(CoreError::PlanRejected {
                reason: format!("Flatpak 计划中出现了官方仓库包：{}", self.name),
            }),
            (PlanSource::Flatpak { .. }, false) => Err(CoreError::PlanRejected {
                reason: format!("pacman 计划中出现了 Flatpak 应用：{}", self.name),
            }),
        }
    }
}

/// 计划风险标注（仅用于 UI 展示，helper 不依赖）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanRisk {
    /// 同时包含 AUR 与官方仓库更新，可能导致部分升级。
    PartialUpgrade,
    /// 卸载会影响其它包。
    ReverseDeps { dependents: usize },
    /// 会带入构建依赖。
    BuildDependencies { count: usize },
    /// 目标包标记为 OutOfDate。
    OutOfDate { packages: Vec<String> },
    /// 计划为空。
    Empty,
}

impl PlanRisk {
    /// 面向用户的警告文案。
    pub fn message(&self) -> String {
        match self {
            PlanRisk::PartialUpgrade => {
                "同时更新官方仓库与 AUR 包可能导致部分升级（partial upgrade），建议分开执行。"
                    .into()
            }
            PlanRisk::ReverseDeps { dependents } => {
                format!("有 {dependents} 个已安装包依赖将被删除的软件。")
            }
            PlanRisk::BuildDependencies { count } => {
                format!("将额外安装 {count} 个仅构建期需要的依赖，构建后可移除。")
            }
            PlanRisk::OutOfDate { packages } => {
                format!("以下软件已被上游标记为过期：{}", packages.join("、"))
            }
            PlanRisk::Empty => "计划为空。".into(),
        }
    }

    /// 风险等级（3 = 最危险），用于排序与颜色。
    pub fn severity(&self) -> u8 {
        match self {
            PlanRisk::Empty => 0,
            PlanRisk::BuildDependencies { .. } => 1,
            PlanRisk::OutOfDate { .. } => 2,
            PlanRisk::ReverseDeps { .. } => 3,
            PlanRisk::PartialUpgrade => 3,
        }
    }
}

/// 事务的声明式描述（纯数据），可序列化为 JSON。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionPlan {
    /// 当前 = 1；helper 遇到未知 schema 直接拒绝
    pub schema: u32,
    pub created_at: u64,
    pub kind: PlanKind,
    pub items: Vec<PlanItem>,
    /// 用户可读的摘要条目（仅用于展示，helper 不信任）
    pub summary: Vec<String>,
}

impl TransactionPlan {
    /// 新建一个空计划（自动填入 schema 与当前时间）。
    pub fn new(kind: PlanKind) -> Self {
        Self {
            schema: PLAN_SCHEMA,
            created_at: now_unix(),
            kind,
            items: Vec::new(),
            summary: Vec::new(),
        }
    }

    /// 追加一项，并同步更新摘要。
    pub fn push(&mut self, item: PlanItem) {
        // 摘要的动词必须看计划类型：卸载计划写"安装"会误导用户
        // （这是端到端实测时在 helper 的输出里发现的）。
        let verb = match (self.kind.is_remove(), item.reason) {
            (true, _) => "卸载",
            (false, PlanItemReason::Explicit) => "安装",
            (false, PlanItemReason::Dependency) => "依赖",
            (false, PlanItemReason::BuildDependency) => "构建依赖",
        };
        let line = format!(
            "{} {}（{}）",
            verb,
            item.name,
            item.target_version.as_deref().unwrap_or("最新版本")
        );
        self.summary.push(line);
        self.items.push(item);
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// 计划中出现的 Flatpak 安装位置（用于 helper 决定是否提权）。
    pub fn flatpak_installation(&self) -> Option<Installation> {
        self.items.iter().find_map(|i| match &i.source {
            PlanSource::Flatpak { installation, .. } => Some(*installation),
            _ => None,
        })
    }

    /// 该计划是否可以在无 root 的情况下由 GUI 直接执行（Flatpak 用户级安装）。
    pub fn is_user_scope(&self) -> bool {
        matches!(self.flatpak_installation(), Some(Installation::User))
    }

    /// 完整校验（GUI 写盘前与 helper 读盘后都必须调用）。
    pub fn validate(&self) -> CoreResult<()> {
        if self.schema != PLAN_SCHEMA {
            return Err(CoreError::PlanRejected {
                reason: format!(
                    "计划 schema 版本 {} 不受支持（本程序支持 {}）",
                    self.schema, PLAN_SCHEMA
                ),
            });
        }
        if self.items.is_empty() {
            return Err(CoreError::PlanRejected {
                reason: "计划为空".to_string(),
            });
        }
        if self.kind == PlanKind::PacmanSync
            && self
                .items
                .iter()
                .any(|i| matches!(i.source, PlanSource::Official { .. }))
        {
            // PacmanSync 允许只含官方仓库包；AUR 由 GUI 直接以用户身份调用助手，不经过本计划。
        }
        let mut seen: Vec<(&str, &str)> = Vec::new();
        for item in &self.items {
            // 计划级的包名校验：对 helper 而言"计划里有非法包名"就是计划被拒绝，
            // 而不是表单错误（InvalidName 的语义见附录 C）
            item.validate(self.kind).map_err(|e| match e {
                CoreError::InvalidName(name) => CoreError::PlanRejected {
                    reason: format!("包名不合规：{name:?}"),
                },
                other => other,
            })?;
            let key = (item.source.kind_str(), item.name.as_str());
            if seen.contains(&key) {
                return Err(CoreError::PlanRejected {
                    reason: format!("计划中存在重复条目：{}", item.name),
                });
            }
            seen.push(key);
        }
        Ok(())
    }

    /// 计算风险标注（仅 UI 使用）。
    ///
    /// 说明：AUR 与官方仓库混批的风险由 GUI 在拆分计划时产生，
    /// 因此以 kinds 参数显式传入"本批是否包含 AUR 更新"。
    pub fn risks(&self, has_aur_update: bool) -> Vec<PlanRisk> {
        let mut out = Vec::new();
        if self.items.is_empty() {
            out.push(PlanRisk::Empty);
            return out;
        }
        if has_aur_update && self.kind == PlanKind::PacmanSync {
            out.push(PlanRisk::PartialUpgrade);
        }
        let build = self
            .items
            .iter()
            .filter(|i| i.reason == PlanItemReason::BuildDependency)
            .count();
        if build > 0 {
            out.push(PlanRisk::BuildDependencies { count: build });
        }
        out
    }

    /// 序列化为 JSON（安全的落盘形式）。
    pub fn to_json(&self) -> CoreResult<String> {
        serde_json::to_string_pretty(self).map_err(|e| CoreError::Internal(e.to_string()))
    }

    /// 从 JSON 反序列化并校验。
    pub fn from_json(text: &str) -> CoreResult<Self> {
        let plan: TransactionPlan = serde_json::from_str(text).map_err(|e| CoreError::Parse {
            context: "TransactionPlan".to_string(),
            raw_head: crate::error::raw_head(text, 3) + " / " + &e.to_string(),
        })?;
        plan.validate()?;
        Ok(plan)
    }

    /// 计划文件的标准路径（崩溃恢复用）。
    pub fn last_plan_path(cache_dir: &Path) -> PathBuf {
        cache_dir.join("plans").join("last.json")
    }

    /// 把计划写入一个 0700 目录下的 0600 文件，返回文件路径。
    ///
    /// 目录与文件权限是安全边界的一部分：helper 会重新检查属主与权限。
    pub fn write_to_dir(&self, dir: &Path, file_name: &str) -> CoreResult<PathBuf> {
        self.validate()?;
        if file_name.contains('/') || file_name.contains("..") {
            return Err(CoreError::InvalidName(file_name.to_string()));
        }
        std::fs::create_dir_all(dir)?;
        set_mode(dir, 0o700)?;
        let path = dir.join(file_name);
        write_private(&path, self.to_json()?.as_bytes())?;
        Ok(path)
    }
}

/// 当前 Unix 时间戳（秒）。系统时钟早于 epoch 时返回 0。
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 以 0600 权限原子写入文件（写入临时文件后 rename）。
pub fn write_private(path: &Path, data: &[u8]) -> CoreResult<()> {
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = tmp_sibling(path);
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 生成同目录下的临时文件名（保证 rename 是同一文件系统内的原子操作）。
pub fn tmp_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "plan".to_string());
    let pid = std::process::id();
    path.with_file_name(format!(".{name}.{pid}.tmp"))
}

/// 设置目录/文件权限（仅 Unix）。
pub fn set_mode(path: &Path, mode: u32) -> CoreResult<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

/// 读取计划文件（0600 权限检查由 helper 侧的 validate_plan_path 完成）。
pub fn read_plan_file(path: &Path) -> CoreResult<TransactionPlan> {
    let text = std::fs::read_to_string(path)?;
    TransactionPlan::from_json(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_name_accepts_real_world_names() {
        for ok in [
            "firefox",
            "gtk3",
            "python-pip",
            "lib32-libgl",
            "org.mozilla.firefox",
            "g++",
            "c++",
            "nodejs@lts",
            "a_b",
        ] {
            assert!(validate_name(ok).is_ok(), "{ok} should be valid");
        }
    }

    #[test]
    fn validate_name_rejects_injection_attempts() {
        for bad in [
            "",
            "-rf",
            "--noconfirm",
            "a/b",
            "../../etc/passwd",
            "a b",
            "a;rm -rf /",
            "a$(id)",
            "a|b",
            "a&b",
            "a\nb",
            "a'b",
            "a\"b",
            "a*b",
            "a?b",
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(validate_name(&"a".repeat(256)).is_err());
        assert!(validate_name(&"a".repeat(255)).is_ok());
    }

    #[test]
    fn plan_json_roundtrip() {
        let mut plan = TransactionPlan::new(PlanKind::PacmanSync);
        plan.push(PlanItem::official("extra", "firefox").with_version("155.0.1-1"));
        plan.push(PlanItem::official("extra", "gtk3").with_reason(PlanItemReason::Dependency));
        let json = plan.to_json().expect("serialize");
        let back = TransactionPlan::from_json(&json).expect("deserialize");
        assert_eq!(plan, back);
        assert_eq!(back.len(), 2);
        assert_eq!(back.summary.len(), 2);
    }

    #[test]
    fn summary_verb_follows_plan_kind() {
        // 卸载计划的摘要不能写"安装"
        let mut sync = TransactionPlan::new(PlanKind::PacmanSync);
        sync.push(PlanItem::official("extra", "firefox"));
        assert_eq!(sync.summary[0], "安装 firefox（最新版本）");

        let mut remove = TransactionPlan::new(PlanKind::PacmanRemove);
        remove.push(PlanItem::official("extra", "firefox"));
        assert_eq!(remove.summary[0], "卸载 firefox（最新版本）");

        let mut fp_remove = TransactionPlan::new(PlanKind::FlatpakUninstall);
        fp_remove.push(PlanItem::flatpak(
            "flathub",
            Installation::System,
            "org.mozilla.firefox",
        ));
        assert!(fp_remove.summary[0].starts_with("卸载"));

        // 依赖项即使出现在卸载计划里也用"卸载"
        let mut dep_in_remove = TransactionPlan::new(PlanKind::PacmanRemove);
        dep_in_remove
            .push(PlanItem::official("extra", "gtk3").with_reason(PlanItemReason::Dependency));
        assert!(dep_in_remove.summary[0].starts_with("卸载"));
    }

    #[test]
    fn plan_rejects_unknown_schema_and_empty_plans() {
        let mut plan = TransactionPlan::new(PlanKind::PacmanSync);
        plan.push(PlanItem::official("extra", "vim"));
        let mut json: serde_json::Value =
            serde_json::from_str(&plan.to_json().expect("ser")).expect("json");
        json["schema"] = serde_json::json!(99);
        let err = TransactionPlan::from_json(&json.to_string()).expect_err("must reject");
        assert!(matches!(err, CoreError::PlanRejected { .. }));

        let empty = TransactionPlan::new(PlanKind::PacmanSync);
        assert!(empty.validate().is_err());
    }

    #[test]
    fn plan_rejects_source_kind_mismatch() {
        let mut plan = TransactionPlan::new(PlanKind::FlatpakInstall);
        plan.items.push(PlanItem::official("extra", "firefox"));
        assert!(plan.validate().is_err());

        let mut plan = TransactionPlan::new(PlanKind::PacmanSync);
        plan.items.push(PlanItem::flatpak(
            "flathub",
            Installation::System,
            "org.mozilla.firefox",
        ));
        assert!(plan.validate().is_err());
    }

    #[test]
    fn plan_rejects_duplicates_and_bad_names() {
        let mut plan = TransactionPlan::new(PlanKind::PacmanSync);
        plan.items.push(PlanItem::official("extra", "vim"));
        plan.items.push(PlanItem::official("extra", "vim"));
        assert!(plan.validate().is_err());

        let mut plan = TransactionPlan::new(PlanKind::PacmanSync);
        plan.items.push(PlanItem::official("extra", "-evil"));
        let err = plan.validate().expect_err("bad name");
        assert!(
            matches!(err, CoreError::PlanRejected { .. }),
            "计划级校验必须给出 PlanRejected，实际 {err:?}"
        );
    }

    #[test]
    fn plan_kind_cli_roundtrip() {
        for k in [
            PlanKind::PacmanSync,
            PlanKind::PacmanRemove,
            PlanKind::FlatpakInstall,
            PlanKind::FlatpakUninstall,
            PlanKind::FlatpakUpdate,
        ] {
            assert_eq!(PlanKind::parse(k.as_str()).expect("parse"), k);
        }
        assert!(PlanKind::parse("rm-rf").is_err());
    }

    #[test]
    fn user_scope_detection() {
        let mut plan = TransactionPlan::new(PlanKind::FlatpakInstall);
        plan.items.push(PlanItem::flatpak(
            "flathub",
            Installation::User,
            "org.mozilla.firefox",
        ));
        assert!(plan.is_user_scope());
        assert_eq!(plan.flatpak_installation(), Some(Installation::User));
    }

    #[test]
    fn write_to_dir_uses_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tmpdir");
        let target = dir.path().join("plans");
        let mut plan = TransactionPlan::new(PlanKind::PacmanSync);
        plan.push(PlanItem::official("extra", "vim"));
        let path = plan.write_to_dir(&target, "last.json").expect("write");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let dir_mode = std::fs::metadata(&target)
            .expect("stat dir")
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        let back = read_plan_file(&path).expect("read back");
        assert_eq!(back, plan);
    }

    #[test]
    fn write_to_dir_rejects_traversal_file_name() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut plan = TransactionPlan::new(PlanKind::PacmanSync);
        plan.push(PlanItem::official("extra", "vim"));
        assert!(plan.write_to_dir(dir.path(), "../evil.json").is_err());
        assert!(plan.write_to_dir(dir.path(), "a/b.json").is_err());
    }
}
