//! 集成测试（只读）：验证 AlpmWorker 在本机真实数据库上的行为（project.md §11.2）。
//!
//! 需要目标机为 Arch；非 Arch 环境自动跳过（打印提示并返回，不算失败）。

use archstore_core::backend::pacman::PacmanBackend;
use archstore_core::backend::{AlpmOp, AlpmPayload, AlpmWorker, PackageBackend, SearchScope};
use archstore_core::env;

fn pacman_available() -> bool {
    std::path::Path::new("/var/lib/pacman/local").is_dir()
}

#[tokio::test]
async fn worker_opens_database_and_answers_installed() {
    if !pacman_available() {
        eprintln!("跳过：本机没有 /var/lib/pacman/local");
        return;
    }
    let worker = AlpmWorker::spawn().expect("spawn worker");
    let statuses = worker.wait_ready().await.expect("ready");
    assert!(!statuses.is_empty(), "至少应解析出一个同步库名");
    for s in &statuses {
        eprintln!(
            "repo {}: available={} packages={} reason={:?}",
            s.name, s.available, s.packages, s.reason
        );
    }

    let payload = worker.request(AlpmOp::Installed).await.expect("installed");
    let AlpmPayload::Summaries(list) = payload else {
        panic!("unexpected payload");
    };
    assert!(
        list.len() > 100,
        "本机应有数百个已安装包，实际 {}",
        list.len()
    );
    // 已安装列表必须全部标记为已安装，并带上版本
    for s in list.iter().take(50) {
        assert!(s.is_installed(), "{} 应标记为已安装", s.id.name);
        assert!(s.version.is_some(), "{} 应带版本", s.id.name);
    }
    // 排序稳定
    let mut sorted = list.clone();
    sorted.sort_by(|a, b| a.id.name.cmp(&b.id.name));
    assert_eq!(sorted, list, "已安装列表应按名称排序");
}

#[tokio::test]
async fn worker_search_and_info() {
    if !pacman_available() {
        return;
    }
    let worker = AlpmWorker::spawn().expect("spawn");
    worker.wait_ready().await.expect("ready");

    let payload = worker
        .request(AlpmOp::Search {
            query: "firefox".to_string(),
            repos: Vec::new(),
            limit: 50,
        })
        .await
        .expect("search");
    let AlpmPayload::Summaries(hits) = payload else {
        panic!("unexpected payload");
    };
    assert!(!hits.is_empty(), "在官方仓库中应能搜到 firefox");
    assert!(
        hits.iter().any(|s| s.id.name == "firefox"),
        "应包含精确名 firefox，实际 {:?}",
        hits.iter().map(|s| &s.id.name).collect::<Vec<_>>()
    );
    assert!(
        hits[0].id.source.repo_name().is_some(),
        "搜索结果必须带仓库名"
    );

    // 精确 Info
    let detail = worker
        .request(AlpmOp::Info {
            name: "firefox".to_string(),
        })
        .await
        .expect("info");
    let AlpmPayload::Detail(d) = detail else {
        panic!("unexpected payload");
    };
    assert_eq!(d.summary.id.name, "firefox");
    assert!(!d.licenses.is_empty());
    assert!(d.download_size.unwrap_or(0) > 0);
    assert!(d.installed_size.unwrap_or(0) > 0);
    assert!(!d.dependencies.is_empty(), "firefox 应有依赖");
    assert!(
        d.dependencies
            .iter()
            .any(|x| x.kind == archstore_core::model::DepKind::Runtime),
        "应至少有运行时依赖"
    );
    // 不存在的包必须报 NotFound，而不是 panic
    let err = worker
        .request(AlpmOp::Info {
            name: "definitely-not-a-real-package-xyz".to_string(),
        })
        .await
        .expect_err("must be NotFound");
    assert!(matches!(err, archstore_core::CoreError::NotFound(_)));
}

#[tokio::test]
async fn worker_groups_and_upgradable() {
    if !pacman_available() {
        return;
    }
    let worker = AlpmWorker::spawn().expect("spawn");
    let statuses = worker.wait_ready().await.expect("ready");
    let has_extra = statuses.iter().any(|s| s.available && s.name == "extra");

    let payload = worker.request(AlpmOp::Groups).await.expect("groups");
    let AlpmPayload::Groups(groups) = payload else {
        panic!("unexpected payload");
    };
    if has_extra {
        assert!(
            groups.len() > 10,
            "extra 实测有 107 个包组，实际 {}",
            groups.len()
        );
        assert!(groups.iter().all(|g| g.source_kind == "pacman"));
        // 分类 id 形如 "extra:gnome"
        let sample = groups
            .iter()
            .find(|g| g.id.starts_with("extra:"))
            .expect("应存在 extra 的包组");
        let members = worker
            .request(AlpmOp::GroupMembers {
                group: sample.id.clone(),
            })
            .await
            .expect("group members");
        let AlpmPayload::Summaries(ms) = members else {
            panic!("unexpected payload");
        };
        assert!(!ms.is_empty(), "包组 {} 应有成员", sample.id);
    }

    // 可更新列表不应 panic；每一项都必须带 update 信息
    let payload = worker
        .request(AlpmOp::Upgradable)
        .await
        .expect("upgradable");
    let AlpmPayload::Summaries(up) = payload else {
        panic!("unexpected payload");
    };
    for s in &up {
        assert!(s.has_update(), "{} 缺少 update 信息", s.id.name);
        assert!(s.is_installed());
        let u = s.update.as_ref().expect("update");
        assert_ne!(u.current, u.candidate, "版本相同不应出现在可更新列表");
    }
    eprintln!("可更新包数量：{}", up.len());
}

#[tokio::test]
async fn reverse_dependencies_match_measured_values() {
    if !pacman_available() {
        return;
    }
    let worker = AlpmWorker::spawn().expect("spawn");
    worker.wait_ready().await.expect("ready");
    let payload = worker
        .request(AlpmOp::RevDeps {
            name: "glibc".to_string(),
        })
        .await
        .expect("revdeps");
    let AlpmPayload::RevDeps(deps) = payload else {
        panic!("unexpected payload");
    };
    // project.md 实测：glibc -> 705 个反向依赖（随系统更新会变化，只做量级断言）
    assert!(
        deps.len() > 100,
        "glibc 的反向依赖应有数百个，实际 {}",
        deps.len()
    );
}

#[tokio::test]
async fn refresh_reopens_handle_without_losing_repos() {
    if !pacman_available() {
        return;
    }
    let worker = AlpmWorker::spawn().expect("spawn");
    let before = worker.wait_ready().await.expect("ready");
    let payload = worker.request(AlpmOp::Refresh).await.expect("refresh");
    let AlpmPayload::Repos(after) = payload else {
        panic!("unexpected payload");
    };
    assert_eq!(before.len(), after.len(), "刷新后仓库数量应一致");
    assert_eq!(
        before.iter().filter(|r| r.available).count(),
        after.iter().filter(|r| r.available).count()
    );
}

#[tokio::test]
async fn pacman_backend_trait_roundtrip() {
    if !pacman_available() {
        return;
    }
    let backend = PacmanBackend::spawn().await.expect("backend");
    assert_eq!(backend.source_kind(), "pacman");

    let local = backend
        .search("firefox", SearchScope::LocalOnly)
        .await
        .expect("local search");
    // 本地搜索只查已安装包
    for s in &local {
        assert!(s.is_installed(), "LocalOnly 搜索不应返回未安装的包");
    }

    let installed = backend.installed().await.expect("installed");
    assert!(installed.len() > 100);
    let upgradable = backend.upgradable().await.expect("upgradable");
    eprintln!(
        "installed={} upgradable={}",
        installed.len(),
        upgradable.len()
    );

    let cats = backend.categories().await.expect("categories");
    if !cats.is_empty() {
        let first = backend
            .list_category(&cats[0].id, archstore_core::backend::Page::first(5))
            .await
            .expect("list_category");
        assert!(first.len() <= 5, "分页上限必须生效");
    }

    let id = archstore_core::model::PackageId::official("extra", "firefox");
    let detail = backend.info(&id).await.expect("info");
    assert_eq!(detail.summary.id.name, "firefox");
    let deps = backend.dependencies(&id).await.expect("deps");
    assert!(!deps.is_empty());
    // 反向依赖只对"已安装"的包有意义：firefox 未必已安装，改用必定存在的 glibc
    let glibc = archstore_core::model::PackageId::official("core", "glibc");
    let rev = backend.reverse_dependencies(&glibc).await.expect("revdeps");
    assert!(
        rev.len() > 100,
        "glibc 的反向依赖应有数百个，实际 {}",
        rev.len()
    );
    eprintln!("glibc 反向依赖：{}", rev.len());
}

#[test]
fn probe_readonly_reports_local_database() {
    if !pacman_available() {
        return;
    }
    let p = archstore_core::backend::pacman_worker::probe_readonly().expect("probe");
    assert!(p.local > 100, "本地包数量异常：{}", p.local);
    assert!(p.explicit > 0 && p.explicit <= p.local);
    // 实测 /var/lib/pacman 对普通用户可读
    assert!(
        !p.repos.is_empty(),
        "sync 目录中应有 .db 文件（{}）",
        env::discover_sync_repos().join(",")
    );
    eprintln!(
        "local={} explicit={} repos={:?}",
        p.local, p.explicit, p.repos
    );
}
