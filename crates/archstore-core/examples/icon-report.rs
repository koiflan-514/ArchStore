//! 图标来源覆盖率报告（开发 / 诊断用）。
//!
//! ```bash
//! cargo run -p archstore-core --release --example icon-report
//! ```
//!
//! 回答两个问题：
//! 1. 每个图标来源各自覆盖多少包（AppStream 数据包 / 已安装包的 .desktop / 主题同名图标）；
//! 2. 还剩多少包只能显示字母头像，以及它们里有没有本该有图标的软件。
//!
//! 判定"主题同名图标"要扫系统图标主题（GTK 之外的路径），这里用文件名近似，
//! 因此该列是**上界**（同名文件不一定真的能被 GTK 主题找到）。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use alpm::{Alpm, SigLevel};

use archstore_core::desktop_icons::{self, DesktopIcons};
use archstore_core::icons::AppstreamIcons;

fn main() {
    let started = Instant::now();

    let repos = archstore_core::env::discover_sync_repos();
    let mut handle = match Alpm::new("/", "/var/lib/pacman") {
        Ok(h) => h,
        Err(e) => {
            eprintln!("无法打开 libalpm：{e}");
            std::process::exit(1);
        }
    };
    for name in &repos {
        let _ = handle.register_syncdb_mut(name.as_str(), SigLevel::USE_DEFAULT);
    }

    // --- 来源 1：AppStream 数据包 ---
    // 消歧需要"这个名字是不是真实包名"的判定。实测：拿 libalpm 逐个查（线性扫描）
    // 比先建一个 15252 个名字的 HashSet 慢一个数量级，所以两种都测一遍留作对照。
    let t = Instant::now();
    let _naive = AppstreamIcons::load();
    let naive_ms = t.elapsed().as_millis();

    let names: HashSet<String> = handle
        .syncdbs()
        .iter()
        .flat_map(|db| db.pkgs().iter().map(|p| p.name().to_string()))
        .chain(handle.localdb().pkgs().iter().map(|p| p.name().to_string()))
        .collect();
    let t = Instant::now();
    let appstream = AppstreamIcons::load_with(&|name: &str| names.contains(name));
    let appstream_ms = t.elapsed().as_millis();

    // --- 来源 2：已安装包的 .desktop ---
    let t = Instant::now();
    let desktop = build_desktop_icons(&handle);
    let desktop_ms = t.elapsed().as_millis();

    // --- 来源 3：主题里与包名同名的图标（近似：只看文件名）---
    let theme = theme_icon_stems();

    let repo_pkgs: HashSet<String> = handle
        .syncdbs()
        .iter()
        .flat_map(|db| db.pkgs().iter().map(|p| p.name().to_string()))
        .collect();
    let mut installed: Vec<(String, bool)> = Vec::new(); // (包名, 是否外来包)
    for pkg in handle.localdb().pkgs().iter() {
        let foreign = !handle.syncdbs().iter().any(|db| db.pkg(pkg.name()).is_ok());
        installed.push((pkg.name().to_string(), foreign));
    }

    println!("仓库包总数        : {}", repo_pkgs.len());
    println!(
        "AppStream 扫描    : 无判定器 {} ms；HashSet 判定器 {} ms",
        naive_ms, appstream_ms
    );
    println!(
        "AppStream 图标    : {}（{:.1}%）",
        repo_pkgs
            .iter()
            .filter(|n| appstream.lookup(n).is_some())
            .count(),
        100.0
            * repo_pkgs
                .iter()
                .filter(|n| appstream.lookup(n).is_some())
                .count() as f64
            / repo_pkgs.len().max(1) as f64,
    );
    println!(
        "索引规模          : AppStream {} 个包；已安装 .desktop {} 个包（耗时 {} ms）",
        appstream.len(),
        desktop.len(),
        desktop_ms
    );

    let covered = |name: &str| {
        appstream.lookup(name).is_some() || desktop.lookup(name).is_some() || theme.contains(name)
    };
    let installed_with_icon = installed.iter().filter(|(n, _)| covered(n)).count();
    let foreign: Vec<&(String, bool)> = installed.iter().filter(|(_, f)| *f).collect();
    let foreign_with_icon = foreign.iter().filter(|(n, _)| covered(n)).count();
    println!();
    println!("已安装包          : {}", installed.len());
    println!(
        "  有图标          : {}（AppStream {} / .desktop {}）",
        installed_with_icon,
        installed
            .iter()
            .filter(|(n, _)| appstream.lookup(n).is_some())
            .count(),
        installed
            .iter()
            .filter(|(n, _)| desktop.lookup(n).is_some())
            .count()
    );
    println!(
        "  只有字母头像    : {}",
        installed.len() - installed_with_icon
    );
    println!(
        "  外来包（AUR 等）: {}，其中有图标 {}（修复前这些包一律是字母头像）",
        foreign.len(),
        foreign_with_icon
    );

    let mut sample: Vec<&String> = foreign
        .iter()
        .filter(|(n, _)| desktop.lookup(n).is_some())
        .map(|(n, _)| n)
        .collect();
    sample.sort();
    println!();
    println!("已安装 AUR 包拿到图标的样例（前 15 个）：");
    for name in sample.iter().take(15) {
        println!("  {name}");
    }
    println!();
    println!("总耗时 {} ms", started.elapsed().as_millis());
}

/// 复刻 alpm 工作线程里的 .desktop 索引构建（只为报告用）。
fn build_desktop_icons(handle: &Alpm) -> DesktopIcons {
    let mut map = HashMap::new();
    for pkg in handle.localdb().pkgs().iter() {
        let name = pkg.name();
        let mut candidates: Vec<(u8, PathBuf)> = Vec::new();
        for file in pkg.files().files() {
            let Ok(path) = std::str::from_utf8(file.name()) else {
                continue;
            };
            if let Some(candidate) = desktop_icons::desktop_candidate(path, name) {
                candidates.push(candidate);
            }
        }
        if let Some(icon) = desktop_icons::resolve_candidates(candidates) {
            map.insert(name.to_string(), icon);
        }
    }
    DesktopIcons::from_map(map)
}

/// 系统图标主题里可用的图标名（去后缀）。
fn theme_icon_stems() -> HashSet<String> {
    let mut roots: Vec<PathBuf> = vec![
        PathBuf::from("/usr/share/icons"),
        PathBuf::from("/usr/share/pixmaps"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join(".local/share/icons"));
    }
    let mut out = HashSet::new();
    for root in roots {
        collect_stems(&root, &mut out, 0);
    }
    out
}

fn collect_stems(dir: &std::path::Path, out: &mut HashSet<String>, depth: usize) {
    if depth > 6 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_stems(&path, out, depth + 1);
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(ext.as_str(), "png" | "svg" | "svgz" | "xpm") {
            out.insert(stem.to_string());
        }
    }
}
