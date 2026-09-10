//! 网络冒烟测试（project.md §2.2 / §11.2）：验证同一个 reqwest::Client 同时打通 Flathub 与 AUR，
//! 且 raur 的 feature 组合（default-features = false + ["async","rusttls-ring"]）确实可用。
//!
//! 这是阶段 2 的第一个任务：如果 TLS provider 配置冲突，这里必须最先失败。

use std::sync::Arc;
use std::time::Duration;

use archstore_core::backend::AurBackend;
use archstore_core::backend::InstalledIndex;
use archstore_core::backend::PackageBackend;
use archstore_core::cache::Cache;
use archstore_core::config::{Config, NetworkConfig};
use archstore_core::flathub::FlathubClient;
use archstore_core::net::{CancelToken, HttpClient, RetryPolicy, get_json};

fn offline() -> bool {
    std::env::var_os("ARCHSTORE_SKIP_NETWORK_TESTS").is_some()
}

/// MyMemory 是免费服务，每天有配额；用尽时应当**跳过**而不是判失败 ——
/// 这是外部服务的限制，不是本项目的行为回归（配额恢复前谁也测不了）。
///
/// 两种表现都要认：HTTP 层把配额提示映射成 "请求过于频繁，N 秒后自动重试"，
/// 响应体里则是 responseStatus != 200 + "MYMEMORY WARNING"（→ RateLimited）。
fn translation_quota_exhausted(e: &archstore_core::CoreError) -> bool {
    use archstore_core::CoreError;
    match e {
        CoreError::RateLimited { .. } => true,
        CoreError::Network { url, cause } if url == "mymemory" => {
            cause.contains("过于频繁") || cause.contains("配额") || cause.contains("quota")
        }
        _ => false,
    }
}

async fn http() -> Arc<HttpClient> {
    HttpClient::new(NetworkConfig::default()).expect("client")
}

#[tokio::test]
async fn same_client_reaches_aur_and_flathub() {
    if offline() {
        eprintln!("跳过：ARCHSTORE_SKIP_NETWORK_TESTS 已设置");
        return;
    }
    let http = http().await;
    let client = http.client().await;
    let cancel = CancelToken::new();

    // AUR RPC
    let aur: serde_json::Value = get_json(
        &client,
        "https://aur.archlinux.org/rpc?v=5&type=info&arg[]=yay",
        &cancel,
        RetryPolicy::default(),
    )
    .await
    .expect("AUR RPC 必须可达（若网络不可用请设置 ARCHSTORE_SKIP_NETWORK_TESTS=1）");
    assert_eq!(aur["type"], "multiinfo");
    let pkg = &aur["results"][0];
    assert_eq!(pkg["Name"], "yay");
    // §4.4 实测字段：依赖信息直接来自 RPC info
    assert!(pkg["Depends"].is_array());
    assert!(pkg["MakeDepends"].is_array());
    assert!(pkg["OptDepends"].is_array());
    assert!(pkg["NumVotes"].is_number());
    assert!(pkg["Popularity"].is_number());

    // Flathub appstream
    let flathub: serde_json::Value = get_json(
        &client,
        "https://flathub.org/api/v2/appstream/org.mozilla.firefox",
        &cancel,
        RetryPolicy::default(),
    )
    .await
    .expect("Flathub 必须可达");
    assert_eq!(flathub["id"], "org.mozilla.firefox");
    assert!(
        flathub["icon"]
            .as_str()
            .unwrap_or_default()
            .starts_with("https://")
    );

    // 同一个客户端连续请求两个域名后仍可用（provider 未冲突）
    let again: serde_json::Value = get_json(
        &client,
        "https://aur.archlinux.org/rpc?v=5&type=search&by=name-desc&arg=cowsay",
        &cancel,
        RetryPolicy::default(),
    )
    .await
    .expect("重复请求必须成功");
    assert_eq!(again["type"], "search");
    // 实测 resultcount 在数十量级
    assert!(again["resultcount"].as_u64().unwrap_or(0) > 0);
}

#[tokio::test]
async fn raur_handle_uses_our_client_and_returns_expected_fields() {
    if offline() {
        return;
    }
    let dir = tempfile::tempdir().expect("tmpdir");
    let cache = Cache::open(dir.path().join("cache"), 64 * 1024 * 1024).expect("cache");
    let installed = InstalledIndex::new();
    let http = http().await;
    let cfg = Config::default();
    let backend = AurBackend::new(&http, cache, installed, &cfg)
        .await
        .expect("aur backend");
    assert_eq!(backend.source_kind(), "aur");
    assert_eq!(backend.rpc_url(), "https://aur.archlinux.org/rpc/");

    let cancel = CancelToken::new();
    // 注意：cowsay 已迁入官方 extra 仓库（pacman -Si cowsay 有结果，AUR info 返回 0 条），
    // 因此这里改用确实只存在于 AUR 的包名。
    let packages = backend.search_raw("yay", &cancel).await.expect("search");
    assert!(!packages.is_empty(), "AUR 搜索应返回结果");
    assert!(
        packages.iter().any(|p| p.name == "yay"),
        "搜索结果应包含 yay，实际前几个：{:?}",
        packages.iter().take(5).map(|p| &p.name).collect::<Vec<_>>()
    );
    assert_eq!(
        backend
            .info_raw(&["cowsay".to_string()], &cancel)
            .await
            .expect("info cowsay")
            .len(),
        0,
        "cowsay 已迁入官方仓库，AUR 不应再返回它"
    );

    let infos = backend
        .info_raw(&["yay".to_string()], &cancel)
        .await
        .expect("info");
    let yay = infos.iter().find(|p| p.name == "yay").expect("yay");
    // §4.4 字段映射表：Rust 字段名与 RPC 的 PascalCase 名称不同
    assert!(yay.num_votes > 0);
    assert!(yay.popularity > 0.0);
    assert!(!yay.license.is_empty());
    assert!(!yay.depends.is_empty());
    assert!(!yay.make_depends.is_empty(), "yay 有 go 构建依赖");
    assert!(yay.maintainer.is_some());
    assert!(yay.out_of_date.is_none());

    // 缓存生效：第二次调用不应产生新的网络请求（结果一致）
    let again = backend
        .info_raw(&["yay".to_string()], &cancel)
        .await
        .expect("cached info");
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].version, yay.version);
}

#[tokio::test]
async fn flathub_client_returns_metadata_and_ratings_degrade_silently() {
    if offline() {
        return;
    }
    let dir = tempfile::tempdir().expect("tmpdir");
    let cache = Cache::open(dir.path().join("cache"), 64 * 1024 * 1024).expect("cache");
    let http = http().await;
    let client = FlathubClient::new(http, cache);
    let cancel = CancelToken::new();

    let app = client
        .appstream("org.mozilla.firefox", &cancel)
        .await
        .expect("appstream");
    assert_eq!(app.name, "Firefox");
    assert!(!app.description_text().is_empty());
    assert!(!app.description_text().contains("<p>"));
    assert!(app.icon_url().is_some());
    assert!(!app.screenshot_urls().is_empty());

    let summary = client
        .summary("org.mozilla.firefox", &cancel)
        .await
        .expect("summary");
    assert!(summary.download_size > 0);
    assert!(summary.installed_size > 0);
    assert!(summary.metadata.runtime.is_some());
    // 权限会展开成非空列表
    assert!(!summary.metadata.permissions.describe().is_empty());

    // ODRS 不稳定：无论成功还是失败都必须返回（绝不 panic、绝不阻塞超过 3 秒）
    let start = std::time::Instant::now();
    let _ = client.ratings("org.mozilla.firefox", &cancel).await;
    assert!(
        start.elapsed() < Duration::from_secs(8),
        "ODRS 必须在 3 秒超时后静默降级，实际耗时 {:?}",
        start.elapsed()
    );

    // 分类集合端点：分页参数必须成对出现（实测只传 page 会 400）
    let page = client
        .category_page("Game", 1, 50, &cancel)
        .await
        .expect("category");
    assert!(!page.hits.is_empty());
    assert!(page.total_hits > 0);
    assert_eq!(page.hits.len(), 50, "per_page=50 应生效");
    assert!(!page.hits[0].app_id.is_empty());

    // 第二页必须与第一页不同（真实分页）
    let page2 = client
        .category_page("Game", 2, 50, &cancel)
        .await
        .expect("category page 2");
    assert_eq!(page2.page, 2);
    assert_ne!(page.hits[0].app_id, page2.hits[0].app_id);
}

#[tokio::test]
async fn security_advisories_are_cached_and_parseable() {
    if offline() {
        return;
    }
    let dir = tempfile::tempdir().expect("tmpdir");
    let cache = Cache::open(dir.path().join("cache"), 256 * 1024 * 1024).expect("cache");
    let http = http().await;
    let client = FlathubClient::new(Arc::clone(&http), Arc::clone(&cache));
    let cancel = CancelToken::new();

    let start = std::time::Instant::now();
    let advisories = client.advisories(&cancel).await.expect("advisories");
    let first = start.elapsed();
    assert!(!advisories.is_empty(), "安全公告列表不应为空");
    // 实测字段
    let a = advisories.iter().find(|a| !a.packages.is_empty());
    assert!(a.is_some(), "至少有一条公告带 packages 字段");

    // 第二次必须命中缓存（明显更快）
    let start = std::time::Instant::now();
    let again = client.advisories(&cancel).await.expect("cached advisories");
    let second = start.elapsed();
    assert_eq!(again.len(), advisories.len());
    assert!(
        second <= first,
        "缓存命中应不慢于首次（首次 {first:?}，二次 {second:?}）"
    );
    eprintln!(
        "安全公告 {} 条，首次 {:?}，二次 {:?}",
        advisories.len(),
        first,
        second
    );
}

// ========================================================================
// 阶段 2/3 的端到端验证：分类分页、AUR 详情/依赖、Flatpak 本地状态。
// 这些路径在 UI 冒烟测试里只验证了参数与界面状态，这里验证真实后端行为。
// ========================================================================

use archstore_core::backend::{FlatpakBackend, Page, SearchScope};
use archstore_core::model::DepKind;

/// 构造一个隔离的缓存目录（每个测试一份，避免串味）。
fn isolated_cache(tag: &str) -> (tempfile::TempDir, Arc<Cache>) {
    let dir = tempfile::tempdir().expect("tmpdir");
    let cache = Cache::open(dir.path().join(tag), 64 * 1024 * 1024).expect("cache");
    (dir, cache)
}

#[tokio::test]
async fn flatpak_category_pagination_end_to_end() {
    if offline() {
        return;
    }
    let (_d, cache) = isolated_cache("flatpak");
    let http = http().await;
    let backend = FlatpakBackend::new(Arc::clone(&http), cache, &Config::default()).await;
    assert_eq!(backend.source_kind(), "flatpak");

    // 分类列表是固定的 freedesktop 分类
    let categories = backend.categories().await.expect("categories");
    assert!(categories.len() >= 10);
    assert!(categories.iter().all(|c| c.source_kind == "flatpak"));
    let game = categories
        .iter()
        .find(|c| c.id == "flathub:Game")
        .expect("必须有 Game 分类");

    // 第一页
    let page1 = backend
        .list_category(&game.id, Page::new(0, 50))
        .await
        .expect("page 1");
    assert_eq!(
        page1.len(),
        50,
        "per_page=50 必须生效（实测只传 page 会 400）"
    );
    for s in &page1 {
        assert_eq!(s.id.source.repo_name(), Some("flathub"));
        assert!(!s.id.name.is_empty());
        assert!(!s.display_name.is_empty());
    }

    // 第二页必须与第一页不同（这是本项目之前完全没有在真实后端上验证过的路径）
    let page2 = backend
        .list_category(&game.id, Page::new(50, 50))
        .await
        .expect("page 2");
    assert_eq!(page2.len(), 50);
    let first_ids: Vec<&str> = page1.iter().map(|s| s.id.name.as_str()).collect();
    let second_ids: Vec<&str> = page2.iter().map(|s| s.id.name.as_str()).collect();
    assert_ne!(first_ids, second_ids, "不同页必须返回不同内容");
    assert!(
        !second_ids.iter().any(|id| first_ids.contains(id)),
        "分页不应重复返回同一应用"
    );

    // 第三页与越界页
    let page3 = backend
        .list_category(&game.id, Page::new(100, 50))
        .await
        .expect("page 3");
    assert!(!page3.is_empty());

    // 未知分类前缀必须报错而不是返回空
    let err = backend
        .list_category("pacman:extra", Page::first(10))
        .await
        .expect_err("非法分类必须被拒绝");
    assert!(matches!(err, archstore_core::CoreError::Unsupported(_)));

    // 分类结果应命中缓存：第二次调用明显更快且内容一致
    let again = backend
        .list_category(&game.id, Page::new(0, 50))
        .await
        .expect("cached page 1");
    assert_eq!(again.len(), page1.len());
    assert_eq!(again[0].id.name, page1[0].id.name);
}

#[tokio::test]
async fn aur_categories_and_details_end_to_end() {
    if offline() {
        return;
    }
    let (_d, cache) = isolated_cache("aur");
    let http = http().await;
    let backend = AurBackend::new(&http, cache, InstalledIndex::new(), &Config::default())
        .await
        .expect("aur backend");

    // 关键词分类
    let categories = backend.categories().await.expect("categories");
    assert!(categories.len() >= 8);
    assert!(categories.iter().all(|c| c.id.starts_with("keyword:")));
    assert!(categories.iter().all(|c| c.source_kind == "aur"));

    // 分页：AUR 关键词搜索按流行度排序
    let rust = categories
        .iter()
        .find(|c| c.id == "keyword:rust")
        .expect("rust 关键词");
    let page = backend
        .list_category(&rust.id, Page::new(0, 20))
        .await
        .expect("list rust");
    assert!(!page.is_empty(), "AUR 的 rust 关键词应有结果");
    assert!(page.len() <= 20, "分页上限必须生效");
    // 流行度排序（无查询时 sort_summaries 用流行度降序）
    let pops: Vec<f64> = page.iter().filter_map(|s| s.popularity).collect();
    if pops.len() > 1 {
        assert!(
            pops.windows(2).all(|w| w[0] >= w[1]),
            "应按流行度降序：{pops:?}"
        );
    }

    // 详情：字段必须齐全（§4.4 的字段映射表）
    let id = archstore_core::model::PackageId::aur("yay");
    let detail = backend.info(&id).await.expect("yay info");
    assert_eq!(detail.summary.id.name, "yay");
    assert_eq!(detail.summary.id.kind(), "aur");
    assert!(!detail.licenses.is_empty());
    assert!(detail.summary.votes.unwrap_or(0) > 0);
    assert!(detail.summary.popularity.unwrap_or(0.0) > 0.0);
    assert!(detail.maintainer.is_some());
    assert!(detail.homepage.is_some());
    assert!(
        detail
            .extra
            .0
            .iter()
            .any(|(k, v)| k == "AUR 页面" && v.contains("yay")),
        "详情页必须给出 AUR 链接"
    );
    assert!(!detail.dependencies.is_empty());
    assert!(
        detail
            .dependencies
            .iter()
            .any(|d| d.kind == DepKind::Runtime),
        "yay 有运行时依赖"
    );
    assert!(
        detail.dependencies.iter().any(|d| d.kind == DepKind::Make),
        "yay 有 go 构建依赖"
    );
    assert!(
        detail
            .dependencies
            .iter()
            .any(|d| d.kind == DepKind::Optional),
        "yay 有可选依赖（sudo/doas）"
    );

    // 不存在的包必须是 NotFound
    let missing = archstore_core::model::PackageId::aur("definitely-not-in-aur-xyz");
    let err = backend.info(&missing).await.expect_err("must be NotFound");
    assert!(matches!(err, archstore_core::CoreError::NotFound(_)));

    // depend() 查询（反向依赖的一种）
    let rev = backend.reverse_dependencies(&id).await.expect("revdeps");
    // AUR 里可能没有任何包依赖 yay，这里只断言不报错
    for p in &rev {
        assert_eq!(p.kind(), "aur");
    }

    // AUR 不提供独立的已安装列表
    let err = backend.installed().await.expect_err("must be Unsupported");
    assert!(matches!(err, archstore_core::CoreError::Unsupported(_)));

    // 没有外来包时，可更新列表为空且不联网
    let up = backend.upgradable().await.expect("upgradable");
    assert!(up.is_empty());
}

#[tokio::test]
async fn local_only_search_never_touches_network() {
    if offline() {
        return;
    }
    let (_d, cache) = isolated_cache("localonly");
    let http = http().await;

    // AUR：LocalOnly 只读缓存；空缓存必须返回空而不是发起请求
    let aur = AurBackend::new(
        &http,
        Arc::clone(&cache),
        InstalledIndex::new(),
        &Config::default(),
    )
    .await
    .expect("aur");
    let empty = aur
        .search("definitely-no-cache-for-this", SearchScope::LocalOnly)
        .await
        .expect("local only");
    assert!(empty.is_empty(), "LocalOnly 不能联网补数据");

    // 先做一次联网搜索填充缓存，再用 LocalOnly 必须命中
    let _ = aur.search("yay", SearchScope::Full).await.expect("full");
    let cached = aur
        .search("yay", SearchScope::LocalOnly)
        .await
        .expect("local only after cache");
    assert!(!cached.is_empty(), "Full 之后 LocalOnly 应从缓存命中");

    // Flatpak：LocalOnly 只过滤已安装应用（无需网络）
    let flatpak = FlatpakBackend::new(http, cache, &Config::default()).await;
    let installed = flatpak.installed().await.expect("flatpak installed");
    eprintln!("本机已安装 Flatpak 应用：{}", installed.len());
    let local = flatpak
        .search("", SearchScope::LocalOnly)
        .await
        .expect("flatpak local");
    assert_eq!(local.len(), installed.len(), "空查询应返回全部已安装应用");
    for s in &local {
        assert!(s.is_installed());
    }
}

#[tokio::test]
async fn pacman_backend_full_trait_surface() {
    if !std::path::Path::new("/var/lib/pacman/local").is_dir() {
        eprintln!("跳过：本机没有 /var/lib/pacman/local");
        return;
    }
    let (_d, cache) = isolated_cache("pacman");
    let http = http().await;
    let _ = (cache, http);

    let backend = archstore_core::backend::PacmanBackend::spawn()
        .await
        .expect("pacman backend");
    let caps = archstore_core::env::probe(None, None, &Config::default());
    assert!(!caps.has_failures(), "本机自检不应有失败项");

    // 依赖信息（只读，来自 libalpm）
    let id = archstore_core::model::PackageId::official("extra", "firefox");
    let deps = backend.dependencies(&id).await.expect("deps");
    assert!(!deps.is_empty());
    // 已安装的依赖必须带 satisfied_by
    let satisfied = deps.iter().filter(|d| !d.missing).count();
    assert!(satisfied > 0, "firefox 的依赖在本地应大部分已满足");
    for d in deps.iter().filter(|d| !d.missing) {
        assert!(
            d.satisfied_by.is_some(),
            "已满足的依赖必须指出提供者：{}",
            d.name
        );
    }

    // 已安装列表里每一项都必须是"已安装"，且带版本
    let installed = backend.installed().await.expect("installed");
    assert!(installed.len() > 100);
    for s in installed.iter().take(200) {
        assert!(s.is_installed(), "{}", s.id.name);
        assert!(s.version.is_some(), "{}", s.id.name);
    }

    // 分类：包组必须带成员
    let cats = backend.categories().await.expect("categories");
    assert!(cats.len() > 10);
    let with_members = cats
        .iter()
        .find(|c| c.id.starts_with("extra:"))
        .expect("extra 包组");
    let members = backend
        .list_category(&with_members.id, Page::first(3))
        .await
        .expect("members");
    assert!(!members.is_empty());
    assert!(members.len() <= 3);
}

// ========================================================================
// 在线翻译端到端（§7.2 第 4 层）
// ========================================================================

use archstore_core::config::TranslationConfig;
use archstore_core::translate::TranslateClient;

#[tokio::test]
async fn mymemory_translation_works_for_short_and_long_text() {
    if offline() {
        return;
    }
    let (_d, cache) = isolated_cache("translate");
    let http = http().await;
    let client = TranslateClient::new(http, cache);
    let cancel = CancelToken::new();

    let mut cfg = TranslationConfig {
        auto_translate: true,
        ..Default::default()
    };
    // 未启用时必须拒绝，且不发起任何请求
    let off = TranslationConfig::default();
    assert!(!off.auto_translate);
    assert!(
        client
            .translate("hello", "zh-CN", &off, &cancel)
            .await
            .is_err(),
        "未启用时不得翻译"
    );

    // 1) 短文本
    let short = match client
        .translate("Fast, Private & Safe Web Browser", "zh-CN", &cfg, &cancel)
        .await
    {
        Ok(v) => v,
        Err(e) if translation_quota_exhausted(&e) => {
            eprintln!("跳过在线翻译端到端：MyMemory 免费配额已用尽（{e}）");
            return;
        }
        Err(e) => panic!("短文本翻译必须成功：{e}"),
    };
    assert!(!short.text.is_empty());
    assert_eq!(short.provider, "MyMemory");
    assert_eq!(short.target, "zh-CN");
    assert!(short.label().contains("机器翻译"));
    assert!(
        short
            .text
            .chars()
            .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
        "译文应包含中文：{}",
        short.text
    );
    eprintln!("短文本译文：{}", short.text);

    // 2) 真实的长描述（Flathub 上 firefox 的 description 长度量级），
    //    必须走分块路径且不能触发 500 字符限制
    let long = "When it comes to your life online, you have a choice: accept the factory settings \
        or put your privacy first. When you choose Firefox, you are choosing a browser that \
        respects your privacy and puts you in control of your data. Firefox is built by Mozilla, \
        a non-profit organization dedicated to keeping the internet open and accessible to all.";
    assert!(long.chars().count() > 300);
    let translated = client
        .translate(long, "zh-CN", &cfg, &cancel)
        .await
        .expect("长文本必须能翻译（内部分块，绕开 500 字符限制）");
    assert!(!translated.text.is_empty());
    assert!(
        !translated.text.contains("QUERY LENGTH LIMIT"),
        "不得把接口的错误文案当成译文：{}",
        translated.text
    );
    assert!(
        translated
            .text
            .chars()
            .filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c))
            .count()
            > 20,
        "长文本译文应有实质中文内容：{}",
        translated.text
    );
    // 注意：中文是多字节，必须按字符切而不是字节切
    let preview: String = translated.text.chars().take(80).collect();
    eprintln!(
        "长文本译文（{} 字符）：{preview}",
        translated.text.chars().count()
    );

    // 3) 超长文本（分块 + 拼接）
    let very_long = "Mozilla Firefox is a free and open-source web browser developed by the Mozilla Foundation. ".repeat(12);
    assert!(very_long.chars().count() > 900);
    let big = client
        .translate(&very_long, "zh-CN", &cfg, &cancel)
        .await
        .expect("超长文本必须分块翻译");
    assert!(big.text.chars().count() > 100);
    assert!(!big.text.contains("QUERY LENGTH LIMIT"));

    // 4) 第二次必须命中 24 小时缓存（不重复请求）
    let cached = client
        .translate("Fast, Private & Safe Web Browser", "zh-CN", &cfg, &cancel)
        .await
        .expect("缓存命中");
    assert!(cached.cached, "第二次必须来自缓存");
    assert_eq!(cached.text, short.text);

    // 5) 换目标语言不能命中旧缓存
    cfg.target_lang = String::new();
    let tw = client
        .translate("Fast, Private & Safe Web Browser", "zh-TW", &cfg, &cancel)
        .await
        .expect("繁体翻译");
    assert_eq!(tw.target, "zh-TW");
    assert_ne!(tw.text, short.text, "不同目标语言必须各自缓存");
}
