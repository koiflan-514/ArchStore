//! 进度面板（§5.5）：总体进度条 + 当前包 + 可展开的原始日志（环形缓冲 5000 行）。

use adw::prelude::*;
use gtk::prelude::*;
use libadwaita as adw;

use crate::state::Progress;
use crate::ui;

/// 进度面板的句柄。
#[derive(Debug, Clone)]
pub struct ProgressPanel {
    pub root: gtk::Box,
    bar: gtk::ProgressBar,
    phase: gtk::Label,
    detail: gtk::Label,
    view: gtk::TextView,
    buffer: gtk::TextBuffer,
    expander: gtk::Expander,
    summary: gtk::Label,
    copy: gtk::Button,
}

impl ProgressPanel {
    pub fn new() -> Self {
        let bar = gtk::ProgressBar::new();
        bar.set_show_text(true);
        bar.set_hexpand(true);

        let phase = gtk::Label::builder().xalign(0.0).build();
        phase.add_css_class("heading");
        let detail = ui::ellipsized("");
        detail.add_css_class("dim-label");

        let summary = gtk::Label::builder().xalign(0.0).wrap(true).build();
        summary.set_visible(false);

        let buffer = gtk::TextBuffer::new(None::<&gtk::TextTagTable>);
        let view = gtk::TextView::builder()
            .buffer(&buffer)
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .child(&view)
            .min_content_height(160)
            .max_content_height(320)
            .build();

        let expander = gtk::Expander::builder()
            .label(ui::t("原始日志"))
            .child(&scroll)
            .build();

        let copy = gtk::Button::with_label(&ui::t("复制错误详情"));
        copy.add_css_class("flat");
        copy.set_visible(false);
        {
            let buffer_clone = buffer.clone();
            let copy_clone = copy.clone();
            copy.connect_clicked(move |_| {
                let text = buffer_clone
                    .text(&buffer_clone.start_iter(), &buffer_clone.end_iter(), false)
                    .to_string();
                if archstore_core::aur_run::copy_to_clipboard(&text).is_ok() {
                    copy_clone.set_label(&ui::t("已复制"));
                } else {
                    copy_clone.set_label(&ui::t("复制失败"));
                }
            });
        }

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        actions.set_halign(gtk::Align::End);
        actions.append(&copy);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
        root.set_margin_top(12);
        root.set_margin_bottom(12);
        root.set_margin_start(12);
        root.set_margin_end(12);
        root.append(&phase);
        root.append(&bar);
        root.append(&detail);
        root.append(&summary);
        root.append(&expander);
        root.append(&actions);
        root.set_visible(false);

        Self {
            root,
            bar,
            phase,
            detail,
            view,
            buffer,
            expander,
            summary,
            copy,
        }
    }

    /// 刷新进度。
    pub fn update(&self, progress: &Progress) {
        self.root.set_visible(true);
        let overall = progress.overall();
        self.bar.set_fraction(f64::from(overall) / 100.0);
        self.phase.set_label(&phase_label(&progress.phase));
        self.detail.set_label(&progress.detail);
        self.sync_log(&progress.log_lines);
    }

    /// 事务结束：展示结果摘要。
    pub fn finish(&self, summary: &crate::state::TxSummary) {
        self.root.set_visible(true);
        self.bar.set_fraction(1.0);
        self.phase.set_label(&ui::t("事务已结束"));
        self.summary.set_visible(true);
        self.summary.set_label(&format!(
            "{}：安装/更新 {}，卸载 {}，失败 {}，耗时 {:.1} 秒",
            ui::t("结果"),
            summary.installed,
            summary.removed,
            summary.failed,
            summary.elapsed_ms as f64 / 1000.0
        ));
        self.copy.set_visible(summary.failed > 0);
    }

    /// 事务失败：附带日志尾部与"复制错误详情"。
    pub fn fail(&self, error: &str, log_tail: &str) {
        self.root.set_visible(true);
        self.bar.set_fraction(1.0);
        self.bar.add_css_class("error");
        self.phase.set_label(&ui::t("事务失败"));
        self.summary.set_visible(true);
        self.summary.set_label(error);
        self.expander.set_expanded(true);
        self.copy.set_visible(true);
        let lines: Vec<String> = log_tail.lines().map(|s| s.to_string()).collect();
        self.sync_log(&lines);
    }

    /// 重置为待执行状态。
    pub fn reset(&self) {
        self.root.set_visible(false);
        self.bar.set_fraction(0.0);
        self.bar.remove_css_class("error");
        self.buffer.set_text("");
        self.summary.set_visible(false);
        self.copy.set_visible(false);
        self.expander.set_expanded(false);
    }

    /// 同步日志：**增量**更新，避免每次事件都重建几千行的文本。
    ///
    /// 旧实现每次都会 `lines.join("\n")` 出一整块字符串再 `set_text`：
    /// flatpak 下载时每秒几十行输出，等于每秒重建几十次 5000 行文本 ——
    /// 既卡顿又制造大量临时内存（用户实测：安装 flatpak 时内存耗空）。
    fn sync_log(&self, lines: &[String]) {
        let n = lines.len();
        let existing = self.buffer.line_count() as usize;

        // 1) 追加一行（最常见）：直接插到尾部
        if n == existing + 1 {
            self.buffer
                .insert(&mut self.buffer.end_iter(), &format!("\n{}", lines[n - 1]));
            self.scroll_log_to_end();
            return;
        }
        // 2) 行数不变：替换最后一行（进度行原地刷新）
        if n == existing && n > 0 {
            let mut start = self
                .buffer
                .iter_at_line((n - 1) as i32)
                .unwrap_or_else(|| self.buffer.end_iter());
            let mut end = self.buffer.end_iter();
            self.buffer.delete(&mut start, &mut end);
            self.buffer
                .insert(&mut self.buffer.end_iter(), &lines[n - 1]);
            self.scroll_log_to_end();
            return;
        }
        // 3) 其它情况（首次显示、日志被环形截断）：全量重建
        self.buffer.set_text(&lines.join("\n"));
        self.scroll_log_to_end();
    }

    /// 日志滚到底部。
    fn scroll_log_to_end(&self) {
        let end = self.buffer.end_iter();
        let mark = self.buffer.create_mark(None, &end, false);
        self.view.scroll_mark_onscreen(&mark);
        self.buffer.delete_mark(&mark);
    }

    pub fn log_text(&self) -> String {
        self.buffer
            .text(&self.buffer.start_iter(), &self.buffer.end_iter(), false)
            .to_string()
    }

    pub fn bar(&self) -> &gtk::ProgressBar {
        &self.bar
    }

    pub fn phase_text(&self) -> String {
        self.phase.text().to_string()
    }

    pub fn is_visible(&self) -> bool {
        self.root.get_visible()
    }
}

impl Default for ProgressPanel {
    fn default() -> Self {
        Self::new()
    }
}

/// 阶段 key -> 面向用户的中文文案（软映射，未知阶段原样展示）。
pub fn phase_label(phase: &str) -> String {
    match phase {
        "resolve" => ui::t("解析依赖"),
        "keyring" => ui::t("检查密钥环"),
        "download" => ui::t("下载中"),
        "verify" => ui::t("校验软件包"),
        "conflict" => ui::t("检查文件冲突"),
        "install" => ui::t("安装中"),
        "upgrade" => ui::t("更新中"),
        "remove" => ui::t("卸载中"),
        "" => ui::t("处理中"),
        other => other.to_string(),
    }
}

/// 关闭窗口时的确认对话框（§5.4 规则 3：不杀 helper）。
pub fn ask_close_while_running(
    parent: &impl IsA<gtk::Widget>,
    on_background: impl Fn() + 'static,
    on_wait: impl Fn() + 'static,
) {
    let dialog = adw::AlertDialog::builder()
        .heading(ui::t("事务正在进行"))
        .body(ui::t(
            "关闭窗口会保留后台执行。杀掉 pacman 比让它跑完更危险，因此本程序不会中止事务。",
        ))
        .build();
    dialog.add_response("wait", &ui::t("等待完成"));
    dialog.add_response("background", &ui::t("最小化到后台"));
    dialog.set_default_response(Some("wait"));
    dialog.set_close_response("wait");
    let on_background = std::rc::Rc::new(on_background);
    let on_wait = std::rc::Rc::new(on_wait);
    dialog.connect_response(None, move |_, response| {
        if response == "background" {
            on_background();
        } else {
            on_wait();
        }
    });
    dialog.present(Some(parent));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_labels_cover_known_phases() {
        for p in [
            "resolve", "keyring", "download", "verify", "conflict", "install", "upgrade", "remove",
            "",
        ] {
            assert!(!phase_label(p).is_empty(), "{p}");
        }
        assert_eq!(phase_label("custom-phase"), "custom-phase");
    }
}
