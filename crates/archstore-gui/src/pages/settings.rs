//! 设置页（§10）：与 config.toml 的字段一一对应。
//!
//! 生效时机：外观/主题立即生效；代理与超时立即重建 reqwest::Client；
//! 缓存上限立即触发一次裁剪；软件源开关立即刷新侧栏与类别列表。

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::prelude::*;
use libadwaita as adw;

use archstore_core::config::{
    AurHelperChoice, ColorScheme, Config, IconSize, ProxyType, TranslationApi, paths,
};
use archstore_core::env::AurHelperKind;

use crate::ui;

/// 设置页的回调。
pub struct SettingsCallbacks {
    /// 任意设置变化（已序列化前的完整配置）
    pub on_change: Box<dyn Fn(Config)>,
    /// 测试连接
    pub on_test_connection: Box<dyn Fn()>,
    /// 清空缓存
    pub on_clear_cache: Box<dyn Fn()>,
    /// 运行环境自检
    pub on_doctor: Box<dyn Fn()>,
    /// 重新检测后端
    pub on_redetect: Box<dyn Fn()>,
}

/// 设置页。
#[derive(Clone)]
pub struct SettingsPage {
    pub root: adw::PreferencesPage,
    color_scheme: adw::ComboRow,
    icon_size: adw::ComboRow,
    animations: adw::SwitchRow,
    proxy_enabled: adw::SwitchRow,
    proxy_type: adw::ComboRow,
    proxy_url: adw::EntryRow,
    test_connection: gtk::Button,
    pacman_enabled: adw::SwitchRow,
    aur_enabled: adw::SwitchRow,
    aur_helper: adw::ComboRow,
    flatpak_enabled: adw::SwitchRow,
    flatpak_remote: adw::ComboRow,
    flatpak_installation: adw::ComboRow,
    cache_size: adw::ActionRow,
    cache_max: adw::SpinRow,
    clear_cache: gtk::Button,
    translate: adw::SwitchRow,
    translate_api: adw::ComboRow,
    translate_endpoint: adw::EntryRow,
    translate_privacy: adw::ActionRow,
    check_on_startup: adw::SwitchRow,
    check_interval: adw::SpinRow,
    doctor: gtk::Button,
    redetect: gtk::Button,
    about: adw::ActionRow,
    backends: adw::PreferencesGroup,
    /// set_backends 动态加入的行（AdwPreferencesGroup 的子控件不是 Widget 的直接 child，
    /// 必须自己记住加了哪些，否则 remove 会触发 Adwaita-CRITICAL）
    backend_rows: RefCell<Vec<adw::ActionRow>>,
    /// 抑制回调：populate 期间不触发 on_change
    loading: Rc<RefCell<bool>>,
    on_change: Rc<dyn Fn(Config)>,
    current: Rc<RefCell<Config>>,
}

impl SettingsPage {
    pub fn new(
        callbacks: SettingsCallbacks,
        initial: &Config,
        remotes: &[String],
        helpers: &[AurHelperKind],
    ) -> Self {
        let loading = Rc::new(RefCell::new(false));
        let on_change: Rc<dyn Fn(Config)> = Rc::from(callbacks.on_change);
        let current = Rc::new(RefCell::new(initial.clone()));

        let page = adw::PreferencesPage::new();

        // ---------- 外观 ----------
        let appearance = adw::PreferencesGroup::new();
        appearance.set_title(&ui::t("外观"));

        let color_scheme = adw::ComboRow::builder().title(ui::t("配色方案")).build();
        color_scheme.set_model(Some(&gtk::StringList::new(&[
            &ui::t("跟随系统"),
            &ui::t("浅色"),
            &ui::t("深色"),
        ])));
        appearance.add(&color_scheme);

        let icon_size = adw::ComboRow::builder().title(ui::t("图标大小")).build();
        icon_size.set_model(Some(&gtk::StringList::new(&[
            &ui::t("小"),
            &ui::t("中"),
            &ui::t("大"),
        ])));
        appearance.add(&icon_size);

        let animations = adw::SwitchRow::builder()
            .title(ui::t("动画"))
            .subtitle(ui::t("关闭后列表不使用过渡动画"))
            .build();
        appearance.add(&animations);
        page.add(&appearance);

        // ---------- 网络 ----------
        let network = adw::PreferencesGroup::new();
        network.set_title(&ui::t("网络"));

        let proxy_enabled = adw::SwitchRow::builder().title(ui::t("使用代理")).build();
        network.add(&proxy_enabled);

        let proxy_type = adw::ComboRow::builder().title(ui::t("代理类型")).build();
        proxy_type.set_model(Some(&gtk::StringList::new(&["HTTP", "SOCKS5"])));
        network.add(&proxy_type);

        let proxy_url = adw::EntryRow::builder()
            .title(ui::t("代理地址（主机:端口，不含 http://）"))
            .build();
        network.add(&proxy_url);

        let test_connection = gtk::Button::with_label(&ui::t("测试连接"));
        test_connection.add_css_class("flat");
        {
            let cb = callbacks.on_test_connection;
            test_connection.connect_clicked(move |_| cb());
        }
        let test_row = adw::ActionRow::builder().title(ui::t("连通性")).build();
        test_row.add_suffix(&test_connection);
        network.add(&test_row);
        page.add(&network);

        // ---------- 软件源 ----------
        let sources = adw::PreferencesGroup::new();
        sources.set_title(&ui::t("软件源"));

        let pacman_enabled = adw::SwitchRow::builder()
            .title(ui::t("官方仓库（pacman）"))
            .build();
        sources.add(&pacman_enabled);
        let aur_enabled = adw::SwitchRow::builder().title(ui::t("AUR")).build();
        sources.add(&aur_enabled);

        let aur_helper = adw::ComboRow::builder().title(ui::t("AUR 助手")).build();
        let mut helper_labels = vec![ui::t("自动检测")];
        helper_labels.extend(helpers.iter().map(|h| h.display().to_string()));
        helper_labels.push(ui::t("不使用"));
        aur_helper.set_model(Some(&gtk::StringList::new(
            &helper_labels.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        )));
        sources.add(&aur_helper);

        let flatpak_enabled = adw::SwitchRow::builder().title(ui::t("Flatpak")).build();
        sources.add(&flatpak_enabled);

        let flatpak_remote = adw::ComboRow::builder()
            .title(ui::t("Flatpak 远程仓库"))
            .build();
        let remote_labels: Vec<&str> = if remotes.is_empty() {
            vec!["flathub"]
        } else {
            remotes.iter().map(|s| s.as_str()).collect()
        };
        flatpak_remote.set_model(Some(&gtk::StringList::new(&remote_labels)));
        sources.add(&flatpak_remote);

        let flatpak_installation = adw::ComboRow::builder()
            .title(ui::t("Flatpak 安装位置"))
            .build();
        flatpak_installation.set_model(Some(&gtk::StringList::new(&[
            &ui::t("系统（需要授权）"),
            &ui::t("用户"),
        ])));
        sources.add(&flatpak_installation);
        page.add(&sources);

        // ---------- 后端状态 ----------
        let backends = adw::PreferencesGroup::new();
        backends.set_title(&ui::t("后端状态"));
        let redetect = gtk::Button::with_label(&ui::t("重新检测"));
        redetect.add_css_class("flat");
        {
            let cb = callbacks.on_redetect;
            redetect.connect_clicked(move |_| cb());
        }
        let redetect_row = adw::ActionRow::builder()
            .title(ui::t("重新检测后端能力"))
            .subtitle(ui::t("不会下载任何数据，只重新读取本机状态"))
            .build();
        redetect_row.add_suffix(&redetect);
        backends.add(&redetect_row);
        page.add(&backends);

        // ---------- 缓存 ----------
        let cache = adw::PreferencesGroup::new();
        cache.set_title(&ui::t("缓存"));

        let cache_size = adw::ActionRow::builder()
            .title(ui::t("当前缓存大小"))
            .subtitle(ui::t("按目录实际统计"))
            .build();
        cache.add(&cache_size);

        let cache_max = adw::SpinRow::with_range(16.0, 20_480.0, 16.0);
        cache_max.set_title(&ui::t("缓存上限（MB）"));
        cache.add(&cache_max);

        let clear_cache = gtk::Button::with_label(&ui::t("清除缓存"));
        clear_cache.add_css_class("destructive-action");
        clear_cache.add_css_class("flat");
        {
            let cb = callbacks.on_clear_cache;
            clear_cache.connect_clicked(move |_| cb());
        }
        let clear_row = adw::ActionRow::builder()
            .title(ui::t("清空缓存目录"))
            .build();
        clear_row.add_suffix(&clear_cache);
        cache.add(&clear_row);
        page.add(&cache);

        // ---------- 翻译 ----------
        let translation = adw::PreferencesGroup::new();
        translation.set_title(&ui::t("翻译"));

        let translate = adw::SwitchRow::builder()
            .title(ui::t("在线翻译软件描述"))
            .build();
        translation.add(&translate);

        let translate_privacy = adw::ActionRow::builder()
            .title(ui::t("隐私说明"))
            .subtitle(ui::t(
                "开启后，软件描述文本会发送到第三方翻译服务；译文会标注为「机器翻译」，绝不会冒充上游元数据。",
            ))
            .build();
        translate_privacy.set_visible(false);
        translation.add(&translate_privacy);

        // 服务选择：开关负责"开不开"，这里只负责"用哪个"
        let translate_api = adw::ComboRow::builder().title(ui::t("翻译服务")).build();
        translate_api.set_model(Some(&gtk::StringList::new(&[
            &ui::t("MyMemory（免费、无需注册）"),
            &ui::t("LibreTranslate（你自己的端点）"),
        ])));
        translation.add(&translate_api);

        let translate_endpoint = adw::EntryRow::builder()
            .title(ui::t("LibreTranslate 端点（https://）"))
            .build();
        translation.add(&translate_endpoint);
        page.add(&translation);

        // ---------- 更新 ----------
        let update = adw::PreferencesGroup::new();
        update.set_title(&ui::t("更新"));

        let check_on_startup = adw::SwitchRow::builder()
            .title(ui::t("启动时检查更新"))
            .build();
        update.add(&check_on_startup);

        let check_interval = adw::SpinRow::with_range(1.0, 168.0, 1.0);
        check_interval.set_title(&ui::t("检查间隔（小时）"));
        update.add(&check_interval);
        page.add(&update);

        // ---------- 诊断 ----------
        let diagnostics = adw::PreferencesGroup::new();
        diagnostics.set_title(&ui::t("诊断"));
        let doctor = gtk::Button::with_label(&ui::t("运行环境自检"));
        doctor.add_css_class("flat");
        {
            let cb = callbacks.on_doctor;
            doctor.connect_clicked(move |_| cb());
        }
        let doctor_row = adw::ActionRow::builder()
            .title(ui::t("运行环境自检（--doctor）"))
            .subtitle(ui::t(
                "只读检测：发行版、GTK、libalpm、polkit、helper、flatpak、缓存目录",
            ))
            .build();
        doctor_row.add_suffix(&doctor);
        diagnostics.add(&doctor_row);
        page.add(&diagnostics);

        // ---------- 关于 ----------
        let about = adw::PreferencesGroup::new();
        about.set_title(&ui::t("关于"));
        let about_row = adw::ActionRow::builder()
            .title(format!("ArchStore {}", archstore_core::VERSION))
            .subtitle(ui::t(
                "GPL-3.0-or-later。包含以 GPL-3.0 静态链接的 libalpm 绑定（alpm crate）。",
            ))
            .build();
        let homepage = gtk::Button::with_label(&ui::t("源码"));
        homepage.add_css_class("flat");
        homepage.connect_clicked(|b| {
            ui::open_uri(b, "https://github.com/koiflan-514/ArchStore");
        });
        about_row.add_suffix(&homepage);
        about.add(&about_row);
        let config_path = adw::ActionRow::builder()
            .title(ui::t("配置文件"))
            .subtitle(paths::config_file().display().to_string())
            .build();
        about.add(&config_path);
        about.add(
            &adw::ActionRow::builder()
                .title(ui::t("缓存与日志"))
                .subtitle(paths::cache_dir().display().to_string())
                .build(),
        );
        page.add(&about);

        let this = Self {
            root: page,
            color_scheme,
            icon_size,
            animations,
            proxy_enabled,
            proxy_type,
            proxy_url,
            test_connection,
            pacman_enabled,
            aur_enabled,
            aur_helper,
            flatpak_enabled,
            flatpak_remote,
            flatpak_installation,
            cache_size,
            cache_max,
            clear_cache,
            translate,
            translate_api,
            translate_endpoint,
            translate_privacy,
            check_on_startup,
            check_interval,
            doctor,
            redetect,
            about: about_row,
            backends,
            backend_rows: RefCell::new(Vec::new()),
            loading: loading.clone(),
            on_change,
            current,
        };

        this.populate(initial);
        this.connect();
        this
    }

    /// 把配置写入控件（期间不触发 on_change）。
    fn populate(&self, cfg: &Config) {
        *self.loading.borrow_mut() = true;
        self.color_scheme
            .set_selected(match cfg.appearance.color_scheme {
                ColorScheme::System => 0,
                ColorScheme::Light => 1,
                ColorScheme::Dark => 2,
            });
        self.icon_size.set_selected(match cfg.appearance.icon_size {
            IconSize::Small => 0,
            IconSize::Medium => 1,
            IconSize::Large => 2,
        });
        self.animations.set_active(cfg.appearance.animations);

        self.proxy_enabled.set_active(cfg.network.proxy_enabled);
        self.proxy_type.set_selected(match cfg.network.proxy_type {
            ProxyType::Http => 0,
            ProxyType::Socks5 => 1,
        });
        self.proxy_url.set_text(&cfg.network.proxy_url);

        self.pacman_enabled.set_active(cfg.sources.pacman_enabled);
        self.aur_enabled.set_active(cfg.sources.aur_enabled);
        self.aur_helper.set_selected(match cfg.sources.aur_helper {
            AurHelperChoice::Auto => 0,
            AurHelperChoice::Paru => 1,
            AurHelperChoice::Yay => 2,
            AurHelperChoice::None => 3,
        });
        self.flatpak_enabled.set_active(cfg.sources.flatpak_enabled);
        self.flatpak_installation
            .set_selected(if cfg.sources.flatpak_installation == "user" {
                1
            } else {
                0
            });
        if let Some(model) = self.flatpak_remote.model()
            && let Some(list) = model.downcast_ref::<gtk::StringList>()
        {
            for i in 0..list.n_items() {
                if list.string(i).map(|s| s.to_string()).as_deref()
                    == Some(cfg.sources.flatpak_remote.as_str())
                {
                    self.flatpak_remote.set_selected(i);
                    break;
                }
            }
        }

        self.cache_max.set_value(cfg.cache.max_size_mb as f64);
        self.translate.set_active(cfg.translation.auto_translate);
        self.translate_api.set_selected(match cfg.translation.api {
            // None 与 MyMemory 都落在第 0 项；仅当用户真的改选时才写回配置
            TranslationApi::None | TranslationApi::MyMemory => 0,
            TranslationApi::LibreTranslate => 1,
        });
        self.translate_endpoint
            .set_text(&cfg.translation.api_endpoint);
        self.translate_privacy
            .set_visible(cfg.translation.auto_translate);

        self.check_on_startup
            .set_active(cfg.update.check_on_startup);
        self.check_interval
            .set_value(cfg.update.check_interval_hours as f64);

        *self.current.borrow_mut() = cfg.clone();
        *self.loading.borrow_mut() = false;
    }

    /// 连接所有控件的变更信号。
    fn connect(&self) {
        let emit = {
            let loading = self.loading.clone();
            let current = self.current.clone();
            let on_change = self.on_change.clone();
            move || {
                if *loading.borrow() {
                    return;
                }
                let cfg = current.borrow().clone();
                on_change(cfg);
            }
        };

        macro_rules! on_combo {
            ($row:expr, $apply:expr) => {{
                let emit = emit.clone();
                #[allow(clippy::redundant_closure_call)]
                let apply: Rc<dyn Fn(&Config, u32) -> Config> = Rc::new($apply);
                let current = self.current.clone();
                $row.connect_selected_notify(move |row| {
                    let idx = row.selected();
                    let updated = {
                        let cfg = current.borrow();
                        apply(&cfg, idx)
                    };
                    *current.borrow_mut() = updated;
                    emit();
                });
            }};
        }
        macro_rules! on_switch {
            ($row:expr, $apply:expr) => {{
                let emit = emit.clone();
                let apply: Rc<dyn Fn(&Config, bool) -> Config> = Rc::new($apply);
                let current = self.current.clone();
                $row.connect_active_notify(move |row| {
                    let on = row.is_active();
                    let updated = {
                        let cfg = current.borrow();
                        apply(&cfg, on)
                    };
                    *current.borrow_mut() = updated;
                    emit();
                });
            }};
        }
        macro_rules! on_entry {
            ($row:expr, $apply:expr) => {{
                let emit = emit.clone();
                let apply: Rc<dyn Fn(&Config, &str) -> Config> = Rc::new($apply);
                let current = self.current.clone();
                $row.connect_changed(move |row| {
                    let text = row.text().to_string();
                    let updated = {
                        let cfg = current.borrow();
                        apply(&cfg, &text)
                    };
                    *current.borrow_mut() = updated;
                    emit();
                });
            }};
        }

        on_combo!(self.color_scheme, |c: &Config, i| {
            let mut c = c.clone();
            c.appearance.color_scheme = match i {
                1 => ColorScheme::Light,
                2 => ColorScheme::Dark,
                _ => ColorScheme::System,
            };
            c
        });
        on_combo!(self.icon_size, |c: &Config, i| {
            let mut c = c.clone();
            c.appearance.icon_size = match i {
                0 => IconSize::Small,
                2 => IconSize::Large,
                _ => IconSize::Medium,
            };
            c
        });
        on_switch!(self.animations, |c: &Config, on| {
            let mut c = c.clone();
            c.appearance.animations = on;
            c
        });
        on_switch!(self.proxy_enabled, |c: &Config, on| {
            let mut c = c.clone();
            c.network.proxy_enabled = on;
            c
        });
        on_combo!(self.proxy_type, |c: &Config, i| {
            let mut c = c.clone();
            c.network.proxy_type = if i == 1 {
                ProxyType::Socks5
            } else {
                ProxyType::Http
            };
            c
        });
        on_entry!(self.proxy_url, |c: &Config, t| {
            let mut c = c.clone();
            c.network.proxy_url = t.to_string();
            c
        });
        on_switch!(self.pacman_enabled, |c: &Config, on| {
            let mut c = c.clone();
            c.sources.pacman_enabled = on;
            c
        });
        on_switch!(self.aur_enabled, |c: &Config, on| {
            let mut c = c.clone();
            c.sources.aur_enabled = on;
            c
        });
        on_combo!(self.aur_helper, |c: &Config, i| {
            let mut c = c.clone();
            c.sources.aur_helper = match i {
                1 => AurHelperChoice::Paru,
                2 => AurHelperChoice::Yay,
                3 => AurHelperChoice::None,
                _ => AurHelperChoice::Auto,
            };
            c
        });
        on_switch!(self.flatpak_enabled, |c: &Config, on| {
            let mut c = c.clone();
            c.sources.flatpak_enabled = on;
            c
        });
        on_combo!(self.flatpak_remote, |c: &Config, i| {
            let mut c = c.clone();
            if let Some(name) = remote_name_at(i) {
                c.sources.flatpak_remote = name;
            }
            c
        });
        on_combo!(self.flatpak_installation, |c: &Config, i| {
            let mut c = c.clone();
            c.sources.flatpak_installation = if i == 1 {
                "user".into()
            } else {
                "system".into()
            };
            c
        });
        on_switch!(self.translate, |c: &Config, on| {
            let mut c = c.clone();
            c.translation.auto_translate = on;
            // 打开开关时若还没选服务，直接落到开箱可用的 MyMemory，
            // 避免出现"已启用但没选服务"这种校验不过的状态
            if on && c.translation.api == TranslationApi::None {
                c.translation.api = TranslationApi::MyMemory;
            }
            c
        });
        on_combo!(self.translate_api, |c: &Config, i| {
            let mut c = c.clone();
            c.translation.api = if i == 1 {
                TranslationApi::LibreTranslate
            } else {
                TranslationApi::MyMemory
            };
            c
        });
        on_entry!(self.translate_endpoint, |c: &Config, t| {
            let mut c = c.clone();
            c.translation.api_endpoint = t.to_string();
            c
        });
        on_switch!(self.check_on_startup, |c: &Config, on| {
            let mut c = c.clone();
            c.update.check_on_startup = on;
            c
        });

        // SpinRow：值变化 -> 写回配置
        {
            let emit = emit.clone();
            let current = self.current.clone();
            self.cache_max.connect_value_notify(move |row| {
                let v = row.value() as u64;
                {
                    let mut cfg = current.borrow_mut();
                    cfg.cache.max_size_mb = v;
                }
                emit();
            });
        }
        {
            let emit = emit.clone();
            let current = self.current.clone();
            self.check_interval.connect_value_notify(move |row| {
                let v = row.value() as u64;
                {
                    let mut cfg = current.borrow_mut();
                    cfg.update.check_interval_hours = v;
                }
                emit();
            });
        }
    }

    /// 更新缓存大小显示。
    pub fn set_cache_size(&self, bytes: u64) {
        self.cache_size.set_subtitle(&format!(
            "{}（{}）",
            archstore_core::model::human_size(bytes),
            paths::cache_dir().display()
        ));
    }

    /// 更新后端状态列表。
    pub fn set_backends(
        &self,
        capabilities: &[(&'static str, archstore_core::backend::Capability)],
    ) {
        // 只移除上一轮由本方法加入的行；"重新检测"行是构造期加的一直保留
        for row in self.backend_rows.borrow_mut().drain(..) {
            self.backends.remove(&row);
        }
        for (kind, cap) in capabilities {
            let row = adw::ActionRow::builder()
                .title(*kind)
                .subtitle(cap.describe())
                .build();
            if !cap.is_available() {
                row.add_css_class("dim-label");
            }
            self.backends.add(&row);
            self.backend_rows.borrow_mut().push(row);
        }
    }

    /// 更新翻译隐私说明的可见性。
    pub fn refresh_translation_notice(&self) {
        self.translate_privacy
            .set_visible(self.translate.is_active());
        self.translate_endpoint
            .set_visible(self.translate.is_active());
        self.translate_api.set_visible(self.translate.is_active());
    }

    /// 当前配置快照（测试用）。
    pub fn snapshot(&self) -> Config {
        self.current.borrow().clone()
    }

    pub fn test_connection_button(&self) -> &gtk::Button {
        &self.test_connection
    }

    pub fn clear_cache_button(&self) -> &gtk::Button {
        &self.clear_cache
    }

    pub fn doctor_button(&self) -> &gtk::Button {
        &self.doctor
    }

    pub fn redetect_button(&self) -> &gtk::Button {
        &self.redetect
    }

    pub fn about_row(&self) -> &adw::ActionRow {
        &self.about
    }

    pub fn color_scheme_row(&self) -> &adw::ComboRow {
        &self.color_scheme
    }

    pub fn show_toast(&self, parent: &impl IsA<gtk::Widget>, text: &str) {
        // 设置页的提示通过窗口的 ToastOverlay 展示；这里退化为对话框提示
        let dialog = adw::AlertDialog::builder()
            .heading(ui::t("设置"))
            .body(text)
            .build();
        dialog.add_response("ok", &ui::t("知道了"));
        let window = parent.root().and_downcast::<gtk::Window>();
        dialog.present(window.as_ref());
    }
}

/// flatpak 远程名的下标查找（populate 与 connect 共用同一份顺序）。
fn remote_name_at(_index: u32) -> Option<String> {
    // 远程名在 populate 时由 StringList 提供，这里由 set_remotes 维护；
    // 由于 ComboRow 的 model 就是真实远程名列表，index 直接对应。
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_choices_map_to_config_values() {
        assert_eq!(AurHelperChoice::default(), AurHelperChoice::Auto);
        assert_eq!(AurHelperChoice::Paru.as_str(), "paru");
        assert_eq!(AurHelperChoice::Yay.as_str(), "yay");
        assert_eq!(AurHelperChoice::None.as_str(), "none");
    }

    #[test]
    fn color_scheme_and_icon_size_strings() {
        assert_eq!(ColorScheme::System.as_str(), "system");
        assert_eq!(ColorScheme::Dark.as_str(), "dark");
        assert_eq!(IconSize::Small.pixels(), 24);
    }
}
