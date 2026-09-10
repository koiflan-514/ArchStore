//! 错误视图（§7.3 错态）：原因 + 重试 + 打开设置。

use gtk::prelude::*;

use crate::ui;

/// 错误视图控件的句柄。
#[derive(Debug, Clone)]
pub struct ErrorView {
    pub root: gtk::Box,
    icon: gtk::Image,
    title: gtk::Label,
    detail: gtk::Label,
    retry: gtk::Button,
    settings: gtk::Button,
    copy: gtk::Button,
}

impl ErrorView {
    /// 构造错误视图。没有回调的动作会被隐藏，而不是显示成死按钮。
    pub fn new(
        on_retry: Option<Box<dyn Fn() + 'static>>,
        on_settings: Option<Box<dyn Fn() + 'static>>,
    ) -> Self {
        let icon = gtk::Image::from_icon_name("dialog-warning-symbolic");
        icon.set_pixel_size(64);
        icon.add_css_class("dim-label");

        let title = gtk::Label::builder()
            .label(ui::t("出错了"))
            .css_classes(["title-2"])
            .build();
        let detail = gtk::Label::builder()
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["dim-label"])
            .build();

        let retry = gtk::Button::with_label(&ui::t("重试"));
        retry.add_css_class("pill");
        retry.add_css_class("suggested-action");
        if let Some(cb) = on_retry {
            retry.connect_clicked(move |_| cb());
        } else {
            retry.set_visible(false);
        }

        let settings = gtk::Button::with_label(&ui::t("打开设置"));
        settings.add_css_class("pill");
        if let Some(cb) = on_settings {
            settings.connect_clicked(move |_| cb());
        } else {
            settings.set_visible(false);
        }

        let copy = gtk::Button::with_label(&ui::t("复制诊断信息"));
        copy.add_css_class("pill");
        {
            let detail_clone = detail.clone();
            let copy_clone = copy.clone();
            copy.connect_clicked(move |_| {
                let text = detail_clone.text().to_string();
                if archstore_core::aur_run::copy_to_clipboard(&text).is_ok() {
                    copy_clone.set_label(&ui::t("已复制"));
                } else {
                    copy_clone.set_label(&ui::t("复制失败"));
                }
            });
        }

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.set_halign(gtk::Align::Center);
        actions.append(&retry);
        actions.append(&settings);
        actions.append(&copy);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.set_valign(gtk::Align::Center);
        root.set_halign(gtk::Align::Center);
        root.set_margin_top(48);
        root.set_margin_bottom(48);
        root.set_margin_start(24);
        root.set_margin_end(24);
        root.append(&icon);
        root.append(&title);
        root.append(&detail);
        root.append(&actions);

        Self {
            root,
            icon,
            title,
            detail,
            retry,
            settings,
            copy,
        }
    }

    /// 展示一个 CoreError（含修复建议）。
    pub fn show_error(&self, error: &archstore_core::CoreError) {
        self.title
            .set_label(&format!("{}（{}）", ui::t("出错了"), error.category()));
        let mut text = error.user_message();
        if let Some(hint) = error.fix_hint() {
            text.push_str("\n\n");
            text.push_str(&hint);
        }
        self.detail.set_label(&text);
        self.retry.set_visible(true);
        self.settings.set_visible(true);
        self.copy.set_visible(true);
    }

    /// 展示自定义文案。
    pub fn show_message(&self, title: &str, detail: &str) {
        self.title.set_label(title);
        self.detail.set_label(detail);
    }

    pub fn set_retry_visible(&self, visible: bool) {
        self.retry.set_visible(visible);
    }

    pub fn set_settings_visible(&self, visible: bool) {
        self.settings.set_visible(visible);
    }

    /// 当前展示的详情文本（测试与复制用）。
    pub fn detail_text(&self) -> String {
        self.detail.text().to_string()
    }

    pub fn title_text(&self) -> String {
        self.title.text().to_string()
    }

    pub fn icon(&self) -> &gtk::Image {
        &self.icon
    }

    pub fn copy_button(&self) -> &gtk::Button {
        &self.copy
    }

    pub fn retry_button(&self) -> &gtk::Button {
        &self.retry
    }
}
