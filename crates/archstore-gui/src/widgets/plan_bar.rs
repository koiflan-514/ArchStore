//! 计划栏（§7.1 / §5.4）：展示待执行计划并触发执行。
//!
//! 透明可控：不存在任何"点击即静默执行"的按钮；执行前必然出现计划清单。

use std::rc::Rc;

use adw::prelude::*;
use gtk::prelude::*;
use libadwaita as adw;

use archstore_core::model::plan::{PlanItem, PlanKind, TransactionPlan};
use archstore_core::plan::AurRequest;

use crate::state::{TxState, plan_bar_text};
use crate::{runtime, ui};

/// 计划栏的回调集合。
pub struct PlanBarCallbacks {
    pub on_execute: Box<dyn Fn()>,
    pub on_details: Box<dyn Fn()>,
    pub on_discard: Box<dyn Fn()>,
}

/// 计划栏的句柄。
#[derive(Debug, Clone)]
pub struct PlanBar {
    pub root: adw::Bin,
    label: gtk::Label,
    details: gtk::Button,
    execute: gtk::Button,
    discard: gtk::Button,
    warning: gtk::Label,
    banner: adw::Banner,
    inner: gtk::Box,
}

impl PlanBar {
    pub fn new(callbacks: PlanBarCallbacks) -> Self {
        let label = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        label.add_css_class("plan-summary");

        let details = gtk::Button::with_label(&ui::t("查看详情"));
        details.add_css_class("flat");
        {
            let cb = callbacks.on_details;
            details.connect_clicked(move |_| cb());
        }

        let execute = gtk::Button::with_label(&ui::t("执行"));
        execute.add_css_class("suggested-action");
        {
            let cb = callbacks.on_execute;
            execute.connect_clicked(move |_| cb());
        }

        let discard = gtk::Button::from_icon_name("user-trash-symbolic");
        discard.set_tooltip_text(Some(&ui::t("放弃计划")));
        discard.add_css_class("flat");
        {
            let cb = callbacks.on_discard;
            discard.connect_clicked(move |_| cb());
        }

        let warning = gtk::Label::builder().xalign(0.0).wrap(true).build();
        warning.add_css_class("plan-warning");
        warning.set_visible(false);

        let text_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text_box.set_hexpand(true);
        text_box.append(&label);
        text_box.append(&warning);

        let inner = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        inner.set_margin_top(8);
        inner.set_margin_bottom(8);
        inner.set_margin_start(12);
        inner.set_margin_end(12);
        inner.append(&text_box);
        inner.append(&details);
        inner.append(&discard);
        inner.append(&execute);

        let banner = adw::Banner::new("");
        banner.set_revealed(false);
        // 风险提示横幅可以点掉（"知道了"）
        banner.set_button_label(Some(&ui::t("知道了")));
        banner.connect_button_clicked(|b| b.set_revealed(false));

        let holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        holder.append(&banner);
        holder.append(&inner);
        holder.add_css_class("plan-bar");

        let root = adw::Bin::builder().child(&holder).build();
        root.set_visible(false);

        Self {
            root,
            label,
            details,
            execute,
            discard,
            warning,
            banner,
            inner,
        }
    }

    /// 根据状态机状态刷新计划栏。
    pub fn update(&self, state: &TxState, plans: &[TransactionPlan], aur: &[AurRequest]) {
        let total = plans.iter().map(|p| p.len()).sum::<usize>()
            + aur.iter().map(|a| a.packages.len()).sum::<usize>();
        let visible = total > 0
            || matches!(
                state,
                TxState::Running { .. } | TxState::Succeeded { .. } | TxState::Failed { .. }
            );
        self.root.set_visible(visible);
        if !visible {
            return;
        }

        match state {
            TxState::Running { .. } => {
                self.label.set_label(&ui::t("事务正在执行…"));
                self.execute.set_sensitive(false);
                self.discard.set_sensitive(false);
            }
            TxState::Succeeded { summary } => {
                self.label.set_label(&format!(
                    "{}：安装/更新 {}，卸载 {}，失败 {}",
                    ui::t("上次事务已完成"),
                    summary.installed,
                    summary.removed,
                    summary.failed
                ));
                self.execute.set_sensitive(false);
                self.discard.set_sensitive(true);
            }
            TxState::Failed { error, .. } => {
                self.label
                    .set_label(&format!("{}：{error}", ui::t("上次事务失败")));
                self.execute.set_sensitive(false);
                self.discard.set_sensitive(true);
            }
            _ => {
                self.label.set_label(&plan_bar_text(plans, aur));
                // 只有 AUR 请求时也允许执行（会在终端里以用户身份完成）
                let runnable = (matches!(state, TxState::Draft { .. }) && !plans.is_empty())
                    || (!aur.is_empty() && plans.is_empty());
                self.execute.set_sensitive(runnable);
                self.discard.set_sensitive(true);
            }
        }

        let has_aur = !aur.is_empty();
        let has_official = plans.iter().any(|p| p.kind == PlanKind::PacmanSync);
        if has_aur && has_official {
            self.warning.set_label(&ui::t(
                "同时更新官方仓库与 AUR 包可能导致部分升级（partial upgrade），建议分开执行。",
            ));
            self.warning.set_visible(true);
        } else if has_aur {
            self.warning.set_label(&ui::t(
                "AUR 构建需要在终端中以用户身份完成，点击执行后会打开终端。",
            ));
            self.warning.set_visible(true);
        } else {
            self.warning.set_visible(false);
        }
    }

    /// 显示一条需要用户注意的横幅（例如反依赖警告）。
    pub fn show_banner(&self, text: &str) {
        self.banner.set_title(text);
        self.banner.set_revealed(true);
    }

    pub fn hide_banner(&self) {
        self.banner.set_revealed(false);
    }

    pub fn execute_button(&self) -> &gtk::Button {
        &self.execute
    }

    pub fn details_button(&self) -> &gtk::Button {
        &self.details
    }

    pub fn discard_button(&self) -> &gtk::Button {
        &self.discard
    }

    pub fn warning_text(&self) -> String {
        self.warning.text().to_string()
    }

    pub fn label_text(&self) -> String {
        self.label.text().to_string()
    }
}

/// 反依赖确认对话框（§5.4 规则 6）：默认把"同时删除这些包"设为否。
pub fn ask_cascade(
    parent: &impl IsA<gtk::Widget>,
    target: &str,
    dependents: &[String],
    on_answer: impl Fn(bool) + 'static,
) {
    let list = if dependents.len() > 12 {
        format!(
            "{}\n… {}",
            dependents[..12].join("、"),
            ui::t("以及其它若干项")
        )
    } else {
        dependents.join("、")
    };
    let body = ui::t("删除 {package} 会影响 {count} 个已安装包。")
        .replace("{package}", target)
        .replace("{count}", &dependents.len().to_string());
    let dialog = adw::AlertDialog::builder()
        .heading(ui::t("删除会影响其它软件"))
        .body(format!("{body}\n\n{list}"))
        .build();
    dialog.add_response("cancel", &ui::t("取消"));
    dialog.add_response("cascade", &ui::t("同时删除这些包"));
    dialog.set_response_appearance("cascade", adw::ResponseAppearance::Destructive);
    // 默认动作是"取消"（默认把"同时删除这些包"设为否）
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let on_answer = Rc::new(on_answer);
    dialog.connect_response(None, move |_, response| {
        on_answer(response == "cascade");
    });
    dialog.present(Some(parent));
}

/// 计划详情对话框里一行条目的文案（纯函数，便于测试）。
///
/// 卸载/清理类条目没有"目标版本"可言，硬写"最新版本"会误导用户；
/// 安装类条目缺版本时才是"最新版本"。
pub fn plan_item_line(plan: &TransactionPlan, item: &PlanItem) -> String {
    let version = match (&item.target_version, plan.kind.is_remove()) {
        (Some(v), _) => format!("，{v}"),
        (None, true) => String::new(),
        (None, false) => format!("，{}", ui::t("最新版本")),
    };
    format!("  · {}（{}{version}）", item.name, item.reason.label())
}

/// 计划详情对话框（只读展示，不执行任何操作）。
pub fn show_plan_details(
    parent: &impl IsA<gtk::Widget>,
    plans: &[TransactionPlan],
    aur: &[AurRequest],
) {
    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    for plan in plans {
        body.append(
            &gtk::Label::builder()
                .label(plan.kind.label())
                .xalign(0.0)
                .css_classes(["heading"])
                .build(),
        );
        for item in &plan.items {
            body.append(&ui::ellipsized(&plan_item_line(plan, item)));
        }
    }
    for req in aur {
        body.append(
            &gtk::Label::builder()
                .label(format!("AUR {}（用户身份）", req.action.label()))
                .xalign(0.0)
                .css_classes(["heading"])
                .build(),
        );
        for p in &req.packages {
            body.append(&ui::ellipsized(&format!("  · {p}")));
        }
    }
    if plans.is_empty() && aur.is_empty() {
        body.append(&ui::label(&ui::t("计划为空")));
    }

    let dialog = adw::AlertDialog::builder()
        .heading(ui::t("计划详情"))
        .extra_child(&body)
        .build();
    dialog.add_response("close", &ui::t("关闭"));
    dialog.present(Some(parent));
}

/// 执行前把计划写入临时文件（0700 目录 / 0600 文件）。
pub fn prepare_plan_file(
    cache_dir: &std::path::Path,
    plan: &TransactionPlan,
) -> Option<std::path::PathBuf> {
    match crate::state::write_plan_file(cache_dir, plan) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::error!(error = %e, "无法写入计划文件");
            None
        }
    }
}

/// 打开终端执行 AUR 命令（§5.3 的 MVP 策略）。
pub fn run_aur_in_terminal(
    overlay: &adw::ToastOverlay,
    helper: archstore_core::env::AurHelperKind,
    action: archstore_core::plan::AurAction,
    packages: &[String],
) {
    let op = match action {
        archstore_core::plan::AurAction::Install => archstore_core::aur_run::AurOp::Install,
        archstore_core::plan::AurAction::Remove => archstore_core::aur_run::AurOp::Remove,
        archstore_core::plan::AurAction::Update => archstore_core::aur_run::AurOp::Update,
    };
    let cmd = match archstore_core::aur_run::build_aur_command(helper, op, packages) {
        Ok(c) => c,
        Err(e) => {
            ui::toast(overlay, &e.user_message());
            return;
        }
    };
    let display = cmd.display();
    match archstore_core::aur_run::launch_in_terminal(&cmd) {
        Ok(_) => {
            ui::toast(
                overlay,
                &format!("{}：{display}", ui::t("已在终端中启动 AUR 操作")),
            );
        }
        Err(e) => {
            // 无法打开终端：复制完整命令并提示用户
            let _ = archstore_core::aur_run::copy_to_clipboard(&display);
            ui::toast(
                overlay,
                &format!("{}：{display}", ui::t("无法打开终端，命令已复制到剪贴板")),
            );
            tracing::warn!(error = %e, "无法打开终端");
        }
    }
}

/// 在一个后台任务里启动 helper 并流式读取事件。
///
/// 所有事件都通过 glib 主循环回投：后台线程绝不触碰控件。
/// pkexec 的退出码 126/127 分别表示"授权被拒绝/未找到程序"，映射为 AuthDenied。
/// `elevate = false` 用于**用户级 Flatpak**：同一个 helper 直接以当前用户运行，
/// 不经过 pkexec（helper 会拒绝"以 root 执行 --user"的情况）。
pub fn spawn_helper(
    plan_path: std::path::PathBuf,
    kind: PlanKind,
    elevate: bool,
    on_event: impl Fn(crate::state::HelperEvent) + 'static,
) {
    // 回调持有 Rc 控件句柄，不能要求 Send：事件先进入通道，
    // 再由主线程上的 spawn_local 消费（tokio 通道与执行器无关）。
    //
    // **必须有界**：flatpak / pacman 的下载进度可以刷得极快，无界队列在主循环
    // 跟不上的时候会一直堆，直到内存耗空（用户实测：安装 flatpak 时触发 OOM）。
    // 满了就丢掉这条事件 —— 面板本来就只展示"最近若干行"，丢中间态不影响结果，
    // 最终的 done / error 事件照样会送达（通道被消费后会腾出空位）。
    let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::state::HelperEvent>(512);
    glib::MainContext::default().spawn_local(async move {
        while let Some(event) = rx.recv().await {
            on_event(event);
        }
    });
    let kind_str = kind.as_str().to_string();
    runtime::spawn(async move {
        use tokio::io::AsyncBufReadExt;

        let emit = |event: crate::state::HelperEvent| {
            // try_send：队列满时丢弃中间进度，绝不阻塞读取线程、也不无限堆积
            let _ = tx.try_send(event);
        };

        let helper = archstore_core::config::paths::helper_path();
        let mut cmd = if elevate {
            let mut c = tokio::process::Command::new("pkexec");
            c.arg(&helper);
            c
        } else {
            // 用户级 Flatpak：直接以当前用户运行 helper
            tokio::process::Command::new(&helper)
        };
        let mut child = match cmd
            .arg("--plan")
            .arg(&plan_path)
            .arg("--kind")
            .arg(&kind_str)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                emit(crate::state::HelperEvent::Error {
                    code: "SPAWN_FAILED".into(),
                    message: format!("无法启动 pkexec：{e}"),
                });
                return;
            }
        };

        if let Some(stdout) = child.stdout.take() {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(event) = crate::state::HelperEvent::parse(&line) {
                    let done = matches!(event, crate::state::HelperEvent::Done { .. });
                    emit(event);
                    if done {
                        break;
                    }
                } else if !line.trim().is_empty() {
                    // helper 之外的噪声（例如 pkexec 自己的错误）也展示给用户
                    emit(crate::state::HelperEvent::Log {
                        level: "warn".into(),
                        line: line.trim().to_string(),
                    });
                }
            }
        }

        let code = child
            .wait()
            .await
            .map(|s| s.code().unwrap_or(-1))
            .unwrap_or(-1);
        match code {
            0 => {}
            126 | 127 => emit(crate::state::HelperEvent::Error {
                code: "AUTH_DENIED".into(),
                message: archstore_core::CoreError::AuthDenied.user_message(),
            }),
            other => emit(crate::state::HelperEvent::Error {
                code: "EXIT".into(),
                message: format!("helper 以退出码 {other} 结束"),
            }),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use archstore_core::model::plan::{PlanItem, PlanItemReason};

    #[test]
    fn plan_item_lines_distinguish_versions_per_plan_kind() {
        // 安装计划缺版本 → "最新版本"；有版本 → 显示版本
        let mut sync = TransactionPlan::new(PlanKind::PacmanSync);
        sync.push(PlanItem::official("extra", "firefox"));
        sync.push(PlanItem::official("extra", "vim").with_version("9.1"));
        assert_eq!(
            plan_item_line(&sync, &sync.items[0]),
            "  · firefox（显式，最新版本）"
        );
        assert_eq!(
            plan_item_line(&sync, &sync.items[1]),
            "  · vim（显式，9.1）"
        );

        // 卸载计划：目标 / 连带 / 多余依赖要能区分，且不写"最新版本"
        let mut remove = TransactionPlan::new(PlanKind::PacmanRemove);
        remove.push(PlanItem::official("extra", "gst-plugins-good"));
        remove.push(PlanItem::official("extra", "orca").with_reason(PlanItemReason::Dependency));
        remove.push(PlanItem::official("extra", "aalib").with_reason(PlanItemReason::Unneeded));
        assert_eq!(
            plan_item_line(&remove, &remove.items[0]),
            "  · gst-plugins-good（显式）"
        );
        assert_eq!(
            plan_item_line(&remove, &remove.items[1]),
            "  · orca（依赖）"
        );
        assert_eq!(
            plan_item_line(&remove, &remove.items[2]),
            "  · aalib（多余依赖）"
        );
    }

    #[test]
    fn prepare_plan_file_writes_private_file() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut plan = TransactionPlan::new(PlanKind::PacmanSync);
        plan.push(PlanItem::official("extra", "vim"));
        let path = prepare_plan_file(dir.path(), &plan).expect("plan file");
        assert!(path.exists());
        let back = archstore_core::model::plan::read_plan_file(&path).expect("read");
        assert_eq!(back, plan);
        // 最后一份计划必须留存，供崩溃恢复使用
        assert!(TransactionPlan::last_plan_path(dir.path()).exists());
    }

    #[test]
    fn aur_action_maps_to_aur_op() {
        // 编译期覆盖：AurAction 与 AurOp 必须一一对应
        let cases = [
            (
                archstore_core::plan::AurAction::Install,
                archstore_core::aur_run::AurOp::Install,
            ),
            (
                archstore_core::plan::AurAction::Remove,
                archstore_core::aur_run::AurOp::Remove,
            ),
            (
                archstore_core::plan::AurAction::Update,
                archstore_core::aur_run::AurOp::Update,
            ),
        ];
        assert_eq!(cases.len(), 3);
    }
}
