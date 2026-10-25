//! `src/rime/`（顶层胶水）：`Session`（c_api 每会话一个，持有引擎 + 提交通道）。

use std::path::Path;

use crate::engine::context::{Context, SelectResult};
use crate::engine::key_event::KeyEvent;
use crate::engine::runtime::Engine;

/// 一个输入会话（对照 C++ `Session`）。持有 `Engine`，并缓存待提交文本。
pub struct Session {
    pub engine: Option<Engine>,
    commit_text: Option<String>,
}

impl Session {
    pub fn create(build_dir: &Path, schema_id: &str) -> Session {
        Session {
            engine: Engine::create(build_dir, schema_id),
            commit_text: None,
        }
    }

    /// 处理一次按键（keycode + mask）。返回是否被消费；待提交文本经 `take_commit` 取走。
    pub fn process_key(&mut self, keycode: u32, mask: u32) -> bool {
        let key = KeyEvent::new(keycode, mask);
        let Some(engine) = &mut self.engine else {
            return false;
        };
        let handled = engine.process_key(&key);
        if let Some(t) = engine.context.commit_text.take() {
            self.commit_text = Some(t);
        }
        handled
    }

    pub fn commit_composition(&mut self) -> bool {
        let Some(engine) = &mut self.engine else {
            return false;
        };
        let ok = engine.context.commit_composition();
        if let Some(t) = engine.context.commit_text.take() {
            self.commit_text = Some(t);
        }
        ok
    }

    pub fn clear_composition(&mut self) {
        if let Some(engine) = &mut self.engine {
            engine.context.clear();
        }
    }

    /// 取走待提交文本（消费一次）。
    pub fn take_commit(&mut self) -> Option<String> {
        self.commit_text.take()
    }

    /// 选择第 `idx` 个候选，并在「只确认前缀」时重排组合 —— 等价于 speller 的数字键路径
    /// （`gears/speller.cc` 里 `XK_1..XK_9` 命中 `Context::ConfirmCurrentSelection` 后，
    /// 若未整段提交则重新 `Engine::Compose`）。返回选择结果，待提交文本经 [`Self::take_commit`]。
    ///
    /// 与直接调 `context().select(idx)` 的区别：这里把 speller 的「续组字 → 重算菜单」补上，
    /// 因此逐音节确认（`你` → 剩 `hao` → `好`）能端到端跑通，且不受数字键 0..8 的范围限制。
    pub fn choose(&mut self, idx: usize) -> Option<SelectResult> {
        let Some(engine) = &mut self.engine else {
            return None;
        };
        let result = engine.context.select(idx);
        if matches!(result, Some(SelectResult::Continued)) {
            engine.context.highlight(0);
            engine.compose();
        }
        if let Some(t) = engine.context.commit_text.take() {
            self.commit_text = Some(t);
        }
        result
    }

    pub fn context(&self) -> Option<&Context> {
        self.engine.as_ref().map(|e| &e.context)
    }

    pub fn context_mut(&mut self) -> Option<&mut Context> {
        self.engine.as_mut().map(|e| &mut e.context)
    }

    pub fn engine_mut(&mut self) -> Option<&mut Engine> {
        self.engine.as_mut()
    }

    pub fn schema_id(&self) -> Option<&str> {
        self.engine.as_ref().map(|e| e.schema_id())
    }
}
