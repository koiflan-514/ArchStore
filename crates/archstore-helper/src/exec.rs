//! 执行引擎：构造参数数组、流式读取子进程输出、软映射阶段进度（project.md §5.2 / §5.5）。
//!
//! 安全红线：
//! - 绝不把用户输入拼进 shell 字符串，也不使用 sh -c；所有子进程用参数数组调用。
//! - 包名已通过白名单校验，并且在参数数组前再加一个 "--" 结束选项解析（双保险）。
//! - helper 只执行 system 级别的 Flatpak 操作；user 级别由 GUI 以普通用户执行。

use std::io::{BufReader, Read};
use std::process::{Command, Stdio};

use archstore_core::error::{CoreError, CoreResult};
use archstore_core::model::plan::{Installation, PlanKind, TransactionPlan};

use crate::protocol::{Emitter, codes};

/// 一条待执行的命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// 完整参数数组（argv[0] 是程序名）
    pub argv: Vec<String>,
    /// 面向用户的动作说明
    pub label: String,
}

impl Step {
    fn new(argv: Vec<String>, label: impl Into<String>) -> Self {
        Self {
            argv,
            label: label.into(),
        }
    }

    /// 供日志展示（不用于执行）。
    pub fn display(&self) -> String {
        self.argv.join(" ")
    }
}

/// 根据计划构造执行步骤。
///
/// MVP 不做部分执行：只要有一个条目有问题，前面就已经在 validate 阶段整体拒绝了。
pub fn build_steps(plan: &TransactionPlan) -> CoreResult<Vec<Step>> {
    // 先做计划级校验（非空、schema、包名、来源一致性），再构造参数
    plan.validate()?;
    let mut steps: Vec<Step> = Vec::new();

    match plan.kind {
        PlanKind::PacmanSync => {
            let mut argv = vec![
                "pacman".to_string(),
                "-S".to_string(),
                "--noconfirm".to_string(),
                "--needed".to_string(),
                "--".to_string(),
            ];
            argv.extend(plan.items.iter().map(|i| i.name.clone()));
            steps.push(Step::new(argv, "安装 / 更新官方仓库软件"));
        }
        PlanKind::PacmanRemove => {
            let mut argv = vec![
                "pacman".to_string(),
                // -Rns：删除包及其不再需要的依赖与配置（清理范围已在计划中展示）
                "-Rns".to_string(),
                "--noconfirm".to_string(),
                "--".to_string(),
            ];
            argv.extend(plan.items.iter().map(|i| i.name.clone()));
            steps.push(Step::new(argv, "卸载官方仓库软件"));
        }
        PlanKind::FlatpakInstall => {
            for item in &plan.items {
                let (remote, installation) = flatpak_target(item)?;
                steps.push(Step::new(
                    vec![
                        "flatpak".to_string(),
                        installation.flag().to_string(),
                        "install".to_string(),
                        "-y".to_string(),
                        remote,
                        item.name.clone(),
                    ],
                    format!("安装 Flatpak 应用 {}", item.name),
                ));
            }
        }
        PlanKind::FlatpakUninstall => {
            for item in &plan.items {
                let (_, installation) = flatpak_target(item)?;
                steps.push(Step::new(
                    vec![
                        "flatpak".to_string(),
                        installation.flag().to_string(),
                        "uninstall".to_string(),
                        "-y".to_string(),
                        item.name.clone(),
                    ],
                    format!("卸载 Flatpak 应用 {}", item.name),
                ));
            }
        }
        PlanKind::FlatpakUpdate => {
            for item in &plan.items {
                let (_, installation) = flatpak_target(item)?;
                steps.push(Step::new(
                    vec![
                        "flatpak".to_string(),
                        installation.flag().to_string(),
                        "update".to_string(),
                        "-y".to_string(),
                        item.name.clone(),
                    ],
                    format!("更新 Flatpak 应用 {}", item.name),
                ));
            }
        }
    }

    if steps.is_empty() {
        return Err(CoreError::PlanRejected {
            reason: "计划没有可执行的步骤".into(),
        });
    }
    Ok(steps)
}

/// 取出 Flatpak 目标，并拒绝 user 级别的提权执行。
fn flatpak_target(
    item: &archstore_core::model::plan::PlanItem,
) -> CoreResult<(String, Installation)> {
    let archstore_core::model::plan::PlanSource::Flatpak {
        remote,
        installation,
    } = &item.source
    else {
        return Err(CoreError::PlanRejected {
            reason: format!("非 Flatpak 条目出现在 Flatpak 计划中：{}", item.name),
        });
    };
    if *installation == Installation::User {
        return Err(CoreError::PlanRejected {
            reason: "用户级 Flatpak 操作不需要提权，应由 GUI 以普通用户身份执行".into(),
        });
    }
    Ok((remote.clone(), *installation))
}

/// 一次执行的统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExecStats {
    pub installed: usize,
    pub removed: usize,
    pub failed: usize,
}

/// 顺序执行所有步骤，逐行输出 JSON 事件。
pub fn execute(
    plan: &TransactionPlan,
    steps: &[Step],
    emitter: &mut Emitter,
) -> CoreResult<ExecStats> {
    let mut stats = ExecStats::default();
    let total = steps.len();

    for (i, step) in steps.iter().enumerate() {
        emitter.info(&format!("[{}/{}] {}", i + 1, total, step.label));
        emitter.info(&format!("执行：{}", step.display()));
        let code = run_step(step, emitter)?;
        match code {
            0 => match plan.kind {
                PlanKind::PacmanRemove | PlanKind::FlatpakUninstall => stats.removed += 1,
                _ => stats.installed += 1,
            },
            other => {
                // 失败即中止（不做部分执行）：统计值不再有意义，直接返回错误
                return Err(CoreError::TransactionFailed {
                    code: other,
                    log_tail: emitter.log_tail(40),
                });
            }
        }
    }
    Ok(stats)
}

/// 运行单条命令并流式转发输出。
fn run_step(step: &Step, emitter: &mut Emitter) -> CoreResult<i32> {
    let Some((program, args)) = step.argv.split_first() else {
        return Err(CoreError::Internal("空的 argv".into()));
    };

    let mut cmd = Command::new(program);
    cmd.args(args)
        // LC_ALL=C：让输出可预期（与 flatpak 解析策略一致）
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("SYSTEMD_PAGER", "")
        .env("PAGER", "cat")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CoreError::BackendUnavailable {
                kind: program.clone(),
                reason: format!("找不到可执行文件 {program}"),
            }
        } else {
            CoreError::Io(format!("无法启动 {program}：{e}"))
        }
    })?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // stderr 在独立线程读取，避免管道写满导致死锁
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let stderr_thread = stderr.map(|err| {
        std::thread::spawn(move || {
            for line in LineStream::new(err) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        })
    });

    if let Some(out) = stdout {
        for line in LineStream::new(out) {
            emit_output_line(emitter, &line);
        }
    }

    if let Some(h) = stderr_thread {
        let _ = h.join();
    }
    for line in rx.try_iter() {
        emit_output_line(emitter, &line);
    }

    let status = child
        .wait()
        .map_err(|e| CoreError::Io(format!("等待 {program} 失败：{e}")))?;
    let code = status.code().unwrap_or(-1);
    if code != 0 {
        let tail = emitter.log_tail(20);
        // 锁冲突与 PGP 密钥是两类需要特殊文案的失败
        if tail.contains("unable to lock database") || tail.contains("db.lck") {
            emitter.error(codes::LOCKED, "数据库被锁定：另一个包管理器正在运行");
        } else if tail.contains("unknown public key")
            || tail.contains("signature from")
            || tail.contains("invalid or corrupted package")
        {
            emitter.needs_tty(
                "需要导入 PGP 密钥：请在本机终端运行 sudo pacman-key --init && sudo pacman-key --populate archlinux",
            );
        } else {
            emitter.error(
                codes::TRANSACTION_FAILED,
                &format!("{program} 以退出码 {code} 结束"),
            );
        }
    }
    Ok(code)
}

/// 把一行子进程输出转成事件（软映射阶段，不做正则强解析）。
fn emit_output_line(emitter: &mut Emitter, line: &str) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    if let Some((phase, percent, detail)) = parse_progress(trimmed) {
        emitter.progress(phase, percent, &detail);
    }
    emitter.log("info", trimmed);
}

/// 阶段软映射：包含即映射，映射失败就当普通日志（§5.5）。
pub fn phase_of(line: &str) -> Option<&'static str> {
    let l = line.to_ascii_lowercase();
    let table: [(&str, &'static str); 10] = [
        ("downloading", "download"),
        ("下载", "download"),
        ("checking keys", "keyring"),
        ("checking keyring", "keyring"),
        ("checking package integrity", "verify"),
        ("loading package files", "verify"),
        ("checking for file conflicts", "conflict"),
        ("installing", "install"),
        ("upgrading", "upgrade"),
        ("removing", "remove"),
    ];
    table.iter().find(|(k, _)| l.contains(k)).map(|(_, v)| *v)
}

/// 从一行输出中提取 (阶段, 百分比, 细节)。
pub fn parse_progress(line: &str) -> Option<(&'static str, Option<u8>, String)> {
    let phase = phase_of(line)?;
    Some((phase, extract_percent(line), line.to_string()))
}

/// 提取形如 "42%" 或 "[####] 100%" 的百分比（软解析，取最后一个）。
pub fn extract_percent(line: &str) -> Option<u8> {
    let bytes = line.as_bytes();
    let mut found = None;
    for i in 0..bytes.len() {
        if bytes[i] != b'%' {
            continue;
        }
        let mut j = i;
        while j > 0 && bytes[j - 1].is_ascii_digit() {
            j -= 1;
        }
        if j == i {
            continue;
        }
        if let Ok(v) = line[j..i].parse::<u32>() {
            found = Some(v.min(100) as u8);
        }
    }
    found
}

/// 按行（同时以 \n 与 \r 分隔）读取子进程输出。
///
/// pacman 的下载进度条用 \r 刷新而不换行，只用 BufRead::lines() 会长时间收不到进度。
pub struct LineStream<R: Read> {
    reader: BufReader<R>,
    buf: Vec<u8>,
    eof: bool,
}

impl<R: Read> LineStream<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            buf: Vec::new(),
            eof: false,
        }
    }
}

impl<R: Read> Iterator for LineStream<R> {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        loop {
            if let Some(pos) = self.buf.iter().position(|b| *b == b'\n' || *b == b'\r') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                let text = String::from_utf8_lossy(&line[..line.len() - 1])
                    .trim()
                    .to_string();
                if text.is_empty() {
                    continue;
                }
                return Some(text);
            }
            if self.eof {
                if self.buf.is_empty() {
                    return None;
                }
                let line = std::mem::take(&mut self.buf);
                let text = String::from_utf8_lossy(&line).trim().to_string();
                if text.is_empty() {
                    return None;
                }
                return Some(text);
            }
            let mut chunk = [0u8; 4096];
            match self.reader.read(&mut chunk) {
                Ok(0) => self.eof = true,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(_) => self.eof = true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use archstore_core::model::plan::{Installation, PlanItem};

    fn plan_of(kind: PlanKind, items: Vec<PlanItem>) -> TransactionPlan {
        let mut p = TransactionPlan::new(kind);
        p.items = items;
        p
    }

    #[test]
    fn pacman_sync_uses_argv_and_end_of_options() {
        let plan = plan_of(
            PlanKind::PacmanSync,
            vec![
                PlanItem::official("extra", "firefox"),
                PlanItem::official("extra", "vim"),
            ],
        );
        let steps = build_steps(&plan).expect("steps");
        assert_eq!(steps.len(), 1);
        let argv = &steps[0].argv;
        assert_eq!(argv[0], "pacman");
        assert!(argv.contains(&"-S".to_string()));
        assert!(argv.contains(&"--noconfirm".to_string()));
        let sep = argv
            .iter()
            .position(|a| a == "--")
            .expect("必须用 -- 结束选项解析");
        assert_eq!(sep, 4, "-- 必须紧跟在 pacman 选项之后");
        assert!(
            argv[sep + 1..].iter().all(|a| !a.starts_with('-')),
            "包名必须全部出现在 -- 之后"
        );
        assert_eq!(argv.last().map(|s| s.as_str()), Some("vim"));
        // 绝不出现 shell
        assert!(!argv.iter().any(|a| a == "sh" || a == "-c"));
    }

    #[test]
    fn pacman_remove_uses_rns() {
        let plan = plan_of(
            PlanKind::PacmanRemove,
            vec![PlanItem::official("extra", "vim")],
        );
        let steps = build_steps(&plan).expect("steps");
        assert_eq!(steps[0].argv[1], "-Rns");
        assert_eq!(steps[0].argv.last().map(|s| s.as_str()), Some("vim"));
    }

    #[test]
    fn flatpak_system_install_passes_remote_then_app() {
        let plan = plan_of(
            PlanKind::FlatpakInstall,
            vec![PlanItem::flatpak(
                "flathub",
                Installation::System,
                "org.mozilla.firefox",
            )],
        );
        let steps = build_steps(&plan).expect("steps");
        assert_eq!(
            steps[0].argv,
            vec![
                "flatpak",
                "--system",
                "install",
                "-y",
                "flathub",
                "org.mozilla.firefox"
            ]
        );
    }

    #[test]
    fn flatpak_uninstall_and_update_omit_remote() {
        let plan = plan_of(
            PlanKind::FlatpakUninstall,
            vec![PlanItem::flatpak(
                "flathub",
                Installation::System,
                "org.mozilla.firefox",
            )],
        );
        assert_eq!(
            build_steps(&plan).expect("steps")[0].argv,
            vec![
                "flatpak",
                "--system",
                "uninstall",
                "-y",
                "org.mozilla.firefox"
            ]
        );

        let plan = plan_of(
            PlanKind::FlatpakUpdate,
            vec![PlanItem::flatpak(
                "flathub",
                Installation::System,
                "org.mozilla.firefox",
            )],
        );
        assert_eq!(
            build_steps(&plan).expect("steps")[0].argv,
            vec!["flatpak", "--system", "update", "-y", "org.mozilla.firefox"]
        );
    }

    #[test]
    fn flatpak_user_scope_is_rejected_by_helper() {
        let plan = plan_of(
            PlanKind::FlatpakInstall,
            vec![PlanItem::flatpak(
                "flathub",
                Installation::User,
                "org.mozilla.firefox",
            )],
        );
        let err = build_steps(&plan).expect_err("user scope must not go through the helper");
        assert!(matches!(err, CoreError::PlanRejected { .. }));
    }

    #[test]
    fn empty_plan_has_no_steps() {
        let plan = plan_of(PlanKind::PacmanSync, Vec::new());
        assert!(build_steps(&plan).is_err());
    }

    #[test]
    fn phase_soft_mapping() {
        assert_eq!(phase_of("downloading firefox-155.0.1"), Some("download"));
        assert_eq!(phase_of("正在下载 firefox"), Some("download"));
        assert_eq!(phase_of("(1/3) installing foo"), Some("install"));
        assert_eq!(phase_of("checking keyring..."), Some("keyring"));
        assert_eq!(phase_of("upgrading bar"), Some("upgrade"));
        assert_eq!(phase_of(":: 正在解析依赖关系..."), None);
    }

    #[test]
    fn percent_extraction() {
        assert_eq!(extract_percent("firefox 1.2 MiB [####] 100%"), Some(100));
        assert_eq!(extract_percent("42% done"), Some(42));
        assert_eq!(extract_percent("no percent here"), None);
        assert_eq!(extract_percent("999%"), Some(100), "越界值需收敛");
    }

    #[test]
    fn line_stream_splits_on_cr_and_lf() {
        let data = b"one\ntwo\rthree\r\nfour";
        let lines: Vec<String> = LineStream::new(&data[..]).collect();
        assert_eq!(lines, vec!["one", "two", "three", "four"]);
    }

    #[test]
    fn line_stream_skips_empty_lines() {
        let data = b"\n\n\na\n\n";
        let lines: Vec<String> = LineStream::new(&data[..]).collect();
        assert_eq!(lines, vec!["a"]);
    }

    #[test]
    fn line_stream_handles_utf8_across_chunks() {
        let text = "中文输出\n第二行\n";
        let bytes = text.as_bytes();
        // 以 3 字节为单位分块读取，模拟被切断的多字节字符
        struct Chunky<'a> {
            data: &'a [u8],
            pos: usize,
            step: usize,
        }
        impl Read for Chunky<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let end = (self.pos + self.step).min(self.data.len());
                let n = end - self.pos;
                buf[..n].copy_from_slice(&self.data[self.pos..end]);
                self.pos = end;
                Ok(n)
            }
        }
        let reader = Chunky {
            data: bytes,
            pos: 0,
            step: 3,
        };
        let lines: Vec<String> = LineStream::new(reader).collect();
        assert_eq!(lines, vec!["中文输出", "第二行"]);
    }
}
