//! `src/rime/`（顶层 ~1,700 行）：Engine / Session / Context / Composition / Menu / Candidate。
//!
//! 这是 `librime-next` 作为**运行时引擎**的核心（对照 C++ `src/rime/`）。编译/部署半边
//! （config / dict 编译 / lever）已在其他模块完成；本目录补齐「按键 → 候选 → 提交」的运行时主线。
//!
//! 模块划分与 C++ 一一对应（无 `mod.rs`，文件名即模块名）：
//! * `key_event`   —— 按键（keycode + modifier），可解析/生成 `"Control+a"` 表示
//! * `candidate`   —— 候选词条 + 候选页
//! * `segmentation`—— 输入串分段（每段带一个 Menu）
//! * `menu`        —— 候选菜单
//! * `composition` —— 组合串（= Segmentation + preedit 计算）
//! * `context`     —— 会话级状态：input / caret / composition / commit / options
//! * `runtime`     —— `Engine`（持有 Schema + Context + 组件，驱动 Compose）
//! * `session`     —— `Session`（引擎 + 提交通道，c_api 每个会话一个）
//!
//! 方案 / 处理器 / 翻译器 / 选择器 / 段 / 过滤器 / 神经 / 字典 已拆到同级 `gear` 模块。

#![allow(dead_code)]

pub mod candidate;
pub mod commit_history;
pub mod personalization;
pub mod privacy;
pub mod user_ngram;
pub mod composition;
pub mod context;
pub mod key_event;
pub mod key_names;
pub mod lm_bigram;
pub mod menu;
pub mod runtime;
pub mod segmentation;
pub mod session;
pub mod session_rebuild;
pub mod stage_trace;
pub mod switch_trace;

pub use candidate::{Candidate, Page};
pub use composition::Composition;
pub use context::{Context, SelectResult};
pub use key_event::KeyEvent;
pub use menu::Menu;
pub use runtime::Engine;
pub use segmentation::{Segment, SegStatus, Segmentation};
pub use session::Session;
