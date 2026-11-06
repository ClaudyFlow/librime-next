//! `gear/keypad_processor.cc`（librime-evo，amzxyz）移植：小键盘数字的三种策略。
//!
//! 上游 librime 没有这个组件；方案里把它排在 `key_binder` **前面**，
//! 用来决定「按小键盘数字」到底是**选字**还是**输入数字**：
//!
//! ```yaml
//! engine:
//!   processors:
//!     - keypad_processor      # 必须在 key_binder 之前
//!     - key_binder
//! keypad_processor:
//!   mode: select              # auto | compose | select（默认 select）
//! ```
//!
//! * `select`  —— 小键盘数字当选字键（原样放行给后面的 `selector` / `speller`）；
//! * `compose` —— 小键盘数字当**输入字符**：数字进组合串，整段标成 `raw`（已选中），
//!   于是数字直接落到预编辑里，不参与分词查表 —— 打「数字/金额」最省事；
//! * `auto`    —— 组字中走 `compose`，没组字时放行。
//!
//! 一旦进了 raw 态，输入就"锁"在小键盘上：退格删一个字符、字母数字继续追加，
//! 全程保持 `raw` 段，直到组合结束（`IsComposing()` 为假）才退出。
//!
//! 与 C++ 的对应关系：
//! * `KeypadProcessor::ProcessKeyEvent` → [`Processor::process_key_event`]
//! * 匿名命名空间里的 `IsKeypadDigit` / `IsRawCharacter` /
//!   `DigitFromKeycode` / `RawCharacterFromKeyEvent` / `MarkInputAsRaw`
//!   → 本文件的同名私有函数。

use serde_json::Value;

use crate::engine::context::Context;
use crate::engine::gear::processor::{ProcessResult, Processor};
use crate::engine::key_event::KeyEvent;
use crate::engine::runtime::Engine;
use crate::engine::segmentation::{SegStatus, Segment};

/// 小键盘 0 / 9 的 keysym（`key_names.rs` 里 `KP_0`…`KP_9`）。
const XK_KP_0: u32 = 0xffb0;
const XK_KP_9: u32 = 0xffb9;
/// `Segment::kSelected` 段上那个标记：告诉后面的分词器「这段是原样输入，别拆」。
const RAW_TAG: &str = "raw";

/// 小键盘策略（C++ `KeypadProcessor::Mode`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeypadMode {
    Auto,
    Compose,
    Select,
}

impl KeypadMode {
    fn name(self) -> &'static str {
        match self {
            KeypadMode::Auto => "auto",
            KeypadMode::Compose => "compose",
            KeypadMode::Select => "select",
        }
    }
}

pub struct KeypadProcessor {
    mode: KeypadMode,
    /// 是否处在"小键盘直输"状态（C++ `raw_mode_`）。
    raw_mode: bool,
}

impl KeypadProcessor {
    pub fn new(config: &Value) -> Self {
        let mut mode = KeypadMode::Select;
        if let Some(text) = config
            .pointer("/keypad_processor/mode")
            .and_then(|v| v.as_str())
        {
            mode = match text {
                "auto" => KeypadMode::Auto,
                "compose" => KeypadMode::Compose,
                "select" => KeypadMode::Select,
                other => {
                    eprintln!("[keypad_processor] invalid keypad_processor/mode: {other}; using select.");
                    KeypadMode::Select
                }
            };
        }
        KeypadProcessor {
            mode,
            raw_mode: false,
        }
    }

    pub fn mode(&self) -> KeypadMode {
        self.mode
    }

    /// 是否处在 raw 直输态（测试/诊断用）。
    pub fn raw_mode(&self) -> bool {
        self.raw_mode
    }
}

fn is_keypad_digit(keycode: u32) -> bool {
    (XK_KP_0..=XK_KP_9).contains(&keycode)
}

fn is_raw_character(keycode: u32) -> bool {
    (0x30..=0x39).contains(&keycode)      // 0-9
        || (0x61..=0x7a).contains(&keycode) // a-z
        || (0x41..=0x5a).contains(&keycode) // A-Z
}

fn digit_from_keycode(keycode: u32) -> char {
    if is_keypad_digit(keycode) {
        char::from(b'0' + (keycode - XK_KP_0) as u8)
    } else {
        char::from(b'0' + (keycode - 0x30) as u8)
    }
}

/// `RawCharacterFromKeyEvent`：按 Shift / CapsLock 把字母翻成对应大小写。
fn raw_character_from_key_event(key: &KeyEvent) -> char {
    let keycode = key.keycode;
    let mut ch = char::from(keycode as u8);
    if (0x61..=0x7a).contains(&keycode) && (key.shift() || key.caps()) {
        ch = char::from(keycode as u8 - b'a' + b'A');
    } else if (0x41..=0x5a).contains(&keycode) && !key.shift() && !key.caps() {
        ch = char::from(keycode as u8 - b'A' + b'a');
    }
    ch
}

/// `MarkInputAsRaw`：把整条输入标成一个 `raw` 已选段（不再分词）。
fn mark_input_as_raw(context: &mut Context) {
    let len = context.input.len();
    context.composition.segmentation.segments.clear();
    let mut raw = Segment::new(0, len);
    raw.status = SegStatus::Selected;
    raw.tags.push(RAW_TAG.to_string());
    context.composition.segmentation.push_back(raw);
    context.composition.segmentation.forward();
}

impl Processor for KeypadProcessor {
    fn process_key_event(&mut self, engine: &mut Engine, key: &KeyEvent) -> ProcessResult {
        let context = &mut engine.context;
        if !context.is_composing() {
            self.raw_mode = false;
        }
        // raw 态下的退格：删一个字符、重新标记整段（C++ 只在无修饰键时接管）
        if self.raw_mode
            && key.keycode == 0xff08 // XK_BackSpace
            && !key.release()
            && !key.shift()
            && !key.ctrl()
            && !key.alt()
            && !key.super_key()
        {
            if !context.pop_input(1) {
                return ProcessResult::Accepted;
            }
            if context.input.is_empty() {
                self.raw_mode = false;
            } else {
                mark_input_as_raw(context);
            }
            return ProcessResult::Accepted;
        }

        if !is_keypad_digit(key.keycode) {
            // raw 态下字母/数字继续追加，保持"原样输入"
            if self.raw_mode
                && !key.release()
                && !key.ctrl()
                && !key.alt()
                && !key.super_key()
                && is_raw_character(key.keycode)
            {
                let ch = raw_character_from_key_event(key);
                if !context.push_input_ch(ch) {
                    return ProcessResult::Noop;
                }
                mark_input_as_raw(context);
                return ProcessResult::Accepted;
            }
            return ProcessResult::Noop;
        }

        // 小键盘数字本身带修饰键 ⇒ 不归这里管（Ctrl+小键盘另有含义）
        if key.release() || key.shift() || key.ctrl() || key.alt() || key.super_key() {
            return ProcessResult::Noop;
        }

        let digit = digit_from_keycode(key.keycode);
        let composing = context.is_composing();
        let compose = self.mode == KeypadMode::Compose
            || (self.mode == KeypadMode::Auto && composing);
        if !compose {
            // select：放行给后面的 selector / speller 去选字
            return ProcessResult::Noop;
        }
        if !context.push_input_ch(digit) {
            return ProcessResult::Noop;
        }
        self.raw_mode = true;
        mark_input_as_raw(context);
        // 下面这行只在诊断时开：每键一次字符串拼接，正式路径不要留
        // eprintln!("[keypad_processor] key={} mode={} composing={composing} digit={digit}",
        //     key.keycode, self.mode.name());
        ProcessResult::Accepted
    }
}

#[cfg(test)]
mod tests {
    //! 对照 librime-evo `test/keypad_processor_test.cc` 的十种情形。
    use super::*;
    use crate::engine::key_event::{K_RELEASE_MASK, K_SHIFT_MASK};
    use serde_json::json;

    const XK_1: u32 = 0x31;
    const XK_a: u32 = 0x61;
    const XK_b: u32 = 0x62;
    const XK_BACK_SPACE: u32 = 0xff08;

    fn engine_with(input: &str) -> Engine {
        let mut e = Engine::empty_for_test();
        if !input.is_empty() {
            e.context.push_input_str(input);
        }
        e
    }

    fn raw_segment_is_selected(engine: &Engine) -> bool {
        engine
            .context
            .composition
            .segmentation
            .segments
            .first()
            .map(|seg| seg.status == SegStatus::Selected && seg.tags.iter().any(|t| t == RAW_TAG))
            .unwrap_or(false)
    }

    #[test]
    fn compose_mode_pushes_keypad_digit() {
        let mut p = KeypadProcessor::new(&json!({"keypad_processor": {"mode": "compose"}}));
        let mut engine = engine_with("");
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(XK_KP_0 + 1, 0)),
            ProcessResult::Accepted
        );
        assert_eq!(engine.context.input, "1");
        assert!(raw_segment_is_selected(&engine), "整段应标成 raw 已选中");
        assert!(p.raw_mode());
    }

    #[test]
    fn auto_mode_passes_idle_keypad_digit() {
        let mut p = KeypadProcessor::new(&json!({"keypad_processor": {"mode": "auto"}}));
        let mut engine = engine_with("");
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(XK_KP_0 + 1, 0)),
            ProcessResult::Noop
        );
        assert_eq!(engine.context.input, "");
        assert!(!p.raw_mode());
    }

    #[test]
    fn auto_mode_pushes_while_composing() {
        let mut p = KeypadProcessor::new(&json!({"keypad_processor": {"mode": "auto"}}));
        let mut engine = engine_with("ni");
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(XK_KP_0 + 2, 0)),
            ProcessResult::Accepted
        );
        assert_eq!(engine.context.input, "ni2");
        assert!(raw_segment_is_selected(&engine));
    }

    #[test]
    fn auto_mode_appends_to_numeric_input() {
        let mut p = KeypadProcessor::new(&json!({"keypad_processor": {"mode": "auto"}}));
        let mut engine = engine_with("1");
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(XK_KP_0 + 2, 0)),
            ProcessResult::Accepted
        );
        assert_eq!(engine.context.input, "12");
    }

    #[test]
    fn keypad_digits_keep_letter_prefix_raw() {
        let mut p = KeypadProcessor::new(&json!({"keypad_processor": {"mode": "auto"}}));
        let mut engine = engine_with("ni");
        for d in 4..=6 {
            assert_eq!(
                p.process_key_event(&mut engine, &KeyEvent::new(XK_KP_0 + d, 0)),
                ProcessResult::Accepted
            );
        }
        assert_eq!(engine.context.input, "ni456");
        // 只有一段 raw 段 + forward 出来的空段
        let segs = &engine.context.composition.segmentation.segments;
        assert_eq!(segs[0].end, 5, "raw 段覆盖整条输入");
        assert!(raw_segment_is_selected(&engine));
    }

    #[test]
    fn compose_mode_keeps_following_main_keys_raw() {
        let mut p = KeypadProcessor::new(&json!({"keypad_processor": {"mode": "compose"}}));
        let mut engine = engine_with("");
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(XK_KP_0 + 4, 0)),
            ProcessResult::Accepted
        );
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(XK_a, 0)),
            ProcessResult::Accepted
        );
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(XK_1, 0)),
            ProcessResult::Accepted
        );
        assert_eq!(engine.context.input, "4a1");
        assert!(raw_segment_is_selected(&engine));
    }

    #[test]
    fn backspace_keeps_raw_mode() {
        let mut p = KeypadProcessor::new(&json!({"keypad_processor": {"mode": "compose"}}));
        let mut engine = engine_with("");
        p.process_key_event(&mut engine, &KeyEvent::new(XK_KP_0 + 4, 0));
        p.process_key_event(&mut engine, &KeyEvent::new(XK_a, 0));
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(XK_BACK_SPACE, 0)),
            ProcessResult::Accepted
        );
        assert_eq!(engine.context.input, "4");
        assert!(p.raw_mode(), "删完还剩字符，raw 态要保持");
        p.process_key_event(&mut engine, &KeyEvent::new(XK_b, 0));
        assert_eq!(engine.context.input, "4b");
        assert!(raw_segment_is_selected(&engine));
    }

    #[test]
    fn select_mode_passes_keypad_digit() {
        let mut p = KeypadProcessor::new(&json!({"keypad_processor": {"mode": "select"}}));
        let mut engine = engine_with("");
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(XK_KP_0 + 3, 0)),
            ProcessResult::Noop
        );
        assert_eq!(engine.context.input, "");
    }

    #[test]
    fn modifiers_and_release_pass_through() {
        let mut p = KeypadProcessor::new(&json!({"keypad_processor": {"mode": "compose"}}));
        let mut engine = engine_with("");
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(XK_KP_0 + 1, K_SHIFT_MASK)),
            ProcessResult::Noop
        );
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(XK_KP_0 + 1, K_RELEASE_MASK)),
            ProcessResult::Noop
        );
        assert_eq!(engine.context.input, "");
    }

    #[test]
    fn main_keyboard_digit_is_passed_to_selector() {
        let mut p = KeypadProcessor::new(&json!({"keypad_processor": {"mode": "compose"}}));
        let mut engine = engine_with("");
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(XK_1, 0)),
            ProcessResult::Noop
        );
        assert_eq!(engine.context.input, "");
    }

    #[test]
    fn unknown_mode_falls_back_to_select() {
        let p = KeypadProcessor::new(&json!({"keypad_processor": {"mode": "nonsense"}}));
        assert_eq!(p.mode(), KeypadMode::Select);
        // 缺配置同样是 select
        assert_eq!(
            KeypadProcessor::new(&json!({})).mode(),
            KeypadMode::Select
        );
    }
}
