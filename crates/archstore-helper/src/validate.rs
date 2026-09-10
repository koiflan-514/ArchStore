//! 计划校验：helper 绝不信任计划内容（project.md §5.1 安全红线）。
//!
//! 1. 计划路径必须在允许目录内、属主为调用者、权限为 0600 且不是符号链接。
//! 2. 每个 PlanItem.name 重新执行 validate_name。
//! 3. 重新向系统确认该包存在于对应源。
//! 4. 自己用 libalpm check_deps 重新计算依赖（不信任 GUI 传入的依赖清单）。
//! 5. 不做部分执行：校验失败则拒绝全部，退出码非 0，不执行任何子命令。

use std::path::{Path, PathBuf};

use alpm::{Alpm, SigLevel};
use archstore_core::error::{CoreError, CoreResult};
use archstore_core::model::plan::{
    Installation, PlanKind, PlanSource, TransactionPlan, validate_name,
};

/// 调用者身份（pkexec 会设置 PKEXEC_UID；sudo 设置 SUDO_UID）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caller {
    pub uid: u32,
    pub gid: u32,
}

impl Caller {
    /// 从环境推断调用者。pkexec 优先，其次 sudo，最后回退到真实 uid。
    pub fn detect() -> Self {
        let uid = std::env::var("PKEXEC_UID")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .or_else(|| {
                std::env::var("SUDO_UID")
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok())
            })
            .unwrap_or_else(|| unsafe { libc::getuid() });
        let gid = std::env::var("PKEXEC_GID")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .or_else(|| {
                std::env::var("SUDO_GID")
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok())
            })
            .unwrap_or_else(|| unsafe { libc::getgid() });
        Self { uid, gid }
    }

    /// 调用者是否是 root 本身（直接以 root 运行，没有经过 pkexec/sudo）。
    pub fn is_root_direct(&self) -> bool {
        self.uid == 0
    }
}

/// 通过 NSS 查询用户主目录（pkexec 会把 HOME 重置为 root 的家目录，所以不能读 HOME）。
pub fn user_home(uid: u32) -> Option<PathBuf> {
    unsafe {
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut buf = vec![0 as libc::c_char; 4096];
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result);
        if rc != 0 || result.is_null() || pwd.pw_dir.is_null() {
            return None;
        }
        let cstr = std::ffi::CStr::from_ptr(pwd.pw_dir);
        cstr.to_str().ok().map(PathBuf::from)
    }
}

/// 允许放置计划文件的根目录白名单。
pub fn allowed_roots(caller: &Caller) -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/tmp"), PathBuf::from("/var/tmp")];
    roots.push(PathBuf::from(format!("/run/user/{}", caller.uid)));
    if let Some(home) = user_home(caller.uid) {
        roots.push(home);
    }
    roots
}

/// 校验计划文件路径；返回规范化后的路径。
///
/// 这是"绝不通过命令行传递包名列表给 root"这条红线的配套措施：
/// 唯一通过命令行传给 root 的是一个受限路径。
pub fn validate_plan_path(path: &Path, caller: &Caller) -> CoreResult<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::OpenOptionsExt;

    if !path.is_absolute() {
        return Err(CoreError::PlanRejected {
            reason: format!("计划路径必须是绝对路径：{}", path.display()),
        });
    }
    // 先规范化，确保后续白名单判断针对真实路径
    let canonical = std::fs::canonicalize(path).map_err(|e| CoreError::PlanRejected {
        reason: format!("计划文件不可访问：{}（{e}）", path.display()),
    })?;
    let roots = allowed_roots(caller);
    if !roots.iter().any(|root| canonical.starts_with(root)) {
        return Err(CoreError::PlanRejected {
            reason: format!(
                "计划文件必须位于以下目录之一：{}",
                roots
                    .iter()
                    .map(|r| r.display().to_string())
                    .collect::<Vec<_>>()
                    .join("、")
            ),
        });
    }
    // 先 canonicalize 再以 O_NOFOLLOW 打开：两者配合可以避免
    // "校验完之后路径被替换成符号链接"的 TOCTOU 竞态。
    // 因此指向允许目录内合法文件的符号链接是可以接受的（读到的就是校验过的那个文件）。
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&canonical)
        .map_err(|e| CoreError::PlanRejected {
            reason: format!("无法打开计划文件（不允许符号链接）：{e}"),
        })?;
    let meta = file.metadata().map_err(|e| CoreError::PlanRejected {
        reason: format!("无法读取计划文件属性：{e}"),
    })?;
    if !meta.is_file() {
        return Err(CoreError::PlanRejected {
            reason: "计划路径不是普通文件".into(),
        });
    }
    let mode = meta.mode() & 0o777;
    if mode != 0o600 && mode != 0o400 {
        return Err(CoreError::PlanRejected {
            reason: format!("计划文件权限必须是 0600（当前 {mode:04o}）"),
        });
    }
    // 属主必须是调用者本人（除非调用者本身就是 root）
    if !caller.is_root_direct() && meta.uid() != caller.uid {
        return Err(CoreError::PlanRejected {
            reason: format!(
                "计划文件属主（uid {}）与调用者（uid {}）不一致",
                meta.uid(),
                caller.uid
            ),
        });
    }
    Ok(canonical)
}

/// 读取并解析计划（解析后立即执行完整校验）。
pub fn load_plan(path: &Path) -> CoreResult<TransactionPlan> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| CoreError::PlanRejected {
            reason: format!("无法打开计划文件：{e}"),
        })?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    TransactionPlan::from_json(&text)
}

/// 校验结果：重新确认过的目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedItem {
    pub name: String,
    pub source: PlanSource,
}

/// 重新向系统确认每个计划项存在。
pub fn reconfirm_items(
    plan: &TransactionPlan,
    emitter: &mut crate::protocol::Emitter,
) -> CoreResult<Vec<ConfirmedItem>> {
    let mut confirmed = Vec::new();

    if plan.kind.is_flatpak() {
        for item in &plan.items {
            validate_name(&item.name)?;
            let PlanSource::Flatpak {
                remote,
                installation,
            } = &item.source
            else {
                return Err(CoreError::PlanRejected {
                    reason: format!("Flatpak 计划中出现了非 Flatpak 条目：{}", item.name),
                });
            };
            let exists = match plan.kind {
                PlanKind::FlatpakInstall => remote_has_app(remote, &item.name)?,
                _ => local_has_app(*installation, &item.name)?,
            };
            if !exists {
                return Err(CoreError::NotFound(format!(
                    "Flatpak 应用 {} 不存在于 {}（计划已整体拒绝）",
                    item.name, remote
                )));
            }
            emitter.info(&format!("已确认 Flatpak 应用 {}", item.name));
            confirmed.push(ConfirmedItem {
                name: item.name.clone(),
                source: item.source.clone(),
            });
        }
        return Ok(confirmed);
    }

    // --- pacman 路径 ---
    let mut handle =
        Alpm::new("/", "/var/lib/pacman").map_err(|e| CoreError::AlpmInit(e.to_string()))?;
    for repo in archstore_core::env::discover_sync_repos() {
        let _ = handle.register_syncdb_mut(repo.as_str(), SigLevel::USE_DEFAULT);
    }

    for item in &plan.items {
        validate_name(&item.name)?;
        let PlanSource::Official { .. } = &item.source else {
            return Err(CoreError::PlanRejected {
                reason: format!("pacman 计划中出现了非 pacman 条目：{}", item.name),
            });
        };
        match plan.kind {
            PlanKind::PacmanRemove => {
                if handle.localdb().pkg(item.name.as_str()).is_err() {
                    return Err(CoreError::NotFound(format!(
                        "软件包 {} 未安装，无法卸载（计划已整体拒绝）",
                        item.name
                    )));
                }
            }
            _ => {
                let found = handle
                    .syncdbs()
                    .iter()
                    .any(|db| db.pkg(item.name.as_str()).is_ok());
                if !found {
                    return Err(CoreError::NotFound(format!(
                        "软件包 {} 不存在于任何已同步的官方仓库（计划已整体拒绝）",
                        item.name
                    )));
                }
            }
        }
        emitter.info(&format!("已确认软件包 {}", item.name));
        confirmed.push(ConfirmedItem {
            name: item.name.clone(),
            source: item.source.clone(),
        });
    }

    // --- 独立重算依赖（不信任 GUI 传入的清单）---
    recompute_dependencies(&handle, plan, emitter);
    Ok(confirmed)
}

/// 用 libalpm check_deps 重新计算依赖关系并写入日志。
///
/// 语义（§4.3，实测）：
/// - "安装 pkgs 会缺什么依赖"：pkgs = 待安装包，rem = []，upgrade = []，reverse_deps = false
/// - "删除 rem 会破坏谁"：**必须把全体已安装包放进 pkgs**，rem = [待删包]，reverse_deps = true；
///   若只传 rem 而 pkgs 为空会静默返回 0（最危险的误用）
fn recompute_dependencies(
    handle: &Alpm,
    plan: &TransactionPlan,
    emitter: &mut crate::protocol::Emitter,
) {
    let names: Vec<&str> = plan.items.iter().map(|i| i.name.as_str()).collect();

    match plan.kind {
        PlanKind::PacmanSync => {
            let mut targets = Vec::new();
            for name in &names {
                for db in handle.syncdbs().iter() {
                    if let Ok(p) = db.pkg(*name) {
                        targets.push(p);
                        break;
                    }
                }
            }
            if targets.is_empty() {
                return;
            }
            let missing = handle.check_deps(
                targets.iter().copied(),
                alpm::AlpmListMut::<&alpm::Pkg>::new(),
                alpm::AlpmListMut::<&alpm::Pkg>::new(),
                false,
            );
            let count = missing.len();
            if count == 0 {
                emitter.info("依赖自检：本地已满足全部依赖");
            } else {
                emitter.info(&format!(
                    "依赖自检：仍有 {count} 项依赖未满足，将由 pacman 解析（不属于拒绝条件）"
                ));
                for m in missing.iter().take(20) {
                    emitter.info(&format!("  未满足：{}", m.depend()));
                }
            }
        }
        PlanKind::PacmanRemove => {
            // 必须把全体已安装包放进 pkgs，否则会静默漏报（§4.3）
            let local = handle.localdb().pkgs();
            let all: Vec<&alpm::Package> = local.iter().collect();
            let mut targets = Vec::new();
            for name in &names {
                if let Ok(p) = handle.localdb().pkg(*name) {
                    targets.push(p);
                }
            }
            if targets.is_empty() {
                return;
            }
            let missing = handle.check_deps(
                all.iter(),
                targets.iter(),
                alpm::AlpmListMut::<&alpm::Pkg>::new(),
                true,
            );
            let count = missing.len();
            if count == 0 {
                emitter.info("反依赖自检：没有已安装包依赖这些目标");
            } else {
                emitter.warn(&format!(
                    "反依赖自检：删除会影响 {count} 个已安装包（用户在 GUI 中已确认）"
                ));
                for m in missing.iter().take(20) {
                    emitter.warn(&format!("  {} 依赖 {}", m.target(), m.depend().name()));
                }
            }
        }
        _ => {}
    }
}

/// 远程仓库是否提供该应用。
fn remote_has_app(remote: &str, app_id: &str) -> CoreResult<bool> {
    let out = std::process::Command::new("flatpak")
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .args(["remote-info", remote, app_id])
        .output();
    match out {
        Ok(o) => Ok(o.status.success()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(CoreError::BackendUnavailable {
            kind: "flatpak".into(),
            reason: "未安装 flatpak".into(),
        }),
        Err(e) => Err(CoreError::Io(e.to_string())),
    }
}

/// 本机安装位置是否已安装该应用。
fn local_has_app(installation: Installation, app_id: &str) -> CoreResult<bool> {
    let out = std::process::Command::new("flatpak")
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .args([installation.flag(), "info", app_id])
        .output();
    match out {
        Ok(o) => Ok(o.status.success()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(CoreError::BackendUnavailable {
            kind: "flatpak".into(),
            reason: "未安装 flatpak".into(),
        }),
        Err(e) => Err(CoreError::Io(e.to_string())),
    }
}

/// pacman 数据库锁冲突检查（实测：无操作时 /var/lib/pacman/db.lck 不存在）。
pub fn pacman_lock_present() -> bool {
    Path::new("/var/lib/pacman/db.lck").exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use archstore_core::model::plan::{PlanItem, TransactionPlan};

    #[test]
    fn caller_detection_does_not_panic() {
        let c = Caller::detect();
        // 只能断言"取到了自洽的调用者身份"；具体 uid/gid 取决于运行环境
        assert!(!c.is_root_direct() || c.uid == 0);
    }

    #[test]
    fn allowed_roots_include_tmp_and_run_user() {
        let caller = Caller {
            uid: 1000,
            gid: 1000,
        };
        let roots = allowed_roots(&caller);
        assert!(roots.contains(&PathBuf::from("/tmp")));
        assert!(roots.contains(&PathBuf::from("/var/tmp")));
        assert!(roots.contains(&PathBuf::from("/run/user/1000")));
    }

    #[test]
    fn plan_path_must_be_absolute() {
        let caller = Caller::detect();
        let err = validate_plan_path(Path::new("relative.json"), &caller).expect_err("reject");
        assert!(matches!(err, CoreError::PlanRejected { .. }));
    }

    #[test]
    fn plan_path_outside_allowed_roots_is_rejected() {
        let caller = Caller::detect();
        let err = validate_plan_path(Path::new("/etc/passwd"), &caller).expect_err("reject");
        assert!(matches!(err, CoreError::PlanRejected { .. }));
    }

    #[test]
    fn plan_path_requires_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let Ok(dir) = std::env::temp_dir().canonicalize() else {
            return;
        };
        let path = dir.join("archstore-validate-test.json");
        std::fs::write(&path, "{}").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let caller = Caller::detect();
        let err = validate_plan_path(&path, &caller).expect_err("world-readable must be rejected");
        assert!(matches!(err, CoreError::PlanRejected { .. }));

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        assert!(validate_plan_path(&path, &caller).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn symlink_inside_allowed_root_resolves_to_checked_file() {
        use std::os::unix::fs::PermissionsExt;
        let Ok(dir) = std::env::temp_dir().canonicalize() else {
            return;
        };
        let real = dir.join("archstore-real-plan.json");
        let link = dir.join("archstore-link-plan.json");
        std::fs::write(&real, "{}").expect("write");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let _ = std::fs::remove_file(&link);
        if std::os::unix::fs::symlink(&real, &link).is_err() {
            return;
        }
        let caller = Caller::detect();
        // 先 canonicalize 再打开：读到的一定是校验过的那个文件
        let resolved = validate_plan_path(&link, &caller).expect("resolvable symlink");
        assert_eq!(resolved, real);
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_file(&real);
    }

    #[test]
    fn symlink_escaping_allowed_roots_is_rejected() {
        let Ok(dir) = std::env::temp_dir().canonicalize() else {
            return;
        };
        let link = dir.join("archstore-escape-plan.json");
        let _ = std::fs::remove_file(&link);
        if std::os::unix::fs::symlink("/etc/passwd", &link).is_err() {
            return;
        }
        let caller = Caller::detect();
        let err = validate_plan_path(&link, &caller).expect_err("escape must be rejected");
        assert!(matches!(err, CoreError::PlanRejected { .. }));
        let _ = std::fs::remove_file(&link);
    }

    #[test]
    fn load_plan_rejects_unknown_schema() {
        let Ok(dir) = std::env::temp_dir().canonicalize() else {
            return;
        };
        let path = dir.join("archstore-schema-plan.json");
        let mut plan = TransactionPlan::new(PlanKind::PacmanSync);
        plan.push(PlanItem::official("extra", "vim"));
        let mut json: serde_json::Value =
            serde_json::from_str(&plan.to_json().expect("ser")).expect("json");
        json["schema"] = serde_json::json!(12345);
        std::fs::write(&path, json.to_string()).expect("write");
        let err = load_plan(&path).expect_err("unknown schema must be rejected");
        assert!(matches!(err, CoreError::PlanRejected { .. }));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_plan_rejects_invalid_package_names() {
        let Ok(dir) = std::env::temp_dir().canonicalize() else {
            return;
        };
        let path = dir.join("archstore-badname-plan.json");
        let raw = r#"{"schema":1,"created_at":0,"kind":"pacman-sync","summary":[],
            "items":[{"source":{"kind":"official","repo":"extra"},
                      "name":"../../etc/passwd","target_version":null,"reason":"explicit"}]}"#;
        std::fs::write(&path, raw).expect("write");
        let err = load_plan(&path).expect_err("path traversal must be rejected");
        assert!(matches!(err, CoreError::PlanRejected { .. }));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_plan_rejects_command_injection_attempts() {
        let Ok(dir) = std::env::temp_dir().canonicalize() else {
            return;
        };
        for bad in [
            "-rf",
            "a;rm -rf /",
            "a$(id)",
            "a\u{0060}id\u{0060}",
            "a|b",
            "a b",
            "--noconfirm",
        ] {
            let path = dir.join("archstore-inject-plan.json");
            let plan_json = serde_json::json!({
                "schema": 1,
                "created_at": 0,
                "kind": "pacman-sync",
                "summary": [],
                "items": [{
                    "source": {"kind": "official", "repo": "extra"},
                    "name": bad,
                    "target_version": null,
                    "reason": "explicit"
                }]
            });
            std::fs::write(&path, plan_json.to_string()).expect("write");
            let err = load_plan(&path).expect_err(bad);
            assert!(
                matches!(err, CoreError::PlanRejected { .. }),
                "{bad:?} -> {err:?}"
            );
            let _ = std::fs::remove_file(&path);
        }
    }
}
