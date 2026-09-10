//! Pacman 后端：只读查询，全部走 alpm 工作线程（project.md §4.3）。
//!
//! 只读查询完全不需要 root，因为 /var/lib/pacman/{local,sync} 是 0755 且文件 0644。

use std::sync::Arc;

use async_trait::async_trait;

use crate::backend::pacman_worker::{AlpmOp, AlpmPayload, AlpmWorker, RepoStatus};
use crate::backend::{Capability, Category, PackageBackend, Page, SearchScope};
use crate::error::{CoreError, CoreResult};
use crate::model::{DependencyInfo, PackageDetail, PackageId, PackageSummary};

/// 官方仓库后端。持有 alpm 工作线程的句柄，自身不接触 libalpm。
pub struct PacmanBackend {
    worker: Arc<AlpmWorker>,
    capability: Capability,
}

impl std::fmt::Debug for PacmanBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PacmanBackend")
            .field("capability", &self.capability)
            .finish_non_exhaustive()
    }
}

impl PacmanBackend {
    /// 启动 alpm 工作线程并等待首次探测完成。
    pub async fn spawn() -> CoreResult<Self> {
        let worker = AlpmWorker::spawn()?;
        let statuses = worker.wait_ready().await?;
        Ok(Self::from_statuses(worker, &statuses))
    }

    /// 用已知的仓库状态构造（避免重复等待）。
    pub fn from_statuses(worker: Arc<AlpmWorker>, statuses: &[RepoStatus]) -> Self {
        Self {
            capability: capability_from(statuses),
            worker,
        }
    }

    /// 共享的 alpm 工作线程（GUI 全局只应有一个）。
    pub fn worker(&self) -> Arc<AlpmWorker> {
        Arc::clone(&self.worker)
    }

    /// 当前同步库状态。
    pub fn repo_status(&self) -> Vec<RepoStatus> {
        self.worker.repo_status()
    }

    /// 当前可用的仓库名。
    pub fn available_repos(&self) -> Vec<String> {
        self.worker.available_repos()
    }

    /// 重新打开句柄并刷新仓库状态（事务后或用户点"重新检测"）。
    pub async fn refresh(&self) -> CoreResult<Vec<RepoStatus>> {
        match self.worker.request(AlpmOp::Refresh).await? {
            AlpmPayload::Repos(r) => Ok(r),
            _ => Ok(self.worker.repo_status()),
        }
    }

    /// 分类页用：按仓库取包组。
    pub async fn group_members(&self, category: &str) -> CoreResult<Vec<PackageSummary>> {
        match self
            .worker
            .request(AlpmOp::GroupMembers {
                group: category.to_string(),
            })
            .await?
        {
            AlpmPayload::Summaries(v) => Ok(v),
            _ => Err(CoreError::Internal("group_members 返回了意外载荷".into())),
        }
    }
}

/// 由仓库状态汇总后端能力。
pub fn capability_from(statuses: &[RepoStatus]) -> Capability {
    if statuses.is_empty() {
        return Capability::unavailable(
            "未检测到可用的同步数据库，请先运行 sudo pacman -Sy（本程序不代为执行）",
        );
    }
    let ready: Vec<&RepoStatus> = statuses.iter().filter(|s| s.available).collect();
    if ready.is_empty() {
        let reasons: Vec<String> = statuses.iter().filter_map(|s| s.reason.clone()).collect();
        return Capability::unavailable(format!(
            "所有同步库均不可用（{}）。请先运行 sudo pacman -Sy（本程序不代为执行）",
            reasons.join("；")
        ));
    }
    let broken: Vec<String> = statuses
        .iter()
        .filter(|s| !s.available)
        .map(|s| s.name.clone())
        .collect();
    if broken.is_empty() {
        Capability::available()
    } else {
        Capability::available_with(Some(format!(
            "{} 未同步，请先运行 sudo pacman -Sy",
            broken.join("、")
        )))
    }
}

#[async_trait]
impl PackageBackend for PacmanBackend {
    fn source_kind(&self) -> &'static str {
        "pacman"
    }

    fn capability(&self) -> &Capability {
        &self.capability
    }

    async fn search(
        &self,
        query: &str,
        scope: SearchScope,
    ) -> Result<Vec<PackageSummary>, CoreError> {
        if !self.capability.is_available() {
            return Err(CoreError::BackendUnavailable {
                kind: "pacman".into(),
                reason: self
                    .capability
                    .reason
                    .clone()
                    .unwrap_or_else(|| "同步数据库不可用".into()),
            });
        }
        let repos = match scope {
            // LocalOnly：只查本地库，绝不联网（本地库查询本来就不联网）
            SearchScope::LocalOnly => vec!["local".to_string()],
            SearchScope::Full => {
                let mut r = self.available_repos();
                r.push("local".to_string());
                r
            }
        };
        match self
            .worker
            .request(AlpmOp::Search {
                query: query.to_string(),
                repos,
                limit: 500,
            })
            .await?
        {
            AlpmPayload::Summaries(v) => Ok(v),
            _ => Err(CoreError::Internal("search 返回了意外载荷".into())),
        }
    }

    async fn info(&self, id: &PackageId) -> Result<PackageDetail, CoreError> {
        match self
            .worker
            .request(AlpmOp::Info {
                name: id.name.clone(),
            })
            .await?
        {
            AlpmPayload::Detail(d) => Ok(*d),
            _ => Err(CoreError::Internal("info 返回了意外载荷".into())),
        }
    }

    async fn installed(&self) -> Result<Vec<PackageSummary>, CoreError> {
        match self.worker.request(AlpmOp::Installed).await? {
            AlpmPayload::Summaries(v) => Ok(v),
            _ => Err(CoreError::Internal("installed 返回了意外载荷".into())),
        }
    }

    async fn upgradable(&self) -> Result<Vec<PackageSummary>, CoreError> {
        match self.worker.request(AlpmOp::Upgradable).await? {
            AlpmPayload::Summaries(v) => Ok(v),
            _ => Err(CoreError::Internal("upgradable 返回了意外载荷".into())),
        }
    }

    async fn categories(&self) -> Result<Vec<Category>, CoreError> {
        match self.worker.request(AlpmOp::Groups).await? {
            AlpmPayload::Groups(v) => Ok(v),
            _ => Err(CoreError::Internal("categories 返回了意外载荷".into())),
        }
    }

    async fn list_category(
        &self,
        category: &str,
        page: Page,
    ) -> Result<Vec<PackageSummary>, CoreError> {
        let all = match self
            .worker
            .request(AlpmOp::GroupMembers {
                group: category.to_string(),
            })
            .await?
        {
            AlpmPayload::Summaries(v) => v,
            _ => return Err(CoreError::Internal("list_category 返回了意外载荷".into())),
        };
        Ok(page.slice(&all).to_vec())
    }

    async fn dependencies(&self, id: &PackageId) -> Result<Vec<DependencyInfo>, CoreError> {
        match self
            .worker
            .request(AlpmOp::Deps {
                name: id.name.clone(),
            })
            .await?
        {
            AlpmPayload::Deps(v) => Ok(v),
            _ => Err(CoreError::Internal("dependencies 返回了意外载荷".into())),
        }
    }

    async fn reverse_dependencies(&self, id: &PackageId) -> Result<Vec<PackageId>, CoreError> {
        match self
            .worker
            .request(AlpmOp::RevDeps {
                name: id.name.clone(),
            })
            .await?
        {
            AlpmPayload::RevDeps(v) => Ok(v),
            _ => Err(CoreError::Internal(
                "reverse_dependencies 返回了意外载荷".into(),
            )),
        }
    }

    async fn unneeded_dependencies(
        &self,
        targets: &[PackageId],
    ) -> Result<Vec<PackageId>, CoreError> {
        let removing: Vec<String> = targets.iter().map(|t| t.name.clone()).collect();
        match self.worker.request(AlpmOp::Unneeded { removing }).await? {
            AlpmPayload::Unneeded(v) => Ok(v),
            _ => Err(CoreError::Internal(
                "unneeded_dependencies 返回了意外载荷".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_repo(name: &str) -> RepoStatus {
        RepoStatus {
            name: name.into(),
            available: true,
            packages: 10,
            reason: None,
        }
    }

    #[test]
    fn capability_available_when_all_repos_ready() {
        let cap = capability_from(&[ok_repo("core"), ok_repo("extra")]);
        assert!(cap.is_available());
        assert!(cap.reason.is_none());
    }

    #[test]
    fn capability_warns_when_some_repos_missing() {
        let cap = capability_from(&[
            ok_repo("core"),
            RepoStatus {
                name: "multilib".into(),
                available: false,
                packages: 0,
                reason: Some("未同步".into()),
            },
        ]);
        assert!(cap.is_available());
        assert!(cap.reason.expect("warn").contains("multilib"));
    }

    #[test]
    fn capability_unavailable_when_no_repo_ready() {
        let cap = capability_from(&[RepoStatus {
            name: "core".into(),
            available: false,
            packages: 0,
            reason: Some("未同步".into()),
        }]);
        assert!(!cap.is_available());
        assert!(cap.reason.expect("reason").contains("pacman -Sy"));
    }

    #[test]
    fn capability_unavailable_when_no_repos_at_all() {
        let cap = capability_from(&[]);
        assert!(!cap.is_available());
        assert!(cap.reason.expect("reason").contains("pacman -Sy"));
    }
}
