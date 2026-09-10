//! 更新页（§9.2）：官方仓库 / AUR / Flatpak 分组 + 安全公告 + 一键更新。

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;

use archstore_core::flathub::Advisory;
use archstore_core::model::{PackageSource, PackageSummary};

use crate::pages::{EmptyState, ListPage, filter_local};
use crate::state::matched_advisories;
use crate::ui;
use crate::widgets::RowContext;

/// 更新页。
#[derive(Debug)]
pub struct UpdatesPage {
    pub page: ListPage,
    all: RefCell<Vec<PackageSummary>>,
    advisories: RefCell<Vec<Advisory>>,
    security: gtk::Label,
    banner: gtk::Box,
    group: RefCell<UpdateGroup>,
    group_dropdown: gtk::DropDown,
    update_all: gtk::Button,
}

/// 更新分组。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateGroup {
    All,
    Official,
    Aur,
    Flatpak,
}

impl UpdateGroup {
    pub fn label(&self) -> String {
        match self {
            UpdateGroup::All => ui::t("全部"),
            UpdateGroup::Official => ui::t("官方仓库更新"),
            UpdateGroup::Aur => ui::t("AUR 更新（需要助手）"),
            UpdateGroup::Flatpak => ui::t("Flatpak 更新"),
        }
    }

    pub fn matches(&self, s: &PackageSummary) -> bool {
        match self {
            UpdateGroup::All => true,
            UpdateGroup::Official => matches!(s.id.source, PackageSource::Official { .. }),
            UpdateGroup::Aur => matches!(s.id.source, PackageSource::Aur),
            UpdateGroup::Flatpak => matches!(s.id.source, PackageSource::Flatpak { .. }),
        }
    }
}

impl UpdatesPage {
    pub fn new(
        rows: &Rc<RowContext>,
        on_open: impl Fn(PackageSummary) + 'static,
        on_retry: Box<dyn Fn() + 'static>,
        on_settings: Box<dyn Fn() + 'static>,
    ) -> Self {
        let security = ui::label("");
        security.add_css_class("dim-label");
        security.set_margin_start(12);
        security.set_margin_end(12);

        let banner = gtk::Box::new(gtk::Orientation::Vertical, 4);
        banner.set_margin_start(12);
        banner.set_margin_end(12);
        banner.set_margin_top(8);
        banner.set_visible(false);

        // 分组展示：安全更新 / 普通更新 / AUR 更新 / Flatpak 更新（§9.2）
        let group_dropdown = gtk::DropDown::from_strings(&[
            &UpdateGroup::All.label(),
            &UpdateGroup::Official.label(),
            &UpdateGroup::Aur.label(),
            &UpdateGroup::Flatpak.label(),
        ]);
        group_dropdown.set_margin_start(12);
        group_dropdown.set_margin_end(12);

        let update_all = gtk::Button::with_label(&ui::t("一键更新"));
        update_all.add_css_class("suggested-action");
        update_all.add_css_class("pill");
        update_all.set_halign(gtk::Align::Start);
        update_all.set_margin_start(12);
        update_all.set_margin_bottom(6);
        update_all.set_sensitive(false);

        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        controls.append(&group_dropdown);
        controls.append(&update_all);

        let header = gtk::Box::new(gtk::Orientation::Vertical, 6);
        header.append(&banner);
        header.append(&security);
        header.append(&controls);

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
            advisories: RefCell::new(Vec::new()),
            security,
            banner,
            group: RefCell::new(UpdateGroup::All),
            group_dropdown,
            update_all,
        }
    }

    /// 分组下拉框（由 window 接线）。
    pub fn group_dropdown(&self) -> &gtk::DropDown {
        &self.group_dropdown
    }

    /// 一键更新按钮（把全部可更新项加入计划，仍走审查栏，不直接执行）。
    pub fn update_all_button(&self) -> &gtk::Button {
        &self.update_all
    }

    /// 由下拉框下标设置分组。
    pub fn set_group_index(&self, index: u32) {
        let group = match index {
            1 => UpdateGroup::Official,
            2 => UpdateGroup::Aur,
            3 => UpdateGroup::Flatpak,
            _ => UpdateGroup::All,
        };
        *self.group.borrow_mut() = group;
        self.apply();
    }

    /// 设置可更新列表与安全公告。
    pub fn set_items(&self, items: &[PackageSummary], advisories: Vec<Advisory>) {
        *self.all.borrow_mut() = items.to_vec();
        *self.advisories.borrow_mut() = advisories;
        self.refresh_security_banner();
        self.apply();
    }

    /// 切换分组。
    pub fn set_group(&self, group: UpdateGroup) {
        *self.group.borrow_mut() = group;
        self.apply();
    }

    fn apply(&self) {
        let all = self.all.borrow();
        let group = *self.group.borrow();
        let visible: Vec<PackageSummary> =
            all.iter().filter(|s| group.matches(s)).cloned().collect();
        let visible = filter_local(&visible, "");
        self.update_all.set_sensitive(!visible.is_empty());
        // 同样是两种"空"：真的没有更新 vs 当前分组下没有更新
        let empty = if all.is_empty() {
            EmptyState::new(
                "emblem-ok-symbolic",
                ui::t("系统已是最新"),
                ui::t("没有可用的更新。"),
            )
        } else {
            EmptyState::new(
                "emblem-ok-symbolic",
                ui::t("该分组下没有更新"),
                format!(
                    "{}：{} / {}。{}",
                    ui::t("当前分组"),
                    visible.len(),
                    all.len(),
                    ui::t("切回「全部」可以看到其它来源的更新。")
                ),
            )
        };
        self.page.set_items(&visible, empty);
    }

    /// 安全公告横幅：仅在更新页可见时拉取一次并缓存 6 小时。
    fn refresh_security_banner(&self) {
        let advisories = self.advisories.borrow();
        let all = self.all.borrow();
        let mut hits: Vec<(String, String, String)> = Vec::new();
        for s in all.iter() {
            let Some(u) = &s.update else {
                continue;
            };
            for a in matched_advisories(&advisories, &s.id.name, &u.current) {
                hits.push((s.id.name.clone(), a.name.clone(), a.severity.clone()));
            }
        }
        while let Some(child) = self.banner.first_child() {
            self.banner.remove(&child);
        }
        if hits.is_empty() {
            self.banner.set_visible(false);
            self.security.set_label(&if advisories.is_empty() {
                ui::t("安全公告：不可用（将自动重试）")
            } else {
                format!(
                    "{}：{}",
                    ui::t("安全公告"),
                    ui::t("没有匹配到受影响的安全问题")
                )
            });
            return;
        }
        self.banner.set_visible(true);
        let title = ui::label(&format!("⚠ {}：{}", ui::t("安全更新"), hits.len()));
        title.add_css_class("plan-warning");
        self.banner.append(&title);
        for (pkg, advisory, severity) in hits.iter().take(6) {
            self.banner.append(&ui::ellipsized(&format!(
                "· {pkg} — {advisory}（{severity}）"
            )));
        }
        self.security.set_label(&format!(
            "{}：{} 条公告",
            ui::t("安全公告"),
            advisories.len()
        ));
    }

    pub fn security_text(&self) -> String {
        self.security.text().to_string()
    }

    pub fn updatable_count(&self) -> usize {
        self.all.borrow().len()
    }
}
