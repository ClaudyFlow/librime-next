//! 主按键处理器：可打印键入栈、退格、Esc、空格/回车提交、数字选词。

use crate::engine::context::SelectResult;
use crate::engine::gear::processor::{ProcessResult, Processor};
use crate::engine::key_event::{KeyEvent, XK_1, XK_9, XK_BACKSPACE, XK_ESCAPE, XK_RETURN, XK_SPACE};

/// 对照 C++ `gears/speller` + `gears/editor` 的「普通键 → 入栈并重算组合」行为。
pub struct Speller;

impl Processor for Speller {
    fn process_key_event(&mut self, engine: &mut crate::engine::Engine, key: &KeyEvent) -> ProcessResult {
        // 可打印 ASCII（字母 / 数字 / 撇号）入栈组字
        if let Some(ch) = key.printable_char() {
            if ch.is_ascii_alphanumeric() || ch == '\'' {
                engine.context.push_input(ch);
                engine.context.highlight = 0;
                engine.compose();
                return ProcessResult::Accepted;
            }
        }
        match key.keycode {
            XK_BACKSPACE => {
                engine.context.pop_input(1);
                engine.context.highlight = 0;
                engine.compose();
                ProcessResult::Accepted
            }
            XK_ESCAPE => {
                engine.context.clear();
                ProcessResult::Accepted
            }
            XK_SPACE | XK_RETURN => {
                if engine.context.is_composing() {
                    if !engine.context.commit_composition() && engine.context.is_composing() {
                        engine.compose();
                    }
                    return ProcessResult::Accepted;
                }
                ProcessResult::Noop
            }
            XK_1..=XK_9 => {
                let idx = (key.keycode - XK_1) as usize;
                if engine.context.has_menu() {
                    match engine.context.select(idx) {
                        Some(SelectResult::Committed(_)) => ProcessResult::Accepted,
                        Some(SelectResult::Continued) => {
                            engine.context.highlight = 0;
                            engine.compose();
                            ProcessResult::Accepted
                        }
                        None => ProcessResult::Noop,
                    }
                } else {
                    ProcessResult::Noop
                }
            }
            _ => ProcessResult::Noop,
        }
    }
}
