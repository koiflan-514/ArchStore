//! 分类页（§7.3）：pacman 包组 + AUR 关键词 + Flatpak 分类，分页 spinner 在列表尾部。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk::prelude::*;

use archstore_core::backend::{Category, Page};
use archstore_core::model::PackageSummary;

use crate::pages::{EmptyState, ListPage};
use crate::ui;
use crate::widgets::RowContext;

/// 每页条目数。
pub const PAGE_SIZE: usize = 50;

/// 分类页。
pub struct CategoryPage {
    pub page: ListPage,
    list: gtk::ListBox,
    footer: gtk::Box,
    spinner: gtk::Spinner,
    more: gtk::Button,
    // 注意：这几个字段必须用 Rc<RefCell<…>> 共享给信号回调。
    // 直接 clone 一个 RefCell 会复制内容，闭包写的是一份独立副本，
    // 导致分页状态永远读不到（UI 冒烟测试抓到的真实缺陷）。
    /// 当前显示的分类（可能被来源过滤缩小）
    categories: Rc<RefCell<Vec<Category>>>,
    /// 全部已知分类（过滤前的完整集合）
    all_categories: Rc<RefCell<Vec<Category>>>,
    /// 来源过滤：None = 全部，"aur" = 只显示 AUR 关键词，"flatpak" = 只显示 Flathub 分类
    source_filter: Rc<RefCell<Option<&'static str>>>,
    current: Rc<RefCell<Option<Category>>>,
    offset: Rc<RefCell<usize>>,
    on_select: Rc<RefCell<Option<CategorySelectCallback>>>,
    /// 三个"发现"导航项（分类 / AUR 社区 / Flatpak）共用同一套控件，
    /// 但**各自的当前分类独立保存**：切走再切回来时能看到自己上次选的那一类。
    sessions: Rc<RefCell<HashMap<&'static str, Category>>>,
    /// 当前活动的来源键（"category" / "aur" / "flatpak"）
    active: Rc<RefCell<&'static str>>,
}

/// 分类选择回调：category + 分页参数。
pub type CategorySelectCallback = Rc<dyn Fn(Category, Page)>;

impl CategoryPage {
    pub fn new(
        rows: &Rc<RowContext>,
        on_open: impl Fn(PackageSummary) + 'static,
        on_retry: Box<dyn Fn() + 'static>,
        on_settings: Box<dyn Fn() + 'static>,
    ) -> Self {
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.add_css_class("navigation-sidebar");
        list.set_margin_top(6);
        list.set_margin_bottom(6);

        let spinner = gtk::Spinner::new();
        let more = gtk::Button::with_label(&ui::t("加载更多"));
        more.add_css_class("pill");
        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        footer.set_halign(gtk::Align::Center);
        footer.set_margin_top(6);
        footer.set_margin_bottom(12);
        footer.append(&spinner);
        footer.append(&more);

        let page = ListPage::new(
            rows,
            Some(footer.clone().upcast()),
            on_open,
            Some(on_retry),
            Some(on_settings),
        );

        let this = Self {
            page,
            list,
            footer,
            spinner,
            more,
            categories: Rc::new(RefCell::new(Vec::new())),
            all_categories: Rc::new(RefCell::new(Vec::new())),
            source_filter: Rc::new(RefCell::new(None)),
            current: Rc::new(RefCell::new(None)),
            offset: Rc::new(RefCell::new(0)),
            on_select: Rc::new(RefCell::new(None)),
            sessions: Rc::new(RefCell::new(HashMap::new())),
            active: Rc::new(RefCell::new("category")),
        };
        this.set_footer_visible(false);
        this
    }

    /// 侧栏+内容区的分类列表控件（供 window 放进侧栏分组）。
    pub fn list_widget(&self) -> &gtk::ListBox {
        &self.list
    }

    /// 注册选择回调。
    pub fn connect_select(&self, cb: impl Fn(Category, Page) + 'static) {
        let cb = Rc::new(cb);
        *self.on_select.borrow_mut() = Some(cb.clone());
        let offset = Rc::clone(&self.offset);
        let current = Rc::clone(&self.current);
        let categories = Rc::clone(&self.categories);
        let page = self.page.clone();
        self.list.connect_row_activated(move |_, row| {
            let index = row.index();
            if index < 0 {
                return;
            }
            let Some(cat) = categories.borrow().get(index as usize).cloned() else {
                return;
            };
            *offset.borrow_mut() = 0;
            *current.borrow_mut() = Some(cat.clone());
            page.progress.set_visible(true);
            page.progress.pulse();
            cb(cat, Page::new(0, PAGE_SIZE));
        });
    }

    /// 更新分类列表（保存完整集合并按当前过滤条件渲染）。
    pub fn set_categories(&self, categories: &[Category]) {
        *self.all_categories.borrow_mut() = categories.to_vec();
        self.render();
    }

    /// 按来源过滤分类列表。
    ///
    /// "AUR 社区"与"Flatpak"两个导航项复用本页：GTK 中一个控件只能有一个父容器，
    /// 不能把同一个页面反复 add 到 ViewStack（会导致 GLib-GObject-CRITICAL）。
    pub fn set_source_filter(&self, filter: Option<&'static str>) {
        *self.source_filter.borrow_mut() = filter;
        self.render();
    }

    /// 切换"发现"里的来源导航，返回**该来源上次选中的分类**（没有则为 None）。
    ///
    /// 行为：
    /// 1. 把当前分类存进旧来源的会话；
    /// 2. 切换侧栏分类列表的来源过滤；
    /// 3. 恢复新来源上次选中的分类（并把侧栏对应行重新选中）。
    ///
    /// 调用方拿到返回值后要**重新拉取该分类的首屏**——用户实测反馈"来回切换要更新"，
    /// 所以切页不是简单把旧数据摆回去，而是重新请求（本地库毫秒级、AUR/Flathub 有缓存）。
    pub fn switch_source(
        &self,
        key: &'static str,
        filter: Option<&'static str>,
    ) -> Option<Category> {
        {
            let previous = *self.active.borrow();
            let mut sessions = self.sessions.borrow_mut();
            match self.current.borrow().clone() {
                Some(cat) => {
                    sessions.insert(previous, cat);
                }
                None => {
                    sessions.remove(previous);
                }
            }
        }
        *self.active.borrow_mut() = key;
        *self.source_filter.borrow_mut() = filter;
        // 先恢复新来源的选择再渲染：render() 会把对应行重新选中
        let restored = self.sessions.borrow().get(key).cloned();
        *self.current.borrow_mut() = restored.clone();
        *self.offset.borrow_mut() = 0;
        self.render();
        restored
    }

    /// 当前来源键（测试用）。
    pub fn active_source(&self) -> &'static str {
        *self.active.borrow()
    }

    /// 当前选中的分类。
    pub fn current_category(&self) -> Option<Category> {
        self.current.borrow().clone()
    }

    /// 没有选中分类时的提示态。
    ///
    /// 切换来源后如果这个来源还没选过分类，内容区必须**清空**并给出提示，
    /// 而不是继续显示上一个来源的软件列表。
    pub fn show_pick_hint(&self) {
        self.page.progress.set_visible(false);
        self.set_footer_visible(false);
        self.page.set_items(
            &[],
            EmptyState::new(
                "view-list-symbolic",
                ui::t("请从左侧选择一个分类"),
                ui::t("左侧列出的是当前来源的分类；点一个分类即可查看其中的软件。"),
            ),
        );
    }

    /// 当前来源过滤。
    pub fn source_filter(&self) -> Option<&'static str> {
        *self.source_filter.borrow()
    }

    /// 按过滤条件重建侧栏列表。
    fn render(&self) {
        let filter = *self.source_filter.borrow();
        let visible: Vec<Category> = self
            .all_categories
            .borrow()
            .iter()
            .filter(|c| filter.is_none_or(|f| c.source_kind == f))
            .cloned()
            .collect();

        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for c in &visible {
            let row = gtk::ListBoxRow::new();
            let bx = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            bx.set_margin_top(6);
            bx.set_margin_bottom(6);
            bx.set_margin_start(6);
            bx.set_margin_end(6);
            bx.append(&ui::ellipsized(&c.display));
            row.set_child(Some(&bx));
            row.set_tooltip_text(Some(&format!("{} · {}", c.source_kind, c.id)));
            self.list.append(&row);
        }
        *self.categories.borrow_mut() = visible;

        // 重新渲染后保持"当前分类"的选中态：
        // 刷新分类列表（load_categories）会重建所有行，不重新选中就会丢掉高亮，
        // 用户会以为"切回来之后什么都没选中"。
        if let Some(cat) = self.current.borrow().clone()
            && let Some(index) = self.categories.borrow().iter().position(|c| c.id == cat.id)
            && let Some(row) = self.list.row_at_index(index as i32)
        {
            self.list.select_row(Some(&row));
        }
    }

    /// 追加分页结果。
    pub fn append_page(&self, items: &[PackageSummary], has_more: bool) {
        self.page.progress.set_visible(false);
        self.spinner.stop();
        let mut all = self.page.items();
        all.extend_from_slice(items);
        *self.offset.borrow_mut() = all.len();
        let empty = EmptyState::new(
            "view-list-symbolic",
            ui::t("该分类暂无软件"),
            ui::t("换一个分类，或确认同步数据库是最新的。"),
        );
        self.page.set_items(&all, empty);
        self.set_footer_visible(has_more);
    }

    /// 设置首屏结果（替换）。
    pub fn set_items(&self, items: &[PackageSummary], has_more: bool) {
        self.page.progress.set_visible(false);
        self.spinner.stop();
        *self.offset.borrow_mut() = items.len();
        let empty = EmptyState::new(
            "view-list-symbolic",
            ui::t("该分类暂无软件"),
            ui::t("换一个分类，或确认同步数据库是最新的。"),
        );
        self.page.set_items(items, empty);
        self.set_footer_visible(has_more);
    }

    /// 请求下一页。
    pub fn next_page(&self) -> Option<(Category, Page)> {
        let cat = self.current.borrow().clone()?;
        let offset = *self.offset.borrow();
        Some((cat, Page::new(offset, PAGE_SIZE)))
    }

    pub fn more_button(&self) -> &gtk::Button {
        &self.more
    }

    pub fn spinner(&self) -> &gtk::Spinner {
        &self.spinner
    }

    /// 尾部 spinner 与"加载更多"的显示。
    pub fn set_footer_visible(&self, visible: bool) {
        self.footer.set_visible(visible);
        self.more.set_visible(visible);
        if !visible {
            self.spinner.stop();
        }
    }

    pub fn show_loading(&self) {
        self.page.progress.set_visible(true);
        self.page.progress.pulse();
    }

    pub fn show_error(&self, error: &archstore_core::CoreError) {
        self.page.progress.set_visible(false);
        self.page.shell.show_error(error);
    }

    pub fn category_count(&self) -> usize {
        self.categories.borrow().len()
    }
}
