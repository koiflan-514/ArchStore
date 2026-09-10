//! archstore-gui 启动序列（project.md §2.4 / §3.4 / §11.1）。
//!
//! 1. 解析命令行：--version / --doctor / --help 走纯 CLI 路径，不初始化 GTK。
//! 2. 初始化日志与 panic hook。
//! 3. gtk::init()；失败则以退出码 1 结束并打印原因（不得 panic）。
//! 4. 校验 gtk / libadwaita 运行期版本，不满足则用原生 gtk::AlertDialog 说明。
//! 5. 单实例保护（flock $XDG_RUNTIME_DIR/archstore.lock）。
//! 6. 启动 AdwApplication。

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]
// 界面层显式放行 dead_code，原因有二，都与"控件句柄只能在主线程使用"有关：
//
// 1. **测试可观测性**：界面是二进制目标，单元测试无法从外部观察控件状态。
//    因此每个控件/页面都保留一组只读访问器（state_name / phase_text / primary_button /
//    log_text / category_count / is_visible …），只被 src/smoke.rs 的 UI 冒烟测试使用。
//    在非 test 目标里它们看起来"从未使用"，但删掉就等于放弃对这些控件的回归测试。
// 2. **测试专用的构造路径**：如 PageShell::show_empty 的 action 参数、
//    EmptyState::with_action、IconCache::clear 等，只在测试或错误恢复路径中调用。
//
// 真正的界面缺陷由 src/smoke.rs 覆盖（构造每一个页面与控件并断言状态转移），
// 而不是靠编译器的"从未使用"告警发现，故在此整体放行。
#![allow(dead_code)]

mod app;
mod icon_cache;
mod pages;
mod runtime;
#[cfg(test)]
mod smoke;
mod state;
mod ui;
mod widgets;
mod window;

use std::path::PathBuf;
use std::process::ExitCode;

use archstore_core::config::paths;
use archstore_core::env::{self, Level};
use archstore_core::{VERSION, i18n};

use gtk::prelude::*;
use libadwaita as adw;

/// 退出码：0 正常；1 环境不满足；2 用法错误。
const EXIT_OK: u8 = 0;
const EXIT_ENV: u8 = 1;
const EXIT_USAGE: u8 = 2;

fn usage() -> String {
    [
        concat!(
            "ArchStore ",
            env!("CARGO_PKG_VERSION"),
            " —— Arch Linux 图形化软件商店"
        ),
        "",
        "用法：",
        "  archstore                 启动图形界面",
        "  archstore --doctor        只读输出环境自检结果（退出码 = 失败项数量）",
        "  archstore --doctor --json 以 JSON 输出自检结果",
        "  archstore --version       显示版本",
        "  archstore --help          显示本帮助",
    ]
    .join("\n")
}

/// 命令行解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Cli {
    Gui,
    Version,
    Help,
    Doctor { json: bool },
}

fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut doctor = false;
    let mut json = false;
    for a in args {
        match a.as_str() {
            "--doctor" => doctor = true,
            "--json" => json = true,
            "--version" | "-V" => return Ok(Cli::Version),
            "--help" | "-h" => return Ok(Cli::Help),
            other => return Err(format!("未知参数：{other}")),
        }
    }
    Ok(if doctor {
        Cli::Doctor { json }
    } else {
        Cli::Gui
    })
}

/// 单实例保护：对 $XDG_RUNTIME_DIR/archstore.lock 取 flock(LOCK_EX | LOCK_NB)。
///
/// 两个实例会各自维护 alpm 只读句柄与缓存索引，第二个实例退出时的索引写入
/// 可能覆盖第一个实例的结果，因此不强行启动第二个实例。
struct InstanceLock {
    _file: std::fs::File,
}

fn acquire_instance_lock() -> Result<Option<InstanceLock>, String> {
    use std::os::unix::io::AsRawFd;

    let path = paths::lock_file();
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return Ok(None);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("无法创建锁文件 {}：{e}", path.display()))?;
    // SAFETY: fd 在本函数返回期间始终有效（file 被移入返回值）
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(Some(InstanceLock { _file: file }))
    } else {
        Ok(None)
    }
}

/// 初始化日志：stderr + $XDG_CACHE_HOME/archstore/archstore.log（滚动 <= 5 MB）。
fn init_logging() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let log_path = paths::log_file();
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    rotate_log(&log_path, 5 * 1024 * 1024);

    let file_layer = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok()
        .map(|f| {
            tracing_subscriber::fmt::layer()
                .with_writer(std::sync::Mutex::new(f))
                .with_ansi(false)
        });

    let env_filter = tracing_subscriber::EnvFilter::try_from_env("ARCHSTORE_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(file_layer)
        .try_init();
}

/// 简单的日志滚动：超过上限就整体截断为最后一半。
fn rotate_log(path: &std::path::Path, max_bytes: u64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= max_bytes {
        return;
    }
    if let Ok(text) = std::fs::read_to_string(path) {
        let keep = text.len() / 2;
        let start = text.len().saturating_sub(keep);
        // 保证不切断 UTF-8 字符
        let mut idx = start;
        while idx < text.len() && !text.is_char_boundary(idx) {
            idx += 1;
        }
        let _ = std::fs::write(path, &text[idx..]);
    }
}

/// panic hook：写入日志并弹出"发生内部错误"对话框，绝不静默退出（§11.1 规则 6）。
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "未知位置".to_string());
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "未知 panic".to_string());
        tracing::error!(location = %location, message = %message, "发生内部错误（panic）");
        // 写一份独立文件，便于用户直接找到并附带在 issue 中
        let crash = paths::cache_dir().join("last-crash.txt");
        let _ = std::fs::write(
            &crash,
            format!(
                "ArchStore {VERSION}\n位置：{location}\n信息：{message}\n日志：{}\n",
                paths::log_file().display()
            ),
        );
        show_crash_dialog(&crash);
        default_hook(info);
    }));
}

/// 用最基础的 GTK 对话框报告崩溃（不依赖 libadwaita 的高级组件）。
fn show_crash_dialog(crash_file: &std::path::Path) {
    // 主线程之外不能创建控件；这里尽力而为，失败就只留日志。
    let path = crash_file.display().to_string();
    let log = paths::log_file().display().to_string();
    runtime::to_main(move || {
        if !gtk::is_initialized() {
            eprintln!("ArchStore 发生内部错误。\n日志：{log}\n详情：{path}");
            return;
        }
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message(i18n::t("发生内部错误"))
            .detail(format!(
                "{}\n\n{}",
                i18n::t("应用遇到了未预期的问题，但没有静默退出。"),
                format_args!("日志：{log}\n详情：{path}")
            ))
            .build();
        dialog.show(None::<&gtk::Window>);
    });
}

/// 读取运行期 gtk / libadwaita 版本。
fn runtime_versions() -> ((u32, u32, u32), (u32, u32, u32)) {
    (
        (
            gtk::major_version(),
            gtk::minor_version(),
            gtk::micro_version(),
        ),
        (
            adw::major_version(),
            adw::minor_version(),
            adw::micro_version(),
        ),
    )
}

/// --doctor：只读输出检测结果，退出码 = 失败项数量。
fn run_doctor(json: bool) -> ExitCode {
    init_logging();
    i18n::init();
    let loaded = archstore_core::config::Config::load_default();
    let cfg = loaded.config.sanitized();

    // 尽力获取 GTK 运行期版本；没有显示服务器时降级为 None
    let (gtk_v, adw_v) = if gtk::init().is_ok() {
        let (g, a) = runtime_versions();
        (Some(g), Some(a))
    } else {
        (None, None)
    };

    let report = env::probe(gtk_v, adw_v, &cfg);
    if json {
        println!("{}", report.to_json());
    } else {
        print!("{}", report.render());
    }
    for c in &report.checks {
        if c.level == Level::Fail {
            eprintln!("[FAIL] {}：{}", c.title, c.detail);
        }
    }
    ExitCode::from(report.exit_code().clamp(0, 255) as u8)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("archstore: {e}");
            eprintln!("{}", usage());
            return ExitCode::from(EXIT_USAGE);
        }
    };

    match cli {
        Cli::Help => {
            println!("{}", usage());
            return ExitCode::from(EXIT_OK);
        }
        Cli::Version => {
            println!("archstore {VERSION}");
            return ExitCode::from(EXIT_OK);
        }
        Cli::Doctor { json } => return run_doctor(json),
        Cli::Gui => {}
    }

    init_logging();
    i18n::init();
    install_panic_hook();

    // 1) gtk::init()：失败以退出码 1 结束并打印原因（不得 panic）
    if let Err(e) = gtk::init() {
        eprintln!("archstore: 无法初始化 GTK：{e}");
        eprintln!("提示：请确认已在图形会话中运行（DISPLAY 或 WAYLAND_DISPLAY 已设置）。");
        return ExitCode::from(EXIT_ENV);
    }

    // 2) 运行期版本校验（防 panic：feature 版本高于运行库会在初始化时崩溃）
    let (gtk_v, adw_v) = runtime_versions();
    let gtk_ok = env::version_at_least(gtk_v, env::MIN_GTK);
    let adw_ok = env::version_at_least(adw_v, env::MIN_ADW);
    if !gtk_ok || !adw_ok {
        let detail = format!(
            "gtk4 运行时 {}.{}.{}（要求 >= {}.{}）\nlibadwaita 运行时 {}.{}.{}（要求 >= {}.{}）\n\n请升级系统包：sudo pacman -Syu gtk4 libadwaita",
            gtk_v.0,
            gtk_v.1,
            gtk_v.2,
            env::MIN_GTK.0,
            env::MIN_GTK.1,
            adw_v.0,
            adw_v.1,
            adw_v.2,
            env::MIN_ADW.0,
            env::MIN_ADW.1
        );
        tracing::error!(%detail, "运行期版本不满足最低要求");
        // 不依赖 libadwaita 的高级组件：用原生 gtk::AlertDialog
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message(i18n::t("系统库版本过低，无法启动 ArchStore"))
            .detail(detail)
            .build();
        dialog.show(None::<&gtk::Window>);
        let ctx = glib::MainContext::default();
        while !gtk::Window::list_toplevels().is_empty() {
            ctx.iteration(true);
        }
        return ExitCode::from(EXIT_ENV);
    }

    // 3) 单一实例保护
    match acquire_instance_lock() {
        Ok(Some(lock)) => {
            tracing::info!(path = %paths::lock_file().display(), "已获得单实例锁");
            std::mem::forget(lock); // 进程存活期间一直持有
        }
        Ok(None) => {
            eprintln!(
                "ArchStore 已在运行（锁文件 {}）。本程序不启动第二个实例。",
                paths::lock_file().display()
            );
            return ExitCode::from(EXIT_OK);
        }
        Err(e) => {
            tracing::warn!(error = %e, "无法建立单实例锁，继续启动");
        }
    }

    // 4) 启动异步运行时（仅网络层使用）
    if let Err(e) = runtime::init() {
        tracing::error!(error = %e, "无法初始化异步运行时，网络功能将不可用");
    }

    // 5) 进入 GTK 主循环
    let app = app::build();
    let code = i32::from(app.run_with_args::<&str>(&[]));
    ExitCode::from(u8::try_from(code.clamp(0, 255)).unwrap_or(1))
}

/// 供 window 使用：从 GApplication 取出应用 id。
pub fn app_id() -> &'static str {
    archstore_core::APP_ID
}

/// 供测试与诊断使用：当前工作目录（避免在主线程做路径推断）。
pub fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_args_variants() {
        assert_eq!(parse_args(&args(&[])).expect("gui"), Cli::Gui);
        assert_eq!(parse_args(&args(&["--version"])).expect("v"), Cli::Version);
        assert_eq!(parse_args(&args(&["-V"])).expect("v"), Cli::Version);
        assert_eq!(parse_args(&args(&["--help"])).expect("h"), Cli::Help);
        assert_eq!(
            parse_args(&args(&["--doctor"])).expect("d"),
            Cli::Doctor { json: false }
        );
        assert_eq!(
            parse_args(&args(&["--doctor", "--json"])).expect("d"),
            Cli::Doctor { json: true }
        );
        assert!(parse_args(&args(&["--evil"])).is_err());
    }

    #[test]
    fn rotate_log_truncates_large_files() {
        let dir = std::env::temp_dir();
        let path = dir.join("archstore-rotate-test.log");
        let big = "x".repeat(1024 * 1024);
        std::fs::write(&path, &big).expect("write");
        rotate_log(&path, 1024);
        let size = std::fs::metadata(&path).expect("stat").len();
        assert!(size < 1024 * 1024, "日志未被滚动：{size}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rotate_log_keeps_small_files() {
        let dir = std::env::temp_dir();
        let path = dir.join("archstore-rotate-small.log");
        std::fs::write(&path, "hello").expect("write");
        rotate_log(&path, 1024);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "hello");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn usage_mentions_doctor_and_version() {
        let u = usage();
        assert!(u.contains("--doctor"));
        assert!(u.contains("--version"));
    }
}
