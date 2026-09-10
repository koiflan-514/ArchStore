//! 详情页（§7.3 / §7.4）：摘要先显示（本地数据），详情字段逐块填充。

use std::rc::Rc;

use gtk::prelude::*;

use archstore_core::model::{PackageDetail, PackageSummary};

use crate::ui;
use crate::widgets::package_detail_view::{self, DetailCallbacks, DetailView};

/// 详情页。
#[derive(Debug)]
pub struct DetailPage {
    pub stack: gtk::Stack,
    pub root: gtk::Box,
    placeholder_box: gtk::Box,
    content_box: gtk::Box,
    title: libadwaita::WindowTitle,
    /// 当前渲染出来的详情视图（截图下载完成后需要用它回填）
    view: std::cell::RefCell<Option<DetailView>>,
}

impl DetailPage {
    pub fn new() -> Self {
        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .vexpand(true)
            .build();

        let placeholder_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        placeholder_box.append(&ui::placeholder(
            "system-software-install-symbolic",
            &ui::t("选择左侧的软件查看详情"),
            &ui::t("详情页会先显示本地数据，再逐块填充网络字段。"),
        ));

        let content_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let content_scroll = gtk::ScrolledWindow::builder()
            .child(&content_box)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        stack.add_named(&placeholder_box, Some("placeholder"));
        stack.add_named(&content_scroll, Some("content"));
        stack.set_visible_child_name("placeholder");

        let title = libadwaita::WindowTitle::new(&ui::t("详情"), "");

        // 必须包一层 AdwToolbarView + AdwHeaderBar：
        // AdwNavigationView 只会往"页面里的 HeaderBar"注入返回键，
        // 纯 GtkBox 的页面被 push 上去后是没有返回按钮的（用户实测反馈）。
        let header = libadwaita::HeaderBar::new();
        header.set_show_end_title_buttons(false);
        let toolbar = libadwaita::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&stack));

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&toolbar);

        Self {
            stack,
            root,
            placeholder_box,
            content_box,
            title,
            view: std::cell::RefCell::new(None),
        }
    }

    /// 摘要先显示（本地数据），随后由 set_detail 填充完整字段。
    pub fn show_summary(&self, summary: &PackageSummary) {
        self.title.set_title(&summary.display_name);
        self.title.set_subtitle(&summary.id.source.display());
        while let Some(child) = self.content_box.first_child() {
            self.content_box.remove(&child);
        }
        let loading = gtk::Box::new(gtk::Orientation::Vertical, 8);
        loading.set_margin_top(24);
        loading.set_halign(gtk::Align::Center);
        let spinner = gtk::Spinner::new();
        spinner.start();
        loading.append(&spinner);
        loading.append(&ui::label(&ui::t("正在补全详情…")));
        // 摘要部分立刻可见
        let head = gtk::Box::new(gtk::Orientation::Vertical, 4);
        head.set_margin_top(16);
        head.set_margin_start(18);
        head.set_margin_end(18);
        let name = ui::label(&summary.display_name);
        name.add_css_class("title-1");
        head.append(&name);
        head.append(&ui::label(&ui::subtitle(summary)));
        self.content_box.append(&head);
        self.content_box.append(&loading);
        self.stack.set_visible_child_name("content");
    }

    /// 填充完整详情。
    pub fn set_detail(
        &self,
        detail: &PackageDetail,
        icons: &Rc<crate::widgets::RowContext>,
        callbacks: DetailCallbacks,
    ) {
        while let Some(child) = self.content_box.first_child() {
            self.content_box.remove(&child);
        }
        self.title.set_title(&detail.summary.display_name);
        self.title.set_subtitle(&detail.summary.id.source.display());
        let view = DetailView::new(detail, icons, callbacks);
        self.content_box
            .append(&package_detail_view::scrollable(&view));
        *self.view.borrow_mut() = Some(view);
        // 详情视图自身是可滚动的；这里让外层不再重复滚动
        self.stack.set_visible_child_name("content");
    }

    /// 详情加载失败：仍然显示摘要 + 错误提示（不整页失败）。
    pub fn set_error(&self, summary: &PackageSummary, error: &archstore_core::CoreError) {
        self.show_summary(summary);
        let note = ui::label(&format!(
            "{}：{}",
            ui::t("部分详情不可用"),
            error.user_message()
        ));
        note.add_css_class("dim-label");
        note.set_margin_start(18);
        note.set_margin_end(18);
        self.content_box.append(&note);
    }

    pub fn clear(&self) {
        *self.view.borrow_mut() = None;
        self.stack.set_visible_child_name("placeholder");
    }

    /// 当前详情视图（截图下载回填用）。
    pub fn view(&self) -> Option<DetailView> {
        self.view.borrow().clone()
    }

    pub fn state_name(&self) -> String {
        self.stack
            .visible_child_name()
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    pub fn title_widget(&self) -> &libadwaita::WindowTitle {
        &self.title
    }
}

impl Default for DetailPage {
    fn default() -> Self {
        Self::new()
    }
}

/// 供 window 使用的占位：确保 Rc 包装的类型可用。
pub type SharedDetailPage = Rc<DetailPage>;
