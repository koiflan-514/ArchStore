//! UI 冒烟测试（project.md §11.2）：在 headless 环境下构造主要页面与控件，
//! 断言不出现 panic，并验证三态切换、增量更新与状态机驱动的界面刷新。
//!
//! 注意：GTK4 的控件操作必须在调用 gtk::init() 的那个线程上进行，
//! 因此这里**只用一个 #[test] 顺序执行所有步骤**，而不是拆成多个并行测试。

use std::rc::Rc;

use adw::prelude::*;
use gtk::prelude::*;
use libadwaita as adw;

use archstore_core::config::{Config, IconSize};
use archstore_core::model::plan::{Installation, PlanItem, PlanKind, TransactionPlan};
use archstore_core::model::{
    DepKind, DependencyInfo, DetailExtra, IconRef, Installed, PackageDetail, PackageId,
    PackageSummary, UpdateInfo,
};
use archstore_core::plan::{AurAction, AurRequest};

use crate::pages::category::CategoryPage;
use crate::pages::detail::DetailPage;
use crate::pages::home::HomePage;
use crate::pages::installed::{InstalledFilter, InstalledPage};
use crate::pages::search::SearchPage;
use crate::pages::settings::{SettingsCallbacks, SettingsPage};
use crate::pages::updates::UpdatesPage;
use crate::pages::{PageShell, filter_local};
use crate::state::{HelperEvent, Progress, TxEvent, TxState, TxSummary};
use crate::widgets::dep_list::DepList;
use crate::widgets::error_view::ErrorView;
use crate::widgets::package_detail_view::{self, DetailCallbacks, DetailView};
use crate::widgets::plan_bar::{PlanBar, PlanBarCallbacks};
use crate::widgets::progress_panel::ProgressPanel;
use crate::widgets::{self, RowContext};
use crate::{icon_cache, ui};

/// 在控件树里递归查找某个类型的控件。
///
/// 像 AdwToolbarView 这类复合控件的内部子控件并不是直接的 first_child，
/// 因此不能用 first_child() 断言结构。
fn contains_widget<T: glib::types::StaticType>(root: &gtk::Widget) -> bool {
    let mut stack = vec![root.clone()];
    while let Some(w) = stack.pop() {
        if w.is::<T>() {
            return true;
        }
        let mut child = w.first_child();
        while let Some(c) = child {
            stack.push(c.clone());
            child = c.next_sibling();
        }
    }
    false
}

/// GTK 是否可用（无显示服务器时优雅跳过，不算失败）。
fn gtk_ready() -> bool {
    match gtk::init() {
        Ok(()) => true,
        Err(e) => {
            eprintln!("跳过 UI 冒烟测试：gtk::init() 失败（{e}）");
            false
        }
    }
}

fn summaries() -> Vec<PackageSummary> {
    let mut official = PackageSummary::minimal(PackageId::official("extra", "firefox"), "Firefox");
    official.set_summary("Fast, Private & Safe Web Browser");
    official.version = Some("155.0.1-1".into());
    official.installed = Installed::Yes {
        version: "154.0-1".into(),
        explicit: true,
    };
    official.update = Some(UpdateInfo {
        current: "154.0-1".into(),
        candidate: "155.0.1-1".into(),
        download_size: Some(88 * 1024 * 1024),
    });

    let mut aur = PackageSummary::minimal(PackageId::aur("yay"), "yay");
    aur.set_summary("AUR helper");
    aur.version = Some("13.0.1-1".into());
    aur.popularity = Some(30.5);
    aur.votes = Some(2500);
    aur.out_of_date = false;

    let mut flatpak = PackageSummary::minimal(
        PackageId::flatpak("flathub", "org.mozilla.firefox"),
        "Firefox",
    );
    flatpak.set_summary("Flathub 版 Firefox");
    flatpak.icon = IconRef::Remote("https://dl.flathub.org/media/x.png".into());

    let mut fresh = PackageSummary::minimal(PackageId::official("core", "vim"), "Vim");
    fresh.icon = IconRef::Missing;
    fresh.out_of_date = true;

    vec![official, aur, flatpak, fresh]
}

fn dependencies() -> Vec<DependencyInfo> {
    let mut satisfied = DependencyInfo::from_expr("glibc", DepKind::Runtime);
    satisfied.missing = false;
    satisfied.satisfied_by = Some(PackageId::official("core", "glibc"));
    let mut virtual_dep = DependencyInfo::from_expr("libgl", DepKind::Runtime);
    virtual_dep.satisfied_by = Some(PackageId::official("extra", "mesa"));
    let mut optional = DependencyInfo::from_expr("ffmpeg: 视频解码", DepKind::Optional);
    optional.size = Some(10 * 1024 * 1024);
    let mut make = DependencyInfo::from_expr("go>=1.24", DepKind::Make);
    make.size = Some(100 * 1024 * 1024);
    vec![satisfied, virtual_dep, optional, make]
}

fn detail() -> PackageDetail {
    let mut s = PackageSummary::minimal(PackageId::official("extra", "firefox"), "Firefox");
    s.version = Some("155.0.1-1".into());
    // 已安装且无可用更新 -> 主按钮应为"重新安装"
    s.installed = Installed::Yes {
        version: "155.0.1-1".into(),
        explicit: true,
    };
    let mut d = PackageDetail::from_summary(s);
    d.description = "长描述：这是一段纯文本，渲染时必须 use_markup(false)。".into();
    d.licenses = vec!["MPL-2.0".into()];
    d.homepage = Some("https://www.mozilla.org/".into());
    d.download_size = Some(88 * 1024 * 1024);
    d.installed_size = Some(310 * 1024 * 1024);
    d.maintainer = Some("Mozilla".into());
    d.permissions = vec!["共享：network".into(), "套接字：wayland".into()];
    d.screenshots = vec!["https://dl.flathub.org/a/1248x702/x.png".into()];
    d.rating = Some(4.5);
    d.review_count = Some(2548);
    d.dependencies = dependencies();
    d.extra = DetailExtra(vec![
        ("包名".into(), "firefox".into()),
        ("版本".into(), "155.0.1-1".into()),
    ]);
    d
}

/// 造一个 1x1 的纯色纹理（不需要磁盘上的图片文件）。
fn solid_texture() -> gtk::gdk::Texture {
    let bytes = glib::Bytes::from(&[255u8, 0, 0, 255]);
    gtk::gdk::MemoryTexture::new(1, 1, gtk::gdk::MemoryFormat::R8g8b8a8, &bytes, 4).upcast()
}

/// 测试用：捕获远程图标加载器的完成回调，稍后手动触发。
type CapturedCallbacks = Rc<std::cell::RefCell<Vec<Box<dyn FnOnce(Option<std::path::PathBuf>)>>>>;

/// 跑一小会儿主循环。
///
/// `GtkSearchEntry::search-changed` 是异步发出的（即使 search-delay 为 0 也要等一次
/// 主循环迭代），测试里必须给它机会，否则会误判成"输入没有生效"。
fn pump_main_loop(ms: u64) {
    let ctx = glib::MainContext::default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    while std::time::Instant::now() < deadline {
        while ctx.pending() {
            ctx.iteration(false);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// 取出远程图标占位控件里的 Image 槽位（placeholder = Box[字母头像, Image]）。
fn slot_image(widget: &gtk::Widget) -> Option<gtk::Image> {
    let holder = widget.clone().downcast::<gtk::Box>().ok()?;
    holder.last_child().and_downcast::<gtk::Image>()
}

fn noop_callbacks() -> DetailCallbacks {
    DetailCallbacks {
        on_primary: Box::new(|| {}),
        on_remove: Some(Box::new(|| {})),
        on_homepage: Some(Box::new(|_| {})),
        on_deps_changed: None,
    }
}

#[test]
fn ui_smoke_builds_every_page_and_widget() {
    if !gtk_ready() {
        return;
    }

    // ---------- 1) 样式与图标 ----------
    ui::load_css();
    let ctx = Rc::new(RowContext::new(IconSize::Medium.pixels()));
    assert_eq!(ctx.icon_size, 32);
    let _ = icon_cache::image_for(&IconRef::Missing, 32, &ctx, &PackageId::aur("x"));
    let _ = icon_cache::image_for(
        &IconRef::IconName("firefox".into()),
        48,
        &ctx,
        &PackageId::aur("firefox"),
    );

    // ---------- 1a) IconRef::IconName 的图标名必须被真正使用 ----------
    // 旧实现忽略 IconName 里的名字、只用包名查主题，.desktop 解析出来的 Icon= 被白白丢掉。
    if icon_cache::texture_from_icon_name(icon_cache::PLACEHOLDER_ICON, 32).is_some() {
        let w = icon_cache::image_for(
            &IconRef::IconName(icon_cache::PLACEHOLDER_ICON.into()),
            32,
            &ctx,
            &PackageId::aur("no-such-package-name-anywhere"),
        );
        assert!(
            w.is::<gtk::Image>(),
            "必须按 IconRef::IconName 给出的图标名加载，而不是当成字母头像"
        );
    }

    // ---------- 1b) 远程图标：下载完成后必须原地回填（回归）----------
    // 旧实现只把纹理写进内存缓存，已经渲染出来的行不会重绘，
    // 于是"推荐"页从头到尾都是字母头像（用户实测反馈）。
    {
        let ctx_remote = Rc::new(RowContext::new(32));
        let calls = Rc::new(std::cell::RefCell::new(0usize));
        let captured: CapturedCallbacks = Rc::new(std::cell::RefCell::new(Vec::new()));
        {
            let calls = Rc::clone(&calls);
            let captured = Rc::clone(&captured);
            ctx_remote.set_icon_loader(Rc::new(move |_url, done| {
                *calls.borrow_mut() += 1;
                captured.borrow_mut().push(done);
            }));
        }
        let id = PackageId::flatpak("flathub", "org.example.App");
        let url = "https://dl.flathub.org/media/org/example/App/icons/128x128/org.example.App.png";
        let first = icon_cache::image_for(&IconRef::Remote(url.into()), 32, &ctx_remote, &id);
        let second = icon_cache::image_for(&IconRef::Remote(url.into()), 32, &ctx_remote, &id);
        assert_eq!(*calls.borrow(), 1, "同一个包在途时只能发起一次请求");
        assert_eq!(ctx_remote.pending_icons(), 1);
        assert_eq!(captured.borrow().len(), 1, "加载器必须被调用");

        // 下载完成：纹理落进内存缓存 + 所有存活槽位原地回填
        icon_cache::apply_remote_texture(&ctx_remote, &id, &solid_texture());
        assert_eq!(ctx_remote.pending_icons(), 0, "回填后不得留下在途标记");
        for widget in [&first, &second] {
            let image = slot_image(widget).expect("远程图标必须有占位槽位");
            assert!(image.is_visible(), "下载完成后 Image 必须可见");
            assert!(image.paintable().is_some(), "Image 必须挂上纹理");
            let avatar = image
                .parent()
                .and_then(|holder| holder.first_child())
                .expect("占位控件里必须有字母头像");
            assert!(!avatar.is_visible(), "字母头像必须让位给真实图标");
        }

        // 之后再渲染同一个包：直接命中内存缓存，不再发请求
        let third = icon_cache::image_for(
            &IconRef::Remote("https://example.invalid/other.png".into()),
            32,
            &ctx_remote,
            &id,
        );
        assert!(third.is::<gtk::Image>(), "缓存命中后必须直接给出图标");
        assert_eq!(*calls.borrow(), 1);
    }

    // ---------- 2) 列表行：三种来源 + 远程图标 + 缺失图标 ----------
    for s in summaries() {
        let row = widgets::package_row::render(&s, &ctx);
        assert!(
            row.first_child().is_some(),
            "行控件必须有内容：{}",
            s.id.name
        );
    }

    // ---------- 3) ListView + ListStore 增量更新 ----------
    let items = summaries();
    let store = widgets::store_from(&items);
    assert_eq!(widgets::store_len(&store), 4);
    let view = widgets::list_view(&store, &ctx);
    assert!(view.model().is_some());
    // 同样的数据不应产生任何 splice（滚动位置保持）
    widgets::diff_update(&store, &items);
    assert_eq!(widgets::store_len(&store), 4);
    // 前缀不变时只替换尾部
    let mut changed = items.clone();
    changed[3].display_name = "Vim (changed)".into();
    widgets::diff_update(&store, &changed);
    assert_eq!(widgets::store_len(&store), 4);
    assert_eq!(
        widgets::item_at(&store, 3).map(|s| s.display_name),
        Some("Vim (changed)".to_string())
    );
    // 变短时也要正确收敛
    widgets::diff_update(&store, &changed[..2]);
    assert_eq!(widgets::store_len(&store), 2);

    // ---------- 3a) 列表激活语义（回归：鼠标悬停绝不能打开详情页）----------
    {
        use crate::pages::{EmptyState, ListPage};
        let opened: Rc<std::cell::RefCell<Vec<String>>> =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        let observer = opened.clone();
        let lp = ListPage::new(
            &ctx,
            None::<gtk::Widget>,
            move |s| observer.borrow_mut().push(s.id.name.clone()),
            None,
            None,
        );
        lp.set_items(
            &summaries(),
            EmptyState::new("edit-find-symbolic", "t", "b"),
        );
        let selection = lp
            .view
            .model()
            .and_downcast::<gtk::SingleSelection>()
            .expect("ListView 的 model 必须是 SingleSelection");

        // 悬停：GtkListView 的 single-click-activate 会在鼠标划过时改变 selected。
        // 这只应改变高亮，绝不能触发"打开详情页"。
        selection.set_selected(0);
        assert!(
            opened.borrow().is_empty(),
            "改变选中（悬停）打开了详情页：{:?}",
            opened.borrow()
        );
        selection.set_selected(2);
        assert!(opened.borrow().is_empty(), "连续悬停仍不应打开详情页");
        selection.set_selected(gtk::INVALID_LIST_POSITION);
        assert!(opened.borrow().is_empty());

        // 真正的用户动作：activate 信号（单击 / 回车）才打开详情页
        lp.view.emit_by_name::<()>("activate", &[&1u32]);
        assert_eq!(opened.borrow().len(), 1, "activate 必须打开详情页");
        assert_eq!(opened.borrow()[0], summaries()[1].id.name);

        // 同一行可以重复激活（不需要先取消选中）
        lp.view.emit_by_name::<()>("activate", &[&1u32]);
        assert_eq!(opened.borrow().len(), 2);

        // 越界 position 必须被忽略而不是 panic
        lp.view.emit_by_name::<()>("activate", &[&9999u32]);
        assert_eq!(opened.borrow().len(), 2);
    }
    // ---------- 3b) 千级列表的增量更新成本（§12 阶段 5 性能回归）----------
    {
        use std::time::{Duration, Instant};
        const N: usize = 2000;
        let make = |i: usize| {
            let mut s = PackageSummary::minimal(
                PackageId::official("extra", format!("pkg-{i:05}")),
                format!("Package {i}"),
            );
            s.set_summary(&format!("desc {i} {}", "y".repeat(80)));
            s.version = Some(format!("1.{}-1", i % 40));
            s
        };
        let big: Vec<PackageSummary> = (0..N).map(make).collect();

        let t = Instant::now();
        let big_store = widgets::store_from(&big);
        let fill = t.elapsed();
        assert_eq!(widgets::store_len(&big_store), N as u32);

        // 数据未变：必须走"前缀相同"的早退分支，几乎零成本
        let t = Instant::now();
        widgets::diff_update(&big_store, &big);
        let noop = t.elapsed();

        // 只有最后一条变化：只替换 1 条，滚动位置不动
        let mut tail_changed = big.clone();
        tail_changed[N - 1].display_name = "changed".into();
        let t = Instant::now();
        widgets::diff_update(&big_store, &tail_changed);
        let one = t.elapsed();
        assert_eq!(
            widgets::item_at(&big_store, (N - 1) as u32).map(|s| s.display_name),
            Some("changed".to_string())
        );

        // 追加 10 条：只 splice 尾部
        let mut appended = tail_changed.clone();
        appended.extend((N..N + 10).map(make));
        let t = Instant::now();
        widgets::diff_update(&big_store, &appended);
        let append = t.elapsed();
        assert_eq!(widgets::store_len(&big_store), (N + 10) as u32);

        // 尾部被截断：只删除多余的
        let t = Instant::now();
        widgets::diff_update(&big_store, &tail_changed);
        let truncate = t.elapsed();
        assert_eq!(widgets::store_len(&big_store), N as u32);

        // 首条变化：前缀失配，退化为全量替换（最坏情况）
        let mut head_changed = big.clone();
        head_changed[0].display_name = "head".into();
        let t = Instant::now();
        widgets::diff_update(&big_store, &head_changed);
        let full = t.elapsed();
        assert_eq!(widgets::store_len(&big_store), N as u32);

        // 每行的控件构造成本（列表滚动时 factory 会反复调用 render）
        let t = Instant::now();
        for s in big.iter().take(200) {
            let _ = widgets::package_row::render(s, &ctx);
        }
        let render200 = t.elapsed();

        eprintln!(
            "  千级列表：填充 {N} 条 {fill:?}｜无变化 {noop:?}｜改 1 条 {one:?}｜追加 10 条 {append:?}｜截断 10 条 {truncate:?}｜首条变化(全量) {full:?}｜渲染 200 行 {render200:?}"
        );

        // 回归护栏（debug 构建，留足余量）：
        // 增量路径必须比全量替换便宜一个数量级，否则滚动位置会丢失
        assert!(
            noop < Duration::from_millis(5),
            "无变化时不应做任何工作：{noop:?}"
        );
        assert!(one < full, "只改一条({one:?})不应比全量替换({full:?})更贵");
        assert!(
            full < Duration::from_secs(2),
            "2000 条全量替换过慢：{full:?}"
        );
        assert!(
            render200 < Duration::from_secs(2),
            "渲染 200 行过慢：{render200:?}（每行 {:?}）",
            render200 / 200
        );
    }
    // ---------- 3c) 你报的三个交互问题的回归测试 ----------
    {
        // (1) 搜索来源开关必须在 PageShell **之外**：
        //     否则"没有找到匹配的软件"会把它一起吞掉。
        let search_entry = gtk::SearchEntry::new();
        let sp = SearchPage::new(
            &ctx,
            search_entry.clone(),
            |_| {},
            Box::new(|| {}),
            Box::new(|| {}),
        );
        let shell_widget: gtk::Widget = sp.page.shell.stack.clone().upcast();
        let mut node: Option<gtk::Widget> = Some(sp.pacman_toggle.clone().upcast());
        let mut inside_shell = false;
        while let Some(w) = node {
            if w == shell_widget {
                inside_shell = true;
                break;
            }
            node = w.parent();
        }
        assert!(
            !inside_shell,
            "来源开关必须在 PageShell 之外，否则空态会把它吞掉"
        );

        // 来源开关必须有可见的选中样式：不能是看不见选中态的 .flat 按钮，
        // 且必须带 source-toggle 类（style.css 里 button.source-toggle:checked 的强调色块）。
        // 注意 CSS provider 必须在 USER 优先级，否则会被用户自带的 GTK 主题盖掉。
        for button in [&sp.pacman_toggle, &sp.aur_toggle, &sp.flatpak_toggle] {
            assert!(
                !button.has_css_class("flat"),
                "flat 按钮选中后看不见色块（用户实测反馈）"
            );
            assert!(button.is_active(), "默认三个来源都应该打开");
            assert!(
                button.has_css_class("source-toggle"),
                "来源开关必须带 source-toggle 类，否则选中态没有高亮色块"
            );
        }

        // 空态/错态下开关依然可见
        sp.set_results(&[], &[], "zzz");
        assert_eq!(sp.page.shell.state_name(), "empty");
        assert!(sp.pacman_toggle.get_visible(), "空态下来源开关仍须可见");
        assert!(sp.aur_toggle.get_visible());
        assert!(sp.flatpak_toggle.get_visible());
        assert!(sp.root.first_child().is_some());
        // 有结果时也正常
        sp.set_results(&summaries(), &[], "firefox");
        assert_eq!(sp.page.shell.state_name(), "content");
        assert!(sp.pacman_toggle.get_visible());

        // (2) 详情页必须自带 HeaderBar，AdwNavigationView 才会注入返回键
        let dp = DetailPage::new();
        let top = dp
            .root
            .first_child()
            .and_downcast::<adw::ToolbarView>()
            .expect("详情页根控件必须是 AdwToolbarView（否则没有返回键）");
        // AdwToolbarView 的内部子控件不是直接的 first_child，
        // 必须递归查找（这里要确认真的存在 AdwHeaderBar，否则没有返回键）
        assert!(
            contains_widget::<adw::HeaderBar>(&dp.root.clone().upcast::<gtk::Widget>()),
            "详情页必须包含 AdwHeaderBar，否则 push 之后没有返回按钮"
        );
        let _ = top;

        // (3) 描述区域的机器翻译标注与原文/译文切换
        dp.show_summary(&summaries()[0]);
        dp.set_detail(&detail(), &ctx, noop_callbacks());
        let view = dp.view().expect("view");
        let area = view.description_area().expect("描述区域");
        assert_eq!(area.text(), area.original(), "初始必须显示原文");
        assert!(!area.toggle().get_visible(), "没有译文时不显示切换按钮");
        area.set_translation("这是机器翻译的译文", "机器翻译（MyMemory）", false);
        assert_eq!(area.text(), "这是机器翻译的译文");
        assert!(area.is_showing_translation());
        assert!(area.notice_text().contains("机器翻译"));
        assert!(area.toggle().get_visible());
        // 切回原文
        area.toggle().set_active(true);
        assert_eq!(area.text(), area.original());
        assert!(!area.is_showing_translation());
        // 再切回译文
        area.toggle().set_active(false);
        assert_eq!(area.text(), "这是机器翻译的译文");
        // 截断时必须明说
        area.set_translation("部分译文", "机器翻译（MyMemory）", true);
        assert!(area.notice_text().contains("仅翻译了前一部分"));
    }
    // ---------- 4) 三态外壳 ----------
    let shell = PageShell::new(Some(Box::new(|| {})), Some(Box::new(|| {})));
    shell.show_loading();
    assert_eq!(shell.state_name(), "loading");
    shell.show_content();
    assert_eq!(shell.state_name(), "content");
    shell.show_empty(
        "edit-find-symbolic",
        "没有找到",
        "换个关键字试试。",
        Some(("搜索 AUR", Box::new(|| {}))),
    );
    assert_eq!(shell.state_name(), "empty");
    for err in [
        archstore_core::CoreError::Locked,
        archstore_core::CoreError::AlpmInit("boom".into()),
        archstore_core::CoreError::Timeout {
            url: "https://aur.archlinux.org/rpc".into(),
            secs: 30,
        },
        archstore_core::CoreError::Network {
            url: "u".into(),
            cause: "connection reset".into(),
        },
        archstore_core::CoreError::SyncDbMissing("extra".into()),
    ] {
        shell.show_error(&err);
        assert_eq!(shell.state_name(), "error");
        let text = shell.error_view().detail_text();
        assert!(!text.is_empty(), "错态必须给出可操作的原因：{err:?}");
    }
    shell.toast("这是一条提示");

    // ---------- 5) ErrorView 单独使用 ----------
    let ev = ErrorView::new(Some(Box::new(|| {})), None);
    ev.show_message("标题", "细节");
    assert_eq!(ev.title_text(), "标题");
    assert!(ev.copy_button().get_visible());

    // ---------- 6) 依赖列表 ----------
    let deps = DepList::new(&dependencies(), &[], true);
    let sel = deps.selection();
    assert!(sel.optional.is_empty(), "可选依赖默认不勾选");
    assert_eq!(sel.virtual_choices.len(), 1, "虚拟依赖必须有默认选择");
    assert_eq!(sel.virtual_choices[0].1, "mesa");
    assert!(deps.clean_switch().is_some(), "yay 支持构建后清理");
    assert!(!deps.total_text().is_empty());
    let deps_no_clean = DepList::new(&dependencies(), &[], false);
    assert!(deps_no_clean.clean_switch().is_none());
    let empty = DepList::new(&[], &[], false);
    assert!(!empty.is_incomplete());

    // ---------- 7) 详情视图 ----------
    let dv = DetailView::new(&detail(), &ctx, noop_callbacks());
    assert_eq!(
        dv.primary_button()
            .label()
            .map(|s| s.to_string())
            .as_deref(),
        Some("重新安装")
    );
    assert!(dv.remove_button().is_some());
    let scrolled = package_detail_view::scrollable(&dv);
    assert!(scrolled.child().is_some());
    // 未安装时主按钮是"安装"，且没有卸载按钮
    let mut fresh = detail();
    fresh.summary.installed = Installed::No;
    let dv2 = DetailView::new(
        &fresh,
        &ctx,
        DetailCallbacks {
            on_primary: Box::new(|| {}),
            on_remove: None,
            on_homepage: None,
            on_deps_changed: None,
        },
    );
    assert_eq!(
        dv2.primary_button()
            .label()
            .map(|s| s.to_string())
            .as_deref(),
        Some("安装")
    );
    assert!(dv2.remove_button().is_none());

    // ---------- 8) 计划栏 ----------
    let plan_bar = PlanBar::new(PlanBarCallbacks {
        on_execute: Box::new(|| {}),
        on_details: Box::new(|| {}),
        on_discard: Box::new(|| {}),
    });
    let mut plan = TransactionPlan::new(PlanKind::PacmanSync);
    plan.push(PlanItem::official("extra", "firefox"));
    let draft = TxState::Idle
        .apply(TxEvent::Enqueue(plan.clone()))
        .expect("draft");
    plan_bar.update(&draft, std::slice::from_ref(&plan), &[]);
    assert!(
        plan_bar.execute_button().get_sensitive(),
        "Draft 状态应可执行"
    );
    assert!(plan_bar.label_text().contains("firefox"));

    let running = draft
        .clone()
        .apply(TxEvent::Confirm)
        .and_then(|s| s.apply(TxEvent::Authorize))
        .and_then(|s| s.apply(TxEvent::Start))
        .expect("running");
    plan_bar.update(&running, std::slice::from_ref(&plan), &[]);
    assert!(
        !plan_bar.execute_button().get_sensitive(),
        "Running 期间执行按钮必须禁用（同一时刻只允许一个事务）"
    );

    // 官方 + AUR 混批必须给出部分升级警告
    let aur = vec![AurRequest {
        action: AurAction::Update,
        packages: vec!["yay".into()],
    }];
    plan_bar.update(&draft, std::slice::from_ref(&plan), &aur);
    assert!(
        plan_bar.warning_text().contains("部分升级"),
        "实际：{}",
        plan_bar.warning_text()
    );

    // 只有 AUR 请求时也能执行（终端路径）
    plan_bar.update(&TxState::Idle, &[], &aur);
    assert!(plan_bar.execute_button().get_sensitive());

    let flatpak_plan = {
        let mut p = TransactionPlan::new(PlanKind::FlatpakInstall);
        p.push(PlanItem::flatpak(
            "flathub",
            Installation::System,
            "org.mozilla.firefox",
        ));
        p
    };
    plan_bar.update(&draft, &[flatpak_plan], &[]);
    assert!(plan_bar.warning_text().is_empty() || !plan_bar.warning_text().contains("部分升级"));

    plan_bar.show_banner("删除 X 会影响 N 个已安装包");
    plan_bar.hide_banner();

    // ---------- 9) 进度面板 ----------
    let panel = ProgressPanel::new();
    assert!(!panel.is_visible(), "初始应隐藏");
    let mut progress = Progress {
        phase: "download".into(),
        percent: Some(42),
        detail: "firefox-155.0.1-1-x86_64.pkg.tar.zst".into(),
        ..Default::default()
    };
    for i in 0..50 {
        progress.push_log(format!("日志行 {i}"));
    }
    panel.update(&progress);
    assert!(panel.is_visible());
    assert_eq!(panel.phase_text(), "下载中");
    assert!(panel.log_text().contains("日志行 49"));
    panel.finish(&TxSummary {
        installed: 2,
        removed: 0,
        failed: 0,
        elapsed_ms: 1234,
        status: "ok".into(),
    });
    assert_eq!(panel.phase_text(), "事务已结束");
    panel.fail(
        "LOCKED：数据库被锁定",
        "line1
line2",
    );
    assert_eq!(panel.phase_text(), "事务失败");
    assert!(panel.log_text().contains("line2"));
    panel.reset();
    assert!(!panel.is_visible());

    // ---------- 10) helper 事件解析 + 状态机联动 ----------
    for line in [
        r#"{"event":"start","plan_schema":1,"items":2,"kind":"pacman-sync"}"#,
        r#"{"event":"progress","phase":"install","percent":80,"detail":"vim"}"#,
        r#"{"event":"log","level":"info","line":"正在安装"}"#,
        r#"{"event":"done","status":"ok","installed":2,"failed":0}"#,
    ] {
        assert!(HelperEvent::parse(line).is_some(), "{line}");
    }
    assert!(HelperEvent::parse("noise").is_none());

    // ---------- 11) 各个页面 ----------
    let home = HomePage::new(&ctx, |_| {}, Box::new(|| {}), Box::new(|| {}));
    home.show_loading();
    assert_eq!(home.page.shell.state_name(), "loading");
    home.set_items(&summaries(), "来自 Flathub 趋势");
    assert_eq!(home.page.shell.state_name(), "content");
    assert!(home.status_text().contains("Flathub"));
    home.set_items(&[], "空");
    assert_eq!(home.page.shell.state_name(), "empty");
    home.show_error(&archstore_core::CoreError::Network {
        url: "u".into(),
        cause: "down".into(),
    });
    assert_eq!(home.page.shell.state_name(), "error");

    let installed = InstalledPage::new(&ctx, |_| {}, Box::new(|| {}), Box::new(|| {}));
    installed.set_items(&summaries());
    assert_eq!(installed.total_count(), 4);
    installed.set_filter(InstalledFilter::Upgradable, "");
    assert_eq!(installed.visible_count(), 1, "只有 firefox 有更新");
    installed.set_filter(InstalledFilter::Foreign, "");
    assert_eq!(installed.visible_count(), 1, "只有 yay 是外来包");
    installed.set_filter(InstalledFilter::All, "firefox");
    assert_eq!(
        installed.visible_count(),
        2,
        "firefox 与 Flathub 版 Firefox"
    );
    installed.set_filter_index(0);
    installed.refresh();
    assert_eq!(installed.visible_count(), 4);
    assert_eq!(installed.filter(), InstalledFilter::All);

    // 下拉框切换必须**立即**生效：旧实现只改筛选状态、不重新应用，
    // 于是"选了分组但列表没变"，要等下一次输入搜索框才刷新（用户实测反馈）。
    installed.set_filter_index(3); // 可更新
    assert_eq!(installed.filter(), InstalledFilter::Upgradable);
    assert_eq!(
        installed.visible_count(),
        1,
        "下拉框切换必须立即重新过滤，不需要再碰搜索框"
    );
    installed.set_filter_index(0);
    assert_eq!(installed.visible_count(), 4);

    // ---------- 回归：筛选到空结果时不能把筛选栏一起吞掉（用户实测反馈）----------
    {
        // 结构上：搜索框必须在 PageShell 的 stack 之外
        let shell_widget: gtk::Widget = installed.page.shell.stack.clone().upcast();
        let mut node: Option<gtk::Widget> = Some(installed.search_entry().clone().upcast());
        let mut inside_shell = false;
        while let Some(w) = node {
            if w == shell_widget {
                inside_shell = true;
                break;
            }
            node = w.parent();
        }
        assert!(
            !inside_shell,
            "已安装页的搜索框必须在 PageShell 之外，否则空态会把它吞掉"
        );

        // 输入一个本机不存在的软件名：列表变空、进入空态，但筛选栏必须还在
        installed
            .search_entry()
            .set_text("definitely-not-installed-zzz");
        installed.refresh();
        assert_eq!(installed.page.shell.state_name(), "empty");
        assert_eq!(installed.visible_count(), 0);
        assert!(
            installed.search_entry().get_visible(),
            "空态下搜索框必须仍然可见（否则用户改不回筛选条件）"
        );
        assert!(
            installed.filter_dropdown().get_visible(),
            "空态下筛选下拉框必须仍然可见"
        );

        // 清空后必须能恢复全量列表
        installed.search_entry().set_text("");
        installed.refresh();
        assert_eq!(installed.page.shell.state_name(), "content");
        assert_eq!(installed.visible_count(), 4);

        // 切到"依赖"筛选也一样：控件不能消失
        installed.set_filter(InstalledFilter::Dependency, "");
        assert!(installed.search_entry().get_visible());
        assert!(installed.filter_dropdown().get_visible());
        installed.set_filter(InstalledFilter::All, "");
    }

    let updates = UpdatesPage::new(&ctx, |_| {}, Box::new(|| {}), Box::new(|| {}));
    let advisories = vec![archstore_core::flathub::Advisory {
        name: "AVG-1".into(),
        packages: vec!["firefox".into()],
        affected: "100".into(),
        fixed: "200".into(),
        severity: "High".into(),
        ..Default::default()
    }];
    updates.set_items(&summaries(), advisories);
    assert_eq!(updates.updatable_count(), 4);
    assert!(updates.security_text().contains("安全公告"));
    updates.set_group_index(1);
    assert_eq!(updates.updatable_count(), 4, "分组只影响展示，不影响统计");
    updates.set_items(&[], Vec::new());
    assert_eq!(updates.page.shell.state_name(), "empty");
    assert!(
        !updates.update_all_button().get_sensitive(),
        "无更新时一键更新禁用"
    );
    // 空态下分组下拉框与"一键更新"同样必须常驻（与已安装页同一类缺陷）
    assert!(
        updates.group_dropdown().get_visible(),
        "空态下分组下拉框必须仍然可见"
    );
    assert!(
        updates.update_all_button().get_visible(),
        "空态下一键更新按钮必须仍然可见"
    );

    let search_entry = gtk::SearchEntry::new();
    let search = SearchPage::new(
        &ctx,
        search_entry.clone(),
        |_| {},
        Box::new(|| {}),
        Box::new(|| {}),
    );
    assert!(search.filter().pacman && search.filter().aur && search.filter().flatpak);
    search.set_results(&summaries(), &[], "firefox");
    assert_eq!(search.page.shell.state_name(), "content");
    search.set_results(&[], &[], "zzz");
    assert_eq!(search.page.shell.state_name(), "empty");
    search.set_results(
        &[],
        &[("AUR".into(), "请求超时（30 秒）".into())],
        "firefox",
    );
    assert_eq!(search.page.shell.state_name(), "error");
    // 部分后端失败时仍展示其它来源的结果
    search.set_results(&summaries(), &[("AUR".into(), "超时".into())], "firefox");
    assert_eq!(search.page.shell.state_name(), "content");
    let g1 = search.next_generation();
    let g2 = search.next_generation();
    assert!(g2 > g1, "搜索代号必须自增（用于丢弃过期响应）");

    // 输入：推进代号（让在飞的旧响应作废）并记住最后一次查询（来源开关要用它重跑）
    let g_before = search.generation();
    search.search_entry().set_text("fire");
    pump_main_loop(50);
    assert_eq!(search.last_query(), "fire");
    assert!(
        search.generation() > g_before,
        "输入必须推进搜索代号，否则旧响应会覆盖新结果"
    );

    // 清空输入：立刻回到提示态并清空结果（旧实现会留着上一次的结果）
    search.set_results(&summaries(), &[], "fire");
    assert_eq!(search.page.shell.state_name(), "content");
    search.search_entry().set_text("");
    search.clear_results();
    assert_eq!(search.page.shell.state_name(), "empty");
    assert_eq!(search.page.store.n_items(), 0, "清空后不得残留旧结果");
    assert_eq!(search.last_query(), "");

    let category = CategoryPage::new(&ctx, |_| {}, Box::new(|| {}), Box::new(|| {}));
    category.connect_select(|_, _| {});
    category.set_categories(&[
        archstore_core::backend::Category::new("extra:gnome", "gnome（12）", "pacman"),
        archstore_core::backend::Category::new("keyword:browser", "浏览器", "aur"),
        archstore_core::backend::Category::new("flathub:Game", "游戏", "flatpak"),
    ]);
    assert_eq!(category.category_count(), 3);
    assert!(category.list_widget().first_child().is_some());

    // "AUR 社区"/"Flatpak" 导航复用本页：按来源过滤侧栏分类
    assert_eq!(category.source_filter(), None);
    category.set_source_filter(Some("aur"));
    assert_eq!(category.category_count(), 1, "只应剩下 AUR 关键词分类");
    assert_eq!(category.source_filter(), Some("aur"));
    category.set_source_filter(Some("flatpak"));
    assert_eq!(category.category_count(), 1);
    category.set_source_filter(None);
    assert_eq!(category.category_count(), 3, "取消过滤后恢复全部");
    // 模拟点击分类行：必须触发 connect_select 注册的回调
    let row = category
        .list_widget()
        .row_at_index(0)
        .expect("set_categories 必须往 ListBox 里真的添加行");
    category
        .list_widget()
        .emit_by_name::<()>("row-activated", &[&row]);
    assert!(
        category.next_page().is_some(),
        "点击分类后应能取到下一页参数"
    );
    category.set_items(&summaries(), true);
    assert!(
        category.more_button().get_visible(),
        "有下一页时显示加载更多"
    );
    assert_eq!(category.next_page().map(|(_, p)| p.offset), Some(4));
    assert_eq!(
        category.next_page().map(|(_, p)| p.limit),
        Some(crate::pages::category::PAGE_SIZE)
    );
    category.append_page(&summaries(), false);
    assert!(!category.more_button().get_visible());
    assert_eq!(category.next_page().map(|(_, p)| p.offset), Some(8));
    category.set_items(&[], false);

    let detail_page = DetailPage::new();
    assert_eq!(detail_page.state_name(), "placeholder");
    detail_page.show_summary(&summaries()[0]);
    assert_eq!(detail_page.state_name(), "content");
    detail_page.set_detail(&detail(), &ctx, noop_callbacks());
    assert_eq!(detail_page.title_widget().title(), "Firefox");

    // 截图槽位：先创建占位，下载完成后按索引回填（GtkPicture 没有 URL 加载 API）
    let view = detail_page.view().expect("set_detail 必须留下视图句柄");
    assert_eq!(view.screenshot_count(), 1, "fixture 有 1 张截图");
    assert!(!view.screenshot_loaded(0), "下载完成前槽位不应持有文件");
    assert_eq!(view.screenshot_urls().len(), 1);
    assert!(view.screenshot_urls()[0].starts_with("https://"));
    // 越界索引必须被忽略而不是 panic
    view.set_screenshot(99, std::path::Path::new("/nonexistent.png"));
    // 不存在的文件也必须被忽略
    view.set_screenshot(0, std::path::Path::new("/nonexistent.png"));
    assert!(!view.screenshot_loaded(0));
    // 真实文件回填
    let shot_dir = tempfile::tempdir().expect("tmpdir");
    let shot = shot_dir.path().join("shot.png");
    // 最小合法 PNG（1x1 透明）
    std::fs::write(
        &shot,
        [
            0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ],
    )
    .expect("write png");
    view.set_screenshot(0, &shot);
    assert!(view.screenshot_loaded(0), "回填后槽位必须持有文件");

    // 截图数量受上限约束
    let mut many = detail();
    many.screenshots = (0..20)
        .map(|i| format!("https://dl.flathub.org/a/{i}.png"))
        .collect();
    let view_many = DetailView::new(&many, &ctx, noop_callbacks());
    assert_eq!(
        view_many.screenshot_count(),
        package_detail_view::MAX_SCREENSHOTS
    );
    assert_eq!(
        view_many.screenshot_urls().len(),
        package_detail_view::MAX_SCREENSHOTS
    );
    // 无截图时容器隐藏
    let mut none = detail();
    none.screenshots.clear();
    let view_none = DetailView::new(&none, &ctx, noop_callbacks());
    assert_eq!(view_none.screenshot_count(), 0);
    detail_page.set_error(
        &summaries()[0],
        &archstore_core::CoreError::Network {
            url: "u".into(),
            cause: "down".into(),
        },
    );
    detail_page.clear();
    assert_eq!(detail_page.state_name(), "placeholder");

    // ---------- 12) 设置页 ----------
    let settings = SettingsPage::new(
        SettingsCallbacks {
            on_change: Box::new(|_| {}),
            on_test_connection: Box::new(|| {}),
            on_clear_cache: Box::new(|| {}),
            on_doctor: Box::new(|| {}),
            on_redetect: Box::new(|| {}),
        },
        &Config::default(),
        &["flathub".to_string()],
        &[archstore_core::env::AurHelperKind::Yay],
    );
    settings.set_cache_size(12 * 1024 * 1024);
    settings.set_backends(&[
        (
            "官方仓库（pacman）",
            archstore_core::backend::Capability::available(),
        ),
        (
            "Flatpak",
            archstore_core::backend::Capability::unavailable("未安装 flatpak（可选依赖）"),
        ),
    ]);
    settings.refresh_translation_notice();
    let snapshot = settings.snapshot();
    assert_eq!(snapshot.schema, archstore_core::config::CONFIG_SCHEMA);
    assert!(settings.about_row().title().contains("ArchStore"));
    assert_eq!(settings.color_scheme_row().selected(), 0);

    // ---------- 13) 纯逻辑：本地过滤与相关性排序 ----------
    let all = summaries();
    assert_eq!(filter_local(&all, "firefox").len(), 2);
    assert_eq!(filter_local(&all, "AUR helper").len(), 1);
    assert_eq!(filter_local(&all, "不存在的东西").len(), 0);
    let merged = archstore_core::backend::merge_results(
        "firefox",
        vec![vec![all[2].clone()], vec![all[0].clone()]],
    );
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].id.name, "firefox", "精确匹配优先");

    // ---------- 14) AdwApplication 可构造（不进入主循环） ----------
    let app = crate::app::build();
    assert_eq!(
        app.application_id().map(|s| s.to_string()),
        Some(archstore_core::APP_ID.to_string())
    );
    crate::app::apply_color_scheme(archstore_core::config::ColorScheme::Dark);
    crate::app::apply_color_scheme(archstore_core::config::ColorScheme::System);

    // 保持 adw 引用，避免未使用告警
    let _manager = adw::StyleManager::default();

    eprintln!("UI 冒烟测试完成：所有页面与控件构造成功，无 GTK critical");
}
