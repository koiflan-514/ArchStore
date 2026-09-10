//! 配置模型 + 原子读写 + 版本迁移（project.md §6.3）。
//!
//! 路径：$XDG_CONFIG_HOME/archstore/config.toml，权限 0600（默认不创建，首次保存时创建）。
//!
//! 读写规则：
//! 1. 读取：文件不存在 -> 使用默认值并不写盘；TOML 解析失败 -> 备份为 config.toml.bak.<ts>，
//!    使用默认值，并以 AdwToast 告知（绝不静默覆盖用户配置）。
//! 2. 写入：原子替换（tmp + rename）；写入前与当前磁盘内容做一次字段级合并。
//! 3. 未知字段保留，保证版本回退不丢配置。
//! 4. schema 高于本程序支持值时，只读模式启动并提示升级应用。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};

/// 本程序支持的配置 schema 版本。
pub const CONFIG_SCHEMA: u32 = 1;

/// XDG 路径解析（支持 ARCHSTORE_* 覆盖，便于测试与便携运行）。
pub mod paths {
    use std::path::PathBuf;

    fn env_path(key: &str) -> Option<PathBuf> {
        std::env::var_os(key)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }

    fn home() -> PathBuf {
        env_path("HOME").unwrap_or_else(|| PathBuf::from("/tmp"))
    }

    pub fn config_home() -> PathBuf {
        env_path("ARCHSTORE_CONFIG_HOME")
            .or_else(|| env_path("XDG_CONFIG_HOME"))
            .unwrap_or_else(|| home().join(".config"))
    }

    pub fn cache_home() -> PathBuf {
        env_path("ARCHSTORE_CACHE_HOME")
            .or_else(|| env_path("XDG_CACHE_HOME"))
            .unwrap_or_else(|| home().join(".cache"))
    }

    pub fn runtime_dir() -> Option<PathBuf> {
        env_path("XDG_RUNTIME_DIR")
    }

    /// 配置文件：$XDG_CONFIG_HOME/archstore/config.toml
    pub fn config_file() -> PathBuf {
        config_home().join("archstore").join("config.toml")
    }

    /// 缓存目录：$XDG_CACHE_HOME/archstore
    pub fn cache_dir() -> PathBuf {
        cache_home().join("archstore")
    }

    /// 日志文件：$XDG_CACHE_HOME/archstore/archstore.log
    pub fn log_file() -> PathBuf {
        cache_dir().join("archstore.log")
    }

    /// 单实例锁文件（优先 $XDG_RUNTIME_DIR，退化到缓存目录）。
    pub fn lock_file() -> PathBuf {
        match runtime_dir() {
            Some(dir) => dir.join("archstore.lock"),
            None => cache_dir().join("instance.lock"),
        }
    }

    /// 内置软件名表（随应用分发，离线可用）。
    pub fn software_names_file() -> Vec<PathBuf> {
        vec![
            PathBuf::from("/usr/share/archstore/software-names.json"),
            PathBuf::from("i18n/software-names.json"),
        ]
    }

    /// helper 二进制的标准安装位置（与 polkit action 的 exec.path 一致）。
    pub fn helper_path() -> PathBuf {
        PathBuf::from("/usr/lib/archstore/archstore-helper")
    }

    /// polkit action 文件位置。
    pub fn policy_file() -> PathBuf {
        PathBuf::from("/usr/share/polkit-1/actions/io.github.archstore.ArchStore.policy")
    }
}

/// 从反序列化时对未知枚举取值宽容：无法识别则退回默认值。
fn lenient<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(T::deserialize(d).unwrap_or_default())
}

/// 配色方案。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorScheme {
    #[default]
    System,
    Light,
    Dark,
}

impl ColorScheme {
    pub fn as_str(&self) -> &'static str {
        match self {
            ColorScheme::System => "system",
            ColorScheme::Light => "light",
            ColorScheme::Dark => "dark",
        }
    }
}

/// 列表图标大小。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IconSize {
    Small,
    #[default]
    Medium,
    Large,
}

impl IconSize {
    pub fn as_str(&self) -> &'static str {
        match self {
            IconSize::Small => "small",
            IconSize::Medium => "medium",
            IconSize::Large => "large",
        }
    }

    /// 列表行像素尺寸。
    pub fn pixels(&self) -> i32 {
        match self {
            IconSize::Small => 24,
            IconSize::Medium => 32,
            IconSize::Large => 48,
        }
    }
}

/// 代理类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyType {
    #[default]
    Http,
    Socks5,
}

impl ProxyType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProxyType::Http => "http",
            ProxyType::Socks5 => "socks5",
        }
    }
}

/// AUR 助手选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AurHelperChoice {
    #[default]
    Auto,
    Paru,
    Yay,
    None,
}

impl AurHelperChoice {
    pub fn as_str(&self) -> &'static str {
        match self {
            AurHelperChoice::Auto => "auto",
            AurHelperChoice::Paru => "paru",
            AurHelperChoice::Yay => "yay",
            AurHelperChoice::None => "none",
        }
    }
}

/// 在线翻译服务选择。
///
/// 实测（2026-09-10）：LibreTranslate 的公共实例已全部失效（403/502/超时），
/// 因此它只能配合用户自建端点使用；默认选 MyMemory（官方免费、无需 key）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum TranslationApi {
    /// 不使用在线翻译
    #[serde(rename = "none")]
    None,
    /// MyMemory：免费、无需 key、开箱可用（单次 <= 500 字符，本程序自动分块）
    #[default]
    #[serde(rename = "mymemory")]
    MyMemory,
    /// LibreTranslate：需要用户提供端点（自建实例）
    #[serde(rename = "libretranslate")]
    LibreTranslate,
}

impl TranslationApi {
    pub fn as_str(&self) -> &'static str {
        match self {
            TranslationApi::None => "none",
            TranslationApi::MyMemory => "mymemory",
            TranslationApi::LibreTranslate => "libretranslate",
        }
    }

    /// 该服务是否需要用户填写端点。
    pub fn needs_endpoint(&self) -> bool {
        matches!(self, TranslationApi::LibreTranslate)
    }

    /// 面向 UI 的说明。
    pub fn describe(&self) -> &'static str {
        match self {
            TranslationApi::None => "不使用在线翻译",
            TranslationApi::MyMemory => "MyMemory（免费、无需注册，开箱可用）",
            TranslationApi::LibreTranslate => "LibreTranslate（需要你自己的服务端点）",
        }
    }
}

/// [appearance]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceConfig {
    #[serde(deserialize_with = "lenient")]
    pub color_scheme: ColorScheme,
    #[serde(deserialize_with = "lenient")]
    pub icon_size: IconSize,
    pub animations: bool,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            color_scheme: ColorScheme::System,
            icon_size: IconSize::Medium,
            animations: true,
        }
    }
}

/// [network]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    pub proxy_enabled: bool,
    #[serde(deserialize_with = "lenient")]
    pub proxy_type: ProxyType,
    pub proxy_url: String,
    pub timeout_secs: u64,
    /// 可选的额外 AUR 镜像 RPC 端点（留空使用官方）
    pub aur_rpc_url: String,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            proxy_enabled: false,
            proxy_type: ProxyType::Http,
            proxy_url: String::new(),
            timeout_secs: 30,
            aur_rpc_url: String::new(),
        }
    }
}

/// [sources]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SourcesConfig {
    pub pacman_enabled: bool,
    pub aur_enabled: bool,
    #[serde(deserialize_with = "lenient")]
    pub aur_helper: AurHelperChoice,
    pub flatpak_enabled: bool,
    pub flatpak_remote: String,
    pub flatpak_installation: String,
}

impl Default for SourcesConfig {
    fn default() -> Self {
        Self {
            pacman_enabled: true,
            aur_enabled: true,
            aur_helper: AurHelperChoice::Auto,
            flatpak_enabled: true,
            flatpak_remote: "flathub".to_string(),
            flatpak_installation: "system".to_string(),
        }
    }
}

/// [cache]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    pub max_size_mb: u64,
    pub aur_search_ttl_minutes: u64,
    pub aur_info_ttl_minutes: u64,
    pub flathub_ttl_hours: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_size_mb: 500,
            aur_search_ttl_minutes: 5,
            aur_info_ttl_minutes: 30,
            flathub_ttl_hours: 6,
        }
    }
}

/// [translation]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TranslationConfig {
    /// 默认关闭：开启后软件描述会发送到第三方服务
    pub auto_translate: bool,
    #[serde(deserialize_with = "lenient")]
    pub api: TranslationApi,
    pub api_endpoint: String,
    /// 目标语言；留空表示按系统 locale 自动判断（中文环境不翻译）
    pub target_lang: String,
}

impl Default for TranslationConfig {
    fn default() -> Self {
        Self {
            auto_translate: false,
            // 默认就选好可用的免费服务：用户只要打开开关即可生效
            api: TranslationApi::MyMemory,
            api_endpoint: String::new(),
            target_lang: String::new(),
        }
    }
}

/// [update]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateConfig {
    pub check_on_startup: bool,
    pub check_interval_hours: u64,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            check_on_startup: true,
            check_interval_hours: 6,
        }
    }
}

/// [ui]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub window_width: i32,
    pub window_height: i32,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            window_width: 1100,
            window_height: 720,
        }
    }
}

fn default_schema() -> u32 {
    CONFIG_SCHEMA
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema: CONFIG_SCHEMA,
            appearance: AppearanceConfig::default(),
            network: NetworkConfig::default(),
            sources: SourcesConfig::default(),
            cache: CacheConfig::default(),
            translation: TranslationConfig::default(),
            update: UpdateConfig::default(),
            ui: UiConfig::default(),
        }
    }
}

/// 完整配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(default = "default_schema")]
    pub schema: u32,
    pub appearance: AppearanceConfig,
    pub network: NetworkConfig,
    pub sources: SourcesConfig,
    pub cache: CacheConfig,
    pub translation: TranslationConfig,
    pub update: UpdateConfig,
    pub ui: UiConfig,
}

/// 配置加载结果：始终返回一份可用配置，并告知是否发生了降级。
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    /// true 表示 schema 高于本程序支持值，只能只读运行
    pub read_only: bool,
    /// 面向用户的提示（损坏重置、schema 过新等）
    pub notice: Option<String>,
    /// 损坏配置的备份路径
    pub backup: Option<PathBuf>,
    /// 配置文件是否存在
    pub existed: bool,
}

impl Config {
    /// 从指定路径加载；任何异常都降级为默认值而不是失败。
    pub fn load(path: &Path) -> Loaded {
        if !path.exists() {
            return Loaded {
                config: Config::default(),
                read_only: false,
                notice: None,
                backup: None,
                existed: false,
            };
        }
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                return Loaded {
                    config: Config::default(),
                    read_only: false,
                    notice: Some(format!("配置读取失败（{e}），已使用默认设置")),
                    backup: None,
                    existed: true,
                };
            }
        };
        let disk: toml::Table = match text.parse::<toml::Table>() {
            Ok(t) => t,
            Err(e) => {
                let backup = backup_corrupt(path);
                return Loaded {
                    config: Config::default(),
                    read_only: false,
                    notice: Some(format!(
                        "配置已损坏（{e}），已重置{}",
                        backup
                            .as_ref()
                            .map(|p| format!("，原文件备份为 {}", p.display()))
                            .unwrap_or_default()
                    )),
                    backup,
                    existed: true,
                };
            }
        };
        let enum_notices = enum_field_notices(&disk);
        let config: Config = match toml::Value::Table(disk).try_into() {
            Ok(c) => c,
            Err(e) => {
                let backup = backup_corrupt(path);
                return Loaded {
                    config: Config::default(),
                    read_only: false,
                    notice: Some(format!(
                        "配置字段无法识别（{e}），已重置{}",
                        backup
                            .as_ref()
                            .map(|p| format!("，原文件备份为 {}", p.display()))
                            .unwrap_or_default()
                    )),
                    backup,
                    existed: true,
                };
            }
        };
        if config.schema > CONFIG_SCHEMA {
            return Loaded {
                read_only: true,
                notice: Some(format!(
                    "配置版本 {} 高于本程序支持的 {}，已进入只读模式。请升级 ArchStore。",
                    config.schema, CONFIG_SCHEMA
                )),
                config,
                backup: None,
                existed: true,
            };
        }
        Loaded {
            config,
            read_only: false,
            notice: (!enum_notices.is_empty()).then(|| enum_notices.join("；")),
            backup: None,
            existed: true,
        }
    }

    /// 便捷方法：加载默认路径的配置。
    pub fn load_default() -> Loaded {
        Config::load(&paths::config_file())
    }

    /// 把越界值收敛到合法范围（读取后与写入前都调用）。
    pub fn sanitized(&self) -> Config {
        let mut c = self.clone();
        c.schema = CONFIG_SCHEMA;
        c.network.timeout_secs = c.network.timeout_secs.clamp(5, 300);
        c.network.proxy_url = c.network.proxy_url.trim().to_string();
        c.network.aur_rpc_url = c.network.aur_rpc_url.trim().to_string();
        c.cache.max_size_mb = c.cache.max_size_mb.clamp(16, 20_480);
        c.cache.aur_search_ttl_minutes = c.cache.aur_search_ttl_minutes.clamp(1, 1_440);
        c.cache.aur_info_ttl_minutes = c.cache.aur_info_ttl_minutes.clamp(1, 10_080);
        c.cache.flathub_ttl_hours = c.cache.flathub_ttl_hours.clamp(1, 720);
        c.update.check_interval_hours = c.update.check_interval_hours.clamp(1, 168);
        c.ui.window_width = c.ui.window_width.clamp(600, 10_000);
        c.ui.window_height = c.ui.window_height.clamp(400, 10_000);
        c.translation.api_endpoint = c.translation.api_endpoint.trim().to_string();
        c.translation.target_lang = c.translation.target_lang.trim().to_string();
        c.sources.flatpak_remote = c.sources.flatpak_remote.trim().to_string();
        if c.sources.flatpak_remote.is_empty() {
            c.sources.flatpak_remote = "flathub".to_string();
        }
        c.sources.flatpak_installation = match c.sources.flatpak_installation.as_str() {
            "user" => "user".to_string(),
            _ => "system".to_string(),
        };
        c
    }

    /// 校验会影响网络行为的字段，给出面向用户的错误。
    pub fn validate(&self) -> CoreResult<()> {
        if self.network.proxy_enabled {
            let url = self.network.proxy_url.trim();
            if url.is_empty() {
                return Err(CoreError::Config("已启用代理但未填写代理地址".into()));
            }
            // host[:port] 形式；不允许出现 scheme（避免 http://http:// 双重前缀）
            if url.contains("://") {
                return Err(CoreError::Config(
                    "代理地址不要包含 http:// 或 socks5:// 前缀，只填主机与端口".into(),
                ));
            }
            if url.contains(char::is_whitespace) {
                return Err(CoreError::Config("代理地址不能包含空白字符".into()));
            }
        }
        if self.translation.auto_translate {
            if self.translation.api == TranslationApi::None {
                return Err(CoreError::Config("已启用自动翻译但未选择翻译服务".into()));
            }
            // 只有 LibreTranslate 需要用户提供端点；MyMemory 是内置端点，开箱可用
            if self.translation.api.needs_endpoint()
                && !self.translation.api_endpoint.starts_with("https://")
                && !self
                    .translation
                    .api_endpoint
                    .starts_with("http://localhost")
                && !self
                    .translation
                    .api_endpoint
                    .starts_with("http://127.0.0.1")
            {
                return Err(CoreError::Config(
                    "翻译端点必须是 https:// 或本机地址（描述文本会发送到该地址）".into(),
                ));
            }
        }
        if !self.network.aur_rpc_url.is_empty() && !self.network.aur_rpc_url.starts_with("https://")
        {
            return Err(CoreError::Config("AUR RPC 端点必须是 https://".into()));
        }
        Ok(())
    }

    /// 原子写盘（tmp + rename），写入前与磁盘内容做字段级合并以保留未知字段。
    pub fn save(&self, path: &Path) -> CoreResult<()> {
        let sanitized = self.sanitized();
        sanitized.validate()?;
        let disk = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| t.parse::<toml::Table>().ok())
            .unwrap_or_default();
        let merged = merge_document(&sanitized, &disk)?;
        let text = toml::to_string_pretty(&merged)
            .map_err(|e| CoreError::Config(format!("无法序列化配置：{e}")))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::model::plan::write_private(path, text.as_bytes())
    }
}

/// 把已知字段写入磁盘文档，保留位置与未知字段。
///
/// TOML 要求"值必须出现在表之前"，因此输出顺序固定为：
/// 1) 顶层标量（schema 与用户自定义标量）；2) 已知的表；3) 用户自定义的表。
pub fn merge_document(config: &Config, disk: &toml::Table) -> CoreResult<toml::Table> {
    let known_value = toml::Value::try_from(config)
        .map_err(|e| CoreError::Config(format!("无法序列化配置：{e}")))?;
    let known = known_value
        .as_table()
        .cloned()
        .ok_or_else(|| CoreError::Config("配置根节点必须是表".into()))?;

    let mut out = toml::Table::new();

    // schema 永远排在最前
    out.insert(
        "schema".to_string(),
        toml::Value::Integer(i64::from(config.schema)),
    );

    // 1) 用户自定义的顶层标量
    for (k, v) in disk.iter() {
        if !v.is_table() && k != "schema" {
            out.insert(k.clone(), v.clone());
        }
    }

    // 2) 已知的表（把用户在同一节内的未知键合并进去）
    for (k, v) in known.iter() {
        if k == "schema" {
            continue;
        }
        let mut section = v.clone();
        if let (Some(target), Some(user)) = (
            section.as_table_mut(),
            disk.get(k).and_then(|x| x.as_table()),
        ) {
            for (uk, uv) in user.iter() {
                target.entry(uk.clone()).or_insert_with(|| uv.clone());
            }
        }
        out.insert(k.clone(), section);
    }

    // 3) 用户自定义的表
    for (k, v) in disk.iter() {
        if v.is_table() && !known.contains_key(k) {
            out.insert(k.clone(), v.clone());
        }
    }

    Ok(out)
}

/// 枚举型配置项的白名单表：(节名, 键名, 允许的取值)。
const ENUM_FIELDS: [(&str, &str, &[&str]); 5] = [
    ("appearance", "color_scheme", &["system", "light", "dark"]),
    ("appearance", "icon_size", &["small", "medium", "large"]),
    ("network", "proxy_type", &["http", "socks5"]),
    ("sources", "aur_helper", &["auto", "paru", "yay", "none"]),
    (
        "translation",
        "api",
        &["none", "mymemory", "libretranslate"],
    ),
];

/// 检查磁盘文档中的枚举型字段。
///
/// 枚举字段反序列化时是宽容的（未知取值退回默认值），因此需要单独检查并对用户给出提示，
/// 否则用户的设置会被静默忽略。
fn enum_field_notices(disk: &toml::Table) -> Vec<String> {
    let mut out = Vec::new();
    for (section, key, allowed) in ENUM_FIELDS {
        let Some(value) = disk.get(section).and_then(|s| s.get(key)) else {
            continue;
        };
        let ok = value
            .as_str()
            .map(|s| allowed.contains(&s))
            .unwrap_or(false);
        if !ok {
            out.push(format!(
                "{section}.{key} 的值 {value} 无法识别（可用：{}），已使用默认值",
                allowed.join(" / ")
            ));
        }
    }
    out
}

/// 把损坏的配置备份为 config.toml.bak.<ts>，返回备份路径。
fn backup_corrupt(path: &Path) -> Option<PathBuf> {
    let ts = crate::model::plan::now_unix();
    let backup = path.with_extension(format!("toml.bak.{ts}"));
    match std::fs::copy(path, &backup) {
        Ok(_) => Some(backup),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "配置备份失败");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_config() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("archstore").join("config.toml");
        (dir, path)
    }

    #[test]
    fn missing_file_uses_defaults_without_writing() {
        let (_d, path) = tmp_config();
        let loaded = Config::load(&path);
        assert!(!loaded.existed);
        assert_eq!(loaded.config.schema, CONFIG_SCHEMA);
        assert!(!path.exists(), "读取不得创建文件");
    }

    #[test]
    fn defaults_match_spec_table() {
        let c = Config::default();
        assert_eq!(c.appearance.color_scheme, ColorScheme::System);
        assert_eq!(c.appearance.icon_size, IconSize::Medium);
        assert!(c.appearance.animations);
        assert!(!c.network.proxy_enabled);
        assert_eq!(c.network.timeout_secs, 30);
        assert_eq!(c.sources.aur_helper, AurHelperChoice::Auto);
        assert_eq!(c.sources.flatpak_remote, "flathub");
        assert_eq!(c.sources.flatpak_installation, "system");
        assert_eq!(c.cache.max_size_mb, 500);
        assert!(!c.translation.auto_translate);
        assert!(c.update.check_on_startup);
        assert_eq!(c.update.check_interval_hours, 6);
        assert_eq!(c.ui.window_width, 1100);
        assert_eq!(c.ui.window_height, 720);
    }

    #[test]
    fn save_and_load_roundtrip_has_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, path) = tmp_config();
        let mut c = Config::default();
        c.appearance.color_scheme = ColorScheme::Dark;
        c.cache.max_size_mb = 800;
        c.save(&path).expect("save");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let loaded = Config::load(&path);
        assert_eq!(loaded.config.appearance.color_scheme, ColorScheme::Dark);
        assert_eq!(loaded.config.cache.max_size_mb, 800);
    }

    #[test]
    fn unknown_fields_are_preserved_across_roundtrip() {
        let (_d, path) = tmp_config();
        let raw = r#"
schema = 1
future_top_level = "keep me"
note = 42

[appearance]
color_scheme = "dark"
future_appearance = true

[future_section]
a = 1
b = "two"
"#;
        std::fs::create_dir_all(path.parent().expect("p")).expect("mkdir");
        std::fs::write(&path, raw).expect("write");
        let loaded = Config::load(&path);
        assert_eq!(loaded.config.appearance.color_scheme, ColorScheme::Dark);
        loaded.config.save(&path).expect("save");

        let text = std::fs::read_to_string(&path).expect("read");
        let doc: toml::Table = text.parse().expect("parse");
        assert_eq!(
            doc.get("future_top_level").and_then(|v| v.as_str()),
            Some("keep me")
        );
        assert_eq!(doc.get("note").and_then(|v| v.as_integer()), Some(42));
        assert_eq!(
            doc.get("appearance")
                .and_then(|v| v.get("future_appearance"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            doc.get("future_section")
                .and_then(|v| v.get("b"))
                .and_then(|v| v.as_str()),
            Some("two")
        );
        // 标量必须出现在表之前，否则 TOML 非法
        let schema_pos = text.find("schema =").expect("schema present");
        let first_table = text.find('[').expect("has tables");
        assert!(schema_pos < first_table, "schema 必须位于所有表之前");
    }

    #[test]
    fn corrupt_config_is_backed_up_and_reset() {
        let (_d, path) = tmp_config();
        std::fs::create_dir_all(path.parent().expect("p")).expect("mkdir");
        std::fs::write(&path, "this is not = = toml [").expect("write");
        let loaded = Config::load(&path);
        assert!(loaded.notice.is_some());
        let backup = loaded.backup.expect("backup path");
        assert!(backup.exists());
        assert!(backup.to_string_lossy().contains("config.toml.bak."));
        assert_eq!(loaded.config, Config::default());
        assert!(path.exists(), "绝不静默删除用户配置");
    }

    #[test]
    fn wrong_typed_field_is_reset_not_panicking() {
        let (_d, path) = tmp_config();
        std::fs::create_dir_all(path.parent().expect("p")).expect("mkdir");
        std::fs::write(&path, "[appearance]\ncolor_scheme = 5\n").expect("write");
        let loaded = Config::load(&path);
        assert!(loaded.notice.is_some());
        assert_eq!(loaded.config.appearance.color_scheme, ColorScheme::System);
    }

    #[test]
    fn unknown_enum_value_falls_back_to_default() {
        let (_d, path) = tmp_config();
        std::fs::create_dir_all(path.parent().expect("p")).expect("mkdir");
        std::fs::write(&path, "[appearance]\ncolor_scheme = \"neon\"\n").expect("write");
        let loaded = Config::load(&path);
        // 未知枚举值不是"文件损坏"（不备份、不重置），但必须告知用户该字段被忽略
        assert!(loaded.backup.is_none(), "不应备份文件");
        assert!(
            loaded.notice.expect("notice").contains("color_scheme"),
            "必须提示被忽略的字段"
        );
        assert_eq!(loaded.config.appearance.color_scheme, ColorScheme::System);
        // 同一文件里其它字段仍然生效
        assert_eq!(loaded.config.cache.max_size_mb, 500);
    }

    #[test]
    fn newer_schema_is_read_only() {
        let (_d, path) = tmp_config();
        std::fs::create_dir_all(path.parent().expect("p")).expect("mkdir");
        std::fs::write(&path, "schema = 99\n").expect("write");
        let loaded = Config::load(&path);
        assert!(loaded.read_only);
        assert!(loaded.notice.expect("notice").contains("只读"));
    }

    #[test]
    fn sanitize_clamps_out_of_range_values() {
        let mut c = Config::default();
        c.network.timeout_secs = 0;
        c.cache.max_size_mb = 1;
        c.update.check_interval_hours = 0;
        c.ui.window_width = 10;
        c.sources.flatpak_installation = "bogus".into();
        let s = c.sanitized();
        assert_eq!(s.network.timeout_secs, 5);
        assert_eq!(s.cache.max_size_mb, 16);
        assert_eq!(s.update.check_interval_hours, 1);
        assert_eq!(s.ui.window_width, 600);
        assert_eq!(s.sources.flatpak_installation, "system");
    }

    #[test]
    fn translation_defaults_to_a_working_free_provider() {
        let c = Config::default();
        assert!(!c.translation.auto_translate, "默认仍必须是关闭的（隐私）");
        assert_eq!(
            c.translation.api,
            TranslationApi::MyMemory,
            "默认服务必须开箱可用"
        );
        assert!(!c.translation.api.needs_endpoint());
        assert!(TranslationApi::LibreTranslate.needs_endpoint());
        assert!(c.translation.api.describe().contains("免费"));
    }

    #[test]
    fn mymemory_does_not_require_an_endpoint() {
        let mut c = Config::default();
        c.translation.auto_translate = true;
        c.translation.api = TranslationApi::MyMemory;
        assert!(c.validate().is_ok(), "MyMemory 是内置端点，不该要求填写");
        c.translation.api = TranslationApi::LibreTranslate;
        assert!(c.validate().is_err(), "LibreTranslate 必须提供端点");
    }

    #[test]
    fn validate_rejects_bad_proxy_and_translation() {
        let mut c = Config::default();
        c.network.proxy_enabled = true;
        c.network.proxy_url = String::new();
        assert!(c.validate().is_err());

        c.network.proxy_url = "http://127.0.0.1:8080".into();
        assert!(c.validate().is_err(), "不允许带 scheme");

        c.network.proxy_url = "127.0.0.1:8080".into();
        assert!(c.validate().is_ok());

        let mut c = Config::default();
        c.translation.auto_translate = true;
        // 默认服务现在是开箱可用的 MyMemory，必须先显式置为 None 才能触发该错误
        c.translation.api = TranslationApi::None;
        assert!(c.validate().is_err(), "启用翻译但未选服务必须报错");
        c.translation.api = TranslationApi::LibreTranslate;
        c.translation.api_endpoint = "http://evil.example".into();
        assert!(c.validate().is_err(), "非本机端点必须 https");
        c.translation.api_endpoint = "https://translate.example".into();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn user_scalar_written_before_tables_even_when_disk_has_them_after() {
        // 人为构造"标量排在表之后"的文档（TOML 要求值必须在表之前，所以只能这样构造）
        let mut disk = toml::Table::new();
        let mut appearance = toml::Table::new();
        appearance.insert(
            "color_scheme".to_string(),
            toml::Value::String("light".to_string()),
        );
        disk.insert("appearance".to_string(), toml::Value::Table(appearance));
        disk.insert(
            "zzz_scalar".to_string(),
            toml::Value::String("late".to_string()),
        );
        let merged = merge_document(&Config::default(), &disk).expect("merge");
        let text = toml::to_string_pretty(&merged).expect("serialize");
        let zzz = text.find("zzz_scalar").expect("present");
        let first_table = text.find('[').expect("has tables");
        assert!(zzz < first_table);
        // 且能被重新解析
        let _: toml::Table = text.parse().expect("reparse");
    }
}
