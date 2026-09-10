//! helper -> GUI 的逐行 JSON 输出协议（project.md §5.2）。
//!
//! 每行一个 JSON 对象，便于解析且不依赖日志格式：
//! {"event":"start","plan_schema":1,"items":3}
//! {"event":"progress","phase":"download","percent":42,"detail":"firefox-155.0.1-1-x86_64.pkg.tar.zst"}
//! {"event":"log","level":"info","line":"正在检查密钥环…"}
//! {"event":"error","code":"LOCKED","message":"数据库被锁定：另一个 pacman 正在运行"}
//! {"event":"done","status":"ok","installed":3,"failed":0}
//! {"event":"needs_tty","hint":"…"}

use std::io::Write;

/// 事件发射器：所有输出必须经过它，保证 stdout 上只有 JSON。
pub struct Emitter {
    out: std::io::Stdout,
    /// 原始日志的环形缓冲，用于结束时输出摘要
    log_tail: Vec<String>,
}

impl Default for Emitter {
    fn default() -> Self {
        Self::new()
    }
}

impl Emitter {
    pub fn new() -> Self {
        Self {
            out: std::io::stdout(),
            log_tail: Vec::new(),
        }
    }

    fn emit(&mut self, value: serde_json::Value) {
        // stdout 只允许出现 JSON：任何写失败都只能放弃（GUI 会因管道关闭而收尾）
        let _ = writeln!(self.out, "{value}");
        let _ = self.out.flush();
    }

    /// 计划开始执行。
    pub fn start(&mut self, plan_schema: u32, items: usize, kind: &str) {
        self.emit(serde_json::json!({
            "event": "start",
            "plan_schema": plan_schema,
            "items": items,
            "kind": kind,
        }));
    }

    /// 阶段进度。
    pub fn progress(&mut self, phase: &str, percent: Option<u8>, detail: &str) {
        let mut v = serde_json::json!({
            "event": "progress",
            "phase": phase,
            "detail": detail,
        });
        if let Some(p) = percent {
            v["percent"] = serde_json::json!(p);
        }
        self.emit(v);
    }

    /// 原始日志行（pacman / flatpak 的输出）。
    pub fn log(&mut self, level: &str, line: &str) {
        self.log_tail.push(line.to_string());
        if self.log_tail.len() > 200 {
            self.log_tail.remove(0);
        }
        self.emit(serde_json::json!({
            "event": "log",
            "level": level,
            "line": line,
        }));
    }

    pub fn info(&mut self, line: &str) {
        self.log("info", line);
    }

    pub fn warn(&mut self, line: &str) {
        self.log("warn", line);
    }

    /// 结构化错误（GUI 依据 code 决定文案与后续动作）。
    pub fn error(&mut self, code: &str, message: &str) {
        self.emit(serde_json::json!({
            "event": "error",
            "code": code,
            "message": message,
        }));
    }

    /// 需要交互式终端（PGP 密钥导入、AUR 助手的编辑器等）。
    pub fn needs_tty(&mut self, hint: &str) {
        self.emit(serde_json::json!({
            "event": "needs_tty",
            "code": codes::NEEDS_TTY,
            "hint": hint,
        }));
    }

    /// 结束。
    pub fn done(
        &mut self,
        status: &str,
        installed: usize,
        failed: usize,
        removed: usize,
        elapsed_ms: u128,
    ) {
        self.emit(serde_json::json!({
            "event": "done",
            "status": status,
            "installed": installed,
            "removed": removed,
            "failed": failed,
            "elapsed_ms": elapsed_ms as u64,
        }));
    }

    /// 日志尾部（GUI 失败面板展示）。
    pub fn log_tail(&self, lines: usize) -> String {
        let start = self.log_tail.len().saturating_sub(lines);
        self.log_tail[start..].join("\n")
    }
}

/// helper 的错误码（与附录 C 的 UI 表现一一对应）。
pub mod codes {
    /// 数据库被锁定
    pub const LOCKED: &str = "LOCKED";
    /// 计划路径不合规
    pub const BAD_PLAN_PATH: &str = "BAD_PLAN_PATH";
    /// 计划内容被拒绝
    pub const PLAN_REJECTED: &str = "PLAN_REJECTED";
    /// 包名不合规
    pub const INVALID_NAME: &str = "INVALID_NAME";
    /// 目标包在系统中不存在
    pub const PKG_NOT_FOUND: &str = "PKG_NOT_FOUND";
    /// 后端不可用
    pub const BACKEND_UNAVAILABLE: &str = "BACKEND_UNAVAILABLE";
    /// 需要交互式终端
    pub const NEEDS_TTY: &str = "NEEDS_TTY";
    /// 事务执行失败
    pub const TRANSACTION_FAILED: &str = "TRANSACTION_FAILED";
    /// 内部错误
    pub const INTERNAL: &str = "INTERNAL";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_tail_keeps_last_lines() {
        let mut e = Emitter::new();
        for i in 0..10 {
            e.log_tail.push(format!("line {i}"));
        }
        assert_eq!(e.log_tail(3), "line 7\nline 8\nline 9");
    }

    #[test]
    fn log_tail_handles_more_than_available() {
        let e = Emitter::new();
        assert_eq!(e.log_tail(5), "");
    }

    #[test]
    fn codes_are_unique() {
        let all = [
            codes::LOCKED,
            codes::BAD_PLAN_PATH,
            codes::PLAN_REJECTED,
            codes::INVALID_NAME,
            codes::PKG_NOT_FOUND,
            codes::BACKEND_UNAVAILABLE,
            codes::NEEDS_TTY,
            codes::TRANSACTION_FAILED,
            codes::INTERNAL,
        ];
        let mut sorted = all.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len());
    }
}
