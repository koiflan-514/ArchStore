//! archstore-helper：唯一以 root 运行的进程（project.md §5.2）。
//!
//! 接口：
//!   archstore-helper --plan <path> --kind <pacman-sync|pacman-remove|flatpak-install|flatpak-uninstall|flatpak-update>
//!   archstore-helper --plan <path> --kind <…> --dry-run     # 只校验不执行
//!   archstore-helper --version
//!   archstore-helper --self-check                            # 只读：验证 polkit action 与自身路径
//!
//! 无 GUI 依赖：不做任何显示服务器相关的事情，polkit action 里 allow_gui=false。

#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

mod exec;
mod protocol;
mod validate;

use std::path::PathBuf;
use std::process::ExitCode;

use archstore_core::VERSION;
use archstore_core::error::{CoreError, CoreResult};
use archstore_core::model::plan::PlanKind;

use protocol::{Emitter, codes};

/// 退出码约定：0 成功，1 失败，2 用法错误。
const EXIT_OK: u8 = 0;
const EXIT_FAILED: u8 = 1;
const EXIT_USAGE: u8 = 2;

/// 命令行解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Cli {
    Version,
    Help,
    SelfCheck,
    Run {
        plan: PathBuf,
        kind: PlanKind,
        dry_run: bool,
    },
}

fn usage() -> String {
    [
        "用法：",
        "  archstore-helper --plan <path> --kind <kind> [--dry-run]",
        "  archstore-helper --version",
        "  archstore-helper --self-check",
        "",
        "kind 取值：pacman-sync | pacman-remove | flatpak-install | flatpak-uninstall | flatpak-update",
    ]
    .join("\n")
}

fn parse_args(args: &[String]) -> CoreResult<Cli> {
    let mut plan: Option<PathBuf> = None;
    let mut kind: Option<PlanKind> = None;
    let mut dry_run = false;
    let mut i = 0usize;

    while i < args.len() {
        match args[i].as_str() {
            "--version" | "-V" => return Ok(Cli::Version),
            "--help" | "-h" => return Ok(Cli::Help),
            "--self-check" => return Ok(Cli::SelfCheck),
            "--dry-run" => dry_run = true,
            "--plan" => {
                i += 1;
                let value = args.get(i).ok_or_else(|| CoreError::PlanRejected {
                    reason: "--plan 缺少参数".into(),
                })?;
                plan = Some(PathBuf::from(value));
            }
            "--kind" => {
                i += 1;
                let value = args.get(i).ok_or_else(|| CoreError::PlanRejected {
                    reason: "--kind 缺少参数".into(),
                })?;
                kind = Some(PlanKind::parse(value)?);
            }
            other => {
                return Err(CoreError::PlanRejected {
                    reason: format!("未知参数：{other}"),
                });
            }
        }
        i += 1;
    }

    match (plan, kind) {
        (Some(plan), Some(kind)) => Ok(Cli::Run {
            plan,
            kind,
            dry_run,
        }),
        _ => Err(CoreError::PlanRejected {
            reason: "必须同时提供 --plan 与 --kind".into(),
        }),
    }
}

/// 把 CoreError 映射到协议里的错误码（GUI 依据 code 决定文案与后续动作）。
fn code_for(e: &CoreError) -> &'static str {
    match e {
        CoreError::InvalidName(_) => codes::INVALID_NAME,
        CoreError::NotFound(_) => codes::PKG_NOT_FOUND,
        CoreError::BackendUnavailable { .. } => codes::BACKEND_UNAVAILABLE,
        CoreError::Locked => codes::LOCKED,
        CoreError::Internal(_) => codes::INTERNAL,
        CoreError::TransactionFailed { .. } => codes::TRANSACTION_FAILED,
        _ => codes::PLAN_REJECTED,
    }
}

/// --self-check：只读地验证自身安装状态。
fn self_check() -> CoreResult<()> {
    let emitter = &mut Emitter::new();
    emitter.info(&format!("archstore-helper {VERSION}"));
    let helper_path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "未知".to_string());
    emitter.info(&format!("可执行文件：{helper_path}"));
    let expected = archstore_core::config::paths::helper_path();
    if helper_path == expected.display().to_string() {
        emitter.info(&format!(
            "路径正确（与 polkit action 的 exec.path 一致）：{}",
            expected.display()
        ));
    } else {
        emitter.warn(&format!(
            "当前路径与 polkit action 约定的 {} 不一致；通过 pkexec 调用时可能被拒绝",
            expected.display()
        ));
    }
    let policy = archstore_core::config::paths::policy_file();
    if policy.exists() {
        emitter.info(&format!("polkit action 已安装：{}", policy.display()));
    } else {
        emitter.warn(&format!("缺少 polkit action：{}", policy.display()));
    }
    if validate::pacman_lock_present() {
        emitter.warn("注意：/var/lib/pacman/db.lck 存在，当前有包管理器在运行");
    } else {
        emitter.info("pacman 数据库未被锁定");
    }
    emitter.info(&format!(
        "调用者：uid={} gid={}（直接以 root 运行：{}）",
        validate::Caller::detect().uid,
        validate::Caller::detect().gid,
        validate::Caller::detect().is_root_direct()
    ));
    Ok(())
}

/// 完整的一次事务执行。
fn run(plan_path: &std::path::Path, kind: PlanKind, dry_run: bool) -> CoreResult<()> {
    let emitter = &mut Emitter::new();
    let caller = validate::Caller::detect();

    // 1) 路径校验（唯一通过命令行传给 root 的东西）
    let path = validate::validate_plan_path(plan_path, &caller).inspect_err(|e| {
        emitter.error(codes::BAD_PLAN_PATH, &e.user_message());
    })?;

    // 2) 计划解析 + schema/包名/来源一致性校验
    let plan = validate::load_plan(&path).inspect_err(|e| {
        emitter.error(codes::PLAN_REJECTED, &e.user_message());
    })?;

    // 3) 命令行传入的 kind 必须与计划内部一致（防止类型混淆）
    if plan.kind != kind {
        let e = CoreError::PlanRejected {
            reason: format!(
                "--kind {} 与计划内部类型 {} 不一致",
                kind.as_str(),
                plan.kind.as_str()
            ),
        };
        emitter.error(codes::PLAN_REJECTED, &e.user_message());
        return Err(e);
    }

    // 4) 锁冲突（实测：无操作时 /var/lib/pacman/db.lck 不存在）
    if !plan.kind.is_flatpak() && validate::pacman_lock_present() {
        let e = CoreError::Locked;
        emitter.error(codes::LOCKED, &e.user_message());
        return Err(e);
    }

    // 5) 重新向系统确认每个条目存在 + 独立重算依赖
    let confirmed = validate::reconfirm_items(&plan, emitter).inspect_err(|e| {
        emitter.error(code_for(e), &e.user_message());
    })?;

    // 6) 构造参数数组
    let steps = exec::build_steps(&plan).inspect_err(|e| {
        emitter.error(codes::PLAN_REJECTED, &e.user_message());
    })?;

    emitter.start(plan.schema, confirmed.len(), plan.kind.as_str());

    if dry_run {
        for step in &steps {
            emitter.info(&format!("[dry-run] 将执行：{}", step.display()));
        }
        emitter.info(&format!(
            "[dry-run] 校验通过，共 {} 个条目、{} 条命令，未做任何修改",
            confirmed.len(),
            steps.len()
        ));
        emitter.done("dry-run", 0, 0, 0, 0);
        return Ok(());
    }

    let started = std::time::Instant::now();
    let stats = exec::execute(&plan, &steps, emitter)?;
    let status = if stats.failed == 0 { "ok" } else { "failed" };
    emitter.done(
        status,
        stats.installed,
        stats.failed,
        stats.removed,
        started.elapsed().as_millis(),
    );
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args) {
        Ok(Cli::Version) => {
            println!("archstore-helper {VERSION}");
            ExitCode::from(EXIT_OK)
        }
        Ok(Cli::Help) => {
            println!("{}", usage());
            ExitCode::from(EXIT_OK)
        }
        Ok(Cli::SelfCheck) => match self_check() {
            Ok(()) => ExitCode::from(EXIT_OK),
            Err(e) => {
                eprintln!("self-check 失败：{}", e.user_message());
                ExitCode::from(EXIT_FAILED)
            }
        },
        Ok(Cli::Run {
            plan,
            kind,
            dry_run,
        }) => match run(&plan, kind, dry_run) {
            Ok(()) => ExitCode::from(EXIT_OK),
            Err(e) => {
                // stdout 上已经发过结构化 error 事件，这里只补一条人读的 stderr
                eprintln!("archstore-helper [{}]: {}", code_for(&e), e.user_message());
                ExitCode::from(EXIT_FAILED)
            }
        },
        Err(e) => {
            eprintln!("archstore-helper: {}", e.user_message());
            eprintln!("{}", usage());
            ExitCode::from(EXIT_USAGE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_version_and_help_and_self_check() {
        assert_eq!(parse_args(&args(&["--version"])).expect("v"), Cli::Version);
        assert_eq!(parse_args(&args(&["-V"])).expect("v"), Cli::Version);
        assert_eq!(parse_args(&args(&["--help"])).expect("h"), Cli::Help);
        assert_eq!(
            parse_args(&args(&["--self-check"])).expect("s"),
            Cli::SelfCheck
        );
    }

    #[test]
    fn parses_run_arguments() {
        let cli =
            parse_args(&args(&["--plan", "/tmp/x.json", "--kind", "pacman-sync"])).expect("run");
        assert_eq!(
            cli,
            Cli::Run {
                plan: PathBuf::from("/tmp/x.json"),
                kind: PlanKind::PacmanSync,
                dry_run: false
            }
        );
    }

    #[test]
    fn parses_dry_run_flag() {
        let cli = parse_args(&args(&[
            "--dry-run",
            "--plan",
            "/tmp/x.json",
            "--kind",
            "flatpak-install",
        ]))
        .expect("run");
        match cli {
            Cli::Run { dry_run, kind, .. } => {
                assert!(dry_run);
                assert_eq!(kind, PlanKind::FlatpakInstall);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_and_unknown_arguments() {
        assert!(parse_args(&args(&[])).is_err());
        assert!(parse_args(&args(&["--plan", "/tmp/x.json"])).is_err());
        assert!(parse_args(&args(&["--kind", "pacman-sync"])).is_err());
        assert!(parse_args(&args(&["--plan"])).is_err());
        assert!(parse_args(&args(&["--kind"])).is_err());
        assert!(
            parse_args(&args(&["--plan", "/tmp/x", "--kind", "bogus"])).is_err(),
            "未知 kind 必须被拒绝"
        );
        assert!(
            parse_args(&args(&[
                "--plan",
                "/tmp/x",
                "--kind",
                "pacman-sync",
                "--evil"
            ]))
            .is_err(),
            "未知参数必须被拒绝"
        );
    }

    #[test]
    fn error_codes_cover_all_helper_visible_variants() {
        assert_eq!(
            code_for(&CoreError::InvalidName("x".into())),
            codes::INVALID_NAME
        );
        assert_eq!(
            code_for(&CoreError::NotFound("x".into())),
            codes::PKG_NOT_FOUND
        );
        assert_eq!(code_for(&CoreError::Locked), codes::LOCKED);
        assert_eq!(
            code_for(&CoreError::BackendUnavailable {
                kind: "flatpak".into(),
                reason: "x".into()
            }),
            codes::BACKEND_UNAVAILABLE
        );
        assert_eq!(code_for(&CoreError::Internal("x".into())), codes::INTERNAL);
        assert_eq!(
            code_for(&CoreError::TransactionFailed {
                code: 1,
                log_tail: String::new()
            }),
            codes::TRANSACTION_FAILED
        );
        assert_eq!(
            code_for(&CoreError::PlanRejected { reason: "x".into() }),
            codes::PLAN_REJECTED
        );
    }

    #[test]
    fn rejects_dangerous_kind_values() {
        for bad in ["rm -rf /", ";id", "../../x", "PACMAN-SYNC"] {
            assert!(
                parse_args(&args(&["--plan", "/tmp/x", "--kind", bad])).is_err(),
                "{bad:?} 必须被拒绝"
            );
        }
    }
}
