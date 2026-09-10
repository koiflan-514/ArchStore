//! AUR 执行路径（project.md §5.3）。
//!
//! 硬性规则：
//! - 检测顺序 paru -> yay -> 无。
//! - 参数必须按助手类型分别构造，且每个 --flag 都要在实现阶段用 --help 实测确认。
//! - 参数白名单：助手参数由程序常量决定，绝不接受来自 UI 或计划的字符串。
//! - AUR 安装/构建不走 pkexec（必须在非 root 下进行），助手内部自行调 sudo。
//! - MVP 不嵌入 PTY：GUI 打开终端执行；无法打开终端则复制完整命令到剪贴板。

use crate::env::AurHelperKind;
use crate::error::{CoreError, CoreResult};

/// 一次 AUR 助手调用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AurCommand {
    pub program: String,
    /// 参数数组（绝不拼 shell 字符串）
    pub args: Vec<String>,
    /// 目标包名（已通过白名单校验）
    pub packages: Vec<String>,
}

impl AurCommand {
    /// 供 UI 展示 / 复制到剪贴板的命令行文本。
    ///
    /// 只用于展示与用户手动粘贴；程序自身的执行一律走参数数组。
    pub fn display(&self) -> String {
        let mut parts = vec![self.program.clone()];
        parts.extend(self.args.iter().cloned());
        parts.extend(self.packages.iter().map(|p| shell_quote(p)));
        parts.join(" ")
    }

    /// 完整的 argv（program + args + packages）。
    pub fn argv(&self) -> Vec<String> {
        let mut out = vec![self.program.clone()];
        out.extend(self.args.iter().cloned());
        out.extend(self.packages.iter().cloned());
        out
    }
}

/// 需要引号时才加引号（包名已在白名单内，通常不需要）。
fn shell_quote(s: &str) -> String {
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-' | b'@'))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// AUR 助手可执行的操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurOp {
    Install,
    Remove,
    Update,
}

/// 按助手类型与操作类型构造参数（全部来自程序常量）。
///
/// yay 的参数已用 yay 13.0.1 --help 实测确认：
/// --noconfirm / --sudoloop / --answerclean / --cleanafter / --pgpfetch / --devel /
/// --rebuild / --save / --repo / --aur 均存在。
///
/// paru 在本机未安装，其参数标记为"待验证（阶段 4 前置任务）"：
/// 在验证完成前只使用必然存在的 -S / -Rns / -Syu 与 --noconfirm。
pub fn build_aur_command(
    helper: AurHelperKind,
    op: AurOp,
    packages: &[String],
) -> CoreResult<AurCommand> {
    if packages.is_empty() {
        return Err(CoreError::PlanRejected {
            reason: "AUR 操作没有任何目标包".into(),
        });
    }
    for p in packages {
        crate::model::plan::validate_name(p).map_err(|_| CoreError::InvalidName(p.clone()))?;
    }

    let args: Vec<String> = match (helper, op) {
        // yay（已实测参数）
        (AurHelperKind::Yay, AurOp::Install) => [
            "-S",
            "--noconfirm",
            "--answerclean",
            "None",
            "--answerdiff",
            "None",
            "--sudoloop",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        (AurHelperKind::Yay, AurOp::Remove) => ["-Rns", "--noconfirm"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        (AurHelperKind::Yay, AurOp::Update) => ["-Syu", "--noconfirm", "--devel"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        // paru（待验证，只用必然存在的参数）
        (AurHelperKind::Paru, AurOp::Install) => ["-S", "--noconfirm"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        (AurHelperKind::Paru, AurOp::Remove) => ["-Rns", "--noconfirm"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        (AurHelperKind::Paru, AurOp::Update) => ["-Syu", "--noconfirm"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };

    Ok(AurCommand {
        program: helper.binary().to_string(),
        args,
        packages: packages.to_vec(),
    })
}

/// 该助手是否支持"构建后自动清理"（对应 yay --cleanafter）。
///
/// 仅当助手确实支持时才在 UI 中展示该开关（§8.2 规则 2）。
pub fn supports_cleanafter(helper: AurHelperKind) -> bool {
    matches!(helper, AurHelperKind::Yay)
}

/// 已知的终端模拟器候选与"执行命令"参数形式。
///
/// 返回 (程序名, 前置参数)，随后追加要执行的 argv。
pub const TERMINAL_CANDIDATES: [(&str, &[&str]); 8] = [
    ("x-terminal-emulator", &["-e"]),
    ("gnome-terminal", &["--"]),
    ("kgx", &["--"]),
    ("konsole", &["-e"]),
    ("alacritty", &["-e"]),
    ("kitty", &[]),
    ("foot", &[]),
    ("xterm", &["-e"]),
];

/// 在 PATH 中挑选第一个可用终端，返回完整的 argv。
pub fn terminal_argv(cmd: &AurCommand) -> CoreResult<Vec<String>> {
    for (program, prefix) in TERMINAL_CANDIDATES {
        if crate::env::which(program).is_none() {
            continue;
        }
        let mut argv: Vec<String> = vec![program.to_string()];
        argv.extend(prefix.iter().map(|s| s.to_string()));
        argv.extend(cmd.argv());
        return Ok(argv);
    }
    Err(CoreError::Unsupported(
        "没有找到可用的终端模拟器（x-terminal-emulator/gnome-terminal/konsole/…）".into(),
    ))
}

/// 以分离进程启动终端执行 AUR 命令。
///
/// 返回启动的 argv，便于 UI 在失败提示里展示。
pub fn launch_in_terminal(cmd: &AurCommand) -> CoreResult<Vec<String>> {
    let argv = terminal_argv(cmd)?;
    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| CoreError::Internal("终端 argv 为空".into()))?;
    std::process::Command::new(program)
        .args(rest)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| CoreError::Io(format!("无法启动终端 {program}：{e}")))?;
    Ok(argv)
}

/// 复制到剪贴板的候选工具（Wayland 优先）。
pub const CLIPBOARD_CANDIDATES: [(&str, &[&str]); 4] = [
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["--clipboard", "--input"]),
    ("cliphist", &["store"]),
];

/// 把命令文本写入剪贴板；失败时返回 Err 由 UI 提示用户手动复制。
pub fn copy_to_clipboard(text: &str) -> CoreResult<()> {
    use std::io::Write;
    for (program, args) in CLIPBOARD_CANDIDATES {
        if crate::env::which(program).is_none() {
            continue;
        }
        let Ok(mut child) = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
        return Ok(());
    }
    Err(CoreError::Unsupported(
        "没有找到剪贴板工具（wl-copy/xclip/xsel）".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkgs() -> Vec<String> {
        vec!["cowsay".to_string()]
    }

    #[test]
    fn yay_install_uses_measured_flags() {
        let cmd = build_aur_command(AurHelperKind::Yay, AurOp::Install, &pkgs()).expect("cmd");
        assert_eq!(cmd.program, "yay");
        assert_eq!(
            cmd.args,
            vec![
                "-S",
                "--noconfirm",
                "--answerclean",
                "None",
                "--answerdiff",
                "None",
                "--sudoloop"
            ]
        );
        assert_eq!(cmd.argv().last().map(|s| s.as_str()), Some("cowsay"));
    }

    #[test]
    fn yay_remove_and_update_flags() {
        let rm = build_aur_command(AurHelperKind::Yay, AurOp::Remove, &pkgs()).expect("cmd");
        assert_eq!(rm.args, vec!["-Rns", "--noconfirm"]);
        let up = build_aur_command(AurHelperKind::Yay, AurOp::Update, &pkgs()).expect("cmd");
        assert_eq!(up.args, vec!["-Syu", "--noconfirm", "--devel"]);
    }

    #[test]
    fn paru_only_uses_guaranteed_flags() {
        // paru 未在本机安装，参数待验证：只允许 -S/-Rns/-Syu 与 --noconfirm
        for (op, expected) in [
            (AurOp::Install, vec!["-S", "--noconfirm"]),
            (AurOp::Remove, vec!["-Rns", "--noconfirm"]),
            (AurOp::Update, vec!["-Syu", "--noconfirm"]),
        ] {
            let cmd = build_aur_command(AurHelperKind::Paru, op, &pkgs()).expect("cmd");
            assert_eq!(cmd.program, "paru");
            assert_eq!(cmd.args, expected, "{op:?}");
        }
        assert!(!supports_cleanafter(AurHelperKind::Paru));
        assert!(supports_cleanafter(AurHelperKind::Yay));
    }

    #[test]
    fn package_names_are_validated_against_injection() {
        for bad in ["-rf", "a;rm -rf /", "a b", "a/b", "$(id)", "a|b", ""] {
            let err = build_aur_command(AurHelperKind::Yay, AurOp::Install, &[bad.to_string()])
                .expect_err("must reject");
            assert!(
                matches!(
                    err,
                    CoreError::InvalidName(_) | CoreError::PlanRejected { .. }
                ),
                "{bad:?} -> {err:?}"
            );
        }
    }

    #[test]
    fn empty_package_list_is_rejected() {
        let err = build_aur_command(AurHelperKind::Yay, AurOp::Install, &[]).expect_err("empty");
        assert!(matches!(err, CoreError::PlanRejected { .. }));
    }

    #[test]
    fn display_is_quoted_and_argv_is_not() {
        let cmd = AurCommand {
            program: "yay".into(),
            args: vec!["-S".into(), "--noconfirm".into()],
            packages: vec!["cowsay".into()],
        };
        assert_eq!(cmd.display(), "yay -S --noconfirm cowsay");
        assert_eq!(cmd.argv(), vec!["yay", "-S", "--noconfirm", "cowsay"]);
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("plain"), "plain");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn terminal_candidates_are_non_empty_and_unique() {
        let mut names: Vec<&str> = TERMINAL_CANDIDATES.iter().map(|(p, _)| *p).collect();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), before);
        assert!(names.iter().all(|n| !n.is_empty()));
        // 每个候选的 argv 形式必须是"程序 + 前缀 + 命令"，绝不经过 shell
        for (program, prefix) in TERMINAL_CANDIDATES {
            assert!(!program.contains(' '));
            assert!(prefix.iter().all(|p| !p.contains(' ') || *p == "--"));
        }
    }

    #[test]
    fn terminal_argv_never_uses_shell() {
        let cmd = AurCommand {
            program: "yay".into(),
            args: vec!["-S".into()],
            packages: vec!["cowsay".into()],
        };
        match terminal_argv(&cmd) {
            Ok(argv) => {
                assert!(argv.len() >= 4);
                assert!(!argv.iter().any(|a| a == "sh" || a == "-c"));
                assert!(argv.ends_with(&[
                    "yay".to_string(),
                    "-S".to_string(),
                    "cowsay".to_string()
                ]));
            }
            Err(e) => {
                // 无终端环境下也必须给出可操作的错误，而不是 panic
                assert!(e.user_message().contains("终端"));
            }
        }
    }
}
