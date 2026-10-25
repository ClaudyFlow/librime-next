//! `src/rime/processor.h`：`ProcessResult` + `Processor` trait。

use crate::engine::Engine;
use crate::engine::key_event::KeyEvent;

/// 按键处理结果（对照 C++ `ProcessResult`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessResult {
    /// 交给 OS 默认处理。
    Rejected,
    /// 已消费。
    Accepted,
    /// 留给其它处理器。
    Noop,
}

/// 按键处理器（对照 C++ `Processor`）。
pub trait Processor {
    fn process_key_event(&mut self, engine: &mut Engine, key: &KeyEvent) -> ProcessResult;
}
