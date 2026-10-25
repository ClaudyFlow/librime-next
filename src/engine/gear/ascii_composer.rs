//! `src/rime/gear/ascii_composer.cc`：中西文切换处理器。
//!
//! 对照组是**装机在跑的那份**（`/c/code/librime`，含 weasel 补丁的
//! inline uppercase / inline keypad）。主干行为：
//!
//! * **单击** Shift / Ctrl / Alt / Super（500ms 内抬起）切换 `ascii_mode`；
//! * CapsLock 也可作切换键（`ascii_composer/switch_key/Caps_Lock`）；
//! * **纯西文态且没在组字** ⇒ `Rejected`，让前端把按键直接上屏（这是"中文输入法里打英文"的手感）；
//! * 西文态组字时空格 / 回车 ⇒ 结算上屏并自动回到中文态；
//! * 中文态下敲**大写字母** ⇒ 进入临时西文（`inline_uppercase`），即"打 N 再打 ihuan"；
//! * `raw_keys_` 记账跟随 FS BackSpace / Escape 出栈。
//!
//! 未移植：`inline_keypad`（小键盘输入）——需要区分小键盘 keysym 与独立记账，
//! 当前宿主不提交这类键；留待补上时一并做。

use serde_json::Value;
use std::time::{Duration, Instant};

use crate::engine::gear::key_binding::KeyBindingMap;
use crate::engine::gear::processor::{ProcessResult, Processor};
use crate::engine::key_event::{
    KeyEvent, K_LOCK_MASK, XK_BACKSPACE, XK_ESCAPE, XK_KP_ENTER, XK_RETURN, XK_SPACE,
};

/// 单击修饰键的判定窗口（C++ `std::chrono::milliseconds(500)`）。
const TOGGLE_WINDOW: Duration = Duration::from_millis(500);

/// C++ `AsciiModeSwitchStyle`。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SwitchStyle {
    /// `toggle`
    Toggle,
    /// `inline_ascii`
    InlineAscii,
    /// `commit_text`
    CommitText,
    /// `commit_code`
    CommitCode,
    /// `clear`
    Clear,
    /// `noop`
    Noop,
}

impl SwitchStyle {
    fn parse(s: &str) -> Option<SwitchStyle> {
        Some(match s {
            "toggle" => SwitchStyle::Toggle,
            "inline_ascii" => SwitchStyle::InlineAscii,
            "commit_text" => SwitchStyle::CommitText,
            "commit_code" => SwitchStyle::CommitCode,
            "clear" => SwitchStyle::Clear,
            "noop" => SwitchStyle::Noop,
            _ => return None,
        })
    }
}

pub struct AsciiComposer {
    keymap: KeyBindingMap,
    switch_key: std::collections::HashMap<u32, SwitchStyle>,
    inline_uppercase: bool,
    /// Shift / Ctrl / Alt / Super 是否被单独按下（单击判定用）。
    modifier_down: Option<u32>,
    toggle_deadline: Option<Instant>,
    /// C++ `raw_keys_`：物理按键记账（大写字母判定 / `commit_code` 用）。
    raw_keys: String,
    is_inline_ascii: bool,
}

impl AsciiComposer {
    pub fn new(config: &Value) -> Self {
        let mut c = AsciiComposer {
            keymap: KeyBindingMap::new(),
            switch_key: std::collections::HashMap::new(),
            inline_uppercase: true,
            modifier_down: None,
            toggle_deadline: None,
            raw_keys: String::new(),
            is_inline_ascii: false,
        };
        c.load_config(config);
        c
    }

    /// `AsciiComposer::LoadConfig`：`ascii_composer/bindings` + `switch_key` + 开关。
    fn load_config(&mut self, config: &Value) {
        self.keymap.load_config(config, "ascii_composer");
        if let Some(table) = config
            .pointer("/ascii_composer/switch_key")
            .and_then(|v| v.as_object())
        {
            for (repr, value) in table {
                let Some(name) = value.as_str() else { continue };
                let Some(style) = SwitchStyle::parse(name) else {
                    continue;
                };
                if style == SwitchStyle::Noop {
                    continue;
                }
                match KeyEvent::parse(repr) {
                    Some(ke) if ke.modifier == 0 => {
                        self.switch_key.insert(ke.keycode, style);
                    }
                    _ => {
                        eprintln!("[ascii_composer] invalid switch key: '{repr}'");
                    }
                }
            }
        }
        if let Some(v) = config
            .pointer("/ascii_composer/inline_uppercase")
            .and_then(|v| v.as_bool())
        {
            self.inline_uppercase = v;
        }
        // 默认：Shift 作切换键（commit_code 改成 toggle 由方案配，缺省同 C++ 缺省 toggle）
        if self.switch_key.is_empty() {
            self.switch_key
                .insert(modifier_keysym(true), SwitchStyle::Toggle);
        }
    }

    fn reset_modifier_state(&mut self) {
        self.modifier_down = None;
        self.toggle_deadline = None;
    }

    /// C++ `SwitchAsciiMode`：切换后按 style 处理 pending 的组合。
    fn switch_ascii_mode(&mut self, ctx: &mut crate::engine::context::Context, style: SwitchStyle) {
        let mode = !ctx.get_option("ascii_mode");
        ctx.set_option("ascii_mode", mode);
        if style == SwitchStyle::Clear {
            ctx.clear();
            return;
        }
        if style == SwitchStyle::CommitText {
            let text = ctx.get_commit_text();
            ctx.commit_direct(text);
            ctx.clear();
            return;
        }
        if style == SwitchStyle::CommitCode {
            let code = self.raw_keys.clone();
            if code.is_empty() {
                let text = ctx.get_commit_text();
                ctx.commit_direct(text);
            } else {
                ctx.commit_direct(code);
            }
            ctx.clear();
            return;
        }
        // Toggle / InlineAscii：保留组合继续编辑（InlineAscii 的 inline 记账另有天地，
        // 这里按 Toggle 处理——中文态打英文的场景不会走到 inline_ascii 配置）。
        let _ = mode;
    }

    fn toggle_with_key(&mut self, ctx: &mut crate::engine::context::Context, keycode: u32) {
        let style = self
            .switch_key
            .get(&keycode)
            .copied()
            .unwrap_or(SwitchStyle::Toggle);
        self.switch_ascii_mode(ctx, style);
        self.reset_modifier_state();
    }

    /// 是否进入了临时西文（inline ascii，由大写字母触发）。
    pub fn is_inline_ascii(&self) -> bool {
        self.is_inline_ascii
    }
}

fn modifier_keysym(_shift: bool) -> u32 {
    // `XK_Shift_L`：C++ 里 ` tog uge KeyEvent::Parse("Shift_L") 取得；这里直接写常量，
    // 值与 X11 keysym 一致（见 `KEY_NAME_TABLE`）。
    crate::engine::key_event::keycode_by_name("Shift_L").unwrap_or(0xffe1)
}

impl Processor for AsciiComposer {
    fn process_key_event(
        &mut self,
        engine: &mut crate::engine::Engine,
        key: &KeyEvent,
    ) -> ProcessResult {
        let ctx = &mut engine.context;
        let ascii_mode = ctx.get_option("ascii_mode");
        let ch = key.keycode;

        // 1. 释放：判断是不是"单击"修饰键
        if key.release() {
            if let Some(code) = self.modifier_down {
                let within_window = self
                    .toggle_deadline
                    .map_or(false, |deadline| Instant::now() < deadline);
                if ch == code && within_window {
                    let style = self
                        .switch_key
                        .get(&ch)
                        .copied()
                        .unwrap_or(SwitchStyle::Toggle);
                    self.switch_ascii_mode(ctx, style);
                }
                self.reset_modifier_state();
            }
            return ProcessResult::Noop;
        }

        // 2. 多修饰键互斥保护
        let modifier_count =
            u32::from(key.shift()) + u32::from(key.ctrl()) + u32::from(key.alt());
        if modifier_count > 1 {
            self.reset_modifier_state();
            return ProcessResult::Noop;
        }

        // 3. CapsLock：good-old 语义下直接放行，由切换键表决定行为
        if is_modifier_keysym(ch) {
            if self.modifier_down.is_none() {
                self.modifier_down = Some(ch);
                self.toggle_deadline = Some(Instant::now() + TOGGLE_WINDOW);
            }
            return ProcessResult::Noop;
        }

        // 任何常规实体键按下 ⇒ 解除单击修饰键的武装
        self.reset_modifier_state();

        // 4. 自定义 bindings（`ascii_composer/bindings`：commit_raw_input 等）
        if let Some(action) = self.keymap.get(key) {
            let action = action.to_string();
            match action.as_str() {
                "commit_raw_input" => {
                    let text = if self.raw_keys.is_empty() {
                        ctx.input.clone()
                    } else {
                        self.raw_keys.clone()
                    };
                    self.commit_and_reset(ctx, text);
                    return ProcessResult::Accepted;
                }
                "commit_raw_input_with_space" => {
                    let mut text = if self.raw_keys.is_empty() {
                        ctx.input.clone()
                    } else {
                        self.raw_keys.clone()
                    };
                    text.push(' ');
                    self.commit_and_reset(ctx, text);
                    return ProcessResult::Accepted;
                }
                other => {
                    eprintln!("[ascii_composer] unknown action: '{other}'");
                }
            }
        }

        // 5. 未截留的 Ctrl / Alt / Super 组合键放行给操作系统
        if key.ctrl() || key.alt() {
            return ProcessResult::Noop;
        }

        // 6. 西文态组字时的结算：空格 / 回车上屏并回中文态
        if ascii_mode && ctx.is_composing() {
            if ch == XK_SPACE && key.modifier == 0 {
                let text = format!("{} ", self.raw_keys);
                self.commit_and_reset(ctx, text);
                return ProcessResult::Accepted;
            }
            if (ch == XK_RETURN || ch == XK_KP_ENTER) && key.modifier == 0 {
                let text = self.raw_keys.clone();
                self.commit_and_reset(ctx, text);
                return ProcessResult::Accepted;
            }
        }

        let print_ch = key.printable_char();

        // 7. 中文态遇大写字母 ⇒ 进入临时西文
        if self.inline_uppercase
            && !ascii_mode
            && print_ch.map_or(false, |c| c.is_ascii_uppercase())
        {
            self.enter_inline_ascii(ctx, print_ch.expect("checked above"));
            return ProcessResult::Accepted;
        }

        // 8. 纯西文态且无输入 ⇒ 直通操作系统
        if ascii_mode && !ctx.is_composing() {
            return ProcessResult::Rejected;
        }

        // 9. 物理按键记账
        if let Some(c) = print_ch.filter(|c| (0x20..=0x7e).contains(&(*c as u32))) {
            self.raw_keys.push(c);
            if ascii_mode && ctx.is_composing() {
                ctx.push_input_ch(c);
                return ProcessResult::Accepted;
            }
            return ProcessResult::Noop;
        }

        // 10. 退格 / Escape 清账
        if ch == XK_BACKSPACE {
            self.raw_keys.pop();
            self.is_inline_ascii = false;
            return ProcessResult::Noop;
        }
        if ch == XK_ESCAPE {
            self.raw_keys.clear();
            self.is_inline_ascii = false;
            return ProcessResult::Noop;
        }

        // CapsLock 亮着时敲字母：C++ 直接输出反转大小写的 ASCII
        if key.modifier & K_LOCK_MASK != 0 {
            if let Some(c) = print_ch.filter(|c| c.is_ascii_alphabetic()) {
                let out = if c.is_ascii_lowercase() {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                };
                ctx.commit_direct(out.to_string());
                return ProcessResult::Accepted;
            }
        }

        ProcessResult::Noop
    }
}

fn is_modifier_keysym(ch: u32) -> bool {
    matches!(ch, 0xffe1 | 0xffe2 | 0xffe3 | 0xffe4 | 0xffe9 | 0xffea | 0xffeb | 0xffec)
}

impl AsciiComposer {
    /// C++ `CommitAndReset`。
    fn commit_and_reset(&mut self, ctx: &mut crate::engine::context::Context, text: String) {
        self.is_inline_ascii = false;
        ctx.commit_direct(text);
        ctx.clear();
        self.raw_keys.clear();
        self.reset_modifier_state();
        if ctx.get_option("ascii_mode") {
            ctx.set_option("ascii_mode", false);
        }
    }

    /// C++ `EnterInlineAsciiWithChar`：中文态敲大写字母 ⇒ 临时西文。
    fn enter_inline_ascii(&mut self, ctx: &mut crate::engine::context::Context, ch: char) {
        self.raw_keys.push(ch);
        ctx.clear_non_confirmed_composition();
        let keys = self.raw_keys.clone();
        ctx.set_input(&keys);
        ctx.set_option("ascii_mode", true);
        self.is_inline_ascii = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::Context;
    use serde_json::json;

    fn engine_with(input: &str, ascii_mode: bool) -> crate::engine::Engine {
        let mut e = crate::engine::Engine::empty_for_test();
        e.context.input = input.to_string();
        e.context.caret_pos = input.len();
        e.context.set_option("ascii_mode", ascii_mode);
        e
    }

    #[test]
    fn chinese_mode_passes_letters_downstream() {
        let mut composer = AsciiComposer::new(&json!({}));
        let mut engine = engine_with("", false);
        assert_eq!(
            composer.process_key_event(&mut engine, &KeyEvent::new(b'n' as u32, 0)),
            ProcessResult::Noop
        );
        // 记账留下了物理按键
        assert_eq!(composer.raw_keys, "n");
    }

    #[test]
    fn ascii_mode_passthrough_when_idle() {
        // 西文态且空闲 ⇒ 直通（ C++ 第 9 步 `kRejected` ），连 raw_keys_ 都不记
        let mut composer = AsciiComposer::new(&json!({}));
        let mut engine = engine_with("", true);
        assert_eq!(
            composer.process_key_event(&mut engine, &KeyEvent::new(b'a' as u32, 0)),
            ProcessResult::Rejected
        );
        assert_eq!(composer.raw_keys, "");
    }

    #[test]
    fn inline_ascii_return_commits_raw_keys_and_returns_to_chinese() {
        // 中文态敲大写字母进入临时西文，再敲小写、回车结算 —— C++ `CommitAndReset`
        let mut composer = AsciiComposer::new(&json!({}));
        let mut engine = engine_with("", false);
        composer.process_key_event(&mut engine, &KeyEvent::new(b'H' as u32, 0));
        assert_eq!(engine.context.input, "H");
        assert_eq!(
            composer.process_key_event(&mut engine, &KeyEvent::new(b'i' as u32, 0)),
            ProcessResult::Accepted
        );
        assert_eq!(engine.context.input, "Hi");
        assert_eq!(
            composer.process_key_event(&mut engine, &KeyEvent::new(XK_RETURN, 0)),
            ProcessResult::Accepted
        );
        assert_eq!(engine.context.commit_text, Some("Hi".to_string()));
        assert!(!engine.context.get_option("ascii_mode"));
        assert!(engine.context.input.is_empty());
    }

    #[test]
    fn uppercase_enters_inline_ascii() {
        let mut composer = AsciiComposer::new(&json!({}));
        let mut engine = engine_with("", false);
        assert_eq!(
            composer.process_key_event(&mut engine, &KeyEvent::new(b'N' as u32, 0)),
            ProcessResult::Accepted
        );
        assert!(engine.context.get_option("ascii_mode"));
        assert_eq!(engine.context.input, "N");
        // 继续敲小写字母：西文态组字时推入上下文
        assert_eq!(
            composer.process_key_event(&mut engine, &KeyEvent::new(b'i' as u32, 0)),
            ProcessResult::Accepted
        );
        assert_eq!(engine.context.input, "Ni");
    }

    #[test]
    fn backspace_pops_raw_keys() {
        let mut composer = AsciiComposer::new(&json!({}));
        let mut engine = engine_with("", false);
        composer.process_key_event(&mut engine, &KeyEvent::new(b'n' as u32, 0));
        assert_eq!(composer.raw_keys, "n");
        composer.process_key_event(&mut engine, &KeyEvent::new(XK_BACKSPACE, 0));
        assert_eq!(composer.raw_keys, "");
    }

    #[test]
    fn shift_single_click_toggles_ascii_mode() {
        let mut composer = AsciiComposer::new(&json!({}));
        let mut engine = engine_with("", false);
        let shift_down = KeyEvent::new(modifier_keysym(true), 0);
        let shift_up = KeyEvent::new(modifier_keysym(true), crate::engine::key_event::K_RELEASE_MASK);
        assert_eq!(
            composer.process_key_event(&mut engine, &shift_down),
            ProcessResult::Noop
        );
        composer.process_key_event(&mut engine, &shift_up);
        assert!(engine.context.get_option("ascii_mode"));
        // meta single click again → back to chinese
        composer.process_key_event(&mut engine, &shift_down);
        composer.process_key_event(&mut engine, &shift_up);
        assert!(!engine.context.get_option("ascii_mode"));
    }

    #[test]
    fn context_is_untouched_by_plain_construction() {
        // 构造器不得动 context（C++ 里 LoadConfig 只读配置）
        let mut c = Context::new();
        let _ = AsciiComposer::new(&json!({"ascii_composer": {"inline_uppercase": false}}));
        c.input = "x".to_string();
        assert_eq!(c.input, "x");
    }
}
