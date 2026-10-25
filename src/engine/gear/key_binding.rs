//! `src/rime/gear/key_binding_processor.h`：`KeyEvent → action 名` 的绑定表。
//!
//! # 查表是**精确匹配**，`kModifierMask` 不参与
//!
//! C++ `KeyBindingProcessor::Accept` 是 `keymap.find(key_event)`，而
//! `KeyEvent::operator==`（`key_event.h:43-45`）比的是**完整的**
//! `(keycode, modifier)` —— 掩码 `kModifierMask` 只在 `repr()` 里用。
//! 实测后果：写着 `- {accept: Return}` 的绑定，**不会**被 `Release+Return` 命中
//! （`kReleaseMask` 是 bit 30，恰好落在 `kModifierMask` 里）。这里跟着做精确匹配。
//!
//! # Shift 的两次兜底
//!
//! C++ 在精确匹配失败后还会按 `FallbackOptions` 试两次
//! （`key_binding_processor_impl.h:22-40`）：带 Shift 时把 Shift 换成
//! Control 再查（`ShiftAsControl`）、或直接去掉 Shift 再查（`IgnoreShift`）；
//! **Ctrl / Alt 按下则不做任何兜底**。Editor 与 Navigator 传 `All`，
//! AsciiComposer 传 `None`。
//!
//! # 后绑定覆盖前绑定
//!
//! C++ 用 `std::map`，同一个键绑定两次时后者生效；这里用 `Vec` 逆序取首个匹配，
//! 语义一致。

use serde_json::Value;

use crate::engine::key_event::{KeyEvent, K_CONTROL_MASK, K_SHIFT_MASK};

/// C++ `KeyBindingProcessor::FallbackOptions`。
pub mod fallback {
    pub const NONE: u8 = 0;
    /// 带 Shift 时把 Shift 换成 Control 再查一次。
    pub const SHIFT_AS_CONTROL: u8 = 1 << 0;
    /// 带 Shift 时去掉 Shift 再查一次。
    pub const IGNORE_SHIFT: u8 = 1 << 1;
    /// Editor / Navigator 用的是这个。
    pub const ALL: u8 = SHIFT_AS_CONTROL | IGNORE_SHIFT;
}

/// 按键绑定表（`KeyBindingProcessor` 的 `Keymap`）。
#[derive(Default)]
pub struct KeyBindingMap {
    /// `(keycode, modifier, action)`；`modifier` 完整保存，查表时精确比。
    bindings: Vec<(u32, u32, String)>,
}

impl KeyBindingMap {
    pub fn new() -> Self {
        KeyBindingMap {
            bindings: Vec::new(),
        }
    }

    /// 绑定键 → 动作（`Keymap::Bind`）。同一键重复绑定 ⇒ 后者生效。
    pub fn bind(&mut self, key: KeyEvent, action: &str) {
        self.bindings.push((key.keycode, key.modifier, action.to_string()));
    }

    fn find(&self, key: &KeyEvent) -> Option<&str> {
        self.bindings
            .iter()
            .rev()
            .find(|(code, mask, _)| *code == key.keycode && *mask == key.modifier)
            .map(|(_, _, action)| action.as_str())
    }

    /// 精确匹配 + Shift 兜底（C++ `KeyBindingProcessor::ProcessKeyEvent`）。
    pub fn lookup(&self, key: &KeyEvent, fallback_options: u8) -> Option<&str> {
        if let Some(action) = self.find(key) {
            return Some(action);
        }
        // Ctrl / Alt 按下 ⇒ 不做任何兜底
        if key.ctrl() || key.alt() {
            return None;
        }
        if !key.shift() {
            return None;
        }
        if fallback_options & fallback::SHIFT_AS_CONTROL != 0 {
            let as_control = KeyEvent::new(
                key.keycode,
                (key.modifier & !K_SHIFT_MASK) | K_CONTROL_MASK,
            );
            if let Some(action) = self.find(&as_control) {
                return Some(action);
            }
        }
        if fallback_options & fallback::IGNORE_SHIFT != 0 {
            let no_shift = KeyEvent::new(key.keycode, key.modifier & !K_SHIFT_MASK);
            if let Some(action) = self.find(&no_shift) {
                return Some(action);
            }
        }
        None
    }

    /// 不带兜底的精确查找（AsciiComposer 就是这么用的）。
    pub fn get(&self, key: &KeyEvent) -> Option<&str> {
        self.find(key)
    }

    /// C++ `KeyBindingProcessor::LoadConfig(config, key)`：读 `<path>/bindings`。
    ///
    /// 形式是 `{ "<键表示>": "<动作名>" }`，键表示走 [`KeyEvent::parse`]
    /// （`"Return"` / `"Control+BackSpace"` / `"space"` 都能认）。解析不了的键记日志跳过。
    pub fn load_config(&mut self, config: &Value, path: &str) {
        let config_path = format!("/{}/bindings", path.trim_matches('/'));
        let Some(table) = config.pointer(&config_path).and_then(|v| v.as_object()) else {
            return;
        };
        for (repr, action) in table {
            let Some(action) = action.as_str() else {
                continue;
            };
            match KeyEvent::parse(repr) {
                Some(key) => self.bind(key, action),
                None => {
                    eprintln!("[key_binding] invalid binding: '{repr}' at '{config_path}'");
                }
            }
        }
    }

    /// 绑定条数（测试用）。
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::fallback::*;
    use super::*;
    use crate::engine::key_event::{
        K_ALT_MASK, K_CONTROL_MASK, K_RELEASE_MASK, K_SHIFT_MASK, XK_LEFT, XK_RETURN,
    };

    fn default_map() -> KeyBindingMap {
        let mut m = KeyBindingMap::new();
        m.bind(KeyEvent::new(XK_RETURN, 0), "commit_raw_input");
        m.bind(KeyEvent::new(XK_RETURN, K_CONTROL_MASK), "commit_script_text");
        m
    }

    #[test]
    fn lookup_is_an_exact_modifier_match() {
        let m = default_map();
        assert_eq!(m.lookup(&KeyEvent::new(XK_RETURN, 0), ALL), Some("commit_raw_input"));
        // Release 位也参与比较（C++ 精确匹配），因此 Release+Return 不命中 Return 绑定
        assert_eq!(m.lookup(&KeyEvent::new(XK_RETURN, K_RELEASE_MASK), ALL), None);
        // 兜底也救不了—— Ctrl/Alt 之外还要求完全相等
        assert_eq!(
            m.lookup(&KeyEvent::new(XK_RETURN, K_RELEASE_MASK), IGNORE_SHIFT),
            None
        );
    }

    #[test]
    fn shift_falls_back_to_control_and_to_none() {
        let mut m = KeyBindingMap::new();
        m.bind(KeyEvent::new(XK_LEFT, K_CONTROL_MASK), "left_by_syllable");
        let shift_left = KeyEvent::new(XK_LEFT, K_SHIFT_MASK);
        // C++ `ShiftAsControl` 先命中
        assert_eq!(m.lookup(&shift_left, ALL), Some("left_by_syllable"));
        // 关掉 ShiftAsControl、只留 IgnoreShift 时命中不了（表里没有无修饰的 Left）
        assert_eq!(m.lookup(&shift_left, IGNORE_SHIFT), None);
        let mut m2 = KeyBindingMap::new();
        m2.bind(KeyEvent::new(XK_LEFT, 0), "rewind");
        assert_eq!(m2.lookup(&shift_left, IGNORE_SHIFT), Some("rewind"));
        assert_eq!(m2.lookup(&shift_left, NONE), None);
    }

    #[test]
    fn ctrl_alt_never_fall_back() {
        let m = default_map();
        assert_eq!(m.lookup(&KeyEvent::new(XK_RETURN, K_CONTROL_MASK), ALL), Some("commit_script_text"));
        let alt_plus_i = KeyEvent::new(b'i' as u32, K_ALT_MASK | K_SHIFT_MASK);
        assert_eq!(m.lookup(&alt_plus_i, ALL), None);
    }

    #[test]
    fn later_binding_overrides_earlier() {
        let mut m = default_map();
        m.bind(KeyEvent::new(XK_RETURN, 0), "commit_script_text");
        assert_eq!(m.get(&KeyEvent::new(XK_RETURN, 0)), Some("commit_script_text"));
    }

    #[test]
    fn load_config_reads_repr_map() {
        let config: Value =
            serde_json::from_str(r#"{"editor":{"bindings":{"Return":"commit_raw_input","Control+a":"home"}}}"#)
                .expect("json");
        let mut m = KeyBindingMap::new();
        m.load_config(&config, "editor");
        assert_eq!(m.len(), 2);
        assert_eq!(m.get(&KeyEvent::new(XK_RETURN, 0)), Some("commit_raw_input"));
        assert_eq!(
            m.get(&KeyEvent::new(b'a' as u32, K_CONTROL_MASK)),
            Some("home")
        );
        // 未知的键表示被跳过
        let bad: Value =
            serde_json::from_str(r#"{"editor":{"bindings":{"NoSuchKey":"noop"}}}"#).expect("json");
        let mut m2 = KeyBindingMap::new();
        m2.load_config(&bad, "editor");
        assert!(m2.is_empty());
    }
}
