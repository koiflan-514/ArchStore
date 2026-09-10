//! 性能回归测试（project.md §12 各阶段的退出标准）。
//!
//! 这些断言是**回归护栏**：数值按本机实测留出余量，用于捕捉数量级退化
//! （例如"把 alpm 查询挪回主线程"或"缓存未命中导致重复遍历整库"）。
//!
//! 运行：cargo test -p archstore-core --test perf -- --nocapture

use std::time::{Duration, Instant};

use archstore_core::backend::{AlpmOp, AlpmPayload, AlpmWorker, PackageBackend, Page, SearchScope};
use archstore_core::cache::Cache;
use archstore_core::config::Config;
use archstore_core::model::{PackageId, PackageSummary};

fn pacman_available() -> bool {
    std::path::Path::new("/var/lib/pacman/local").is_dir()
}

#[tokio::test]
async fn alpm_worker_meets_latency_budgets() {
    if !pacman_available() {
        eprintln!("跳过：本机没有 /var/lib/pacman/local");
        return;
    }
    eprintln!("=== alpm 工作线程冷启动与查询延迟 ===");

    // 1) 冷启动：打开句柄 + 注册全部同步库（含 extra 的 14955 个包）
    let start = Instant::now();
    let worker = AlpmWorker::spawn().expect("spawn");
    let statuses = worker.wait_ready().await.expect("ready");
    let ready = start.elapsed();
    eprintln!(
        "  {:<44} {ready:>10.1?}",
        "worker 冷启动（打开句柄 + 注册同步库）"
    );
    let total: usize = statuses.iter().map(|s| s.packages).sum();
    eprintln!("  同步库包总数：{total}（{} 个库）", statuses.len());

    // 2) 已安装列表（构建本地索引，972 个包）
    let (installed, t_installed) = {
        let start = Instant::now();
        let payload = worker.request(AlpmOp::Installed).await.expect("installed");
        let elapsed = start.elapsed();
        let AlpmPayload::Summaries(v) = payload else {
            panic!("unexpected payload")
        };
        eprintln!(
            "  {:<44} {elapsed:>10.1?}",
            format!("Installed（{} 个包）", v.len())
        );
        (v, elapsed)
    };
    assert!(installed.len() > 100);

    // 3) 已安装列表（第二次：本地索引已建好，应明显更快）
    let start = Instant::now();
    let _ = worker.request(AlpmOp::Installed).await.expect("installed2");
    let t_installed_warm = start.elapsed();
    eprintln!("  {:<44} {t_installed_warm:>10.1?}", "Installed（热缓存）");

    // 4) 可更新列表（本地库 vs 同步库逐个 vercmp）
    let start = Instant::now();
    let _ = worker
        .request(AlpmOp::Upgradable)
        .await
        .expect("upgradable");
    let t_upgradable = start.elapsed();
    eprintln!("  {:<44} {t_upgradable:>10.1?}", "Upgradable");

    // 5) 库内搜索（db.search，走 libalpm 的索引）
    let start = Instant::now();
    let _ = worker
        .request(AlpmOp::Search {
            query: "firefox".into(),
            repos: Vec::new(),
            limit: 200,
        })
        .await
        .expect("search");
    let t_search = start.elapsed();
    eprintln!("  {:<44} {t_search:>10.1?}", "Search(extra+core, firefox)");

    // 6) 包组（107 个组 + 成员统计）
    let start = Instant::now();
    let payload = worker.request(AlpmOp::Groups).await.expect("groups");
    let t_groups = start.elapsed();
    let AlpmPayload::Groups(groups) = payload else {
        panic!("unexpected payload")
    };
    eprintln!(
        "  {:<44} {t_groups:>10.1?}",
        format!("Groups（{} 个包组）", groups.len())
    );

    // 7) 详情（含 check_deps 依赖自检）
    let start = Instant::now();
    let _ = worker
        .request(AlpmOp::Info {
            name: "firefox".into(),
        })
        .await
        .expect("info");
    let t_info = start.elapsed();
    eprintln!("  {:<44} {t_info:>10.1?}", "Info(firefox)");

    // 8) 反向依赖（glibc，实测数百条）
    let start = Instant::now();
    let payload = worker
        .request(AlpmOp::RevDeps {
            name: "glibc".into(),
        })
        .await
        .expect("revdeps");
    let t_revdeps = start.elapsed();
    let AlpmPayload::RevDeps(rev) = payload else {
        panic!("unexpected payload")
    };
    eprintln!(
        "  {:<44} {t_revdeps:>10.1?}",
        format!("RevDeps(glibc)（{} 条）", rev.len())
    );

    // --- 断言（回归护栏，非精确基准）---
    // §12 阶段 1：extra 首次遍历 < 300 ms。冷启动还包含打开句柄与注册，
    // 因此这里给到 3 秒，真正捕捉的是"数量级退化"。
    assert!(
        ready < Duration::from_secs(3),
        "worker 冷启动过慢：{ready:?}（预期 < 3s）"
    );
    // §12 阶段 1：主线程不出现 > 50 ms 阻塞。查询本身必须远低于这个量级，
    // 否则 UI 线程即使只做投递也会被拖累。
    for (label, t) in [
        ("Installed", t_installed),
        ("Upgradable", t_upgradable),
        ("Search", t_search),
        ("Groups", t_groups),
        ("Info", t_info),
        ("RevDeps", t_revdeps),
    ] {
        assert!(
            t < Duration::from_millis(500),
            "{label} 查询过慢：{t:?}（预期 < 500ms）—— 检查是否退化为每次全库遍历"
        );
    }
    // 本地索引生效：第二次 Installed 不应比第一次更慢
    assert!(
        t_installed_warm <= t_installed + Duration::from_millis(50),
        "热缓存反而更慢（{t_installed_warm:?} vs {t_installed:?}）—— 本地索引可能没生效"
    );
    eprintln!("  ✓ 全部查询均在预算内");
}

#[tokio::test]
async fn cache_lookup_is_effectively_free() {
    let dir = tempfile::tempdir().expect("tmpdir");
    let cache = Cache::open(dir.path().join("cache"), 64 * 1024 * 1024).expect("cache");

    // 写入 2000 条条目，测 LRU 索引与磁盘写入成本
    let start = Instant::now();
    for i in 0..2000 {
        cache
            .put(
                "perf",
                &format!("k{i}"),
                &format!("value-{i}"),
                Duration::from_secs(300),
            )
            .await
            .expect("put");
    }
    let write = start.elapsed();
    eprintln!("  写入 2000 条缓存：{write:?}");

    // 读取 2000 条（应命中内存索引 + 小文件读）
    let start = Instant::now();
    let mut hits = 0usize;
    for i in 0..2000 {
        if cache
            .get::<String>("perf", &format!("k{i}"), false)
            .await
            .is_some()
        {
            hits += 1;
        }
    }
    let read = start.elapsed();
    eprintln!("  读取 2000 条缓存：{read:?}（命中 {hits}）");
    assert_eq!(hits, 2000);

    // 单次读取必须远低于一帧（16.7ms）
    let start = Instant::now();
    let _ = cache.get::<String>("perf", "k0", false).await;
    let single = start.elapsed();
    eprintln!("  单次缓存读取：{single:?}");
    assert!(
        single < Duration::from_millis(5),
        "单次缓存读取过慢：{single:?}"
    );
    assert!(write < Duration::from_secs(20), "缓存写入过慢：{write:?}");
}

#[tokio::test]
async fn list_scale_operations_stay_bounded() {
    // 用合成的千级数据测量"UI 线程上会执行"的操作。
    // 合成数据规模可控、结果可复现，且不依赖本机包库的形状
    // （实测 extra 的包组都很小，凑不出千级规模）。
    eprintln!("=== 千级列表操作（合成 5000 条）===");
    const N: usize = 5000;
    let make = |i: usize| {
        let mut s = PackageSummary::minimal(
            PackageId::official(
                if i.is_multiple_of(3) { "extra" } else { "core" },
                format!("package-name-{i:05}-with-some-length"),
            ),
            format!("Package {i}"),
        );
        s.set_summary(&format!("Description for package {i}: {}", "x".repeat(90)));
        s.version = Some(format!("1.{}.{}-1", i % 50, i % 10));
        s.popularity = Some((i % 100) as f64 / 3.0);
        s.votes = Some((i % 500) as u32);
        s
    };
    let mut items: Vec<PackageSummary> = (0..N).map(make).collect();
    let bytes: usize = items
        .iter()
        .map(|s| s.id.name.len() + s.display_name.len() + s.summary.len() + 64)
        .sum();
    eprintln!(
        "  合成 {N} 条，估算占用 {}",
        archstore_core::model::human_size(bytes as u64)
    );

    // 1) 排序（无查询：分类页/推荐页的路径）
    let start = Instant::now();
    archstore_core::backend::sort_summaries("", &mut items);
    let sort_plain = start.elapsed();
    eprintln!("  排序 {N} 条（无查询）：{sort_plain:?}");
    assert!(
        sort_plain < Duration::from_millis(300),
        "排序过慢：{sort_plain:?}—— UI 线程会掉帧"
    );

    // 2) 排序（带查询：走完整比较器）
    let start = Instant::now();
    archstore_core::backend::sort_summaries("package-name-00042", &mut items);
    let sort_query = start.elapsed();
    eprintln!("  排序 {N} 条（带查询）：{sort_query:?}");
    assert!(
        sort_query < Duration::from_millis(300),
        "排序过慢：{sort_query:?}"
    );

    // 3) 合并去重（搜索路径：三个后端的结果合并）
    let batches: Vec<Vec<PackageSummary>> = (0..3)
        .map(|b| (0..N / 3).map(|i| make(i * 3 + b)).collect::<Vec<_>>())
        .collect();
    let total: usize = batches.iter().map(|b| b.len()).sum();
    let start = Instant::now();
    let merged = archstore_core::backend::merge_results("package", batches);
    let merge = start.elapsed();
    eprintln!(
        "  合并去重 {total} 条（输入）-> {} 条：{merge:?}",
        merged.len()
    );
    assert_eq!(merged.len(), total, "合成数据没有重复项");
    // 绝对预算是宽松护栏（debug 构建比 release 慢约一个数量级）
    assert!(merge < Duration::from_millis(400), "合并过慢：{merge:?}");

    // 真正的护栏是**复杂度**：规模翻倍时，O(n log n) 约 2.1 倍、O(n²) 约 4 倍。
    // 这样断言不受 debug/release 差异影响。
    //
    // 测量方法（微基准的常规做法，用来压住噪声）：
    //   - 先预热一次（首次分配会向内核要页，明显偏慢）
    //   - 取 3 次中的最小值（CPU 频率/调度抖动只会让某次更慢）
    // 规模选在毫秒级：太小时固定开销会让比值失真。
    let time_merge = |n: usize| -> (usize, Duration) {
        let batches: Vec<Vec<PackageSummary>> = (0..3)
            .map(|b| (0..n / 3).map(|i| make(i * 3 + b)).collect::<Vec<_>>())
            .collect();
        // 预热
        let _ = archstore_core::backend::merge_results("package", batches.clone());
        let mut best = Duration::MAX;
        let mut len = 0usize;
        for _ in 0..3 {
            let input = batches.clone();
            let start = Instant::now();
            len = archstore_core::backend::merge_results("package", input).len();
            best = best.min(start.elapsed());
        }
        (len, best)
    };
    const SMALL: usize = 4000;
    const LARGE: usize = 8000;
    let (n1, t1) = time_merge(SMALL);
    let (n2, t2) = time_merge(LARGE);
    // 3 个后端各返回 n/3 条互不重复的条目（整数除法会少 1~2 条）
    assert!(
        (n1 as f64) > SMALL as f64 * 0.98 && (n2 as f64) > LARGE as f64 * 0.98,
        "样本规模不符：{n1} / {n2}"
    );
    let ratio = t2.as_secs_f64() / t1.as_secs_f64().max(f64::EPSILON);
    eprintln!(
        "  复杂度护栏：{SMALL} 条 {t1:?} -> {LARGE} 条 {t2:?}（{ratio:.2}×，O(n²) 会是 ~4×）"
    );
    // 阈值取值依据（实测 + 理论）：
    //   理论上 O(n log n) 翻倍输入 = 2.17×；实测 debug 2.28×、release 2.53×
    //   （HashSet 扩容与排序的缓存行为会略高于理论值）
    //   O(n²) 则稳定在 4.0×（旧实现 5000 条 239ms → 4000/8000 约 153ms/612ms）
    // 取 3.2：对正常路径留 ~26% 余量，同时与 O(n²) 的 4.0× 保持 ~20% 间距。
    assert!(
        ratio < 3.2,
        "合并耗时随规模超线性增长（{t1:?} -> {t2:?}，{ratio:.2}×）：去重退化为 O(n²) 了？"
    );
    // 去重必须真的生效：重复输入不能产生重复输出
    let dupes = archstore_core::backend::merge_results(
        "package",
        vec![
            (0..200).map(make).collect(),
            (0..200).map(make).collect(),
            (0..200).map(make).collect(),
        ],
    );
    assert_eq!(dupes.len(), 200, "三个后端返回同一批时必须去重到 200 条");

    // 4) 全序性：结果必须与输入顺序无关
    let a = archstore_core::backend::merge_results("package", vec![(0..500).map(make).collect()]);
    let mut reversed: Vec<PackageSummary> = (0..500).map(make).collect();
    reversed.reverse();
    let b = archstore_core::backend::merge_results("package", vec![reversed]);
    assert_eq!(
        a.iter().map(|s| s.id.name.clone()).collect::<Vec<_>>(),
        b.iter().map(|s| s.id.name.clone()).collect::<Vec<_>>(),
        "排序必须是全序：同分条目按名称兜底"
    );
}

#[tokio::test]
async fn backend_trait_operations_stay_bounded() {
    if !pacman_available() {
        return;
    }
    let backend = archstore_core::backend::PacmanBackend::spawn()
        .await
        .expect("backend");

    let start = Instant::now();
    let _ = backend
        .search("git", SearchScope::LocalOnly)
        .await
        .expect("search");
    let local = start.elapsed();
    eprintln!("  LocalOnly 搜索：{local:?}");
    assert!(
        local < Duration::from_millis(200),
        "LocalOnly 搜索过慢：{local:?}"
    );

    let cats = backend.categories().await.expect("categories");
    let start = Instant::now();
    let page = backend
        .list_category(&cats[0].id, Page::first(50))
        .await
        .expect("page");
    let page_time = start.elapsed();
    eprintln!("  分类首页 {} 条：{page_time:?}", page.len());
    assert!(page_time < Duration::from_millis(500));
    assert!(page.len() <= 50, "分页上限必须生效");

    // 缓存目录大小统计（设置页用）不应阻塞过久
    let start = Instant::now();
    let size = archstore_core::env::dir_size(&archstore_core::config::paths::cache_dir());
    let dir = start.elapsed();
    eprintln!(
        "  缓存目录统计（{}）：{dir:?}",
        archstore_core::model::human_size(size)
    );
    assert!(dir < Duration::from_secs(5));

    let _ = Config::default();
}
