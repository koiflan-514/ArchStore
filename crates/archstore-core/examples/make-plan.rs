//! 手动集成测试辅助工具（project.md §11.2 "集成测试（写，手动）"）。
//!
//! 用**真实的计划构建代码**生成计划文件，再交给 helper 校验/执行，
//! 这样端到端验证覆盖的是生产路径，而不是手写的 JSON。
//!
//! 用法：
//!   cargo run --release -p archstore-core --example make-plan -- <输出目录> install <包名> [仓库]
//!   cargo run --release -p archstore-core --example make-plan -- <输出目录> remove  <包名> [仓库]
//!   cargo run --release -p archstore-core --example make-plan -- <输出目录> remove-cascade <包名> [仓库]
//!   cargo run --release -p archstore-core --example make-plan -- <输出目录> flatpak-install <应用 ID> [remote]
//!
//! stdout 只输出计划文件的绝对路径，便于脚本捕获：
//!   PLAN=$(cargo run --release -q -p archstore-core --example make-plan -- ~/.cache/archstore/plans install sl extra | tail -1)
//!   pkexec /usr/lib/archstore/archstore-helper --plan "$PLAN" --kind pacman-sync
//!
//! 目录权限 0700、文件权限 0600，与 GUI 写盘完全一致。

use std::path::PathBuf;
use std::sync::Arc;

use archstore_core::backend::{FlatpakBackend, PackageBackend, PacmanBackend};
use archstore_core::cache::Cache;
use archstore_core::config::{Config, paths};
use archstore_core::model::PackageId;
use archstore_core::net::HttpClient;
use archstore_core::plan::InstallOptions;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!(
            "用法：make-plan <输出目录> <install|remove|flatpak-install> <名称> [仓库/remote]"
        );
        return std::process::ExitCode::from(2);
    }
    let out_dir = PathBuf::from(&args[0]);
    let action = args[1].clone();
    let name = args[2].clone();
    let third = args.get(3).cloned().unwrap_or_else(|| "extra".to_string());

    let backend: Arc<dyn PackageBackend>;
    let id: PackageId;
    match action.as_str() {
        "flatpak-install" => {
            let cfg = Config::load_default().config.sanitized();
            let cache = match Cache::open(
                paths::cache_dir(),
                cfg.cache.max_size_mb.saturating_mul(1024 * 1024),
            ) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("无法打开缓存：{}", e.user_message());
                    return std::process::ExitCode::from(1);
                }
            };
            let http = match HttpClient::new(cfg.network.clone()) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("无法创建网络客户端：{}", e.user_message());
                    return std::process::ExitCode::from(1);
                }
            };
            let flatpak = FlatpakBackend::new(http, cache, &cfg).await;
            if !flatpak.capability().is_available() {
                eprintln!(
                    "Flatpak 后端不可用：{}",
                    flatpak.capability().reason.clone().unwrap_or_default()
                );
                return std::process::ExitCode::from(1);
            }
            let remote = if args.len() > 3 {
                third.clone()
            } else {
                cfg.sources.flatpak_remote.clone()
            };
            id = PackageId::flatpak(remote, name.clone());
            backend = flatpak;
        }
        _ => {
            let pacman: Arc<dyn PackageBackend> = match PacmanBackend::spawn().await {
                Ok(b) => Arc::new(b),
                Err(e) => {
                    eprintln!("无法打开 pacman 后端：{}", e.user_message());
                    return std::process::ExitCode::from(1);
                }
            };
            id = PackageId::official(third.clone(), name.clone());
            backend = pacman;
        }
    }

    let backends = [backend];
    let result = match action.as_str() {
        "install" | "flatpak-install" => {
            archstore_core::plan::build_install_plan(
                &backends,
                std::slice::from_ref(&id),
                &InstallOptions::default(),
            )
            .await
        }
        "remove" => {
            archstore_core::plan::build_remove_plan(&backends, std::slice::from_ref(&id), false)
                .await
        }
        // 级联卸载（用户在反依赖对话框里点了"同时删除这些包"）：
        // 计划里必须同时列出连带删除的包与不再需要的依赖（§9.3）。
        "remove-cascade" => {
            archstore_core::plan::build_remove_plan(&backends, std::slice::from_ref(&id), true)
                .await
        }
        other => {
            eprintln!(
                "未知动作：{other}（应为 install / remove / remove-cascade / flatpak-install）"
            );
            return std::process::ExitCode::from(2);
        }
    };

    let outcome = match result {
        Ok(o) => o,
        Err(e) => {
            eprintln!("计划构建失败：{}", e.user_message());
            if let archstore_core::CoreError::ReverseDeps {
                dependents, count, ..
            } = &e
            {
                eprintln!(
                    "  （有 {count} 个反向依赖，前 5 个：{:?}）",
                    dependents.iter().take(5).collect::<Vec<_>>()
                );
            }
            return std::process::ExitCode::from(1);
        }
    };

    if outcome.plans.is_empty() {
        eprintln!("没有需要提权的计划（AUR 请求：{}）", outcome.aur.len());
        return std::process::ExitCode::from(1);
    }

    let plan = &outcome.plans[0];
    if let Err(e) = plan.validate() {
        eprintln!("计划校验失败：{}", e.user_message());
        return std::process::ExitCode::from(1);
    }
    let file_name = format!(
        "{}-{}-{}.json",
        plan.kind.as_str(),
        name,
        archstore_core::model::plan::now_unix()
    );
    match plan.write_to_dir(&out_dir, &file_name) {
        Ok(path) => {
            eprintln!(
                "已写出计划：kind={} 条目={} 摘要={:?}",
                plan.kind.as_str(),
                plan.len(),
                plan.summary
            );
            println!("{}", path.display());
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("写盘失败：{}", e.user_message());
            std::process::ExitCode::from(1)
        }
    }
}
