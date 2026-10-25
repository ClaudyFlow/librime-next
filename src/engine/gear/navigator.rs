//! `src/rime/gear/navigator.cc`：插入符导航处理器（左右 / Home / End / 音节跳）。
//!
//! 对照组是 **C++ `gear/navigator.cc`**（装机版同一份）。它管的是**插入符（caret）**
//! 在多段组合里的移动，候选高亮翻页是 `Selector` 的事 —— 方案里
//! `selectors` 通常排在 `navigator` 之前，所以 Up/Down 先被 `Selector` 吃掉。
//!
//! # 绑定的两套表
//!
//! C++ 按 `_vertical` 开关分 Horizontal / Vertical 两张 keymap
//! （`"navigator"` / `"navigator/vertical"`）：

use serde_json::Value;

use crate::engine::context::Context;
use crate::engine::gear::key_binding::{fallback, KeyBindingMap};
use crate::engine::gear::processor::{ProcessResult, Processor};
use crate::engine::key_event::{
    KeyEvent, K_CONTROL_MASK, XK_DOWN, XK_END, XK_HOME, XK_LEFT, XK_RIGHT, XK_UP,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Orientation {
    Horizontal,
    Vertical,
}

pub struct Navigator {
    horizontal: KeyBindingMap,
    vertical: KeyBindingMap,
    /// `speller/delimiter`（音节跳转的切分符）。
    delimiters: String,
}

impl Navigator {
    pub fn new(config: &Value) -> Self {
        let mut n = Navigator {
            horizontal: KeyBindingMap::new(),
            vertical: KeyBindingMap::new(),
            delimiters: " '".to_string(),
        };
        n.bind_defaults();
        n.horizontal.load_config(config, "navigator");
        n.vertical.load_config(config, "navigator/vertical");
        if let Some(v) = config.pointer("/speller/delimiter").and_then(|v| v.as_str()) {
            n.delimiters = v.to_string();
        } else if let Some(v) = config.pointer("/speller/delimiters").and_then(|v| v.as_str()) {
            // 补丁版遵从 C++ `Config::GetString` 的 fallback 链：先看 `delimiter`
            n.delimiters = v.to_string();
        }
        n
    }

    fn bind_defaults(&mut self) {
        // Horizontal（`navigator.cc: Navigator::Navigator`）
        let h = &mut self.horizontal;
        h.bind(KeyEvent::new(XK_LEFT, 0), "rewind");
        h.bind(KeyEvent::new(XK_LEFT, K_CONTROL_MASK), "left_by_syllable");
        h.bind(KeyEvent::new(XK_RIGHT, 0), "forward");
        h.bind(KeyEvent::new(XK_RIGHT, K_CONTROL_MASK), "right_by_syllable");
        h.bind(KeyEvent::new(XK_HOME, 0), "home");
        h.bind(KeyEvent::new(XK_END, 0), "end");
        // Vertical
        let v = &mut self.vertical;
        v.bind(KeyEvent::new(XK_UP, 0), "rewind");
        v.bind(KeyEvent::new(XK_UP, K_CONTROL_MASK), "left_by_syllable");
        v.bind(KeyEvent::new(XK_DOWN, 0), "forward");
        v.bind(KeyEvent::new(XK_DOWN, K_CONTROL_MASK), "right_by_syllable");
        v.bind(KeyEvent::new(XK_HOME, 0), "home");
        v.bind(KeyEvent::new(XK_END, 0), "end");
    }

    fn keymap(&self, vertical: bool) -> &KeyBindingMap {
        if vertical {
            &self.vertical
        } else {
            &self.horizontal
        }
    }

    fn perform(&self, ctx: &mut Context, action: &str) -> bool {
        match action {
            "rewind" => {
                let pos = ctx.composition.segmentation.confirmed_position();
                ctx.set_caret_pos(pos);
                true
            }
            "forward" => {
                let end = ctx.input.len();
                ctx.set_caret_pos(end);
                true
            }
            "left_by_char" => move_left(ctx) || go_to_end(ctx),
            "right_by_char" => move_right(ctx) || go_home(ctx),
            "left_by_char_no_loop" => move_left(ctx),
            "right_by_char_no_loop" => move_right(ctx),
            "home" => go_home(ctx),
            "end" => go_to_end(ctx),
            "left_by_syllable" => {
                let confirmed = ctx.composition.segmentation.confirmed_position();
                jump_left(ctx, confirmed, &self.delimiters)
            }
            "right_by_syllable" => {
                let confirmed = ctx.composition.segmentation.confirmed_position();
                jump_right(ctx, confirmed, &self.delimiters)
            }
            "left_by_syllable_no_loop" => {
                let confirmed = ctx.composition.segmentation.confirmed_position();
                jump_left(ctx, confirmed, &self.delimiters)
            }
            "right_by_syllable_no_loop" => jump_right(ctx, 0, &self.delimiters),
            "noop" => true,
            other => {
                eprintln!("[navigator] unknown action: '{other}'");
                false
            }
        }
    }
}

fn move_left(ctx: &mut Context) -> bool {
    if ctx.caret_pos == 0 {
        return false;
    }
    ctx.set_caret_pos(ctx.caret_pos - 1);
    true
}

fn move_right(ctx: &mut Context) -> bool {
    if ctx.caret_pos >= ctx.input.len() {
        return false;
    }
    ctx.set_caret_pos(ctx.caret_pos + 1);
    true
}

fn go_home(ctx: &mut Context) -> bool {
    ctx.set_caret_pos(0);
    true
}

fn go_to_end(ctx: &mut Context) -> bool {
    let len = ctx.input.len();
    ctx.set_caret_pos(len);
    true
}

/// 往左跳一个音节：先退化到 confirmed 位置之前的第一个切分符之后。
fn jump_left(ctx: &mut Context, floor: usize, delimiters: &str) -> bool {
    let bytes = ctx.input.as_bytes();
    let mut start = ctx.caret_pos.min(bytes.len());
    // 已经站在切分符之后 ⇒ 先跨过它，否则连按会被卡住
    if start > floor && delimiters.contains(bytes[start - 1] as char) {
        start -= 1;
    }
    let mut i = start;
    while i > floor {
        i -= 1;
        if delimiters.contains(bytes[i] as char) {
            ctx.set_caret_pos(i + 1);
            return true;
        }
    }
    ctx.set_caret_pos(floor);
    true
}

/// 往右跳一个音节：找下一个切分符之后的位置。
fn jump_right(ctx: &mut Context, floor: usize, delimiters: &str) -> bool {
    let bytes = ctx.input.as_bytes();
    let start = ctx.caret_pos.max(floor).min(bytes.len());
    let mut i = start;
    while i < bytes.len() {
        if delimiters.contains(bytes[i] as char) {
            ctx.set_caret_pos(i + 1);
            return true;
        }
        i += 1;
    }
    go_to_end(ctx)
}

impl Processor for Navigator {
    fn process_key_event(
        &mut self,
        engine: &mut crate::engine::Engine,
        key: &KeyEvent,
    ) -> ProcessResult {
        if key.release() {
            return ProcessResult::Noop;
        }
        let ctx = &mut engine.context;
        if !ctx.is_composing() {
            return ProcessResult::Noop;
        }
        let vertical = ctx.get_option("_vertical");
        // C++ Navigator 也传 `FallbackOptions::All`
        let Some(action) = self
            .keymap(vertical)
            .lookup(key, fallback::ALL)
            .map(str::to_string)
        else {
            return ProcessResult::Noop;
        };
        if self.perform(ctx, &action) {
            ProcessResult::Accepted
        } else {
            ProcessResult::Noop
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn engine_with(input: &str, caret: usize) -> crate::engine::Engine {
        let mut e = crate::engine::Engine::empty_for_test();
        e.context.input = input.to_string();
        e.context.caret_pos = caret;
        e
    }

    #[test]
    fn not_composing_is_noop() {
        let mut nav = Navigator::new(&json!({}));
        let mut engine = engine_with("", 0);
        assert_eq!(
            nav.process_key_event(&mut engine, &KeyEvent::new(XK_LEFT, 0)),
            ProcessResult::Noop
        );
    }

    #[test]
    fn home_and_end_move_caret() {
        let mut nav = Navigator::new(&json!({}));
        let mut engine = engine_with("nihao", 3);
        assert_eq!(
            nav.process_key_event(&mut engine, &KeyEvent::new(XK_HOME, 0)),
            ProcessResult::Accepted
        );
        assert_eq!(engine.context.caret_pos, 0);
        nav.process_key_event(&mut engine, &KeyEvent::new(XK_END, 0));
        assert_eq!(engine.context.caret_pos, 5);
    }

    #[test]
    fn left_moves_one_char_right_loops_home() {
        let mut nav = Navigator::new(&json!({"navigator": {"bindings": {"Left": "left_by_char"}}}));
        let mut engine = engine_with("ni", 2);
        nav.process_key_event(&mut engine, &KeyEvent::new(XK_LEFT, 0));
        assert_eq!(engine.context.caret_pos, 1);
        nav.process_key_event(&mut engine, &KeyEvent::new(XK_LEFT, 0));
        assert_eq!(engine.context.caret_pos, 0);
        // 已到最左 ⇒ loop 到末尾
        nav.process_key_event(&mut engine, &KeyEvent::new(XK_LEFT, 0));
        assert_eq!(engine.context.caret_pos, 2);
    }

    #[test]
    fn control_left_jumps_by_syllable() {
        let mut nav = Navigator::new(&json!({}));
        // delimiter 默认是空格与撇号
        let mut engine = engine_with("ni hao", 6);
        nav.process_key_event(&mut engine, &KeyEvent::new(XK_LEFT, K_CONTROL_MASK));
        assert_eq!(engine.context.caret_pos, 3);
        nav.process_key_event(&mut engine, &KeyEvent::new(XK_LEFT, K_CONTROL_MASK));
        assert_eq!(engine.context.caret_pos, 0);
    }

    #[test]
    fn vertical_map_is_used_when_option_is_on() {
        let mut nav = Navigator::new(&json!({}));
        let mut engine = engine_with("ni", 2);
        engine.context.set_option("_vertical", true);
        // Up 在纵向表里 = rewind
        nav.process_key_event(&mut engine, &KeyEvent::new(XK_UP, 0));
        assert_eq!(engine.context.caret_pos, 0);
        // 横向表的 Left 在纵向模式下不应命中
        let mut engine = engine_with("ni", 2);
        engine.context.set_option("_vertical", true);
        assert_eq!(
            nav.process_key_event(&mut engine, &KeyEvent::new(XK_LEFT, 0)),
            ProcessResult::Noop
        );
    }
}
