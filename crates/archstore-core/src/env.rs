//! 环境探测：Capability 汇总，供 --doctor 与设置页使用（project.md §2.3 / §4.3）。
//!
//! 自检必须是只读的：不得注册同步库之外的动作、不得写 /var/lib/pacman、不得触发下载。

use std::path::{Path, PathBuf};

use crate::config::{AurHelperChoice, paths};
use crate::error::{CoreError, CoreResult};

/// gtk4 最低运行时版本（与 gtk4 = 0.11 + v4_18 feature 对应）。
pub const MIN_GTK: (u32, u32) = (4, 18);
/// libadwaita 最低运行时版本（与 libadwaita = 0.9 + v1_8 feature 对应）。
pub const MIN_ADW: (u32, u32) = (1, 8);

/// 自检结果的严重级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

impl Level {
    /// 与 --doctor 输出中的标记一致。
    pub fn marker(&self) -> &'static str {
        match self {
            Level::Ok => " OK ",
            Level::Warn => "WARN",
            Level::Fail => "FAIL",
        }
    }

    pub fn is_failure(&self) -> bool {
        matches!(self, Level::Fail)
    }
}

/// 单条自检项。
#[derive(Debug, Clone)]
pub struct Check {
    pub level: Level,
    /// 检查项名称（如 "发行版"）
    pub title: String,
    /// 结论与关键数据
    pub detail: String,
}

/// 自检报告。
#[derive(Debug, Clone, Default)]
pub struct DoctorReport {
    pub checks: Vec<Check>,
}

impl DoctorReport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, level: Level, title: impl Into<String>, detail: impl Into<String>) {
        self.checks.push(Check {
            level,
            title: title.into(),
            detail: detail.into(),
        });
    }

    pub fn ok(&mut self, title: impl Into<String>, detail: impl Into<String>) {
        self.push(Level::Ok, title, detail);
    }

    pub fn warn(&mut self, title: impl Into<String>, detail: impl Into<String>) {
        self.push(Level::Warn, title, detail);
    }

    pub fn fail(&mut self, title: impl Into<String>, detail: impl Into<String>) {
        self.push(Level::Fail, title, detail);
    }

    /// 退出码 = 失败项数量（0 表示全部就绪）。
    pub fn exit_code(&self) -> i32 {
        self.checks.iter().filter(|c| c.level.is_failure()).count() as i32
    }

    pub fn has_failures(&self) -> bool {
        self.checks.iter().any(|c| c.level.is_failure())
    }

    pub fn failure_count(&self) -> usize {
        self.checks.iter().filter(|c| c.level.is_failure()).count()
    }

    pub fn warn_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.level == Level::Warn)
            .count()
    }

    /// 渲染为 --doctor 的文本输出。
    pub fn render(&self) -> String {
        let mut out = String::new();
        for c in &self.checks {
            out.push_str(&format!(
                "[{}] {}：{}\n",
                c.level.marker(),
                c.title,
                c.detail
            ));
        }
        out
    }

    /// 渲染为 JSON（供 GUI 的诊断窗口复用同一份数据）。
    pub fn to_json(&self) -> String {
        let items: Vec<serde_json::Value> = self
            .checks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "level": c.level.marker().trim(),
                    "title": c.title,
                    "detail": c.detail,
                })
            })
            .collect();
        serde_json::json!({
            "failures": self.failure_count(),
            "warnings": self.warn_count(),
            "checks": items,
        })
        .to_string()
    }
}

/// AUR 助手种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurHelperKind {
    Paru,
    Yay,
}

impl AurHelperKind {
    pub fn binary(&self) -> &'static str {
        match self {
            AurHelperKind::Paru => "paru",
            AurHelperKind::Yay => "yay",
        }
    }

    pub fn display(&self) -> &'static str {
        match self {
            AurHelperKind::Paru => "paru",
            AurHelperKind::Yay => "yay",
        }
    }
}

/// 在 PATH 中查找可执行文件。
pub fn which(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let p = PathBuf::from(bin);
        return is_executable(&p).then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// 按检测顺序（paru -> yay）选择 AUR 助手，并尊重用户的显式选择。
pub fn find_aur_helper(choice: AurHelperChoice) -> Option<AurHelperKind> {
    let has = |k: AurHelperKind| which(k.binary()).is_some();
    match choice {
        AurHelperChoice::None => None,
        AurHelperChoice::Paru => has(AurHelperKind::Paru).then_some(AurHelperKind::Paru),
        AurHelperChoice::Yay => has(AurHelperKind::Yay).then_some(AurHelperKind::Yay),
        AurHelperChoice::Auto => {
            if has(AurHelperKind::Paru) {
                Some(AurHelperKind::Paru)
            } else if has(AurHelperKind::Yay) {
                Some(AurHelperKind::Yay)
            } else {
                None
            }
        }
    }
}

/// 判断是否为 Arch 及其衍生发行版。
pub fn detect_distro() -> (String, bool) {
    let text = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let mut name = String::from("未知系统");
    let mut is_arch = false;
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim().trim_matches('"');
        match k.trim() {
            "PRETTY_NAME" | "NAME" => {
                if name == "未知系统" || k.trim() == "PRETTY_NAME" {
                    name = v.to_string();
                }
            }
            "ID" => {
                if v == "arch" || v == "archarm" {
                    is_arch = true;
                }
            }
            "ID_LIKE" if v.split_whitespace().any(|x| x == "arch") => {
                is_arch = true;
            }
            _ => {}
        }
    }
    // 兜底：存在 pacman 与 libalpm 亦视作可用
    if !is_arch && which("pacman").is_some() {
        is_arch = true;
    }
    (name, is_arch)
}

/// 从 pacman.conf 文本中解析同步库名（[section] 且非 options）。
///
/// 不硬编码 core/extra/multilib：本机 multilib 实测为 0 包（未启用）即为反例。
/// 库名必须原样保留大小写，不做事后归一（register_syncdb_mut("Extra") 同样返回 Ok）。
pub fn parse_pacman_conf_repos(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix('[')
            && let Some(name) = rest.strip_suffix(']')
        {
            let name = name.trim();
            if name.is_empty() || name.eq_ignore_ascii_case("options") {
                continue;
            }
            if !out.iter().any(|x: &String| x == name) {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// 探测 /var/lib/pacman/sync/<name>.db 是否存在且非空。
pub fn sync_db_file_ready(db_path: &Path, name: &str) -> bool {
    let f = db_path.join(format!("{name}.db"));
    std::fs::metadata(&f).map(|m| m.len() > 0).unwrap_or(false)
}

/// 解析同步库名；解析失败时降级为探测 /var/lib/pacman/sync/*.db 的文件名。
pub fn discover_sync_repos() -> Vec<String> {
    let conf = std::fs::read_to_string("/etc/pacman.conf").unwrap_or_default();
    let mut repos = parse_pacman_conf_repos(&conf);
    if repos.is_empty() {
        repos = discover_sync_repos_from_dir(Path::new("/var/lib/pacman/sync"));
    }
    repos
}

/// 从 sync 目录中的 *.db 文件名推断仓库名。
pub fn discover_sync_repos_from_dir(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(stem) = name.strip_suffix(".db")
            && !stem.is_empty()
            && !out.iter().any(|x: &String| x == stem)
        {
            out.push(stem.to_string());
        }
    }
    out.sort();
    out
}

/// 环境能力汇总（设置页置灰与"为什么不能用"提示的数据源）。
#[derive(Debug, Clone)]
pub struct Capabilities {
    pub distro: String,
    pub pacman: crate::backend::Capability,
    pub aur: crate::backend::Capability,
    pub flatpak: crate::backend::Capability,
    /// 已检测到的 AUR 助手
    pub aur_helper: Option<AurHelperKind>,
    /// Flatpak 远程仓库名列表
    pub flatpak_remotes: Vec<String>,
    /// helper 是否已安装及其版本
    pub helper: Option<String>,
}

/// 只读检测 helper 是否就位。
pub fn detect_helper() -> Option<String> {
    let path = paths::helper_path();
    if !is_executable(&path) {
        return None;
    }
    let out = std::process::Command::new(&path)
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// 检测 polkit 提权通道。
pub fn detect_pkexec() -> Option<PathBuf> {
    which("pkexec")
}

/// 检测 flatpak CLI 与远程仓库。
///
/// 注意（实测）：flatpak remotes 没有 installation 列
/// （flatpak remotes --columns=installation 直接报"未知列"），
/// 因此改为分别查询 --system 与 --user 两个安装位置。
pub fn detect_flatpak() -> Option<(String, Vec<String>)> {
    let bin = which("flatpak")?;
    let mut remotes: Vec<String> = Vec::new();
    for flag in ["--system", "--user"] {
        let Ok(out) = std::process::Command::new(&bin)
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .args(["remotes", flag, "--columns=name"])
            .output()
        else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        for name in crate::backend::flatpak::parse_remotes(&String::from_utf8_lossy(&out.stdout)) {
            if !remotes.iter().any(|x| x == &name) {
                remotes.push(name);
            }
        }
    }
    let version = std::process::Command::new(&bin)
        .arg("--version")
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .trim_start_matches("Flatpak ")
                .to_string()
        })
        .unwrap_or_default();
    Some((version, remotes))
}

/// 执行一次完整的只读环境自检。
///
/// gtk/adw 的运行期版本由 GUI 侧通过 gtk::major_version() 等读取后传入
/// （core 不允许依赖 GTK）。传入 None 表示当前不是 GUI 入口。
pub fn probe(
    gtk: Option<(u32, u32, u32)>,
    adw: Option<(u32, u32, u32)>,
    cfg: &crate::config::Config,
) -> DoctorReport {
    let mut r = DoctorReport::new();

    // 1) 发行版
    let (distro, is_arch) = detect_distro();
    if is_arch {
        r.ok("发行版", distro);
    } else {
        r.fail(
            "发行版",
            format!("{distro}（本项目只支持 Arch Linux 及其衍生发行版）"),
        );
    }

    // 2) gtk4 运行时
    match gtk {
        Some((maj, min, mic)) => {
            let text = format!("{maj}.{min}.{mic}（要求 >= {}.{}）", MIN_GTK.0, MIN_GTK.1);
            if (maj, min) >= MIN_GTK {
                r.ok("gtk4 运行时", text);
            } else {
                r.fail("gtk4 运行时", format!("{text}；请升级系统包 gtk4"));
            }
        }
        None => r.warn("gtk4 运行时", "未在图形入口中检测（--doctor 从 CLI 运行）"),
    }

    // 3) libadwaita 运行时
    match adw {
        Some((maj, min, mic)) => {
            let text = format!("{maj}.{min}.{mic}（要求 >= {}.{}）", MIN_ADW.0, MIN_ADW.1);
            if (maj, min) >= MIN_ADW {
                r.ok("libadwaita 运行时", text);
            } else {
                r.fail(
                    "libadwaita 运行时",
                    format!("{text}；请升级系统包 libadwaita"),
                );
            }
        }
        None => r.warn(
            "libadwaita 运行时",
            "未在图形入口中检测（--doctor 从 CLI 运行）",
        ),
    }

    // 4) libalpm 只读句柄（结果在第 9 项里复用：图标来源统计）
    let probe = crate::backend::pacman_worker::probe_readonly();
    match &probe {
        Ok(p) => {
            let ready: Vec<String> = p
                .repos
                .iter()
                .filter(|x| x.available)
                .map(|x| format!("{}（{} 包）", x.name, x.packages))
                .collect();
            let empty: Vec<String> = p
                .repos
                .iter()
                .filter(|x| !x.available)
                .map(|x| x.name.clone())
                .collect();
            if ready.is_empty() {
                r.fail(
                    "libalpm 可打开",
                    format!(
                        "local {} 包，但没有任何可用的同步库（未同步？请运行 sudo pacman -Sy）",
                        p.local
                    ),
                );
            } else if empty.is_empty() {
                r.ok(
                    "libalpm 可打开（非 root，只读）",
                    format!(
                        "local {} 包（其中显式 {}），sync {}",
                        p.local,
                        p.explicit,
                        ready.join("、")
                    ),
                );
            } else {
                r.warn(
                    "libalpm 可打开（非 root，只读）",
                    format!(
                        "local {} 包，可用同步库 {}；未同步：{}（请运行 sudo pacman -Sy）",
                        p.local,
                        ready.join("、"),
                        empty.join("、")
                    ),
                );
            }
        }
        Err(e) => r.fail("libalpm 可打开", e.user_message()),
    }

    // 5) polkit
    match detect_pkexec() {
        Some(p) => r.ok("polkit 可用", p.display().to_string()),
        None => r.fail(
            "polkit 可用",
            "未找到 pkexec；安装、更新与卸载功能将不可用（请安装 polkit 包）",
        ),
    }

    // 6) helper
    match detect_helper() {
        Some(v) => {
            let policy_ok = paths::policy_file().exists();
            if policy_ok {
                r.ok(
                    "helper",
                    format!(
                        "{}（版本 {v}，polkit action 已安装）",
                        paths::helper_path().display()
                    ),
                );
            } else {
                r.warn(
                    "helper",
                    format!(
                        "{}（版本 {v}）已安装，但缺少 polkit action {}",
                        paths::helper_path().display(),
                        paths::policy_file().display()
                    ),
                );
            }
        }
        None => r.warn(
            "helper",
            format!(
                "未安装 {}，安装/卸载功能将置灰（重新安装 archstore 或运行 make install）",
                paths::helper_path().display()
            ),
        ),
    }

    // 7) flatpak
    if !cfg.sources.flatpak_enabled {
        r.warn("flatpak", "已在设置中关闭");
    } else {
        match detect_flatpak() {
            Some((version, remotes)) if !remotes.is_empty() => {
                r.ok("flatpak", format!("{version}，远程 {}", remotes.join("/")))
            }
            Some((version, _)) => r.warn(
                "flatpak",
                format!("{version} 已安装，但没有配置任何远程仓库（如 flathub）"),
            ),
            None => r.warn("flatpak", "未安装 flatpak（可选），Flatpak 源将置灰"),
        }
    }

    // 8) AUR 助手
    if !cfg.sources.aur_enabled {
        r.warn("AUR 助手", "已在设置中关闭 AUR 源");
    } else {
        match find_aur_helper(cfg.sources.aur_helper) {
            Some(k) => r.ok(
                "AUR 助手",
                format!(
                    "{}（{}）",
                    k.display(),
                    which(k.binary())
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                ),
            ),
            None => r.warn(
                "AUR 助手",
                "未检测到 paru/yay，AUR 安装功能将置灰（查询不受影响）",
            ),
        }
    }

    // 9) 软件图标来源（"很多软件没有图标"的两个本地来源）
    //
    // AppStream 数据包覆盖**所有**仓库包，但实测只有 8.3% 的仓库包在里面；
    // 已安装包的 .desktop 只覆盖已安装的包，却几乎覆盖所有 GUI 软件（含 AUR）。
    // 两者都不是对方的上位替代，所以分开报告。
    let (appstream_icons, desktop_icons) = match &probe {
        Ok(p) => (p.appstream_icons, p.desktop_icons),
        // libalpm 打不开时仍然报告 AppStream 一项（它不需要 alpm）
        Err(_) => (crate::icons::AppstreamIcons::load().len(), 0),
    };
    if appstream_icons == 0 {
        r.warn(
            "软件图标",
            format!(
                "AppStream 数据未安装：未安装的仓库软件只能显示字母头像（已安装软件仍可从 .desktop 取图，当前 {} 个）。安装数据包：{}",
                desktop_icons,
                crate::icons::install_hint()
            ),
        );
    } else {
        r.ok(
            "软件图标",
            format!(
                "AppStream 覆盖 {} 个仓库包；{} 个已安装包另有 .desktop 图标（含 AUR 软件）",
                appstream_icons, desktop_icons
            ),
        );
    }

    // 10) 缓存目录
    let cache_dir = paths::cache_dir();
    match ensure_writable_dir(&cache_dir) {
        Ok(size) => r.ok(
            "缓存目录",
            format!(
                "{}（可写，{}）",
                cache_dir.display(),
                crate::model::human_size(size)
            ),
        ),
        Err(e) => r.fail("缓存目录", format!("{} 不可写：{e}", cache_dir.display())),
    }

    r
}

/// 确保目录存在且可写，返回当前占用字节数。
pub fn ensure_writable_dir(dir: &Path) -> CoreResult<u64> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(".archstore-write-test");
    std::fs::write(&probe, b"ok")?;
    let _ = std::fs::remove_file(&probe);
    Ok(dir_size(dir))
}

/// 递归统计目录大小（用于设置页"当前缓存大小"）。
pub fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for e in entries.flatten() {
        match e.file_type() {
            Ok(t) if t.is_dir() => total += dir_size(&e.path()),
            Ok(t) if t.is_file() => {
                if let Ok(m) = e.metadata() {
                    total += m.len();
                }
            }
            _ => {}
        }
    }
    total
}

/// 把 probe 的结果汇总为后端 Capability。
pub fn capabilities_from(report: &DoctorReport) -> Capabilities {
    let find = |title: &str| report.checks.iter().find(|c| c.title.starts_with(title));
    let alpm = find("libalpm 可打开");
    let flatpak = find("flatpak");
    let helper = detect_helper();
    let (distro, _) = detect_distro();
    let cfg = crate::config::Config::default();
    let aur_helper = find_aur_helper(cfg.sources.aur_helper);
    let flatpak_remotes = detect_flatpak().map(|(_, r)| r).unwrap_or_default();

    let alpm_cap = match alpm {
        Some(c) if c.level == Level::Fail => {
            crate::backend::Capability::unavailable(c.detail.clone())
        }
        Some(c) => crate::backend::Capability::available_with(
            (c.level == Level::Warn).then(|| c.detail.clone()),
        ),
        None => crate::backend::Capability::unavailable("未检测"),
    };

    let flatpak_cap = match flatpak {
        Some(c) if c.level == Level::Fail => {
            crate::backend::Capability::unavailable(c.detail.clone())
        }
        Some(c) => crate::backend::Capability::available_with(
            (c.level == Level::Warn).then(|| c.detail.clone()),
        ),
        None => crate::backend::Capability::unavailable("未检测"),
    };

    let aur_cap = match find("AUR 助手") {
        Some(c) if c.level == Level::Fail => {
            crate::backend::Capability::unavailable(c.detail.clone())
        }
        _ => crate::backend::Capability::available(),
    };

    Capabilities {
        distro,
        pacman: alpm_cap,
        aur: aur_cap,
        flatpak: flatpak_cap,
        aur_helper,
        flatpak_remotes,
        helper,
    }
}

/// 供 GUI 在启动阶段使用的版本比较辅助。
pub fn version_at_least(version: (u32, u32, u32), min: (u32, u32)) -> bool {
    (version.0, version.1) >= min
}

/// 生成"非 Arch 系统"的启动错误。
pub fn require_arch() -> CoreResult<()> {
    let (distro, is_arch) = detect_distro();
    if is_arch {
        Ok(())
    } else {
        Err(CoreError::EnvUnsupported(format!(
            "{distro} 上不存在 pacman/libalpm；本程序只支持 Arch Linux 及其衍生发行版"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacman_conf_repo_parsing_ignores_options_and_duplicates() {
        let text = r#"
# comment
[options]
HoldPkg = pacman glibc

[core]
Include = /etc/pacman.d/mirrorlist

[extra]
Include = /etc/pacman.d/mirrorlist

[  multilib  ]
#Include = /etc/pacman.d/mirrorlist
"#;
        assert_eq!(
            parse_pacman_conf_repos(text),
            vec![
                "core".to_string(),
                "extra".to_string(),
                "multilib".to_string()
            ]
        );
    }

    #[test]
    fn pacman_conf_repo_parsing_preserves_case() {
        // register_syncdb_mut("Extra") 返回 Ok，所以大小写必须原样保留
        assert_eq!(
            parse_pacman_conf_repos("[Extra]\n"),
            vec!["Extra".to_string()]
        );
    }

    #[test]
    fn discover_from_dir_reads_db_names() {
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(dir.path().join("core.db"), b"x").expect("w");
        std::fs::write(dir.path().join("extra.db"), b"x").expect("w");
        std::fs::write(dir.path().join("multilib.db"), b"").expect("w");
        std::fs::write(dir.path().join("notes.txt"), b"x").expect("w");
        assert_eq!(
            discover_sync_repos_from_dir(dir.path()),
            vec![
                "core".to_string(),
                "extra".to_string(),
                "multilib".to_string()
            ]
        );
        assert!(sync_db_file_ready(dir.path(), "core"));
        assert!(
            !sync_db_file_ready(dir.path(), "multilib"),
            "空 db 视为未就绪"
        );
        assert!(!sync_db_file_ready(dir.path(), "missing"));
    }

    #[test]
    fn doctor_report_exit_code_counts_failures() {
        let mut r = DoctorReport::new();
        r.ok("a", "1");
        r.warn("b", "2");
        assert_eq!(r.exit_code(), 0);
        assert!(!r.has_failures());
        r.fail("c", "3");
        r.fail("d", "4");
        assert_eq!(r.exit_code(), 2);
        assert_eq!(r.failure_count(), 2);
        assert_eq!(r.warn_count(), 1);
        let text = r.render();
        assert!(text.contains("[ OK ] a：1"));
        assert!(text.contains("[WARN] b：2"));
        assert!(text.contains("[FAIL] c：3"));
    }

    #[test]
    fn doctor_reports_icon_sources() {
        // 无论本机是否装了数据包、libalpm 是否可用，都必须产出一条结论（OK 或 WARN），
        // 而不是静默跳过 —— 图标是用户最容易察觉的缺失。
        let report = probe(None, None, &crate::config::Config::default());
        let check = report
            .checks
            .iter()
            .find(|c| c.title == "软件图标")
            .expect("必须报告软件图标来源");
        assert!(!check.detail.is_empty());
        if crate::icons::AppstreamIcons::load().is_empty() {
            assert_eq!(check.level, Level::Warn);
            assert!(check.detail.contains(crate::icons::DATA_PACKAGE));
        } else {
            assert_eq!(check.level, Level::Ok);
            assert!(check.detail.contains("AppStream 覆盖"));
        }
    }

    #[test]
    fn doctor_json_is_valid() {
        let mut r = DoctorReport::new();
        r.ok("标题", "细节");
        let v: serde_json::Value = serde_json::from_str(&r.to_json()).expect("json");
        assert_eq!(v["failures"], 0);
        assert_eq!(v["checks"][0]["title"], "标题");
    }

    #[test]
    fn version_at_least_compares_major_minor_only() {
        assert!(version_at_least((4, 18, 0), MIN_GTK));
        assert!(version_at_least((4, 22, 4), MIN_GTK));
        assert!(!version_at_least((4, 17, 9), MIN_GTK));
        assert!(!version_at_least((3, 24, 0), MIN_GTK));
        assert!(version_at_least((1, 9, 3), MIN_ADW));
        assert!(!version_at_least((1, 7, 0), MIN_ADW));
    }

    #[test]
    fn which_finds_sh_and_rejects_nonexistent() {
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-real-binary-xyz").is_none());
    }

    #[test]
    fn aur_helper_choice_respects_none() {
        assert_eq!(find_aur_helper(AurHelperChoice::None), None);
    }

    #[test]
    fn detect_distro_returns_something() {
        let (name, _) = detect_distro();
        assert!(!name.is_empty());
    }
}
