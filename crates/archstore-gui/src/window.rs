//! 主窗口（§7.1）：AdwApplicationWindow -> AdwToastOverlay -> AdwNavigationSplitView。
//!
//! 布局：
//! ┌ HeaderBar：搜索框 · 刷新 · 更新(N) · 设置 ┐
//! ├ 侧栏（发现 / 我的）│ 内容区 AdwViewStack  ┤
//! └ 计划栏 + 进度面板 ────────────────────────┘
//!
//! 窄窗口（< 600sp）通过 AdwBreakpoint 自动折叠，不手写尺寸判断。

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::prelude::*;
use libadwaita as adw;

use archstore_core::backend::{PackageBackend, SearchScope};
use archstore_core::config::Config;
use archstore_core::model::plan::PlanKind;
use archstore_core::model::{PackageId, PackageSummary};
use archstore_core::net::CancelToken;
use archstore_core::plan::InstallOptions;

use crate::pages::category::CategoryPage;
use crate::pages::detail::DetailPage;
use crate::pages::home::HomePage;
use crate::pages::installed::InstalledPage;
use crate::pages::search::SearchPage;
use crate::pages::settings::{SettingsCallbacks, SettingsPage};
use crate::pages::updates::UpdatesPage;
use crate::runtime;
use crate::state::{AppState, Services, TxEvent, TxState};
use crate::ui;
use crate::widgets::RowContext;
use crate::widgets::plan_bar::{self, PlanBar, PlanBarCallbacks};
use crate::widgets::progress_panel::{self, ProgressPanel};

/// 侧栏导航项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Home,
    Category,
    Aur,
    Flatpak,
    Installed,
    Updates,
    Settings,
}

impl Nav {
    fn all() -> [(Nav, &'static str, &'static str); 7] {
        [
            (Nav::Home, "发现 · 首页", "go-home-symbolic"),
            (Nav::Category, "发现 · 分类", "view-grid-symbolic"),
            (
                Nav::Aur,
                "发现 · AUR 社区",
                "system-software-update-symbolic",
            ),
            (
                Nav::Flatpak,
                "发现 · Flatpak",
                "application-x-executable-symbolic",
            ),
            (Nav::Installed, "我的 · 已安装", "drive-harddisk-symbolic"),
            (
                Nav::Updates,
                "我的 · 可更新",
                "software-update-available-symbolic",
            ),
            (Nav::Settings, "设置", "emblem-system-symbolic"),
        ]
    }

    fn page_name(self) -> &'static str {
        match self {
            Nav::Home => "home",
            Nav::Category => "category",
            Nav::Aur => "aur",
            Nav::Flatpak => "flatpak",
            Nav::Installed => "installed",
            Nav::Updates => "updates",
            Nav::Settings => "settings",
        }
    }
}

/// 主窗口的全部控件与状态。
pub struct MainWindow {
    pub window: adw::ApplicationWindow,
    pub toast: adw::ToastOverlay,
    pub split: adw::NavigationSplitView,
    pub stack: adw::ViewStack,
    pub sidebar: gtk::ListBox,
    pub state: Rc<AppState>,
    pub services: RefCell<Option<std::sync::Arc<Services>>>,
    pub rows: Rc<RowContext>,
    pub home: Rc<HomePage>,
    pub category: Rc<CategoryPage>,
    pub installed: Rc<InstalledPage>,
    pub updates: Rc<UpdatesPage>,
    pub search: Rc<SearchPage>,
    pub detail: Rc<DetailPage>,
    pub settings: Rc<SettingsPage>,
    pub plan_bar: Rc<PlanBar>,
    pub progress: Rc<ProgressPanel>,
    pub search_entry: gtk::SearchEntry,
    pub updates_button: gtk::Button,
    pub banner: adw::Banner,
    /// 侧栏"可更新"右侧的计数
    pub updates_count: gtk::Label,
    pub current_nav: RefCell<Nav>,
    /// 详情页的导航页（push 到内层 NavigationView 上，返回由它负责）
    pub detail_nav: adw::NavigationPage,
    /// 内容区的导航栈（详情页 push/pop 用；NavigationSplitView 自身不提供 push）
    pub nav_view: adw::NavigationView,
}

impl std::fmt::Debug for MainWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MainWindow").finish_non_exhaustive()
    }
}

/// 构造主窗口。
pub fn build(app: &adw::Application) -> Rc<MainWindow> {
    let loaded = archstore_core::config::Config::load_default();
    let cfg = loaded.config.sanitized();

    let rows = Rc::new(RowContext::new(cfg.appearance.icon_size.pixels()));
    let state = Rc::new(AppState::new());

    // ---------- 页面 ----------
    // 页面在构造时需要回调，而回调又需要 Rc<MainWindow>：用 Weak 打破这个循环，
    // 回调在真正触发时才 upgrade（此时 MainWindow 一定已经构造完成）。
    let weak: WeakSlot = Rc::new(RefCell::new(None));

    let home = Rc::new({
        let w = weak.clone();
        HomePage::new(
            &rows,
            {
                let w = w.clone();
                move |s| open_detail_lazy(&w, s)
            },
            nav_to_settings(&weak),
            nav_to_settings(&weak),
        )
    });

    let installed = Rc::new({
        let w = weak.clone();
        InstalledPage::new(
            &rows,
            {
                let w = w.clone();
                move |s| open_detail_lazy(&w, s)
            },
            retry_lazy(&weak, reload_installed),
            nav_to_settings(&weak),
        )
    });

    let updates = Rc::new({
        let w = weak.clone();
        UpdatesPage::new(
            &rows,
            {
                let w = w.clone();
                move |s| open_detail_lazy(&w, s)
            },
            retry_lazy(&weak, reload_updates),
            nav_to_settings(&weak),
        )
    });

    let search_entry = gtk::SearchEntry::new();
    search_entry.set_placeholder_text(Some(&ui::t("搜索软件…")));
    search_entry.set_hexpand(true);

    let search = Rc::new({
        let w = weak.clone();
        SearchPage::new(
            &rows,
            search_entry.clone(),
            {
                let w = w.clone();
                move |s| open_detail_lazy(&w, s)
            },
            search_retry(&weak),
            nav_to_settings(&weak),
            Box::new(|_q| {}),
        )
    });

    let category = Rc::new({
        let w = weak.clone();
        CategoryPage::new(
            &rows,
            {
                let w = w.clone();
                move |s| open_detail_lazy(&w, s)
            },
            retry_lazy(&weak, reload_categories),
            nav_to_settings(&weak),
        )
    });

    let detail = Rc::new(DetailPage::new());
    let detail_nav = adw::NavigationPage::builder()
        .title(ui::t("详情"))
        .child(&detail.root)
        .build();

    let settings = Rc::new(SettingsPage::new(
        SettingsCallbacks {
            on_change: settings_changed(&weak),
            on_test_connection: test_connection(&weak),
            on_clear_cache: clear_cache(&weak),
            on_doctor: show_doctor(&weak),
            on_redetect: redetect(&weak),
        },
        &cfg,
        &[],
        &[],
    ));

    // ---------- ViewStack ----------
    let stack = adw::ViewStack::new();
    stack.add_titled(&home.page.shell.overlay, Some("home"), &ui::t("首页"));
    stack.add_titled(
        &category.page.shell.overlay,
        Some("category"),
        &ui::t("分类"),
    );
    // 注意：GTK 中一个控件只能有一个父容器。"AUR 社区"与"Flatpak"不注册独立页面，
    // 而是复用分类页并按来源过滤（否则会出现 GLib-GObject-CRITICAL 且页面不可用）。
    stack.add_titled(
        &installed.page.shell.overlay,
        Some("installed"),
        &ui::t("已安装"),
    );
    stack.add_titled(
        &updates.page.shell.overlay,
        Some("updates"),
        &ui::t("可更新"),
    );
    stack.add_titled(&settings.root, Some("settings"), &ui::t("设置"));
    // 搜索页：来源开关常驻在结果区之上，空态只替换结果区
    stack.add_titled(&search.root, Some("search"), &ui::t("搜索"));

    // ---------- 侧栏 ----------
    let count = gtk::Label::new(Some("0"));
    let sidebar = gtk::ListBox::new();
    sidebar.set_selection_mode(gtk::SelectionMode::Single);
    sidebar.add_css_class("navigation-sidebar");
    sidebar.set_margin_top(6);
    sidebar.set_margin_bottom(6);
    for (nav, title, icon) in Nav::all() {
        let row = gtk::ListBoxRow::new();
        row.set_activatable(true);
        let bx = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        bx.set_margin_top(8);
        bx.set_margin_bottom(8);
        bx.set_margin_start(8);
        bx.set_margin_end(8);
        let img = gtk::Image::from_icon_name(icon);
        bx.append(&img);
        let label = ui::label(&ui::t(title));
        label.set_hexpand(true);
        bx.append(&label);
        if nav == Nav::Updates {
            count.add_css_class("sidebar-count");
            bx.append(&count);
        }
        row.set_child(Some(&bx));
        row.set_widget_name(nav.page_name());
        sidebar.append(&row);
    }

    let sidebar_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let side_title = gtk::Label::builder()
        .label(ui::t("软件来源"))
        .xalign(0.0)
        .css_classes(["heading"])
        .margin_top(12)
        .margin_start(12)
        .build();
    sidebar_box.append(&side_title);
    sidebar_box.append(&sidebar);

    // 分类列表（pacman 包组 + AUR 关键词 + Flatpak 分类）放在导航下方
    let category_title = gtk::Label::builder()
        .label(ui::t("分类"))
        .xalign(0.0)
        .css_classes(["heading"])
        .margin_top(18)
        .margin_start(12)
        .build();
    sidebar_box.append(&category_title);
    sidebar_box.append(category.list_widget());
    let side_scroll = gtk::ScrolledWindow::builder()
        .child(&sidebar_box)
        .vexpand(true)
        .build();

    // ---------- 计划栏 + 进度面板 ----------
    let plan_bar = Rc::new(PlanBar::new(PlanBarCallbacks {
        on_execute: {
            let w = weak.clone();
            Box::new(move || {
                if let Some(m) = upgrade(&w) {
                    execute_plan(&m);
                }
            })
        },
        on_details: {
            let w = weak.clone();
            Box::new(move || {
                if let Some(m) = upgrade(&w) {
                    let plans = m.state.plans();
                    plan_bar::show_plan_details(&m.window, &plans, &m.state.aur_requests());
                }
            })
        },
        on_discard: {
            let w = weak.clone();
            Box::new(move || {
                if let Some(m) = upgrade(&w) {
                    m.state.reset();
                    refresh_plan_bar(&m);
                    m.progress.reset();
                }
            })
        },
    }));
    let progress = Rc::new(ProgressPanel::new());

    let bottom = gtk::Box::new(gtk::Orientation::Vertical, 0);
    bottom.append(&progress.root);
    bottom.append(&plan_bar.root);

    // ---------- 主布局 ----------
    let sidebar_page = adw::NavigationPage::builder()
        .title(ui::t("导航"))
        .child(&side_scroll)
        .build();
    // 内容区使用 NavigationView：详情页 push 上去，返回由它负责
    let nav_view = adw::NavigationView::new();
    let root_page = adw::NavigationPage::builder()
        .title("ArchStore")
        .child(&stack)
        .tag("root")
        .build();
    nav_view.add(&root_page);
    nav_view.add(&detail_nav);
    nav_view.pop_to_tag("root");

    let content_page = adw::NavigationPage::builder()
        .title("ArchStore")
        .child(&nav_view)
        .build();
    let split = adw::NavigationSplitView::builder()
        .sidebar(&sidebar_page)
        .content(&content_page)
        .build();

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&search_entry));
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some(&ui::t("刷新（F5）")));
    refresh.add_css_class("flat");
    header.pack_start(&refresh);
    let updates_button = gtk::Button::with_label(&ui::t("更新"));
    updates_button.add_css_class("flat");
    header.pack_end(&updates_button);
    let settings_button = gtk::Button::from_icon_name("emblem-system-symbolic");
    settings_button.set_tooltip_text(Some(&ui::t("设置（Ctrl+,）")));
    settings_button.add_css_class("flat");
    header.pack_end(&settings_button);
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&split));
    toolbar.add_bottom_bar(&bottom);

    let banner = adw::Banner::new("");
    banner.set_revealed(false);
    toolbar.add_top_bar(&banner);

    let toast = adw::ToastOverlay::new();
    toast.set_child(Some(&toolbar));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("ArchStore")
        .default_width(cfg.ui.window_width)
        .default_height(cfg.ui.window_height)
        .content(&toast)
        .build();

    // 窄窗口自动折叠（声明式，不手写尺寸判断）
    let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        600.0,
        adw::LengthUnit::Sp,
    ));
    let collapsed = true.to_value();
    breakpoint.add_setter(&split, "collapsed", Some(&collapsed));
    window.add_breakpoint(breakpoint);

    let main = Rc::new(MainWindow {
        window: window.clone(),
        toast: toast.clone(),
        split,
        stack,
        sidebar: sidebar.clone(),
        state,
        services: RefCell::new(None),
        rows,
        home,
        category,
        installed,
        updates,
        search,
        detail,
        settings,
        plan_bar,
        progress,
        search_entry: search_entry.clone(),
        updates_button: updates_button.clone(),
        banner,
        updates_count: count.clone(),
        current_nav: RefCell::new(Nav::Home),
        detail_nav: detail_nav.clone(),
        nav_view: nav_view.clone(),
    });

    // 页面回调从现在起可以拿到窗口
    *weak.borrow_mut() = Some(Rc::downgrade(&main));

    // ---------- 事件接线 ----------
    wire_navigation(&main);
    wire_header(&main, &refresh, &updates_button, &settings_button);
    wire_plan_bar(&main);
    wire_search(&main);
    wire_settings(&main);
    wire_category(&main);
    wire_installed_filters(&main);
    wire_updates_controls(&main);
    wire_shortcuts(&main);

    if let Some(note) = loaded.notice {
        main.banner.set_title(&note);
        main.banner.set_revealed(true);
    }

    // 崩溃恢复横幅
    if let Some(plan) =
        crate::state::load_recovered_plan(&archstore_core::config::paths::cache_dir())
    {
        let text = format!(
            "{}：{}（{} 项）。{}",
            ui::t("上次事务可能未完成"),
            plan.kind.label(),
            plan.len(),
            ui::t("请运行 checkupdates / pacman -Qkk 检查系统状态")
        );
        main.banner.set_title(&text);
        main.banner.set_revealed(true);
    }

    // 设置页初始提示
    main.settings.set_cache_size(archstore_core::env::dir_size(
        &archstore_core::config::paths::cache_dir(),
    ));

    main
}

/// 启动服务注册（异步），完成后填充各页面。
pub fn start_services(main: &Rc<MainWindow>) {
    let loaded = archstore_core::config::Config::load_default();
    let cfg = loaded.config.sanitized();
    let main_ref = main.clone();
    let read_only = loaded.read_only;
    let notice = loaded.notice.clone();
    runtime::spawn_ui(
        async move { Services::build(cfg, read_only, notice).await },
        move |result| match result {
            Ok(services) => {
                apply_services(&main_ref, services);
            }
            Err(e) => {
                main_ref.home.show_error(&e);
                main_ref.installed.page.shell.show_error(&e);
                ui::toast(&main_ref.toast, &e.user_message());
            }
        },
    );
}

/// 服务就绪后刷新界面。
fn apply_services(main: &Rc<MainWindow>, services: std::sync::Arc<Services>) {
    main.settings.set_backends(&services.capabilities);

    // 注入远程图标加载器：列表行与详情页的 Flathub 图标按需下载
    // （并发上限 4 由 HttpClient 内的闸门控制；URL -> 本地路径 的映射缓存 30 天）。
    // 未注入时 UI 不会发起任何图标网络请求。
    main.rows.set_icon_loader(std::rc::Rc::new({
        let services = std::sync::Arc::clone(&services);
        move |url: String, done: Box<dyn FnOnce(Option<std::path::PathBuf>)>| {
            let services = std::sync::Arc::clone(&services);
            runtime::spawn_ui(
                async move {
                    let cancel = CancelToken::new();
                    services.flathub.download_cached(&url, &cancel).await.ok()
                },
                done,
            );
        }
    }));
    if let Some(reason) = services.elevation_reason() {
        tracing::warn!(%reason, "提权通道不可用");
    }
    *main.services.borrow_mut() = Some(services);
    load_installed(main);
    load_updates(main);
    load_home(main);
    load_categories(main);
}

/// 读取已安装列表（本地、毫秒级）。
fn load_installed(main: &Rc<MainWindow>) {
    let Some(services) = main.services.borrow().clone() else {
        return;
    };
    main.installed.page.shell.show_loading();
    let main_ref = main.clone();
    let services_for_task = std::sync::Arc::clone(&services);
    runtime::spawn_ui(
        async move {
            match services_for_task.pacman.as_ref() {
                Some(p) => p.installed().await,
                None => Err(archstore_core::CoreError::BackendUnavailable {
                    kind: "pacman".into(),
                    reason: "官方仓库后端未启用".into(),
                }),
            }
        },
        move |result| match result {
            Ok(items) => {
                services.installed.replace(&items);
                main_ref.installed.set_items(&items);
            }
            Err(e) => main_ref.installed.page.shell.show_error(&e),
        },
    );
}

/// 读取可更新列表（含安全公告与 AUR 更新，仅在更新页可见时拉取）。
fn load_updates(main: &Rc<MainWindow>) {
    let Some(services) = main.services.borrow().clone() else {
        return;
    };
    main.updates.page.shell.show_loading();
    let main_ref = main.clone();
    runtime::spawn_ui(
        async move {
            let mut items: Vec<PackageSummary> = Vec::new();
            if let Some(p) = services.pacman.as_ref() {
                items.extend(p.upgradable().await.unwrap_or_default());
            }
            if let Some(a) = services.aur.as_ref()
                && services.config.sources.aur_enabled
                && let Ok(mut aur) = a.upgradable().await
            {
                items.append(&mut aur);
            }
            if let Some(f) = services.flatpak.as_ref()
                && services.config.sources.flatpak_enabled
                && let Ok(mut fp) = f.upgradable().await
            {
                items.append(&mut fp);
            }
            let advisories = {
                let cancel = CancelToken::new();
                services
                    .flathub
                    .advisories(&cancel)
                    .await
                    .unwrap_or_default()
            };
            Ok::<_, archstore_core::CoreError>((items, advisories))
        },
        move |result| match result {
            Ok((items, advisories)) => {
                let count = items.len();
                main_ref.updates.set_items(&items, advisories);
                main_ref
                    .updates_button
                    .set_label(&format!("{} ({count})", ui::t("更新")));
                // 侧栏计数与按钮保持一致；0 时隐藏，避免视觉噪音
                main_ref.updates_count.set_label(&count.to_string());
                main_ref.updates_count.set_visible(count > 0);
            }
            Err(e) => main_ref.updates.page.shell.show_error(&e),
        },
    );
}

/// 首页推荐（Flathub 趋势）。
fn load_home(main: &Rc<MainWindow>) {
    let Some(services) = main.services.borrow().clone() else {
        return;
    };
    main.home.show_loading();
    let main_ref = main.clone();
    runtime::spawn_ui(
        async move {
            let cancel = CancelToken::new();
            let page = services
                .flathub
                .collection(
                    archstore_core::flathub::CollectionKind::Trending,
                    1,
                    40,
                    &cancel,
                )
                .await?;
            // 必须复用后端的"集合命中 -> 摘要"映射：图标 URL 只在那一处被搬进摘要，
            // 首页自己构造摘要会让"推荐"整页退化成字母头像（用户实测反馈）。
            let items: Vec<PackageSummary> = match services.flatpak.as_ref() {
                Some(f) => f.hits_to_summaries(&page.hits).await,
                None => page
                    .hits
                    .iter()
                    .map(|h| {
                        archstore_core::backend::flatpak::summary_from_hit(
                            &services.config.sources.flatpak_remote,
                            h,
                        )
                    })
                    .collect(),
            };
            Ok::<_, archstore_core::CoreError>(items)
        },
        move |result| match result {
            Ok(items) => main_ref.home.set_items(&items, &ui::t("来自 Flathub 趋势")),
            Err(e) => {
                main_ref.home.show_error(&e);
            }
        },
    );
}

/// 分类列表（pacman 包组 + AUR 关键词 + Flatpak 分类）。
fn load_categories(main: &Rc<MainWindow>) {
    let Some(services) = main.services.borrow().clone() else {
        return;
    };
    let main_ref = main.clone();
    runtime::spawn_ui(
        async move {
            let mut cats = Vec::new();
            if let Some(p) = services.pacman.as_ref() {
                cats.extend(p.categories().await.unwrap_or_default());
            }
            if let Some(a) = services.aur.as_ref() {
                cats.extend(a.categories().await.unwrap_or_default());
            }
            if let Some(f) = services.flatpak.as_ref() {
                cats.extend(f.categories().await.unwrap_or_default());
            }
            cats
        },
        move |cats| main_ref.category.set_categories(&cats),
    );
}

// ============================ 惰性窗口引用 ============================
//
// 页面在构造期需要回调，回调又需要 Rc<MainWindow>。用 Weak 打破这一循环：
// 回调触发时窗口必然已构造完成，upgrade() 一定成功。

/// MainWindow 的弱引用槽（普通 Rust 结构体，用 std 的 Weak 即可）。
pub type WeakSlot = Rc<RefCell<Option<Weak<MainWindow>>>>;

/// 取得窗口（窗口已销毁时返回 None）。
fn upgrade(slot: &WeakSlot) -> Option<Rc<MainWindow>> {
    let guard = slot.borrow();
    guard.as_ref()?.upgrade()
}

/// "回到设置页"的回调。
fn nav_to_settings(slot: &WeakSlot) -> Box<dyn Fn() + 'static> {
    let w = slot.clone();
    Box::new(move || {
        if let Some(m) = upgrade(&w) {
            m.nav_view.pop_to_tag("root");
            m.stack.set_visible_child_name("settings");
            if let Some(row) = m.sidebar.row_at_index(6) {
                m.sidebar.select_row(Some(&row));
            }
        }
    })
}

/// 带回退动作的重试回调（错态里的"重试"按钮）。
fn retry_lazy(slot: &WeakSlot, action: fn(&Rc<MainWindow>)) -> Box<dyn Fn() + 'static> {
    let w = slot.clone();
    Box::new(move || {
        if let Some(m) = upgrade(&w) {
            action(&m);
        }
    })
}

/// 搜索页的重试：重跑当前关键字的搜索。
fn search_retry(slot: &WeakSlot) -> Box<dyn Fn() + 'static> {
    let w = slot.clone();
    Box::new(move || {
        if let Some(m) = upgrade(&w) {
            run_search(&m);
        }
    })
}

fn reload_installed(main: &Rc<MainWindow>) {
    load_installed(main);
}

fn reload_updates(main: &Rc<MainWindow>) {
    load_updates(main);
}

fn reload_categories(main: &Rc<MainWindow>) {
    load_categories(main);
}

// ============================ 详情页 ============================

/// 打开详情页（惰性版本）。
fn open_detail_lazy(slot: &WeakSlot, summary: PackageSummary) {
    if let Some(main) = upgrade(slot) {
        open_detail(&main, summary);
    }
}

/// 打开详情页：先把本地摘要显示出来，再异步补全网络字段（§7.4）。
pub fn open_detail(main: &Rc<MainWindow>, summary: PackageSummary) {
    main.detail.show_summary(&summary);
    main.nav_view.push(&main.detail_nav);

    let Some(services) = main.services.borrow().clone() else {
        // 服务尚未就绪：只显示摘要，不报错
        return;
    };
    let id = summary.id.clone();
    let main_ref = main.clone();
    runtime::spawn_ui(
        async move {
            let backend = match id.kind() {
                "pacman" => services
                    .pacman
                    .clone()
                    .map(|b| b as std::sync::Arc<dyn PackageBackend>),
                "aur" => services
                    .aur
                    .clone()
                    .map(|b| b as std::sync::Arc<dyn PackageBackend>),
                "flatpak" => services
                    .flatpak
                    .clone()
                    .map(|b| b as std::sync::Arc<dyn PackageBackend>),
                _ => None,
            };
            match backend {
                Some(b) => b.info(&id).await,
                None => Err(archstore_core::CoreError::BackendUnavailable {
                    kind: id.kind().to_string(),
                    reason: "该后端未启用".into(),
                }),
            }
        },
        move |result| {
            // 用户可能已经点了别的软件；只更新仍然显示这个软件的情况
            let current = main_ref.detail.title_widget().title().to_string();
            let _ = current;
            match result {
                Ok(detail) => render_detail(&main_ref, detail),
                Err(e) => main_ref.detail.set_error(&summary, &e),
            }
        },
    );
}

/// 渲染详情并接线按钮。
fn render_detail(main: &Rc<MainWindow>, detail: archstore_core::model::PackageDetail) {
    let id = detail.summary.id.clone();
    let installed = detail.summary.is_installed();
    let has_update = detail.summary.has_update();
    let homepage = detail.homepage.clone();

    let main_primary = main.clone();
    let id_primary = id.clone();
    let primary = move || plan_install(&main_primary, id_primary.clone());

    let remove = if installed {
        let main_remove = main.clone();
        let id_remove = id.clone();
        Some(Box::new(move || plan_remove(&main_remove, id_remove.clone(), false)) as Box<dyn Fn()>)
    } else {
        None
    };

    let on_homepage = homepage.map(|_url| {
        let main_home = main.clone();
        Box::new(move |u: String| {
            ui::open_uri(&main_home.window, &u);
        }) as Box<dyn Fn(String)>
    });

    let main_deps = main.clone();
    let id_deps = id.clone();
    let on_deps_changed = Some(
        Box::new(move |sel: crate::widgets::dep_list::DepSelection| {
            // 依赖选择变化：把勾选结果记到状态里，下一次"安装"时带上
            *main_deps.state.dep_selection.borrow_mut() = Some((id_deps.clone(), sel));
        }) as Box<dyn Fn(crate::widgets::dep_list::DepSelection)>,
    );

    let _ = has_update;
    main.detail.set_detail(
        &detail,
        &main.rows,
        crate::widgets::package_detail_view::DetailCallbacks {
            on_primary: Box::new(primary),
            on_remove: remove,
            on_homepage,
            on_deps_changed,
        },
    );

    // 在线翻译：默认关闭；开启时只翻译当前详情页的描述（§7.2 规则 4），
    // 失败静默回退原文、不弹错误（§7.2 规则 3）。
    spawn_translation(main, &detail);

    // 截图：异步下载到缓存后回填槽位（GtkPicture 没有 URL 加载 API）
    let Some(view) = main.detail.view() else {
        return;
    };
    let urls = view.screenshot_urls().to_vec();
    if urls.is_empty() {
        return;
    }
    let Some(services) = main.services.borrow().clone() else {
        return;
    };
    runtime::spawn_ui(
        async move {
            let cancel = CancelToken::new();
            services.flathub.download_all(&urls, &cancel).await
        },
        move |results| {
            for (i, path) in results.iter().enumerate() {
                if let Some(path) = path {
                    view.set_screenshot(i, path);
                }
            }
        },
    );
}

// ============================ 事务计划 ============================

/// 刷新计划栏（状态机 + 计划 + AUR 请求一起给它）。
fn refresh_plan_bar(main: &Rc<MainWindow>) {
    let state = main.state.tx.borrow().clone();
    let plans = main.state.plans();
    let aur = main.state.aur_requests();
    main.plan_bar.update(&state, &plans, &aur);
}

/// 把构建结果入队并刷新界面。
fn enqueue_outcome(main: &Rc<MainWindow>, outcome: archstore_core::plan::BuildOutcome) {
    let risks = archstore_core::plan::summarize_risks(&outcome);
    if let Err(e) = main.state.enqueue(outcome) {
        ui::toast(&main.toast, &e.user_message());
        return;
    }
    let warnings: Vec<String> = risks.iter().map(|r| r.message()).collect();
    if !warnings.is_empty() {
        main.plan_bar.show_banner(&warnings.join(" "));
    } else {
        main.plan_bar.hide_banner();
    }
    refresh_plan_bar(main);
    ui::toast(
        &main.toast,
        &format!("{}，{}", ui::t("已加入计划"), ui::t("需要确认后才会执行")),
    );
}

/// 安装 / 更新一个软件（加入计划，不直接执行）。
pub fn plan_install(main: &Rc<MainWindow>, id: PackageId) {
    let Some(services) = main.services.borrow().clone() else {
        return;
    };
    if !services.can_elevate() && id.kind() != "aur" {
        let reason = services
            .elevation_reason()
            .unwrap_or_else(|| ui::t("提权不可用"));
        ui::toast(&main.toast, &reason);
        return;
    }
    let backends = services.backends();
    let options = main
        .state
        .dep_selection
        .borrow()
        .as_ref()
        .filter(|(pid, _)| pid == &id)
        .map(|(_, sel)| InstallOptions {
            optional: sel.optional.clone(),
            include_dependencies: true,
            virtual_choices: sel.virtual_choices.clone(),
        })
        .unwrap_or_default();
    let main_ref = main.clone();
    ui::toast(&main_ref.toast, &ui::t("正在构建计划…"));
    runtime::spawn_ui(
        async move { archstore_core::plan::build_install_plan(&backends, &[id], &options).await },
        move |result| match result {
            Ok(outcome) => enqueue_outcome(&main_ref, outcome),
            Err(e) => ui::toast(&main_ref.toast, &e.user_message()),
        },
    );
}

/// 卸载一个软件（先做反依赖检查，非级联时询问用户）。
pub fn plan_remove(main: &Rc<MainWindow>, id: PackageId, cascade: bool) {
    let Some(services) = main.services.borrow().clone() else {
        return;
    };
    let backends = services.backends();
    let main_ref = main.clone();
    runtime::spawn_ui(
        async move { archstore_core::plan::build_remove_plan(&backends, &[id], cascade).await },
        move |result| match result {
            Ok(outcome) => enqueue_outcome(&main_ref, outcome),
            Err(archstore_core::CoreError::ReverseDeps {
                target, dependents, ..
            }) => {
                // 默认把"同时删除这些包"设为否（§5.4 规则 6）
                let m = main_ref.clone();
                let target_id = PackageId::official(String::new(), target.clone());
                plan_bar::ask_cascade(&main_ref.window, &target, &dependents, move |cascade| {
                    if cascade {
                        plan_remove(&m, target_id.clone(), true);
                    }
                });
            }
            Err(e) => ui::toast(&main_ref.toast, &e.user_message()),
        },
    );
}

/// 一键更新：全部可更新项加入计划（官方仓库整批 + AUR 单独 + Flatpak 单独）。
pub fn plan_update_all(main: &Rc<MainWindow>) {
    let Some(services) = main.services.borrow().clone() else {
        return;
    };
    let backends = services.backends();
    let main_ref = main.clone();
    runtime::spawn_ui(
        async move { archstore_core::plan::build_update_plan(&backends, true).await },
        move |result| match result {
            Ok(outcome) => enqueue_outcome(&main_ref, outcome),
            Err(e) => ui::toast(&main_ref.toast, &e.user_message()),
        },
    );
}

// ============================ 设置页回调 ============================

/// 配置变化：写盘 + 立即生效（主题 / 代理与超时 / 缓存上限）。
fn settings_changed(slot: &WeakSlot) -> Box<dyn Fn(Config) + 'static> {
    let w = slot.clone();
    Box::new(move |cfg: Config| {
        let Some(main) = upgrade(&w) else {
            return;
        };
        // 1) 外观立即生效
        crate::app::apply_color_scheme(cfg.appearance.color_scheme);
        // 2) 写盘（原子写 + 未知字段保留）
        if let Some(Err(e)) = main.services.borrow().clone().map(|s| s.save_config(&cfg)) {
            ui::toast(&main.toast, &e.user_message());
        }
        // 3) 翻译端点变化 -> 更新网络白名单
        archstore_core::net::set_extra_hosts(if cfg.translation.api_endpoint.is_empty() {
            Vec::new()
        } else {
            archstore_core::net::host_of(&cfg.translation.api_endpoint)
                .into_iter()
                .collect()
        });

        // 4) 网络配置变化 -> 立即重建 reqwest::Client（旧请求保留但不再复用）
        if let Some(services) = main.services.borrow().clone() {
            let network = cfg.network.clone();
            let cache = services.cache.clone();
            let max_bytes = cfg.cache.max_size_mb.saturating_mul(1024 * 1024);
            let main_ref = main.clone();
            runtime::spawn_ui(
                async move {
                    let r = services.http.reconfigure(network).await;
                    cache.set_max_bytes(max_bytes);
                    let freed = cache.prune().await;
                    r.map(|_| freed)
                },
                move |result| match result {
                    Ok(freed) if freed > 0 => ui::toast(
                        &main_ref.toast,
                        &format!(
                            "{}：{}",
                            ui::t("缓存已按新上限裁剪"),
                            archstore_core::model::human_size(freed)
                        ),
                    ),
                    Ok(_) => {}
                    Err(e) => ui::toast(&main_ref.toast, &e.user_message()),
                },
            );
        }
        main.settings.set_cache_size(archstore_core::env::dir_size(
            &archstore_core::config::paths::cache_dir(),
        ));
    })
}

/// "测试连接"：请求 AUR RPC 的空查询（§10）。
fn test_connection(slot: &WeakSlot) -> Box<dyn Fn() + 'static> {
    let w = slot.clone();
    Box::new(move || {
        let Some(main) = upgrade(&w) else {
            return;
        };
        let Some(services) = main.services.borrow().clone() else {
            return;
        };
        let main_ref = main.clone();
        runtime::spawn_ui(
            async move {
                let cancel = CancelToken::new();
                let client = services.http.client().await;
                archstore_core::net::get_json::<serde_json::Value>(
                    &client,
                    "https://aur.archlinux.org/rpc?v=5&type=search&arg=",
                    &cancel,
                    archstore_core::net::RetryPolicy::NONE,
                )
                .await
                .map(|_| ())
            },
            move |result| match result {
                Ok(()) => ui::toast(&main_ref.toast, &ui::t("连接成功")),
                Err(e) => ui::toast(&main_ref.toast, &e.user_message()),
            },
        );
    })
}

/// "清除缓存"：目录实际统计 + 确认后清空。
fn clear_cache(slot: &WeakSlot) -> Box<dyn Fn() + 'static> {
    let w = slot.clone();
    Box::new(move || {
        let Some(main) = upgrade(&w) else {
            return;
        };
        let Some(services) = main.services.borrow().clone() else {
            return;
        };
        let size = archstore_core::env::dir_size(&archstore_core::config::paths::cache_dir());
        let main_ref = main.clone();
        let dialog = adw::AlertDialog::builder()
            .heading(ui::t("清除缓存"))
            .body(format!(
                "{}\n\n{}",
                ui::t("将删除全部缓存数据（元数据与图标）。"),
                archstore_core::model::human_size(size)
            ))
            .build();
        dialog.add_response("cancel", &ui::t("取消"));
        dialog.add_response("clear", &ui::t("清除"));
        dialog.set_response_appearance("clear", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.connect_response(None, move |_, response| {
            if response != "clear" {
                return;
            }
            let services = services.clone();
            let main_inner = main_ref.clone();
            runtime::spawn_ui(
                async move {
                    services.cache.clear().await?;
                    Ok::<_, archstore_core::CoreError>(archstore_core::env::dir_size(
                        &archstore_core::config::paths::cache_dir(),
                    ))
                },
                move |result| match result {
                    Ok(size) => {
                        main_inner.settings.set_cache_size(size);
                        ui::toast(&main_inner.toast, &ui::t("缓存已清空"));
                    }
                    Err(e) => ui::toast(&main_inner.toast, &e.user_message()),
                },
            );
        });
        dialog.present(Some(&main.window));
    })
}

/// "运行环境自检"：复用 --doctor 的只读检测，在窗口里展示结果（§10 诊断组）。
fn show_doctor(slot: &WeakSlot) -> Box<dyn Fn() + 'static> {
    let w = slot.clone();
    Box::new(move || {
        let Some(main) = upgrade(&w) else {
            return;
        };
        let (gtk_v, adw_v) = (
            Some((
                gtk::major_version(),
                gtk::minor_version(),
                gtk::micro_version(),
            )),
            Some((
                adw::major_version(),
                adw::minor_version(),
                adw::micro_version(),
            )),
        );
        let cfg = main
            .services
            .borrow()
            .clone()
            .map(|s| s.config.clone())
            .unwrap_or_default();
        let main_ref = main.clone();
        // 自检会打开只读 libalpm 句柄，必须离开主线程
        runtime::spawn_ui(
            async move {
                tokio::task::spawn_blocking(move || archstore_core::env::probe(gtk_v, adw_v, &cfg))
                    .await
                    .unwrap_or_else(|_| archstore_core::env::DoctorReport::new())
            },
            move |report| {
                let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
                let scroll = gtk::ScrolledWindow::builder()
                    .child(&body)
                    .min_content_height(320)
                    .min_content_width(560)
                    .build();
                for check in &report.checks {
                    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                    let marker = gtk::Label::new(Some(check.level.marker().trim()));
                    marker.add_css_class(match check.level {
                        archstore_core::env::Level::Ok => "state-pill",
                        archstore_core::env::Level::Warn => "plan-warning",
                        archstore_core::env::Level::Fail => "dep-missing",
                    });
                    row.append(&marker);
                    let text = ui::label(&format!("{}：{}", check.title, check.detail));
                    text.set_wrap(true);
                    row.append(&text);
                    body.append(&row);
                }
                let dialog = adw::AlertDialog::builder()
                    .heading(format!(
                        "{}（{} 项失败 / {} 项警告）",
                        ui::t("运行环境自检"),
                        report.failure_count(),
                        report.warn_count()
                    ))
                    .extra_child(&scroll)
                    .build();
                dialog.add_response("close", &ui::t("关闭"));
                dialog.present(Some(&main_ref.window));
            },
        );
    })
}

/// "重新检测后端"：重新读取本机状态并刷新界面。
fn redetect(slot: &WeakSlot) -> Box<dyn Fn() + 'static> {
    let w = slot.clone();
    Box::new(move || {
        if let Some(main) = upgrade(&w) {
            load_installed(&main);
            load_updates(&main);
            load_categories(&main);
            ui::toast(&main.toast, &ui::t("已重新检测"));
        }
    })
}

// ============================ 筛选控件 ============================

/// 已安装页：筛选下拉框 + 本地搜索框（都只做本地过滤，不联网）。
fn wire_installed_filters(main: &Rc<MainWindow>) {
    let main_ref = main.clone();
    main.installed
        .filter_dropdown()
        .clone()
        .connect_selected_notify(move |d| {
            main_ref.installed.set_filter_index(d.selected());
        });
    let main_ref = main.clone();
    main.installed
        .search_entry()
        .clone()
        .connect_search_changed(move |_| main_ref.installed.refresh());
}

/// 更新页：分组下拉框 + 一键更新。
fn wire_updates_controls(main: &Rc<MainWindow>) {
    let main_ref = main.clone();
    main.updates
        .group_dropdown()
        .clone()
        .connect_selected_notify(move |d| {
            main_ref.updates.set_group_index(d.selected());
        });
    let main_ref = main.clone();
    main.updates
        .update_all_button()
        .clone()
        .connect_clicked(move |_| plan_update_all(&main_ref));
}

fn wire_navigation(main: &Rc<MainWindow>) {
    let main_ref = main.clone();
    main.sidebar.connect_row_selected(move |_, row| {
        let Some(row) = row else {
            return;
        };
        let name = row.widget_name().to_string();
        let nav = match name.as_str() {
            "home" => Nav::Home,
            "category" => Nav::Category,
            "aur" => Nav::Aur,
            "flatpak" => Nav::Flatpak,
            "installed" => Nav::Installed,
            "updates" => Nav::Updates,
            "settings" => Nav::Settings,
            _ => Nav::Home,
        };
        // 点击侧栏任何一项都必须回到根页面：
        // 否则用户从详情页点侧栏会"点了没反应"（详情页还压在导航栈上）。
        main_ref.nav_view.pop_to_tag("root");

        // "AUR 社区"/"Flatpak" 复用分类页，只切换来源过滤
        let (page_name, filter) = match nav {
            Nav::Aur => ("category", Some("aur")),
            Nav::Flatpak => ("category", Some("flatpak")),
            Nav::Category => ("category", None),
            other => (other.page_name(), None),
        };
        if matches!(nav, Nav::Category | Nav::Aur | Nav::Flatpak) {
            main_ref.category.set_source_filter(filter);
        }
        main_ref.stack.set_visible_child_name(page_name);
        *main_ref.current_nav.borrow_mut() = nav;
        if nav == Nav::Updates {
            // 安全公告只在更新页可见时拉取一次（缓存 6 小时）
            load_updates(&main_ref);
        }
    });
    // 默认选中首页
    if let Some(row) = main.sidebar.row_at_index(0) {
        main.sidebar.select_row(Some(&row));
    }
}

fn wire_header(
    main: &Rc<MainWindow>,
    refresh: &gtk::Button,
    updates_button: &gtk::Button,
    settings_button: &gtk::Button,
) {
    {
        let main = main.clone();
        refresh.connect_clicked(move |_| {
            load_installed(&main);
            load_updates(&main);
            load_categories(&main);
            ui::toast(&main.toast, &ui::t("已刷新"));
        });
    }
    {
        let main = main.clone();
        updates_button.connect_clicked(move |_| {
            main.stack.set_visible_child_name("updates");
            if let Some(row) = main.sidebar.row_at_index(5) {
                main.sidebar.select_row(Some(&row));
            }
            load_updates(&main);
        });
    }
    {
        let main = main.clone();
        settings_button.connect_clicked(move |_| {
            main.stack.set_visible_child_name("settings");
            if let Some(row) = main.sidebar.row_at_index(6) {
                main.sidebar.select_row(Some(&row));
            }
        });
    }
}

fn wire_plan_bar(main: &Rc<MainWindow>) {
    {
        let main = main.clone();
        main.plan_bar
            .details_button()
            .clone()
            .connect_clicked(move |_| {
                if let Some(plan) = main.state.tx.borrow().plan() {
                    plan_bar::show_plan_details(&main.window, std::slice::from_ref(&*plan), &[]);
                }
            });
    }
    {
        let main = main.clone();
        main.plan_bar
            .discard_button()
            .clone()
            .connect_clicked(move |_| {
                main.state.reset();
                refresh_plan_bar(&main);
                main.plan_bar.hide_banner();
                main.progress.reset();
            });
    }
    {
        let main = main.clone();
        main.plan_bar
            .execute_button()
            .clone()
            .connect_clicked(move |_| {
                execute_plan(&main);
            });
    }
}

/// 执行当前计划。
///
/// 两条路径：
/// 1. 需要提权的计划 -> 写计划文件 -> pkexec helper -> 流式事件；
/// 2. AUR 请求 -> 以用户身份在终端里执行（§5.3：AUR 构建必须在非 root 下进行）。
///    提权计划全部成功后才进行第 2 步，避免"部分升级"式的中途状态。
fn execute_plan(main: &Rc<MainWindow>) {
    let Some(services) = main.services.borrow().clone() else {
        return;
    };
    let aur_requests = main.state.aur_requests();

    // 只有 AUR 请求：不经过 helper，直接交给终端
    if main.state.aur_only() || main.state.tx.borrow().plan().is_none() {
        if aur_requests.is_empty() {
            return;
        }
        run_aur_requests(main, &services, &aur_requests);
        return;
    }

    if !services.can_elevate() {
        let reason = services
            .elevation_reason()
            .unwrap_or_else(|| ui::t("提权不可用"));
        ui::toast(&main.toast, &reason);
        return;
    }
    let current = main.state.tx.borrow().clone();
    let confirmed = match current.apply(TxEvent::Confirm) {
        Ok(next) => next,
        Err(e) => {
            ui::toast(&main.toast, &e.user_message());
            return;
        }
    };
    let Some(plan) = confirmed.plan() else {
        return;
    };
    let cache_dir = archstore_core::config::paths::cache_dir();
    let Some(path) = plan_bar::prepare_plan_file(&cache_dir, &plan) else {
        ui::toast(&main.toast, &ui::t("无法写入计划文件"));
        return;
    };

    let authorized = match confirmed.apply(TxEvent::Authorize) {
        Ok(next) => next,
        Err(e) => {
            ui::toast(&main.toast, &e.user_message());
            return;
        }
    };
    let running = match authorized.apply(TxEvent::Start) {
        Ok(next) => next,
        Err(e) => {
            ui::toast(&main.toast, &e.user_message());
            return;
        }
    };
    *main.state.tx.borrow_mut() = running;
    refresh_plan_bar(main);
    main.progress.reset();

    // 新事务开始：清除上一次的"已完成"标记（§5.4 规则 4）
    crate::state::clear_plan_done(&archstore_core::config::paths::cache_dir());

    let main_ref = main.clone();
    plan_bar::spawn_helper(path.clone(), plan.kind, move |event| {
        let finished_ok = matches!(
            &event,
            crate::state::HelperEvent::Done { status, .. } if status == "ok"
        );
        if finished_ok {
            // 事务成功结束：写入标记，下次启动不再提示"上次事务可能未完成"
            if let Err(e) =
                crate::state::mark_plan_done(&archstore_core::config::paths::cache_dir())
            {
                tracing::warn!(error = %e, "无法写入事务完成标记");
            }
        }
        handle_helper_event(&main_ref, event, &path, plan.kind);
        // 提权部分成功后再把 AUR 请求交给终端
        if finished_ok
            && !main_ref.state.aur_requests().is_empty()
            && let Some(services) = main_ref.services.borrow().clone()
        {
            let requests = main_ref.state.aur_requests();
            run_aur_requests(&main_ref, &services, &requests);
        }
    });
}

/// 翻译当前详情页的描述（若已启用）。
///
/// 只在详情页可见时发起；一次只翻译一个条目的描述，绝不批量翻译列表页。
fn spawn_translation(main: &Rc<MainWindow>, detail: &archstore_core::model::PackageDetail) {
    let Some(services) = main.services.borrow().clone() else {
        return;
    };
    if !services.config.translation.auto_translate
        || services.config.translation.api == archstore_core::config::TranslationApi::None
    {
        return;
    }
    if detail.description.trim().is_empty() {
        return;
    }
    let Some(area) = main
        .detail
        .view()
        .and_then(|v| v.description_area().cloned())
    else {
        return;
    };
    // 目标语言：配置优先，留空则跟随本程序的界面语言（简体中文）
    let target = if services.config.translation.target_lang.is_empty() {
        "zh-CN".to_string()
    } else {
        services.config.translation.target_lang.clone()
    };
    let cfg = services.config.translation.clone();
    let text = detail.description.clone();
    runtime::spawn_ui(
        async move {
            let cancel = CancelToken::new();
            services
                .translate
                .translate(&text, &target, &cfg, &cancel)
                .await
        },
        move |result| match result {
            Ok(t) => {
                tracing::info!(provider = %t.provider, cached = t.cached, "描述翻译完成");
                area.set_translation(&t.text, &t.label(), t.truncated);
            }
            Err(e) => {
                // 静默回退：保留原文，不弹错误（§7.2 规则 3）
                tracing::debug!(error = %e, "翻译失败，保留原文");
            }
        },
    );
}

/// 把 AUR 请求交给终端以用户身份执行（§5.3）。
///
/// 找不到 paru/yay 时给出可操作的提示；无法打开终端时把完整命令复制到剪贴板。
fn run_aur_requests(
    main: &Rc<MainWindow>,
    services: &Services,
    requests: &[archstore_core::plan::AurRequest],
) {
    let Some(helper) = archstore_core::env::find_aur_helper(services.config.sources.aur_helper)
    else {
        ui::toast(
            &main.toast,
            &ui::t("需要 paru 或 yay 才能执行 AUR 操作；查询功能不受影响。"),
        );
        return;
    };
    for request in requests {
        plan_bar::run_aur_in_terminal(&main.toast, helper, request.action, &request.packages);
    }
}

/// 处理 helper 的逐行 JSON 事件。
fn handle_helper_event(
    main: &Rc<MainWindow>,
    event: crate::state::HelperEvent,
    _path: &std::path::Path,
    kind: PlanKind,
) {
    use crate::state::HelperEvent as E;
    let current = main.state.tx.borrow().clone();
    match event {
        E::Progress { .. } | E::Log { .. } => {
            if let Some(progress) = current_progress(&current) {
                let mut next = progress.clone();
                match &event {
                    E::Progress {
                        phase,
                        percent,
                        detail,
                    } => {
                        next.phase = phase.clone();
                        next.percent = *percent;
                        next.detail = detail.clone();
                    }
                    E::Log { line, .. } => next.push_log(line.clone()),
                    _ => {}
                }
                main.progress.update(&next);
                if let Ok(state) = current.clone().apply(TxEvent::Progress(next)) {
                    *main.state.tx.borrow_mut() = state;
                }
            }
        }
        E::Start { .. } => {}
        E::Error { code, message } => {
            let error = format!("{code}：{message}");
            if let Ok(state) = current.apply(TxEvent::Fail {
                error: error.clone(),
                log_tail: String::new(),
            }) {
                *main.state.tx.borrow_mut() = state;
            }
            main.progress.fail(&error, "");
            ui::toast(&main.toast, &error);
        }
        E::NeedsTty { hint, .. } => {
            main.progress.fail(&hint, "");
            ui::toast(&main.toast, &hint);
        }
        E::Done {
            status,
            installed,
            removed,
            failed,
            elapsed_ms,
        } => {
            if status == "ok" || status == "dry-run" {
                let summary = crate::state::TxSummary {
                    installed,
                    removed,
                    failed,
                    elapsed_ms,
                    status,
                };
                if let Ok(state) = current.apply(TxEvent::Succeed(summary.clone())) {
                    *main.state.tx.borrow_mut() = state;
                }
                main.progress.finish(&summary);
                refresh_after_transaction(main);
            } else {
                let error = format!("{}：{}", ui::t("事务失败"), status);
                if let Ok(state) = current.apply(TxEvent::Fail {
                    error: error.clone(),
                    log_tail: String::new(),
                }) {
                    *main.state.tx.borrow_mut() = state;
                }
                main.progress.fail(&error, "");
            }
            let _ = kind;
            refresh_plan_bar(main);
        }
    }
}

fn current_progress(state: &TxState) -> Option<&crate::state::Progress> {
    match state {
        TxState::Running { progress, .. } => Some(progress),
        _ => None,
    }
}

/// 事务完成后必须重新打开 alpm 句柄（libalpm 会缓存本地库）。
fn refresh_after_transaction(main: &Rc<MainWindow>) {
    let Some(services) = main.services.borrow().clone() else {
        return;
    };
    let main_ref = main.clone();
    runtime::spawn_ui(
        async move {
            if let Some(p) = services.pacman.as_ref() {
                let _ = p.refresh().await;
            }
            Ok::<_, archstore_core::CoreError>(())
        },
        move |_| {
            load_installed(&main_ref);
            load_updates(&main_ref);
        },
    );
}

/// 搜索：按来源开关并行发起，各后端的错误只影响自己。
fn wire_search(main: &Rc<MainWindow>) {
    let main_ref = main.clone();
    main.search.search_entry().connect_activate(move |_| {
        run_search(&main_ref);
    });
    let main_ref2 = main.clone();
    main.search.search_entry().connect_search_changed(move |_| {
        let text = main_ref2.search.search_entry().text().to_string();
        if text.trim().is_empty() {
            return;
        }
        // 立即显示加载态；真正的请求由 activate 或防抖后触发
        main_ref2.search.show_loading();
    });
    // 来源开关变化后自动重跑一次
    let main_ref3 = main.clone();
    main.search.pacman_toggle.connect_toggled(move |_| {
        if !main_ref3.search.last_query().trim().is_empty() {
            run_search(&main_ref3);
        }
    });
}

/// 执行一次搜索（合并三个后端的结果）。
pub fn run_search(main: &Rc<MainWindow>) {
    let Some(services) = main.services.borrow().clone() else {
        return;
    };
    let query = main.search.search_entry().text().to_string();
    if query.trim().is_empty() {
        return;
    }
    // 搜索时切到搜索页，否则结果写进了一个用户看不见的页面
    main.nav_view.pop_to_tag("root");
    main.stack.set_visible_child_name("search");
    *main.current_nav.borrow_mut() = Nav::Home;

    let generation = main.search.next_generation();
    main.search.show_loading();
    let filter = main.search.filter();
    let main_ref = main.clone();
    let query_for_task = query.clone();
    runtime::spawn_ui(
        async move {
            let query = query_for_task;
            let mut batches: Vec<Vec<PackageSummary>> = Vec::new();
            let mut errors: Vec<(String, String)> = Vec::new();
            if filter.pacman
                && let Some(p) = services.pacman.as_ref()
            {
                match p.search(&query, SearchScope::Full).await {
                    Ok(v) => batches.push(v),
                    Err(e) => errors.push(("pacman".into(), e.user_message())),
                }
            }
            if filter.aur
                && let Some(a) = services.aur.as_ref()
            {
                match a.search(&query, SearchScope::Full).await {
                    Ok(v) => batches.push(v),
                    Err(e) => errors.push(("AUR".into(), e.user_message())),
                }
            }
            if filter.flatpak
                && let Some(f) = services.flatpak.as_ref()
            {
                match f.search(&query, SearchScope::Full).await {
                    Ok(v) => batches.push(v),
                    Err(e) => errors.push(("Flatpak".into(), e.user_message())),
                }
            }
            let merged = archstore_core::backend::merge_results(&query, batches);
            (merged, errors)
        },
        move |(items, errors)| {
            // 丢弃过期响应
            if main_ref.search.generation() != generation {
                return;
            }
            main_ref.search.set_results(&items, &errors, &query);
        },
    );
}

/// 分类选择：加载某个分类的第一页。
fn wire_category(main: &Rc<MainWindow>) {
    let main_ref = main.clone();
    main.category.connect_select(move |cat, page| {
        let Some(services) = main_ref.services.borrow().clone() else {
            return;
        };
        main_ref.category.show_loading();
        let main_inner = main_ref.clone();
        runtime::spawn_ui(
            async move {
                let backend = match cat.source_kind {
                    "aur" => services
                        .aur
                        .clone()
                        .map(|b| b as std::sync::Arc<dyn PackageBackend>),
                    "flatpak" => services
                        .flatpak
                        .clone()
                        .map(|b| b as std::sync::Arc<dyn PackageBackend>),
                    _ => services
                        .pacman
                        .clone()
                        .map(|b| b as std::sync::Arc<dyn PackageBackend>),
                };
                let Some(backend) = backend else {
                    return Err(archstore_core::CoreError::BackendUnavailable {
                        kind: cat.source_kind.to_string(),
                        reason: "该后端未启用".into(),
                    });
                };
                backend.list_category(&cat.id, page).await
            },
            move |result| match result {
                Ok(items) => {
                    let has_more = items.len() >= crate::pages::category::PAGE_SIZE;
                    main_inner.category.set_items(&items, has_more);
                }
                Err(e) => main_inner.category.show_error(&e),
            },
        );
    });

    let main_ref = main.clone();
    main.category
        .more_button()
        .clone()
        .connect_clicked(move |_| {
            let Some((cat, page)) = main_ref.category.next_page() else {
                return;
            };
            let Some(services) = main_ref.services.borrow().clone() else {
                return;
            };
            main_ref.category.spinner().start();
            let main_inner = main_ref.clone();
            runtime::spawn_ui(
                async move {
                    let backend = match cat.source_kind {
                        "aur" => services
                            .aur
                            .clone()
                            .map(|b| b as std::sync::Arc<dyn PackageBackend>),
                        "flatpak" => services
                            .flatpak
                            .clone()
                            .map(|b| b as std::sync::Arc<dyn PackageBackend>),
                        _ => services
                            .pacman
                            .clone()
                            .map(|b| b as std::sync::Arc<dyn PackageBackend>),
                    };
                    match backend {
                        Some(b) => b.list_category(&cat.id, page).await,
                        None => Ok(Vec::new()),
                    }
                },
                move |result| match result {
                    Ok(items) => {
                        let has_more = items.len() >= crate::pages::category::PAGE_SIZE;
                        main_inner.category.append_page(&items, has_more);
                    }
                    Err(e) => main_inner.category.show_error(&e),
                },
            );
        });
}

/// 设置页接线。
fn wire_settings(main: &Rc<MainWindow>) {
    let main_ref = main.clone();
    main.settings
        .test_connection_button()
        .clone()
        .connect_clicked(move |_| {
            let Some(services) = main_ref.services.borrow().clone() else {
                return;
            };
            let main_inner = main_ref.clone();
            runtime::spawn_ui(
                async move {
                    let cancel = CancelToken::new();
                    let client = services.http.client().await;
                    archstore_core::net::get_json::<serde_json::Value>(
                        &client,
                        "https://aur.archlinux.org/rpc?v=5&type=search&arg=",
                        &cancel,
                        archstore_core::net::RetryPolicy::NONE,
                    )
                    .await
                    .map(|_| ())
                },
                move |result| match result {
                    Ok(()) => ui::toast(&main_inner.toast, &ui::t("连接成功")),
                    Err(e) => ui::toast(&main_inner.toast, &e.user_message()),
                },
            );
        });

    let main_ref = main.clone();
    main.settings
        .clear_cache_button()
        .clone()
        .connect_clicked(move |_| {
            let Some(services) = main_ref.services.borrow().clone() else {
                return;
            };
            let main_inner = main_ref.clone();
            runtime::spawn_ui(
                async move {
                    services.cache.clear().await?;
                    Ok::<_, archstore_core::CoreError>(archstore_core::env::dir_size(
                        &archstore_core::config::paths::cache_dir(),
                    ))
                },
                move |result| match result {
                    Ok(size) => {
                        main_inner.settings.set_cache_size(size);
                        ui::toast(&main_inner.toast, &ui::t("缓存已清空"));
                    }
                    Err(e) => ui::toast(&main_inner.toast, &e.user_message()),
                },
            );
        });

    let main_ref = main.clone();
    main.settings
        .redetect_button()
        .clone()
        .connect_clicked(move |_| {
            load_installed(&main_ref);
            load_updates(&main_ref);
            load_categories(&main_ref);
            ui::toast(&main_ref.toast, &ui::t("已重新检测"));
        });
}

/// 键盘快捷键（§7.1）。
fn wire_shortcuts(main: &Rc<MainWindow>) {
    let main_ref = main.clone();
    let controller = gtk::ShortcutController::new();
    controller.set_scope(gtk::ShortcutScope::Global);
    let add = |accel: &str, cb: Box<dyn Fn() + 'static>| {
        let trigger = gtk::ShortcutTrigger::parse_string(accel);
        let Some(trigger) = trigger else {
            return;
        };
        let action = gtk::CallbackAction::new(move |_, _| {
            cb();
            glib::Propagation::Stop
        });
        let shortcut = gtk::Shortcut::new(Some(trigger), Some(action));
        controller_add(&controller, &shortcut);
    };
    add("F5", {
        let m = main_ref.clone();
        Box::new(move || {
            load_installed(&m);
            load_updates(&m);
        })
    });
    add("<Control>f", {
        let m = main_ref.clone();
        Box::new(move || {
            m.search_entry.grab_focus();
        })
    });
    add("<Control>comma", {
        let m = main_ref.clone();
        Box::new(move || {
            m.stack.set_visible_child_name("settings");
        })
    });
    add("Escape", {
        let m = main_ref.clone();
        Box::new(move || {
            // 详情页上按 Esc 先返回列表，其次才清空搜索框
            if m.nav_view.pop_to_tag("root") {
                return;
            }
            m.search_entry.set_text("");
        })
    });
    add("<Alt>Left", {
        let m = main_ref.clone();
        Box::new(move || {
            m.nav_view.pop();
        })
    });
    main.window.add_controller(controller);
}

fn controller_add(controller: &gtk::ShortcutController, shortcut: &gtk::Shortcut) {
    controller.add_shortcut(shortcut.clone());
}

/// 供 app.rs 使用：应用退出前的收尾。
pub fn shutdown(main: &Rc<MainWindow>) {
    if main.state.tx.borrow().is_running() {
        tracing::warn!("事务仍在执行，退出不会杀掉 helper");
    }
    let Some(services) = main.services.borrow().clone() else {
        return;
    };
    let cache = services.cache.clone();
    runtime::spawn(async move {
        let _ = cache.flush(true).await;
    });
    runtime::shutdown();
}

/// 关闭窗口：Running 时询问用户（不杀 helper）。
pub fn request_close(main: &Rc<MainWindow>) -> glib::Propagation {
    if !main.state.tx.borrow().is_running() {
        return glib::Propagation::Proceed;
    }
    let window = main.window.clone();
    progress_panel::ask_close_while_running(
        &window,
        {
            let window = main.window.clone();
            move || {
                window.set_visible(false);
            }
        },
        || {},
    );
    glib::Propagation::Stop
}
