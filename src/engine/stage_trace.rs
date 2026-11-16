//! 阶段计时日志：默认关闭，关闭时零行为差异。
//!
//! 打开方式（二者其一）：
//! - 环境变量 `WEASEL_STAGE_TRACE=1`
//! - 标记文件 `%LOCALAPPDATA%\Rime-Next\Logs\stage-trace.on`
//!
//! 输出同时走两处：
//! - `tracing` 的 `weasel_stage` target（**仅当调用方进程的 subscriber 与本 crate
//!   共用同一个 tracing 全局 registry 时**才会进 `server.<date>.log`；进程外 Engine
//!   动态加载本 DLL 时两者 registry 各自独立，这条通常收不到，属已知限制）
//! - 同目录的 `stage-timing.log`（兜底且**以它为准**：任何加载方式下都会追加）
//!
//! 同一按键的 `compose` / `predict_key` / `predict_desk` / `memorize` /
//! `ngram_learn` / `ngram_flush` 都带同一个 `seq`，便于对齐。
//! `ngram_flush` 是后台线程落盘，可能比同一 `seq` 的其它行晚出现（甚至跨按键），
//! 这是刻意的：落盘已移出按键路径（见 `user_ngram::request_flush`）。

use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

static ONCE: OnceLock<bool> = OnceLock::new();
static NEXT_SEQ: AtomicU64 = AtomicU64::new(0);
static CURRENT_SEQ: AtomicU64 = AtomicU64::new(0);
/// 会话重建（切方案）的序列号空间：与按键 `seq` 从数值上就能分开。
static NEXT_REBUILD_SEQ: AtomicU64 = AtomicU64::new(1_000_000_000);

fn marker_path() -> std::path::PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join("Rime-Next")
        .join("Logs")
        .join("stage-trace.on")
}

fn log_path() -> std::path::PathBuf {
    log_dir().join("stage-timing.log")
}

/// 阶段日志目录（`%LOCALAPPDATA%\Rime-Next\Logs`）。
///
/// 公开给 [`super::switch_trace`]：方案切换的分段计时落在同一个目录，
/// 便于事后把「按键阶段」与「重建阶段」按时间轴对在一起看。
pub(crate) fn log_dir() -> std::path::PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join("Rime-Next")
        .join("Logs")
}

/// 开关是否打开：环境变量存在，或标记文件存在。
pub fn enabled() -> bool {
    *ONCE.get_or_init(|| {
        std::env::var_os("WEASEL_STAGE_TRACE").is_some() || marker_path().exists()
    })
}

/// 每次 `process_key` 开头调用：关闭时返回 0，不产生任何额外开销。
pub fn begin_key() -> u64 {
    if !enabled() {
        return 0;
    }
    let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    CURRENT_SEQ.store(seq, Ordering::Relaxed);
    seq
}

/// 当前按键的关联 id（供没有 `Engine` 句柄的深层路径使用，如 `rescore`）。
pub fn current() -> u64 {
    CURRENT_SEQ.load(Ordering::Relaxed)
}

/// 开始一次「会话重建」的计时序列，返回该次重建的 id（≥ 1e9，和按键 seq 不重叠）。
///
/// 只在重建路径（建词典 / prism / lua / 组件装配）里用，不进按键热路径。
pub fn begin_rebuild() -> u64 {
    NEXT_REBUILD_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// 打一行阶段耗时；关闭时直接返回。
pub fn emit(seq: u64, stage: &str, us: u128) {
    if !enabled() {
        return;
    }
    tracing::info!(target: "weasel_stage", seq, stage, us = us as u64, "阶段耗时");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        let _ = writeln!(file, "seq={seq} stage={stage} us={us}");
    }
}
