//! 方案切换 / 会话重建的阶段计时（默认关闭，关闭时零行为差异）。
//!
//! 背景（2026-11-10）：`rime_select_schema` 在全局锁里整个重建会话，实测重建期间
//! 引擎静默 25~84 秒，期间所有客户端按键排队。旧日志只知道"很慢"，但分不清慢在
//! 建词典、prism、重绑码表、aux、还是 lua 脚本 —— 所以先按阶段打点，再动结构。
//!
//! 开关（任一即可）：
//! - 环境变量 `WEASEL_TRACE_SCHEMA=1`（本模块的主开关，沿用旧诊断开关名）
//! - 环境变量 `WEASEL_TRACE_COMPONENTS=1`（只看逐组件耗时）
//! - `stage_trace` 的开关（`WEASEL_STAGE_TRACE=1` 或标记文件）
//!
//! 输出两处：
//! - stderr：`[schema-switch] <阶段>: <x.x> ms`（沿用旧格式，历史 grep 仍可用）
//! - `%LOCALAPPDATA%\Rime-Next\Logs\schema-switch.log`（兜底，按 `seq` 分组）
//!
//! 一次重建的所有阶段共享同一个 `seq`（由 [`begin_rebuild`] 起头），日志里按
//! `seq` 排序即可还原整条时间线。

use std::cell::Cell;
use std::io::Write as _;
use std::sync::OnceLock;
use std::time::Instant;

static ON: OnceLock<bool> = OnceLock::new();

thread_local! {
    /// 当前线程正在进行的重建 `seq`：背景工作线程建会话时由 [`begin_rebuild`] 设定。
    static CURRENT: Cell<u64> = const { Cell::new(0) };
}

/// 分段计时是否打开。
pub fn enabled() -> bool {
    *ON.get_or_init(|| {
        std::env::var_os("WEASEL_TRACE_SCHEMA").is_some()
            || std::env::var_os("WEASEL_TRACE_COMPONENTS").is_some()
            || crate::engine::stage_trace::enabled()
    })
}

/// 逐组件计时是否打开（`WEASEL_TRACE_COMPONENTS` 只关心这一层）。
pub fn components_enabled() -> bool {
    enabled()
}

/// 开始一次重建计时：返回 `seq` 并记为当前线程的关联 id。
///
/// 关闭时返回 0 —— 调用方照常计时也没关系，[`done`] 会直接丢掉。
pub fn begin_rebuild() -> u64 {
    if !enabled() {
        CURRENT.with(|c| c.set(0));
        return 0;
    }
    let seq = crate::engine::stage_trace::begin_rebuild();
    CURRENT.with(|c| c.set(seq));
    seq
}

/// 当前线程的重建 `seq`（0 = 没开或不在重建里）。
pub fn current_seq() -> u64 {
    CURRENT.with(Cell::get)
}

/// 起一个可选计时点：开关关闭时返回 `None`，不产生额外开销。
pub fn mark() -> Option<Instant> {
    enabled().then(Instant::now)
}

/// 结束一段计时并打点（标签用调用方给的字符串）。
pub fn done(label: &str, start: Option<Instant>) {
    let Some(start) = start else { return };
    let us = start.elapsed().as_micros();
    record_us(label, us);
}

/// 直接记一段已知耗时（微秒）。
pub fn record_us(label: &str, us: u128) {
    if !enabled() {
        return;
    }
    eprintln!("[schema-switch] {label}: {:.1} ms", us as f64 / 1000.0);
    let seq = current_seq();
    let path = crate::engine::stage_trace::log_dir().join("schema-switch.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "seq={seq} stage={label} us={us}");
    }
}

/// 记一条不带耗时的备注（方案 id、目录、成败原因），和计时行同一 `seq`。
pub fn note(label: &str) {
    if !enabled() {
        return;
    }
    eprintln!("[schema-switch] {label}");
    let seq = current_seq();
    let path = crate::engine::stage_trace::log_dir().join("schema-switch.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "seq={seq} note={label}");
    }
}

/// 给一段计时代码用的小工具：`let t = span(); ...; done("x", t)` 的简写形式。
pub fn span<T>(label: &str, body: impl FnOnce() -> T) -> T {
    let start = mark();
    let value = body();
    done(label, start);
    value
}
