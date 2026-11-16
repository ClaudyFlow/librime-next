//! 按键处理器（Processor 链路）。

pub mod ascii_composer;
pub mod chord_composer;
pub mod editor;
pub mod input_type_selector;
pub mod input_type_speller;
pub mod key_binder;
pub mod key_binding;
pub mod keypad_processor;
pub mod navigator;
pub mod processor;
pub mod selector;
pub mod speller;
pub mod streaming_chord_processor;
pub mod switcher;

// 同名叶子模块 `processor` 的导出项在 `gear/processor/processor.rs`，
// 这里再导出一层，使 `crate::gear::processor::Processor` 与相对 `super::processor::Processor` 都能解析。
pub use processor::{ProcessResult, Processor};
