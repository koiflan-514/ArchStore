//! 已安装包的 `.desktop` 图标索引（AUR 与"AppStream 里没有"的仓库包）。
//!
//! **问题（实测）**：本机 15252 个仓库包里只有 1260 个（8.2%）在
//! archlinux-appstream-data 里有图标文件，AUR 包则一个都没有；而 hicolor 主题里
//! "与包名同名"的图标只有 13 个。只靠这两条路，绝大多数软件只能显示字母头像。
//!
//! **方案**：已安装的软件几乎都会在 `/usr/share/applications/` 下安装一个
//! `.desktop`，其中的 `Icon=` 直接给出主题图标名（或图标绝对路径）——
//! GNOME Software / pamac 对**已安装**应用就是这么找图标的。libalpm 的本地库
//! 自带文件清单（即 `pacman -Ql` 的数据源），所以这一步
//! **不需要 root、不需要联网、不需要额外数据包**。
//!
//! **范围**：只能覆盖"已安装"的包。未安装的包磁盘上没有它的 `.desktop`，
//! 仍然依赖 AppStream 数据包（仓库包）或字母头像（AUR）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::model::IconRef;

/// 单个 `.desktop` 的读取上限（异常大的文件直接跳过）。
pub const MAX_DESKTOP_BYTES: u64 = 128 * 1024;

/// 包名 -> 图标（来自该包安装的 `.desktop`）。
#[derive(Debug, Clone, Default)]
pub struct DesktopIcons {
    map: HashMap<String, IconRef>,
}

impl DesktopIcons {
    /// 由已解析好的映射构造（解析在调用方完成，便于测试）。
    pub fn from_map(map: HashMap<String, IconRef>) -> Self {
        Self { map }
    }

    /// 按包名查图标。只有已安装的包才会命中。
    pub fn lookup(&self, package: &str) -> Option<&IconRef> {
        self.map.get(package)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// 判断一个（相对或绝对）文件路径是否是某包的候选 `.desktop`。
///
/// 返回 `(优先级, 绝对路径)`，优先级越小越先尝试：
/// 0 = 文件名与包名完全相同，1 = 以包名开头（`code` -> `code-oss.desktop`），
/// 2 = 包名以文件名开头，3 = 其它（如 gnome-control-center 的各个面板入口）。
pub fn desktop_candidate(path: &str, package: &str) -> Option<(u8, PathBuf)> {
    let rel = path.strip_prefix('/').unwrap_or(path);
    // 只认 XDG 应用目录（含 kde4/ 这类子目录）
    let in_app_dir = rel.starts_with("usr/share/applications/")
        || rel.starts_with("usr/local/share/applications/");
    if !in_app_dir || !rel.ends_with(".desktop") {
        return None;
    }
    let stem = Path::new(rel).file_stem()?.to_str()?;
    // 空文件名与隐藏文件（如 ".desktop"）都不是应用入口
    if stem.is_empty() || stem.starts_with('.') || package.is_empty() {
        return None;
    }
    let rank = if stem == package {
        0
    } else if stem.starts_with(package) {
        1
    } else if package.starts_with(stem) {
        2
    } else {
        3
    };
    Some((rank, Path::new("/").join(rel)))
}

/// 解析 `.desktop` 文本里的 `Icon=`（只看 `[Desktop Entry]` 组）。
///
/// 按 XDG 规范，后续组的键不得影响主组；这里严格按组解析，
/// 避免把 `[Desktop Action foo]` 里的 `Icon=` 当成应用图标。
pub fn parse_icon(text: &str) -> Option<String> {
    let mut in_entry = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        if let Some(value) = line.strip_prefix("Icon=") {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// `Icon=` 的值 -> `IconRef`。
///
/// - 绝对路径且文件存在：`CachedFile` 直接引用；
/// - 绝对路径但文件不存在：`None`（损坏的 .desktop，别拿它当主题名）；
/// - 其它：当作主题图标名（GUI 侧再用 IconTheme 解析），并去掉 `.png` 这类后缀 ——
///   `Icon=foo.png` 这种写法在第三方包里很常见，而主题名是 `foo`。
pub fn to_ref(value: &str) -> Option<IconRef> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.starts_with('/') {
        let path = PathBuf::from(value);
        return path.is_file().then_some(IconRef::CachedFile(path));
    }
    Some(IconRef::IconName(strip_image_ext(value).to_string()))
}

/// 去掉图标文件后缀（主题名不带后缀）。
fn strip_image_ext(name: &str) -> &str {
    for ext in [".png", ".svg", ".svgz", ".xpm", ".jpg", ".jpeg", ".ico"] {
        if let Some(stem) = name.strip_suffix(ext)
            && !stem.is_empty()
        {
            return stem;
        }
    }
    name
}

/// 读取一个 `.desktop` 文件并给出图标；读不到或没有 `Icon=` 时返回 `None`。
pub fn icon_from_file(path: &Path) -> Option<IconRef> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_DESKTOP_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    parse_icon(&text).and_then(|value| to_ref(&value))
}

/// 从一个包的候选 `.desktop` 列表里挑出图标：按优先级排序，取第一个能解析出图标的。
///
/// 候选来自 libalpm 的文件清单，因此调用方负责提供 `(优先级, 路径)`。
pub fn resolve_candidates(mut candidates: Vec<(u8, PathBuf)>) -> Option<IconRef> {
    candidates.sort();
    candidates
        .into_iter()
        .find_map(|(_, path)| icon_from_file(&path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_accepts_xdg_application_dirs() {
        let (rank, path) =
            desktop_candidate("usr/share/applications/firefox.desktop", "firefox").expect("候选");
        assert_eq!(rank, 0);
        assert_eq!(
            path,
            PathBuf::from("/usr/share/applications/firefox.desktop")
        );
        assert!(desktop_candidate("usr/local/share/applications/foo.desktop", "foo").is_some());
        assert!(desktop_candidate("usr/share/applications/kde4/foo.desktop", "foo").is_some());
    }

    #[test]
    fn candidate_rejects_non_application_paths() {
        // 不是 .desktop
        assert!(desktop_candidate("usr/share/applications/foo.txt", "foo").is_none());
        // 不在应用目录
        assert!(desktop_candidate("usr/share/foo.desktop", "foo").is_none());
        assert!(desktop_candidate("usr/share/icons/hicolor/foo.desktop", "foo").is_none());
        // 空包名/空文件名
        assert!(desktop_candidate("usr/share/applications/.desktop", "foo").is_none());
        assert!(desktop_candidate("usr/share/applications/foo.desktop", "").is_none());
    }

    #[test]
    fn candidate_ranking_prefers_name_match() {
        let exact =
            desktop_candidate("usr/share/applications/code.desktop", "code").expect("exact");
        let prefixed =
            desktop_candidate("usr/share/applications/code-oss.desktop", "code").expect("prefix");
        let other = desktop_candidate("usr/share/applications/zzz.desktop", "code").expect("other");
        assert!(exact.0 < prefixed.0, "同名必须优先于前缀匹配");
        assert!(prefixed.0 < other.0, "前缀匹配必须优先于无关名字");
    }

    #[test]
    fn parse_icon_only_reads_desktop_entry_group() {
        let text = "\
[Desktop Entry]
Type=Application
Name=Example
Icon=example-app

[Desktop Action new]
Icon=should-not-be-used
";
        assert_eq!(parse_icon(text).as_deref(), Some("example-app"));
    }

    #[test]
    fn parse_icon_handles_missing_and_empty() {
        assert_eq!(parse_icon(""), None);
        assert_eq!(parse_icon("[Desktop Entry]\nName=x\n"), None);
        assert_eq!(
            parse_icon("[Desktop Entry]\nIcon=\nIcon=real\n"),
            Some("real".into())
        );
        // 组之前的键不算数
        assert_eq!(parse_icon("Icon=too-early\n[Desktop Entry]\n"), None);
    }

    #[test]
    fn to_ref_keeps_absolute_paths_that_exist() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let icon = dir.path().join("logo.png");
        std::fs::write(&icon, b"png").expect("write");
        let value = icon.display().to_string();
        match to_ref(&value) {
            Some(IconRef::CachedFile(p)) => assert_eq!(p, icon),
            other => panic!("绝对路径必须映射为 CachedFile：{other:?}"),
        }
    }

    #[test]
    fn to_ref_falls_back_to_icon_name() {
        match to_ref("firefox") {
            Some(IconRef::IconName(n)) => assert_eq!(n, "firefox"),
            other => panic!("主题名必须映射为 IconName：{other:?}"),
        }
        assert!(
            to_ref("/nonexistent/logo.png").is_none(),
            "不存在的绝对路径不是可用的图标来源"
        );
        assert!(to_ref("").is_none());
    }

    #[test]
    fn to_ref_strips_image_extensions() {
        for (input, want) in [
            ("foo.png", "foo"),
            ("org.gnome.Foo.svg", "org.gnome.Foo"),
            ("foo.xpm", "foo"),
            (".png", ".png"),
        ] {
            match to_ref(input) {
                Some(IconRef::IconName(n)) => assert_eq!(n, want, "{input}"),
                other => panic!("{input}: {other:?}"),
            }
        }
    }

    #[test]
    fn icon_from_file_reads_real_fixture() {
        let dir = tempfile::tempdir().expect("tmpdir");

        // 绝对路径且文件存在 -> CachedFile
        let icon_file = dir.path().join("app.png");
        std::fs::write(&icon_file, b"png").expect("write");
        let abs = dir.path().join("abs.desktop");
        std::fs::write(
            &abs,
            format!(
                "[Desktop Entry]\nType=Application\nIcon={}\n",
                icon_file.display()
            ),
        )
        .expect("write");
        match icon_from_file(&abs) {
            Some(IconRef::CachedFile(p)) => assert_eq!(p, icon_file),
            other => panic!("绝对路径必须落到文件：{other:?}"),
        }

        // 只有图标名 -> IconName（交给主题查找）
        let named = dir.path().join("named.desktop");
        std::fs::write(&named, "[Desktop Entry]\nIcon=app\n").expect("write");
        match icon_from_file(&named) {
            Some(IconRef::IconName(n)) => assert_eq!(n, "app"),
            other => panic!("{other:?}"),
        }

        // 没有 Icon= / 文件不存在 -> None
        let no_icon = dir.path().join("no-icon.desktop");
        std::fs::write(&no_icon, "[Desktop Entry]\nName=No Icon\n").expect("write");
        assert!(icon_from_file(&no_icon).is_none());
        assert!(icon_from_file(&dir.path().join("missing.desktop")).is_none());
    }

    #[test]
    fn resolve_candidates_prefers_lower_rank() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let good = dir.path().join("code.desktop");
        let bad = dir.path().join("code-oss.desktop");
        std::fs::write(&bad, "[Desktop Entry]\nIcon=bad\n").expect("write");
        std::fs::write(&good, "[Desktop Entry]\nIcon=good\n").expect("write");
        let picked = resolve_candidates(vec![(1, bad.clone()), (0, good)]).expect("命中");
        match picked {
            IconRef::IconName(n) => assert_eq!(n, "good"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn resolve_candidates_skips_files_without_icon() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let empty = dir.path().join("empty.desktop");
        let ok = dir.path().join("ok.desktop");
        std::fs::write(&empty, "[Desktop Entry]\nName=No Icon\n").expect("write");
        std::fs::write(&ok, "[Desktop Entry]\nIcon=ok\n").expect("write");
        let picked = resolve_candidates(vec![(0, empty), (1, ok)]).expect("命中");
        match picked {
            IconRef::IconName(n) => assert_eq!(n, "ok"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn empty_index_is_fine() {
        let icons = DesktopIcons::default();
        assert!(icons.is_empty());
        assert_eq!(icons.len(), 0);
        assert!(icons.lookup("firefox").is_none());
    }
}
