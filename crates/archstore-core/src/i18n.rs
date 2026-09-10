//! 国际化：gettext 初始化与软件名表（project.md §7.2）。
//!
//! 中文分四层：
//! 1. AppStream 内嵌翻译（Flatpak 元数据随 locale 变化）—— 由 Flathub API 提供，默认开启。
//! 2. 内置软件名表 i18n/software-names.json —— 只做名称映射，离线可用，默认开启。
//! 3. 界面文案 gettext po/zh_CN.po —— 全部 UI 字符串通过 t() 获取，默认开启。
//! 4. 在线翻译描述 —— 默认关闭，需用户在设置页显式开启并展示隐私说明。

use std::collections::HashMap;
use std::path::Path;

use crate::model::PackageId;

/// 初始化 gettext。任何一步失败都不影响程序启动（最多是界面显示英文原文）。
pub fn init() {
    let _ = gettextrs::bindtextdomain("archstore", "/usr/share/locale");
    let _ = gettextrs::bind_textdomain_codeset("archstore", "UTF-8");
    let _ = gettextrs::textdomain("archstore");
}

/// 取一条界面文案的译文。
pub fn t(s: &str) -> String {
    gettextrs::gettext(s)
}

/// 标记可翻译字符串（用于 xgettext 抽取 POT）。
///
/// 返回值在运行期不做替换，调用方应在展示前调用 t()。
pub const fn n(s: &str) -> &str {
    s
}

/// 内置软件名表：只做名称映射（如 firefox -> 火狐浏览器）。
#[derive(Debug, Clone, Default)]
pub struct SoftwareNames {
    map: HashMap<String, String>,
}

impl SoftwareNames {
    /// 从 JSON 文本构造。格式为扁平的 {"包名或应用ID": "中文名"}。
    pub fn from_json(text: &str) -> Self {
        let map: HashMap<String, String> = serde_json::from_str(text).unwrap_or_default();
        Self {
            map: map
                .into_iter()
                .filter(|(k, v)| !k.is_empty() && !v.is_empty())
                .map(|(k, v)| (k.to_ascii_lowercase(), v))
                .collect(),
        }
    }

    /// 依次尝试多个内置路径；第一个成功读取的文件生效。
    pub fn load_from(paths: &[std::path::PathBuf]) -> Self {
        for p in paths {
            if let Ok(text) = std::fs::read_to_string(p) {
                let names = Self::from_json(&text);
                if !names.map.is_empty() {
                    tracing::debug!(path = %p.display(), count = names.map.len(), "已加载软件名表");
                    return names;
                }
            }
        }
        Self::default()
    }

    /// 载入随应用分发或安装到 /usr/share 的名表。
    pub fn load() -> Self {
        Self::load_from(&crate::config::paths::software_names_file())
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// 按应用 ID / 包名查询中文名。
    pub fn lookup(&self, id: &PackageId) -> Option<&str> {
        let name = id.name.to_ascii_lowercase();
        if let Some(v) = self.map.get(&name) {
            return Some(v.as_str());
        }
        // Flatpak 应用 ID 的末段常常就是包名（org.mozilla.firefox -> firefox）
        if let Some(last) = name.rsplit('.').next()
            && let Some(v) = self.map.get(last)
        {
            return Some(v.as_str());
        }
        None
    }

    /// 用名表覆盖展示名（找不到则保留原名）。
    pub fn apply(&self, id: &PackageId, display_name: &str) -> String {
        self.lookup(id).unwrap_or(display_name).to_string()
    }

    /// 从文件路径加载（供 GUI 在设置变更后重载）。
    pub fn load_from_file(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_json(&text),
            Err(_) => Self::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_names_lookup_by_name_and_app_id() {
        let names = SoftwareNames::from_json(
            r#"{"firefox":"火狐浏览器","org.mozilla.firefox":"Mozilla Firefox","gimp":"GIMP"}"#,
        );
        assert_eq!(names.len(), 3);
        assert_eq!(names.lookup(&PackageId::aur("firefox")), Some("火狐浏览器"));
        assert_eq!(
            names.lookup(&PackageId::official("extra", "gimp")),
            Some("GIMP")
        );
        // 精确的 app id 优先
        assert_eq!(
            names.lookup(&PackageId::flatpak("flathub", "org.mozilla.firefox")),
            Some("Mozilla Firefox")
        );
        // 回退到末段
        assert_eq!(
            names.lookup(&PackageId::flatpak("flathub", "org.gnome.gimp")),
            Some("GIMP")
        );
        assert_eq!(names.lookup(&PackageId::aur("unknown-pkg")), None);
    }

    #[test]
    fn malformed_json_yields_empty_table() {
        let names = SoftwareNames::from_json("{not json");
        assert!(names.is_empty());
    }

    #[test]
    fn t_falls_back_to_source_string() {
        init();
        // 没有安装 .mo 文件时必须原样返回，绝不 panic
        assert_eq!(t("Install"), "Install");
    }
}
