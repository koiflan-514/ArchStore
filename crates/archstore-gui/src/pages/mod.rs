//! 页面层：每个页面都必须实现加载 / 空 / 错三种非正常状态（§7.3）。

pub mod category;
pub mod detail;
pub mod home;
pub mod installed;
pub mod search;
pub mod settings;
pub mod updates;

use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;

use archstore_core::model::PackageSummary;

use crate::ui;
use crate::widgets::error_view::ErrorView;
use crate::widgets::{self, RowContext};

/// 空态行动按钮：(标签, 回调)。
pub type EmptyAction = (&'static str, Box<dyn Fn() + 'static>);

/// 页面外壳：加载态 / 空态 / 错态 / 内容态四选一。
#[derive(Debug, Clone)]
pub struct PageShell {
    pub stack: gtk::Stack,
    pub content: gtk::Box,
    error: Rc<ErrorView>,
    spinner: gtk::Spinner,
    loading_box: gtk::Box,
    empty_box: gtk::Box,
    /// 页面自身的 Toast 层（window 需要把它放进 ViewStack）
    pub overlay: adw::ToastOverlay,
}

impl PageShell {
    /// 构造三态外壳。回调为 None 时对应按钮隐藏。
    pub fn new(
        on_retry: Option<Box<dyn Fn() + 'static>>,
        on_settings: Option<Box<dyn Fn() + 'static>>,
    ) -> Self {
        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .vexpand(true)
            .build();

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_vexpand(true);

        // 加载态：骨架屏占位行
        let loading_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
        loading_box.set_margin_top(12);
        loading_box.set_margin_bottom(12);
        loading_box.set_margin_start(12);
        loading_box.set_margin_end(12);
        let spinner = gtk::Spinner::new();
        spinner.set_size_request(24, 24);
        spinner.set_halign(gtk::Align::Center);
        let loading_label = ui::label(&ui::t("正在加载…"));
        loading_label.add_css_class("dim-label");
        loading_box.append(&spinner);
        loading_box.append(&loading_label);
        for _ in 0..6 {
            loading_box.append(&ui::skeleton_row());
        }

        let empty_box = gtk::Box::new(gtk::Orientation::Vertical, 0);

        let error = Rc::new(ErrorView::new(on_retry, on_settings));
        let error_scroll = gtk::ScrolledWindow::builder()
            .child(&error.root)
            .vexpand(true)
            .build();

        stack.add_named(&loading_box, Some("loading"));
        stack.add_named(&empty_box, Some("empty"));
        stack.add_named(&error_scroll, Some("error"));
        stack.add_named(&content, Some("content"));
        stack.set_visible_child_name("loading");

        let overlay = adw::ToastOverlay::new();
        overlay.set_child(Some(&stack));

        let shell = Self {
            stack,
            content,
            error,
            spinner,
            loading_box,
            empty_box,
            overlay,
        };
        shell.show_loading();
        shell
    }

    /// 加载态。
    pub fn show_loading(&self) {
        self.spinner.start();
        self.stack.set_visible_child_name("loading");
    }

    /// 内容态。
    pub fn show_content(&self) {
        self.spinner.stop();
        self.stack.set_visible_child_name("content");
    }

    /// 空态（带可选的行动按钮）。
    pub fn show_empty(&self, icon: &str, title: &str, body: &str, action: Option<EmptyAction>) {
        while let Some(child) = self.empty_box.first_child() {
            self.empty_box.remove(&child);
        }
        let placeholder = ui::placeholder(icon, title, body);
        self.empty_box.append(&placeholder);
        if let Some((label, cb)) = action {
            let button = gtk::Button::with_label(label);
            button.add_css_class("pill");
            button.add_css_class("suggested-action");
            button.set_halign(gtk::Align::Center);
            button.set_margin_bottom(24);
            button.connect_clicked(move |_| cb());
            self.empty_box.append(&button);
        }
        self.spinner.stop();
        self.stack.set_visible_child_name("empty");
    }

    /// 错态。
    pub fn show_error(&self, error: &archstore_core::CoreError) {
        self.error.show_error(error);
        self.spinner.stop();
        self.stack.set_visible_child_name("error");
    }

    /// 错态（自定义文案）。
    pub fn show_error_text(&self, title: &str, detail: &str) {
        self.error.show_message(title, detail);
        self.spinner.stop();
        self.stack.set_visible_child_name("error");
    }

    pub fn error_view(&self) -> &ErrorView {
        &self.error
    }

    pub fn toast(&self, text: &str) {
        self.overlay.add_toast(adw::Toast::new(text));
    }

    pub fn toast_overlay(&self) -> &adw::ToastOverlay {
        &self.overlay
    }

    /// 当前可见状态名（测试用）。
    pub fn state_name(&self) -> String {
        self.stack
            .visible_child_name()
            .map(|s| s.to_string())
            .unwrap_or_default()
    }
}

/// 列表页：PageShell + GtkListView + 增量更新。
#[derive(Clone)]
pub struct ListPage {
    pub shell: PageShell,
    pub store: gio::ListStore,
    pub view: gtk::ListView,
    pub scroll: gtk::ScrolledWindow,
    /// 顶部细进度条（不阻塞输入，搜索/分页用）
    pub progress: gtk::ProgressBar,
}

impl ListPage {
    /// 构造一个列表页。header 会固定在列表上方。
    pub fn new(
        ctx: &Rc<RowContext>,
        header: Option<gtk::Widget>,
        on_activate: impl Fn(PackageSummary) + 'static,
        on_retry: Option<Box<dyn Fn() + 'static>>,
        on_settings: Option<Box<dyn Fn() + 'static>>,
    ) -> Self {
        let shell = PageShell::new(on_retry, on_settings);
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let view = widgets::list_view(&store, ctx);
        let progress = ui::thin_progress();
        progress.set_visible(false);

        let scroll = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        // 必须用 ListView 的 activate 信号表示"用户点击了某一行"。
        //
        // 曾经用 SingleSelection::selected-notify 当点击信号，结果鼠标一划过就打开详情页：
        // GtkListView 的 single-click-activate 属性的语义是
        // "Activate rows on single click **and select them on hover**"，
        // 悬停本身就会改变 selected。selected 是"选中态"（悬停/键盘焦点都会变），
        // activate 才是"用户动作"。
        view.connect_activate({
            let store = store.clone();
            move |_, position| {
                if let Some(summary) = widgets::item_at(&store, position) {
                    on_activate(summary);
                }
            }
        });

        if let Some(header) = header {
            shell.content.append(&header);
        }
        shell.content.append(&progress);
        shell.content.append(&scroll);

        Self {
            shell,
            store,
            view,
            scroll,
            progress,
        }
    }

    /// 用新数据做增量更新，并根据是否有数据显示空态。
    pub fn set_items(&self, items: &[PackageSummary], empty: EmptyState) {
        widgets::diff_update(&self.store, items);
        if items.is_empty() {
            self.shell
                .show_empty(empty.icon, &empty.title, &empty.body, empty.action);
        } else {
            self.shell.show_content();
        }
    }

    pub fn items(&self) -> Vec<PackageSummary> {
        (0..self.store.n_items())
            .filter_map(|i| widgets::item_at(&self.store, i))
            .collect()
    }
}

impl std::fmt::Debug for ListPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListPage").finish_non_exhaustive()
    }
}

/// 空态描述。
pub struct EmptyState {
    pub icon: &'static str,
    pub title: String,
    pub body: String,
    pub action: Option<EmptyAction>,
}

impl EmptyState {
    pub fn new(icon: &'static str, title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            icon,
            title: title.into(),
            body: body.into(),
            action: None,
        }
    }

    pub fn with_action(mut self, label: &'static str, cb: Box<dyn Fn() + 'static>) -> Self {
        self.action = Some((label, cb));
        self
    }
}

/// 页面标题（AdwToolbarView 的标题控件）。
pub fn page_title(text: &str) -> adw::WindowTitle {
    adw::WindowTitle::new(text, "")
}

/// 把列表项按来源过滤。
pub fn filter_by_source(
    items: &[PackageSummary],
    filter: archstore_core::backend::SourceFilter,
) -> Vec<PackageSummary> {
    items
        .iter()
        .filter(|s| filter.enabled(s.id.kind()))
        .cloned()
        .collect()
}

/// 在本地按名称/描述过滤（不联网）。
pub fn filter_local(items: &[PackageSummary], query: &str) -> Vec<PackageSummary> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return items.to_vec();
    }
    items
        .iter()
        .filter(|s| {
            s.id.name.to_lowercase().contains(&q)
                || s.display_name.to_lowercase().contains(&q)
                || s.summary.to_lowercase().contains(&q)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use archstore_core::model::{IconRef, PackageId};

    fn items() -> Vec<PackageSummary> {
        let mut a = PackageSummary::minimal(PackageId::official("extra", "firefox"), "Firefox");
        a.summary = "浏览器".into();
        a.icon = IconRef::Missing;
        let mut b = PackageSummary::minimal(PackageId::aur("yay"), "yay");
        b.summary = "AUR helper".into();
        b.icon = IconRef::Missing;
        let mut c = PackageSummary::minimal(PackageId::flatpak("flathub", "org.x.Y"), "Y");
        c.summary = "Flatpak app".into();
        c.icon = IconRef::Missing;
        vec![a, b, c]
    }

    #[test]
    fn local_filter_matches_name_display_and_summary() {
        let all = items();
        assert_eq!(filter_local(&all, "firefox").len(), 1);
        assert_eq!(filter_local(&all, "Firefox").len(), 1);
        assert_eq!(filter_local(&all, "浏览器").len(), 1);
        assert_eq!(filter_local(&all, "helper").len(), 1);
        assert_eq!(filter_local(&all, "").len(), 3);
        assert_eq!(filter_local(&all, "no-such-thing").len(), 0);
    }

    #[test]
    fn source_filter_respects_toggles() {
        let all = items();
        let f = archstore_core::backend::SourceFilter {
            pacman: true,
            aur: false,
            flatpak: false,
        };
        assert_eq!(filter_by_source(&all, f).len(), 1);
        let f = archstore_core::backend::SourceFilter::default();
        assert_eq!(filter_by_source(&all, f).len(), 3);
    }

    #[test]
    fn empty_state_builder() {
        let e = EmptyState::new("edit-find-symbolic", "标题", "正文");
        assert_eq!(e.title, "标题");
        assert!(e.action.is_none());
        let e = e.with_action("按钮", Box::new(|| {}));
        assert_eq!(e.action.as_ref().map(|(l, _)| *l), Some("按钮"));
    }
}
