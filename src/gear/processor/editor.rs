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
use crate::gear::processor::key_binding::{fallback, KeyBindingMap};
use crate::gear::processor::{ProcessResult, Processor};
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
        // 逗号:高亮在首位→上屏,否则向前翻页;句号:只向后翻页(用户 2026-11-07)。
        m.bind(KeyEvent::new(0x2c, 0), "smart_comma"); // XK_comma
        m.bind(KeyEvent::new(0x2e, 0), "page_next"); // XK_period
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
        m.bind(KeyEvent::new(0x2c, 0), "smart_comma"); // XK_comma
        m.bind(KeyEvent::new(0x2e, 0), "page_next"); // XK_period
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
    ///
    /// `repeat` 是系统自动重复帧（`K_REPEAT_MASK`）。目前只影响 `revert`：
    /// 长按退格只删 raw 编码，绝不回退 `committed_prefix`；单次退格保持原行为。
    fn perform(&self, ctx: &mut Context, action: &str, repeat: bool) -> bool {
        match action {
            "confirm" => {
                // 空格 = **选中当前高亮**（`Context::select`），不是 `Commit()` 整串。
                //
                // C++ `Editor::Confirm` 走 `ConfirmCurrentSelection() || Commit()`：把末段标成
                // Selected 再整串提交。`Commit` 取的是 `Composition::GetCommitText()`，而那家伙
                // 对"没被候选覆盖的剩余输入"是**原样拼回去**的 ⇒ 用户 2026-10-31 两次报的
                // "按空格把一串编码也吐出去 / 数字选完剩余的码没回到匹配"就是这条路。
                //
                // 改走 `select(highlight)`：候选覆盖整段 → 提交；只覆盖前缀 → 前缀入账、
                // 把已确认的输入从 `input` 里截掉，剩下的继续组字（`SelectResult::Continued`）。
                let index = ctx.highlight_index();
                if ctx.has_menu() {
                    let _ = ctx.select(index);
                } else if ctx.is_composing() {
                    // 没有候选（纯标点/直通）时才退回整串提交
                    ctx.commit();
                }
                true
            }
            "smart_comma" => {
                // 逗号(用户 2026-11-07):当前高亮是第一个候选 → 上屏;
                // 否则向前翻一页。无候选时返回 false 让标点冒泡(正常打逗号)。
                if !ctx.has_menu() {
                    return false;
                }
                if ctx.highlight_index() == 0 {
                    let index = ctx.highlight_index();
                    let _ = ctx.select(index);
                } else {
                    ctx.move_highlight(-(ctx.page_size as isize));
                }
                true
            }
            "page_next" => {
                // 句号:只向后翻页,不上屏。无候选时冒泡为正常句号。
                if !ctx.has_menu() {
                    return false;
                }
                ctx.move_highlight(ctx.page_size as isize);
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
                if repeat {
                    // 长按退格（自动重复帧）：只删 raw 编码，raw 编码清空即停。
                    // 绝不走 revert_last_partial / reopen_previous_selection，
                    // preedit 里已逐音节确认的 committed_prefix 保持不动。
                    return ctx.pop_input(1);
                }
                // 单次退格（原有手感）：先删剩余编码;删空后回退最近一次部分确认(改前面的字)。
                let _ = ctx.revert_last_partial()
                    || ctx.reopen_previous_selection()
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
                if self.perform(ctx, &action, key.repeat()) {
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
                    //
                    // ⚠️ 2026-11-01 诊断日志：用户报「打字突然多出一堆英文字母」，
                    // 这里是**最可疑**的一条路——组字中按到字母会把整串组合交出去。
                    // 先只记录、不改行为；复现后按日志判定要不要动。
                    eprintln!(
                        "[editor] DirectCommit: input={:?} 将整串上屏（字符 {:?}）",
                        ctx.input, ch
                    );
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
    fn space_commits_only_the_highlight_and_keeps_rest_composing() {
        // 用户 2026-10-31 报的回归：输入 "yige"、候选「一」只覆盖前两个字符，
        // 按空格应当是「一」上屏 + 剩 "ge" 继续组字，而不是把 "yige" 整串吐出去。
        use crate::engine::candidate::Candidate;
        use crate::engine::composition::Composition;
        use crate::gear::translator::translation::VecTranslation;
        use crate::engine::menu::Menu;
        use crate::engine::segmentation::Segment;

        let menu = Menu::new();
        menu.add_translation(Box::new(VecTranslation::new(vec![Candidate::new(
            "table", 0, 2, "一", 1.0,
        )])));
        let mut comp = Composition::new();
        comp.segmentation.input = "yige".to_string();
        let mut seg = Segment::new(0, 4);
        seg.menu = Some(menu);
        comp.segmentation.segments.push(seg);

        let mut engine = crate::engine::Engine::empty_for_test();
        engine.context = Context::new();
        engine.context.input = "yige".to_string();
        engine.context.caret_pos = 4;
        engine.context.composition = comp;

        let mut editor = Editor::express(&json!({}));
        assert_eq!(
            editor.process_key_event(&mut engine, &KeyEvent::new(XK_SPACE, 0)),
            ProcessResult::Accepted
        );
        // 只覆盖前缀 ⇒ 走 `SelectResult::Continued`：前缀入账到 committed_prefix，
        // 已确认的输入从 input 里截掉，剩下的继续组字（这才是"中文部分 + 剩余编码"）。
        assert_eq!(engine.context.commit_text, None);
        assert_eq!(engine.context.committed_prefix, "一");
        assert_eq!(engine.context.input, "ge");
        // 编码框必须显示「已确认汉字+剩余编码」(用户 2026-11-07 长编码需求 A)。
        // 生产路径在 select 后由 runtime compose() 重切分;测试里手动补一次。
        engine.compose();
        let pre = engine.context.get_preedit();
        assert_eq!(pre.text, "一ge", "preedit 缺已确认前缀!");

        // 再按一次空格、换成覆盖整段的候选 ⇒ 整段提交，preedit 里两截一起出去
        let menu = Menu::new();
        menu.add_translation(Box::new(VecTranslation::new(vec![Candidate::new(
            "table", 0, 2, "格", 1.0,
        )])));
        let mut seg = Segment::new(0, 2);
        seg.menu = Some(menu);
        let mut comp = Composition::new();
        comp.segmentation.input = "ge".to_string();
        comp.segmentation.segments.push(seg);
        engine.context.composition = comp;
        assert_eq!(
            editor.process_key_event(&mut engine, &KeyEvent::new(XK_SPACE, 0)),
            ProcessResult::Accepted
        );
        assert_eq!(engine.context.commit_text.as_deref(), Some("一格"));
        assert!(engine.context.input.is_empty());
    }

    #[test]
    fn repeat_backspace_deletes_one_code_char() {
        // 长按退格的后续帧必须和首次按下走同一条 "revert"，每次删一个编码位。
        let mut editor = Editor::express(&json!({}));
        let mut engine = crate::engine::Engine::empty_for_test();
        engine.context = ctx_for_test();
        engine.compose();
        let repeat = KeyEvent::new(XK_BACKSPACE, crate::engine::key_event::K_REPEAT_MASK);
        let before = engine.context.input.clone();
        assert_eq!(
            editor.process_key_event(&mut engine, &repeat),
            ProcessResult::Accepted
        );
        assert_eq!(engine.context.input, before[..before.len() - 1]);
    }

    #[test]
    fn auto_repeated_backspace_deletes_whole_code_then_stops() {
        // 模拟按住退格：首帧 + 后续重复帧，每帧删一个编码位；编码删空后引擎
        // 必须回 Noop，由 TIP 拦截剩余重复帧（否则宿主会删已上屏文字）。
        let mut editor = Editor::express(&json!({}));
        let mut engine = crate::engine::Engine::empty_for_test();
        engine.context = ctx_for_test();
        engine.compose();
        let repeat = KeyEvent::new(XK_BACKSPACE, crate::engine::key_event::K_REPEAT_MASK);
        for expected in ["niha", "nih", "ni", "n", ""] {
            assert_eq!(
                editor.process_key_event(&mut engine, &repeat),
                ProcessResult::Accepted,
                "输入还剩余码时，重复帧应当继续删编码"
            );
            // 生产路径（runtime）每帧后都会重新组字；这里手动补一次，保持分段同步。
            engine.compose();
            assert_eq!(engine.context.input, expected);
        }
        assert_eq!(
            editor.process_key_event(&mut engine, &repeat),
            ProcessResult::Noop,
            "编码已清空，引擎不能再接受退格"
        );
    }

    #[test]
    fn repeat_revert_keeps_committed_prefix_and_single_press_still_reverts_it() {
        let editor = Editor::express(&json!({}));
        let mut ctx = Context::new();
        ctx.input = "ni".to_string();
        ctx.caret_pos = 2;
        ctx.committed_prefix = "你".to_string();
        ctx.partial_history.push((1, "ni".to_string()));

        // 长按重复帧：只削 raw 编码，逐音节确认的前缀不动。
        assert!(editor.perform(&mut ctx, "revert", true));
        assert_eq!(ctx.input, "n");
        assert_eq!(ctx.committed_prefix, "你");
        assert!(editor.perform(&mut ctx, "revert", true));
        assert_eq!(ctx.input, "");
        assert_eq!(ctx.committed_prefix, "你");

        // raw 编码已空：重复帧不再处理任何东西，更不会回退前缀。
        assert!(!editor.perform(&mut ctx, "revert", true));
        assert_eq!(ctx.committed_prefix, "你");

        // 单次退格（repeat=false）行为不变：立即回退最近一次已确认前缀。
        assert!(editor.perform(&mut ctx, "revert", false));
        assert_eq!(ctx.committed_prefix, "");
        assert_eq!(ctx.input, "ni");
    }

    #[test]
    fn repeat_key_event_keeps_committed_prefix() {
        let mut editor = Editor::express(&json!({}));
        let mut engine = crate::engine::Engine::empty_for_test();
        let mut ctx = Context::new();
        ctx.input = "i".to_string();
        ctx.caret_pos = 1;
        ctx.committed_prefix = "你".to_string();
        ctx.partial_history.push((1, "ni".to_string()));
        engine.context = ctx;
        engine.compose();

        let repeat = KeyEvent::new(XK_BACKSPACE, crate::engine::key_event::K_REPEAT_MASK);
        // 还有一格 raw 编码：重复帧只删它。
        assert_eq!(
            editor.process_key_event(&mut engine, &repeat),
            ProcessResult::Accepted
        );
        assert_eq!(engine.context.input, "");
        assert_eq!(engine.context.committed_prefix, "你");
        engine.compose();

        // raw 编码已空：引擎拒绝继续处理，前缀仍保持不动。
        assert_eq!(
            editor.process_key_event(&mut engine, &repeat),
            ProcessResult::Noop
        );
        assert_eq!(engine.context.committed_prefix, "你");

        // 单次退格仍走原 revert：回退已确认前缀，并把编码按原逻辑拼在剩余输入前面。
        engine.context.set_input("i");
        let single = KeyEvent::new(XK_BACKSPACE, 0);
        assert_eq!(
            editor.process_key_event(&mut engine, &single),
            ProcessResult::Accepted
        );
        assert_eq!(engine.context.committed_prefix, "");
        assert_eq!(engine.context.input, "nii");
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
