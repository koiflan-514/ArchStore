//! 统一错误类型（见 project.md 附录 C）。
//!
//! 原则：错误必须携带足够的上下文让用户自己修复（URL、路径、退出码、原始输出片段），
//! 而不是"出错了"。

use std::fmt;

/// crate 内统一使用的 Result 别名。
pub type CoreResult<T> = Result<T, CoreError>;

/// ArchStore 核心错误。
///
/// 每个变体都对应附录 C 中的一行，并通过 user_message() 给出面向用户的中文文案。
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// 非 Arch 系统、gtk/adw 版本不足。
    #[error("环境不受支持：{0}")]
    EnvUnsupported(String),

    /// Alpm::new 失败。
    #[error("libalpm 初始化失败：{0}")]
    AlpmInit(String),

    /// 同步库缺失/为空。
    #[error("同步数据库不可用：{0}")]
    SyncDbMissing(String),

    /// 后端能力探测失败。
    #[error("后端 {kind} 不可用：{reason}")]
    BackendUnavailable { kind: String, reason: String },

    /// 请求失败。
    #[error("网络请求失败：{url}（{cause}）")]
    Network { url: String, cause: String },

    /// 超时。
    #[error("请求超时（{secs} 秒）：{url}")]
    Timeout { url: String, secs: u64 },

    /// 429 / 503。
    #[error("请求过于频繁，{retry_after} 秒后自动重试")]
    RateLimited { retry_after: u64 },

    /// 子进程 / JSON / 文本解析失败。
    #[error("无法识别 {context} 的输出：{raw_head}")]
    Parse { context: String, raw_head: String },

    /// 包名不合规。
    #[error("包名不合规：{0}")]
    InvalidName(String),

    /// helper 拒绝计划。
    #[error("计划被拒绝：{reason}")]
    PlanRejected { reason: String },

    /// /var/lib/pacman/db.lck 存在。
    #[error("数据库被锁定：另一个包管理器正在运行")]
    Locked,

    /// 卸载会破坏依赖。
    #[error("卸载 {target} 会破坏 {count} 个已安装包的依赖")]
    ReverseDeps {
        target: String,
        dependents: Vec<String>,
        count: usize,
    },

    /// polkit 拒绝/取消。
    #[error("已取消授权")]
    AuthDenied,

    /// helper 返回非 0。
    #[error("事务失败（退出码 {code}）：{log_tail}")]
    TransactionFailed { code: i32, log_tail: String },

    /// 配置解析/写入失败。
    #[error("配置错误：{0}")]
    Config(String),

    /// 缓存读写失败（缓存故障永不应导致功能失效，因此调用方通常降级处理）。
    #[error("缓存错误：{0}")]
    Cache(String),

    /// 文件系统错误。
    #[error("文件读写失败：{0}")]
    Io(String),

    /// 未找到。
    #[error("未找到：{0}")]
    NotFound(String),

    /// 后端明确不支持该操作。
    #[error("该操作不受支持：{0}")]
    Unsupported(String),

    /// 用户主动取消（请求被丢弃）。
    #[error("操作已取消")]
    Cancelled,

    /// 内部一致性错误（不应出现在正常路径）。
    #[error("内部错误：{0}")]
    Internal(String),
}

impl CoreError {
    /// 面向用户的一句话说明（UI 直接展示，不含英文技术细节）。
    pub fn user_message(&self) -> String {
        match self {
            CoreError::EnvUnsupported(what) => format!("当前环境不受支持：{what}"),
            CoreError::AlpmInit(why) => {
                format!("无法读取本地软件包数据库（{why}）。官方仓库相关功能不可用。")
            }
            CoreError::SyncDbMissing(name) => {
                format!("仓库 {name} 未同步，请先运行 sudo pacman -Sy（本程序不代为执行）")
            }
            CoreError::BackendUnavailable { kind, reason } => {
                format!("{kind} 后端不可用：{reason}")
            }
            CoreError::Network { url, cause } => {
                format!("无法连接 {url}（{cause}）。已显示缓存数据（如有）。")
            }
            CoreError::Timeout { url, secs } => format!("请求超时（{secs} 秒）：{url}"),
            CoreError::RateLimited { retry_after } => {
                format!("请求过于频繁，{retry_after} 秒后自动重试")
            }
            CoreError::Parse { context, .. } => format!("无法识别 {context} 的输出格式"),
            CoreError::InvalidName(name) => format!("包名 {name:?} 不合规"),
            CoreError::PlanRejected { reason } => format!("计划被拒绝：{reason}"),
            CoreError::Locked => {
                "另一个包管理器正在运行（可能是 pacman 或另一个更新器），请稍后重试".to_string()
            }
            CoreError::ReverseDeps { target, count, .. } => {
                format!("删除 {target} 会影响 {count} 个已安装包")
            }
            CoreError::AuthDenied => "已取消授权，计划已保留".to_string(),
            CoreError::TransactionFailed { code, .. } => format!("事务执行失败（退出码 {code}）"),
            CoreError::Config(why) => format!("配置错误：{why}"),
            CoreError::Cache(why) => format!("缓存错误：{why}"),
            CoreError::Io(why) => format!("文件读写失败：{why}"),
            CoreError::NotFound(what) => format!("未找到 {what}"),
            CoreError::Unsupported(what) => format!("该操作不受支持：{what}"),
            CoreError::Cancelled => "操作已取消".to_string(),
            CoreError::Internal(why) => format!("内部错误：{why}"),
        }
    }

    /// 可选的修复建议（UI 的 ErrorView 中作为第二行显示）。
    pub fn fix_hint(&self) -> Option<String> {
        match self {
            CoreError::AlpmInit(_) => Some(
                "确认 /var/lib/pacman 存在且可读；若在容器中运行，请挂载宿主机的 pacman 数据库。"
                    .into(),
            ),
            CoreError::SyncDbMissing(_) => Some(
                "运行 sudo pacman -Sy 后点击「重新检测」。Arch 不建议部分升级，请随后完整升级。"
                    .into(),
            ),
            CoreError::Network { .. } | CoreError::Timeout { .. } => {
                Some("检查网络连接与代理设置（设置 → 网络），或稍后重试。".into())
            }
            CoreError::Locked => Some(
                "等待另一个包管理器结束。若确认没有包管理器在运行，可删除 /var/lib/pacman/db.lck。"
                    .into(),
            ),
            CoreError::AuthDenied => Some("重新点击「执行」以再次请求授权。".into()),
            CoreError::Config(_) => {
                Some("损坏的配置已备份为 config.toml.bak.<时间戳>，可在设置页重新配置。".into())
            }
            CoreError::EnvUnsupported(_) => Some("本项目只支持 Arch Linux 及其衍生发行版。".into()),
            CoreError::Parse { .. } => Some("点击「复制诊断信息」以获得原始输出片段。".into()),
            _ => None,
        }
    }

    /// 该错误是否属于"认证被拒绝"一类（UI 需要保留计划而不是报失败）。
    pub fn is_auth_denied(&self) -> bool {
        matches!(self, CoreError::AuthDenied)
    }

    /// 该错误是否值得重试（网络类）。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            CoreError::Network { .. } | CoreError::Timeout { .. } | CoreError::RateLimited { .. }
        )
    }

    /// 便于在 UI 中归类显示：错误属于哪个环节。
    pub fn category(&self) -> &'static str {
        match self {
            CoreError::EnvUnsupported(_) => "环境",
            CoreError::AlpmInit(_) | CoreError::SyncDbMissing(_) => "本地数据库",
            CoreError::BackendUnavailable { .. } => "后端",
            CoreError::Network { .. }
            | CoreError::Timeout { .. }
            | CoreError::RateLimited { .. } => "网络",
            CoreError::Parse { .. } => "解析",
            CoreError::InvalidName(_) | CoreError::PlanRejected { .. } => "计划",
            CoreError::Locked | CoreError::ReverseDeps { .. } => "事务",
            CoreError::AuthDenied => "授权",
            CoreError::TransactionFailed { .. } => "事务",
            CoreError::Config(_) => "配置",
            CoreError::Cache(_) => "缓存",
            CoreError::Io(_) => "文件",
            CoreError::NotFound(_) => "查找",
            CoreError::Unsupported(_) => "能力",
            CoreError::Cancelled => "取消",
            CoreError::Internal(_) => "内部",
        }
    }
}

impl From<std::io::Error> for CoreError {
    fn from(e: std::io::Error) -> Self {
        CoreError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for CoreError {
    fn from(e: serde_json::Error) -> Self {
        CoreError::Parse {
            context: "JSON".to_string(),
            raw_head: e.to_string(),
        }
    }
}

impl From<CoreError> for String {
    fn from(e: CoreError) -> Self {
        e.to_string()
    }
}

/// 把可能很长的原始输出截断为前 n 行，用于 CoreError::Parse.raw_head。
pub fn raw_head(text: &str, lines: usize) -> String {
    let mut out = String::new();
    for (i, line) in text.lines().take(lines).enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }
    const MAX: usize = 2000;
    if out.len() > MAX {
        let mut cut = MAX;
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push('…');
    }
    out
}

/// 用于 UI 展示的 Display 包装：只输出用户可读信息。
#[derive(Debug)]
pub struct UserFacing<'a>(pub &'a CoreError);

impl fmt::Display for UserFacing<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.user_message())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_head_truncates_by_lines() {
        let s = "a\nb\nc\nd";
        assert_eq!(raw_head(s, 2), "a\nb");
        assert_eq!(raw_head(s, 10), s);
    }

    #[test]
    fn raw_head_truncates_by_bytes_on_char_boundary() {
        let s = "中".repeat(2000);
        let out = raw_head(&s, 1);
        assert!(out.ends_with('…'));
        assert!(out.len() <= 2004);
    }

    #[test]
    fn io_error_converts() {
        let e = std::io::Error::new(std::io::ErrorKind::NotFound, "nope");
        let ce: CoreError = e.into();
        assert!(matches!(ce, CoreError::Io(_)));
    }

    #[test]
    fn every_variant_has_user_message_and_category() {
        let cases = [
            CoreError::EnvUnsupported("x".into()),
            CoreError::AlpmInit("x".into()),
            CoreError::SyncDbMissing("x".into()),
            CoreError::BackendUnavailable {
                kind: "aur".into(),
                reason: "x".into(),
            },
            CoreError::Network {
                url: "u".into(),
                cause: "s".into(),
            },
            CoreError::Timeout {
                url: "u".into(),
                secs: 30,
            },
            CoreError::RateLimited { retry_after: 5 },
            CoreError::Parse {
                context: "c".into(),
                raw_head: "r".into(),
            },
            CoreError::InvalidName("n".into()),
            CoreError::PlanRejected { reason: "r".into() },
            CoreError::Locked,
            CoreError::ReverseDeps {
                target: "t".into(),
                dependents: vec![],
                count: 0,
            },
            CoreError::AuthDenied,
            CoreError::TransactionFailed {
                code: 1,
                log_tail: "l".into(),
            },
            CoreError::Config("c".into()),
            CoreError::Cache("c".into()),
            CoreError::Io("i".into()),
            CoreError::NotFound("n".into()),
            CoreError::Unsupported("u".into()),
            CoreError::Cancelled,
            CoreError::Internal("i".into()),
        ];
        for e in cases {
            assert!(!e.user_message().is_empty(), "{e:?}");
            assert!(!e.category().is_empty(), "{e:?}");
        }
    }
}
