//! Flathub v2 API 客户端：元数据、图标、大小、权限、评分与安全公告（project.md §4.6 / §9.2）。
//!
//! 实测端点（2026-09-10）：
//! - GET https://flathub.org/api/v2/appstream/{app_id} -> 名称/分类/许可/图标URL/截图/关键词
//! - GET https://flathub.org/api/v2/summary/{app_id}   -> 下载/安装大小、runtime、权限、extensions
//! - GET https://odrs.gnome.org/1.0/reviews/api/ratings/{app_id} -> {star0..star5,total}
//!   **不稳定**：curl 可用、reqwest 直接请求曾经连接重置。必须 3 s 超时 + 静默降级为"无评分"，
//!   且不得阻塞详情页渲染。
//! - GET https://security.archlinux.org/issues/all.json -> 308 重定向后 200（须跟随重定向），
//!   响应体约 898 KB 且耗时 5 s 级：仅在更新页可见时拉取一次，缓存 6 小时。

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::cache::{Cache, ttl};
use crate::error::{CoreError, CoreResult};
use crate::net::{CancelToken, HttpClient, RetryPolicy};

/// Flathub 应用元数据（只保留本项目使用的字段）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Appstream {
    pub id: String,
    pub name: String,
    pub summary: String,
    /// 原始 HTML 描述（来自上游，展示前必须转成纯文本）
    pub description: String,
    pub project_license: String,
    pub developer_name: String,
    /// 图标 URL（dl.flathub.org/media/icons/128x128/….png）
    pub icon: Option<String>,
    pub categories: Vec<String>,
    pub keywords: Vec<String>,
    pub screenshots: Vec<Screenshot>,
    #[serde(rename = "is_eol")]
    pub is_eol: bool,
    pub releases: Vec<Release>,
}

/// 截图组（每个组有多个分辨率）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Screenshot {
    pub caption: Option<String>,
    pub sizes: Vec<ScreenshotSize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenshotSize {
    pub src: String,
    pub width: String,
    pub height: String,
    pub scale: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Release {
    pub version: String,
    pub timestamp: String,
}

impl Appstream {
    /// 挑出每张截图最合适的一档分辨率（优先 1248 宽，其次最大的非缩略图）。
    pub fn screenshot_urls(&self) -> Vec<String> {
        const PREFERRED: [&str; 4] = ["1248", "1504", "752", "624"];
        let mut out = Vec::new();
        for shot in &self.screenshots {
            let pick = PREFERRED
                .iter()
                .find_map(|want| {
                    shot.sizes
                        .iter()
                        .find(|s| s.width == *want && s.src.starts_with("https://"))
                })
                .or_else(|| {
                    shot.sizes
                        .iter()
                        .filter(|s| s.src.starts_with("https://"))
                        .max_by_key(|s| s.width.parse::<u32>().unwrap_or(0))
                });
            if let Some(p) = pick {
                out.push(p.src.clone());
            }
        }
        out
    }

    /// 长描述转换为纯文本（上游是 HTML）。
    pub fn description_text(&self) -> String {
        html_to_text(&self.description)
    }

    /// 最近一次发布版本（若 API 提供）。
    pub fn latest_release(&self) -> Option<&Release> {
        self.releases.first()
    }

    /// 图标 URL（仅允许 https）。
    pub fn icon_url(&self) -> Option<&str> {
        self.icon.as_deref().filter(|u| u.starts_with("https://"))
    }
}

/// Flathub summary 端点返回的元数据。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Summary {
    pub branch: String,
    pub timestamp: i64,
    pub download_size: u64,
    pub installed_size: u64,
    pub metadata: Metadata,
    pub arches: Vec<String>,
}

/// summary.metadata。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Metadata {
    pub runtime: Option<String>,
    #[serde(rename = "runtimeName")]
    pub runtime_name: Option<String>,
    #[serde(rename = "runtimeIsEol")]
    pub runtime_is_eol: bool,
    pub sdk: Option<String>,
    pub permissions: Permissions,
    pub extensions: std::collections::HashMap<String, serde_json::Value>,
    #[serde(rename = "runtimeInstalledSize")]
    pub runtime_installed_size: Option<u64>,
}

/// summary.metadata.permissions：用于详情页的权限展示。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Permissions {
    pub shared: Vec<String>,
    pub sockets: Vec<String>,
    pub devices: Vec<String>,
    pub filesystems: Vec<String>,
    pub persistent: Vec<String>,
    pub features: Vec<String>,
    #[serde(rename = "session-bus")]
    pub session_bus: BusPermissions,
    #[serde(rename = "system-bus")]
    pub system_bus: BusPermissions,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BusPermissions {
    pub own: Vec<String>,
    pub talk: Vec<String>,
    pub see: Vec<String>,
}

impl Permissions {
    /// 展开成面向用户的中文权限条目。
    pub fn describe(&self) -> Vec<String> {
        let mut out = Vec::new();
        for s in &self.shared {
            out.push(format!("共享：{s}"));
        }
        for s in &self.sockets {
            out.push(format!("套接字：{s}"));
        }
        for s in &self.devices {
            out.push(format!("设备：{s}"));
        }
        for s in &self.filesystems {
            out.push(format!("文件系统：{s}"));
        }
        for s in &self.persistent {
            out.push(format!("持久化目录：{s}"));
        }
        for s in &self.features {
            out.push(format!("特性：{s}"));
        }
        for s in &self.session_bus.own {
            out.push(format!("会话总线（提供）：{s}"));
        }
        for s in &self.session_bus.talk {
            out.push(format!("会话总线（访问）：{s}"));
        }
        for s in &self.system_bus.talk {
            out.push(format!("系统总线（访问）：{s}"));
        }
        out
    }
}

/// ODRS 评分聚合。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rating {
    /// 0.0..=5.0
    pub stars: f32,
    pub total: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct RawRating {
    /// star0 对均值没有贡献，但必须接受该字段（避免 deny_unknown_fields 之外的分歧）
    #[serde(rename = "star0")]
    _star0: u32,
    star1: u32,
    star2: u32,
    star3: u32,
    star4: u32,
    star5: u32,
    total: u32,
}

impl RawRating {
    fn to_rating(&self) -> Option<Rating> {
        if self.total == 0 {
            return None;
        }
        let sum = u64::from(self.star1)
            + 2 * u64::from(self.star2)
            + 3 * u64::from(self.star3)
            + 4 * u64::from(self.star4)
            + 5 * u64::from(self.star5);
        Some(Rating {
            stars: (sum as f64 / f64::from(self.total)) as f32,
            total: self.total,
        })
    }
}

/// 把 JSON 的 null 当作"字段缺失"处理。
///
/// 实测 security.archlinux.org/issues/all.json：2444 条公告中
/// 2239 条的 ticket 为 null，202 条的 fixed 为 null。
/// 若按普通 String 反序列化会整批失败（一个 null 毁掉整个更新页）。
fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Arch 安全公告条目（§9.2）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Advisory {
    #[serde(deserialize_with = "null_as_default")]
    pub name: String,
    #[serde(deserialize_with = "null_as_default")]
    pub packages: Vec<String>,
    #[serde(deserialize_with = "null_as_default")]
    pub status: String,
    #[serde(deserialize_with = "null_as_default")]
    pub severity: String,
    /// 受影响版本区间下界（实测可能为 null）
    #[serde(deserialize_with = "null_as_default")]
    pub affected: String,
    /// 修复版本（实测 202/2444 为 null = 尚未修复）
    #[serde(deserialize_with = "null_as_default")]
    pub fixed: String,
    #[serde(deserialize_with = "null_as_default")]
    pub issues: Vec<String>,
}

impl Advisory {
    /// 判定某个已安装版本是否落在 affected..fixed 区间内。
    ///
    /// 匹配规则：packages 包含包名 **且** 当前版本落在 affected..fixed 区间。
    pub fn matches(&self, package: &str, current_version: &str) -> bool {
        if !self.packages.iter().any(|p| p == package) {
            return false;
        }
        let affected_ok = self.affected.is_empty()
            || alpm::vercmp(current_version, self.affected.as_str()) != std::cmp::Ordering::Less;
        let fixed_ok = self.fixed.is_empty()
            || alpm::vercmp(current_version, self.fixed.as_str()) == std::cmp::Ordering::Less;
        affected_ok && fixed_ok
    }
}

/// Flathub / ODRS / Arch 安全公告客户端。
pub struct FlathubClient {
    http: Arc<HttpClient>,
    cache: Arc<Cache>,
}

impl std::fmt::Debug for FlathubClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlathubClient").finish_non_exhaustive()
    }
}

impl FlathubClient {
    pub fn new(http: Arc<HttpClient>, cache: Arc<Cache>) -> Self {
        Self { http, cache }
    }

    /// 应用元数据（缓存 6 小时）。
    pub async fn appstream(&self, app_id: &str, cancel: &CancelToken) -> CoreResult<Appstream> {
        let url = format!("https://flathub.org/api/v2/appstream/{app_id}");
        let client = self.http.client().await;
        let entry = self
            .cache
            .get_or_fetch(
                "flathub",
                &format!("appstream:{app_id}"),
                ttl::FLATHUB,
                || async {
                    crate::net::get_json::<Appstream>(&client, &url, cancel, RetryPolicy::default())
                        .await
                },
            )
            .await?;
        Ok(entry.value)
    }

    /// 下载/安装大小、runtime、权限（缓存 6 小时）。
    pub async fn summary(&self, app_id: &str, cancel: &CancelToken) -> CoreResult<Summary> {
        let url = format!("https://flathub.org/api/v2/summary/{app_id}");
        let client = self.http.client().await;
        let entry = self
            .cache
            .get_or_fetch(
                "flathub",
                &format!("summary:{app_id}"),
                ttl::FLATHUB,
                || async {
                    crate::net::get_json::<Summary>(&client, &url, cancel, RetryPolicy::default())
                        .await
                },
            )
            .await?;
        Ok(entry.value)
    }

    /// ODRS 星级。3 秒超时，失败静默返回 None（绝不阻塞详情页）。
    pub async fn ratings(&self, app_id: &str, cancel: &CancelToken) -> Option<Rating> {
        let url = format!("https://odrs.gnome.org/1.0/reviews/api/ratings/{app_id}");
        let client = self.http.client().await;
        let fetch = async {
            let fut = crate::net::get_json::<RawRating>(&client, &url, cancel, RetryPolicy::NONE);
            tokio::time::timeout(std::time::Duration::from_secs(3), fut)
                .await
                .ok()
                .and_then(|r| r.ok())
                .and_then(|r| r.to_rating())
        };
        match tokio::time::timeout(std::time::Duration::from_secs(3), fetch).await {
            Ok(v) => v,
            Err(_) => {
                tracing::debug!(app_id, "ODRS 评分请求超时，静默降级为无评分");
                None
            }
        }
    }

    /// Arch 安全公告（898 KB / 5 s 级延迟，必须缓存 6 小时）。
    pub async fn advisories(&self, cancel: &CancelToken) -> CoreResult<Vec<Advisory>> {
        let url = "https://security.archlinux.org/issues/all.json";
        let client = self.http.client().await;
        let entry = self
            .cache
            .get_or_fetch(
                "security",
                "advisories",
                ttl::SECURITY_ADVISORIES,
                || async {
                    crate::net::get_json::<Vec<Advisory>>(
                        &client,
                        url,
                        cancel,
                        RetryPolicy::default(),
                    )
                    .await
                },
            )
            .await?;
        Ok(entry.value)
    }

    /// 下载一个图标到缓存目录，返回本地文件路径。
    pub async fn download_icon(
        &self,
        url: &str,
        cancel: &CancelToken,
    ) -> CoreResult<std::path::PathBuf> {
        ensure_https(url)?;
        let _permit = self
            .http
            .icon_gate()
            .acquire()
            .await
            .map_err(|_| CoreError::Internal("图标下载闸门已关闭".into()))?;
        let bytes = self
            .http
            .get_bytes(
                url,
                cancel,
                RetryPolicy {
                    attempts: 2,
                    base: std::time::Duration::from_secs(1),
                },
                crate::net::MAX_ICON_BYTES,
            )
            .await?;
        let ext = url
            .rsplit('.')
            .next()
            .filter(|e| e.len() <= 4)
            .unwrap_or("png");
        self.cache.store_icon(&bytes, ext).await
    }

    /// 带 URL -> 本地路径映射的下载（图标与截图共用）。
    ///
    /// 图标文件本身按内容哈希命名（天然去重），但**无法由 URL 反查**，
    /// 因此这里额外缓存一层 "url -> 路径" 的映射，避免每次进入详情页都重新下载。
    /// 映射随图标 TTL（30 天）过期；文件不存在时自动失效并重新下载。
    pub async fn download_cached(
        &self,
        url: &str,
        cancel: &CancelToken,
    ) -> CoreResult<std::path::PathBuf> {
        ensure_https(url)?;
        let key = format!("asset:{}", crate::cache::key::encode(url));
        if let Some(hit) = self
            .cache
            .get::<String>("flathub", &key, true)
            .await
            .map(|e| e.value)
        {
            let path = std::path::PathBuf::from(&hit);
            if path.is_file() {
                return Ok(path);
            }
            tracing::debug!(url, "缓存映射指向的文件已不存在，重新下载");
        }
        let path = self.download_icon(url, cancel).await?;
        let _ = self
            .cache
            .put(
                "flathub",
                &key,
                &path.to_string_lossy().to_string(),
                ttl::ICON,
            )
            .await;
        Ok(path)
    }

    /// 批量下载一组资源（截图），返回与输入等长的结果（失败为 None）。
    ///
    /// 串行执行：图标闸门本身限制并发为 4，详情页的截图数量通常 <= 8，
    /// 串行可以避免与列表图标下载争抢带宽。
    pub async fn download_all(
        &self,
        urls: &[String],
        cancel: &CancelToken,
    ) -> Vec<Option<std::path::PathBuf>> {
        let mut out = Vec::with_capacity(urls.len());
        for url in urls {
            if cancel.is_cancelled() {
                out.push(None);
                continue;
            }
            match self.download_cached(url, cancel).await {
                Ok(path) => out.push(Some(path)),
                Err(e) => {
                    // 截图失败是纯展示问题，静默降级（§4.6 的失败原则）
                    tracing::debug!(url, error = %e, "资源下载失败，跳过");
                    out.push(None);
                }
            }
        }
        out
    }
}

/// Flathub 集合（分类/热门/趋势）的单页结果。
///
/// 实测端点：GET https://flathub.org/api/v2/collection/category/{Category}
///            GET https://flathub.org/api/v2/collection/trending
///            GET https://flathub.org/api/v2/collection/popular
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CollectionPage {
    pub hits: Vec<CollectionHit>,
    pub page: u32,
    pub total_pages: u32,
    pub hits_per_page: u32,
    pub total_hits: u32,
}

impl CollectionPage {
    /// 本页是否还有后续页。
    pub fn has_more(&self) -> bool {
        self.page < self.total_pages
    }
}

/// 集合中的一条应用。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CollectionHit {
    /// 形如 org.vinegarhq.Sober
    pub app_id: String,
    /// 形如 org_vinegarhq_Sober（仅用于内部索引）
    pub id: String,
    pub name: String,
    pub summary: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub project_license: Option<String>,
    pub developer_name: Option<String>,
    /// 主分类（小写，如 "game"）
    pub main_categories: Option<String>,
    pub sub_categories: Vec<String>,
    pub runtime: Option<String>,
    pub installs_last_month: Option<u64>,
    pub updated_at: Option<i64>,
    #[serde(rename = "is_free_license")]
    pub is_free_license: bool,
}

impl CollectionHit {
    /// 图标 URL（仅 https）。
    pub fn icon_url(&self) -> Option<&str> {
        self.icon.as_deref().filter(|u| u.starts_with("https://"))
    }
}

/// Flathub 的标准主分类（freedesktop 菜单分类，与 API 的 main_categories 对应）。
pub const FLATHUB_CATEGORIES: [(&str, &str); 12] = [
    ("AudioVideo", "影音"),
    ("Development", "开发"),
    ("Education", "教育"),
    ("Game", "游戏"),
    ("Graphics", "图形"),
    ("Network", "网络"),
    ("Office", "办公"),
    ("Science", "科学"),
    ("Settings", "设置"),
    ("System", "系统"),
    ("Utility", "实用工具"),
    ("Emulator", "模拟器"),
];

impl FlathubClient {
    /// 某个分类下的一页应用（缓存 6 小时）。
    ///
    /// 分页参数必须成对出现：实测只传 page 或只传 per_page 都会返回 HTTP 400。
    pub async fn category_page(
        &self,
        category: &str,
        page: u32,
        per_page: u32,
        cancel: &CancelToken,
    ) -> CoreResult<CollectionPage> {
        // 分类名只允许 ASCII 字母数字，避免拼出奇怪的 URL
        if category.is_empty() || !category.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(CoreError::Unsupported(format!("非法分类名：{category}")));
        }
        let (page, per_page) = normalize_paging(page, per_page);
        let url = format!(
            "https://flathub.org/api/v2/collection/category/{category}?page={page}&per_page={per_page}"
        );
        let key = format!(
            "collection:category:{}:{page}:{per_page}",
            category.to_ascii_lowercase()
        );
        let client = self.http.client().await;
        let entry = self
            .cache
            .get_or_fetch("flathub", &key, ttl::FLATHUB, || async {
                crate::net::get_json::<CollectionPage>(
                    &client,
                    &url,
                    cancel,
                    RetryPolicy::default(),
                )
                .await
            })
            .await?;
        Ok(entry.value)
    }

    /// 趋势 / 热门集合（首页推荐位用）。
    pub async fn collection(
        &self,
        kind: CollectionKind,
        page: u32,
        per_page: u32,
        cancel: &CancelToken,
    ) -> CoreResult<CollectionPage> {
        let (page, per_page) = normalize_paging(page, per_page);
        let url = format!(
            "https://flathub.org/api/v2/collection/{}?page={page}&per_page={per_page}",
            kind.slug()
        );
        let key = format!("collection:{}:{page}:{per_page}", kind.slug());
        let client = self.http.client().await;
        let entry = self
            .cache
            .get_or_fetch("flathub", &key, ttl::FLATHUB, || async {
                crate::net::get_json::<CollectionPage>(
                    &client,
                    &url,
                    cancel,
                    RetryPolicy::default(),
                )
                .await
            })
            .await?;
        Ok(entry.value)
    }
}

/// 分页参数规范化：page >= 1，per_page 落在 10..=250（API 的上限实测为 250）。
pub fn normalize_paging(page: u32, per_page: u32) -> (u32, u32) {
    (page.max(1), per_page.clamp(10, 250))
}

/// Flathub 的集合类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionKind {
    Trending,
    Popular,
    RecentlyUpdated,
    RecentlyAdded,
}

impl CollectionKind {
    pub fn slug(&self) -> &'static str {
        match self {
            CollectionKind::Trending => "trending",
            CollectionKind::Popular => "popular",
            CollectionKind::RecentlyUpdated => "recently-updated",
            CollectionKind::RecentlyAdded => "recently-added",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            CollectionKind::Trending => "趋势",
            CollectionKind::Popular => "热门",
            CollectionKind::RecentlyUpdated => "最近更新",
            CollectionKind::RecentlyAdded => "最近上架",
        }
    }
}

/// 由应用 ID 构造 Flathub 的 128x128 图标 URL。
///
/// **实测（2026-09-10）**：该 URL 是**可预测**的，无需先调 API：
/// https://dl.flathub.org/media/icons/128x128/<app_id>.png
/// 对 org.mozilla.firefox / org.videolan.VLC / com.spotify.Client /
/// org.gnome.Calculator / org.inkscape.Inkscape 全部返回 200 且是真实 PNG。
///
/// 这样搜索结果与分类列表里的 Flatpak 应用不必先请求 appstream 就能拿到图标。
/// app_id 必须是合法的反向域名（只允许字母数字与点，且至少两段），
/// 避免把任意字符串拼进 URL。
pub fn icon_url_for(app_id: &str) -> Option<String> {
    let ok = app_id.len() >= 3
        && app_id.len() <= 255
        && app_id.contains('.')
        && !app_id.starts_with('.')
        && !app_id.ends_with('.')
        && !app_id.contains("..")
        && app_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-');
    ok.then(|| format!("https://dl.flathub.org/media/icons/128x128/{app_id}.png"))
}

/// 把 Flathub 描述里的 HTML 转换成纯文本。
///
/// 出于安全考虑不保留任何标签，也不保留任何可用于注入的属性。
/// 返回值是纯文本，调用方必须以 use_markup(false) 渲染（见 to_pango_escaped）。
///
/// 注意：`&lt;` 这类实体会被解码成字面量 `<`。它是文本而不是标签，
/// 只要以纯文本模式渲染就完全安全（§11.1 规则 4）。
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut chars = html.chars().peekable();
    let mut last_was_space = false;
    let mut pending_break = false;

    while let Some(c) = chars.next() {
        match c {
            '<' => {
                // 收集标签名，用于判断块级元素
                let mut tag = String::new();
                for t in chars.by_ref() {
                    if t == '>' {
                        break;
                    }
                    if tag.len() < 16 {
                        tag.push(t);
                    }
                }
                let t = tag.trim_start_matches('/').trim().to_ascii_lowercase();
                if matches!(
                    t.split_whitespace().next().unwrap_or(""),
                    "p" | "br" | "li" | "ul" | "ol" | "div" | "h1" | "h2" | "h3" | "h4" | "tr"
                ) {
                    pending_break = true;
                }
            }
            '&' => {
                let mut entity = String::new();
                for e in chars.by_ref() {
                    if e == ';' || entity.len() > 10 {
                        break;
                    }
                    entity.push(e);
                }
                let decoded = match entity.as_str() {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some(' '),
                    "hellip" => Some('…'),
                    "mdash" => Some('—'),
                    "ndash" => Some('–'),
                    _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                        u32::from_str_radix(&entity[2..], 16)
                            .ok()
                            .and_then(char::from_u32)
                    }
                    _ if entity.starts_with('#') => {
                        entity[1..].parse::<u32>().ok().and_then(char::from_u32)
                    }
                    _ => Some(' '),
                };
                if let Some(d) = decoded {
                    push_char(&mut out, d, &mut last_was_space, &mut pending_break);
                }
            }
            _ => push_char(&mut out, c, &mut last_was_space, &mut pending_break),
        }
    }
    out.trim().to_string()
}

/// 把纯文本转义成合法的 Pango markup 内容（需要加粗/链接时使用）。
///
/// GTK 侧仍应把结果交给 gtk::pango::parse_markup 校验后再使用。
pub fn to_pango_escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

fn push_char(out: &mut String, c: char, last_was_space: &mut bool, pending_break: &mut bool) {
    if *pending_break {
        *pending_break = false;
        if !out.is_empty() {
            out.push('\n');
        }
        *last_was_space = false;
    }
    if c.is_whitespace() {
        if !*last_was_space && !out.is_empty() {
            out.push(' ');
            *last_was_space = true;
        }
        return;
    }
    out.push(c);
    *last_was_space = false;
}

/// 解析 flatpak 输出的大小字符串（如 "125.6 MB" / "16.4 MB" / "1.2 GB"）为字节数。
pub fn parse_size(text: &str) -> Option<u64> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let value: f64 = num.trim().parse().ok()?;
    let unit = unit.trim().to_ascii_lowercase();
    let mult = match unit.as_str() {
        "" | "b" | "byte" | "bytes" => 1.0,
        "kb" | "kib" => 1024.0,
        "mb" | "mib" => 1024.0 * 1024.0,
        "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((value * mult) as u64)
}

/// 校验一个 URL 必须是 https（详情页外链、图标、截图共用）。
pub fn ensure_https(url: &str) -> CoreResult<()> {
    if url.starts_with("https://") {
        Ok(())
    } else {
        Err(CoreError::Unsupported(format!(
            "只允许 https 链接，已拒绝：{url}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_strips_tags_and_decodes_entities() {
        let html = "<p>Hello &amp; welcome</p><p>Second <b>line</b></p>";
        let text = html_to_text(html);
        assert_eq!(text, "Hello & welcome\nSecond line");
    }

    #[test]
    fn html_to_text_handles_lists_and_entities() {
        let html = "<ul><li>one</li><li>two&nbsp;&mdash;three</li></ul>";
        let text = html_to_text(html);
        assert!(text.contains("one"));
        assert!(text.contains("two"));
        assert!(text.contains('—'));
    }

    #[test]
    fn html_to_text_removes_all_tags() {
        let html = "<script>alert(1)</script><p>safe &lt;b&gt;text&lt;/b&gt;</p>";
        let text = html_to_text(html);
        // 标签全部消失（script 的内容作为文本保留，但不含任何标签本身）
        assert!(!text.contains("script"));
        assert!(!text.contains("<p>"));
        assert!(text.contains("safe"));
        // 解码后的实体是字面量文本；渲染时必须用纯文本模式
        assert!(text.contains("<b>text</b>"));
    }

    #[test]
    fn pango_escape_makes_text_safe_for_markup_rendering() {
        let escaped = to_pango_escaped("a & b <i>c</i>");
        assert_eq!(escaped, "a &amp; b &lt;i&gt;c&lt;/i&gt;");
        assert!(!escaped.contains("<i>"));
    }

    #[test]
    fn html_to_text_numeric_entities() {
        assert_eq!(html_to_text("caf&#233;"), "café");
        assert_eq!(html_to_text("A&#x42;C"), "ABC");
    }

    #[test]
    fn parse_size_handles_flatpak_units() {
        assert_eq!(parse_size("16.4 MB"), Some((16.4 * 1024.0 * 1024.0) as u64));
        assert_eq!(
            parse_size("125.6 MB"),
            Some((125.6 * 1024.0 * 1024.0) as u64)
        );
        assert_eq!(parse_size("1.2 GB"), Some((1.2 * 1024.0f64.powi(3)) as u64));
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("bogus"), None);
    }

    #[test]
    fn advisory_range_matching() {
        let a = Advisory {
            name: "AVG-2843".into(),
            packages: vec!["vim".into()],
            affected: "9.0.1224-1".into(),
            fixed: "9.0.1225-1".into(),
            ..Default::default()
        };
        assert!(a.matches("vim", "9.0.1224-1"), "区间下界包含");
        assert!(a.matches("vim", "9.0.1224-5"), "下界之上、修复之前");
        assert!(!a.matches("vim", "9.0.1225-1"), "已修复版本不再匹配");
        assert!(!a.matches("vim", "9.0.1300-1"));
        assert!(!a.matches("nano", "9.0.1224-1"), "包名不匹配");
        // 版本比较必须是 vercmp 语义：1.10 > 1.9
        let b = Advisory {
            packages: vec!["x".into()],
            affected: "1.9".into(),
            fixed: "1.10".into(),
            ..Default::default()
        };
        assert!(b.matches("x", "1.9"));
        assert!(!b.matches("x", "1.10"));
        // 空区间上界表示"尚未修复"
        let c = Advisory {
            packages: vec!["y".into()],
            affected: "1.0".into(),
            fixed: String::new(),
            ..Default::default()
        };
        assert!(c.matches("y", "99.0"));
    }

    #[test]
    fn raw_rating_computes_weighted_average() {
        let r = RawRating {
            star1: 1,
            star5: 3,
            total: 4,
            ..Default::default()
        };
        let rating = r.to_rating().expect("rating");
        assert_eq!(rating.total, 4);
        assert!((rating.stars - 4.0).abs() < 0.01);
        assert!(RawRating::default().to_rating().is_none());
    }

    #[test]
    fn permissions_describe_is_non_empty() {
        let p = Permissions {
            shared: vec!["network".into()],
            sockets: vec!["wayland".into()],
            filesystems: vec!["xdg-download".into()],
            ..Default::default()
        };
        let d = p.describe();
        assert_eq!(d.len(), 3);
        assert!(d.iter().any(|s| s.contains("network")));
    }

    #[test]
    fn screenshots_prefer_large_resolution() {
        let a = Appstream {
            screenshots: vec![Screenshot {
                caption: None,
                sizes: vec![
                    ScreenshotSize {
                        src: "https://x/112x63.png".into(),
                        width: "112".into(),
                        height: "63".into(),
                        scale: "1x".into(),
                    },
                    ScreenshotSize {
                        src: "https://x/1248x702.png".into(),
                        width: "1248".into(),
                        height: "702".into(),
                        scale: "1x".into(),
                    },
                ],
            }],
            ..Default::default()
        };
        assert_eq!(a.screenshot_urls(), vec!["https://x/1248x702.png"]);
    }

    #[test]
    fn screenshots_reject_non_https() {
        let a = Appstream {
            screenshots: vec![Screenshot {
                sizes: vec![ScreenshotSize {
                    src: "http://insecure/x.png".into(),
                    width: "1248".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(a.screenshot_urls().is_empty());
    }

    #[test]
    fn appstream_deserializes_real_fixture() {
        // 真实抓取的字段子集（2026-09-10）
        let json = r#"{
            "id": "org.mozilla.firefox",
            "name": "Firefox",
            "summary": "Fast, Private & Safe Web Browser",
            "description": "<p>When it comes to your life online.</p>",
            "project_license": "MPL-2.0",
            "developer_name": "Mozilla",
            "icon": "https://dl.flathub.org/media/icons/128x128/org.mozilla.firefox.png",
            "categories": ["Network", "WebBrowser"],
            "keywords": ["Browser"],
            "is_eol": false,
            "screenshots": [{"caption": null, "default": true, "sizes": [
                {"src": "https://dl.flathub.org/a/624x351/x.png", "width": "624", "height": "351", "scale": "1x"}
            ]}],
            "releases": [{"version": "155.0.1", "timestamp": "1788393600"}]
        }"#;
        let a: Appstream = serde_json::from_str(json).expect("parse");
        assert_eq!(a.name, "Firefox");
        assert_eq!(a.project_license, "MPL-2.0");
        assert_eq!(a.categories, vec!["Network", "WebBrowser"]);
        assert_eq!(a.description_text(), "When it comes to your life online.");
        assert_eq!(
            a.icon_url(),
            Some("https://dl.flathub.org/media/icons/128x128/org.mozilla.firefox.png")
        );
        assert_eq!(
            a.latest_release().map(|r| r.version.as_str()),
            Some("155.0.1")
        );
    }

    #[test]
    fn summary_deserializes_real_fixture() {
        let json = r#"{
            "branch": "stable",
            "timestamp": 1788531696,
            "installed_size": 334923776,
            "download_size": 125615082,
            "arches": ["x86_64", "aarch64"],
            "metadata": {
                "runtime": "org.freedesktop.Platform/x86_64/25.08",
                "runtimeName": "Freedesktop Platform version 25.08",
                "runtimeIsEol": false,
                "sdk": "org.freedesktop.Sdk/x86_64/25.08",
                "runtimeInstalledSize": 659874304,
                "permissions": {
                    "shared": ["network", "ipc"],
                    "sockets": ["x11", "wayland"],
                    "filesystems": ["xdg-download"],
                    "session-bus": {"own": ["org.mozilla.firefox.*"], "talk": ["org.gtk.vfs.*"]},
                    "system-bus": {"talk": ["org.freedesktop.NetworkManager"]}
                },
                "extensions": {"org.mozilla.firefox.Locale": {"locale-subset": "true"}}
            }
        }"#;
        let s: Summary = serde_json::from_str(json).expect("parse");
        assert_eq!(s.download_size, 125615082);
        assert_eq!(s.installed_size, 334923776);
        assert_eq!(s.branch, "stable");
        assert_eq!(
            s.metadata.runtime_name.as_deref(),
            Some("Freedesktop Platform version 25.08")
        );
        assert!(!s.metadata.runtime_is_eol);
        assert_eq!(s.metadata.permissions.shared, vec!["network", "ipc"]);
        assert_eq!(s.metadata.permissions.session_bus.own.len(), 1);
        assert_eq!(s.metadata.extensions.len(), 1);
        assert!(!s.metadata.permissions.describe().is_empty());
    }

    #[test]
    fn collection_page_deserializes_real_fixture() {
        // 真实抓取的字段子集（2026-09-10，collection/category/Game）
        let json = r#"{
            "hits": [{
                "name": "Sober",
                "summary": "Play, chat & explore on Roblox",
                "id": "org_vinegarhq_Sober",
                "app_id": "org.vinegarhq.Sober",
                "icon": "https://dl.flathub.org/media/org/vinegarhq/Sober/icons/128x128/org.vinegarhq.Sober.png",
                "main_categories": "game",
                "sub_categories": ["GNOME", "GTK"],
                "developer_name": "VinegarHQ & Sober contributors",
                "project_license": "LicenseRef-proprietary",
                "is_free_license": false,
                "installs_last_month": 210126,
                "runtime": "org.gnome.Platform/x86_64/50"
            }],
            "page": 1,
            "totalPages": 3,
            "hitsPerPage": 250,
            "totalHits": 727
        }"#;
        let p: CollectionPage = serde_json::from_str(json).expect("parse");
        assert_eq!(p.hits.len(), 1);
        assert_eq!(p.total_pages, 3);
        assert_eq!(p.hits_per_page, 250);
        assert_eq!(p.total_hits, 727);
        assert!(p.has_more());
        assert_eq!(p.hits[0].app_id, "org.vinegarhq.Sober");
        assert_eq!(p.hits[0].main_categories.as_deref(), Some("game"));
        assert_eq!(p.hits[0].installs_last_month, Some(210126));
        assert!(p.hits[0].icon_url().is_some());
    }

    #[test]
    fn advisory_deserializes_null_fields() {
        // 实测：2444 条公告中 2239 条 ticket 为 null、202 条 fixed 为 null
        let json = r#"[
          {"name":"AVG-2843","packages":["vim"],"status":"Unknown","severity":"Unknown",
           "type":"unknown","affected":"9.0.1224-1","fixed":null,"ticket":null,
           "issues":["CVE-2023-0433"]},
          {"name":"AVG-1","packages":null,"affected":null,"fixed":null,"issues":null}
        ]"#;
        let list: Vec<Advisory> = serde_json::from_str(json).expect("必须容忍 null 字段");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "AVG-2843");
        assert_eq!(list[0].fixed, "", "null 视为尚未修复");
        // fixed 为空 = 尚未修复：凡是 >= affected 的版本都仍然受影响
        assert!(list[0].matches("vim", "9.0.1224-1"));
        assert!(list[0].matches("vim", "9.0.1300-1"));
        assert!(
            !list[0].matches("vim", "9.0.1223-1"),
            "低于 affected 不受影响"
        );
        assert!(!list[0].matches("nano", "9.0.1300-1"), "包名不匹配");
        assert!(list[1].packages.is_empty());
        assert!(list[1].issues.is_empty());
        assert!(
            !list[1].matches("anything", "1.0"),
            "affected 为空则无法判定区间"
        );
    }

    #[test]
    fn paging_is_normalized() {
        assert_eq!(normalize_paging(0, 0), (1, 10));
        assert_eq!(normalize_paging(2, 50), (2, 50));
        assert_eq!(normalize_paging(1, 9999), (1, 250));
    }

    #[test]
    fn icon_url_is_predictable_and_safe() {
        assert_eq!(
            icon_url_for("org.mozilla.firefox").as_deref(),
            Some("https://dl.flathub.org/media/icons/128x128/org.mozilla.firefox.png")
        );
        assert_eq!(
            icon_url_for("com.github.gmg137.netease-cloud-music-gtk").as_deref(),
            Some(
                "https://dl.flathub.org/media/icons/128x128/com.github.gmg137.netease-cloud-music-gtk.png"
            )
        );
        // 非法输入必须拒绝，避免把任意字符串拼进 URL
        for bad in [
            "",
            "nodot",
            ".leading",
            "trailing.",
            "double..dot",
            "with space",
            "slash/inside",
            "query?x",
            "..",
        ] {
            assert!(icon_url_for(bad).is_none(), "{bad:?} 必须被拒绝");
        }
        assert!(icon_url_for(&"a.".repeat(200)).is_none(), "超长必须被拒绝");
    }

    #[test]
    fn collection_kind_slugs_match_api() {
        assert_eq!(CollectionKind::Trending.slug(), "trending");
        assert_eq!(CollectionKind::Popular.slug(), "popular");
        assert_eq!(CollectionKind::RecentlyUpdated.slug(), "recently-updated");
        assert_eq!(CollectionKind::RecentlyAdded.slug(), "recently-added");
    }

    #[test]
    fn flathub_categories_are_ascii_and_unique() {
        let mut names: Vec<&str> = FLATHUB_CATEGORIES.iter().map(|(c, _)| *c).collect();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), before);
        assert!(
            names
                .iter()
                .all(|c| c.bytes().all(|b| b.is_ascii_alphanumeric()))
        );
    }

    #[test]
    fn ensure_https_rejects_other_schemes() {
        assert!(ensure_https("https://flathub.org/x").is_ok());
        assert!(ensure_https("http://flathub.org/x").is_err());
        assert!(ensure_https("file:///etc/passwd").is_err());
        assert!(ensure_https("javascript:alert(1)").is_err());
    }
}
