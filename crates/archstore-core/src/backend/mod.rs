//! 后端抽象层（project.md §4.2）。
//!
//! 依赖方向约束：archstore-core 的依赖里不得出现 gtk4/libadwaita；CI 用 cargo tree 校验。
//! trait 里没有 build_install_cmd / build_remove_cmd：执行完全外移到 helper（§5），
//! 后端只负责只读的事实（包名、版本、依赖、大小），计划构建由 plan.rs 完成。

pub mod aur;
pub mod flatpak;
pub mod pacman;
pub mod pacman_worker;

pub use aur::AurBackend;
pub use flatpak::FlatpakBackend;
pub use pacman::PacmanBackend;
pub use pacman_worker::{AlpmOp, AlpmPayload, AlpmWorker, RepoStatus};

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::model::{IconRef, Installed, PackageSource};

use async_trait::async_trait;

use crate::error::CoreError;
use crate::model::{DependencyInfo, PackageDetail, PackageId, PackageSummary};

/// 后端能力描述：用于设置页置灰、--doctor 报告与"为什么不能用"提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub available: bool,
    /// 不可用/降级原因（面向用户的中文句子，含修复建议）
    pub reason: Option<String>,
}

impl Capability {
    pub fn available() -> Self {
        Self {
            available: true,
            reason: None,
        }
    }

    /// 可用但有注意事项（例如某个仓库未同步）。
    pub fn available_with(warning: Option<String>) -> Self {
        Self {
            available: true,
            reason: warning,
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            reason: Some(reason.into()),
        }
    }

    pub fn is_available(&self) -> bool {
        self.available
    }

    /// 面向 UI 的一句话说明。
    pub fn describe(&self) -> String {
        match (&self.available, &self.reason) {
            (true, None) => "可用".to_string(),
            (true, Some(w)) => format!("可用（{w}）"),
            (false, Some(r)) => format!("不可用：{r}"),
            (false, None) => "不可用".to_string(),
        }
    }
}

/// 搜索范围：LocalOnly 绝不联网。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    /// 只查本地数据（已安装 + 已有缓存），绝不联网
    LocalOnly,
    /// 走该后端的完整搜索（可能联网）
    Full,
}

/// 分页参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    pub offset: usize,
    pub limit: usize,
}

impl Page {
    pub fn new(offset: usize, limit: usize) -> Self {
        Self { offset, limit }
    }

    pub fn first(limit: usize) -> Self {
        Self { offset: 0, limit }
    }

    /// 按本页参数切片。
    pub fn slice<'a, T>(&self, items: &'a [T]) -> &'a [T] {
        let start = self.offset.min(items.len());
        let end = start.saturating_add(self.limit).min(items.len());
        &items[start..end]
    }
}

/// 分类（pacman 包组 / Flatpak 分类 / AUR 关键词）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Category {
    pub id: String,
    pub display: String,
    pub source_kind: &'static str,
}

impl Category {
    pub fn new(
        id: impl Into<String>,
        display: impl Into<String>,
        source_kind: &'static str,
    ) -> Self {
        Self {
            id: id.into(),
            display: display.into(),
            source_kind,
        }
    }
}

/// 只读后端契约。
#[async_trait]
pub trait PackageBackend: Send + Sync {
    /// "pacman" | "aur" | "flatpak"
    fn source_kind(&self) -> &'static str;

    fn capability(&self) -> &Capability;

    async fn search(
        &self,
        query: &str,
        scope: SearchScope,
    ) -> Result<Vec<PackageSummary>, CoreError>;

    async fn info(&self, id: &PackageId) -> Result<PackageDetail, CoreError>;

    async fn installed(&self) -> Result<Vec<PackageSummary>, CoreError>;

    async fn upgradable(&self) -> Result<Vec<PackageSummary>, CoreError>;

    async fn categories(&self) -> Result<Vec<Category>, CoreError>;

    async fn list_category(
        &self,
        category: &str,
        page: Page,
    ) -> Result<Vec<PackageSummary>, CoreError>;

    /// 依赖信息（只读、用于展示与计划构建）
    async fn dependencies(&self, id: &PackageId) -> Result<Vec<DependencyInfo>, CoreError>;

    /// 反向依赖（卸载前警告用）
    async fn reverse_dependencies(&self, id: &PackageId) -> Result<Vec<PackageId>, CoreError>;
}

/// 已安装包的内存快照。
///
/// pacman 后端是唯一的数据来源；AUR/Flatpak 后端通过它判断"是否已安装"，
/// 这样搜索结果才能显示正确的安装状态而不需要各自再查一次本地库。
#[derive(Debug, Clone, Default)]
pub struct InstalledSnapshot {
    /// name -> (来源, 版本, 是否显式安装, 图标)
    ///
    /// 图标一并快照下来，AUR 后端才能给**已安装**的 AUR 包显示真实图标
    /// （pacman 后端从 .desktop 里解析出来的那个，见 `crate::desktop_icons`）。
    map: HashMap<String, (PackageSource, String, bool, IconRef)>,
}

impl InstalledSnapshot {
    pub fn from_summaries(items: &[PackageSummary]) -> Self {
        let mut map = HashMap::with_capacity(items.len());
        for s in items {
            if let Installed::Yes { version, explicit } = &s.installed {
                map.insert(
                    s.id.name.clone(),
                    (
                        s.id.source.clone(),
                        version.clone(),
                        *explicit,
                        s.icon.clone(),
                    ),
                );
            }
        }
        Self { map }
    }

    pub fn get(&self, name: &str) -> Installed {
        match self.map.get(name) {
            Some((_, version, explicit, _)) => Installed::Yes {
                version: version.clone(),
                explicit: *explicit,
            },
            None => Installed::No,
        }
    }

    pub fn version(&self, name: &str) -> Option<&str> {
        self.map.get(name).map(|(_, v, _, _)| v.as_str())
    }

    /// 已安装包在 pacman 侧的图标（没有则该包没有可用的本地图标来源）。
    pub fn icon(&self, name: &str) -> Option<&IconRef> {
        self.map.get(name).map(|(_, _, _, icon)| icon)
    }

    /// 外来包（不在任何同步库中，通常来自 AUR）的 (名称, 版本) 列表。
    pub fn foreign(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .map
            .iter()
            .filter(|(_, (source, _, _, _))| matches!(source, PackageSource::Aur))
            .map(|(name, (_, version, _, _))| (name.clone(), version.clone()))
            .collect();
        out.sort();
        out
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// 线程安全共享的已安装快照（pacman 后端刷新后由 AppState 写入）。
#[derive(Debug, Default)]
pub struct InstalledIndex {
    inner: RwLock<InstalledSnapshot>,
}

impl InstalledIndex {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 用 pacman 后端的已安装列表整体替换快照。
    pub fn replace(&self, items: &[PackageSummary]) {
        let snapshot = InstalledSnapshot::from_summaries(items);
        match self.inner.write() {
            Ok(mut guard) => *guard = snapshot,
            Err(poisoned) => *poisoned.into_inner() = snapshot,
        }
    }

    pub fn installed(&self, name: &str) -> Installed {
        self.read().get(name)
    }

    /// 已安装包在 pacman 侧解析出的图标（供 AUR 后端复用）。
    pub fn icon(&self, name: &str) -> Option<IconRef> {
        self.read().icon(name).cloned()
    }

    pub fn snapshot(&self) -> InstalledSnapshot {
        self.read().clone()
    }

    pub fn len(&self) -> usize {
        self.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.read().is_empty()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, InstalledSnapshot> {
        match self.inner.read() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// 搜索结果的来源过滤开关（§3.3：搜索栏右侧的来源开关）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceFilter {
    pub pacman: bool,
    pub aur: bool,
    pub flatpak: bool,
}

impl Default for SourceFilter {
    fn default() -> Self {
        Self {
            pacman: true,
            aur: true,
            flatpak: true,
        }
    }
}

impl SourceFilter {
    /// 只查本地（绝不联网）。
    pub fn local_only() -> Self {
        Self {
            pacman: true,
            aur: false,
            flatpak: false,
        }
    }

    pub fn enabled(&self, kind: &str) -> bool {
        match kind {
            "pacman" => self.pacman,
            "aur" => self.aur,
            "flatpak" => self.flatpak,
            _ => false,
        }
    }
}

/// 匹配等级：0 = 名称精确，1 = 展示名精确，2 = 名称前缀，3 = 展示名前缀，4 = 其它。
///
/// 只表达"文本匹配得有多好"，不含流行度——流行度是 f64、票数是 u32，量级完全不同，
/// 把它们塞进同一个整数键会让票数压倒流行度（真实数据上抓到过排序反转）。
pub fn relevance_rank(query: &str, s: &PackageSummary) -> u8 {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return 4;
    }
    let name = s.id.name.to_ascii_lowercase();
    let display = s.display_name.to_ascii_lowercase();
    if name == q {
        0
    } else if display == q {
        1
    } else if name.starts_with(&q) {
        2
    } else if display.starts_with(&q) {
        3
    } else {
        4
    }
}

/// 完整比较器：匹配等级 > 流行度（降序）> 票数（降序）> 名称（升序）。
///
/// 名称兜底保证排序是全序（同分条目的顺序不依赖输入顺序）。
pub fn compare_relevance(
    query: &str,
    a: &PackageSummary,
    b: &PackageSummary,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    relevance_rank(query, a)
        .cmp(&relevance_rank(query, b))
        .then_with(|| {
            // NaN / 无穷视为 0，避免 partial_cmp 返回 None 时丢掉排序
            let pa = a.popularity.filter(|p| p.is_finite()).unwrap_or(0.0);
            let pb = b.popularity.filter(|p| p.is_finite()).unwrap_or(0.0);
            pb.partial_cmp(&pa).unwrap_or(Ordering::Equal)
        })
        .then_with(|| b.votes.unwrap_or(0).cmp(&a.votes.unwrap_or(0)))
        .then_with(|| a.id.name.cmp(&b.id.name))
}

/// 合并多个后端的结果：按 PackageId 去重，再按相关性排序。
///
/// 去重必须用哈希集合：曾经用 Vec 做线性扫描，5000 条输入实测要 239 ms
/// （是排序耗时的 26 倍），在 UI 线程上会直接掉帧。哈希版本是 O(n)。
pub fn merge_results(query: &str, batches: Vec<Vec<PackageSummary>>) -> Vec<PackageSummary> {
    let mut seen: std::collections::HashSet<PackageId> =
        std::collections::HashSet::with_capacity(batches.iter().map(|b| b.len()).sum());
    let mut out: Vec<PackageSummary> = Vec::with_capacity(seen.capacity());
    for batch in batches {
        for item in batch {
            if !seen.insert(item.id.clone()) {
                continue;
            }
            out.push(item);
        }
    }
    sort_summaries(query, &mut out);
    out
}

/// 按相关性就地排序。
pub fn sort_summaries(query: &str, items: &mut [PackageSummary]) {
    items.sort_by(|a, b| compare_relevance(query, a, b));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Installed;

    fn summary(name: &str, pop: Option<f64>) -> PackageSummary {
        let mut s = PackageSummary::minimal(PackageId::aur(name), name);
        s.popularity = pop;
        s.votes = Some(0);
        s
    }

    #[test]
    fn merge_dedupes_and_ranks_exact_match_first() {
        let mut exact = summary("firefox", Some(1.0));
        exact.installed = Installed::Yes {
            version: "1".into(),
            explicit: true,
        };
        let batches = vec![
            vec![summary("firefox-nightly", Some(9.0)), exact],
            vec![summary("firefox", Some(0.1))],
        ];
        let merged = merge_results("firefox", batches);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].id.name, "firefox");
        assert_eq!(merged[1].id.name, "firefox-nightly");
    }

    #[test]
    fn popularity_breaks_ties() {
        let mut items = vec![
            summary("a-firefox", Some(0.5)),
            summary("b-firefox", Some(8.0)),
        ];
        sort_summaries("firefox", &mut items);
        assert_eq!(items[0].id.name, "b-firefox");
    }

    #[test]
    fn votes_must_not_outrank_popularity() {
        // 真实数据上抓到的排序反转：流行度 1.053 但票数 800 的包
        // 曾排在流行度 1.086 但票数 10 的包前面。
        let mut many_votes = summary("aaa-many-votes", Some(1.053));
        many_votes.votes = Some(800);
        let mut few_votes = summary("zzz-few-votes", Some(1.086));
        few_votes.votes = Some(10);

        let mut items = vec![many_votes, few_votes];
        sort_summaries("", &mut items);
        assert_eq!(items[0].id.name, "zzz-few-votes", "流行度必须优先于票数");
        assert!(
            items[0].popularity.unwrap() > items[1].popularity.unwrap(),
            "结果必须按流行度降序"
        );
    }

    #[test]
    fn sorting_is_total_and_survives_nan() {
        let mut a = summary("beta", Some(1.0));
        a.votes = Some(5);
        let mut b = summary("alpha", Some(1.0));
        b.votes = Some(5);
        let mut items = vec![a, b];
        sort_summaries("", &mut items);
        assert_eq!(items[0].id.name, "alpha", "同分时按名称升序，保证全序");
        assert_eq!(items[1].id.name, "beta");

        let mut nan = summary("nan-pkg", Some(f64::NAN));
        nan.votes = Some(1);
        let mut items = vec![nan, summary("normal", Some(0.1))];
        sort_summaries("", &mut items);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn exact_match_wins_over_popularity() {
        let mut popular = summary("firefox-nightly", Some(50.0));
        popular.votes = Some(3000);
        let exact = summary("firefox", Some(0.1));
        let mut items = vec![popular, exact];
        sort_summaries("firefox", &mut items);
        assert_eq!(items[0].id.name, "firefox");
    }

    #[test]
    fn page_slice_clamps() {
        let v: Vec<u32> = (0..10).collect();
        assert_eq!(Page::new(0, 3).slice(&v), &[0, 1, 2]);
        assert_eq!(Page::new(8, 5).slice(&v), &[8, 9]);
        assert_eq!(Page::new(99, 5).slice(&v), &[] as &[u32]);
    }

    #[test]
    fn capability_describe_variants() {
        assert_eq!(Capability::available().describe(), "可用");
        assert_eq!(
            Capability::available_with(Some("extra 未同步".into())).describe(),
            "可用（extra 未同步）"
        );
        assert!(
            Capability::unavailable("未安装 flatpak")
                .describe()
                .contains("未安装")
        );
    }

    #[test]
    fn installed_snapshot_tracks_foreign_packages() {
        let mut a = PackageSummary::minimal(
            crate::model::PackageId::official("extra", "firefox"),
            "firefox",
        );
        a.installed = Installed::Yes {
            version: "155.0.1-1".into(),
            explicit: true,
        };
        let mut b = PackageSummary::minimal(crate::model::PackageId::aur("yay"), "yay");
        b.installed = Installed::Yes {
            version: "13.0.1-1".into(),
            explicit: true,
        };
        let mut c = PackageSummary::minimal(crate::model::PackageId::aur("not-installed"), "x");
        c.installed = Installed::No;

        let snap = InstalledSnapshot::from_summaries(&[a, b, c]);
        assert_eq!(snap.len(), 2);
        assert_eq!(snap.version("firefox"), Some("155.0.1-1"));
        assert!(snap.get("yay").is_yes());
        assert!(!snap.get("not-installed").is_yes());
        assert_eq!(
            snap.foreign(),
            vec![("yay".to_string(), "13.0.1-1".to_string())]
        );

        let index = InstalledIndex::new();
        assert!(index.is_empty());
        index.replace(&[PackageSummary::minimal(
            crate::model::PackageId::aur("yay"),
            "yay",
        )]);
        assert!(index.is_empty(), "未安装的包不进入快照");
    }

    #[test]
    fn source_filter_local_only_is_offline() {
        let f = SourceFilter::local_only();
        assert!(f.enabled("pacman"));
        assert!(!f.enabled("aur"));
        assert!(!f.enabled("flatpak"));
        assert!(!f.enabled("nonsense"));
    }
}
