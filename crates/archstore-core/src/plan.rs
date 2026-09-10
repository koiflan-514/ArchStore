//! 计划构建：把"用户的选择 + 后端事实"变成可序列化的事务计划（project.md §5.1 / §9.3）。
//!
//! 本模块只产出纯数据，绝不执行任何系统变更。执行完全由 archstore-helper 负责。

use std::sync::Arc;

use crate::backend::PackageBackend;
use crate::error::{CoreError, CoreResult};
use crate::model::plan::{
    Installation, PlanItem, PlanItemReason, PlanKind, PlanRisk, PlanSource, TransactionPlan,
};
use crate::model::{DepKind, DependencyInfo, PackageId, PackageSource, PackageSummary};

/// 安装计划的构建选项。
#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    /// 用户勾选的可选依赖（原始依赖表达式或包名）
    pub optional: Vec<String>,
    /// 是否把"缺失的运行时依赖"一并加入计划（默认 true）
    pub include_dependencies: bool,
    /// 虚拟依赖的具体提供者选择：依赖名 -> 选中的包名
    pub virtual_choices: Vec<(String, String)>,
}

/// AUR 相关的执行请求（不走 pkexec，也不进入 TransactionPlan）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AurRequest {
    pub action: AurAction,
    pub packages: Vec<String>,
}

/// AUR 助手的操作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurAction {
    Install,
    Remove,
    Update,
}

impl AurAction {
    pub fn label(&self) -> &'static str {
        match self {
            AurAction::Install => "安装",
            AurAction::Remove => "卸载",
            AurAction::Update => "更新",
        }
    }
}

/// 构建结果：可能同时包含"需要提权的计划"和"以用户身份执行的 AUR 请求"。
#[derive(Debug, Clone, Default)]
pub struct BuildOutcome {
    pub plans: Vec<TransactionPlan>,
    /// AUR 请求（GUI 以用户身份打开终端执行）
    pub aur: Vec<AurRequest>,
    /// 需要向用户展示的警告（部分升级等）
    pub risks: Vec<PlanRisk>,
}

impl BuildOutcome {
    pub fn is_empty(&self) -> bool {
        self.plans.iter().all(|p| p.is_empty()) && self.aur.is_empty()
    }

    /// 计划中的条目总数。
    pub fn item_count(&self) -> usize {
        self.plans.iter().map(|p| p.len()).sum::<usize>()
            + self.aur.iter().map(|a| a.packages.len()).sum::<usize>()
    }

    /// 所有条目的人类可读摘要（计划栏展示）。
    pub fn summary_lines(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for p in &self.plans {
            out.extend(p.summary.iter().cloned());
        }
        for a in &self.aur {
            for pkg in &a.packages {
                out.push(format!("{}（AUR）{}", a.action.label(), pkg));
            }
        }
        out
    }
}

/// 判断一个依赖是否需要用户处理（缺失且非构建期）。
fn actionable(dep: &DependencyInfo) -> bool {
    dep.missing && !dep.kind.is_build_only()
}

/// 从依赖列表中挑出需要加入计划的条目。
pub fn select_dependencies(
    deps: &[DependencyInfo],
    options: &InstallOptions,
) -> Vec<DependencyInfo> {
    let mut out: Vec<DependencyInfo> = Vec::new();
    for dep in deps {
        let selected = match dep.kind {
            // 可选依赖默认不勾选（§8.2 硬性规则 1）
            DepKind::Optional => options
                .optional
                .iter()
                .any(|o| o == &dep.name || DependencyInfo::parse(o).0 == dep.name),
            _ => options.include_dependencies && actionable(dep),
        };
        if selected && !out.iter().any(|d| d.name == dep.name) {
            out.push(dep.clone());
        }
    }
    out
}

/// 构建安装计划（官方仓库 + Flatpak 分开）。
///
/// 后端只提供只读事实，计划构建完全在这里完成。
pub async fn build_install_plan(
    backends: &[Arc<dyn PackageBackend>],
    targets: &[PackageId],
    options: &InstallOptions,
) -> CoreResult<BuildOutcome> {
    let mut outcome = BuildOutcome::default();

    let pacman_targets: Vec<&PackageId> = targets
        .iter()
        .filter(|t| matches!(t.source, PackageSource::Official { .. }))
        .collect();
    let flatpak_targets: Vec<&PackageId> = targets
        .iter()
        .filter(|t| matches!(t.source, PackageSource::Flatpak { .. }))
        .collect();
    let aur_targets: Vec<&PackageId> = targets
        .iter()
        .filter(|t| t.source == PackageSource::Aur)
        .collect();

    if !pacman_targets.is_empty() {
        let backend = find_backend(backends, "pacman")?;
        let mut plan = TransactionPlan::new(PlanKind::PacmanSync);
        for id in &pacman_targets {
            crate::model::plan::validate_name(&id.name)?;
            let repo = id.source.repo_name().unwrap_or_default().to_string();
            let detail = backend.info(id).await?;
            let mut item = PlanItem::official(repo.clone(), id.name.clone());
            if let Some(v) = &detail.summary.version {
                item = item.with_version(v.clone());
            }
            plan.push(item);
            for dep in select_dependencies(&detail.dependencies, options) {
                let choice = options
                    .virtual_choices
                    .iter()
                    .find(|(name, _)| name == &dep.name)
                    .map(|(_, chosen)| chosen.clone());
                let provider = choice
                    .or_else(|| dep.satisfied_by.as_ref().map(|p| p.name.clone()))
                    .unwrap_or_else(|| dep.name.clone());
                if plan.items.iter().any(|i| i.name == provider) {
                    continue;
                }
                let mut extra = PlanItem::official(repo.clone(), provider);
                extra.reason = PlanItemReason::Dependency;
                if let Some(id) = &dep.satisfied_by {
                    extra.source = match &id.source {
                        PackageSource::Official { repo } => {
                            PlanSource::Official { repo: repo.clone() }
                        }
                        _ => PlanSource::Official { repo: repo.clone() },
                    };
                }
                if let Some(size) = dep.size {
                    extra.target_version = extra
                        .target_version
                        .clone()
                        .or_else(|| Some(format!("{} 字节", size)));
                }
                plan.push(extra);
            }
        }
        validate_or_reject(&plan)?;
        outcome.plans.push(plan);
    }

    if !flatpak_targets.is_empty() {
        let backend = find_backend(backends, "flatpak")?;
        for id in &flatpak_targets {
            crate::model::plan::validate_name(&id.name)?;
            let remote = id.source.repo_name().unwrap_or("flathub").to_string();
            let installation = backend_installation(backend);
            let detail = backend.info(id).await?;
            let mut plan = TransactionPlan::new(PlanKind::FlatpakInstall);
            let mut item = PlanItem::flatpak(remote.clone(), installation, id.name.clone());
            if let Some(v) = &detail.summary.version {
                item = item.with_version(v.clone());
            }
            plan.push(item);
            validate_or_reject(&plan)?;
            outcome.plans.push(plan);
        }
    }

    if !aur_targets.is_empty() {
        for id in &aur_targets {
            crate::model::plan::validate_name(&id.name)?;
        }
        outcome.aur.push(AurRequest {
            action: AurAction::Install,
            packages: aur_targets.iter().map(|i| i.name.clone()).collect(),
        });
    }

    if outcome.plans.is_empty() && outcome.aur.is_empty() {
        return Err(CoreError::PlanRejected {
            reason: "没有可执行的条目".into(),
        });
    }
    Ok(outcome)
}

/// 构建删除计划（§9.3）。
///
/// 1) 反依赖检查（非 root，只读）
/// 2) 有反依赖且 !cascade -> 返回 Err(CoreError::ReverseDeps{..})，由 UI 询问用户
/// 3) 生成 PlanKind::PacmanRemove / FlatpakUninstall
/// 4) helper 会再次独立校验并自行计算依赖
pub async fn build_remove_plan(
    backends: &[Arc<dyn PackageBackend>],
    targets: &[PackageId],
    cascade: bool,
) -> CoreResult<BuildOutcome> {
    let mut outcome = BuildOutcome::default();

    let pacman_targets: Vec<&PackageId> = targets
        .iter()
        .filter(|t| matches!(t.source, PackageSource::Official { .. }))
        .collect();
    let flatpak_targets: Vec<&PackageId> = targets
        .iter()
        .filter(|t| matches!(t.source, PackageSource::Flatpak { .. }))
        .collect();
    let aur_targets: Vec<&PackageId> = targets
        .iter()
        .filter(|t| t.source == PackageSource::Aur)
        .collect();

    if !pacman_targets.is_empty() {
        let backend = find_backend(backends, "pacman")?;
        let mut plan = TransactionPlan::new(PlanKind::PacmanRemove);

        // 1) 反依赖检查。注意保留 PackageId 而不只是包名：
        //    后续生成计划项时要用它自己的仓库/来源（曾经只留名字，
        //    计划里的仓库名就成了空串，级联删除必然校验失败）。
        let mut all_dependents: Vec<PackageId> = Vec::new();
        for id in &pacman_targets {
            let dependents = backend.reverse_dependencies(id).await.unwrap_or_default();
            for d in dependents {
                if !all_dependents.iter().any(|p| p.name == d.name)
                    && !pacman_targets.iter().any(|t| t.name == d.name)
                {
                    all_dependents.push(d);
                }
            }
        }
        if !all_dependents.is_empty() && !cascade {
            let target = pacman_targets
                .first()
                .map(|t| t.name.clone())
                .unwrap_or_default();
            return Err(CoreError::ReverseDeps {
                target,
                count: all_dependents.len(),
                dependents: all_dependents.iter().map(|d| d.name.clone()).collect(),
            });
        }
        outcome.risks.push(PlanRisk::ReverseDeps {
            dependents: all_dependents.len(),
        });

        for id in &pacman_targets {
            crate::model::plan::validate_name(&id.name)?;
            let repo = id.source.repo_name().unwrap_or_default().to_string();
            plan.push(PlanItem::official(repo, id.name.clone()));
        }
        // 级联删除：pacman -Rns 语义，但清理范围必须展示在计划里
        if cascade {
            for dependent in &all_dependents {
                let repo = dependent.source.repo_name().unwrap_or_default().to_string();
                let mut item = PlanItem::official(repo, dependent.name.clone());
                item.reason = PlanItemReason::Dependency;
                plan.push(item);
            }
        }

        // 2) "多余依赖"：目标包（以及用户确认级联删除的包）删掉之后，
        //    不再被任何已安装包需要的依赖也要一并删除，而且必须**逐条列进计划**。
        //    pacman -Rns 本来就会隐式清理它们，但隐式清理正是 §9.3 明令禁止的：
        //    用户必须在执行前看到完整的清理范围。
        //
        //    计算必须一次性传入完整的删除集合：两个待删包共同依赖的包不是多余依赖。
        let mut removing: Vec<PackageId> = pacman_targets.iter().map(|t| (*t).clone()).collect();
        removing.extend(all_dependents.iter().cloned());
        let unneeded = backend
            .unneeded_dependencies(&removing)
            .await
            .unwrap_or_default();
        for id in &unneeded {
            if plan.items.iter().any(|i| i.name == id.name)
                || removing.iter().any(|r| r.name == id.name)
            {
                continue;
            }
            let repo = id.source.repo_name().unwrap_or_default().to_string();
            let item =
                PlanItem::official(repo, id.name.clone()).with_reason(PlanItemReason::Unneeded);
            plan.push(item);
        }

        validate_or_reject(&plan)?;
        outcome.plans.push(plan);
    }

    if !flatpak_targets.is_empty() {
        let backend = find_backend(backends, "flatpak")?;
        for id in &flatpak_targets {
            crate::model::plan::validate_name(&id.name)?;
            let remote = id.source.repo_name().unwrap_or("flathub").to_string();
            let installation = backend_installation(backend);
            let mut plan = TransactionPlan::new(PlanKind::FlatpakUninstall);
            plan.push(PlanItem::flatpak(remote, installation, id.name.clone()));
            validate_or_reject(&plan)?;
            outcome.plans.push(plan);
        }
    }

    if !aur_targets.is_empty() {
        for id in &aur_targets {
            crate::model::plan::validate_name(&id.name)?;
        }
        outcome.aur.push(AurRequest {
            action: AurAction::Remove,
            packages: aur_targets.iter().map(|i| i.name.clone()).collect(),
        });
    }

    if outcome.plans.is_empty() && outcome.aur.is_empty() {
        return Err(CoreError::PlanRejected {
            reason: "没有可执行的条目".into(),
        });
    }
    Ok(outcome)
}

/// 构建更新计划。
///
/// **AUR 与官方仓库的更新不能混在一次执行里**（Arch 明确不建议部分升级），
/// 因此这里始终拆成两个顺序动作，并在 UI 中给出警告（§5.3）。
pub async fn build_update_plan(
    backends: &[Arc<dyn PackageBackend>],
    include_flatpak: bool,
) -> CoreResult<BuildOutcome> {
    let mut outcome = BuildOutcome::default();

    if let Ok(pacman) = find_backend(backends, "pacman") {
        let upgradable = pacman.upgradable().await.unwrap_or_default();
        let (official, aur_updates) = split_updates(&upgradable);
        if !official.is_empty() {
            let mut plan = TransactionPlan::new(PlanKind::PacmanSync);
            for s in &official {
                crate::model::plan::validate_name(&s.id.name)?;
                let mut item = PlanItem::official(
                    s.id.source.repo_name().unwrap_or_default().to_string(),
                    s.id.name.clone(),
                );
                if let Some(u) = &s.update {
                    item = item.with_version(u.candidate.clone());
                }
                plan.push(item);
            }
            validate_or_reject(&plan)?;
            outcome.plans.push(plan);
        }
        if !aur_updates.is_empty() {
            outcome.aur.push(AurRequest {
                action: AurAction::Update,
                packages: aur_updates,
            });
        }
    }

    if include_flatpak && let Ok(flatpak) = find_backend(backends, "flatpak") {
        let upgradable = flatpak.upgradable().await.unwrap_or_default();
        if !upgradable.is_empty() {
            let installation = backend_installation(flatpak);
            let mut plan = TransactionPlan::new(PlanKind::FlatpakUpdate);
            for s in &upgradable {
                crate::model::plan::validate_name(&s.id.name)?;
                let remote = s.id.source.repo_name().unwrap_or("flathub").to_string();
                let mut item = PlanItem::flatpak(remote, installation, s.id.name.clone());
                if let Some(u) = &s.update {
                    item = item.with_version(u.candidate.clone());
                }
                plan.push(item);
            }
            validate_or_reject(&plan)?;
            outcome.plans.push(plan);
        }
    }

    if outcome.plans.is_empty() && outcome.aur.is_empty() {
        return Err(CoreError::PlanRejected {
            reason: "系统已是最新，没有可执行的条目".into(),
        });
    }
    Ok(outcome)
}

/// 把可更新列表拆成"官方仓库包"与"AUR 外来包"。
///
/// 返回 (官方仓库可更新项, AUR 包名列表)。
pub fn split_updates(items: &[PackageSummary]) -> (Vec<PackageSummary>, Vec<String>) {
    let mut official = Vec::new();
    let mut aur = Vec::new();
    for s in items {
        match &s.id.source {
            PackageSource::Official { .. } => official.push(s.clone()),
            PackageSource::Aur => aur.push(s.id.name.clone()),
            // Flatpak 有独立的更新计划
            PackageSource::Flatpak { .. } => {}
        }
    }
    aur.sort();
    aur.dedup();
    (official, aur)
}

/// 官方仓库包必须整批更新：返回"用户只勾选了一部分"的检测结果。
pub fn detect_partial_upgrade(selected: &[PackageId], all_upgradable: &[PackageSummary]) -> bool {
    let official_total = all_upgradable
        .iter()
        .filter(|s| matches!(s.id.source, PackageSource::Official { .. }))
        .count();
    let official_selected = selected
        .iter()
        .filter(|s| matches!(s.source, PackageSource::Official { .. }))
        .count();
    official_selected > 0 && official_selected < official_total
}

/// 在给定后端集合中按 source_kind 查找。
pub fn find_backend<'a>(
    backends: &'a [Arc<dyn PackageBackend>],
    kind: &str,
) -> CoreResult<&'a Arc<dyn PackageBackend>> {
    backends
        .iter()
        .find(|b| b.source_kind() == kind)
        .ok_or_else(|| CoreError::BackendUnavailable {
            kind: kind.to_string(),
            reason: "该后端未启用".into(),
        })
}

fn backend_installation(backend: &Arc<dyn PackageBackend>) -> Installation {
    // Flatpak 后端的安装位置来自配置；默认 system（需要提权）
    match backend.source_kind() {
        "flatpak" => Installation::System,
        _ => Installation::System,
    }
}

fn validate_or_reject(plan: &TransactionPlan) -> CoreResult<()> {
    plan.validate().map_err(|e| match e {
        CoreError::PlanRejected { reason } => CoreError::PlanRejected { reason },
        other => other,
    })
}

/// 供 UI 展示的风险提示汇总（含 AUR 混批警告）。
pub fn summarize_risks(outcome: &BuildOutcome) -> Vec<PlanRisk> {
    let mut risks = outcome.risks.clone();
    let has_aur = !outcome.aur.is_empty();
    let has_official = outcome.plans.iter().any(|p| p.kind == PlanKind::PacmanSync);
    if has_aur && has_official {
        risks.push(PlanRisk::PartialUpgrade);
    }
    risks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Capability, Page, SearchScope};
    use crate::model::{Installed, UpdateInfo};

    /// 只实现卸载计划需要的三个方法的假后端（其余一律不可用）。
    ///
    /// 之所以要这个假后端：卸载计划的"清理多余依赖"是纯装配逻辑，
    /// 用真实 libalpm 数据没法稳定复现"共同依赖/级联/外来包"这些边界。
    struct RemoveMock {
        rev: Vec<PackageId>,
        unneeded: Vec<PackageId>,
        /// 记录 unneeded_dependencies 收到的完整删除集合
        seen: std::sync::Mutex<Vec<String>>,
    }

    impl RemoveMock {
        fn new(rev: Vec<PackageId>, unneeded: Vec<PackageId>) -> Self {
            Self {
                rev,
                unneeded,
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn seen(&self) -> Vec<String> {
            self.seen.lock().expect("lock").clone()
        }
    }

    #[async_trait::async_trait]
    impl PackageBackend for RemoveMock {
        fn source_kind(&self) -> &'static str {
            "pacman"
        }

        fn capability(&self) -> &Capability {
            // 能力对象需要返回值引用：用一份静态的可用能力
            static CAP: std::sync::OnceLock<Capability> = std::sync::OnceLock::new();
            CAP.get_or_init(Capability::available)
        }

        async fn search(
            &self,
            _query: &str,
            _scope: SearchScope,
        ) -> CoreResult<Vec<PackageSummary>> {
            Ok(Vec::new())
        }

        async fn info(&self, id: &PackageId) -> CoreResult<crate::model::PackageDetail> {
            Err(CoreError::NotFound(id.name.clone()))
        }

        async fn installed(&self) -> CoreResult<Vec<PackageSummary>> {
            Ok(Vec::new())
        }

        async fn upgradable(&self) -> CoreResult<Vec<PackageSummary>> {
            Ok(Vec::new())
        }

        async fn categories(&self) -> CoreResult<Vec<crate::backend::Category>> {
            Ok(Vec::new())
        }

        async fn list_category(
            &self,
            _category: &str,
            _page: Page,
        ) -> CoreResult<Vec<PackageSummary>> {
            Ok(Vec::new())
        }

        async fn dependencies(&self, _id: &PackageId) -> CoreResult<Vec<DependencyInfo>> {
            Ok(Vec::new())
        }

        async fn reverse_dependencies(&self, _id: &PackageId) -> CoreResult<Vec<PackageId>> {
            Ok(self.rev.clone())
        }

        async fn unneeded_dependencies(&self, targets: &[PackageId]) -> CoreResult<Vec<PackageId>> {
            self.seen
                .lock()
                .expect("lock")
                .extend(targets.iter().map(|t| t.name.clone()));
            Ok(self.unneeded.clone())
        }
    }

    fn backends(mock: RemoveMock) -> Vec<Arc<dyn PackageBackend>> {
        vec![Arc::new(mock)]
    }

    #[tokio::test]
    async fn remove_plan_lists_unneeded_dependencies_and_uses_full_removal_set() {
        let mock = RemoveMock::new(
            // 级联删除到的反向依赖是一个外来包（没有同步库归属 → 空仓库名）
            vec![PackageId::aur("pygtk-demo")],
            vec![PackageId::official("extra", "aalib")],
        );
        let outcome = build_remove_plan(
            &backends(mock),
            &[PackageId::official("extra", "gst-plugins-good")],
            true,
        )
        .await
        .expect("级联卸载计划必须能构建");

        let plan = &outcome.plans[0];
        assert_eq!(plan.kind, PlanKind::PacmanRemove);
        // 目标 + 连带删除 + 多余依赖，一个都不能少
        let names: Vec<&str> = plan.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["gst-plugins-good", "pygtk-demo", "aalib"]);

        let reasons: Vec<PlanItemReason> = plan.items.iter().map(|i| i.reason).collect();
        assert_eq!(
            reasons,
            vec![
                PlanItemReason::Explicit,
                PlanItemReason::Dependency,
                PlanItemReason::Unneeded
            ]
        );
        // 摘要必须把"多余依赖"与目标包区分开（§9.3 展示清理范围）
        assert_eq!(plan.summary[0], "卸载 gst-plugins-good");
        assert_eq!(plan.summary[1], "连带卸载 pygtk-demo");
        assert_eq!(plan.summary[2], "清理多余依赖 aalib");
        // 计划必须自校验通过：级联删除到的外来包允许空仓库名（旧实现会在这里失败）
        plan.validate().expect("卸载计划必须有效");
    }

    #[tokio::test]
    async fn unneeded_dependencies_receive_targets_plus_cascaded_dependents() {
        // "两个待删包共同依赖一个依赖"这种场景只有在删除集完整时才判得对，
        // 因此必须一次性把 目标包 + 级联依赖 一起传给后端。
        let mock = Arc::new(RemoveMock::new(
            vec![PackageId::official("extra", "app")],
            Vec::new(),
        ));
        let backends: Vec<Arc<dyn PackageBackend>> = vec![mock.clone()];
        build_remove_plan(&backends, &[PackageId::official("extra", "lib")], true)
            .await
            .expect("plan");
        let mut seen = mock.seen();
        seen.sort();
        assert_eq!(seen, vec!["app".to_string(), "lib".to_string()]);
    }

    #[tokio::test]
    async fn remove_plan_without_cascade_reports_reverse_deps() {
        let mock = RemoveMock::new(vec![PackageId::official("extra", "app")], Vec::new());
        let err = build_remove_plan(
            &backends(mock),
            &[PackageId::official("extra", "lib")],
            false,
        )
        .await
        .expect_err("有反向依赖且未级联时必须报 ReverseDeps");
        match err {
            CoreError::ReverseDeps {
                target,
                count,
                dependents,
            } => {
                assert_eq!(target, "lib");
                assert_eq!(count, 1);
                assert_eq!(dependents, vec!["app".to_string()]);
            }
            other => panic!("期望 ReverseDeps，实际 {other:?}"),
        }
    }

    fn summary(name: &str, source: PackageSource, update: bool) -> PackageSummary {
        let mut s = PackageSummary::minimal(
            PackageId {
                source,
                name: name.into(),
            },
            name,
        );
        if update {
            s.installed = Installed::Yes {
                version: "1.0".into(),
                explicit: true,
            };
            s.update = Some(UpdateInfo {
                current: "1.0".into(),
                candidate: "2.0".into(),
                download_size: None,
            });
        }
        s
    }

    #[test]
    fn split_updates_separates_official_aur_and_flatpak() {
        let items = vec![
            summary(
                "firefox",
                PackageSource::Official {
                    repo: "extra".into(),
                },
                true,
            ),
            summary("yay", PackageSource::Aur, true),
            summary(
                "org.mozilla.firefox",
                PackageSource::Flatpak {
                    remote: "flathub".into(),
                },
                true,
            ),
            summary(
                "vim",
                PackageSource::Official {
                    repo: "extra".into(),
                },
                true,
            ),
        ];
        let (official, aur) = split_updates(&items);
        assert_eq!(official.len(), 2);
        assert_eq!(aur, vec!["yay".to_string()]);
    }

    #[test]
    fn partial_upgrade_detection_only_counts_official() {
        let all = vec![
            summary(
                "a",
                PackageSource::Official {
                    repo: "extra".into(),
                },
                true,
            ),
            summary(
                "b",
                PackageSource::Official {
                    repo: "extra".into(),
                },
                true,
            ),
            summary("yay", PackageSource::Aur, true),
        ];
        let sel = vec![PackageId::official("extra", "a")];
        assert!(detect_partial_upgrade(&sel, &all));
        let sel = vec![
            PackageId::official("extra", "a"),
            PackageId::official("extra", "b"),
        ];
        assert!(!detect_partial_upgrade(&sel, &all));
        let sel = vec![PackageId::aur("yay")];
        assert!(!detect_partial_upgrade(&sel, &all), "只选 AUR 不算部分升级");
    }

    #[test]
    fn optional_dependencies_are_off_by_default() {
        let deps = vec![
            DependencyInfo {
                name: "gtk3".into(),
                kind: DepKind::Runtime,
                description: None,
                satisfied_by: None,
                missing: true,
                size: Some(1024),
                recommended: false,
            },
            DependencyInfo {
                name: "ffmpeg".into(),
                kind: DepKind::Optional,
                description: Some("视频解码".into()),
                satisfied_by: None,
                missing: true,
                size: Some(2048),
                recommended: false,
            },
            DependencyInfo {
                name: "go".into(),
                kind: DepKind::Make,
                description: None,
                satisfied_by: None,
                missing: true,
                size: Some(4096),
                recommended: false,
            },
            DependencyInfo {
                name: "glibc".into(),
                kind: DepKind::Runtime,
                description: None,
                satisfied_by: Some(PackageId::official("core", "glibc")),
                missing: false,
                size: None,
                recommended: false,
            },
        ];
        let opts = InstallOptions {
            include_dependencies: true,
            ..Default::default()
        };
        let picked = select_dependencies(&deps, &opts);
        let names: Vec<&str> = picked.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["gtk3"], "构建依赖与可选依赖默认不进计划");

        let opts = InstallOptions {
            include_dependencies: true,
            optional: vec!["ffmpeg".into()],
            ..Default::default()
        };
        let names: Vec<String> = select_dependencies(&deps, &opts)
            .iter()
            .map(|d| d.name.clone())
            .collect();
        assert!(names.contains(&"ffmpeg".to_string()));
        assert!(names.contains(&"gtk3".to_string()));
    }

    #[test]
    fn select_dependencies_skips_satisfied_ones() {
        let deps = vec![DependencyInfo {
            name: "glibc".into(),
            kind: DepKind::Runtime,
            description: None,
            satisfied_by: Some(PackageId::official("core", "glibc")),
            missing: false,
            size: None,
            recommended: false,
        }];
        let opts = InstallOptions {
            include_dependencies: true,
            ..Default::default()
        };
        assert!(select_dependencies(&deps, &opts).is_empty());
    }

    #[test]
    fn build_outcome_summary_and_counts() {
        let mut outcome = BuildOutcome::default();
        let mut plan = TransactionPlan::new(PlanKind::PacmanSync);
        plan.push(PlanItem::official("extra", "firefox"));
        outcome.plans.push(plan);
        outcome.aur.push(AurRequest {
            action: AurAction::Install,
            packages: vec!["cowsay".into()],
        });
        assert_eq!(outcome.item_count(), 2);
        assert!(!outcome.is_empty());
        let lines = outcome.summary_lines();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("AUR"));
    }

    #[test]
    fn summarize_risks_warns_on_mixed_updates() {
        let mut outcome = BuildOutcome::default();
        let mut plan = TransactionPlan::new(PlanKind::PacmanSync);
        plan.push(PlanItem::official("extra", "firefox"));
        outcome.plans.push(plan);
        assert!(summarize_risks(&outcome).is_empty());

        outcome.aur.push(AurRequest {
            action: AurAction::Update,
            packages: vec!["yay".into()],
        });
        let risks = summarize_risks(&outcome);
        assert!(
            risks.contains(&PlanRisk::PartialUpgrade),
            "官方 + AUR 混批必须警告部分升级"
        );
    }

    #[test]
    fn find_backend_reports_missing_kind() {
        let backends: Vec<Arc<dyn PackageBackend>> = Vec::new();
        let err = find_backend(&backends, "aur")
            .map(|_| ())
            .expect_err("must fail");
        assert!(matches!(err, CoreError::BackendUnavailable { .. }));
    }
}
