//! `src/rime/gear/editor.cc`：`Editor` / `FluidEditor` / `ExpressEditor`。
//!
//! 对照组是**装机在跑的那份**（`/c/code/librime` 的 `gear/editor.cc`，含 weasel 补丁），
//! 行为要点：
//!
//! * 按键 release ⇒ `Rejected`（交回前端）；
//! * 组字中先查 `<editor>/bindings` 的键盘绑定，命中 ⇒ `Accepted`；
//! * 未命中且是**可打印字符**（`0x20 < ch < 0x7f`，空格被排除因为在绑定表里）⇒ 交给
//!   `char_handler`：`ExpressEditor` 是 `DirectCommit`（**直接上屏**并交回前端），
//!   `FluidEditor` 是 `AddToInput`（入\binom组字串）；
//! * 其余 ⇒ `Noop`，交给下游 speller。
//!
//! # 为什么要 `ExpressEditor`
//!
//! 真实方案的 `engine/processors` 几乎都列的是 `express_editor`（luna_pinyin、
//! yuhao、wubi86 …都一样）。它 `char_handler = DirectCommit` + 空格/回车确认，
//! 是"拼音边打边选、直接回车上屏原始串"这套手感的来源。

use crate::engine::context::Context;
use crate::engine::gear::key_binding::{fallback, KeyBindingMap};
use crate::engine::gear::processor::{ProcessResult, Processor};
use crate::engine::key_event::{
    KeyEvent, K_CONTROL_MASK, K_SHIFT_MASK, K_SUPER_MASK, XK_BACKSPACE, XK_DELETE, XK_ESCAPE,
    XK_RETURN, XK_SPACE,
};
use serde_json::Value;

/// C++ `Editor::CharHandlerPtr`：`DirectCommit` / `AddToInput`。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CharHandler {
    /// `ExpressEditor`：直接commit当前组合，并把本次按键**退回前端**。
    DirectCommit,
    /// `FluidEditor`：把字符推入组字串。
    AddToInput,
}

/// 编辑器（对照 C++ `Editor`；`auto_commit` 由子类决定）。
pub struct Editor {
    keymap: KeyBindingMap,
    handler: Option<CharHandler>,
    auto_commit: bool,
}

impl Editor {
    /// `ExpressEditor`（`auto_commit = true`，`char_handler = DirectCommit`）。
    pub fn express(config: &Value) -> Self {
        let mut e = Editor {
            keymap: KeyBindingMap::new(),
            handler: Some(CharHandler::DirectCommit),
            auto_commit: true,
        };
        e.bind_express_defaults();
        e.load_config(config, "editor");
        e
    }

    /// `FluidEditor` / `fluency_editor`（`auto_commit = false`，`char_handler = AddToInput`）。
    pub fn fluid(config: &Value) -> Self {
        let mut e = Editor {
            keymap: KeyBindingMap::new(),
            handler: Some(CharHandler::AddToInput),
            auto_commit: false,
        };
        e.bind_fluid_defaults();
        e.load_config(config, "editor");
        e
    }

    /// ExpressEditor 的默认绑定（`editor.cc:ExpressEditor::ExpressEditor`）。
    fn bind_express_defaults(&mut self) {
        let m = &mut self.keymap;
        m.bind(KeyEvent::new(XK_SPACE, 0), "confirm");
        m.bind(KeyEvent::new(XK_BACKSPACE, 0), "revert");
        m.bind(KeyEvent::new(XK_BACKSPACE, K_CONTROL_MASK), "back_syllable");
        m.bind(KeyEvent::new(XK_RETURN, 0), "commit_raw_input");
        m.bind(KeyEvent::new(XK_RETURN, K_CONTROL_MASK), "commit_script_text");
        m.bind(
            KeyEvent::new(XK_RETURN, K_CONTROL_MASK | K_SHIFT_MASK),
            "commit_comment",
        );
        m.bind(KeyEvent::new(XK_DELETE, 0), "delete");
        m.bind(KeyEvent::new(XK_DELETE, K_CONTROL_MASK), "delete_candidate");
        m.bind(KeyEvent::new(XK_ESCAPE, 0), "cancel");
    }

    /// FluidEditor 的默认绑定（`editor.cc:FluidEditor::FluidEditor`）。
    fn bind_fluid_defaults(&mut self) {
        let m = &mut self.keymap;
        m.bind(KeyEvent::new(XK_SPACE, 0), "confirm");
        m.bind(KeyEvent::new(XK_BACKSPACE, 0), "back");
        m.bind(KeyEvent::new(XK_BACKSPACE, K_CONTROL_MASK), "back_syllable");
        m.bind(KeyEvent::new(XK_RETURN, 0), "commit_composition");
        m.bind(KeyEvent::new(XK_RETURN, K_CONTROL_MASK), "commit_raw_input");
        m.bind(KeyEvent::new(XK_RETURN, K_SHIFT_MASK), "commit_script_text");
        m.bind(
            KeyEvent::new(XK_RETURN, K_CONTROL_MASK | K_SHIFT_MASK),
            "commit_comment",
        );
        m.bind(KeyEvent::new(XK_DELETE, 0), "delete");
        m.bind(KeyEvent::new(XK_DELETE, K_CONTROL_MASK), "delete_candidate");
        m.bind(KeyEvent::new(XK_ESCAPE, 0), "cancel");
    }

    /// `Editor::LoadConfig`：`<path>/bindings` + `<path>/char_handler`。
    fn load_config(&mut self, config: &Value, path: &str) {
        self.keymap.load_config(config, path);
        if let Some(name) = config.pointer(&format!("/{path}/char_handler")).and_then(|v| v.as_str())
        {
            match name {
                "direct_commit" => self.handler = Some(CharHandler::DirectCommit),
                "add_to_input" => self.handler = Some(CharHandler::AddToInput),
                "noop" => self.handler = None,
                other => {
                    eprintln!("[editor] invalid char_handler: '{other}'");
                }
            }
        }
    }

    pub fn set_auto_commit(&self, ctx: &mut Context) {
        ctx.set_option("_auto_commit", self.auto_commit);
    }

    pub fn auto_commit(&self) -> bool {
        self.auto_commit
    }

    /// 一次动作（`Editor::Handler` 们）。返回是否处理了。
    fn perform(&self, ctx: &mut Context, action: &str) -> bool {
        match action {
            "confirm" => {
                // C++ `Editor::Confirm`：`ConfirmCurrentSelection() || Commit()`
                let _ = ctx.confirm_current_selection() || ctx.commit();
                true
            }
            "toggle_selection" => {
                let _ = ctx.reopen_previous_segment() || ctx.confirm_current_selection();
                true
            }
            "commit_comment" => {
                let text = ctx
                    .get_selected_candidate()
                    .map(|c| c.comment.clone())
                    .unwrap_or_default();
                if !text.is_empty() {
                    push_commit(ctx, text);
                    ctx.clear();
                }
                true
            }
            "commit_script_text" => {
                let text = ctx.get_script_text();
                push_commit(ctx, text);
                ctx.clear();
                true
            }
            // C++：`ctx->ClearNonConfirmedComposition(); ctx->Commit();`
            "commit_raw_input" => {
                ctx.clear_non_confirmed_composition();
                ctx.commit();
                true
            }
            "commit_composition" => {
                if !ctx.confirm_current_selection() || !ctx.has_menu() {
                    ctx.commit();
                }
                true
            }
            // C++ `RevertLastEdit`：`ReopenPreviousSelection() || (PopInput() && ReopenPreviousSegment())`
            "revert" => {
                let _ = ctx.reopen_previous_selection()
                    || (ctx.pop_input(1) && ctx.reopen_previous_segment());
                true
            }
            // C++ `BackToPreviousInput`：`ReopenPreviousSegment() || ReopenPreviousSelection() || PopInput()`
            "back" => {
                let _ = ctx.reopen_previous_segment()
                    || ctx.reopen_previous_selection()
                    || ctx.pop_input(1);
                true
            }
            // C++ `BackToPreviousSyllable`：逐音节回退；我们的候选没有 span 信息，
            // 降级为整字节回退一格（`pop_input_by_syllable` 恒假时的等价分支）。
            "back_syllable" => {
                let _ = ctx.reopen_previous_selection()
                    || (ctx.pop_input(1) && ctx.reopen_previous_segment());
                true
            }
            "delete_candidate" => {
                ctx.delete_current_selection();
                true
            }
            "delete" => {
                ctx.delete_input(1);
                true
            }
            "cancel" => {
                if !ctx.clear_previous_segment() {
                    ctx.clear();
                }
                true
            }
            "noop" => true,
            other => {
                eprintln!("[editor] unknown action: '{other}'");
                false
            }
        }
    }
}

/// C++ `engine_->sink()(text)`：不经组合**直接**把一段文本送到提交管道
/// （薄封装到 [`Context::commit_direct`]）。
fn push_commit(ctx: &mut Context, text: String) {
    ctx.commit_direct(text);
}

impl Processor for Editor {
    fn process_key_event(
        &mut self,
        engine: &mut crate::engine::Engine,
        key: &KeyEvent,
    ) -> ProcessResult {
        if key.release() {
            return ProcessResult::Rejected;
        }
        let ctx = &mut engine.context;
        if ctx.is_composing() {
            // C++ Editor / Navigator 传 `FallbackOptions::All`：Shift 可兜底成 Control
            if let Some(action) = self.keymap.lookup(key, fallback::ALL) {
                let action = action.to_string();
                if self.perform(ctx, &action) {
                    return ProcessResult::Accepted;
                }
            }
        }
        let Some(handler) = self.handler else {
            return ProcessResult::Noop;
        };
        if key.ctrl() || key.alt() || (key.modifier & K_SUPER_MASK != 0) {
            return ProcessResult::Noop;
        }
        // C++：ch > 0x20 && ch < 0x7f
        if let Some(ch) = key.printable_char().filter(|c| (*c as u32) > 0x20) {
            return match handler {
                CharHandler::DirectCommit => {
                    // C++ `Editor::DirectCommit`：`ctx->Commit(); return kRejected;`
                    ctx.commit();
                    ProcessResult::Rejected
                }
                CharHandler::AddToInput => {
                    ctx.push_input_ch(ch);
                    ctx.begin_editing();
                    ProcessResult::Accepted
                }
            };
        }
        ProcessResult::Noop
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx_for_test() -> Context {
        let mut c = Context::new();
        c.input = "nihao".to_string();
        c.caret_pos = 5;
        c
    }

    #[test]
    fn express_not_composing_is_noop() {
        let mut editor = Editor::express(&json!({}));
        let mut engine = crate::engine::Engine::empty_for_test();
        assert_eq!(
            editor.process_key_event(&mut engine, &KeyEvent::new(XK_RETURN, 0)),
            ProcessResult::Noop
        );
    }

    #[test]
    fn direct_commit_returns_rejected() {
        // ExpressEditor 的 char_handler 是 DirectCommit：组字中的字母直接上屏、交回前端
        let mut editor = Editor::express(&json!({}));
        let mut engine = crate::engine::Engine::empty_for_test();
        engine.context = ctx_for_test();
        engine.compose();
        assert_eq!(
            editor.process_key_event(&mut engine, &KeyEvent::new(b'z' as u32, 0)),
            ProcessResult::Rejected
        );
        // 已组字的和拼串被送出，交回前端处理 'z' 本身
        assert_eq!(engine.context.commit_text, Some("nihao".to_string()));
        assert!(engine.context.input.is_empty());
    }

    #[test]
    fn space_confirms_and_escape_cancels() {
        let config = json!({});
        let mut editor = Editor::express(&config);
        let mut engine = crate::engine::Engine::empty_for_test();
        engine.context = ctx_for_test();
        assert_eq!(
            editor.process_key_event(&mut engine, &KeyEvent::new(XK_SPACE, 0)),
            ProcessResult::Accepted
        );
        let mut editor = Editor::express(&config);
        let mut engine = crate::engine::Engine::empty_for_test();
        engine.context = ctx_for_test();
        assert_eq!(
            editor.process_key_event(&mut engine, &KeyEvent::new(XK_ESCAPE, 0)),
            ProcessResult::Accepted
        );
        assert!(engine.context.input.is_empty());
    }

    #[test]
    fn config_can_override_char_handler() {
        let config = json!({"editor": {"char_handler": "add_to_input"}});
        let editor = Editor::express(&config);
        assert_eq!(editor.handler, Some(CharHandler::AddToInput));
    }

    #[test]
    fn fluid_adds_to_input() {
        let mut editor = Editor::fluid(&json!({}));
        let mut engine = crate::engine::Engine::empty_for_test();
        engine.context.input = "ni".to_string();
        engine.context.caret_pos = 2;
        assert_eq!(
            editor.process_key_event(&mut engine, &KeyEvent::new(b'h' as u32, 0)),
            ProcessResult::Accepted
        );
        assert_eq!(engine.context.input, "nih");
    }
}
