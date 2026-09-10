//! 已安装页（§9.1）：名称、版本、来源、安装大小、状态药丸；本地过滤，不联网。

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;

use archstore_core::model::{Installed, PackageSummary};

use crate::pages::{EmptyState, ListPage, filter_local};
use crate::ui;
use crate::widgets::RowContext;

/// 状态筛选。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstalledFilter {
    All,
    Explicit,
    Dependency,
    Upgradable,
    Foreign,
}

impl InstalledFilter {
    pub fn label(&self) -> String {
        match self {
            InstalledFilter::All => ui::t("全部"),
            InstalledFilter::Explicit => ui::t("显式安装"),
            InstalledFilter::Dependency => ui::t("依赖"),
            InstalledFilter::Upgradable => ui::t("可更新"),
            InstalledFilter::Foreign => ui::t("外来包（可能来自 AUR）"),
        }
    }

    pub fn matches(&self, s: &PackageSummary) -> bool {
        match self {
            InstalledFilter::All => true,
            InstalledFilter::Explicit => {
                matches!(s.installed, Installed::Yes { explicit: true, .. })
            }
            InstalledFilter::Dependency => s.installed.is_dependency(),
            InstalledFilter::Upgradable => s.has_update(),
            InstalledFilter::Foreign => {
                matches!(s.id.source, archstore_core::model::PackageSource::Aur)
            }
        }
    }
}

/// 已安装页。
#[derive(Debug)]
pub struct InstalledPage {
    pub page: ListPage,
    all: RefCell<Vec<PackageSummary>>,
    filter: RefCell<InstalledFilter>,
    search: gtk::SearchEntry,
    filter_dropdown: gtk::DropDown,
    count: gtk::Label,
}

impl InstalledPage {
    pub fn new(
        rows: &Rc<RowContext>,
        on_open: impl Fn(PackageSummary) + 'static,
        on_retry: Box<dyn Fn() + 'static>,
        on_settings: Box<dyn Fn() + 'static>,
    ) -> Self {
        let search = gtk::SearchEntry::new();
        search.set_placeholder_text(Some(&ui::t("在已安装的软件中筛选（不联网）")));
        search.set_margin_start(12);
        search.set_margin_end(12);
        search.set_margin_top(8);

        let count = ui::label("");
        count.add_css_class("dim-label");
        count.set_margin_start(12);
        count.set_margin_bottom(4);

        let filter_dropdown = gtk::DropDown::from_strings(&[
            &InstalledFilter::All.label(),
            &InstalledFilter::Explicit.label(),
            &InstalledFilter::Dependency.label(),
            &InstalledFilter::Upgradable.label(),
            &InstalledFilter::Foreign.label(),
        ]);
        filter_dropdown.set_margin_start(12);
        filter_dropdown.set_margin_end(12);

        let header = gtk::Box::new(gtk::Orientation::Vertical, 6);
        header.append(&search);
        header.append(&filter_dropdown);
        header.append(&count);

        let page = ListPage::new(
            rows,
            Some(header.upcast()),
            on_open,
            Some(on_retry),
            Some(on_settings),
        );

        Self {
            page,
            all: RefCell::new(Vec::new()),
            filter: RefCell::new(InstalledFilter::All),
            search: search.clone(),
            filter_dropdown: filter_dropdown.clone(),
            count,
        }
    }

    /// 让调用方接入筛选回调（避免在构造期形成自引用）。
    pub fn connect_filters(&self, on_change: impl Fn() + 'static) {
        let cb = Rc::new(on_change);
        let cb1 = cb.clone();
        self.search.connect_search_changed(move |_| cb1());
    }

    /// 选择筛选类型（由外部 DropDown 回调驱动）。
    pub fn set_filter(&self, filter: InstalledFilter, search_text: &str) {
        *self.filter.borrow_mut() = filter;
        self.apply(search_text);
    }

    /// 设置全量数据（一次读取后在内存建立索引，§9.1）。
    pub fn set_items(&self, items: &[PackageSummary]) {
        *self.all.borrow_mut() = items.to_vec();
        self.apply(&self.search.text());
    }

    fn apply(&self, search_text: &str) {
        let all = self.all.borrow();
        let filter = *self.filter.borrow();
        let filtered: Vec<PackageSummary> =
            all.iter().filter(|s| filter.matches(s)).cloned().collect();
        let visible = filter_local(&filtered, search_text);
        self.count.set_label(&format!(
            "{}：{} / {}",
            ui::t("显示"),
            visible.len(),
            all.len()
        ));
        // 两种"空"要分开说：数据本身为空（读不到本地库）vs 筛选/搜索没命中。
        // 后者不该让用户以为系统数据库坏了。
        let empty = if all.is_empty() {
            EmptyState::new(
                "dialog-warning-symbolic",
                ui::t("列表为空"),
                ui::t("若系统已有软件包却显示为空，说明无法读取本地数据库，请运行诊断。")
                    .to_string(),
            )
        } else {
            EmptyState::new(
                "edit-find-symbolic",
                ui::t("没有匹配的软件"),
                format!(
                    "{}：{} / {}。{}",
                    ui::t("筛选后没有匹配项"),
                    visible.len(),
                    all.len(),
                    ui::t("换个关键字，或把筛选切回「全部」。")
                ),
            )
        };
        self.page.set_items(&visible, empty);
    }

    pub fn search_entry(&self) -> &gtk::SearchEntry {
        &self.search
    }

    /// 状态筛选下拉框（由 window 接线；只做本地过滤，不联网）。
    pub fn filter_dropdown(&self) -> &gtk::DropDown {
        &self.filter_dropdown
    }

    /// 当前筛选条件。
    pub fn filter(&self) -> InstalledFilter {
        *self.filter.borrow()
    }

    /// 由下拉框下标设置筛选条件。
    pub fn set_filter_index(&self, index: u32) {
        let filter = match index {
            1 => InstalledFilter::Explicit,
            2 => InstalledFilter::Dependency,
            3 => InstalledFilter::Upgradable,
            4 => InstalledFilter::Foreign,
            _ => InstalledFilter::All,
        };
        *self.filter.borrow_mut() = filter;
    }

    /// 重新应用筛选（搜索框内容变化或数据刷新后调用）。
    pub fn refresh(&self) {
        self.apply(&self.search.text());
    }

    pub fn visible_count(&self) -> u32 {
        self.page.store.n_items()
    }

    pub fn total_count(&self) -> usize {
        self.all.borrow().len()
    }
}
