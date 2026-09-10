//! 搜索页（§3.3 / §7.3）：防抖 300ms、来源开关、后端错误分区展示。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;

use archstore_core::backend::SourceFilter;
use archstore_core::model::PackageSummary;

use crate::pages::{EmptyState, ListPage};
use crate::ui;
use crate::widgets::RowContext;

/// 防抖时长（§3.3）。
pub const DEBOUNCE_MS: u32 = 300;

/// 搜索页。
pub struct SearchPage {
    pub page: ListPage,
    entry: gtk::SearchEntry,
    toggles: Rc<RefCell<SourceFilter>>,
    /// 当前有效的搜索代号：用于丢弃过期响应
    generation: Rc<Cell<u64>>,
    pending: Rc<Cell<bool>>,
    /// 与防抖闭包共享（clone 一个 RefCell 只会复制内容，分页/查询状态会读不到）
    last_query: Rc<RefCell<String>>,
    pub pacman_toggle: gtk::ToggleButton,
    pub aur_toggle: gtk::ToggleButton,
    pub flatpak_toggle: gtk::ToggleButton,
    /// 页面根控件：**来源开关常驻在结果区之外**。
    ///
    /// 这样"没有找到匹配的软件"只会替换结果区，
    /// 不会把搜索来源开关（以及搜索条件）一起吞掉。
    pub root: gtk::Box,
}

impl SearchPage {
    pub fn new(
        rows: &Rc<RowContext>,
        entry: gtk::SearchEntry,
        on_open: impl Fn(PackageSummary) + 'static,
        on_retry: Box<dyn Fn() + 'static>,
        on_settings: Box<dyn Fn() + 'static>,
        on_search_aur: Box<dyn Fn(String) + 'static>,
    ) -> Self {
        let toggles = Rc::new(RefCell::new(SourceFilter::default()));

        let make_toggle = |label: &str, active: bool| {
            let b = gtk::ToggleButton::with_label(label);
            b.set_active(active);
            b.add_css_class("flat");
            b
        };
        let pacman_toggle = make_toggle(&ui::t("本地"), true);
        let aur_toggle = make_toggle("AUR", true);
        let flatpak_toggle = make_toggle("Flatpak", true);

        let toggle_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        toggle_box.set_margin_start(12);
        toggle_box.set_margin_end(12);
        toggle_box.append(&ui::label(&ui::t("搜索来源：")));
        toggle_box.append(&pacman_toggle);
        toggle_box.append(&aur_toggle);
        toggle_box.append(&flatpak_toggle);

        let note = ui::label(&ui::t(
            "勾选 AUR / Flatpak 会在输入后联网请求；取消勾选则只查本地数据。",
        ));
        note.add_css_class("dim-label");
        note.set_margin_start(12);
        note.set_margin_end(12);

        // 结果区（三态在它内部切换）
        let page = ListPage::new(rows, None, on_open, Some(on_retry), Some(on_settings));

        // 来源开关 + 说明常驻在结果区上方，任何空态/错态都不会影响它们
        let controls = gtk::Box::new(gtk::Orientation::Vertical, 6);
        controls.set_margin_top(8);
        controls.append(&toggle_box);
        controls.append(&note);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.append(&controls);
        root.append(&page.shell.overlay);

        let this = Self {
            root,
            page,
            entry: entry.clone(),
            toggles: toggles.clone(),
            generation: Rc::new(Cell::new(0)),
            pending: Rc::new(Cell::new(false)),
            last_query: Rc::new(RefCell::new(String::new())),
            pacman_toggle,
            aur_toggle,
            flatpak_toggle,
        };

        // 来源开关只更新过滤状态；窗口会自行连接这些按钮并在变化后重跑搜索
        for (button, kind) in [
            (this.pacman_toggle.clone(), "pacman"),
            (this.aur_toggle.clone(), "aur"),
            (this.flatpak_toggle.clone(), "flatpak"),
        ] {
            let toggles = toggles.clone();
            button.connect_toggled(move |b| {
                let mut f = toggles.borrow_mut();
                match kind {
                    "pacman" => f.pacman = b.is_active(),
                    "aur" => f.aur = b.is_active(),
                    _ => f.flatpak = b.is_active(),
                }
            });
        }

        // 防抖：300ms 内的连续输入只触发一次真正的搜索
        {
            let generation = Rc::clone(&this.generation);
            let pending = Rc::clone(&this.pending);
            let last_query = Rc::clone(&this.last_query);
            let on_search_aur = Rc::new(on_search_aur);
            let page = this.page.clone();
            entry.connect_search_changed(move |e| {
                let text = e.text().to_string();
                *last_query.borrow_mut() = text.clone();
                let this_gen = generation.get() + 1;
                generation.set(this_gen);
                if text.trim().is_empty() {
                    page.store.remove_all();
                    page.shell.show_empty(
                        "edit-find-symbolic",
                        &ui::t("输入关键字开始搜索"),
                        &ui::t("默认只搜本地；勾选 AUR / Flatpak 会联网查询。"),
                        None,
                    );
                    return;
                }
                pending.set(true);
                page.progress.set_visible(true);
                page.progress.pulse();
                let generation = generation.clone();
                let pending = pending.clone();
                let page_clone = page.clone();
                let on_search_aur = on_search_aur.clone();
                glib::timeout_add_local_once(
                    std::time::Duration::from_millis(DEBOUNCE_MS as u64),
                    move || {
                        // 过期请求直接丢弃（用户在这 300ms 内又输入了）
                        if generation.get() != this_gen {
                            return;
                        }
                        if pending.get() {
                            page_clone.progress.set_visible(true);
                        }
                        let _ = &on_search_aur;
                    },
                );
            });
        }

        this
    }

    /// 当前来源开关。
    pub fn filter(&self) -> SourceFilter {
        *self.toggles.borrow()
    }

    /// 当前代号（丢弃过期响应）。
    pub fn generation(&self) -> u64 {
        self.generation.get()
    }

    pub fn next_generation(&self) -> u64 {
        let g = self.generation.get() + 1;
        self.generation.set(g);
        g
    }

    pub fn last_query(&self) -> String {
        self.last_query.borrow().clone()
    }

    /// 展示结果；分后端错误只影响对应来源。
    pub fn set_results(
        &self,
        items: &[PackageSummary],
        backend_errors: &[(String, String)],
        query: &str,
    ) {
        self.pending.set(false);
        self.page.progress.set_visible(false);
        if !backend_errors.is_empty() {
            let text = backend_errors
                .iter()
                .map(|(kind, msg)| format!("{kind} 请求失败：{msg}"))
                .collect::<Vec<_>>()
                .join("\n");
            if items.is_empty() {
                self.page.shell.show_error_text(&ui::t("搜索失败"), &text);
                return;
            }
            self.page.shell.toast(&text);
        }
        let empty = EmptyState::new(
            "edit-find-symbolic",
            format!("{}「{query}」{}", ui::t("没有找到与"), ui::t("匹配的软件")),
            ui::t("可以尝试勾选 AUR / Flatpak 来源，或检查拼写。"),
        );
        self.page.set_items(items, empty);
    }

    pub fn show_loading(&self) {
        self.page.progress.set_visible(true);
        self.page.progress.pulse();
    }

    pub fn search_entry(&self) -> &gtk::SearchEntry {
        &self.entry
    }
}
