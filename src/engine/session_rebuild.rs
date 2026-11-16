//! 会话重建的后台执行器（切方案不再把按键堵在全局锁后面）。
//!
//! 2026-11-10 背景：`rime_select_schema` 原来在 `global()` 锁里跑
//! `Session::create_with_roots()` 整个重建（建词典 / prism / 重绑码表 / aux / lua），
//! 实测静默 25~84 秒，期间所有客户端按键排队，用户看到的是"打几个拼音突然卡住、
//! 然后候选窗才爆出来"。
//!
//! 现在把重建挪到**一条常驻工作线程**上：
//! - 请求方只登记"要切到哪个方案"，立刻返回（`rime_select_schema` 返回 1 = 已受理）；
//! - 重建期间按键继续进**旧会话**（旧引擎此时是只读快照，不会被新方案的码表状态污染）；
//! - 建好后由工作线程在全局锁里**原子换入**，换入那段临界区只有一次 `HashMap::insert`。
//!
//! 为什么是常驻线程而不是每次 `spawn`：lua 的 VM / 整句码表缓存是线程局部的
//! （见 `gear/lua.rs` 的 `SHARED_VMS`），每条新线程都要重新引导一遍；常驻线程能把
//! 这层缓存留到下一次切方案。代价是这台机器会多一份 lua VM（与引擎线程各一份），
//! 这是"切方案不卡按键"换来的内存开销。
//!
//! 另外，多线程并发重建会让 lua 大表同时建两份（内存翻倍、userdb 争锁），
//! 所以队列**串行**执行：后一次切换排队，前一次的结果由调用方按代号丢弃。

use std::sync::OnceLock;
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

/// 一个待执行的重建任务。任务自己负责把结果写回（并在写回时做代号校验）。
pub type Job = Box<dyn FnOnce() + Send + 'static>;

static QUEUE: OnceLock<Option<Sender<Job>>> = OnceLock::new();

/// 测试/诊断钩子：`RIME_REBUILD_TEST_DELAY_MS=<毫秒>` 时，任务执行前先睡这么久。
///
/// 给"重建中仍有按键"的回归用例用 —— 真实重建可能只有几十毫秒，
/// 没法稳定地在窗口期里按键。
fn test_delay() -> Option<Duration> {
    std::env::var("RIME_REBUILD_TEST_DELAY_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
}

fn queue() -> Option<&'static Sender<Job>> {
    QUEUE
        .get_or_init(|| {
            let (tx, rx) = channel::<Job>();
            let spawned = std::thread::Builder::new()
                .name("rime-session-rebuild".into())
                .spawn(move || {
                    while let Ok(job) = rx.recv() {
                        if let Some(delay) = test_delay() {
                            std::thread::sleep(delay);
                        }
                        // 一次重建里的 panic 不能带走工作线程（否则后续切换都排队到死）。
                        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                        if outcome.is_err() {
                            eprintln!(
                                "[session-rebuild] 后台重建 panic 已吞掉；旧会话继续服务，\
                                 下一次切换仍可用"
                            );
                        }
                    }
                })
                .ok();
            if spawned.is_none() {
                eprintln!("[session-rebuild] 工作线程起不来，将退回同步重建");
            }
            spawned.map(|_| tx)
        })
        .as_ref()
}

/// 投递一个重建任务。返回 `Err(job)` 表示工作线程不可用，调用方应退回同步重建
/// （旧行为），而不是把这次切换丢掉。
pub fn dispatch(job: Job) -> Result<(), Job> {
    match queue() {
        Some(tx) => tx.send(job).map_err(|err| err.0),
        None => Err(job),
    }
}
