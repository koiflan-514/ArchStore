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
    Flatpak,
}

impl InstalledFilter {
    pub fn label(&self) -> String {
        match self {
            InstalledFilter::All => ui::t("全部"),
            InstalledFilter::Explicit => ui::t("显式安装"),
            InstalledFilter::Dependency => ui::t("依赖"),
            InstalledFilter::Upgradable => ui::t("可更新"),
            InstalledFilter::Foreign => ui::t("外来包（可能来自 AUR）"),
            InstalledFilter::Flatpak => ui::t("Flatpak 应用"),
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
            InstalledFilter::Flatpak => {
                matches!(
                    s.id.source,
                    archstore_core::model::PackageSource::Flatpak { .. }
                )
            }
        }
    }
}

/// 合并"已安装"页的多个来源（官方仓库 + Flatpak 等），按包名排序。
///
/// 去重键是 **(来源, 名字)**：Flatpak 应用 ID 与 pacman 包名属于两个命名空间，
/// 恰好同名并不代表同一个软件，因此绝不能靠包名互相顶掉。
/// 传进来的顺序决定同键条目的取舍（前面的优先）。
pub fn merge_installed_sources(sources: Vec<Vec<PackageSummary>>) -> Vec<PackageSummary> {
    let mut seen: std::collections::HashSet<archstore_core::model::PackageId> =
        std::collections::HashSet::new();
    let mut out: Vec<PackageSummary> = Vec::new();
    for source in sources {
        for item in source {
            if seen.insert(item.id.clone()) {
                out.push(item);
            }
        }
    }
    out.sort_by(|a, b| a.id.name.cmp(&b.id.name));
    out
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
        // 本地过滤（毫秒级），不需要 GTK 自带的 150 ms search-changed 延迟
        search.set_search_delay(0);
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
            &InstalledFilter::Flatpak.label(),
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

    /// 由下拉框下标设置筛选条件，并**立即**重新过滤。
    ///
    /// 旧实现只改筛选状态、不重新应用，于是"选了分组但列表没变"，
    /// 要等下一次输入搜索框才生效（用户实测反馈：筛选没有实时更新）。
    pub fn set_filter_index(&self, index: u32) {
        let filter = match index {
            1 => InstalledFilter::Explicit,
            2 => InstalledFilter::Dependency,
            3 => InstalledFilter::Upgradable,
            4 => InstalledFilter::Foreign,
            5 => InstalledFilter::Flatpak,
            _ => InstalledFilter::All,
        };
        *self.filter.borrow_mut() = filter;
        self.apply(&self.search.text());
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

#[cfg(test)]
mod tests {
    use super::*;
    use archstore_core::model::{Installed, PackageId};

    fn installed_summary(
        source: archstore_core::model::PackageSource,
        name: &str,
    ) -> PackageSummary {
        let mut s = PackageSummary::minimal(
            PackageId {
                source,
                name: name.into(),
            },
            name,
        );
        s.installed = Installed::Yes {
            version: "1.0".into(),
            explicit: true,
        };
        s
    }

    #[test]
    fn merge_keeps_flatpak_apps_next_to_repo_packages() {
        let pacman = vec![installed_summary(
            archstore_core::model::PackageSource::Official {
                repo: "extra".into(),
            },
            "firefox",
        )];
        let flatpak = vec![installed_summary(
            archstore_core::model::PackageSource::Flatpak {
                remote: "flathub".into(),
            },
            "org.mozilla.firefox",
        )];
        let merged = merge_installed_sources(vec![pacman, flatpak]);
        assert_eq!(merged.len(), 2, "Flatpak 项不能落下");
        assert_eq!(merged[0].id.name, "firefox");
        assert_eq!(merged[1].id.name, "org.mozilla.firefox");
        assert!(merged.iter().any(|s| InstalledFilter::Flatpak.matches(s)));
    }

    #[test]
    fn merge_dedupes_within_the_same_source() {
        let dup = installed_summary(
            archstore_core::model::PackageSource::Official {
                repo: "extra".into(),
            },
            "vim",
        );
        let merged = merge_installed_sources(vec![vec![dup.clone(), dup]]);
        assert_eq!(merged.len(), 1, "同一来源里的重复条目必须去掉");
    }

    #[test]
    fn flatpak_filter_matches_only_flatpak() {
        let fp = installed_summary(
            archstore_core::model::PackageSource::Flatpak {
                remote: "flathub".into(),
            },
            "org.gnome.Calculator",
        );
        let official = installed_summary(
            archstore_core::model::PackageSource::Official {
                repo: "extra".into(),
            },
            "gnome-calculator",
        );
        assert!(InstalledFilter::Flatpak.matches(&fp));
        assert!(!InstalledFilter::Flatpak.matches(&official));
    }
}
