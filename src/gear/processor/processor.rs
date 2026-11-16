//! `src/rime/processor.h`：`ProcessResult` + `Processor` trait。

use crate::engine::key_event::KeyEvent;
use crate::engine::runtime::Engine;

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
///
/// # 三个回调都在追 C++ 的 notifier
///
/// C++ 的 `Processor` 只有 `ProcessKeyEvent` 一个虚函数；另外两条通道是
/// `Context` 上的 `update_notifier` / `unhandled_key_notifier`，组件自己在构造函数里
/// `connect` 上去（`chord_composer.cc:47-52`）。Rust 这里把 `Context` 做成
/// **纯数据**（没有回调槽，免得 `Context` 反向持有 `Engine`），改由引擎在对应时机
/// 逐个通知组件 —— 语义等价，接线方式不同。
pub trait Processor {
    fn process_key_event(&mut self, engine: &mut Engine, key: &KeyEvent) -> ProcessResult;

    /// C++ `Processor` 拿到 `engine_` 之后的初始化（对应构造函数里对 `context()`
    /// 的那些副作用）。
    ///
    /// 典型用途：`ChordComposer` 构造时 `ctx->set_option("_chord_typing", true)`
    /// （`chord_composer.cc:48`）—— 那是给宿主看的"本方案要抬键事件"信号。
    /// Rust 的组件构造函数只拿得到方案配置树，碰不到 `Context`，所以拆成这一步。
    fn on_attach(&mut self, _engine: &mut Engine) {}

    /// C++ `Context::update_notifier`：输入串/组合态变化后回调。
    ///
    /// 引擎在 `Compose` 之前触发（`context.cc` 里 `PushInput` 等函数发通知，
    /// `ConcreteEngine::OnContextUpdate` 收）。目前只有 `chord_composer` 用得上。
    fn on_context_update(&mut self, _engine: &mut Engine) {}

    /// C++ `Context::unhandled_key_notifier`（`engine.cc:193`）：整条链都没吃下这个键。
    ///
    /// **合成键也会触发**（C++ 只跳过 `commit_history`，notifier 是无条件发的），
    /// `chord_composer` 正是靠这一点在"和弦产出上屏后"清掉 raw 输入。
    fn on_unhandled_key(&mut self, _engine: &mut Engine, _key: &KeyEvent) {}
}
