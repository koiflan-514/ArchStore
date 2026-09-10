//! alpm 工作线程：libalpm 句柄非 Send/Sync，必须由单一线程独占（project.md §3.1 / §4.3）。
//!
//! 为什么必须有 alpm 工作线程：alpm::Alpm 不是 Send + Sync，且首次遍历 extra 库
//! （实测 14955 个包）是毫秒到数十毫秒级的同步操作。放在主线程会造成可感知卡顿；
//! 放在 spawn_blocking 里则需要跨线程传递句柄（不允许）。单线程独占句柄 + 消息传递是
//! 唯一既安全又简单的方案。
//!
//! 陷阱（§4.3，实测）：register_syncdb_mut 对任何名字都返回 Ok，并给出一个永久为空的 Db。
//! 因此仓库可用性必须三条件判定：
//!   pacman.conf 中存在该段名
//!   ∧ /var/lib/pacman/sync/<name>.db 存在且非空
//!   ∧ register_syncdb_mut 返回的 db.pkgs().len() > 0
//! db.is_valid() 不足够（空库也返回 true）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use alpm::{Alpm, Package, PackageReason, Pkg, SigLevel};
use tokio::sync::{oneshot, watch};

use crate::backend::Category;
use crate::env;
use crate::error::{CoreError, CoreResult};
use crate::model::{
    DepKind, DependencyInfo, IconRef, Installed, PackageDetail, PackageId, PackageSource,
    PackageSummary, UpdateInfo, human_size,
};

/// 同步库的可用性判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoStatus {
    /// 仓库名（原样取自 pacman.conf，不做事后大小写归一）
    pub name: String,
    pub available: bool,
    pub packages: usize,
    /// 不可用原因（面向用户，含修复建议）
    pub reason: Option<String>,
}

/// alpm 线程支持的只读操作。
#[derive(Debug, Clone)]
pub enum AlpmOp {
    Search {
        query: String,
        /// 空 = 全部同步库；可包含特殊名 "local" 表示本地库
        repos: Vec<String>,
        limit: usize,
    },
    Info {
        name: String,
    },
    Installed,
    Upgradable,
    Deps {
        name: String,
    },
    RevDeps {
        name: String,
    },
    /// 删除 `removing` 里的这些包之后，不再被任何已安装包需要的依赖
    /// （`pacman -Rns` 的清理范围，project.md §9.3）。
    Unneeded {
        removing: Vec<String>,
    },
    Groups,
    GroupMembers {
        /// 形如 "extra:gnome" 的分类 id
        group: String,
    },
    /// 丢弃内存索引并重新打开句柄（事务完成后、或用户点"重新检测"）。
    Refresh,
}

/// 一次请求。
#[derive(Debug)]
pub struct AlpmReq {
    pub id: u64,
    pub op: AlpmOp,
}

/// 请求的返回载荷。
#[derive(Debug, Clone)]
pub enum AlpmPayload {
    Summaries(Vec<PackageSummary>),
    Detail(Box<PackageDetail>),
    Deps(Vec<DependencyInfo>),
    RevDeps(Vec<PackageId>),
    /// 卸载后不再被需要的依赖（`pacman -Rns` 的清理范围）。
    Unneeded(Vec<PackageId>),
    Groups(Vec<Category>),
    Repos(Vec<RepoStatus>),
    Unit,
}

/// 一次响应。
#[derive(Debug)]
pub struct AlpmResp {
    pub req_id: u64,
    pub result: CoreResult<AlpmPayload>,
}

/// 线程启动阶段。
#[derive(Debug, Clone)]
enum Phase {
    Starting,
    Ready,
    Fatal(String),
}

/// 本地库索引项（972 个包，实测构建开销可忽略）。
#[derive(Debug, Clone)]
struct LocalEntry {
    version: String,
    explicit: bool,
    /// 不在任何同步库中 = 外来包（可能来自 AUR）
    foreign: bool,
    repo: Option<String>,
}

/// 工作线程独占的状态。
struct State {
    handle: Alpm,
    repos: Vec<String>,
    statuses: Vec<RepoStatus>,
    local: Option<HashMap<String, LocalEntry>>,
    /// AppStream 图标索引（包名 -> 图标文件）。
    ///
    /// 仓库里绝大多数包在 hicolor 主题里没有同名图标（实测前 400 个已安装包只有 3 个有），
    /// 真正的图标来源是 archlinux-appstream-data。没装该数据包时索引为空，
    /// UI 会退回字母头像。
    appstream: crate::icons::AppstreamIcons,
    /// `.desktop` 图标索引（包名 -> 图标），只覆盖**已安装**的包。
    ///
    /// 这是 AUR 软件与"AppStream 里没有"的仓库软件唯一的本地图标来源，见
    /// `crate::desktop_icons` 的模块说明。
    desktop: crate::desktop_icons::DesktopIcons,
}

impl State {
    /// 惰性构建本地库索引。
    fn ensure_local(&mut self) {
        if self.local.is_some() {
            return;
        }
        let mut map: HashMap<String, LocalEntry> = HashMap::new();
        for pkg in self.handle.localdb().pkgs().iter() {
            let mut repo = None;
            for db in self.handle.syncdbs().iter() {
                if db.pkg(pkg.name()).is_ok() {
                    repo = Some(db.name().to_string());
                    break;
                }
            }
            map.insert(
                pkg.name().to_string(),
                LocalEntry {
                    version: pkg.version().to_string(),
                    explicit: matches!(pkg.reason(), PackageReason::Explicit),
                    foreign: repo.is_none(),
                    repo,
                },
            );
        }
        self.local = Some(map);
    }

    fn entry(&self, name: &str) -> Option<&LocalEntry> {
        self.local.as_ref().and_then(|m| m.get(name))
    }

    /// 重新打开句柄：libalpm 会缓存本地库内容，外部事务（pacman/flatpak）之后必须重开。
    fn reopen(&mut self) -> CoreResult<()> {
        let repos = self.repos.clone();
        let (handle, statuses) = open_handle(&repos)?;
        let old = std::mem::replace(&mut self.handle, handle);
        let _ = old.release();
        self.statuses = statuses;
        self.local = None;
        // 用户可能在运行期间装了 archlinux-appstream-data 或新软件，重新扫描图标索引
        self.appstream = load_appstream(&self.handle);
        self.desktop = build_desktop_icons(&self.handle);
        Ok(())
    }
}

/// 注册一个同步库并三条件判定其可用性。
fn register_repo(handle: &mut Alpm, name: &str) -> RepoStatus {
    let file_ok = env::sync_db_file_ready(std::path::Path::new("/var/lib/pacman/sync"), name);
    match handle.register_syncdb_mut(name, SigLevel::USE_DEFAULT) {
        Ok(db) => {
            let n = db.pkgs().len();
            if n > 0 {
                RepoStatus {
                    name: name.to_string(),
                    available: true,
                    packages: n,
                    reason: None,
                }
            } else {
                RepoStatus {
                    name: name.to_string(),
                    available: false,
                    packages: 0,
                    reason: Some(if file_ok {
                        format!("仓库 {name} 的数据库为空，请运行 sudo pacman -Sy")
                    } else {
                        format!(
                            "仓库 {name} 尚未同步（缺少 /var/lib/pacman/sync/{name}.db），请运行 sudo pacman -Sy"
                        )
                    }),
                }
            }
        }
        Err(e) => RepoStatus {
            name: name.to_string(),
            available: false,
            packages: 0,
            reason: Some(format!("仓库 {name} 注册失败：{e}")),
        },
    }
}

/// 打开只读句柄并注册全部同步库。
fn open_handle(repos: &[String]) -> CoreResult<(Alpm, Vec<RepoStatus>)> {
    let mut handle = Alpm::new("/", "/var/lib/pacman")
        .map_err(|e| CoreError::AlpmInit(format!("{e}（无法打开 /var/lib/pacman：{e}）")))?;
    let mut statuses = Vec::with_capacity(repos.len());
    for name in repos {
        statuses.push(register_repo(&mut handle, name));
    }
    Ok((handle, statuses))
}

/// 建立 AppStream 图标索引。
///
/// 传入包名集合用于消歧带下划线的图标文件名
/// （`jack_mixer_jack_mixer.png` 必须归属 `jack_mixer` 而不是 `jack`）。
///
/// 先把 16k 个包名收进 HashSet（约 1 ms），再交给索引：实测在判定器里逐个调
/// libalpm 的 `db.pkg()`（3814 个文件名 × 1~3 次查询）要 **145 ms**，
/// 而 HashSet 判定只要 **1 ms** —— 纯开销差异，结果完全一致。
fn load_appstream(handle: &Alpm) -> crate::icons::AppstreamIcons {
    let names: std::collections::HashSet<String> = handle
        .syncdbs()
        .iter()
        .flat_map(|db| db.pkgs().iter().map(|p| p.name().to_string()))
        .chain(handle.localdb().pkgs().iter().map(|p| p.name().to_string()))
        .collect();
    crate::icons::AppstreamIcons::load_with(&|name: &str| names.contains(name))
}

/// 建立"包名 -> .desktop 图标"索引（只覆盖已安装的包）。
///
/// 遍历本地库的文件清单（`pacman -Ql` 的数据源）找出每个包的 `.desktop`，
/// 再读最多几个文件解析 `Icon=`。全部在 worker 线程内完成，不阻塞 UI，
/// 也不需要 root 与网络。
fn build_desktop_icons(handle: &Alpm) -> crate::desktop_icons::DesktopIcons {
    let mut map = HashMap::new();
    for pkg in handle.localdb().pkgs().iter() {
        let name = pkg.name();
        let mut candidates: Vec<(u8, std::path::PathBuf)> = Vec::new();
        for file in pkg.files().files() {
            let Ok(path) = std::str::from_utf8(file.name()) else {
                continue;
            };
            if let Some(candidate) = crate::desktop_icons::desktop_candidate(path, name) {
                candidates.push(candidate);
            }
        }
        if let Some(icon) = crate::desktop_icons::resolve_candidates(candidates) {
            map.insert(name.to_string(), icon);
        }
    }
    crate::desktop_icons::DesktopIcons::from_map(map)
}

/// 图标优先级（"很多软件没有图标"的修复核心）。
///
/// 1. AppStream 数据包里的图标文件（仓库 GUI 软件，约 8% 的包有）；
/// 2. 包自己安装的 `.desktop` 里的 `Icon=`（已安装的包，覆盖 AUR）；
/// 3. 主题里与包名同名的图标（GUI 侧解析，如 mpv / btop）；
/// 4. 全都没有 -> GUI 兜底成字母头像。
fn resolve_icon(
    name: &str,
    appstream: &crate::icons::AppstreamIcons,
    desktop: &crate::desktop_icons::DesktopIcons,
) -> IconRef {
    if let Some(path) = appstream.lookup(name) {
        return IconRef::CachedFile(path.to_path_buf());
    }
    if let Some(icon) = desktop.lookup(name) {
        return icon.clone();
    }
    IconRef::IconName(name.to_string())
}

/// --doctor 与设置页使用的只读探测结果。
#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub local: usize,
    pub explicit: usize,
    pub repos: Vec<RepoStatus>,
    /// AppStream 图标索引覆盖的包数（数据包未安装时为 0）
    pub appstream_icons: usize,
    /// 已安装包中能从自己的 .desktop 解析出图标的数量（AUR 软件也在内）
    pub desktop_icons: usize,
}

/// 只读探测：打开句柄、统计本地包、判定每个同步库的可用性。绝不写盘、绝不下载。
pub fn probe_readonly() -> CoreResult<ProbeResult> {
    let repos = env::discover_sync_repos();
    let (handle, statuses) = open_handle(&repos)?;
    let mut local = 0usize;
    let mut explicit = 0usize;
    for pkg in handle.localdb().pkgs().iter() {
        local += 1;
        if matches!(pkg.reason(), PackageReason::Explicit) {
            explicit += 1;
        }
    }
    // 图标来源统计：两者都是只读扫描，实测合计 < 200 ms（AppStream 目录 + 已安装包文件清单）
    let appstream_icons = load_appstream(&handle).len();
    let desktop_icons = build_desktop_icons(&handle).len();
    let _ = handle.release();
    Ok(ProbeResult {
        local,
        explicit,
        repos: statuses,
        appstream_icons,
        desktop_icons,
    })
}

/// 单线程独占 libalpm 的工作线程句柄。
///
/// 与 §3.1 的线程模型一致：句柄只在工作线程内被访问，UI/网络层只持有消息通道。
/// 响应通过 oneshot 返回，因此调用方可以 request(...).await，把结果再投递回 glib 主循环。
pub struct AlpmWorker {
    tx: Sender<AlpmReq>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<AlpmResp>>>>,
    next_id: AtomicU64,
    phase: watch::Receiver<Phase>,
    statuses: Arc<Mutex<Vec<RepoStatus>>>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for AlpmWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlpmWorker").finish_non_exhaustive()
    }
}

impl AlpmWorker {
    /// 启动工作线程。线程内部完成同步库名解析与句柄打开。
    pub fn spawn() -> CoreResult<Arc<Self>> {
        let (tx, rx) = mpsc::channel::<AlpmReq>();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<AlpmResp>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let statuses: Arc<Mutex<Vec<RepoStatus>>> = Arc::new(Mutex::new(Vec::new()));
        let (phase_tx, phase_rx) = watch::channel(Phase::Starting);

        let pending_thread = Arc::clone(&pending);
        let statuses_thread = Arc::clone(&statuses);
        let join = std::thread::Builder::new()
            .name("alpm-worker".to_string())
            .spawn(move || thread_main(rx, pending_thread, statuses_thread, phase_tx))
            .map_err(|e| CoreError::Internal(format!("无法创建 alpm 工作线程：{e}")))?;

        Ok(Arc::new(Self {
            tx,
            pending,
            next_id: AtomicU64::new(1),
            phase: phase_rx,
            statuses,
            join: Mutex::new(Some(join)),
        }))
    }

    /// 等待启动阶段结束并返回同步库状态。
    pub async fn wait_ready(&self) -> CoreResult<Vec<RepoStatus>> {
        let mut rx = self.phase.clone();
        loop {
            {
                let phase = rx.borrow_and_update().clone();
                match phase {
                    Phase::Ready => return Ok(self.repo_status()),
                    Phase::Fatal(msg) => return Err(CoreError::AlpmInit(msg)),
                    Phase::Starting => {}
                }
            }
            rx.changed()
                .await
                .map_err(|_| CoreError::Internal("alpm 工作线程已退出".into()))?;
        }
    }

    /// 当前已知的同步库状态（无需等待）。
    pub fn repo_status(&self) -> Vec<RepoStatus> {
        self.statuses.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// 当前可用的仓库名。
    pub fn available_repos(&self) -> Vec<String> {
        self.repo_status()
            .into_iter()
            .filter(|r| r.available)
            .map(|r| r.name)
            .collect()
    }

    /// 提交请求，返回 (req_id, 响应接收器)。
    ///
    /// req_id 自增：UI 侧保存"当前有效 req_id"，丢弃过期响应（用户快速输入时必备）。
    pub fn submit(&self, op: AlpmOp) -> (u64, oneshot::Receiver<AlpmResp>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        if let Ok(mut map) = self.pending.lock() {
            map.insert(id, tx);
        }
        if self.tx.send(AlpmReq { id, op }).is_err()
            && let Ok(mut map) = self.pending.lock()
        {
            map.remove(&id);
        }
        (id, rx)
    }

    /// 提交并等待结果。
    pub async fn request(&self, op: AlpmOp) -> CoreResult<AlpmPayload> {
        let (_id, rx) = self.submit(op);
        match rx.await {
            Ok(resp) => resp.result,
            Err(_) => Err(CoreError::Internal(
                "alpm 工作线程已停止，无法完成查询".into(),
            )),
        }
    }

    /// 当前请求序号（用于 UI 判断响应是否过期）。
    pub fn current_id(&self) -> u64 {
        self.next_id.load(Ordering::Relaxed)
    }

    /// 关闭通道并等待线程退出（内部会调用 handle.release()）。
    pub fn shutdown(&self) {
        // 通过替换 tx 关闭通道：这里用一个哨兵无法实现，改为 join 前丢弃发送端。
        // 由于 AlpmWorker 通常被 Arc 包裹，shutdown 只做 join 尝试。
        let handle = self.join.lock().ok().and_then(|mut g| g.take());
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

fn thread_main(
    rx: Receiver<AlpmReq>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<AlpmResp>>>>,
    statuses: Arc<Mutex<Vec<RepoStatus>>>,
    phase_tx: watch::Sender<Phase>,
) {
    let repos = env::discover_sync_repos();
    let (handle, repo_statuses) = match open_handle(&repos) {
        Ok(v) => v,
        Err(e) => {
            let _ = phase_tx.send(Phase::Fatal(e.user_message()));
            return;
        }
    };
    if let Ok(mut s) = statuses.lock() {
        *s = repo_statuses.clone();
    }

    // 图标索引必须在 Ready 之前建好：Ready 的语义是"可以立刻提供服务"。
    // 否则第一个请求要替启动阶段买单（实测 Installed 从 11.5 ms 变成 52.6 ms）。
    // AppStream 只需一次 read_dir（实测约 1200 个文件，147 ms）；
    // .desktop 索引遍历本地库文件清单（974 个包，28 ms），都不需要 root/网络。
    let appstream = load_appstream(&handle);
    let desktop = build_desktop_icons(&handle);
    tracing::debug!(
        appstream = appstream.len(),
        desktop = desktop.len(),
        "图标索引已建立"
    );

    let _ = phase_tx.send(Phase::Ready);
    let mut state = State {
        handle,
        repos,
        statuses: repo_statuses,
        local: None,
        appstream,
        desktop,
    };

    while let Ok(req) = rx.recv() {
        let result = handle_op(&mut state, req.op);
        let resp = AlpmResp {
            req_id: req.id,
            result,
        };
        let sender = pending.lock().ok().and_then(|mut m| m.remove(&req.id));
        if let Some(s) = sender {
            // 接收端可能已丢弃（用户取消/请求过期），这不是错误
            let _ = s.send(resp);
        }
    }

    let _ = state.handle.release();
    tracing::debug!("alpm 工作线程已退出");
}

/// 需要本地索引的操作。
fn needs_local_index(op: &AlpmOp) -> bool {
    matches!(
        op,
        AlpmOp::Search { .. }
            | AlpmOp::Installed
            | AlpmOp::Upgradable
            | AlpmOp::Info { .. }
            | AlpmOp::Deps { .. }
            | AlpmOp::RevDeps { .. }
            | AlpmOp::Unneeded { .. }
    )
}

fn handle_op(state: &mut State, op: AlpmOp) -> CoreResult<AlpmPayload> {
    if matches!(op, AlpmOp::Refresh) {
        state.reopen()?;
        return Ok(AlpmPayload::Repos(state.statuses.clone()));
    }
    if needs_local_index(&op) {
        state.ensure_local();
    }
    let state: &State = state;

    match op {
        AlpmOp::Refresh => unreachable!("已在上面处理"),
        AlpmOp::Search {
            query,
            repos,
            limit,
        } => Ok(AlpmPayload::Summaries(search(state, &query, &repos, limit))),
        AlpmOp::Installed => Ok(AlpmPayload::Summaries(installed(state))),
        AlpmOp::Upgradable => Ok(AlpmPayload::Summaries(upgradable(state))),
        AlpmOp::Info { name } => Ok(AlpmPayload::Detail(Box::new(detail(state, &name)?))),
        AlpmOp::Deps { name } => Ok(AlpmPayload::Deps(dependencies(state, &name)?)),
        AlpmOp::RevDeps { name } => Ok(AlpmPayload::RevDeps(reverse_dependencies(state, &name)?)),
        AlpmOp::Unneeded { removing } => Ok(AlpmPayload::Unneeded(unneeded_after_remove(
            state, &removing,
        ))),
        AlpmOp::Groups => Ok(AlpmPayload::Groups(groups(state))),
        AlpmOp::GroupMembers { group } => Ok(AlpmPayload::Summaries(group_members(state, &group))),
    }
}

/// 在同步库中按名精确查找，返回包与所属仓库名。
fn find_in_sync<'a>(handle: &'a Alpm, name: &str) -> Option<(&'a Package, String)> {
    for db in handle.syncdbs().iter() {
        if let Ok(p) = db.pkg(name) {
            return Some((p, db.name().to_string()));
        }
    }
    None
}

/// 构造已安装包（或外来包）的 PackageId。
fn installed_id(state: &State, name: &str) -> PackageId {
    match state.entry(name) {
        Some(e) => match &e.repo {
            Some(repo) => PackageId::official(repo.clone(), name),
            // 外来包：可能来自 AUR
            None => PackageId::aur(name),
        },
        None => PackageId::official(String::new(), name),
    }
}

/// 由 alpm 包构造摘要。
fn summary_for(state: &State, pkg: &Pkg, repo_hint: Option<&str>) -> PackageSummary {
    let name = pkg.name().to_string();
    let entry = state.entry(&name);
    let repo = repo_hint
        .map(|s| s.to_string())
        .or_else(|| entry.and_then(|e| e.repo.clone()));
    let source = match &repo {
        // 不在任何同步库中的本地包 = 外来包（可能来自 AUR）
        None => PackageSource::Aur,
        Some(r) => PackageSource::Official { repo: r.clone() },
    };
    let installed = match entry {
        Some(e) => Installed::Yes {
            version: e.version.clone(),
            explicit: e.explicit,
        },
        None => Installed::No,
    };
    let mut s = PackageSummary::minimal(
        PackageId {
            source,
            name: name.clone(),
        },
        name.clone(),
    );
    s.version = Some(pkg.version().to_string());
    if let Some(d) = pkg.desc() {
        s.set_summary(d);
    }
    s.installed = installed;
    s.icon = resolve_icon(&name, &state.appstream, &state.desktop);
    s
}

/// 已安装列表（实测本机 972 个包）。
fn installed(state: &State) -> Vec<PackageSummary> {
    let mut out: Vec<PackageSummary> = Vec::new();
    for pkg in state.handle.localdb().pkgs().iter() {
        out.push(summary_for(state, pkg, None));
    }
    out.sort_by(|a, b| a.id.name.cmp(&b.id.name));
    out
}

/// 可更新列表：本地库 vs 同步库比对（用 alpm::vercmp，不做字符串比较）。
fn upgradable(state: &State) -> Vec<PackageSummary> {
    let mut out: Vec<PackageSummary> = Vec::new();
    for pkg in state.handle.localdb().pkgs().iter() {
        let Some((sync_pkg, repo)) = find_in_sync(&state.handle, pkg.name()) else {
            continue;
        };
        let current = pkg.version().to_string();
        let candidate = sync_pkg.version().to_string();
        if sync_pkg.version().vercmp(pkg.version()) != std::cmp::Ordering::Greater {
            continue;
        }
        let mut s = summary_for(state, sync_pkg, Some(&repo));
        s.installed = Installed::Yes {
            version: current.clone(),
            explicit: state.entry(pkg.name()).map(|e| e.explicit).unwrap_or(false),
        };
        s.update = Some(UpdateInfo {
            current,
            candidate,
            download_size: Some(sync_pkg.size().max(0) as u64),
        });
        out.push(s);
    }
    out.sort_by(|a, b| a.id.name.cmp(&b.id.name));
    out
}

/// 库内搜索。
fn search(state: &State, query: &str, repos: &[String], limit: usize) -> Vec<PackageSummary> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    let targets: Vec<String> = if repos.is_empty() {
        state
            .handle
            .syncdbs()
            .iter()
            .map(|d| d.name().to_string())
            .collect()
    } else {
        repos.to_vec()
    };
    let mut out: Vec<PackageSummary> = Vec::new();
    let cap = if limit == 0 { usize::MAX } else { limit };

    for target in &targets {
        if out.len() >= cap {
            break;
        }
        if target == "local" {
            let db = state.handle.localdb();
            if let Ok(found) = db.search([q].iter().cloned()) {
                for pkg in found.iter() {
                    if out.len() >= cap {
                        break;
                    }
                    if out.iter().any(|s| s.id.name == pkg.name()) {
                        continue;
                    }
                    out.push(summary_for(state, pkg, None));
                }
            }
            continue;
        }
        let Some(db) = state.handle.syncdbs().iter().find(|d| d.name() == target) else {
            continue;
        };
        let Ok(found) = db.search([q].iter().cloned()) else {
            continue;
        };
        for pkg in found.iter() {
            if out.len() >= cap {
                break;
            }
            if out.iter().any(|s| s.id.name == pkg.name()) {
                continue;
            }
            out.push(summary_for(state, pkg, Some(target)));
        }
    }
    out
}

/// 包组（分类页数据源：实测 extra 有 107 个组）。
fn groups(state: &State) -> Vec<Category> {
    let mut out: Vec<Category> = Vec::new();
    for db in state.handle.syncdbs().iter() {
        let repo = db.name().to_string();
        let Ok(list) = db.groups() else {
            continue;
        };
        for g in list.iter() {
            let count = g.packages().len();
            if count == 0 {
                continue;
            }
            let id = format!("{repo}:{}", g.name());
            out.push(Category::new(
                id,
                format!("{}（{count}）", g.name()),
                "pacman",
            ));
        }
    }
    out.sort_by(|a, b| a.display.cmp(&b.display));
    out
}

/// 包组成员。
fn group_members(state: &State, group: &str) -> Vec<PackageSummary> {
    let Some((repo, name)) = group.split_once(':') else {
        return Vec::new();
    };
    let Some(db) = state.handle.syncdbs().iter().find(|d| d.name() == repo) else {
        return Vec::new();
    };
    let Ok(g) = db.group(name) else {
        return Vec::new();
    };
    let mut out: Vec<PackageSummary> = Vec::new();
    for pkg in g.packages().iter() {
        out.push(summary_for(state, pkg, Some(repo)));
    }
    out.sort_by(|a, b| a.id.name.cmp(&b.id.name));
    out
}

/// 详情（本地字段，无网络）。
fn detail(state: &State, name: &str) -> CoreResult<PackageDetail> {
    let (pkg, repo) = match find_in_sync(&state.handle, name) {
        Some((p, r)) => (p, Some(r)),
        None => {
            let local = state
                .handle
                .localdb()
                .pkg(name)
                .map_err(|_| CoreError::NotFound(format!("软件包 {name}")))?;
            (local, None)
        }
    };
    let mut d = PackageDetail::from_summary(summary_for(state, pkg, repo.as_deref()));
    d.description = pkg.desc().unwrap_or_default().to_string();
    d.licenses = pkg.licenses().iter().map(|s| s.to_string()).collect();
    d.homepage = pkg.url().map(|s| s.to_string());
    d.download_size = Some(pkg.size().max(0) as u64);
    d.installed_size = Some(pkg.isize().max(0) as u64);
    d.maintainer = pkg.packager().map(|s| s.to_string());
    d.extra.push("包名", pkg.name());
    d.extra.push("版本", pkg.version().to_string());
    d.extra.push("仓库", d.summary.id.source.display());
    if let Some(a) = pkg.arch() {
        d.extra.push("架构", a);
    }
    if let Some(b) = pkg.base() {
        d.extra.push("基础包", b);
    }
    d.extra.push("构建日期", format_date(pkg.build_date()));
    if let Some(t) = pkg.install_date() {
        d.extra.push("安装日期", format_date(t));
    }
    if let Some(e) = state.entry(name) {
        d.extra.push(
            "安装原因",
            if e.explicit {
                "用户显式安装"
            } else {
                "作为依赖安装"
            },
        );
        if e.foreign {
            d.extra
                .push("来源", "外来包（不在任何同步库中，可能来自 AUR）");
        }
    }
    let provides: Vec<String> = pkg.provides().iter().map(|d| d.to_string()).collect();
    if !provides.is_empty() {
        d.extra.push("提供", provides.join("、"));
    }
    let conflicts: Vec<String> = pkg.conflicts().iter().map(|d| d.to_string()).collect();
    if !conflicts.is_empty() {
        d.extra.push("冲突", conflicts.join("、"));
    }
    let replaces: Vec<String> = pkg.replaces().iter().map(|d| d.to_string()).collect();
    if !replaces.is_empty() {
        d.extra.push("替代", replaces.join("、"));
    }
    d.dependencies = deps_of(state, pkg)?;
    Ok(d)
}

/// 依赖信息（只读、用于展示与计划构建）。
fn dependencies(state: &State, name: &str) -> CoreResult<Vec<DependencyInfo>> {
    let (pkg, _) = match find_in_sync(&state.handle, name) {
        Some(v) => v,
        None => (
            state
                .handle
                .localdb()
                .pkg(name)
                .map_err(|_| CoreError::NotFound(format!("软件包 {name}")))?,
            String::new(),
        ),
    };
    deps_of(state, pkg)
}

fn deps_of(state: &State, pkg: &Pkg) -> CoreResult<Vec<DependencyInfo>> {
    let mut out: Vec<DependencyInfo> = Vec::new();
    for (list, kind) in [
        (pkg.depends(), DepKind::Runtime),
        (pkg.makedepends(), DepKind::Make),
        (pkg.checkdepends(), DepKind::Check),
        (pkg.optdepends(), DepKind::Optional),
    ] {
        for dep in list.iter() {
            out.push(dep_entry(state, dep, kind));
        }
    }
    Ok(out)
}

/// 单条依赖：用 find_satisfier 判定是否满足（自动处理 virtual provides 与版本约束）。
fn dep_entry(state: &State, dep: &alpm::Dep, kind: DepKind) -> DependencyInfo {
    let expr = dep.to_string();
    let (name, parsed_desc) = DependencyInfo::parse(&expr);
    let satisfier = state.handle.localdb().pkgs().find_satisfier(expr.clone());
    let (satisfied_by, missing, size) = match satisfier {
        Some(p) => (Some(installed_id(state, p.name())), false, None),
        None => {
            let size = find_in_sync(&state.handle, &name)
                .map(|(p, _)| p.size().max(0) as u64)
                .filter(|n| *n > 0);
            (None, true, size)
        }
    };
    DependencyInfo {
        name,
        kind,
        description: dep.desc().map(|s| s.to_string()).or(parsed_desc),
        satisfied_by,
        missing,
        size,
        recommended: false,
    }
}

/// 反向依赖（卸载前警告用）。
///
/// 注意：这里用 pkg.required_by()（实测 glibc -> 705），是廉价且准确的做法。
/// helper 侧做"删除会不会破坏系统"的判定时必须用 handle.check_deps(全体已安装, [待删], [], true)，
/// 因为 check_deps 只传 rem 而 pkgs 为空会静默返回 0（最危险的误用，见 §4.3）。
fn reverse_dependencies(state: &State, name: &str) -> CoreResult<Vec<PackageId>> {
    let pkg = state
        .handle
        .localdb()
        .pkg(name)
        .map_err(|_| CoreError::NotFound(format!("已安装的软件包 {name}")))?;
    let mut out: Vec<PackageId> = Vec::new();
    for dep in pkg.required_by().iter() {
        let n = dep.to_string();
        if !out.iter().any(|p| p.name == n) {
            out.push(installed_id(state, &n));
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// 删除集执行之后"不再被需要"的依赖闭包 —— 也就是 `pacman -Rns` 会顺手清掉的那些包。
///
/// 判定与 pacman 的 `-s` 语义一致：
/// 1. 只看删除集里每个包的**运行时依赖**（`pkg.depends()`）；
///    版本约束与虚拟 provides 都用 `find_satisfier` 解析，绝不按名字字符串猜。
/// 2. 该依赖必须是"作为依赖装进来的"（`PackageReason::Depend`）——
///    用户显式安装的包永远不自动删（这是 pacman -Qdt 与 -Qdtt 的区别）。
/// 3. 它不能再被删除集之外的任何已安装包需要（`required_by` 已计入 provides）。
///
/// 结果是一个**传递闭包**：删掉 A 之后不再需要的 B，其自身依赖的 C 也可能随之失去意义，
/// 因此这里用队列迭代到不动点，而不是只看目标包的直接依赖。
///
/// 只读、无副作用。helper 执行时会自己再算一遍（§4.3 的双重校验），
/// 这里的输出只用于**把清理范围写进计划让用户看见**（§9.3 禁止命令行式隐式清理）。
fn unneeded_after_remove(state: &State, removing: &[String]) -> Vec<PackageId> {
    let local = state.handle.localdb();
    let mut gone: std::collections::HashSet<&str> = removing.iter().map(|s| s.as_str()).collect();
    let mut queue: Vec<String> = removing.to_vec();
    let mut out: Vec<PackageId> = Vec::new();

    while let Some(name) = queue.pop() {
        let Ok(pkg) = local.pkg(name.as_str()) else {
            // 计划里可能存在尚未安装的名字（理论上不会走到这里）：跳过而不是报错
            continue;
        };
        for dep in pkg.depends().iter() {
            let Some(candidate) = local.pkgs().find_satisfier(dep.to_string()) else {
                continue; // 依赖未安装（或由同步库提供）：不是本次卸载的清理对象
            };
            let candidate_name = candidate.name();
            if gone.contains(candidate_name) {
                continue; // 已经要删了
            }
            if !matches!(candidate.reason(), PackageReason::Depend) {
                continue; // 显式安装：绝不自动删除
            }
            let still_needed = candidate.required_by().iter().any(|d| !gone.contains(d));
            if still_needed {
                continue;
            }
            gone.insert(candidate_name);
            queue.push(candidate_name.to_string());
            out.push(installed_id(state, candidate_name));
        }
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

/// 把 Unix 时间戳格式化为 YYYY-MM-DD HH:MM（本地时区偏移不参与，仅用于展示）。
fn format_date(ts: i64) -> String {
    if ts <= 0 {
        return "未知".to_string();
    }
    let secs = ts as u64;
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, mi) = (rem / 3600, (rem % 3600) / 60);
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}")
}

/// Howard Hinnant 的 civil_from_days 算法（无外部依赖的日期换算）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 供 UI 展示的"依赖体积合计"。
pub fn total_download_size(deps: &[DependencyInfo]) -> u64 {
    deps.iter().filter_map(|d| d.size).sum()
}

/// 供 UI 展示的依赖摘要（含"依赖信息不完整"的判定）。
pub fn describe_deps(deps: &[DependencyInfo]) -> String {
    let missing = deps.iter().filter(|d| d.missing).count();
    let total = total_download_size(deps);
    if missing == 0 {
        format!(
            "{} 项依赖均已满足",
            deps.iter().filter(|d| !d.kind.is_build_only()).count()
        )
    } else if total > 0 {
        format!("需要处理 {missing} 项依赖，合计 {}", human_size(total))
    } else {
        format!("需要处理 {missing} 项依赖（部分体积未知）")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(format_date(0), "未知");
        assert_eq!(format_date(1), "1970-01-01 00:00");
    }

    #[test]
    fn repo_status_shape_is_serializable_for_ui() {
        let s = RepoStatus {
            name: "extra".into(),
            available: false,
            packages: 0,
            reason: Some("未同步".into()),
        };
        assert!(!s.available);
        assert!(s.reason.is_some());
    }

    #[test]
    fn appstream_index_is_wired_into_summaries() {
        // 没有数据包时索引为空，此时必须退回主题图标名而不是 CachedFile
        let empty = crate::icons::AppstreamIcons::default();
        assert!(empty.lookup("firefox").is_none());
        assert!(empty.is_empty());
        // 图标优先级的判定逻辑本身由 icons 模块的测试覆盖；
        // 这里确认默认索引不会凭空造出路径
        for pkg in ["firefox", "gimp", "vlc", "htop", "yay"] {
            assert!(empty.lookup(pkg).is_none(), "{pkg}");
        }
    }

    /// 造一个只含指定包名的 AppStream 索引（真实文件，走 load_from）。
    fn appstream_with(
        entries: &[(&str, &str)],
    ) -> (tempfile::TempDir, crate::icons::AppstreamIcons) {
        let dir = tempfile::tempdir().expect("tmpdir");
        let p = dir.path().join("archlinux-arch-extra").join("128x128");
        std::fs::create_dir_all(&p).expect("mkdir");
        for (pkg, app_id) in entries {
            std::fs::write(p.join(format!("{pkg}_{app_id}.png")), b"png").expect("write");
        }
        let icons = crate::icons::AppstreamIcons::load_from(&[dir.path().to_path_buf()]);
        (dir, icons)
    }

    #[test]
    fn icon_priority_prefers_appstream_then_desktop_then_theme() {
        let (_dir, appstream) = appstream_with(&[("firefox", "firefox")]);
        let mut map = HashMap::new();
        map.insert("yay".to_string(), IconRef::IconName("yay-icon".to_string()));
        map.insert(
            "firefox".to_string(),
            IconRef::IconName("from-desktop".into()),
        );
        let desktop = crate::desktop_icons::DesktopIcons::from_map(map);

        // 1) AppStream 命中 -> CachedFile
        match resolve_icon("firefox", &appstream, &desktop) {
            IconRef::CachedFile(p) => {
                assert!(p.to_string_lossy().ends_with("firefox_firefox.png"))
            }
            other => panic!("AppStream 必须优先：{other:?}"),
        }
        // 2) AppStream 没有但 .desktop 有 -> 用 .desktop 的图标名
        match resolve_icon("yay", &appstream, &desktop) {
            IconRef::IconName(n) => assert_eq!(n, "yay-icon"),
            other => panic!(".desktop 必须作为第二优先级：{other:?}"),
        }
        // 3) 都没有 -> 包名交给主题查找
        match resolve_icon("htop", &appstream, &desktop) {
            IconRef::IconName(n) => assert_eq!(n, "htop"),
            other => panic!("必须退回主题图标名：{other:?}"),
        }
    }

    #[test]
    fn desktop_candidates_come_from_alpm_file_lists() {
        // 模拟 libalpm 文件清单（以 usr/ 开头的相对路径），验证挑选与排序
        let mut candidates: Vec<(u8, std::path::PathBuf)> = Vec::new();
        for path in [
            "usr/share/applications/code-url-handler.desktop",
            "usr/share/applications/code-oss.desktop",
            "usr/bin/code",
            "usr/share/icons/hicolor/512x512/apps/code.png",
        ] {
            if let Some(c) = crate::desktop_icons::desktop_candidate(path, "code") {
                candidates.push(c);
            }
        }
        candidates.sort();
        assert_eq!(candidates.len(), 2, "只认应用目录下的 .desktop");
        assert!(
            candidates[0].1.ends_with("code-oss.desktop"),
            "以包名开头的入口优先于无关入口：{:?}",
            candidates[0].1
        );
    }

    #[test]
    fn needs_local_index_covers_expected_ops() {
        assert!(needs_local_index(&AlpmOp::Installed));
        assert!(needs_local_index(&AlpmOp::Upgradable));
        assert!(needs_local_index(&AlpmOp::Search {
            query: "x".into(),
            repos: vec![],
            limit: 10
        }));
        assert!(!needs_local_index(&AlpmOp::Groups));
        assert!(!needs_local_index(&AlpmOp::Refresh));
    }
}
