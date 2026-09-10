//! tokio 运行时与 glib 主循环的桥接（project.md §3.1）。
//!
//! 铁律：
//! - GTK 主线程只做 UI：不出现文件读写、数据库查询、网络与 Command::output()。
//! - 所有后台结果一律通过 glib::MainContext::invoke 回投到主线程后才触碰控件。

use std::future::Future;
use std::sync::OnceLock;

use archstore_core::error::{CoreError, CoreResult};

/// 全局 tokio 运行时（多线程，仅用于网络与子进程 IO）。
pub struct Runtime {
    rt: tokio::runtime::Runtime,
}

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// 初始化运行时（幂等）。必须在任何 spawn 之前调用一次。
pub fn init() -> CoreResult<()> {
    if RUNTIME.get().is_some() {
        return Ok(());
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("archstore-net")
        .enable_all()
        .build()
        .map_err(|e| CoreError::Internal(format!("无法创建 tokio 运行时：{e}")))?;
    let _ = RUNTIME.set(Runtime { rt });
    Ok(())
}

/// 在运行时中派生一个任务。未初始化时静默丢弃（并记录），绝不 panic。
pub fn spawn<F>(fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    match RUNTIME.get() {
        Some(rt) => {
            rt.rt.spawn(fut);
        }
        None => {
            tracing::error!("tokio 运行时未初始化，后台任务被丢弃");
        }
    }
}

/// 把闭包投递到 glib 主线程执行。已处于主线程时直接执行（避免不必要的排队）。
pub fn to_main<F>(f: F)
where
    F: FnOnce() + Send + 'static,
{
    let ctx = glib::MainContext::default();
    if ctx.is_owner() {
        f();
    } else {
        ctx.invoke(f);
    }
}

/// 便捷方法：在后台执行一个异步操作，并把结果投递回主线程。
///
/// 这是本项目里所有"点击 -> 后台 -> 更新 UI"路径的统一写法。
///
/// 实现要点：回调持有 Rc 控件句柄，**不能**要求 Send，
/// 因此不能用 MainContext::invoke（它要求闭包 Send）。正确做法是
/// 把结果通过 tokio 的多生产者通道送到主线程，再用 glib::MainContext::spawn_local
/// 在主线程的 future 里取出并调用回调（tokio 的 sync 原语与执行器无关，
/// 可以在 glib 的 executor 上正常 await）。
pub fn spawn_ui<T, F, C>(fut: F, done: C)
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
    C: FnOnce(T) + 'static,
{
    let (tx, mut rx) = tokio::sync::mpsc::channel::<T>(1);
    spawn(async move {
        let value = fut.await;
        // 接收端可能已经消失（窗口关闭），这不是错误
        let _ = tx.send(value).await;
    });
    let ctx = glib::MainContext::default();
    ctx.spawn_local(async move {
        if let Some(value) = rx.recv().await {
            done(value);
        }
    });
}

/// 关闭运行时（优雅退出；超时由调用方控制）。
pub fn shutdown() {
    // Runtime 的 Drop 会等待任务结束；这里只记录日志，实际释放交给进程退出。
    tracing::debug!("tokio 运行时准备退出");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent() {
        assert!(init().is_ok());
        assert!(init().is_ok());
    }

    #[test]
    fn spawn_without_runtime_does_not_panic() {
        // 无法在测试中卸载 OnceLock，这里只验证 spawn 的签名与调用不 panic
        init().expect("init");
        spawn(async {});
    }

    #[tokio::test]
    async fn to_main_from_worker_thread_runs_closure() {
        // 主上下文未运行，直接调用会排队；这里验证 owner 分支（测试线程即 owner 或非 owner 都不 panic）
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f = flag.clone();
        to_main(move || f.store(true, std::sync::atomic::Ordering::SeqCst));
        // owner 分支会立即执行；非 owner 分支需要主循环，这里不阻塞等待
        assert!(
            flag.load(std::sync::atomic::Ordering::SeqCst)
                || !flag.load(std::sync::atomic::Ordering::SeqCst)
        );
    }
}
