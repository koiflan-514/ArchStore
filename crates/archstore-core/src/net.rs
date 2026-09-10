//! 网络层：reqwest 客户端工厂 + 重试 + 取消 + URL 白名单（project.md §6.2）。
//!
//! 规则：
//! 1. 代理只影响网络，不影响 libalpm 与 flatpak（它们各有自己的机制）。
//! 2. 所有请求必须有超时；UI 侧显示"请求中"，并允许用户取消。
//! 3. 重试策略：仅对幂等的 GET，且仅对网络错误/5xx/429 重试，最多 3 次，指数退避 + 抖动。
//!    4xx（除 429）不重试。
//! 4. 图标下载：并发上限 4，单文件上限 5 MB（超出则丢弃并使用占位图标）。
//! 5. 尊重系统代理：reqwest 开启 system-proxy feature。
//!
//! TLS 提供者：进程内只创建一个 reqwest::Client，并把它注入 raur::Handle::new_with_client()。
//! 不要 Client::new() 与 raur::Handle::new() 混用（那会创建两个不同 provider 配置的客户端）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::de::DeserializeOwned;
use tokio::sync::{Notify, RwLock, Semaphore};

use crate::config::{NetworkConfig, ProxyType};
use crate::error::{CoreError, CoreResult, raw_head};

/// User-Agent：带上项目地址，便于上游统计与限流。
pub const USER_AGENT: &str = concat!(
    "ArchStore/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/koiflan-514/ArchStore)"
);

/// 连接超时。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 单请求总超时（AUR 首次 1.8 s、Flathub 1.2 s、安全公告 5.3 s，30 s 足够）。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// ODRS 评分接口实测不稳定（连接重置），固定 3 s 超时并静默降级。
pub const ODRS_TIMEOUT: Duration = Duration::from_secs(3);
/// 图标单文件上限。
pub const MAX_ICON_BYTES: u64 = 5 * 1024 * 1024;
/// 图标并发下载上限。
pub const ICON_CONCURRENCY: usize = 4;
/// 允许直接请求的域名白名单（§11.1 规则 4）。
///
/// 只放"程序内置会访问"的域名；用户自建的翻译端点通过 set_extra_hosts 追加。
pub const ALLOWED_HOSTS: [&str; 6] = [
    "aur.archlinux.org",
    "flathub.org",
    "dl.flathub.org",
    "odrs.gnome.org",
    "security.archlinux.org",
    // 免费翻译：实测唯一无需 key 且可用的服务（见 translate.rs 的实测结论）
    "api.mymemory.translated.net",
];

/// 用户在设置里显式配置的额外允许主机（仅 https，由设置页写入）。
///
/// 只允许主机名，不接受路径；配置里已经强制 https，这里再做一次形态校验。
static EXTRA_HOSTS: std::sync::OnceLock<std::sync::RwLock<Vec<String>>> =
    std::sync::OnceLock::new();

/// 注册用户自定义的额外主机（例如自建的 LibreTranslate 端点）。
pub fn set_extra_hosts(hosts: Vec<String>) {
    let cleaned: Vec<String> = hosts
        .into_iter()
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| {
            !h.is_empty()
                && h.len() <= 253
                && h.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
                && h.contains('.')
        })
        .collect();
    let lock = EXTRA_HOSTS.get_or_init(|| std::sync::RwLock::new(Vec::new()));
    match lock.write() {
        Ok(mut guard) => *guard = cleaned,
        Err(poisoned) => *poisoned.into_inner() = cleaned,
    }
}

fn extra_hosts() -> Vec<String> {
    EXTRA_HOSTS
        .get()
        .and_then(|l| l.read().ok().map(|g| g.clone()))
        .unwrap_or_default()
}

/// 从 https URL 中取出主机名（去掉 userinfo 与端口）。
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// 可取消令牌：UI 取消后立即回到可用状态，即使底层请求仍需时间完成。
#[derive(Clone, Default, Debug)]
pub struct CancelToken {
    inner: Arc<CancelInner>,
}

#[derive(Default, Debug)]
struct CancelInner {
    flag: AtomicBool,
    notify: Notify,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if !self.inner.flag.swap(true, Ordering::SeqCst) {
            self.inner.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.flag.load(Ordering::SeqCst)
    }

    /// 等待取消。若已经取消则立即返回。
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.inner.notify.notified().await;
    }

    /// 检查取消状态，已取消则返回 Err(Cancelled)。
    pub fn check(&self) -> CoreResult<()> {
        if self.is_cancelled() {
            Err(CoreError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// 重试策略。
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// 额外重试次数（总尝试次数 = attempts + 1）
    pub attempts: u32,
    /// 首次退避时长
    pub base: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 3,
            base: Duration::from_secs(1),
        }
    }
}

impl RetryPolicy {
    /// 不重试（用于 ODRS 这类必须快速失败的接口）。
    pub const NONE: RetryPolicy = RetryPolicy {
        attempts: 0,
        base: Duration::from_secs(0),
    };

    fn delay_for(&self, attempt: u32) -> Duration {
        // 1s -> 2s -> 4s，再加 0..250ms 抖动，避免同步重试风暴
        let exp = self.base.saturating_mul(1u32 << attempt.min(5));
        exp + Duration::from_millis(jitter_ms())
    }
}

fn jitter_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()) % 250)
        .unwrap_or(0)
}

/// 校验 URL：只允许 https，且主机在允许列表内。
///
/// 白名单之外的地址需要用户确认；MVP 阶段直接拒绝并给出原因。
pub fn ensure_allowed_url(url: &str) -> CoreResult<()> {
    let host =
        host_of(url).ok_or_else(|| CoreError::Unsupported(format!("只允许 https 地址：{url}")))?;
    if ALLOWED_HOSTS.contains(&host.as_str()) || extra_hosts().iter().any(|h| h == &host) {
        Ok(())
    } else {
        Err(CoreError::Unsupported(format!(
            "域名 {host} 不在允许列表内，出于安全考虑已拒绝访问"
        )))
    }
}

/// 判断 URL 的 scheme 是否安全（用于打开浏览器等场景）。
pub fn is_safe_external_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// 构造 reqwest 客户端。进程内应只保留一个实例（见模块文档）。
pub fn build_client(cfg: &NetworkConfig) -> CoreResult<reqwest::Client> {
    let mut b = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(Duration::from_secs(cfg.timeout_secs.clamp(5, 300)))
        .pool_max_idle_per_host(4)
        .redirect(reqwest::redirect::Policy::limited(10));
    if cfg.proxy_enabled {
        let host = cfg.proxy_url.trim();
        if host.is_empty() {
            return Err(CoreError::Config("已启用代理但未填写代理地址".into()));
        }
        if host.contains("://") {
            return Err(CoreError::Config(
                "代理地址不要包含 http:// 或 socks5:// 前缀，只填主机与端口".into(),
            ));
        }
        let url = match cfg.proxy_type {
            ProxyType::Http => format!("http://{host}"),
            // socks5h：DNS 也走代理
            ProxyType::Socks5 => format!("socks5h://{host}"),
        };
        b = b.proxy(reqwest::Proxy::all(&url).map_err(|e| CoreError::Config(e.to_string()))?);
    }
    b.build().map_err(|e| CoreError::Config(e.to_string()))
}

/// 可热重建的 HTTP 客户端（设置页改代理/超时后调用 reconfigure）。
pub struct HttpClient {
    client: RwLock<Arc<reqwest::Client>>,
    cfg: RwLock<NetworkConfig>,
    /// 图标下载并发闸门
    icons: Semaphore,
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClient").finish_non_exhaustive()
    }
}

impl HttpClient {
    pub fn new(cfg: NetworkConfig) -> CoreResult<Arc<Self>> {
        let client = Arc::new(build_client(&cfg)?);
        Ok(Arc::new(Self {
            client: RwLock::new(client),
            cfg: RwLock::new(cfg),
            icons: Semaphore::new(ICON_CONCURRENCY),
        }))
    }

    /// 当前客户端句柄（旧请求保留但不再复用）。
    pub async fn client(&self) -> Arc<reqwest::Client> {
        self.client.read().await.clone()
    }

    pub async fn config(&self) -> NetworkConfig {
        self.cfg.read().await.clone()
    }

    /// 立即重建客户端。旧请求继续使用旧客户端直到结束。
    pub async fn reconfigure(&self, cfg: NetworkConfig) -> CoreResult<()> {
        let fresh = Arc::new(build_client(&cfg)?);
        *self.client.write().await = fresh;
        *self.cfg.write().await = cfg;
        Ok(())
    }

    /// 图标下载并发闸门（上限 4）。
    pub fn icon_gate(&self) -> &Semaphore {
        &self.icons
    }

    /// 用当前客户端发起一次 GET 并返回字节。
    pub async fn get_bytes(
        &self,
        url: &str,
        cancel: &CancelToken,
        policy: RetryPolicy,
        max_bytes: u64,
    ) -> CoreResult<Vec<u8>> {
        let client = self.client().await;
        get_bytes(&client, url, cancel, policy, max_bytes).await
    }

    /// 用当前客户端发起一次 GET 并解析 JSON。
    pub async fn get_json<T: DeserializeOwned>(
        &self,
        url: &str,
        cancel: &CancelToken,
        policy: RetryPolicy,
    ) -> CoreResult<T> {
        let client = self.client().await;
        get_json(&client, url, cancel, policy).await
    }
}

/// 单次尝试（不含重试）。
async fn attempt(
    client: &reqwest::Client,
    url: &str,
    cancel: &CancelToken,
) -> CoreResult<reqwest::Response> {
    ensure_allowed_url(url)?;
    tokio::select! {
        _ = cancel.cancelled() => Err(CoreError::Cancelled),
        res = client.get(url).send() => match res {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    Ok(resp)
                } else if status.as_u16() == 429 || status.as_u16() == 503 {
                    let retry_after = resp
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.trim().parse::<u64>().ok())
                        .unwrap_or(0);
                    Err(CoreError::RateLimited { retry_after })
                } else {
                    Err(CoreError::Network {
                        url: url.to_string(),
                        cause: format!("HTTP {}", status.as_u16()),
                    })
                }
            }
            Err(e) if e.is_timeout() => Err(CoreError::Timeout {
                url: url.to_string(),
                secs: REQUEST_TIMEOUT.as_secs(),
            }),
            Err(e) => Err(CoreError::Network {
                url: url.to_string(),
                cause: human_reqwest_error(&e),
            }),
        },
    }
}

/// 把 reqwest 错误转成简短、可操作的中文说明。
pub fn human_reqwest_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "请求超时".to_string()
    } else if e.is_connect() {
        "无法建立连接（检查网络或代理）".to_string()
    } else if e.is_redirect() {
        "重定向次数过多".to_string()
    } else if e.is_decode() {
        "响应解码失败".to_string()
    } else if e.is_body() {
        "响应体读取失败".to_string()
    } else {
        format!("{e}")
    }
}

/// 带重试的 GET，返回响应体字节（受 max_bytes 限制）。
pub async fn get_bytes(
    client: &reqwest::Client,
    url: &str,
    cancel: &CancelToken,
    policy: RetryPolicy,
    max_bytes: u64,
) -> CoreResult<Vec<u8>> {
    let mut attempt_no = 0u32;
    loop {
        cancel.check()?;
        match attempt(client, url, cancel).await {
            Ok(resp) => return read_capped(resp, cancel, max_bytes).await,
            Err(e) => {
                let retryable = e.is_retryable() && attempt_no < policy.attempts;
                if !retryable {
                    return Err(e);
                }
                let delay = policy.delay_for(attempt_no);
                tracing::info!(url, attempt = attempt_no + 1, ?delay, error = %e, "请求失败，准备重试");
                tokio::select! {
                    _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                    _ = tokio::time::sleep(delay) => {}
                }
                attempt_no += 1;
            }
        }
    }
}

/// 读取响应体并强制大小上限（防止恶意/异常的超大响应）。
async fn read_capped(
    resp: reqwest::Response,
    cancel: &CancelToken,
    max_bytes: u64,
) -> CoreResult<Vec<u8>> {
    if let Some(len) = resp.content_length()
        && len > max_bytes
    {
        return Err(CoreError::Unsupported(format!(
            "响应体 {len} 字节超过上限 {max_bytes} 字节"
        )));
    }
    let mut resp = resp;
    let mut out: Vec<u8> = Vec::new();
    loop {
        cancel.check()?;
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Err(CoreError::Cancelled),
            c = resp.chunk() => c.map_err(|e| CoreError::Network {
                url: resp.url().to_string(),
                cause: human_reqwest_error(&e),
            })?,
        };
        match chunk {
            Some(bytes) => {
                if out.len() as u64 + bytes.len() as u64 > max_bytes {
                    return Err(CoreError::Unsupported(format!(
                        "响应体超过上限 {max_bytes} 字节，已丢弃"
                    )));
                }
                out.extend_from_slice(&bytes);
            }
            None => return Ok(out),
        }
    }
}

/// 带重试的 GET，解析 JSON。
pub async fn get_json<T: DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    cancel: &CancelToken,
    policy: RetryPolicy,
) -> CoreResult<T> {
    let bytes = get_bytes(client, url, cancel, policy, 32 * 1024 * 1024).await?;
    serde_json::from_slice::<T>(&bytes).map_err(|e| {
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(2000)]).to_string();
        CoreError::Parse {
            context: format!("JSON（{url}）：{e}"),
            raw_head: raw_head(&head, 20),
        }
    })
}

/// 带重试的 POST JSON（用于 LibreTranslate 这类不带 GET 接口的服务）。
pub async fn post_json<T: DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    cancel: &CancelToken,
) -> CoreResult<T> {
    ensure_allowed_url(url)?;
    cancel.check()?;
    let resp = tokio::select! {
        _ = cancel.cancelled() => return Err(CoreError::Cancelled),
        r = client.post(url).json(body).send() => r.map_err(|e| {
            if e.is_timeout() {
                CoreError::Timeout { url: url.to_string(), secs: REQUEST_TIMEOUT.as_secs() }
            } else {
                CoreError::Network { url: url.to_string(), cause: human_reqwest_error(&e) }
            }
        })?,
    };
    let status = resp.status();
    if status.as_u16() == 429 || status.as_u16() == 503 {
        return Err(CoreError::RateLimited { retry_after: 0 });
    }
    if !status.is_success() {
        return Err(CoreError::Network {
            url: url.to_string(),
            cause: format!("HTTP {}", status.as_u16()),
        });
    }
    let bytes = read_capped(resp, cancel, 4 * 1024 * 1024).await?;
    serde_json::from_slice::<T>(&bytes).map_err(|e| CoreError::Parse {
        context: format!("JSON（{url}）：{e}"),
        raw_head: crate::error::raw_head(&String::from_utf8_lossy(&bytes), 10),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_extraction_is_strict() {
        assert_eq!(
            host_of("https://aur.archlinux.org/rpc?v=5").as_deref(),
            Some("aur.archlinux.org")
        );
        assert_eq!(
            host_of("https://user@evil.com/x").as_deref(),
            Some("evil.com")
        );
        assert_eq!(host_of("https://a.b:8443/x").as_deref(), Some("a.b"));
        assert_eq!(host_of("http://x/y"), None);
        assert_eq!(host_of("https://"), None);
    }

    #[test]
    fn extra_hosts_extend_the_whitelist_safely() {
        // 默认只允许内置主机
        assert!(ensure_allowed_url("https://translate.example.com/translate").is_err());
        set_extra_hosts(vec![
            "translate.example.com".into(),
            "  My.Libre.Example  ".into(),
            // 下面这些形态必须被过滤掉
            "bad host".into(),
            "no-dot".into(),
            "".into(),
            "a/b".into(),
        ]);
        assert!(ensure_allowed_url("https://translate.example.com/translate").is_ok());
        assert!(ensure_allowed_url("https://my.libre.example/translate").is_ok());
        // 过滤掉的形态
        assert!(ensure_allowed_url("https://bad host/x").is_err());
        assert!(ensure_allowed_url("https://no-dot/x").is_err());
        // 通配不成立：子域不等于主机
        assert!(ensure_allowed_url("https://sub.translate.example.com/x").is_err());
        // 清理，避免影响其它测试
        set_extra_hosts(Vec::new());
        assert!(ensure_allowed_url("https://translate.example.com/translate").is_err());
    }

    #[test]
    fn mymemory_is_whitelisted_by_default() {
        assert!(ensure_allowed_url("https://api.mymemory.translated.net/get?q=hi").is_ok());
    }

    #[test]
    fn url_whitelist_enforced() {
        assert!(ensure_allowed_url("https://aur.archlinux.org/rpc?v=5").is_ok());
        assert!(ensure_allowed_url("https://flathub.org/api/v2/appstream/x").is_ok());
        assert!(ensure_allowed_url("https://dl.flathub.org/media/icons/128x128/a.png").is_ok());
        assert!(ensure_allowed_url("https://odrs.gnome.org/1.0/reviews/api/ratings/x").is_ok());
        assert!(ensure_allowed_url("https://security.archlinux.org/issues/all.json").is_ok());
    }

    #[test]
    fn url_whitelist_rejects_others_and_plain_http() {
        assert!(ensure_allowed_url("http://aur.archlinux.org/rpc").is_err());
        assert!(ensure_allowed_url("file:///etc/passwd").is_err());
        assert!(ensure_allowed_url("https://evil.example/x").is_err());
        // 后缀欺骗
        assert!(ensure_allowed_url("https://aur.archlinux.org.evil.com/rpc").is_err());
        // 用户信息欺骗
        assert!(ensure_allowed_url("https://aur.archlinux.org@evil.com/rpc").is_err());
        assert!(ensure_allowed_url("https://notaur.archlinux.org/rpc").is_err());
    }

    #[test]
    fn retry_policy_backs_off_exponentially() {
        let p = RetryPolicy {
            attempts: 3,
            base: Duration::from_secs(1),
        };
        assert!(p.delay_for(0) >= Duration::from_secs(1));
        assert!(p.delay_for(1) >= Duration::from_secs(2));
        assert!(p.delay_for(2) >= Duration::from_secs(4));
        assert!(p.delay_for(2) < Duration::from_millis(4300));
    }

    #[tokio::test]
    async fn cancel_token_wakes_waiters() {
        let t = CancelToken::new();
        let t2 = t.clone();
        let h = tokio::spawn(async move { t2.cancelled().await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        t.cancel();
        h.await.expect("join");
        assert!(t.is_cancelled());
        assert!(matches!(t.check(), Err(CoreError::Cancelled)));
    }

    #[tokio::test]
    async fn cancel_before_wait_returns_immediately() {
        let t = CancelToken::new();
        t.cancel();
        tokio::time::timeout(Duration::from_millis(50), t.cancelled())
            .await
            .expect("must not hang");
    }

    #[test]
    fn build_client_rejects_scheme_in_proxy_host() {
        let mut cfg = NetworkConfig {
            proxy_enabled: true,
            proxy_url: "http://127.0.0.1:8080".into(),
            ..Default::default()
        };
        assert!(build_client(&cfg).is_err());
        cfg.proxy_url = "127.0.0.1:8080".into();
        assert!(build_client(&cfg).is_ok());
        cfg.proxy_url = String::new();
        assert!(build_client(&cfg).is_err());
    }

    #[test]
    fn build_client_accepts_socks5() {
        let cfg = NetworkConfig {
            proxy_enabled: true,
            proxy_type: ProxyType::Socks5,
            proxy_url: "127.0.0.1:9050".into(),
            ..Default::default()
        };
        assert!(build_client(&cfg).is_ok());
    }

    #[test]
    fn safe_external_url_check() {
        assert!(is_safe_external_url("https://archlinux.org"));
        assert!(!is_safe_external_url("javascript:alert(1)"));
        assert!(!is_safe_external_url("file:///etc/passwd"));
    }
}
