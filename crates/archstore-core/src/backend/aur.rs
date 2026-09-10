//! AUR 后端：raur + AUR RPC v5（project.md §4.4）。
//!
//! 关键结论：依赖信息可以直接从 RPC info 得到，无需下载解析 .SRCINFO。
//!
//! 与 v0.1.0 的差异（照抄会失败）：
//! - search/info 是 trait 方法，use raur::Raur; 必须在作用域内。
//! - raur::Cache 是进程内去重集合，不是持久化缓存；磁盘缓存由本项目 cache::Cache 负责。
//! - raur::Package 字段是 make_depends/opt_depends/check_depends/num_votes，
//!   不是 RPC 文档里的 MakeDepends/NumVotes。
//! - description / url / maintainer / out_of_date 是 Option，不要直接 unwrap。
//! - default-features = false 会关掉 async feature，必须显式加回（见 §2.2）。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use raur::{Raur, SearchBy};

use crate::backend::{Capability, Category, InstalledIndex, PackageBackend, Page, SearchScope};
use crate::cache::{Cache, ttl};
use crate::error::{CoreError, CoreResult};
use crate::model::{
    DepKind, DependencyInfo, IconRef, Installed, PackageDetail, PackageId, PackageSource,
    PackageSummary, UpdateInfo,
};
use crate::net::{CancelToken, HttpClient, RetryPolicy, human_reqwest_error};

/// AUR RPC 批量 info 的每批上限（§4.4）。
pub const INFO_BATCH: usize = 100;
/// AUR 搜索结果的展示上限（resultcount 可达 508，全量渲染没有意义）。
pub const SEARCH_LIMIT: usize = 300;

/// AUR 分类页使用固定的关键词集合（AUR 没有"列出全部关键词"的端点）。
const KEYWORD_CATEGORIES: [(&str, &str); 12] = [
    ("browser", "浏览器"),
    ("editor", "编辑器"),
    ("terminal", "终端"),
    ("music", "音乐"),
    ("video", "视频"),
    ("game", "游戏"),
    ("theme", "主题"),
    ("font", "字体"),
    ("development", "开发"),
    ("network", "网络"),
    ("python", "Python"),
    ("rust", "Rust"),
];

/// AUR 后端。
pub struct AurBackend {
    handle: raur::Handle,
    cache: Arc<Cache>,
    installed: Arc<InstalledIndex>,
    /// 串行化 search/info（单飞），避免对 AUR 造成突发压力
    gate: tokio::sync::Mutex<()>,
    /// raur::Cache 仅作为进程内的二级去重
    raur_cache: tokio::sync::Mutex<raur::Cache>,
    rpc_url: String,
    capability: Capability,
}

impl std::fmt::Debug for AurBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AurBackend")
            .field("rpc_url", &self.rpc_url)
            .finish_non_exhaustive()
    }
}

impl AurBackend {
    /// 注入自建 reqwest::Client，使代理/超时/UA 统一生效。
    ///
    /// 进程内只创建一个 Client（见 net.rs 模块文档）：raur::Handle::new_with_client 与
    /// 我们自己的请求共用同一个 TLS provider 配置。
    pub async fn new(
        http: &HttpClient,
        cache: Arc<Cache>,
        installed: Arc<InstalledIndex>,
        cfg: &crate::config::Config,
    ) -> CoreResult<Arc<Self>> {
        let rpc_url = if cfg.network.aur_rpc_url.trim().is_empty() {
            raur::AUR_RPC_URL.to_string()
        } else {
            let raw = cfg.network.aur_rpc_url.trim();
            if !raw.starts_with("https://") {
                return Err(CoreError::Config("AUR RPC 端点必须是 https:// 开头".into()));
            }
            if raw.ends_with('/') {
                raw.to_string()
            } else {
                format!("{raw}/")
            }
        };
        let client = http.client().await;
        let handle = raur::Handle::new_with_settings((*client).clone(), rpc_url.clone());
        Ok(Arc::new(Self {
            handle,
            cache,
            installed,
            gate: tokio::sync::Mutex::new(()),
            raur_cache: tokio::sync::Mutex::new(raur::Cache::new()),
            rpc_url,
            capability: Capability::available(),
        }))
    }

    /// 当前使用的 RPC 端点（设置页展示 / --doctor 用）。
    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    /// 单飞锁：串行化对 AUR 的请求。
    async fn acquire(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.gate.lock().await
    }

    /// AUR 搜索（带 5 分钟缓存与 429/503 指数退避）。
    pub async fn search_raw(
        &self,
        query: &str,
        cancel: &CancelToken,
    ) -> CoreResult<Vec<raur::Package>> {
        let key = format!("search:{}", query.trim().to_ascii_lowercase());
        let cache = Arc::clone(&self.cache);
        let entry = cache
            .get_or_fetch("aur", &key, ttl::AUR_SEARCH, || async {
                self.fetch_with_retry(cancel, || async {
                    self.handle
                        .search_by(query, SearchBy::NameDesc)
                        .await
                        .map_err(|e| self.map_error(e))
                })
                .await
            })
            .await?;
        Ok(entry.value)
    }

    /// 按关键词搜索（分类页用）。
    async fn search_keyword(
        &self,
        keyword: &str,
        cancel: &CancelToken,
    ) -> CoreResult<Vec<raur::Package>> {
        let key = format!("keyword:{}", keyword.trim().to_ascii_lowercase());
        let cache = Arc::clone(&self.cache);
        let entry = cache
            .get_or_fetch("aur", &key, ttl::AUR_SEARCH, || async {
                self.fetch_with_retry(cancel, || async {
                    self.handle
                        .search_by(keyword, SearchBy::Keywords)
                        .await
                        .map_err(|e| self.map_error(e))
                })
                .await
            })
            .await?;
        Ok(entry.value)
    }

    /// 批量 info（每批 <= 100，带 30 分钟缓存）。
    pub async fn info_raw(
        &self,
        names: &[String],
        cancel: &CancelToken,
    ) -> CoreResult<Vec<raur::Package>> {
        let mut out: Vec<raur::Package> = Vec::new();
        let mut missing: Vec<String> = Vec::new();
        for name in names {
            match self
                .cache
                .get::<raur::Package>("aur", &info_key(name), false)
                .await
            {
                Some(hit) => out.push(hit.value),
                None => missing.push(name.clone()),
            }
        }
        if missing.is_empty() {
            return Ok(out);
        }
        let fetched = self
            .fetch_with_retry(cancel, || async {
                let mut cache = self.raur_cache.lock().await;
                self.handle
                    .cache_info(&mut cache, &missing)
                    .await
                    .map(|v| v.into_iter().map(|p| (*p).clone()).collect::<Vec<_>>())
                    .map_err(|e| self.map_error(e))
            })
            .await?;
        for pkg in &fetched {
            let _ = self
                .cache
                .put("aur", &info_key(&pkg.name), pkg, ttl::AUR_INFO)
                .await;
        }
        out.extend(fetched);
        Ok(out)
    }

    /// 统一的重试循环：仅对网络错误 / 5xx / 429 重试，1s -> 2s -> 4s，最多 3 次。
    async fn fetch_with_retry<T, F, Fut>(&self, cancel: &CancelToken, mut call: F) -> CoreResult<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = CoreResult<T>>,
    {
        let policy = RetryPolicy::default();
        let mut attempt = 0u32;
        loop {
            cancel.check()?;
            let result = tokio::select! {
                _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                r = call() => r,
            };
            match result {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if !e.is_retryable() || attempt >= policy.attempts {
                        return Err(e);
                    }
                    let delay = Duration::from_secs(1 << attempt) + Duration::from_millis(200);
                    tracing::info!(attempt = attempt + 1, ?delay, error = %e, "AUR 请求失败，准备重试");
                    tokio::select! {
                        _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                        _ = tokio::time::sleep(delay) => {}
                    }
                    attempt += 1;
                }
            }
        }
    }

    /// 把 raur 错误映射成带足够上下文的 CoreError。
    fn map_error(&self, e: raur::Error) -> CoreError {
        match e {
            raur::Error::Aur(msg) => CoreError::Network {
                url: self.rpc_url.clone(),
                cause: format!("AUR 返回错误：{msg}"),
            },
            raur::Error::Reqwest(re) => {
                if re.is_timeout() {
                    return CoreError::Timeout {
                        url: self.rpc_url.clone(),
                        secs: 30,
                    };
                }
                if let Some(status) = re.status() {
                    let code = status.as_u16();
                    if code == 429 || code == 503 {
                        return CoreError::RateLimited { retry_after: 0 };
                    }
                    if (500..600).contains(&code) {
                        return CoreError::Network {
                            url: self.rpc_url.clone(),
                            cause: format!("HTTP {code}"),
                        };
                    }
                    return CoreError::Network {
                        url: self.rpc_url.clone(),
                        cause: format!("HTTP {code}"),
                    };
                }
                CoreError::Network {
                    url: self.rpc_url.clone(),
                    cause: human_reqwest_error(&re),
                }
            }
        }
    }

    /// raur::Package -> PackageSummary。
    fn to_summary(&self, pkg: &raur::Package) -> PackageSummary {
        let mut s = PackageSummary::minimal(PackageId::aur(pkg.name.clone()), pkg.name.clone());
        s.set_summary(pkg.description.as_deref().unwrap_or(""));
        s.version = Some(pkg.version.clone());
        s.installed = self.installed.installed(&pkg.name);
        // 已安装的 AUR 包复用 pacman 侧从 .desktop 解析出的图标；
        // 未安装的包没有本地元数据，交给主题同名图标 -> 字母头像。
        s.icon = self
            .installed
            .icon(&pkg.name)
            .unwrap_or_else(|| IconRef::IconName(pkg.name.clone()));
        s.popularity = Some(pkg.popularity);
        s.votes = Some(pkg.num_votes);
        s.out_of_date = pkg.out_of_date.is_some();
        s
    }

    /// 已安装的外来包中，AUR 版本更高的那些。
    pub async fn upgradable_from_snapshot(
        &self,
        cancel: &CancelToken,
    ) -> CoreResult<Vec<PackageSummary>> {
        let foreign = self.installed.snapshot().foreign();
        if foreign.is_empty() {
            return Ok(Vec::new());
        }
        let names: Vec<String> = foreign.iter().map(|(n, _)| n.clone()).collect();
        let packages = self.info_raw(&names, cancel).await?;
        let mut out = Vec::new();
        for pkg in &packages {
            let Some((_, current)) = foreign.iter().find(|(n, _)| n == &pkg.name) else {
                continue;
            };
            if alpm::vercmp(pkg.version.as_str(), current.as_str()) != std::cmp::Ordering::Greater {
                continue;
            }
            let mut s = self.to_summary(pkg);
            s.update = Some(UpdateInfo {
                current: current.clone(),
                candidate: pkg.version.clone(),
                download_size: None,
            });
            out.push(s);
        }
        out.sort_by(|a, b| a.id.name.cmp(&b.id.name));
        Ok(out)
    }

    /// 从缓存读取搜索结果（LocalOnly，绝不联网）。
    async fn search_cached(&self, query: &str) -> Vec<PackageSummary> {
        let key = format!("search:{}", query.trim().to_ascii_lowercase());
        match self
            .cache
            .get::<Vec<raur::Package>>("aur", &key, true)
            .await
        {
            Some(hit) => hit.value.iter().map(|p| self.to_summary(p)).collect(),
            None => Vec::new(),
        }
    }
}

fn info_key(name: &str) -> String {
    format!("info:{}", name.to_ascii_lowercase())
}

#[async_trait]
impl PackageBackend for AurBackend {
    fn source_kind(&self) -> &'static str {
        "aur"
    }

    fn capability(&self) -> &Capability {
        &self.capability
    }

    async fn search(
        &self,
        query: &str,
        scope: SearchScope,
    ) -> Result<Vec<PackageSummary>, CoreError> {
        let cancel = CancelToken::new();
        let packages = match scope {
            // LocalOnly：只读缓存，绝不联网（§3.3 禁止静默联网）
            SearchScope::LocalOnly => return Ok(self.search_cached(query).await),
            SearchScope::Full => {
                let _guard = self.acquire().await;
                self.search_raw(query, &cancel).await?
            }
        };
        let mut out: Vec<PackageSummary> = packages.iter().map(|p| self.to_summary(p)).collect();
        out.truncate(SEARCH_LIMIT);
        Ok(out)
    }

    async fn info(&self, id: &PackageId) -> Result<PackageDetail, CoreError> {
        let cancel = CancelToken::new();
        let _guard = self.acquire().await;
        let packages = self
            .info_raw(std::slice::from_ref(&id.name), &cancel)
            .await?;
        let pkg = packages
            .into_iter()
            .find(|p| p.name == id.name)
            .ok_or_else(|| CoreError::NotFound(format!("AUR 软件包 {}", id.name)))?;
        let mut d = PackageDetail::from_summary(self.to_summary(&pkg));
        d.description = pkg.description.clone().unwrap_or_default();
        d.licenses = pkg.license.clone();
        d.homepage = pkg.url.clone();
        d.maintainer = pkg.maintainer.clone();
        d.extra.push("包名", pkg.name.clone());
        d.extra.push("版本", pkg.version.clone());
        d.extra.push(
            "AUR 页面",
            format!("https://aur.archlinux.org/packages/{}", pkg.name),
        );
        d.extra.push("包基础名", pkg.package_base.clone());
        d.extra.push("投票数", pkg.num_votes.to_string());
        d.extra.push("流行度", format!("{:.2}", pkg.popularity));
        if let Some(m) = &pkg.maintainer {
            d.extra.push("维护者", m.clone());
        } else {
            d.extra.push("维护者", "无（孤儿包）");
        }
        if let Some(ts) = pkg.out_of_date {
            d.extra.push(
                "状态",
                format!("已被标记为过期（{}）", format_timestamp(ts)),
            );
        }
        if let Some(s) = &pkg.submitter {
            d.extra.push("提交者", s.clone());
        }
        d.extra
            .push("提交时间", format_timestamp(pkg.first_submitted));
        d.extra
            .push("最后修改", format_timestamp(pkg.last_modified));
        if !pkg.keywords.is_empty() {
            d.extra.push("关键词", pkg.keywords.join("、"));
        }
        if !pkg.provides.is_empty() {
            d.extra.push("提供", pkg.provides.join("、"));
        }
        if !pkg.conflicts.is_empty() {
            d.extra.push("冲突", pkg.conflicts.join("、"));
        }
        d.dependencies = aur_dependencies(&pkg);
        Ok(d)
    }

    async fn installed(&self) -> Result<Vec<PackageSummary>, CoreError> {
        // AUR 不提供独立的"已安装"列表：外来包由本地数据库识别（§9.1）
        Err(CoreError::Unsupported(
            "AUR 没有独立的已安装列表；外来包由本地数据库识别".into(),
        ))
    }

    async fn upgradable(&self) -> Result<Vec<PackageSummary>, CoreError> {
        let cancel = CancelToken::new();
        let _guard = self.acquire().await;
        self.upgradable_from_snapshot(&cancel).await
    }

    async fn categories(&self) -> Result<Vec<Category>, CoreError> {
        Ok(KEYWORD_CATEGORIES
            .iter()
            .map(|(kw, display)| Category::new(format!("keyword:{kw}"), *display, "aur"))
            .collect())
    }

    async fn list_category(
        &self,
        category: &str,
        page: Page,
    ) -> Result<Vec<PackageSummary>, CoreError> {
        let Some(keyword) = category.strip_prefix("keyword:") else {
            return Err(CoreError::Unsupported(format!(
                "未知的 AUR 分类：{category}"
            )));
        };
        let cancel = CancelToken::new();
        let _guard = self.acquire().await;
        let packages = self.search_keyword(keyword, &cancel).await?;
        let mut all: Vec<PackageSummary> = packages.iter().map(|p| self.to_summary(p)).collect();
        crate::backend::sort_summaries("", &mut all);
        Ok(page.slice(&all).to_vec())
    }

    async fn dependencies(&self, id: &PackageId) -> Result<Vec<DependencyInfo>, CoreError> {
        let cancel = CancelToken::new();
        let _guard = self.acquire().await;
        let packages = self
            .info_raw(std::slice::from_ref(&id.name), &cancel)
            .await?;
        let pkg = packages
            .into_iter()
            .find(|p| p.name == id.name)
            .ok_or_else(|| CoreError::NotFound(format!("AUR 软件包 {}", id.name)))?;
        Ok(aur_dependencies(&pkg))
    }

    async fn reverse_dependencies(&self, id: &PackageId) -> Result<Vec<PackageId>, CoreError> {
        let cancel = CancelToken::new();
        let _guard = self.acquire().await;
        let packages = self
            .fetch_with_retry(&cancel, || async {
                self.handle
                    .search_by(&id.name, SearchBy::Depends)
                    .await
                    .map_err(|e| self.map_error(e))
            })
            .await?;
        Ok(packages
            .into_iter()
            .map(|p| PackageId::aur(p.name))
            .collect())
    }
}

/// AUR 的依赖清单（依赖信息直接来自 RPC info，不需要 .SRCINFO）。
pub fn aur_dependencies(pkg: &raur::Package) -> Vec<DependencyInfo> {
    let mut out = Vec::new();
    for (list, kind) in [
        (&pkg.depends, DepKind::Runtime),
        (&pkg.make_depends, DepKind::Make),
        (&pkg.check_depends, DepKind::Check),
        (&pkg.opt_depends, DepKind::Optional),
    ] {
        for expr in list {
            out.push(DependencyInfo::from_expr(expr, kind));
        }
    }
    out
}

/// 面向 UI 的 AUR 来源描述。
pub fn aur_source_label(name: &str) -> String {
    format!("AUR · {name}")
}

/// 把 Unix 时间戳格式化为 YYYY-MM-DD。
fn format_timestamp(ts: i64) -> String {
    if ts <= 0 {
        return "未知".to_string();
    }
    let days = (ts as u64 / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// AUR 包名是否可安全地交给助手（复用计划的包名白名单）。
pub fn validate_aur_name(name: &str) -> CoreResult<()> {
    crate::model::plan::validate_name(name)
}

/// 判断某个 PackageSource 是否属于 AUR。
pub fn is_aur_source(source: &PackageSource) -> bool {
    matches!(source, PackageSource::Aur)
}

/// 已安装状态到展示文案。
pub fn installed_label(installed: &Installed) -> &'static str {
    match installed {
        Installed::Yes { explicit: true, .. } => "已安装",
        Installed::Yes {
            explicit: false, ..
        } => "已安装（依赖）",
        Installed::No => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_package() -> raur::Package {
        raur::Package {
            id: 1,
            name: "yay".into(),
            package_base_id: 1,
            package_base: "yay".into(),
            version: "13.0.1-1".into(),
            description: Some("Yet another yogurt. Pacman wrapper and AUR helper".into()),
            url: Some("https://github.com/Jguer/yay".into()),
            num_votes: 2500,
            popularity: 30.5,
            out_of_date: None,
            maintainer: Some("jguer".into()),
            submitter: Some("jguer".into()),
            first_submitted: 1_500_000_000,
            last_modified: 1_700_000_000,
            url_path: "/cgit/aur.git/snapshot/yay.tar.gz".into(),
            groups: vec![],
            depends: vec!["pacman>6.1".into(), "git".into()],
            make_depends: vec!["go>=1.24".into()],
            opt_depends: vec!["sudo: privilege escalation".into(), "doas".into()],
            check_depends: vec![],
            conflicts: vec![],
            replaces: vec![],
            provides: vec![],
            license: vec!["GPL-3.0-only".into()],
            keywords: vec!["aur".into(), "helper".into()],
            co_maintainers: vec![],
        }
    }

    #[test]
    fn dependency_mapping_uses_raur_field_names() {
        let deps = aur_dependencies(&sample_package());
        let runtime: Vec<&str> = deps
            .iter()
            .filter(|d| d.kind == DepKind::Runtime)
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(runtime, vec!["pacman", "git"]);
        let make: Vec<&str> = deps
            .iter()
            .filter(|d| d.kind == DepKind::Make)
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(make, vec!["go"]);
        let opt: Vec<&str> = deps
            .iter()
            .filter(|d| d.kind == DepKind::Optional)
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(opt, vec!["sudo", "doas"]);
        let sudo = deps.iter().find(|d| d.name == "sudo").expect("sudo");
        assert_eq!(sudo.description.as_deref(), Some("privilege escalation"));
    }

    #[test]
    fn timestamp_formatting_is_char_safe() {
        assert_eq!(format_timestamp(0), "未知");
        assert_eq!(format_timestamp(-5), "未知");
        // 1_500_000_000 = 2017-07-14 UTC
        assert_eq!(format_timestamp(1_500_000_000), "2017-07-14");
    }

    #[test]
    fn aur_name_validation_reuses_plan_whitelist() {
        assert!(validate_aur_name("yay").is_ok());
        assert!(validate_aur_name("python-pip").is_ok());
        assert!(validate_aur_name("-evil").is_err());
        assert!(validate_aur_name("a/b").is_err());
        assert!(validate_aur_name("a;rm -rf /").is_err());
    }

    #[test]
    fn keyword_categories_are_unique_and_prefixed() {
        let mut ids: Vec<String> = KEYWORD_CATEGORIES
            .iter()
            .map(|(k, _)| format!("keyword:{k}"))
            .collect();
        let before = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), before, "分类 id 必须唯一");
        assert!(ids.iter().all(|i| i.starts_with("keyword:")));
    }

    #[test]
    fn info_key_is_case_insensitive() {
        assert_eq!(info_key("Yay"), info_key("yay"));
    }

    #[test]
    fn installed_label_variants() {
        assert_eq!(
            installed_label(&Installed::Yes {
                version: "1".into(),
                explicit: true
            }),
            "已安装"
        );
        assert_eq!(installed_label(&Installed::No), "");
    }

    #[test]
    fn aur_package_json_roundtrip_for_cache() {
        let pkg = sample_package();
        let json = serde_json::to_string(&pkg).expect("serialize");
        // raur 使用 PascalCase 重命名，缓存必须能原样往返
        assert!(json.contains("\"NumVotes\""), "实际：{json}");
        assert!(json.contains("\"MakeDepends\""));
        let back: raur::Package = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.name, pkg.name);
        assert_eq!(back.make_depends, pkg.make_depends);
        assert_eq!(back.num_votes, pkg.num_votes);
    }
}
