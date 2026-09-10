//! AppState：ServiceRegistry + 计划队列 + 事务状态机（project.md §3.1 / §5.4 / §5.5）。
//!
//! 状态机被实现为"单一枚举 + 转移函数"，并且不依赖 GTK，便于单元测试。

use std::path::PathBuf;
use std::sync::Arc;

use crate::widgets::dep_list::DepSelection;
use archstore_core::backend::{
    AurBackend, Capability, FlatpakBackend, InstalledIndex, PackageBackend, PacmanBackend,
};
use archstore_core::cache::Cache;
use archstore_core::config::Config;
use archstore_core::error::{CoreError, CoreResult};
use archstore_core::flathub::{Advisory, FlathubClient};
use archstore_core::i18n::SoftwareNames;
use archstore_core::model::plan::{PlanKind, TransactionPlan};
use archstore_core::model::{PackageId, PackageSummary};
use archstore_core::net::HttpClient;
use archstore_core::plan::AurRequest;
use archstore_core::plan::InstallOptions;

/// 后端集合与全局服务（全部 Arc，可安全地交给后台任务）。
pub struct Services {
    pub config: Config,
    pub cache: Arc<Cache>,
    pub http: Arc<HttpClient>,
    pub pacman: Option<Arc<PacmanBackend>>,
    pub aur: Option<Arc<AurBackend>>,
    pub flatpak: Option<Arc<FlatpakBackend>>,
    pub flathub: FlathubClient,
    /// 在线翻译（默认关闭；开启后按配置把描述发到第三方服务）
    pub translate: archstore_core::translate::TranslateClient,
    pub installed: Arc<InstalledIndex>,
    pub names: SoftwareNames,
    /// helper 的路径（未安装时为 None，安装/卸载按钮置灰）
    pub helper: Option<PathBuf>,
    pub pkexec: Option<PathBuf>,
    /// 配置加载时的提示（损坏重置 / 只读模式）
    pub config_notice: Option<String>,
    pub config_read_only: bool,
    /// 所有后端的可用性（设置页与置灰提示）
    pub capabilities: Vec<(&'static str, Capability)>,
}

impl Services {
    /// 构建全部服务（在 tokio 运行时中执行；失败的后端会被标记为不可用而不是中断启动）。
    pub async fn build(
        cfg: Config,
        config_read_only: bool,
        config_notice: Option<String>,
    ) -> CoreResult<Arc<Self>> {
        let cache = Cache::open(
            archstore_core::config::paths::cache_dir(),
            cfg.cache.max_size_mb.saturating_mul(1024 * 1024),
        )?;
        let http = HttpClient::new(cfg.network.clone())?;
        let installed = InstalledIndex::new();

        let mut capabilities: Vec<(&'static str, Capability)> = Vec::new();

        let pacman = if cfg.sources.pacman_enabled {
            match PacmanBackend::spawn().await {
                Ok(b) => {
                    let cap = b.capability().clone();
                    capabilities.push(("官方仓库（pacman）", cap));
                    Some(Arc::new(b))
                }
                Err(e) => {
                    capabilities.push((
                        "官方仓库（pacman）",
                        Capability::unavailable(e.user_message()),
                    ));
                    None
                }
            }
        } else {
            capabilities.push((
                "官方仓库（pacman）",
                Capability::unavailable("已在设置中关闭"),
            ));
            None
        };

        let aur = if cfg.sources.aur_enabled {
            let helper = archstore_core::env::find_aur_helper(cfg.sources.aur_helper);
            match AurBackend::new(&http, Arc::clone(&cache), Arc::clone(&installed), &cfg).await {
                Ok(b) => {
                    let cap = match helper {
                        Some(k) => Capability::available_with(Some(format!(
                            "安装通过 {} 在用户身份下完成",
                            k.display()
                        ))),
                        None => Capability::available_with(Some(
                            "未检测到 paru/yay，AUR 安装功能置灰（查询不受影响）".into(),
                        )),
                    };
                    capabilities.push(("AUR", cap));
                    Some(b)
                }
                Err(e) => {
                    capabilities.push(("AUR", Capability::unavailable(e.user_message())));
                    None
                }
            }
        } else {
            capabilities.push(("AUR", Capability::unavailable("已在设置中关闭")));
            None
        };

        let flatpak = if cfg.sources.flatpak_enabled {
            let b = FlatpakBackend::new(Arc::clone(&http), Arc::clone(&cache), &cfg).await;
            capabilities.push(("Flatpak", b.capability().clone()));
            Some(b)
        } else {
            capabilities.push(("Flatpak", Capability::unavailable("已在设置中关闭")));
            None
        };

        let flathub = FlathubClient::new(Arc::clone(&http), Arc::clone(&cache));
        let translate =
            archstore_core::translate::TranslateClient::new(Arc::clone(&http), Arc::clone(&cache));
        // 把用户配置的翻译端点加入网络白名单（仅 https，由 config 校验保证）
        archstore_core::net::set_extra_hosts(if cfg.translation.api_endpoint.is_empty() {
            Vec::new()
        } else {
            archstore_core::net::host_of(&cfg.translation.api_endpoint)
                .into_iter()
                .collect()
        });
        let names = SoftwareNames::load();
        let helper = archstore_core::env::detect_helper()
            .map(|_| archstore_core::config::paths::helper_path());
        let pkexec = archstore_core::env::detect_pkexec();

        Ok(Arc::new(Self {
            config: cfg,
            cache,
            http,
            pacman,
            aur,
            flatpak,
            flathub,
            translate,
            installed,
            names,
            helper,
            pkexec,
            config_notice,
            config_read_only,
            capabilities,
        }))
    }

    /// 保存配置到磁盘（原子写 + 未知字段保留）。
    pub fn save_config(&self, cfg: &Config) -> CoreResult<()> {
        if self.config_read_only {
            return Err(CoreError::Config(
                "配置版本高于本程序支持值，已进入只读模式".into(),
            ));
        }
        cfg.save(&archstore_core::config::paths::config_file())
    }
}

/// 仅主线程使用的界面状态（GTK 单线程模型下用 RefCell 即可）。
pub struct AppState {
    /// 事务状态机
    pub tx: std::cell::RefCell<TxState>,
    /// 已构建但未确认的计划
    pub draft: std::cell::RefCell<Vec<TransactionPlan>>,
    /// 需要以用户身份执行的 AUR 请求
    pub aur: std::cell::RefCell<Vec<AurRequest>>,
    /// 依赖弹窗里用户的选择（包 -> 选择），构建安装计划时带上
    pub dep_selection: std::cell::RefCell<Option<(PackageId, DepSelection)>>,
    /// 详情页当前展示的软件：事务结束后用它重新拉取详情，让按钮状态（安装/已安装）跟着变
    pub current_detail: std::cell::RefCell<Option<PackageSummary>>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState").finish_non_exhaustive()
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            tx: std::cell::RefCell::new(TxState::Idle),
            draft: std::cell::RefCell::new(Vec::new()),
            aur: std::cell::RefCell::new(Vec::new()),
            dep_selection: std::cell::RefCell::new(None),
            current_detail: std::cell::RefCell::new(None),
        }
    }

    /// 把构建结果入队（冻结快照）。
    ///
    /// 只有 AUR 请求时**不进入事务状态机**：AUR 构建以用户身份在终端完成，
    /// 既不经过 pkexec 也没有 helper 的事件流（§5.3 的 MVP 决策）。
    pub fn enqueue(&self, outcome: archstore_core::plan::BuildOutcome) -> CoreResult<()> {
        *self.draft.borrow_mut() = outcome.plans.clone();
        *self.aur.borrow_mut() = outcome.aur.clone();
        if outcome.plans.is_empty() {
            if outcome.aur.is_empty() {
                return Err(CoreError::PlanRejected {
                    reason: "没有可执行的条目".into(),
                });
            }
            return Ok(());
        }
        let merged = merge_plans(&outcome.plans)?;
        let next = self.tx.borrow().clone().apply(TxEvent::Enqueue(merged))?;
        *self.tx.borrow_mut() = next;
        Ok(())
    }

    /// 是否只有 AUR 请求（没有需要提权的计划）。
    pub fn aur_only(&self) -> bool {
        self.draft.borrow().is_empty() && !self.aur.borrow().is_empty()
    }

    /// 清空队列（计划栏的"放弃/删除"按钮走这里）。
    pub fn reset(&self) {
        *self.draft.borrow_mut() = Vec::new();
        *self.aur.borrow_mut() = Vec::new();
        *self.dep_selection.borrow_mut() = None;
        // 先克隆、再回写：不能在同一个表达式里同时持有 borrow() 与 borrow_mut()。
        // 旧实现写成 `if let Ok(next) = self.tx.borrow().clone().apply(..)`，
        // 临时 Ref 会活到整个 if let 语句结束（含分支体），
        // 于是分支体里的 borrow_mut() 抛 "RefCell already borrowed" 并 panic ——
        // 用户点底栏的删除按钮就会崩（实测日志：state.rs:232）。
        let current = self.tx.borrow().clone();
        if let Ok(next) = current.apply(TxEvent::Reset) {
            *self.tx.borrow_mut() = next;
        }
    }

    pub fn plans(&self) -> Vec<TransactionPlan> {
        self.draft.borrow().clone()
    }

    pub fn aur_requests(&self) -> Vec<AurRequest> {
        self.aur.borrow().clone()
    }
}

/// 把多个计划合并成一个可入队的计划（同一时刻只允许一个事务）。
///
/// 合并规则：同 kind 的合并；不同 kind 依次执行时只入队第一个，
/// 其余的保留在 draft 中由 UI 说明（MVP 不做多阶段队列）。
pub fn merge_plans(plans: &[TransactionPlan]) -> CoreResult<TransactionPlan> {
    let Some(first) = plans.first() else {
        return Err(CoreError::PlanRejected {
            reason: "没有可执行的计划".into(),
        });
    };
    let mut merged = first.clone();
    for p in plans.iter().skip(1) {
        if p.kind != merged.kind {
            continue;
        }
        for item in &p.items {
            if merged.items.iter().any(|i| i.name == item.name) {
                continue;
            }
            merged.push(item.clone());
        }
    }
    merged.validate()?;
    Ok(merged)
}

/// 用安装选项构建计划（GUI 的唯一入口）。
pub async fn build_install(
    backends: &[Arc<dyn archstore_core::backend::PackageBackend>],
    targets: &[PackageId],
    options: &InstallOptions,
) -> CoreResult<archstore_core::plan::BuildOutcome> {
    archstore_core::plan::build_install_plan(backends, targets, options).await
}

impl std::fmt::Debug for Services {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Services")
            .field("helper", &self.helper)
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl Services {
    /// 按 source_kind 取出后端 trait 对象列表（计划构建使用）。
    pub fn backends(&self) -> Vec<Arc<dyn archstore_core::backend::PackageBackend>> {
        let mut out: Vec<Arc<dyn archstore_core::backend::PackageBackend>> = Vec::new();
        if let Some(p) = &self.pacman {
            out.push(p.clone());
        }
        if let Some(a) = &self.aur {
            out.push(a.clone());
        }
        if let Some(f) = &self.flatpak {
            out.push(f.clone());
        }
        out
    }

    /// 是否可以执行需要提权的事务。
    pub fn can_elevate(&self) -> bool {
        self.helper.is_some() && self.pkexec.is_some()
    }

    /// 是否可以执行**用户级** Flatpak 事务（不需要 pkexec，直接以当前用户跑 helper）。
    pub fn can_run_user_scope(&self) -> bool {
        self.helper.is_some()
    }

    /// 用户级事务不可用的原因。
    pub fn user_scope_reason(&self) -> Option<String> {
        if self.helper.is_some() {
            None
        } else {
            Some(
                "尚未安装 archstore-helper（/usr/lib/archstore/archstore-helper），Flatpak 用户级安装不可用"
                    .into(),
            )
        }
    }

    /// 提权不可用的原因（面向用户）。
    pub fn elevation_reason(&self) -> Option<String> {
        match (&self.helper, &self.pkexec) {
            (Some(_), Some(_)) => None,
            (None, _) => Some(
                "尚未安装 archstore-helper（/usr/lib/archstore/archstore-helper），安装与卸载功能不可用"
                    .into(),
            ),
            (_, None) => Some("未找到 pkexec（polkit），无法请求提权".into()),
        }
    }
}

/// 事务状态机的状态（§5.4）。
///
/// Idle -> Draft(计划非空) -> Confirmed(用户点执行) -> Authorized(polkit 通过)
///      -> Running -> { Succeeded | Failed | Cancelled }
/// 任何状态 -> Idle（清空队列）
#[derive(Debug, Clone, PartialEq)]
pub enum TxState {
    Idle,
    Draft {
        plan: Arc<TransactionPlan>,
    },
    Confirmed {
        plan: Arc<TransactionPlan>,
    },
    Authorized {
        plan: Arc<TransactionPlan>,
    },
    Running {
        plan: Arc<TransactionPlan>,
        progress: Progress,
    },
    Succeeded {
        summary: TxSummary,
    },
    Failed {
        error: String,
        log_tail: String,
    },
    Cancelled,
}

/// 事务内触发状态转移的事件。
#[derive(Debug, Clone)]
pub enum TxEvent {
    /// 计划入队（冻结一份不可变快照）
    Enqueue(TransactionPlan),
    /// 用户点击"执行"
    Confirm,
    /// polkit 通过
    Authorize,
    /// helper 开始输出
    Start,
    /// 进度更新
    Progress(Progress),
    /// 成功结束
    Succeed(TxSummary),
    /// 失败
    Fail { error: String, log_tail: String },
    /// 用户主动取消（仅在非 Running 状态允许）
    Cancel,
    /// 清空队列
    Reset,
}

/// 进度快照（§5.5：按阶段权重估算的总体进度）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Progress {
    /// 当前阶段："resolve" | "download" | "verify" | "install" | "remove" | …
    pub phase: String,
    /// 0..=100；None 表示未知
    pub percent: Option<u8>,
    /// 当前处理的包/文件
    pub detail: String,
    /// 原始日志行（GUI 侧再截断到 5000 行）
    pub log_lines: Vec<String>,
    /// 已完成的条目数（由 done 事件的统计填充）
    pub items_done: usize,
    pub items_total: usize,
}

impl Progress {
    /// 阶段权重（用于没有 percent 时估算总体进度）。
    pub fn phase_weight(phase: &str) -> u8 {
        match phase {
            "resolve" => 5,
            "keyring" => 10,
            "download" => 45,
            "verify" => 55,
            "conflict" => 60,
            "install" => 100,
            "upgrade" => 100,
            "remove" => 100,
            _ => 50,
        }
    }

    /// 综合进度：优先用 percent，否则用阶段权重，再否则用条目完成度。
    pub fn overall(&self) -> u8 {
        if let Some(p) = self.percent {
            return Self::phase_weight(&self.phase)
                .min(p.max(Self::phase_weight(&self.phase).saturating_sub(50)));
        }
        if let Some(percent) = self
            .items_done
            .checked_mul(100)
            .and_then(|n| n.checked_div(self.items_total))
        {
            return percent.min(100) as u8;
        }
        Self::phase_weight(&self.phase)
    }

    /// 追加日志（环形上限 5000 行）。
    pub fn push_log(&mut self, line: impl Into<String>) {
        if self.log_lines.len() >= Self::MAX_LOG_LINES {
            self.log_lines.remove(0);
        }
        self.log_lines.push(line.into());
    }

    /// 日志行数上限。
    pub const MAX_LOG_LINES: usize = 5000;

    /// 把日志截回上限。
    ///
    /// **为什么必须有这个函数**：状态机合并进度时如果只 append 不设上限，
    /// 日志会**指数增长** —— 实测旧实现每来一行日志，行数就翻一次倍
    /// （旧日志 + 「旧日志 + 新行」），flatpak 下载几秒就能吃光内存。
    pub fn trim_log(&mut self) {
        if self.log_lines.len() > Self::MAX_LOG_LINES {
            let drop = self.log_lines.len() - Self::MAX_LOG_LINES;
            self.log_lines.drain(..drop);
        }
    }
}

/// 事务结果摘要。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TxSummary {
    pub installed: usize,
    pub removed: usize,
    pub failed: usize,
    pub elapsed_ms: u64,
    pub status: String,
}

impl TxState {
    /// 是否正在执行（此时"执行"按钮必须禁用、队列只读）。
    pub fn is_running(&self) -> bool {
        matches!(self, TxState::Running { .. })
    }

    /// 是否可以开始新事务。
    pub fn accepts_new_plan(&self) -> bool {
        matches!(
            self,
            TxState::Idle | TxState::Succeeded { .. } | TxState::Failed { .. } | TxState::Cancelled
        ) || matches!(self, TxState::Draft { .. } | TxState::Confirmed { .. })
    }

    /// 当前挂起的计划（用于计划栏展示与写盘）。
    pub fn plan(&self) -> Option<Arc<TransactionPlan>> {
        match self {
            TxState::Draft { plan }
            | TxState::Confirmed { plan }
            | TxState::Authorized { plan }
            | TxState::Running { plan, .. } => Some(plan.clone()),
            _ => None,
        }
    }

    /// 计划是否可以执行（需要处于 Draft）。
    pub fn can_execute(&self) -> bool {
        matches!(self, TxState::Draft { .. })
    }

    /// 当前进度（仅运行中）。
    pub fn progress(&self) -> Option<&Progress> {
        match self {
            TxState::Running { progress, .. } => Some(progress),
            _ => None,
        }
    }

    /// 就地追加一行日志。
    ///
    /// 为什么不用 `apply(TxEvent::Progress(…))`：那需要先克隆一份完整进度
    /// （最多 5000 行日志）再合并，flatpak 下载时每行输出都要克隆一次，
    /// 既费内存又费 CPU。就地改一行是 O(1)。
    pub fn push_progress_log(&mut self, line: impl Into<String>) -> bool {
        match self {
            TxState::Running { progress, .. } => {
                progress.push_log(line);
                true
            }
            _ => false,
        }
    }

    /// 就地更新进度的阶段/百分比/详情。
    pub fn set_progress(&mut self, phase: &str, percent: Option<u8>, detail: &str) -> bool {
        match self {
            TxState::Running { progress, .. } => {
                progress.phase = phase.to_string();
                progress.percent = percent;
                progress.detail = detail.to_string();
                true
            }
            _ => false,
        }
    }

    /// 状态机转移函数：非法转移返回 Err 且不改变状态。
    pub fn apply(self, event: TxEvent) -> CoreResult<TxState> {
        use TxEvent as E;
        match (self, event) {
            // 任何状态 -> Idle
            (_, E::Reset) => Ok(TxState::Idle),

            // 入队：Running 期间拒绝（同一时刻只允许一个事务）
            (TxState::Running { .. }, E::Enqueue(_)) => Err(CoreError::PlanRejected {
                reason: "已有事务正在执行，请等待它结束".into(),
            }),
            (_, E::Enqueue(plan)) => {
                plan.validate()?;
                Ok(TxState::Draft {
                    plan: Arc::new(plan),
                })
            }

            // 确认
            (TxState::Draft { plan }, E::Confirm) => Ok(TxState::Confirmed { plan }),
            (state, E::Confirm) => Err(invalid(&state, "确认")),

            // 授权
            (TxState::Confirmed { plan }, E::Authorize) => Ok(TxState::Authorized { plan }),
            (state, E::Authorize) => Err(invalid(&state, "授权")),

            // 开始执行
            (TxState::Authorized { plan }, E::Start) => Ok(TxState::Running {
                plan,
                progress: Progress::default(),
            }),
            (state, E::Start) => Err(invalid(&state, "开始执行")),

            // 进度
            (TxState::Running { plan, progress }, E::Progress(next)) => {
                // next 已经是"旧进度 + 本次新增"的完整快照（调用方先 push_log 再传进来），
                // 这里**只能截断、不能再把旧日志接一遍**：
                //   lines = 旧日志 + (旧日志 + 新行)  =>  每来一行日志行数翻倍
                // 旧实现在 flatpak 下载时几秒钟就把内存吃光（用户实测反馈）。
                let _ = progress; // 旧进度只用于说明：不要再 append 它
                let mut merged = next;
                merged.trim_log();
                Ok(TxState::Running {
                    plan,
                    progress: merged,
                })
            }
            (state, E::Progress(_)) => Err(invalid(&state, "更新进度")),

            // 成功
            (TxState::Running { .. }, E::Succeed(summary)) => Ok(TxState::Succeeded { summary }),
            (state, E::Succeed(_)) => Err(invalid(&state, "完成")),

            // 失败
            (TxState::Running { .. }, E::Fail { error, log_tail }) => {
                Ok(TxState::Failed { error, log_tail })
            }
            (state, E::Fail { .. }) => Err(invalid(&state, "失败")),

            // 取消：Running 期间不允许（不杀 helper，杀 pacman 比让它跑完更危险）
            (TxState::Running { .. }, E::Cancel) => Err(CoreError::Unsupported(
                "事务正在执行，无法取消；可以最小化到后台继续执行".into(),
            )),
            (_, E::Cancel) => Ok(TxState::Cancelled),
        }
    }
}

fn invalid(state: &TxState, action: &str) -> CoreError {
    CoreError::Internal(format!("在状态 {state:?} 下不能{action}"))
}

/// helper 输出的一行 JSON 事件（§5.2 输出协议）。
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum HelperEvent {
    Start {
        plan_schema: u32,
        items: usize,
        kind: String,
    },
    Progress {
        phase: String,
        #[serde(default)]
        percent: Option<u8>,
        #[serde(default)]
        detail: String,
    },
    Log {
        level: String,
        line: String,
    },
    Error {
        code: String,
        message: String,
    },
    NeedsTty {
        #[serde(default)]
        code: String,
        hint: String,
    },
    Done {
        status: String,
        #[serde(default)]
        installed: usize,
        #[serde(default)]
        removed: usize,
        #[serde(default)]
        failed: usize,
        #[serde(default)]
        elapsed_ms: u64,
    },
}

impl HelperEvent {
    /// 解析一行输出；无法识别时返回 None（helper 之外的噪声不应中断解析）。
    pub fn parse(line: &str) -> Option<Self> {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            return None;
        }
        serde_json::from_str(trimmed)
            .map_err(|e| tracing::debug!(error = %e, line = %trimmed, "无法解析 helper 事件"))
            .ok()
    }

    /// 转成进度快照（Start/Done 用统计信息）。
    pub fn to_progress(&self) -> Option<Progress> {
        match self {
            HelperEvent::Progress {
                phase,
                percent,
                detail,
            } => Some(Progress {
                phase: phase.clone(),
                percent: *percent,
                detail: detail.clone(),
                ..Default::default()
            }),
            HelperEvent::Log { line, .. } => Some(Progress {
                phase: String::new(),
                percent: None,
                detail: String::new(),
                log_lines: vec![line.clone()],
                items_done: 0,
                items_total: 0,
            }),
            _ => None,
        }
    }
}

/// "已完成"标记的路径。
///
/// 只有"存在计划文件但没有对应的 done 记录"才说明上次事务可能中途夭折（§5.4 规则 4）。
/// 正常情况下事务结束后会写入这个标记，因此不会每次启动都弹横幅。
pub fn plan_done_marker(cache_dir: &std::path::Path) -> std::path::PathBuf {
    cache_dir.join("plans").join("last.done")
}

/// 事务成功结束后写入"已完成"标记。
pub fn mark_plan_done(cache_dir: &std::path::Path) -> std::io::Result<()> {
    let path = plan_done_marker(cache_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, archstore_core::model::plan::now_unix().to_string())
}

/// 清除标记（开始新事务时调用）。
pub fn clear_plan_done(cache_dir: &std::path::Path) {
    let _ = std::fs::remove_file(plan_done_marker(cache_dir));
}

/// 崩溃恢复：读取上次的计划文件（§5.4 规则 4）。
///
/// 仅当计划文件的 mtime 晚于 done 标记时才返回（即上次确实没有正常收尾）。
pub fn load_recovered_plan(cache_dir: &std::path::Path) -> Option<TransactionPlan> {
    let path = TransactionPlan::last_plan_path(cache_dir);
    if !path.exists() {
        return None;
    }
    let marker = plan_done_marker(cache_dir);
    if marker.exists() {
        let plan_time = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let done_time = std::fs::metadata(&marker).and_then(|m| m.modified()).ok();
        // 标记比计划新 -> 上次事务已正常结束，不需要提醒
        if let (Some(p), Some(d)) = (plan_time, done_time)
            && d >= p
        {
            return None;
        }
    }
    match archstore_core::model::plan::read_plan_file(&path) {
        Ok(plan) => Some(plan),
        Err(e) => {
            tracing::warn!(error = %e, "无法读取上次的计划文件");
            None
        }
    }
}

/// 把计划写入临时文件并返回路径（0700 目录 + 0600 文件，§5.4 规则 2）。
pub fn write_plan_file(cache_dir: &std::path::Path, plan: &TransactionPlan) -> CoreResult<PathBuf> {
    let dir = cache_dir.join("plans");
    let name = format!("plan-{}.json", archstore_core::model::plan::now_unix());
    let path = plan.write_to_dir(&dir, &name)?;
    // 同时保留最后一份，供崩溃恢复使用
    let last = TransactionPlan::last_plan_path(cache_dir);
    let _ = std::fs::copy(&path, &last);
    Ok(path)
}

/// 计划栏要展示的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanLine {
    pub text: String,
    pub kind: PlanKind,
}

/// 把计划转换成计划栏文案（§7.1：安装 firefox、GIMP（2 项，下载 92 MB））。
pub fn plan_bar_text(plans: &[TransactionPlan], aur: &[AurRequest]) -> String {
    let mut items: Vec<String> = Vec::new();
    let mut count = 0usize;
    for p in plans {
        for i in &p.items {
            items.push(i.name.clone());
        }
        count += p.len();
    }
    for a in aur {
        count += a.packages.len();
        items.extend(a.packages.iter().map(|p| format!("{p}（AUR）")));
    }
    if count == 0 {
        return String::new();
    }
    let head: Vec<String> = items.iter().take(3).cloned().collect();
    let mut text = head.join("、");
    if items.len() > 3 {
        text.push_str(&format!(" 等 {} 项", items.len()));
    }
    format!("{text}（共 {count} 项）")
}

/// 依赖弹窗的分组（§8.2）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DepGroups {
    pub runtime: Vec<archstore_core::model::DependencyInfo>,
    pub build: Vec<archstore_core::model::DependencyInfo>,
    pub optional: Vec<archstore_core::model::DependencyInfo>,
    pub flatpak: Vec<archstore_core::model::DependencyInfo>,
}

impl DepGroups {
    pub fn from_deps(deps: &[archstore_core::model::DependencyInfo]) -> Self {
        use archstore_core::model::DepKind;
        let mut g = Self::default();
        for d in deps {
            match d.kind {
                DepKind::Runtime => g.runtime.push(d.clone()),
                DepKind::Make | DepKind::Check => g.build.push(d.clone()),
                DepKind::Optional => g.optional.push(d.clone()),
                DepKind::RuntimeRef | DepKind::Extension => g.flatpak.push(d.clone()),
            }
        }
        g
    }

    pub fn total(&self) -> usize {
        self.runtime.len() + self.build.len() + self.optional.len() + self.flatpak.len()
    }

    /// 是否有"依赖信息不完整"（缺失且体积未知，§8.2 规则 5）。
    pub fn has_unknown(&self) -> bool {
        self.runtime
            .iter()
            .chain(self.build.iter())
            .any(|d| d.missing && d.size.is_none())
    }

    /// 勾选后的下载体积合计。
    pub fn download_size(&self, include_optional: &[String]) -> u64 {
        let mut total: u64 = self
            .runtime
            .iter()
            .chain(self.build.iter())
            .filter(|d| d.missing)
            .filter_map(|d| d.size)
            .sum();
        for d in &self.optional {
            if include_optional.iter().any(|o| o == &d.name) {
                total += d.size.unwrap_or(0);
            }
        }
        total
    }
}

/// 安全公告匹配（§9.2）：更新页只在可见时拉取一次。
pub fn matched_advisories<'a>(
    advisories: &'a [Advisory],
    package: &str,
    current_version: &str,
) -> Vec<&'a Advisory> {
    advisories
        .iter()
        .filter(|a| a.matches(package, current_version))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use archstore_core::model::plan::{Installation, PlanItem};

    fn plan() -> TransactionPlan {
        let mut p = TransactionPlan::new(PlanKind::PacmanSync);
        p.push(PlanItem::official("extra", "firefox"));
        p
    }

    fn aur_plan() -> TransactionPlan {
        let mut p = TransactionPlan::new(PlanKind::FlatpakInstall);
        p.push(PlanItem::flatpak(
            "flathub",
            Installation::System,
            "org.mozilla.firefox",
        ));
        p
    }

    #[test]
    fn happy_path_transitions() {
        let s = TxState::Idle
            .apply(TxEvent::Enqueue(plan()))
            .expect("enqueue");
        assert!(matches!(s, TxState::Draft { .. }));
        assert!(s.can_execute());

        let s = s.apply(TxEvent::Confirm).expect("confirm");
        assert!(matches!(s, TxState::Confirmed { .. }));
        assert!(!s.can_execute(), "确认后不能再点执行");

        let s = s.apply(TxEvent::Authorize).expect("authorize");
        let s = s.apply(TxEvent::Start).expect("start");
        assert!(s.is_running());

        let s = s
            .apply(TxEvent::Succeed(TxSummary {
                installed: 1,
                status: "ok".into(),
                ..Default::default()
            }))
            .expect("succeed");
        assert!(matches!(s, TxState::Succeeded { .. }));
        assert!(s.accepts_new_plan());
    }

    /// 回归：底栏的"删除/放弃"按钮调用 `AppState::reset()»。
    ///
    /// 旧实现是 `if let Ok(next) = self.tx.borrow().clone().apply(..) { *self.tx.borrow_mut() = next; }» ——
    /// 临时 `Ref» 活到整个 if let 语句结束（含分支体），分支体里的 `borrow_mut()» 直接 panic：
    /// "RefCell already borrowed"，用户点一下删除按钮整个应用就崩（实测日志 state.rs:232）。
    #[test]
    fn app_state_reset_does_not_panic() {
        let state = AppState::new();
        *state.tx.borrow_mut() = TxState::Idle
            .apply(TxEvent::Enqueue(plan()))
            .expect("enqueue")
            .apply(TxEvent::Confirm)
            .expect("confirm")
            .apply(TxEvent::Authorize)
            .expect("authorize")
            .apply(TxEvent::Start)
            .expect("start");
        assert!(state.tx.borrow().is_running());

        state.reset();
        assert!(matches!(*state.tx.borrow(), TxState::Idle));
        // 再点一次也不能出问题（幂等）
        state.reset();
        assert!(matches!(*state.tx.borrow(), TxState::Idle));
        assert!(state.plans().is_empty());
        assert!(state.aur_requests().is_empty());
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        assert!(TxState::Idle.apply(TxEvent::Confirm).is_err());
        assert!(TxState::Idle.apply(TxEvent::Authorize).is_err());
        assert!(TxState::Idle.apply(TxEvent::Start).is_err());
        assert!(
            TxState::Idle
                .apply(TxEvent::Succeed(TxSummary::default()))
                .is_err()
        );
        let draft = TxState::Idle
            .apply(TxEvent::Enqueue(plan()))
            .expect("draft");
        assert!(draft.clone().apply(TxEvent::Start).is_err());
        assert!(draft.apply(TxEvent::Succeed(TxSummary::default())).is_err());
    }

    #[test]
    fn only_one_transaction_at_a_time() {
        let running = TxState::Idle
            .apply(TxEvent::Enqueue(plan()))
            .and_then(|s| s.apply(TxEvent::Confirm))
            .and_then(|s| s.apply(TxEvent::Authorize))
            .and_then(|s| s.apply(TxEvent::Start))
            .expect("running");
        let err = running
            .clone()
            .apply(TxEvent::Enqueue(plan()))
            .expect_err("must reject");
        assert!(matches!(err, CoreError::PlanRejected { .. }));
        assert!(running.is_running());
    }

    #[test]
    fn cancel_is_blocked_while_running() {
        let running = TxState::Idle
            .apply(TxEvent::Enqueue(plan()))
            .and_then(|s| s.apply(TxEvent::Confirm))
            .and_then(|s| s.apply(TxEvent::Authorize))
            .and_then(|s| s.apply(TxEvent::Start))
            .expect("running");
        let err = running.apply(TxEvent::Cancel).expect_err("cannot cancel");
        assert!(matches!(err, CoreError::Unsupported(_)));

        let draft = TxState::Idle
            .apply(TxEvent::Enqueue(plan()))
            .expect("draft");
        assert!(matches!(
            draft.apply(TxEvent::Cancel).expect("cancel"),
            TxState::Cancelled
        ));
    }

    #[test]
    fn reset_from_any_state() {
        for state in [
            TxState::Idle,
            TxState::Succeeded {
                summary: TxSummary::default(),
            },
            TxState::Failed {
                error: "x".into(),
                log_tail: String::new(),
            },
            TxState::Cancelled,
        ] {
            assert_eq!(state.apply(TxEvent::Reset).expect("reset"), TxState::Idle);
        }
    }

    #[test]
    fn enqueue_rejects_invalid_plan() {
        let empty = TransactionPlan::new(PlanKind::PacmanSync);
        assert!(TxState::Idle.apply(TxEvent::Enqueue(empty)).is_err());
    }

    #[test]
    fn plan_snapshot_is_frozen() {
        let s = TxState::Idle
            .apply(TxEvent::Enqueue(plan()))
            .expect("draft");
        let snapshot = s.plan().expect("snapshot");
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot.items[0].name, "firefox");
        assert_eq!(snapshot.kind, PlanKind::PacmanSync);
    }

    #[test]
    fn helper_events_parse_from_protocol_lines() {
        let start = HelperEvent::parse(
            r#"{"event":"start","plan_schema":1,"items":3,"kind":"pacman-sync"}"#,
        );
        assert!(matches!(start, Some(HelperEvent::Start { items: 3, .. })));
        let progress = HelperEvent::parse(
            r#"{"event":"progress","phase":"download","percent":42,"detail":"firefox-155.0.1-1-x86_64.pkg.tar.zst"}"#,
        );
        match progress {
            Some(HelperEvent::Progress {
                phase,
                percent,
                detail,
            }) => {
                assert_eq!(phase, "download");
                assert_eq!(percent, Some(42));
                assert!(detail.contains("firefox"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            HelperEvent::parse(r#"{"event":"log","level":"info","line":"正在检查密钥环…"}"#),
            Some(HelperEvent::Log { .. })
        ));
        assert!(matches!(
            HelperEvent::parse(r#"{"event":"error","code":"LOCKED","message":"数据库被锁定"}"#),
            Some(HelperEvent::Error { .. })
        ));
        assert!(matches!(
            HelperEvent::parse(r#"{"event":"needs_tty","hint":"需要导入 PGP 密钥"}"#),
            Some(HelperEvent::NeedsTty { .. })
        ));
        assert!(matches!(
            HelperEvent::parse(r#"{"event":"done","status":"ok","installed":3,"failed":0}"#),
            Some(HelperEvent::Done { installed: 3, .. })
        ));
    }

    #[test]
    fn helper_event_parsing_ignores_noise() {
        assert!(HelperEvent::parse("warning: something").is_none());
        assert!(HelperEvent::parse("").is_none());
        assert!(HelperEvent::parse("{not json").is_none());
        assert!(
            HelperEvent::parse(r#"{"event":"unknown_future_event","x":1}"#).is_none(),
            "未知事件必须被忽略而不是崩溃"
        );
    }

    #[test]
    fn progress_overall_uses_phase_and_percent() {
        let p = Progress {
            phase: "install".into(),
            percent: None,
            ..Default::default()
        };
        assert_eq!(p.overall(), 100);
        let p = Progress {
            phase: "download".into(),
            ..Default::default()
        };
        assert_eq!(p.overall(), 45);
        let p = Progress {
            phase: String::new(),
            items_total: 4,
            items_done: 1,
            ..Default::default()
        };
        assert_eq!(p.overall(), 25);
    }

    #[test]
    fn progress_log_is_bounded() {
        let mut p = Progress::default();
        for i in 0..6000 {
            p.push_log(format!("line {i}"));
        }
        assert_eq!(p.log_lines.len(), 5000);
        assert_eq!(p.log_lines.last().map(|s| s.as_str()), Some("line 5999"));
    }

    /// 回归：进度事件合并**绝不能**让日志指数增长。
    ///
    /// 旧实现把「旧日志 + (旧日志 + 新行)」接在一起，每来一行日志行数就翻倍，
    /// flatpak 下载几秒钟就把内存吃光（用户实测反馈）。
    #[test]
    fn progress_events_do_not_grow_logs_exponentially() {
        let mut plan = TransactionPlan::new(PlanKind::FlatpakInstall);
        plan.push(PlanItem::flatpak(
            String::from("flathub"),
            Installation::System,
            String::from("org.mozilla.firefox"),
        ));
        let mut state = TxState::Idle
            .apply(TxEvent::Enqueue(plan))
            .expect("enqueue");
        state = state.apply(TxEvent::Confirm).expect("confirm");
        state = state.apply(TxEvent::Authorize).expect("authorize");
        state = state.apply(TxEvent::Start).expect("start");

        // 模拟 200 行 flatpak 输出：每行都是"旧进度 + 新行"的完整快照。
        // 旧实现（apply 里再 append 一次旧日志）到这里已经是 2^200 行，早就 OOM；
        // 正确实现必须严格线性：第 n 次事件后正好 n 行。
        const EVENTS: usize = 200;
        for i in 0..EVENTS {
            let mut next = state.progress().cloned().expect("running");
            next.push_log(format!("progress line {i}"));
            state = state.apply(TxEvent::Progress(next)).expect("progress");
            assert_eq!(
                state.progress().expect("running").log_lines.len(),
                i + 1,
                "日志行数必须线性增长（每行日志 +1），第 {i} 次事件后已经不是"
            );
        }
        let lines = &state.progress().expect("running").log_lines;
        assert!(lines.len() <= Progress::MAX_LOG_LINES);
        assert_eq!(lines.last().map(String::as_str), Some("progress line 199"));
    }

    /// 就地更新 API 与状态机合并必须等价（且不复制整个进度）。
    #[test]
    fn in_place_progress_update_is_bounded() {
        let mut plan = TransactionPlan::new(PlanKind::FlatpakInstall);
        plan.push(PlanItem::flatpak(
            String::from("flathub"),
            Installation::System,
            String::from("org.mozilla.firefox"),
        ));
        let mut state = TxState::Idle
            .apply(TxEvent::Enqueue(plan))
            .expect("enqueue");
        state = state.apply(TxEvent::Confirm).expect("confirm");
        state = state.apply(TxEvent::Authorize).expect("authorize");
        state = state.apply(TxEvent::Start).expect("start");
        for i in 0..8000 {
            assert!(state.push_progress_log(format!("line {i}")));
        }
        assert!(state.set_progress("download", Some(42), "firefox.flatpak"));
        let p = state.progress().expect("running");
        assert_eq!(p.log_lines.len(), Progress::MAX_LOG_LINES);
        assert_eq!(p.phase, "download");
        assert_eq!(p.percent, Some(42));
    }

    #[test]
    fn done_marker_suppresses_crash_banner() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut plan = TransactionPlan::new(PlanKind::PacmanSync);
        plan.push(PlanItem::official("extra", "vim"));
        let _ = crate::state::write_plan_file(dir.path(), &plan).expect("plan file");

        // 没有 done 标记 -> 视为上次未完成
        assert!(
            load_recovered_plan(dir.path()).is_some(),
            "缺少 done 记录时必须提示"
        );

        // 写入标记后不再提示（标记比计划新）
        std::thread::sleep(std::time::Duration::from_millis(20));
        mark_plan_done(dir.path()).expect("mark");
        assert!(
            load_recovered_plan(dir.path()).is_none(),
            "已有 done 记录时不应再提示"
        );

        // 清除标记后重新提示
        clear_plan_done(dir.path());
        assert!(load_recovered_plan(dir.path()).is_some());
    }
    #[test]
    fn plan_bar_text_summarizes() {
        let mut p = TransactionPlan::new(PlanKind::PacmanSync);
        p.push(PlanItem::official("extra", "firefox"));
        p.push(PlanItem::official("extra", "gimp"));
        let aur = vec![AurRequest {
            action: archstore_core::plan::AurAction::Install,
            packages: vec!["yay".into()],
        }];
        let text = plan_bar_text(&[p], &aur);
        assert!(text.contains("firefox"));
        assert!(text.contains("AUR"));
        assert!(text.contains("共 3 项"));
        assert_eq!(plan_bar_text(&[], &[]), "");
    }

    #[test]
    fn plan_bar_text_truncates_long_lists() {
        let mut p = TransactionPlan::new(PlanKind::PacmanSync);
        for n in ["a", "b", "c", "d", "e"] {
            p.push(PlanItem::official("extra", n));
        }
        let text = plan_bar_text(&[p], &[]);
        assert!(text.contains("等 5 项"));
    }

    #[test]
    fn dep_groups_split_by_kind() {
        use archstore_core::model::{DepKind, DependencyInfo};
        let mut optional = DependencyInfo::from_expr("ffmpeg: 视频解码", DepKind::Optional);
        optional.missing = true;
        optional.size = Some(10 * 1024 * 1024);
        let deps = vec![
            DependencyInfo::from_expr("gtk3", DepKind::Runtime),
            DependencyInfo::from_expr("go", DepKind::Make),
            optional,
            DependencyInfo::from_expr("org.freedesktop.Platform", DepKind::RuntimeRef),
        ];
        let g = DepGroups::from_deps(&deps);
        assert_eq!(g.total(), 4);
        assert_eq!(g.runtime.len(), 1);
        assert_eq!(g.build.len(), 1);
        assert_eq!(g.optional.len(), 1);
        assert_eq!(g.flatpak.len(), 1);
        assert!(
            g.download_size(&["ffmpeg".to_string()]) > 0,
            "勾选可选依赖后应有体积"
        );
    }

    #[test]
    fn dep_groups_detect_unknown_sizes() {
        use archstore_core::model::{DepKind, DependencyInfo};
        let mut d = DependencyInfo::from_expr("mystery", DepKind::Runtime);
        d.missing = true;
        d.size = None;
        let g = DepGroups::from_deps(&[d]);
        assert!(g.has_unknown());

        let mut d = DependencyInfo::from_expr("known", DepKind::Runtime);
        d.missing = true;
        d.size = Some(10);
        assert!(!DepGroups::from_deps(&[d]).has_unknown());
    }

    #[test]
    fn advisory_matching_uses_version_ranges() {
        let advisories = vec![Advisory {
            name: "AVG-1".into(),
            packages: vec!["vim".into()],
            affected: "9.0.1224-1".into(),
            fixed: "9.0.1225-1".into(),
            ..Default::default()
        }];
        assert_eq!(
            matched_advisories(&advisories, "vim", "9.0.1224-1").len(),
            1
        );
        assert_eq!(
            matched_advisories(&advisories, "vim", "9.0.1225-1").len(),
            0
        );
        assert_eq!(
            matched_advisories(&advisories, "nano", "9.0.1224-1").len(),
            0
        );
    }

    #[test]
    fn services_report_elevation_reason() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let cfg = Config::default();
        let cache = Cache::open(dir.path().join("c"), 1 << 20).expect("cache");
        let services = Services {
            config: cfg,
            cache,
            http: HttpClient::new(archstore_core::config::NetworkConfig::default()).expect("http2"),
            pacman: None,
            aur: None,
            flatpak: None,
            translate: archstore_core::translate::TranslateClient::new(
                HttpClient::new(archstore_core::config::NetworkConfig::default()).expect("http4"),
                Cache::open(dir.path().join("c3"), 1 << 20).expect("cache3"),
            ),
            flathub: FlathubClient::new(
                HttpClient::new(archstore_core::config::NetworkConfig::default()).expect("http3"),
                Cache::open(dir.path().join("c2"), 1 << 20).expect("cache2"),
            ),
            installed: InstalledIndex::new(),
            names: SoftwareNames::default(),
            helper: None,
            pkexec: None,
            config_notice: None,
            config_read_only: false,
            capabilities: Vec::new(),
        };
        assert!(!services.can_elevate());
        assert!(services.elevation_reason().is_some());
        assert!(services.backends().is_empty());
    }
}
