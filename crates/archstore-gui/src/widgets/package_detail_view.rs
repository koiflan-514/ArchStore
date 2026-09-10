//! 软件详情视图（§7.4）。
//!
//! 结构：
//! [图标 128] 名称 + 操作按钮
//!            版本 · 来源 · 大小
//!            许可证 · 维护者 · 主页
//! ────────────────
//! [截图横向滚动]
//! [描述（纯文本渲染，绝不做 markup 注入）]
//! [权限（仅 Flatpak）]
//! [依赖 (N)]
//! [详情] 键值对

use std::cell::RefCell;

use gtk::prelude::*;

use archstore_core::model::{PackageDetail, human_size};

use crate::icon_cache;
use crate::ui;
use crate::widgets::dep_list::DepList;

/// 详情视图的动作回调。
pub struct DetailCallbacks {
    /// 主操作：安装 / 更新 / 卸载
    pub on_primary: Box<dyn Fn()>,
    /// 卸载（已安装时展示）
    pub on_remove: Option<Box<dyn Fn()>>,
    /// 打开主页
    pub on_homepage: Option<Box<dyn Fn(String)>>,
    /// 依赖选择变化（用于更新计划）
    pub on_deps_changed: Option<Box<dyn Fn(crate::widgets::dep_list::DepSelection)>>,
}

/// 截图槽位上限（与 §7.4 的横向滚动区域一致）。
pub const MAX_SCREENSHOTS: usize = 8;

/// 描述区域：机器翻译异步到达后由这里回填。
#[derive(Debug, Clone)]
pub struct DescriptionArea {
    body: gtk::Label,
    notice: gtk::Label,
    toggle: gtk::ToggleButton,
    notice_row: gtk::Box,
    /// 上游原文（切换到"显示原文"时用）
    original: String,
    /// 已到达的译文
    translated: std::rc::Rc<RefCell<Option<String>>>,
}

impl DescriptionArea {
    /// 用译文替换描述，并显示"机器翻译（服务名）"标注与原文切换按钮。
    ///
    /// 硬性规则（§7.2）：译文必须标注来源，绝不冒充上游元数据。
    pub fn set_translation(&self, text: &str, label: &str, truncated: bool) {
        *self.translated.borrow_mut() = Some(text.to_string());
        self.body.set_text(text);
        let mut notice = label.to_string();
        if truncated {
            notice.push_str(" · ");
            notice.push_str(&crate::ui::t("仅翻译了前一部分"));
        }
        self.notice.set_label(&notice);
        self.notice_row.set_visible(true);
        self.notice.set_visible(true);
        self.toggle.set_visible(true);
        self.toggle.set_active(false);
        self.toggle.set_label(&crate::ui::t("显示原文"));
    }

    /// 当前显示的正文（测试用）。
    pub fn text(&self) -> String {
        self.body.text().to_string()
    }

    pub fn is_showing_translation(&self) -> bool {
        self.translated.borrow().is_some() && !self.toggle.is_active()
    }

    pub fn notice_text(&self) -> String {
        self.notice.text().to_string()
    }

    pub fn toggle(&self) -> &gtk::ToggleButton {
        &self.toggle
    }

    pub fn original(&self) -> &str {
        &self.original
    }
}

/// 详情视图的句柄。
#[derive(Debug, Clone)]
pub struct DetailView {
    pub root: gtk::Box,
    primary: gtk::Button,
    remove: Option<gtk::Button>,
    deps: std::rc::Rc<DepList>,
    screenshots: gtk::Box,
    description_area: Option<DescriptionArea>,
    /// 每个截图槽位对应的 Picture（按顺序），下载完成后由 set_screenshot 填入
    screenshot_slots: std::rc::Rc<RefCell<Vec<gtk::Picture>>>,
    /// 每个槽位的原始 URL（用于提示与测试）
    screenshot_urls: Vec<String>,
}

impl DetailView {
    /// 根据详情数据构造视图。
    pub fn new(
        detail: &PackageDetail,
        icons: &std::rc::Rc<crate::widgets::RowContext>,
        callbacks: DetailCallbacks,
    ) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 16);
        root.set_margin_top(16);
        root.set_margin_bottom(24);
        root.set_margin_start(18);
        root.set_margin_end(18);

        // --- 头部 ---
        // 与列表行同一条路径：远程图标下载完成后会原地回填这个控件
        let icon = icon_cache::image_for(&detail.summary.icon, 128, icons, &detail.summary.id);
        icon.set_valign(gtk::Align::Start);

        let title = gtk::Label::builder()
            .label(&detail.summary.display_name)
            .xalign(0.0)
            .wrap(true)
            .css_classes(["title-1"])
            .build();

        let mut meta_parts: Vec<String> = Vec::new();
        if let Some(v) = &detail.summary.version {
            meta_parts.push(v.clone());
        }
        meta_parts.push(detail.summary.id.source.display());
        let sizes = ui::size_text(detail.download_size, detail.installed_size);
        if !sizes.is_empty() {
            meta_parts.push(sizes);
        }
        let meta = ui::label(&meta_parts.join(" · "));
        meta.add_css_class("dim-label");

        let mut extra_parts: Vec<String> = Vec::new();
        if !detail.licenses.is_empty() {
            extra_parts.push(detail.licenses.join("、"));
        }
        if let Some(m) = &detail.maintainer {
            extra_parts.push(format!("{} {m}", ui::t("维护者")));
        }
        if let Some(r) = detail.rating {
            extra_parts.push(format!("★ {r:.1}（{}）", detail.review_count.unwrap_or(0)));
        }
        let extra = ui::label(&extra_parts.join(" · "));
        extra.add_css_class("dim-label");

        let homepage = gtk::Button::with_label(&ui::t("主页"));
        homepage.add_css_class("flat");
        match (&detail.homepage, callbacks.on_homepage) {
            (Some(url), Some(cb)) => {
                let url = url.clone();
                homepage.connect_clicked(move |_| cb(url.clone()));
            }
            _ => homepage.set_visible(false),
        }

        let mut primary_label = ui::t("安装");
        if detail.summary.is_installed() {
            primary_label = if detail.summary.has_update() {
                ui::t("更新")
            } else {
                ui::t("重新安装")
            };
        }
        let primary = gtk::Button::with_label(&primary_label);
        primary.add_css_class("suggested-action");
        primary.add_css_class("pill");
        {
            let cb = callbacks.on_primary;
            primary.connect_clicked(move |_| cb());
        }

        let remove = callbacks.on_remove.map(|cb| {
            let b = gtk::Button::with_label(&ui::t("卸载"));
            b.add_css_class("destructive-action");
            b.add_css_class("pill");
            b.set_visible(detail.summary.is_installed());
            b.connect_clicked(move |_| cb());
            b
        });

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_valign(gtk::Align::Start);
        buttons.append(&primary);
        if let Some(b) = &remove {
            buttons.append(b);
        }
        buttons.append(&homepage);

        let title_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        title_box.set_hexpand(true);
        title_box.append(&title);
        title_box.append(&meta);
        if !extra_parts.is_empty() {
            title_box.append(&extra);
        }
        title_box.append(&ui::label(&ui::t("操作前会先显示计划清单，确认后才执行。")));
        title_box.append(&buttons);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 18);
        header.append(&icon);
        header.append(&title_box);
        root.append(&header);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        // --- 截图 ---
        let screenshots = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let shot_scroll = gtk::ScrolledWindow::builder()
            .child(&screenshots)
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .min_content_height(180)
            .build();
        let mut screenshot_slots: Vec<gtk::Picture> = Vec::new();
        let screenshot_urls: Vec<String> = detail
            .screenshots
            .iter()
            .take(MAX_SCREENSHOTS)
            .cloned()
            .collect();
        if screenshot_urls.is_empty() {
            shot_scroll.set_visible(false);
        } else {
            for url in &screenshot_urls {
                // GtkPicture 没有"从 URL 异步加载"的 API（v0.1.0 的 set_from_file_async 不存在），
                // 因此先放占位，下载到本地缓存后再 set_file。
                let picture = gtk::Picture::new();
                picture.set_size_request(320, 180);
                picture.set_content_fit(gtk::ContentFit::Contain);
                picture.add_css_class("screenshot");
                picture.set_tooltip_text(Some(url));
                screenshots.append(&picture);
                screenshot_slots.push(picture);
            }
        }
        root.append(&shot_scroll);

        // --- 描述（纯文本：上游 HTML 已经转成文本，绝不做 markup 注入） ---
        let mut description_area = None;
        if !detail.description.is_empty() {
            let expander = gtk::Expander::builder()
                .label(ui::t("描述"))
                .expanded(true)
                .build();

            let body = ui::plain_text(&detail.description);
            body.set_margin_top(6);

            // 机器翻译的标注 + 原文/译文切换（默认隐藏，译文到达后才出现）
            let notice = ui::label("");
            notice.add_css_class("dim-label");
            notice.set_visible(false);
            let toggle = gtk::ToggleButton::with_label(&ui::t("显示原文"));
            toggle.add_css_class("flat");
            toggle.set_halign(gtk::Align::Start);
            toggle.set_visible(false);
            let notice_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            notice_row.append(&notice);
            notice_row.append(&toggle);
            notice_row.set_visible(false);

            let inner = gtk::Box::new(gtk::Orientation::Vertical, 4);
            inner.append(&body);
            inner.append(&notice_row);
            expander.set_child(Some(&inner));
            root.append(&expander);

            let area = DescriptionArea {
                body: body.clone(),
                notice: notice.clone(),
                toggle: toggle.clone(),
                notice_row: notice_row.clone(),
                original: detail.description.clone(),
                translated: std::rc::Rc::new(RefCell::new(None)),
            };
            // 切换按钮：在原文与译文之间来回
            {
                let area = area.clone();
                toggle.connect_toggled(move |b| {
                    if b.is_active() {
                        area.body.set_text(&area.original);
                        b.set_label(&ui::t("显示译文"));
                    } else {
                        let text = area
                            .translated
                            .borrow()
                            .clone()
                            .unwrap_or_else(|| area.original.clone());
                        area.body.set_text(&text);
                        b.set_label(&ui::t("显示原文"));
                    }
                });
            }
            description_area = Some(area);
        }

        // --- 权限（仅 Flatpak） ---
        if !detail.permissions.is_empty() {
            let expander = gtk::Expander::builder()
                .label(format!("{}（{}）", ui::t("权限"), detail.permissions.len()))
                .build();
            let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
            list.set_margin_top(6);
            for p in &detail.permissions {
                list.append(&ui::ellipsized(&format!("· {p}")));
            }
            expander.set_child(Some(&list));
            root.append(&expander);
        }

        // --- 依赖 ---
        let deps = std::rc::Rc::new(DepList::new(&detail.dependencies, &[], false));
        let dep_expander = gtk::Expander::builder()
            .label(format!(
                "{}（{}）",
                ui::t("依赖"),
                detail.dependencies.len()
            ))
            .expanded(!detail.dependencies.is_empty() && detail.dependencies.len() <= 12)
            .build();
        dep_expander.set_child(Some(&deps.root));
        root.append(&dep_expander);

        // --- 详情（键值对） ---
        if !detail.extra.is_empty() {
            let expander = gtk::Expander::builder().label(ui::t("详情")).build();
            let grid = gtk::Grid::builder()
                .row_spacing(4)
                .column_spacing(18)
                .margin_top(6)
                .build();
            for (i, (k, v)) in detail.extra.0.iter().enumerate() {
                let key = gtk::Label::builder()
                    .label(k)
                    .xalign(0.0)
                    .css_classes(["dim-label"])
                    .build();
                let value = gtk::Label::builder()
                    .label(v)
                    .xalign(0.0)
                    .wrap(true)
                    .selectable(true)
                    .build();
                grid.attach(&key, 0, i as i32, 1, 1);
                grid.attach(&value, 1, i as i32, 1, 1);
            }
            expander.set_child(Some(&grid));
            root.append(&expander);
        }

        // --- 原始键值兜底：缺失字段显示"不可用"而不是整页失败 ---
        if detail.summary.version.is_none() {
            let note = ui::label(&ui::t("版本信息不可用"));
            note.add_css_class("dim-label");
            root.append(&note);
        }

        Self {
            root,
            primary,
            remove,
            deps,
            screenshots,
            description_area,
            screenshot_slots: std::rc::Rc::new(RefCell::new(screenshot_slots)),
            screenshot_urls,
        }
    }

    pub fn primary_button(&self) -> &gtk::Button {
        &self.primary
    }

    pub fn remove_button(&self) -> Option<&gtk::Button> {
        self.remove.as_ref()
    }

    pub fn dep_list(&self) -> &DepList {
        &self.deps
    }

    pub fn screenshot_container(&self) -> &gtk::Box {
        &self.screenshots
    }

    /// 描述区域（异步翻译回填用）。
    pub fn description_area(&self) -> Option<&DescriptionArea> {
        self.description_area.as_ref()
    }

    /// 截图数量（已创建槽位的个数）。
    pub fn screenshot_count(&self) -> usize {
        self.screenshot_slots.borrow().len()
    }

    /// 截图槽位对应的原始 URL。
    pub fn screenshot_urls(&self) -> &[String] {
        &self.screenshot_urls
    }

    /// 把已下载到本地的截图填入对应槽位。
    ///
    /// 索引越界或文件不存在时静默忽略（截图是纯展示，缺失不应影响详情页）。
    pub fn set_screenshot(&self, index: usize, path: &std::path::Path) {
        let slots = self.screenshot_slots.borrow();
        let Some(picture) = slots.get(index) else {
            return;
        };
        if !path.is_file() {
            return;
        }
        picture.set_file(Some(&gio::File::for_path(path)));
        picture.set_tooltip_text(Some(&path.display().to_string()));
    }

    /// 某个槽位当前是否已经填充了本地文件。
    pub fn screenshot_loaded(&self, index: usize) -> bool {
        self.screenshot_slots
            .borrow()
            .get(index)
            .and_then(|p| p.file())
            .is_some()
    }
}

/// 把详情页包进一个可滚动容器。
pub fn scrollable(view: &DetailView) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .child(&view.root)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build()
}

/// 头部一行摘要文案（测试用：确保字段拼接稳定）。
pub fn header_summary(detail: &PackageDetail) -> String {
    let mut parts = vec![detail.summary.display_name.clone()];
    if let Some(v) = &detail.summary.version {
        parts.push(v.clone());
    }
    parts.push(detail.summary.id.source.display());
    let sizes = ui::size_text(detail.download_size, detail.installed_size);
    if !sizes.is_empty() {
        parts.push(sizes);
    }
    parts.push(human_size(detail.installed_size.unwrap_or(0)));
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use archstore_core::model::{PackageId, PackageSummary};

    fn detail() -> PackageDetail {
        let mut s = PackageSummary::minimal(PackageId::official("extra", "firefox"), "Firefox");
        s.version = Some("155.0.1-1".into());
        let mut d = PackageDetail::from_summary(s);
        d.download_size = Some(88 * 1024 * 1024);
        d.installed_size = Some(310 * 1024 * 1024);
        d.licenses = vec!["MPL-2.0".into()];
        d.maintainer = Some("Mozilla".into());
        d
    }

    #[test]
    fn header_summary_contains_key_fields() {
        let h = header_summary(&detail());
        assert!(h.contains("Firefox"));
        assert!(h.contains("155.0.1-1"));
        assert!(h.contains("extra"));
        assert!(h.contains("下载"));
    }

    #[test]
    fn detail_defaults_have_no_screenshots() {
        let d = detail();
        assert!(d.screenshots.is_empty());
        assert!(d.permissions.is_empty());
    }
}
