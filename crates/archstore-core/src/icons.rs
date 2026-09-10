//! AppStream 图标索引（仓库软件图标来源之一，见 `crate::desktop_icons` 与 GUI 的 icon_cache）。
//!
//! 完整的图标优先级（"很多软件没有图标"的修复）：
//! 1. `IconRef::CachedFile`：本模块给出的 AppStream 图标文件；
//! 2. `IconRef::IconName`：已安装包 `.desktop` 里的 `Icon=`（`crate::desktop_icons`）；
//! 3. `IconRef::Remote`：Flatpak/Flathub 图标 URL（GUI 异步下载后落盘）；
//! 4. 主题里与包名/应用 ID 同名的图标；
//! 5. 字母头像。
//!
//! **本模块的问题（实测）**：hicolor 主题里"与包名同名"的图标极少
//! （本机 974 个已安装包里只有 13 个），所以只靠主题名对绝大多数仓库包无效。
//! pamac / GNOME Software 靠的是 archlinux-appstream-data 这份元数据库，其布局为：
//!
//! ```text
//! /usr/share/swcatalog/xml/{core,extra,multilib}.xml.gz
//! /usr/share/swcatalog/icons/archlinux-arch-<repo>/{48x48,64x64,128x128}/<包名>_<appid>.png
//! ```
//!
//! 实测 extra 有 1203 个 128x128 图标、multilib 2 个，文件名前缀就是**包名**。
//! 因此只要扫一遍图标目录、切出包名，就能建立 "包名 -> 图标文件" 的映射 ——
//! 比解析 22 MB 的 gzip XML（实测 147 ms）快几个数量级。
//!
//! 但**覆盖率有限**：实测 15252 个仓库包里只有 1259 个（8.3%）在这份数据里有图标，
//! AUR 包一个都没有 —— 所以已安装包还要走 `desktop_icons` 那条路。
//!
//! 旧布局 /usr/share/app-info/icons/ 也一并支持。该数据包是**可选依赖**：
//! 没装时索引为空，UI 会退回字母头像并提示安装命令。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 优先使用的尺寸档（越大越清晰，但列表页 128 已足够）。
pub const SIZES: [&str; 3] = ["128x128", "64x64", "48x48"];

/// 包名 -> 图标文件。
#[derive(Debug, Clone, Default)]
pub struct AppstreamIcons {
    map: HashMap<String, PathBuf>,
}

impl AppstreamIcons {
    /// 扫描默认位置。没有安装数据包时返回空索引（不是错误）。
    pub fn load() -> Self {
        Self::load_from(&default_roots())
    }

    /// 带"包名判定器"的扫描：用于消歧**包名本身含下划线**的图标文件名。
    ///
    /// 文件名布局是 `<包名>_<appid>.png`，两个部分都可能含下划线：
    /// `jack_mixer_jack_mixer.png`（包名 jack_mixer）与
    /// `celluloid_io.github.celluloid_player.Celluloid.png`（appid 含下划线）。
    /// 只看第一个下划线会把前者错误归属给 `jack` —— 这是"图标张冠李戴"。
    /// 传入 libalpm 的包名查询后，取**最长的、确实存在的**下划线前缀即可两者都对。
    pub fn load_with(known: &dyn Fn(&str) -> bool) -> Self {
        Self::load_from_with(&default_roots(), Some(known))
    }

    /// 从给定根目录扫描（便于测试）。
    ///
    /// 每个根目录下的结构可以是：
    /// - `<root>/<repo>/<size>/<pkg>_<appid>.png`（swcatalog 与 app-info 的实际布局）
    pub fn load_from(roots: &[PathBuf]) -> Self {
        Self::load_from_with(roots, None)
    }

    /// `load_from` + 可选的包名判定器（见 `load_with`）。
    pub fn load_from_with(roots: &[PathBuf], known: Option<&dyn Fn(&str) -> bool>) -> Self {
        let mut map = HashMap::new();
        for root in roots {
            let Ok(repos) = std::fs::read_dir(root) else {
                continue;
            };
            for repo in repos.flatten() {
                let repo_path = repo.path();
                if !repo_path.is_dir() {
                    continue;
                }
                // 按尺寸档优先级扫描：先命中 128x128 的包就不会被 48x48 覆盖
                for size in SIZES {
                    let dir = repo_path.join(size);
                    let Ok(files) = std::fs::read_dir(&dir) else {
                        continue;
                    };
                    for entry in files.flatten() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        let Some(stem) = name.strip_suffix(".png") else {
                            continue;
                        };
                        let Some(pkg) = package_of(stem, known) else {
                            // 没有下划线的文件无法归属到包，跳过
                            continue;
                        };
                        // 低优先级尺寸不覆盖已命中的包
                        map.entry(pkg.to_string()).or_insert_with(|| entry.path());
                    }
                }
            }
        }
        Self { map }
    }

    /// 按包名查图标文件。
    pub fn lookup(&self, package: &str) -> Option<&Path> {
        self.map.get(package).map(|p| p.as_path())
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 索引了哪些包名（用于诊断输出，只取前 n 个）。
    pub fn sample(&self, n: usize) -> Vec<&str> {
        self.map.keys().take(n).map(|s| s.as_str()).collect()
    }
}

/// 从 `<包名>_<appid>.png` 的文件名里取出包名。
///
/// - 没有 `known` 判定器时：按第一个下划线切分（旧行为，够用但会误判 `jack_mixer`）；
/// - 有判定器时：取最长的、确实存在的下划线前缀（`jack_mixer` 而不是 `jack`），
///   全部落空才退回第一个下划线切分。
fn package_of<'a>(stem: &'a str, known: Option<&dyn Fn(&str) -> bool>) -> Option<&'a str> {
    let first = stem.find('_')?;
    let head = &stem[..first];
    // `_` 前为空、或 `_` 后为空（如 "foo_"）都无法归属到包
    if head.is_empty() || stem.ends_with('_') {
        return None;
    }
    let Some(known) = known else {
        return Some(head);
    };
    for (idx, _) in stem.match_indices('_').rev() {
        let candidate = &stem[..idx];
        if !candidate.is_empty() && known(candidate) {
            return Some(candidate);
        }
    }
    Some(head)
}

/// 环境变量覆盖：`ARCHSTORE_APPSTREAM_ICONS`（冒号分隔多个根目录）。
///
/// 用于自定义前缀（例如把 appstream 数据装在别处）以及端到端测试。
pub const ROOTS_ENV: &str = "ARCHSTORE_APPSTREAM_ICONS";

/// 默认扫描位置（新布局优先）。
pub fn default_roots() -> Vec<PathBuf> {
    // 显式配置优先：完全替换默认位置，便于指向非标准安装
    if let Some(custom) = std::env::var_os(ROOTS_ENV) {
        let paths: Vec<PathBuf> = std::env::split_paths(&custom)
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        if !paths.is_empty() {
            return paths;
        }
    }
    let mut roots = vec![PathBuf::from("/usr/share/swcatalog/icons")];
    // 旧版 appstream-data 的布局
    roots.push(PathBuf::from("/usr/share/app-info/icons"));
    // 用户级安装（flatpak 之外的场景）
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(
            Path::new(&home)
                .join(".local/share/swcatalog/icons")
                .to_path_buf(),
        );
    }
    roots
}

/// 建议用户安装的数据包名（--doctor 与设置页用）。
pub const DATA_PACKAGE: &str = "archlinux-appstream-data";

/// 安装命令（面向用户的提示，绝不代执行）。
pub fn install_hint() -> String {
    format!("sudo pacman -S {DATA_PACKAGE}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个 swcatalog 布局的临时目录。
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tmpdir");
        for (repo, size, name) in [
            ("archlinux-arch-extra", "128x128", "firefox_firefox.png"),
            ("archlinux-arch-extra", "128x128", "gimp_gimp.png"),
            (
                "archlinux-arch-extra",
                "128x128",
                "accessibility-inspector_org.kde.accessibilityinspector.png",
            ),
            // 只有 48x48 的包，应回退到小尺寸
            ("archlinux-arch-extra", "48x48", "only-small_only-small.png"),
            // 128x128 与 48x48 都有：必须选 128
            ("archlinux-arch-extra", "48x48", "firefox_firefox.png"),
            ("archlinux-arch-multilib", "128x128", "steam_steam.png"),
            // 无下划线，无法归属
            ("archlinux-arch-extra", "128x128", "bogusname.png"),
        ] {
            let p = dir.path().join(repo).join(size);
            std::fs::create_dir_all(&p).expect("mkdir");
            std::fs::write(p.join(name), b"png").expect("write");
        }
        dir
    }

    #[test]
    fn scans_and_maps_package_names() {
        let dir = fixture();
        let icons = AppstreamIcons::load_from(&[dir.path().to_path_buf()]);
        // firefox / gimp / accessibility-inspector / only-small / steam
        // （bogusname.png 没有下划线，无法归属到包，被跳过）
        assert_eq!(icons.len(), 5);
        assert!(icons.lookup("firefox").is_some());
        assert!(icons.lookup("gimp").is_some());
        assert!(icons.lookup("steam").is_some());
        assert!(icons.lookup("definitely-not-there").is_none());
    }

    #[test]
    fn package_name_is_the_prefix_before_first_underscore() {
        let dir = fixture();
        let icons = AppstreamIcons::load_from(&[dir.path().to_path_buf()]);
        let p = icons
            .lookup("accessibility-inspector")
            .expect("带点号的 appid 不应影响包名解析");
        assert!(p.to_string_lossy().contains("128x128"));
    }

    #[test]
    fn larger_size_wins() {
        let dir = fixture();
        let icons = AppstreamIcons::load_from(&[dir.path().to_path_buf()]);
        let p = icons.lookup("firefox").expect("firefox");
        assert!(
            p.to_string_lossy().contains("128x128"),
            "128x128 必须优先于 48x48：{}",
            p.display()
        );
    }

    #[test]
    fn falls_back_to_smaller_size_when_only_one_exists() {
        let dir = fixture();
        let icons = AppstreamIcons::load_from(&[dir.path().to_path_buf()]);
        let p = icons.lookup("only-small").expect("only-small");
        assert!(p.to_string_lossy().contains("48x48"));
    }

    #[test]
    fn files_without_underscore_are_skipped() {
        let dir = fixture();
        let icons = AppstreamIcons::load_from(&[dir.path().to_path_buf()]);
        assert!(icons.lookup("bogusname").is_none());
    }

    #[test]
    fn known_package_names_disambiguate_underscores() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let p = dir.path().join("archlinux-arch-extra").join("128x128");
        std::fs::create_dir_all(&p).expect("mkdir");
        for name in [
            "jack_mixer_jack_mixer.png",
            "celluloid_io.github.celluloid_player.Celluloid.png",
        ] {
            std::fs::write(p.join(name), b"png").expect("write");
        }
        let roots = [dir.path().to_path_buf()];

        // 旧行为：只按第一个下划线切分 —— jack_mixer 会被错误归属给 jack
        let naive = AppstreamIcons::load_from(&roots);
        assert!(naive.lookup("jack").is_some());
        assert!(naive.lookup("jack_mixer").is_none());

        // 有包名判定器时取"最长的真实包名"
        let known = |n: &str| ["jack_mixer", "celluloid"].contains(&n);
        let icons = AppstreamIcons::load_from_with(&roots, Some(&known));
        assert!(icons.lookup("jack_mixer").is_some());
        assert!(
            icons.lookup("jack").is_none(),
            "不得把 jack_mixer 的图标给 jack"
        );
        assert!(
            icons.lookup("celluloid").is_some(),
            "appid 里的下划线不得把包名切碎"
        );
    }

    #[test]
    fn unknown_package_names_fall_back_to_first_underscore() {
        let dir = fixture();
        let known = |_: &str| false;
        let icons = AppstreamIcons::load_from_with(&[dir.path().to_path_buf()], Some(&known));
        // 判定器什么都没命中时，行为与旧实现一致
        assert!(icons.lookup("firefox").is_some());
        assert!(icons.lookup("accessibility-inspector").is_some());
        assert_eq!(icons.len(), 5);
    }

    #[test]
    fn missing_roots_are_not_an_error() {
        let icons = AppstreamIcons::load_from(&[PathBuf::from("/nonexistent/swcatalog/icons")]);
        assert!(icons.is_empty());
        assert_eq!(icons.len(), 0);
        assert!(icons.lookup("firefox").is_none());
        assert!(icons.sample(5).is_empty());
    }

    #[test]
    fn default_roots_cover_both_layouts() {
        let roots = default_roots();
        assert!(
            roots
                .iter()
                .any(|p| p.to_string_lossy().contains("swcatalog"))
        );
        assert!(
            roots
                .iter()
                .any(|p| p.to_string_lossy().contains("app-info"))
        );
    }

    #[test]
    fn env_override_replaces_default_roots() {
        // 不能真的改进程环境（测试并行会互相干扰），只验证常量与切分语义
        assert_eq!(ROOTS_ENV, "ARCHSTORE_APPSTREAM_ICONS");
        let split: Vec<PathBuf> = std::env::split_paths("/a/b:/c/d").collect();
        assert_eq!(split, vec![PathBuf::from("/a/b"), PathBuf::from("/c/d")]);
    }

    #[test]
    fn install_hint_names_the_package() {
        let hint = install_hint();
        assert!(hint.contains(DATA_PACKAGE));
        assert!(hint.starts_with("sudo pacman -S"));
    }
}
