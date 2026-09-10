//! 通用 UI 工具：文案、徽章、样式加载、外链打开（project.md §7.5 / §11.1）。

use archstore_core::model::{Installed, PackageSource, PackageSummary, human_size};
use gtk::prelude::*;
use libadwaita as adw;

/// 取一条界面文案的译文（§7.2 第 3 层）。
pub fn t(s: &str) -> String {
    archstore_core::i18n::t(s)
}

/// 普通标签。
pub fn label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .halign(gtk::Align::Start)
        .build()
}

/// 次要文本样式。
pub fn dim(label: &gtk::Label) {
    label.add_css_class("dim-label");
}

/// 单行省略标签。
pub fn ellipsized(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .halign(gtk::Align::Start)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .single_line_mode(true)
        .build()
}

/// 纯文本标签（禁止 markup 注入：外部描述一律走这里）。
pub fn plain_text(text: &str) -> gtk::Label {
    let l = gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .halign(gtk::Align::Start)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .build();
    l.set_use_markup(false);
    l
}

/// 校验并设置 Pango markup（§11.1 规则 4：先 parse_markup）。
///
/// 校验失败时退回纯文本，绝不把未校验的字符串交给 markup 渲染。
pub fn set_validated_markup(label: &gtk::Label, markup: &str) {
    match gtk::pango::parse_markup(markup, '\0') {
        Ok(_) => {
            label.set_use_markup(true);
            label.set_markup(markup);
        }
        Err(e) => {
            tracing::debug!(error = %e, "markup 校验失败，退回纯文本");
            label.set_use_markup(false);
            label.set_text(markup);
        }
    }
}

/// 来源徽章（.source-badge.official / .aur / .flatpak）。
pub fn source_badge(source: &PackageSource) -> gtk::Label {
    let l = gtk::Label::builder()
        .label(match source {
            PackageSource::Official { repo } => format!("官方 {repo}"),
            PackageSource::Aur => "AUR".to_string(),
            PackageSource::Flatpak { remote } => format!("Flatpak {remote}"),
        })
        .build();
    l.add_css_class("source-badge");
    l.add_css_class(match source {
        PackageSource::Official { .. } => "official",
        PackageSource::Aur => "aur",
        PackageSource::Flatpak { .. } => "flatpak",
    });
    l.set_valign(gtk::Align::Center);
    l
}

/// 状态药丸：已安装 / 可更新 / 依赖 / 孤儿。
pub fn state_pill(summary: &PackageSummary) -> Option<gtk::Label> {
    let (text, class) = match (&summary.installed, summary.has_update()) {
        (_, true) => ("可更新".to_string(), "upgradable"),
        (Installed::Yes { explicit: true, .. }, _) => ("已安装".to_string(), "installed"),
        (
            Installed::Yes {
                explicit: false, ..
            },
            _,
        ) => ("依赖".to_string(), "orphan"),
        (Installed::No, _) => return None,
    };
    let l = gtk::Label::builder().label(text).build();
    l.add_css_class("state-pill");
    l.add_css_class(class);
    l.set_valign(gtk::Align::Center);
    Some(l)
}

/// "88.2 MB 下载 / 309.9 MB 安装" 之类的尺寸文案。
pub fn size_text(download: Option<u64>, installed: Option<u64>) -> String {
    match (download, installed) {
        (Some(d), Some(i)) if d > 0 && i > 0 => {
            format!("{} 下载 / {} 安装", human_size(d), human_size(i))
        }
        (Some(d), _) if d > 0 => format!("{} 下载", human_size(d)),
        (_, Some(i)) if i > 0 => format!("{} 安装", human_size(i)),
        _ => String::new(),
    }
}

/// 版本 + 来源 + 大小 的一行副标题（§7.4）。
pub fn subtitle(summary: &PackageSummary) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(v) = &summary.version {
        parts.push(v.clone());
    }
    parts.push(summary.id.source.display());
    if let Some(u) = &summary.update {
        parts.push(format!("可更新到 {}", u.candidate));
    }
    if let Some(p) = summary.popularity
        && p > 0.0
    {
        parts.push(format!("流行度 {p:.2}"));
    }
    if let Some(v) = summary.votes
        && v > 0
    {
        parts.push(format!("{v} 票"));
    }
    if summary.out_of_date {
        parts.push("已被上游标记为过期".to_string());
    }
    parts.join(" · ")
}

/// 加载 CSS（开发模式下监听文件变化热重载）。
///
/// 颜色一律使用 libadwaita 提供的命名色，保证深浅色自适应。
///
/// **优先级必须是 USER**：实测用户自带的 GTK 主题（如 Noctalia 的 Material You
/// gtk.css）以 `GTK_STYLE_PROVIDER_PRIORITY_USER` 加载，APPLICATION 级别会被它整个盖住 ——
/// 表现就是"来源开关勾了却看不到高亮色块"（用户实测反馈）。
/// USER 与我们自己的类选择器同时生效时，带 .source-toggle 的规则更具体，稳定胜出。
pub fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    if let Some(display) = gdk_display() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_USER,
        );
    }

    // 开发模式：监听磁盘上的 style.css 变化
    if std::env::var("ARCHSTORE_DEV").as_deref() == Ok("1") {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/style.css");
        let provider_reload = gtk::CssProvider::new();
        if let Some(display) = gdk_display() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider_reload,
                gtk::STYLE_PROVIDER_PRIORITY_USER + 1,
            );
        }
        let file = gio::File::for_path(&path);
        if let Ok(monitor) = file.monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
        {
            let path_for_reload = path.clone();
            monitor.connect_changed(move |_, _, _, _| {
                provider_reload.load_from_path(&path_for_reload);
            });
        }
        tracing::info!(path = %path.display(), "开发模式：已启用 CSS 热重载");
    }
}

fn gdk_display() -> Option<gtk::gdk::Display> {
    gtk::gdk::Display::default()
}

/// 用系统默认程序打开链接（只允许 http/https，§11.1 规则 4）。
pub fn open_uri(parent: &impl IsA<gtk::Widget>, uri: &str) {
    if !archstore_core::net::is_safe_external_url(uri) {
        tracing::warn!(uri, "拒绝打开非 http(s) 链接");
        return;
    }
    let launcher = gtk::UriLauncher::new(uri);
    // 只允许从窗口发起：UriLauncher 需要一个 gtk::Window 作为父窗口
    let window = parent
        .clone()
        .upcast::<gtk::Widget>()
        .root()
        .and_downcast::<gtk::Window>();
    launcher.launch(window.as_ref(), gio::Cancellable::NONE, |res| {
        if let Err(e) = res {
            tracing::warn!(error = %e, "无法打开链接");
        }
    });
}

/// 弹出 Toast。
pub fn toast(overlay: &adw::ToastOverlay, text: &str) {
    overlay.add_toast(adw::Toast::new(text));
}

/// 空态/错态都需要的"居中信息块"。
pub fn placeholder(icon_name: &str, title: &str, body: &str) -> gtk::Widget {
    let icon = gtk::Image::from_icon_name(icon_name);
    icon.set_pixel_size(64);
    icon.add_css_class("dim-label");

    let title_label = gtk::Label::builder()
        .label(title)
        .css_classes(["title-2"])
        .build();
    let body_label = gtk::Label::builder()
        .label(body)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["dim-label"])
        .build();

    let bx = gtk::Box::new(gtk::Orientation::Vertical, 12);
    bx.set_valign(gtk::Align::Center);
    bx.set_halign(gtk::Align::Center);
    bx.set_margin_top(48);
    bx.set_margin_bottom(48);
    bx.set_margin_start(24);
    bx.set_margin_end(24);
    bx.append(&icon);
    bx.append(&title_label);
    bx.append(&body_label);
    bx.upcast()
}

/// 加载中的骨架屏占位行（§7.3）。
pub fn skeleton_row() -> gtk::Widget {
    let bx = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    bx.add_css_class("package-row");
    let icon = gtk::Box::new(gtk::Orientation::Vertical, 0);
    icon.set_size_request(32, 32);
    icon.add_css_class("skeleton");
    let text = gtk::Box::new(gtk::Orientation::Vertical, 6);
    text.set_hexpand(true);
    let l1 = gtk::Box::new(gtk::Orientation::Vertical, 0);
    l1.set_size_request(-1, 14);
    l1.add_css_class("skeleton");
    let l2 = gtk::Box::new(gtk::Orientation::Vertical, 0);
    l2.set_size_request(-1, 10);
    l2.add_css_class("skeleton");
    l2.set_hexpand(true);
    text.append(&l1);
    text.append(&l2);
    bx.append(&icon);
    bx.append(&text);
    bx.upcast()
}

/// 顶部细进度条（搜索页用，不阻塞输入）。
pub fn thin_progress() -> gtk::ProgressBar {
    let bar = gtk::ProgressBar::new();
    bar.set_pulse_step(0.05);
    bar.set_valign(gtk::Align::Start);
    bar
}

#[cfg(test)]
mod tests {
    use super::*;
    use archstore_core::model::{IconRef, PackageId, UpdateInfo};

    fn sample() -> PackageSummary {
        let mut s = PackageSummary::minimal(PackageId::official("extra", "firefox"), "Firefox");
        s.version = Some("155.0.1-1".into());
        s.icon = IconRef::IconName("firefox".into());
        s
    }

    #[test]
    fn subtitle_includes_version_and_source() {
        let s = sample();
        let text = subtitle(&s);
        assert!(text.contains("155.0.1-1"));
        assert!(text.contains("extra"));
    }

    #[test]
    fn subtitle_mentions_update_and_flags() {
        let mut s = sample();
        s.update = Some(UpdateInfo {
            current: "1".into(),
            candidate: "2".into(),
            download_size: None,
        });
        s.out_of_date = true;
        s.popularity = Some(4.5);
        s.votes = Some(10);
        let text = subtitle(&s);
        assert!(text.contains("可更新到 2"));
        assert!(text.contains("过期"));
        assert!(text.contains("4.50"));
        assert!(text.contains("10 票"));
    }

    #[test]
    fn size_text_variants() {
        assert_eq!(
            size_text(Some(1024 * 1024), Some(2 * 1024 * 1024)),
            "1.0 MiB 下载 / 2.0 MiB 安装"
        );
        assert_eq!(size_text(Some(1024), None), "1.0 KiB 下载");
        assert_eq!(size_text(None, Some(1024)), "1.0 KiB 安装");
        assert_eq!(size_text(None, None), "");
        assert_eq!(size_text(Some(0), Some(0)), "");
    }

    #[test]
    fn t_falls_back_to_source_string() {
        assert_eq!(t("Install"), "Install");
    }
}
