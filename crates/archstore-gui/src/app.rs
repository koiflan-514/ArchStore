//! AdwApplication 装配（§2.4 / §3.4 / §11.1 规则 5）。

use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;

use crate::window::{self, MainWindow};

/// 构造应用对象。
pub fn build() -> adw::Application {
    let app = adw::Application::builder()
        .application_id(archstore_core::APP_ID)
        .flags(gio::ApplicationFlags::empty())
        .build();

    let main_window: Rc<std::cell::RefCell<Option<Rc<MainWindow>>>> =
        Rc::new(std::cell::RefCell::new(None));

    {
        app.connect_startup(move |_| {
            // 主题按配置立即生效
            let loaded = archstore_core::config::Config::load_default();
            let cfg = loaded.config.sanitized();
            apply_color_scheme(cfg.appearance.color_scheme);
            crate::ui::load_css();
            tracing::info!("应用启动");
        });
    }

    {
        let holder = main_window.clone();
        app.connect_activate(move |app| {
            if let Some(existing) = holder.borrow().clone() {
                existing.window.present();
                return;
            }
            let win = window::build(app);
            window::start_services(&win);
            {
                let win_for_close = win.clone();
                win.window
                    .connect_close_request(move |_| window::request_close(&win_for_close));
            }
            win.window.present();
            *holder.borrow_mut() = Some(win);
        });
    }

    {
        let holder = main_window.clone();
        app.connect_shutdown(move |_| {
            if let Some(win) = holder.borrow().clone() {
                window::shutdown(&win);
            }
            tracing::info!("应用退出");
        });
    }

    app
}

/// 把配置里的配色方案应用到 AdwStyleManager。
pub fn apply_color_scheme(scheme: archstore_core::config::ColorScheme) {
    let manager = adw::StyleManager::default();
    let value = match scheme {
        archstore_core::config::ColorScheme::System => adw::ColorScheme::Default,
        archstore_core::config::ColorScheme::Light => adw::ColorScheme::ForceLight,
        archstore_core::config::ColorScheme::Dark => adw::ColorScheme::ForceDark,
    };
    manager.set_color_scheme(value);
}
