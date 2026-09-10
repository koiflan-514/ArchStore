//! 依赖列表（§8.2）：可展开 + 可选依赖勾选 + 虚拟依赖选择。
//!
//! 硬性规则：
//! 1. 可选依赖默认不勾选，勾选后其体积计入总计。
//! 2. 构建依赖用不同样式标注，并提供"构建后自动清理"开关（仅在该助手支持时展示）。
//! 3. 虚拟依赖必须让用户选择；选择结果写入计划。
//! 4. 弹窗是只读展示 + 选择，它不执行任何操作。
//! 5. 依赖无法解析完整时明确显示"依赖信息不完整"。

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;

use archstore_core::model::{DependencyInfo, PackageSummary, human_size};

use crate::state::DepGroups;
use crate::ui;

/// 用户在依赖列表中做出的选择。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DepSelection {
    /// 勾选的可选依赖
    pub optional: Vec<String>,
    /// 虚拟依赖的选择：依赖名 -> 选中的提供者
    pub virtual_choices: Vec<(String, String)>,
    /// 是否在构建后自动清理（仅 yay 等支持的助手）
    pub clean_after_build: bool,
}

/// 依赖列表控件。
#[derive(Debug, Clone)]
pub struct DepList {
    pub root: gtk::Box,
    state: Rc<RefCell<DepSelection>>,
    total: gtk::Label,
    incomplete: gtk::Label,
    clean_switch: Option<gtk::Switch>,
}

impl DepList {
    /// 构造依赖列表。
    ///
    /// packages 用于展示"哪些依赖已经安装"，supports_clean_after 决定是否展示清理开关。
    pub fn new(
        deps: &[DependencyInfo],
        _packages: &[PackageSummary],
        supports_clean_after: bool,
    ) -> Self {
        let groups = DepGroups::from_deps(deps);
        let state = Rc::new(RefCell::new(DepSelection::default()));

        let root = gtk::Box::new(gtk::Orientation::Vertical, 8);

        let total = gtk::Label::builder().xalign(0.0).build();
        total.add_css_class("dep-total");

        let incomplete = gtk::Label::builder().xalign(0.0).wrap(true).build();
        incomplete.add_css_class("dep-missing");
        incomplete.set_visible(groups.has_unknown());
        incomplete.set_label(&ui::t(
            "依赖信息不完整：部分依赖无法解析（可能未收录或网络不可用）。",
        ));

        if groups.total() == 0 {
            root.append(&ui::label(&ui::t("没有需要处理的依赖")));
            root.append(&total);
            root.append(&incomplete);
            return Self {
                root,
                state,
                total,
                incomplete,
                clean_switch: None,
            };
        }

        // 运行时依赖
        if !groups.runtime.is_empty() {
            root.append(&section(
                &format!("{}（{}）", ui::t("运行时依赖"), groups.runtime.len()),
                &groups.runtime,
            ));
        }
        // 构建依赖（仅构建期）
        if !groups.build.is_empty() {
            let note = ui::label(&ui::t("仅构建期需要，构建后可移除"));
            note.add_css_class("dep-build");
            root.append(&section(
                &format!("{}（{}）", ui::t("构建依赖"), groups.build.len()),
                &groups.build,
            ));
            root.append(&note);
        }
        // Flatpak 运行时与扩展
        if !groups.flatpak.is_empty() {
            root.append(&section(
                &format!("{}（{}）", ui::t("运行时与扩展"), groups.flatpak.len()),
                &groups.flatpak,
            ));
        }
        // 可选依赖（默认不勾选）
        if !groups.optional.is_empty() {
            root.append(&ui::label(&format!(
                "{}（{}）",
                ui::t("可选依赖"),
                groups.optional.len()
            )));
            for dep in &groups.optional {
                let cb = gtk::CheckButton::new();
                cb.set_active(false);
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                row.append(&cb);
                let label = ui::ellipsized(&match &dep.description {
                    Some(d) => format!("{} — {d}", dep.name),
                    None => dep.name.clone(),
                });
                row.append(&label);
                if let Some(size) = dep.size {
                    let size_label = gtk::Label::builder()
                        .label(human_size(size))
                        .xalign(1.0)
                        .hexpand(true)
                        .build();
                    size_label.add_css_class("dim-label");
                    row.append(&size_label);
                }
                let state_ref = state.clone();
                let total_ref = total.clone();
                let groups_ref = groups.clone();
                let dep_name = dep.name.clone();
                cb.connect_toggled(move |b| {
                    let mut s = state_ref.borrow_mut();
                    if b.is_active() {
                        if !s.optional.contains(&dep_name) {
                            s.optional.push(dep_name.clone());
                        }
                    } else {
                        s.optional.retain(|n| n != &dep_name);
                    }
                    let size = groups_ref.download_size(&s.optional);
                    total_ref.set_label(&format!("{}：{}", ui::t("需要下载"), human_size(size)));
                });
                root.append(&row);
            }
        }

        // 虚拟依赖：名字与满足者不同（通过 provides 满足）时必须让用户选择
        for dep in groups.runtime.iter().chain(groups.build.iter()) {
            let Some(provider) = &dep.satisfied_by else {
                continue;
            };
            if provider.name == dep.name {
                continue;
            }
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let label = ui::ellipsized(&format!(
                "{} — {}：{}",
                ui::t("虚拟依赖"),
                dep.name,
                provider.name
            ));
            row.append(&label);
            let choice = gtk::DropDown::from_strings(&[
                &format!("{}（{}）", provider.name, ui::t("推荐")),
                &ui::t("仅安装主包"),
            ]);
            choice.set_selected(0);
            row.append(&choice);
            let state_ref = state.clone();
            let dep_name = dep.name.clone();
            let provider_name = provider.name.clone();
            choice.connect_selected_notify(move |d| {
                let mut s = state_ref.borrow_mut();
                s.virtual_choices.retain(|(n, _)| n != &dep_name);
                if d.selected() == 0 {
                    s.virtual_choices
                        .push((dep_name.clone(), provider_name.clone()));
                }
            });
            // 默认选中推荐提供者
            state
                .borrow_mut()
                .virtual_choices
                .push((dep.name.clone(), provider.name.clone()));
            root.append(&row);
        }

        // 构建后自动清理（仅在该助手支持时展示）
        let clean_switch = if supports_clean_after && !groups.build.is_empty() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let label = ui::label(&ui::t("构建后自动清理构建依赖"));
            label.set_hexpand(true);
            let sw = gtk::Switch::new();
            sw.set_active(false);
            let state_ref = state.clone();
            sw.connect_active_notify(move |s| {
                state_ref.borrow_mut().clean_after_build = s.is_active();
            });
            row.append(&label);
            row.append(&sw);
            root.append(&row);
            Some(sw)
        } else {
            None
        };

        total.set_label(&format!(
            "{}：{}",
            ui::t("需要下载"),
            human_size(groups.download_size(&[]))
        ));
        root.append(&total);
        root.append(&incomplete);

        Self {
            root,
            state,
            total,
            incomplete,
            clean_switch,
        }
    }

    /// 当前选择。
    pub fn selection(&self) -> DepSelection {
        self.state.borrow().clone()
    }

    /// 小计文案（测试与展示用）。
    pub fn total_text(&self) -> String {
        self.total.text().to_string()
    }

    pub fn is_incomplete(&self) -> bool {
        self.incomplete.get_visible()
    }

    pub fn clean_switch(&self) -> Option<&gtk::Switch> {
        self.clean_switch.as_ref()
    }
}

/// 一个分组的标题 + 成员行。
fn section(title: &str, deps: &[DependencyInfo]) -> gtk::Widget {
    let box_ = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let heading = gtk::Label::builder().label(title).xalign(0.0).build();
    heading.add_css_class("heading");
    box_.append(&heading);
    for dep in deps {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let text = match &dep.description {
            Some(d) => format!("{} — {d}", dep.name),
            None => dep.name.clone(),
        };
        let label = ui::ellipsized(&text);
        label.set_hexpand(true);
        if dep.kind.is_build_only() {
            label.add_css_class("dep-build");
        }
        row.append(&label);

        let status = if dep.missing {
            let l = gtk::Label::new(Some(&ui::t("将安装")));
            l.add_css_class("dep-missing");
            l
        } else {
            let l = gtk::Label::new(Some(&ui::t("已安装")));
            l.add_css_class("dim-label");
            l
        };
        row.append(&status);

        if let Some(size) = dep.size
            && dep.missing
        {
            let size_label = gtk::Label::builder()
                .label(human_size(size))
                .xalign(1.0)
                .build();
            size_label.add_css_class("dim-label");
            row.append(&size_label);
        }
        box_.append(&row);
    }
    box_.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;
    use archstore_core::model::{DepKind, DependencyInfo, PackageId};

    fn deps() -> Vec<DependencyInfo> {
        let mut satisfied = DependencyInfo::from_expr("glibc", DepKind::Runtime);
        satisfied.missing = false;
        satisfied.satisfied_by = Some(PackageId::official("core", "glibc"));
        let mut virtual_dep = DependencyInfo::from_expr("libgl", DepKind::Runtime);
        virtual_dep.missing = false;
        virtual_dep.satisfied_by = Some(PackageId::official("extra", "mesa"));
        let mut opt = DependencyInfo::from_expr("ffmpeg: 视频解码", DepKind::Optional);
        opt.missing = true;
        opt.size = Some(10 * 1024 * 1024);
        let mut make = DependencyInfo::from_expr("go", DepKind::Make);
        make.missing = true;
        make.size = Some(100 * 1024 * 1024);
        vec![satisfied, virtual_dep, opt, make]
    }

    #[test]
    fn groups_split_correctly() {
        let g = DepGroups::from_deps(&deps());
        assert_eq!(g.runtime.len(), 2);
        assert_eq!(g.build.len(), 1);
        assert_eq!(g.optional.len(), 1);
        assert_eq!(g.total(), 4);
    }

    #[test]
    fn optional_deps_are_excluded_by_default() {
        let g = DepGroups::from_deps(&deps());
        let default_size = g.download_size(&[]);
        let with_optional = g.download_size(&["ffmpeg".to_string()]);
        assert!(with_optional > default_size, "勾选可选依赖后体积必须计入");
        assert_eq!(default_size, 100 * 1024 * 1024, "只有构建依赖缺失");
    }

    #[test]
    fn default_selection_is_empty() {
        let s = DepSelection::default();
        assert!(s.optional.is_empty());
        assert!(!s.clean_after_build);
    }
}
