//! Flatpak 后端：flatpak CLI 封装与解析（project.md §4.5）。
//!
//! 实测（本机 flatpak 1.18.2）：
//! - flatpak search --columns=…,origin  -> **失败**：未知列 origin（列名是 remotes）
//! - flatpak remotes --columns=installation -> **失败**：未知列 installation
//! - flatpak list --columns=…,origin,size,installation -> 成功
//! - flatpak remote-info 没有 -j，只能解析文本
//! - flatpak -j 的 JSON 键名会被本地化（中文环境实测键名是"应用程序_id"）
//!
//! 因此：所有调用固定 LC_ALL=C，并且 JSON 按 key、纯文本按列序（两种策略互相校验）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::backend::{Capability, Category, PackageBackend, Page, SearchScope};
use crate::cache::{Cache, ttl};
use crate::error::{CoreError, CoreResult, raw_head};
use crate::flathub::{self, CollectionHit, FlathubClient};
use crate::model::plan::Installation;
use crate::model::{
    DepKind, DependencyInfo, IconRef, Installed, PackageDetail, PackageId, PackageSummary,
};
use crate::net::{CancelToken, HttpClient};

/// flatpak 子进程超时（搜索/列表在慢盘上可能较久）。
pub const FLATPAK_TIMEOUT: Duration = Duration::from_secs(30);
/// 离线解析 appstream.xml 时保留的组件上限（内存上限保护）。
pub const MAX_LOCAL_COMPONENTS: usize = 4000;

/// 统一构造 flatpak 子进程。
///
/// LC_ALL=C 是硬性要求：否则 -j 的 JSON 键名会被本地化，解析会随机失败。
pub fn flatpak_cmd(args: &[&str]) -> tokio::process::Command {
    let mut c = tokio::process::Command::new("flatpak");
    c.env("LC_ALL", "C")
        .env("LANG", "C")
        // 避免用户环境注入的模块影响输出
        .env_remove("GIO_EXTRA_MODULES")
        .args(args)
        .stdin(std::process::Stdio::null());
    c
}

/// 执行 flatpak 并返回 stdout。失败时携带原始前 20 行。
pub async fn flatpak(args: &[&str]) -> CoreResult<String> {
    let mut cmd = flatpak_cmd(args);
    let joined = format!("flatpak {}", args.join(" "));
    let output = tokio::time::timeout(FLATPAK_TIMEOUT, cmd.output())
        .await
        .map_err(|_| CoreError::Timeout {
            url: joined.clone(),
            secs: FLATPAK_TIMEOUT.as_secs(),
        })?
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CoreError::BackendUnavailable {
                    kind: "flatpak".into(),
                    reason: "未安装 flatpak（可选依赖）".into(),
                }
            } else {
                CoreError::Io(format!("{joined}：{e}"))
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(command = %joined, stderr = %raw_head(&stderr, 20), "flatpak 命令失败");
        return Err(CoreError::Parse {
            context: joined,
            raw_head: raw_head(&stderr, 20),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// 判断 flatpak 是否可用（只读）。
pub fn detect() -> Option<String> {
    let out = std::process::Command::new("flatpak")
        .arg("--version")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// 解析 flatpak remotes --columns=name 的结果（按安装位置区分）。
pub fn parse_remotes(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(name) = line.split_whitespace().next()
            && !name.is_empty()
            && !out.iter().any(|x| x == name)
        {
            out.push(name.to_string());
        }
    }
    out
}

/// JSON 行解析：按列名取值，带别名与位置兜底。
///
/// 位置兜底需要 serde_json 保持键插入顺序（preserve_order feature），
/// flatpak 输出的键顺序与 --columns 一致。
fn json_column<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    column: &str,
    index: usize,
) -> Option<&'a serde_json::Value> {
    if let Some(v) = obj.get(column) {
        return Some(v);
    }
    // 已知别名：list 的 size 列在 JSON 中叫 installed_size
    let alias = match column {
        "size" => Some("installed_size"),
        "remotes" => Some("remote"),
        "origin" => Some("remote"),
        _ => None,
    };
    if let Some(a) = alias
        && let Some(v) = obj.get(a)
    {
        return Some(v);
    }
    // 本地化兜底：中文 locale 下 application 列的键是"应用程序_id"
    if column == "application"
        && let Some((_, v)) = obj.iter().find(|(k, _)| k.ends_with("_id"))
    {
        return Some(v);
    }
    // 位置兜底（键顺序与 --columns 一致）
    obj.iter().nth(index).map(|(_, v)| v)
}

fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 把 flatpak -j 输出解析成按列排列的字符串矩阵。
pub fn parse_json_rows(text: &str, columns: &[&str]) -> CoreResult<Vec<Vec<String>>> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| CoreError::Parse {
        context: "flatpak JSON".to_string(),
        raw_head: format!("{e} / {}", raw_head(text, 20)),
    })?;
    let arr = value.as_array().ok_or_else(|| CoreError::Parse {
        context: "flatpak JSON（顶层不是数组）".to_string(),
        raw_head: raw_head(text, 20),
    })?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let obj = item.as_object().ok_or_else(|| CoreError::Parse {
            context: "flatpak JSON（元素不是对象）".to_string(),
            raw_head: raw_head(text, 20),
        })?;
        let mut row = Vec::with_capacity(columns.len());
        for (i, col) in columns.iter().enumerate() {
            row.push(
                json_column(obj, col, i)
                    .map(value_to_string)
                    .unwrap_or_default(),
            );
        }
        out.push(row);
    }
    Ok(out)
}

/// 把 --columns 的纯文本输出（制表符分隔）按列序解析。
pub fn parse_plain_rows(text: &str, columns: usize) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut row: Vec<String> = line.split('\t').map(|s| s.trim().to_string()).collect();
        row.resize(columns, String::new());
        out.push(row);
    }
    out
}

/// 一个 Flatpak 应用（已安装或搜索结果）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlatpakApp {
    pub app_id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub remote: String,
    pub installed_size: Option<u64>,
    pub installation: Option<Installation>,
}

impl FlatpakApp {
    fn from_row(row: &[String], columns: &[&str]) -> Option<Self> {
        let get = |want: &str| -> String {
            columns
                .iter()
                .position(|c| *c == want)
                .and_then(|i| row.get(i))
                .cloned()
                .unwrap_or_default()
        };
        let app_id = get("application");
        if app_id.is_empty() {
            return None;
        }
        let name = {
            let n = get("name");
            if n.is_empty() { app_id.clone() } else { n }
        };
        let remote = {
            let r = get("remotes");
            if r.is_empty() { get("origin") } else { r }
        };
        Some(Self {
            app_id,
            name,
            description: get("description"),
            version: get("version"),
            remote,
            installed_size: flathub::parse_size(&get("size")),
            installation: match get("installation").as_str() {
                "system" => Some(Installation::System),
                "user" => Some(Installation::User),
                _ => None,
            },
        })
    }

    /// 转成统一摘要。
    pub fn to_summary(&self, installed: Installed) -> PackageSummary {
        let remote = if self.remote.is_empty() {
            "flathub".to_string()
        } else {
            self.remote.clone()
        };
        let mut s = PackageSummary::minimal(
            PackageId::flatpak(remote, self.app_id.clone()),
            self.name.clone(),
        );
        s.set_summary(&self.description);
        s.version = (!self.version.is_empty()).then(|| self.version.clone());
        s.installed = installed;
        // Flathub 的图标 URL 是可预测的（见 flathub::icon_url_for），
        // 因此搜索结果与分类列表不必先请求 appstream 也能显示图标
        s.icon = match flathub::icon_url_for(&self.app_id) {
            Some(url) => IconRef::Remote(url),
            None => IconRef::IconName(self.app_id.clone()),
        };
        s
    }
}

/// 由 Flathub 集合命中构造摘要（纯函数：不含"是否已安装"的本地查询）。
///
/// 图标 URL 是集合响应自带的字段（CollectionHit::icon_url，只接受 https）。
/// 首页"推荐"走的就是这条路径；一旦有人改成自己构造摘要而忘了 icon，
/// collection_hit_summary_keeps_remote_icon 会立刻失败。
pub fn summary_from_hit(remote: &str, hit: &CollectionHit) -> PackageSummary {
    let mut s = PackageSummary::minimal(
        PackageId::flatpak(remote.to_string(), hit.app_id.clone()),
        if hit.name.is_empty() {
            hit.app_id.clone()
        } else {
            hit.name.clone()
        },
    );
    s.set_summary(&hit.summary);
    s.icon = match hit.icon_url() {
        Some(url) => IconRef::Remote(url.to_string()),
        None => IconRef::IconName(hit.app_id.clone()),
    };
    s
}

/// flatpak remote-info 的文本解析结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteInfo {
    pub id: String,
    pub ref_: String,
    pub arch: String,
    pub branch: String,
    pub version: String,
    pub license: String,
    pub download_size: Option<u64>,
    pub installed_size: Option<u64>,
    pub runtime: Option<String>,
    pub sdk: Option<String>,
    pub commit: Option<String>,
    pub date: Option<String>,
    pub subject: Option<String>,
}

/// 标签白名单：C locale 下是英文，仍做"两种语言都试"的兜底。
const REMOTE_INFO_LABELS: [(&str, &[&str]); 13] = [
    ("id", &["ID", "标识"]),
    ("ref", &["Ref", "引用"]),
    ("arch", &["Arch", "架构"]),
    ("branch", &["Branch", "分支"]),
    ("version", &["Version", "版本"]),
    ("license", &["License", "许可证"]),
    ("download_size", &["Download Size", "下载大小"]),
    ("installed_size", &["Installed Size", "安装大小"]),
    ("runtime", &["Runtime", "运行时"]),
    ("sdk", &["Sdk", "SDK"]),
    ("commit", &["Commit", "提交"]),
    ("date", &["Date", "日期"]),
    ("subject", &["Subject", "主题"]),
];

/// 逐行「键：值」解析 remote-info 输出，只提取白名单字段。
pub fn parse_remote_info(text: &str) -> RemoteInfo {
    let mut info = RemoteInfo::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let Some((field, _)) = REMOTE_INFO_LABELS
            .iter()
            .find(|(_, labels)| labels.iter().any(|l| l.eq_ignore_ascii_case(key)))
        else {
            continue;
        };
        match *field {
            "id" => info.id = value.to_string(),
            "ref" => info.ref_ = value.to_string(),
            "arch" => info.arch = value.to_string(),
            "branch" => info.branch = value.to_string(),
            "version" => info.version = value.to_string(),
            "license" => info.license = value.to_string(),
            "download_size" => info.download_size = flathub::parse_size(value),
            "installed_size" => info.installed_size = flathub::parse_size(value),
            "runtime" => info.runtime = Some(value.to_string()),
            "sdk" => info.sdk = Some(value.to_string()),
            "commit" => info.commit = Some(value.to_string()),
            "date" => info.date = Some(value.to_string()),
            "subject" => info.subject = Some(value.to_string()),
            _ => {}
        }
    }
    info
}

/// 从 ref 中解析应用 id（形如 app/org.mozilla.firefox/x86_64/stable）。
pub fn app_id_from_ref(ref_: &str) -> Option<String> {
    let mut parts = ref_.split('/');
    let kind = parts.next()?;
    if kind != "app" && kind != "runtime" {
        return None;
    }
    parts.next().map(|s| s.to_string())
}

/// 本地 appstream 缓存（离线优先，§4.6）。
///
/// **禁止在启动时解析该文件**：仅在用户打开"Flatpak 分类页"且网络不可用时，
/// 按需流式解析，且必须有内存上限保护。
#[derive(Debug, Clone)]
pub struct LocalAppstream {
    pub path: PathBuf,
}

impl LocalAppstream {
    /// 定位某个远程仓库的 appstream.xml（优先 system，其次 user）。
    pub fn locate(remote: &str) -> Option<Self> {
        if remote.is_empty()
            || !remote
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return None;
        }
        let arch = std::env::consts::ARCH;
        if let Some(home) = std::env::var_os("HOME") {
            let base = Path::new(&home)
                .join(".local/share/flatpak/appstream")
                .join(remote)
                .join(arch);
            if let Ok(entries) = std::fs::read_dir(&base) {
                for e in entries.flatten() {
                    let p = e.path().join("appstream.xml");
                    if p.is_file() {
                        return Some(Self { path: p });
                    }
                }
            }
        }
        let candidates = [
            PathBuf::from(format!(
                "/var/lib/flatpak/appstream/{remote}/{arch}/active/appstream.xml"
            )),
            PathBuf::from(format!(
                "/var/lib/flatpak/appstream/{remote}/{arch}/appstream.xml"
            )),
        ];
        candidates
            .into_iter()
            .find(|p| p.is_file())
            .map(|path| Self { path })
    }

    /// 流式解析出某个分类下的组件（带组件数量上限）。
    ///
    /// 只保留 id/name/summary/icon/categories 五个字段，
    /// 避免 49 MB 级别（实测本机 49 MB）的文件把内存吃光。
    pub fn components_in_category(
        &self,
        category: &str,
        limit: usize,
    ) -> CoreResult<Vec<LocalComponent>> {
        use quick_xml::events::Event;

        let file = std::fs::File::open(&self.path)
            .map_err(|e| CoreError::Io(format!("{}：{e}", self.path.display())))?;
        let mut reader = quick_xml::Reader::from_reader(std::io::BufReader::new(file));
        {
            let config = reader.config_mut();
            config.trim_text(true);
            config.check_end_names = false;
        }

        let category_lc = category.to_ascii_lowercase();
        let mut buf = Vec::new();
        let mut out: Vec<LocalComponent> = Vec::new();
        let mut current: Option<LocalComponent> = None;
        let mut in_component = false;
        let mut text_target: Option<&'static str> = None;
        let mut in_category = false;
        let mut scanned = 0usize;

        loop {
            let event = reader
                .read_event_into(&mut buf)
                .map_err(|e| CoreError::Parse {
                    context: format!("appstream.xml（{}）", self.path.display()),
                    raw_head: e.to_string(),
                })?;
            match event {
                Event::Start(e) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    match name.as_str() {
                        "component" => {
                            in_component = true;
                            current = Some(LocalComponent::default());
                        }
                        "id" if in_component => text_target = Some("id"),
                        "name" if in_component => text_target = Some("name"),
                        "summary" if in_component => text_target = Some("summary"),
                        "icon" if in_component => text_target = Some("icon"),
                        "category" if in_component => {
                            in_category = true;
                            text_target = Some("category");
                        }
                        _ => {}
                    }
                }
                Event::Text(t) => {
                    let target = text_target.take();
                    if let (Some(target), Some(cur)) = (target, current.as_mut()) {
                        let value = t
                            .decode()
                            .map(|s| s.into_owned())
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        if !value.is_empty() {
                            match target {
                                "id" => cur.id = value,
                                "name" => {
                                    if cur.name.is_empty() {
                                        cur.name = value;
                                    }
                                }
                                "summary" => {
                                    if cur.summary.is_empty() {
                                        cur.summary = value;
                                    }
                                }
                                "icon" => {
                                    if cur.icon.is_none() {
                                        cur.icon = Some(value);
                                    }
                                }
                                "category" => {
                                    if value.to_ascii_lowercase().contains(&category_lc) {
                                        cur.category_hit = true;
                                    }
                                    cur.categories.push(value);
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Event::CData(t) => {
                    let target = text_target.take();
                    if let (Some(target), Some(cur)) = (target, current.as_mut()) {
                        let value = String::from_utf8_lossy(t.as_ref()).trim().to_string();
                        match target {
                            "id" => cur.id = value,
                            "name" if cur.name.is_empty() => cur.name = value,
                            "summary" if cur.summary.is_empty() => cur.summary = value,
                            _ => {}
                        }
                    }
                }
                Event::End(e) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    match name.as_str() {
                        "category" => in_category = false,
                        "component" => {
                            in_component = false;
                            scanned += 1;
                            if let Some(cur) = current.take()
                                && cur.category_hit
                                && !cur.id.is_empty()
                            {
                                out.push(cur);
                                if out.len() >= limit {
                                    break;
                                }
                            }
                            if scanned >= MAX_LOCAL_COMPONENTS {
                                tracing::debug!(
                                    scanned,
                                    found = out.len(),
                                    "appstream 扫描达到组件上限，提前结束"
                                );
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                Event::Eof => break,
                _ => {}
            }
            let _ = in_category;
            buf.clear();
        }

        out.truncate(limit);
        Ok(out)
    }

    /// 解析图标缓存目录下的实际文件路径。
    pub fn icon_path(&self, icon_name: &str) -> Option<PathBuf> {
        if icon_name.contains('/') || icon_name.contains("..") {
            return None;
        }
        let dir = self.path.parent()?;
        for size in ["128x128", "64x64", "256x256"] {
            let p = dir.join("icons").join(size).join(icon_name);
            if p.is_file() {
                return Some(p);
            }
        }
        None
    }
}

/// 本地 appstream 中的一个组件（只保留展示需要的字段）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalComponent {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub icon: Option<String>,
    pub categories: Vec<String>,
    category_hit: bool,
}

/// Flatpak 后端。
pub struct FlatpakBackend {
    cache: Arc<Cache>,
    flathub: FlathubClient,
    remote: String,
    installation: Installation,
    capability: Capability,
    /// 已安装应用（内存索引，由 installed() 刷新）
    installed: tokio::sync::RwLock<HashMap<String, FlatpakApp>>,
}

impl std::fmt::Debug for FlatpakBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlatpakBackend")
            .field("remote", &self.remote)
            .field("capability", &self.capability)
            .finish_non_exhaustive()
    }
}

impl FlatpakBackend {
    /// 构造后端；flatpak 不存在时 capability 置为不可用。
    pub async fn new(
        http: Arc<HttpClient>,
        cache: Arc<Cache>,
        cfg: &crate::config::Config,
    ) -> Arc<Self> {
        let flathub = FlathubClient::new(Arc::clone(&http), Arc::clone(&cache));
        let remote = cfg.sources.flatpak_remote.clone();
        let installation = match cfg.sources.flatpak_installation.as_str() {
            "user" => Installation::User,
            _ => Installation::System,
        };
        let capability = if !cfg.sources.flatpak_enabled {
            Capability::unavailable("已在设置中关闭 Flatpak 源")
        } else {
            match detect() {
                None => Capability::unavailable(
                    "未安装 flatpak（可选依赖）。安装后即可管理 Flatpak 应用：sudo pacman -S flatpak",
                ),
                Some(_) => {
                    let remotes = Self::remotes().await.unwrap_or_default();
                    if remotes.is_empty() {
                        Capability::available_with(Some(
                            "尚未配置任何 Flatpak 远程仓库，请先添加 flathub".into(),
                        ))
                    } else if !remotes.iter().any(|r| r == &remote) {
                        Capability::available_with(Some(format!(
                            "配置的远程仓库 {remote} 不存在（可用：{}）",
                            remotes.join("、")
                        )))
                    } else {
                        Capability::available()
                    }
                }
            }
        };
        Arc::new(Self {
            cache,
            flathub,
            remote,
            installation,
            capability,
            installed: tokio::sync::RwLock::new(HashMap::new()),
        })
    }

    /// 列出远程仓库名（system + user）。
    pub async fn remotes() -> CoreResult<Vec<String>> {
        let mut out: Vec<String> = Vec::new();
        for flag in ["--system", "--user"] {
            if let Ok(text) = flatpak(&["remotes", flag, "--columns=name"]).await {
                for name in parse_remotes(&text) {
                    if !out.contains(&name) {
                        out.push(name);
                    }
                }
            }
        }
        if out.is_empty()
            && let Ok(text) = flatpak(&["remotes", "--columns=name"]).await
        {
            out = parse_remotes(&text);
        }
        Ok(out)
    }

    pub fn remote(&self) -> &str {
        &self.remote
    }

    /// Flathub 元数据客户端（图标下载、评分、安全公告共用）。
    pub fn flathub(&self) -> &FlathubClient {
        &self.flathub
    }

    /// 下载图标到缓存目录。
    pub async fn download_icon(
        &self,
        url: &str,
        cancel: &CancelToken,
    ) -> CoreResult<std::path::PathBuf> {
        self.flathub.download_icon(url, cancel).await
    }

    pub fn installation(&self) -> Installation {
        self.installation
    }

    /// 已安装应用列表（不入缓存：每次查询本地，保证状态永远真实）。
    pub async fn list_installed(&self) -> CoreResult<Vec<FlatpakApp>> {
        const COLUMNS: &[&str] = &[
            "application",
            "name",
            "version",
            "origin",
            "size",
            "installation",
        ];
        let cols = COLUMNS.join(",");
        let json_cols = format!("--columns={cols}");
        let json_args = ["list", "--app", "-j", json_cols.as_str()];
        let plain_args = ["list", "--app", json_cols.as_str()];

        let rows = match flatpak(&json_args).await {
            Ok(text) => match parse_json_rows(&text, COLUMNS) {
                Ok(rows) => rows,
                Err(e) => {
                    tracing::warn!(error = %e, "-j 解析失败，改用列序解析");
                    let text = flatpak(&plain_args).await?;
                    parse_plain_rows(&text, COLUMNS.len())
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "flatpak list -j 失败，改用列序解析");
                let text = flatpak(&plain_args).await?;
                parse_plain_rows(&text, COLUMNS.len())
            }
        };

        let apps: Vec<FlatpakApp> = rows
            .iter()
            .filter_map(|r| FlatpakApp::from_row(r, COLUMNS))
            .collect();
        let mut guard = self.installed.write().await;
        *guard = apps
            .iter()
            .cloned()
            .map(|a| (a.app_id.clone(), a))
            .collect();
        Ok(apps)
    }

    /// 已安装状态查询（内存索引）。
    async fn installed_state(&self, app_id: &str) -> Installed {
        let guard = self.installed.read().await;
        match guard.get(app_id) {
            Some(app) => Installed::Yes {
                version: app.version.clone(),
                explicit: true,
            },
            None => Installed::No,
        }
    }

    /// flatpak remote-info（文本解析，无 -j）。
    pub async fn remote_info(&self, app_id: &str) -> CoreResult<RemoteInfo> {
        let text = flatpak(&["remote-info", &self.remote, app_id]).await?;
        let info = parse_remote_info(&text);
        if info.id.is_empty() && info.ref_.is_empty() && info.version.is_empty() {
            return Err(CoreError::Parse {
                context: format!("flatpak remote-info {app_id}"),
                raw_head: raw_head(&text, 20),
            });
        }
        Ok(info)
    }

    /// 从 Flathub 集合结果构造摘要。
    async fn hit_to_summary(&self, hit: &CollectionHit) -> PackageSummary {
        let mut s = summary_from_hit(&self.remote, hit);
        s.installed = self.installed_state(&hit.app_id).await;
        s
    }

    /// 一页集合命中 -> 摘要列表（首页"推荐"、Flatpak 分类与搜索共用）。
    ///
    /// **不要绕过这个函数自己构造 PackageSummary**：集合响应里的图标 URL 只有
    /// 在这里才会被搬进摘要，漏掉它界面上就是一排字母头像（首页曾经的实测缺陷）。
    pub async fn hits_to_summaries(&self, hits: &[CollectionHit]) -> Vec<PackageSummary> {
        let mut out = Vec::with_capacity(hits.len());
        for hit in hits {
            out.push(self.hit_to_summary(hit).await);
        }
        out
    }

    /// 离线兜底：从本地 appstream.xml 中按分类查找（在阻塞线程池中执行）。
    async fn offline_category(
        &self,
        category: &str,
        page: Page,
    ) -> CoreResult<Vec<PackageSummary>> {
        let remote = self.remote.clone();
        let cat = category.to_string();
        let want = page.offset.saturating_add(page.limit).max(page.limit);
        let components = tokio::task::spawn_blocking(move || -> CoreResult<Vec<LocalComponent>> {
            let Some(local) = LocalAppstream::locate(&remote) else {
                return Err(CoreError::Unsupported(
                    "网络不可用，且本机没有 Flatpak 的 appstream 缓存".into(),
                ));
            };
            local.components_in_category(&cat, want)
        })
        .await
        .map_err(|e| CoreError::Internal(format!("appstream 解析任务失败：{e}")))??;

        let mut out = Vec::with_capacity(components.len());
        for c in &components {
            let installed = self.installed_state(&c.id).await;
            let mut s = PackageSummary::minimal(
                PackageId::flatpak(self.remote.clone(), c.id.clone()),
                if c.name.is_empty() {
                    c.id.clone()
                } else {
                    c.name.clone()
                },
            );
            s.set_summary(&c.summary);
            s.installed = installed;
            s.icon = IconRef::IconName(c.id.clone());
            out.push(s);
        }
        Ok(page.slice(&out).to_vec())
    }
}

/// 分类 id 前缀，避免与 pacman 的分类冲突。
pub const CATEGORY_PREFIX: &str = "flathub:";

#[async_trait]
impl PackageBackend for FlatpakBackend {
    fn source_kind(&self) -> &'static str {
        "flatpak"
    }

    fn capability(&self) -> &Capability {
        &self.capability
    }

    async fn search(
        &self,
        query: &str,
        scope: SearchScope,
    ) -> Result<Vec<PackageSummary>, CoreError> {
        if !self.capability.is_available() {
            return Err(CoreError::BackendUnavailable {
                kind: "flatpak".into(),
                reason: self
                    .capability
                    .reason
                    .clone()
                    .unwrap_or_else(|| "flatpak 不可用".into()),
            });
        }
        if scope == SearchScope::LocalOnly {
            // 只搜已安装应用（本地、不联网）
            let apps = self.list_installed().await?;
            let q = query.to_lowercase();
            let mut out = Vec::new();
            for app in apps {
                if app.name.to_lowercase().contains(&q) || app.app_id.to_lowercase().contains(&q) {
                    out.push(app.to_summary(Installed::Yes {
                        version: app.version.clone(),
                        explicit: true,
                    }));
                }
            }
            return Ok(out);
        }
        const COLUMNS: &[&str] = &["application", "name", "description", "version", "remotes"];
        let cols = COLUMNS.join(",");
        let json_cols = format!("--columns={cols}");
        let json_args = ["search", "-j", json_cols.as_str(), query];
        let plain_args = ["search", json_cols.as_str(), query];

        let rows = match flatpak(&json_args).await {
            Ok(text) => match parse_json_rows(&text, COLUMNS) {
                Ok(rows) => rows,
                Err(e) => {
                    tracing::warn!(error = %e, "flatpak search -j 解析失败，改用列序解析");
                    let text = flatpak(&plain_args).await?;
                    parse_plain_rows(&text, COLUMNS.len())
                }
            },
            Err(CoreError::Parse { .. }) => {
                let text = flatpak(&plain_args).await?;
                parse_plain_rows(&text, COLUMNS.len())
            }
            Err(e) => return Err(e),
        };

        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let Some(app) = FlatpakApp::from_row(row, COLUMNS) else {
                continue;
            };
            let installed = self.installed_state(&app.app_id).await;
            out.push(app.to_summary(installed));
        }
        Ok(out)
    }

    async fn info(&self, id: &PackageId) -> Result<PackageDetail, CoreError> {
        let app_id = id.name.clone();
        let cancel = CancelToken::new();
        let installed = self.installed_state(&app_id).await;

        // 本机数据先填（不阻塞），网络字段后填
        let mut detail =
            PackageDetail::from_summary(PackageSummary::minimal(id.clone(), app_id.clone()));
        detail.summary.installed = installed;

        let mut online_error: Option<CoreError> = None;
        match self.flathub.appstream(&app_id, &cancel).await {
            Ok(a) => {
                detail.summary.display_name = if a.name.is_empty() {
                    app_id.clone()
                } else {
                    a.name.clone()
                };
                detail.summary.set_summary(&a.summary);
                detail.description = a.description_text();
                if !a.project_license.is_empty() {
                    detail.licenses.push(a.project_license.clone());
                }
                detail.maintainer =
                    (!a.developer_name.is_empty()).then(|| a.developer_name.clone());
                detail.screenshots = a.screenshot_urls();
                if let Some(icon) = a.icon_url() {
                    detail.summary.icon = IconRef::Remote(icon.to_string());
                }
                detail.summary.out_of_date = a.is_eol;
                if let Some(r) = a.latest_release() {
                    detail.summary.version = Some(r.version.clone());
                }
                if !a.categories.is_empty() {
                    detail.extra.push("分类", a.categories.join("、"));
                }
                if !a.keywords.is_empty() {
                    detail.extra.push("关键词", a.keywords.join("、"));
                }
                detail
                    .extra
                    .push("Flathub 页面", format!("https://flathub.org/apps/{app_id}"));
            }
            Err(e) => {
                tracing::info!(app_id, error = %e, "Flathub 元数据不可用，降级到本机信息");
                online_error = Some(e);
            }
        }

        match self.flathub.summary(&app_id, &cancel).await {
            Ok(s) => {
                detail.download_size = (s.download_size > 0).then_some(s.download_size);
                detail.installed_size = (s.installed_size > 0).then_some(s.installed_size);
                detail.permissions = s.metadata.permissions.describe();
                if let Some(rt) = &s.metadata.runtime {
                    let mut dep = DependencyInfo::from_expr(rt, DepKind::RuntimeRef);
                    dep.missing = false;
                    dep.satisfied_by = Some(PackageId::flatpak(self.remote.clone(), rt.clone()));
                    detail.dependencies.push(dep);
                    detail.extra.push("运行时", rt.clone());
                }
                if let Some(name) = &s.metadata.runtime_name {
                    detail.extra.push("运行时名称", name.clone());
                }
                if s.metadata.runtime_is_eol {
                    detail.extra.push("提示", "所用运行时已停止维护（EOL）");
                }
                for ext in s.metadata.extensions.keys() {
                    let mut dep = DependencyInfo::from_expr(ext, DepKind::Extension);
                    dep.missing = false;
                    detail.dependencies.push(dep);
                }
                if !s.arches.is_empty() {
                    detail.extra.push("架构", s.arches.join("、"));
                }
                if !s.branch.is_empty() {
                    detail.extra.push("分支", s.branch.clone());
                }
            }
            Err(e) => {
                tracing::info!(app_id, error = %e, "Flathub summary 不可用");
                online_error = online_error.or(Some(e));
            }
        }

        // 评分：3 秒超时，失败静默
        if let Some(r) = self.flathub.ratings(&app_id, &cancel).await {
            detail.rating = Some(r.stars);
            detail.review_count = Some(r.total);
        }

        // 本机已安装信息
        {
            let guard = self.installed.read().await;
            if let Some(app) = guard.get(&app_id) {
                detail.summary.installed = Installed::Yes {
                    version: app.version.clone(),
                    explicit: true,
                };
                if detail.summary.version.is_none() {
                    detail.summary.version = Some(app.version.clone());
                }
                detail.installed_size = detail.installed_size.or(app.installed_size);
                detail.extra.push(
                    "安装位置",
                    app.installation
                        .unwrap_or(self.installation)
                        .as_str()
                        .to_string(),
                );
            }
        }

        if detail.summary.version.is_none()
            || detail.download_size.is_none()
            || detail.licenses.is_empty()
        {
            // 用 remote-info（读取本地缓存，通常不需要网络）补齐
            if let Ok(info) = self.remote_info(&app_id).await {
                if detail.summary.version.is_none() && !info.version.is_empty() {
                    detail.summary.version = Some(info.version.clone());
                }
                detail.download_size = detail.download_size.or(info.download_size);
                detail.installed_size = detail.installed_size.or(info.installed_size);
                if detail.licenses.is_empty() && !info.license.is_empty() {
                    detail.licenses.push(info.license.clone());
                }
                if let Some(c) = &info.commit {
                    detail.extra.push("提交", c.clone());
                }
                if let Some(d) = &info.date {
                    detail.extra.push("发布日期", d.clone());
                }
            }
        }

        detail.extra.push("应用 ID", app_id.clone());

        // 完全无法取得任何字段时才报错（"缺失字段显示不可用，而不是整页失败"）
        if detail.summary.display_name == app_id
            && detail.description.is_empty()
            && detail.summary.version.is_none()
            && !detail.summary.installed.is_yes()
            && let Some(e) = online_error
        {
            return Err(e);
        }
        Ok(detail)
    }

    async fn installed(&self) -> Result<Vec<PackageSummary>, CoreError> {
        let apps = self.list_installed().await?;
        Ok(apps
            .into_iter()
            .map(|a| {
                let installed = Installed::Yes {
                    version: a.version.clone(),
                    explicit: true,
                };
                a.to_summary(installed)
            })
            .collect())
    }

    async fn upgradable(&self) -> Result<Vec<PackageSummary>, CoreError> {
        let apps = self.list_installed().await?;
        let mut out = Vec::new();
        for app in apps {
            // 与远端缓存版本比对（结果缓存 30 分钟，避免每次进入更新页都请求）
            let key = format!("upgradable:{}", app.app_id);
            let candidate = match self.cache.get::<String>("flatpak", &key, false).await {
                Some(hit) => Some(hit.value),
                None => match self.remote_info(&app.app_id).await {
                    Ok(info) if !info.version.is_empty() => {
                        let _ = self
                            .cache
                            .put("flatpak", &key, &info.version, ttl::FLATPAK_UPDATES)
                            .await;
                        Some(info.version)
                    }
                    _ => None,
                },
            };
            let Some(candidate) = candidate else {
                continue;
            };
            if candidate == app.version || app.version.is_empty() {
                continue;
            }
            let mut s = app.to_summary(Installed::Yes {
                version: app.version.clone(),
                explicit: true,
            });
            s.update = Some(crate::model::UpdateInfo {
                current: app.version.clone(),
                candidate: candidate.clone(),
                download_size: None,
            });
            s.version = Some(candidate);
            out.push(s);
        }
        out.sort_by(|a, b| a.id.name.cmp(&b.id.name));
        Ok(out)
    }

    async fn categories(&self) -> Result<Vec<Category>, CoreError> {
        Ok(flathub::FLATHUB_CATEGORIES
            .iter()
            .map(|(id, display)| {
                Category::new(format!("{CATEGORY_PREFIX}{id}"), *display, "flatpak")
            })
            .collect())
    }

    async fn list_category(
        &self,
        category: &str,
        page: Page,
    ) -> Result<Vec<PackageSummary>, CoreError> {
        let Some(id) = category.strip_prefix(CATEGORY_PREFIX) else {
            return Err(CoreError::Unsupported(format!(
                "未知的 Flatpak 分类：{category}"
            )));
        };
        let cancel = CancelToken::new();
        let per_page = page.limit.clamp(10, 250) as u32;
        // limit 为 0 时视为第一页，同时避免除零
        let page_no = page
            .offset
            .checked_div(page.limit)
            .map(|n| n as u32 + 1)
            .unwrap_or(1);
        match self
            .flathub
            .category_page(id, page_no, per_page, &cancel)
            .await
        {
            Ok(result) => {
                let mut out = Vec::with_capacity(result.hits.len());
                for hit in &result.hits {
                    out.push(self.hit_to_summary(hit).await);
                }
                Ok(out)
            }
            Err(e) => {
                tracing::info!(category = id, error = %e, "分类请求失败，尝试离线 appstream");
                self.offline_category(id, page).await
            }
        }
    }

    async fn dependencies(&self, id: &PackageId) -> Result<Vec<DependencyInfo>, CoreError> {
        let cancel = CancelToken::new();
        let s = self.flathub.summary(&id.name, &cancel).await?;
        let mut out = Vec::new();
        if let Some(rt) = &s.metadata.runtime {
            let mut dep = DependencyInfo::from_expr(rt, DepKind::RuntimeRef);
            dep.missing = false;
            out.push(dep);
        }
        for ext in s.metadata.extensions.keys() {
            let mut dep = DependencyInfo::from_expr(ext, DepKind::Extension);
            dep.missing = false;
            out.push(dep);
        }
        Ok(out)
    }

    async fn reverse_dependencies(&self, _id: &PackageId) -> Result<Vec<PackageId>, CoreError> {
        // Flatpak 没有反向依赖概念（每个应用自带运行时）
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_rows_reads_c_locale_keys() {
        // 实测的 LC_ALL=C 输出（search）
        let json = r#"[
          {"application_id":"org.mozilla.firefox","name":"Firefox","description":"Fast","version":"155.0.1","remotes":"flathub"},
          {"application_id":"dev.qwery.AddWater","name":"Add Water","description":"Keep Firefox in fashion","version":"1.3","remotes":"flathub"}
        ]"#;
        let cols = ["application", "name", "description", "version", "remotes"];
        let rows = parse_json_rows(json, &cols).expect("parse");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], "org.mozilla.firefox");
        assert_eq!(rows[0][1], "Firefox");
        assert_eq!(rows[0][4], "flathub");
    }

    #[test]
    fn parse_json_rows_reads_list_keys_including_size_alias() {
        // 实测的 LC_ALL=C 输出（list）：--columns=size 在 JSON 中叫 installed_size
        let json = r#"[{"application_id":"com.github.gmg137.netease-cloud-music-gtk","name":"NetEase Cloud Music Gtk4","version":"2.5.4","origin":"flathub","installed_size":"16.4 MB"}]"#;
        let cols = ["application", "name", "version", "origin", "size"];
        let rows = parse_json_rows(json, &cols).expect("parse");
        assert_eq!(rows[0][3], "flathub");
        assert_eq!(rows[0][4], "16.4 MB");
    }

    #[test]
    fn parse_json_rows_survives_localized_keys() {
        // 中文 locale 下实测的键名（说明为什么必须固定 LC_ALL=C）
        let json = r#"[{"应用程序_id":"com.github.gmg137.netease-cloud-music-gtk","名称":"网易云音乐","版本":"2.5.4"}]"#;
        let cols = ["application", "name", "version"];
        let rows = parse_json_rows(json, &cols).expect("parse");
        assert_eq!(rows[0][0], "com.github.gmg137.netease-cloud-music-gtk");
    }

    #[test]
    fn parse_json_rows_rejects_garbage() {
        let err = parse_json_rows("not json at all", &["application"]).expect_err("must fail");
        assert!(matches!(err, CoreError::Parse { .. }));
        let err = parse_json_rows(r#"{"a":1}"#, &["application"]).expect_err("must fail");
        assert!(matches!(err, CoreError::Parse { .. }));
    }

    #[test]
    fn parse_plain_rows_is_positional() {
        // 实测：制表符分隔，列序与 --columns 一致
        let text = "org.mozilla.firefox\tFirefox\t155.0.1\tflathub\n";
        let rows = parse_plain_rows(text, 5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], "org.mozilla.firefox");
        assert_eq!(rows[0][3], "flathub");
        assert_eq!(rows[0][4], "", "缺失列补空串");
    }

    #[test]
    fn flatpak_app_from_row_builds_summary() {
        let cols = [
            "application",
            "name",
            "description",
            "version",
            "remotes",
            "size",
        ];
        let row: Vec<String> = [
            "org.mozilla.firefox",
            "Firefox",
            "Fast",
            "155.0.1",
            "flathub",
            "125.6 MB",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let app = FlatpakApp::from_row(&row, &cols).expect("app");
        assert_eq!(app.app_id, "org.mozilla.firefox");
        assert_eq!(app.remote, "flathub");
        assert_eq!(app.installed_size, Some((125.6 * 1024.0 * 1024.0) as u64));
        let s = app.to_summary(Installed::No);
        assert_eq!(s.id.name, "org.mozilla.firefox");
        assert_eq!(s.id.source.repo_name(), Some("flathub"));
        assert_eq!(s.display_name, "Firefox");
        assert_eq!(s.summary, "Fast");
    }

    #[test]
    fn flatpak_app_from_row_requires_application_id() {
        let cols = ["application", "name"];
        let row: Vec<String> = vec![String::new(), "x".into()];
        assert!(FlatpakApp::from_row(&row, &cols).is_none());
    }

    #[test]
    fn remote_info_parses_real_output() {
        // 实测的 flatpak 1.18.2 输出
        let text = "\nFirefox - Fast, Private & Safe Web Browser\n\n            ID: org.mozilla.firefox\n           Ref: app/org.mozilla.firefox/x86_64/stable\n          Arch: x86_64\n        Branch: stable\n       Version: 155.0.1\n       License: MPL-2.0\n    Collection: org.flathub.Stable\n Download Size: 125.6 MB\nInstalled Size: 334.9 MB\n       Runtime: org.freedesktop.Platform/x86_64/25.08\n           Sdk: org.freedesktop.Sdk/x86_64/25.08\n\n        Commit: 0997f32c35493844d0bfa5c7d49f3b6e012bb5009f31c47f3c7d12bc3e2b8951\n       Subject: Export org.mozilla.firefox\n          Date: 2026-09-04 14:21:36 +0000\n";
        let info = parse_remote_info(text);
        assert_eq!(info.id, "org.mozilla.firefox");
        assert_eq!(info.version, "155.0.1");
        assert_eq!(info.license, "MPL-2.0");
        assert_eq!(info.branch, "stable");
        assert_eq!(info.arch, "x86_64");
        assert_eq!(info.download_size, Some((125.6 * 1024.0 * 1024.0) as u64));
        assert_eq!(info.installed_size, Some((334.9 * 1024.0 * 1024.0) as u64));
        assert_eq!(
            info.runtime.as_deref(),
            Some("org.freedesktop.Platform/x86_64/25.08")
        );
        assert!(info.commit.expect("commit").starts_with("0997f32"));
        assert_eq!(info.date.as_deref(), Some("2026-09-04 14:21:36 +0000"));
        assert_eq!(
            app_id_from_ref(&info.ref_),
            Some("org.mozilla.firefox".to_string())
        );
    }

    #[test]
    fn remote_info_ignores_unknown_fields_and_handles_chinese_labels() {
        let text = "            ID: org.x.Y\n        下载大小: 12.5 MB\n  未知字段: 值\n";
        let info = parse_remote_info(text);
        assert_eq!(info.id, "org.x.Y");
        assert_eq!(info.download_size, Some((12.5 * 1024.0 * 1024.0) as u64));
    }

    #[test]
    fn remote_info_stays_empty_on_garbage() {
        let info = parse_remote_info("完全不是 remote-info 的输出\n");
        assert_eq!(info, RemoteInfo::default());
    }

    #[test]
    fn app_id_from_ref_variants() {
        assert_eq!(
            app_id_from_ref("app/org.mozilla.firefox/x86_64/stable"),
            Some("org.mozilla.firefox".to_string())
        );
        assert_eq!(
            app_id_from_ref("runtime/org.freedesktop.Platform/x86_64/25.08"),
            Some("org.freedesktop.Platform".to_string())
        );
        assert_eq!(app_id_from_ref("bogus"), None);
        assert_eq!(app_id_from_ref("app"), None);
    }

    #[test]
    fn parse_remotes_dedupes_and_uses_first_column() {
        // 实测：flatpak remotes 默认输出是"名称<TAB>安装位置"
        assert_eq!(
            parse_remotes("flathub\tsystem\nflathub\tuser\ngnome-nightly\tsystem\n"),
            vec!["flathub".to_string(), "gnome-nightly".to_string()]
        );
        assert!(parse_remotes("").is_empty());
    }

    #[test]
    fn local_appstream_locate_rejects_bad_remote_names() {
        assert!(LocalAppstream::locate("../etc").is_none());
        assert!(LocalAppstream::locate("").is_none());
        assert!(LocalAppstream::locate("a/b").is_none());
    }

    #[test]
    fn local_appstream_streams_components_by_category() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("appstream.xml");
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<components version="0.8" origin="flatpak">
  <component type="desktop-application">
    <id>org.example.Game</id>
    <name>Example Game</name>
    <summary>A game</summary>
    <categories><category>Game</category></categories>
    <icon type="cached" width="128" height="128">org.example.Game.png</icon>
  </component>
  <component type="desktop-application">
    <id>org.example.Editor</id>
    <name>Example Editor</name>
    <summary>An editor</summary>
    <categories><category>Development</category></categories>
  </component>
  <component type="runtime">
    <id>org.freedesktop.Platform</id>
    <name>Platform</name>
  </component>
</components>"#;
        std::fs::write(&path, xml).expect("write");
        let local = LocalAppstream { path };
        let games = local.components_in_category("game", 10).expect("scan");
        assert_eq!(games.len(), 1, "只应匹配 Game 分类");
        assert_eq!(games[0].id, "org.example.Game");
        assert_eq!(games[0].name, "Example Game");
        assert_eq!(games[0].summary, "A game");
        assert_eq!(games[0].icon.as_deref(), Some("org.example.Game.png"));

        let dev = local
            .components_in_category("Development", 10)
            .expect("scan");
        assert_eq!(dev.len(), 1);
        assert_eq!(dev[0].id, "org.example.Editor");
        assert_eq!(dev[0].icon, None);

        assert!(
            local
                .components_in_category("Nonexistent", 10)
                .expect("scan")
                .is_empty()
        );
        assert_eq!(
            local.components_in_category("Game", 1).expect("scan").len(),
            1
        );
    }

    #[test]
    fn local_appstream_icon_path_rejects_traversal() {
        let local = LocalAppstream {
            path: PathBuf::from("/tmp/x/appstream.xml"),
        };
        assert!(local.icon_path("../../etc/passwd").is_none());
        assert!(local.icon_path("a/b.png").is_none());
    }

    #[test]
    fn collection_hit_summary_keeps_remote_icon() {
        // 首页"推荐"用的是 Flathub 集合接口。曾经的缺陷：首页自己构造摘要、
        // 漏掉了 icon 字段，于是整页都是字母头像（用户实测反馈）。
        let hit: CollectionHit = serde_json::from_str(
            r#"{
                "app_id": "org.mozilla.firefox",
                "name": "Firefox",
                "summary": "Fast, Private & Safe Web Browser",
                "icon": "https://dl.flathub.org/media/icons/128x128/org.mozilla.firefox.png"
            }"#,
        )
        .expect("解析集合命中");

        let s = summary_from_hit("flathub", &hit);
        assert_eq!(s.id.name, "org.mozilla.firefox");
        assert_eq!(s.display_name, "Firefox");
        match &s.icon {
            IconRef::Remote(url) => assert!(
                url.starts_with("https://dl.flathub.org/"),
                "集合摘要必须带图标 URL：{url}"
            ),
            other => panic!("集合摘要丢了图标 URL：{other:?}"),
        }

        // 集合没有图标时退回应用 ID 的主题查找，而不是留下 Missing
        let mut no_icon = hit.clone();
        no_icon.icon = None;
        match summary_from_hit("flathub", &no_icon).icon {
            IconRef::IconName(n) => assert_eq!(n, "org.mozilla.firefox"),
            other => panic!("没有图标 URL 时应退回主题图标名：{other:?}"),
        }

        // 非 https 的图标必须被忽略（net.rs 的域名白名单之外不允许下载）
        let mut http_icon = hit.clone();
        http_icon.icon = Some("http://evil.example/x.png".into());
        match summary_from_hit("flathub", &http_icon).icon {
            IconRef::IconName(n) => assert_eq!(n, "org.mozilla.firefox"),
            other => panic!("http 图标必须被拒绝：{other:?}"),
        }

        // 名字为空时用应用 ID 兜底
        let mut unnamed = hit.clone();
        unnamed.name = String::new();
        assert_eq!(
            summary_from_hit("flathub", &unnamed).display_name,
            "org.mozilla.firefox"
        );
    }
}
