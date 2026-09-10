//! 列表行渲染：数据行 + 独立渲染函数（**不是**自引用结构，见附录 D #11）。

use gtk::prelude::*;

use archstore_core::model::{PackageSummary, human_size};

use crate::icon_cache;
use crate::ui;
use crate::widgets::RowContext;

/// 渲染一行软件条目（§7.1）。
///
/// 返回的控件不持有 summary 的引用；点击由 ListView 的 single_click_activate +
/// ListItem 携带的 BoxedAnyObject 取回数据。
pub fn render(summary: &PackageSummary, ctx: &RowContext) -> gtk::Widget {
    let icon = icon_cache::image_for(&summary.icon, ctx.icon_size, ctx, &summary.id);
    icon.set_valign(gtk::Align::Center);

    // 第一行：名称 + 来源徽章 + 状态药丸
    let title_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let name = gtk::Label::builder()
        .label(&summary.display_name)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    name.add_css_class("package-name");
    title_row.append(&name);
    title_row.append(&ui::source_badge(&summary.id.source));
    if let Some(pill) = ui::state_pill(summary) {
        title_row.append(&pill);
    }

    // 第二行：描述
    let desc = ui::ellipsized(&summary.summary);
    desc.add_css_class("dim-label");
    desc.set_hexpand(true);

    let text_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text_box.set_hexpand(true);
    text_box.set_valign(gtk::Align::Center);
    text_box.append(&title_row);
    text_box.append(&desc);

    // 右侧：版本 / 下载体积
    let right = gtk::Box::new(gtk::Orientation::Vertical, 2);
    right.set_valign(gtk::Align::Center);
    let mut has_right = false;
    if let Some(v) = &summary.version {
        let version = gtk::Label::builder().label(v).xalign(1.0).build();
        version.add_css_class("dim-label");
        version.add_css_class("package-version");
        right.append(&version);
        has_right = true;
    }
    if let Some(u) = &summary.update
        && let Some(size) = u.download_size
        && size > 0
    {
        let size_label = gtk::Label::builder()
            .label(human_size(size))
            .xalign(1.0)
            .build();
        size_label.add_css_class("dim-label");
        right.append(&size_label);
        has_right = true;
    }
    right.set_visible(has_right);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.add_css_class("package-row");
    row.append(&icon);
    row.append(&text_box);
    row.append(&right);
    row.upcast()
}

/// 行高（供 ListView 预估，避免滚动抖动）。
pub fn row_height(ctx: &RowContext) -> i32 {
    ctx.icon_size.max(32) + 16
}

#[cfg(test)]
mod tests {
    use super::*;
    use archstore_core::model::{IconRef, Installed, PackageId, UpdateInfo};

    fn summary() -> PackageSummary {
        let mut s = PackageSummary::minimal(PackageId::official("extra", "firefox"), "Firefox");
        s.set_summary("Fast, Private & Safe Web Browser");
        s.version = Some("155.0.1-1".into());
        s.installed = Installed::Yes {
            version: "154.0-1".into(),
            explicit: true,
        };
        s.update = Some(UpdateInfo {
            current: "154.0-1".into(),
            candidate: "155.0.1-1".into(),
            download_size: Some(88 * 1024 * 1024),
        });
        s.icon = IconRef::IconName("firefox".into());
        s
    }

    #[test]
    fn row_height_has_a_readable_minimum() {
        assert_eq!(row_height(&RowContext::new(32)), 48);
        assert_eq!(row_height(&RowContext::new(16)), 48);
        assert_eq!(row_height(&RowContext::new(64)), 80);
    }

    #[test]
    fn summary_state_flags() {
        let s = summary();
        assert!(s.has_update());
        assert!(s.is_installed());
        assert!(s.summary.chars().count() <= 120);
    }
}
