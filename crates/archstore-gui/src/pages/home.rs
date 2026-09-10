//! 首页：Flathub 趋势 / 热门推荐（§7.3 三态：骨架屏 -> 内容 / 空 / 错）。

use std::rc::Rc;

use gtk::prelude::*;

use archstore_core::model::PackageSummary;

use crate::pages::{EmptyState, ListPage};
use crate::ui;
use crate::widgets::RowContext;

/// 首页。
#[derive(Debug)]
pub struct HomePage {
    pub page: ListPage,
    /// 数据来源说明（"来自缓存（X 分钟前）"或"正在请求 Flathub…"）
    status: gtk::Label,
}

impl HomePage {
    pub fn new(
        rows: &Rc<RowContext>,
        on_open: impl Fn(PackageSummary) + 'static,
        on_retry: Box<dyn Fn() + 'static>,
        on_settings: Box<dyn Fn() + 'static>,
    ) -> Self {
        let status = ui::label(&ui::t("正在请求 Flathub…"));
        status.add_css_class("dim-label");
        status.set_margin_top(8);
        status.set_margin_start(12);
        status.set_margin_end(12);

        let header = gtk::Box::new(gtk::Orientation::Vertical, 4);
        header.append(&status);
        let title = ui::label(&ui::t("推荐"));
        title.add_css_class("title-3");
        title.set_margin_start(12);
        header.append(&title);

        let page = ListPage::new(
            rows,
            Some(header.upcast()),
            on_open,
            Some(on_retry),
            Some(on_settings),
        );
        Self { page, status }
    }

    /// 更新数据与来源说明。
    pub fn set_items(&self, items: &[PackageSummary], source_note: &str) {
        self.status.set_label(source_note);
        self.page.set_items(
            items,
            EmptyState::new(
                "starred-symbolic",
                ui::t("暂无推荐内容"),
                ui::t("网络不可用或缓存为空。连接网络后点击重试。"),
            ),
        );
    }

    pub fn show_loading(&self) {
        self.page.shell.show_loading();
    }

    pub fn show_error(&self, error: &archstore_core::CoreError) {
        self.page.shell.show_error(error);
    }

    pub fn status_text(&self) -> String {
        self.status.text().to_string()
    }
}
