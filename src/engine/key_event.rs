//! `src/rime/key_event.{h,cc}`：按键事件（keycode + modifier）。
//!
//! # 键码是 X11 keysym，不是 Windows 虚拟键
//!
//! 宿主（`Tip/src/keyboard.rs` 的 `translate()`）把 Windows 虚拟键**转成 X11 keysym**
//! 再交给引擎 —— 特殊键走 `special_key()` 映射（`0x08 → 0xff08`），可打印键走
//! `ToUnicodeEx()` 取真正的字符码。这与 C++ librime 是同一套约定，所以：
//!
//! * 退格 = `0xff08`、回车 = `0xff0d`、方向键 = `0xff51..0xff54`；
//! * ASCII 区（字母、数字、空格 `0x20`）两者**同值**，所以 `'n'` 就是 `0x6e`。
//!
//! 早先这里用的是 Windows 虚拟键（`0x08` / `0x0d`），与宿主对不上 ——
//! 退格/回车/方向键在真实宿主里永远不会命中。
//!
//! # 修饰键位必须与 C++ 一致
//!
//! `RimeModifier`（`key_table.h:12-46`）的位位置由 C++ 规定，宿主按这套位传 mask。
//! 早先本文件的 `K_SUPER_MASK` 等常量位位置是错的（Super 应在 bit 26，不是 9），
//! 已按 C++ 逐位改正。

use crate::engine::key_names::KEY_NAME_TABLE;

// ----------------------------------------------------------------- 修饰键位

/// `kShiftMask`。
pub const K_SHIFT_MASK: u32 = 1 << 0;
/// `kLockMask`（CapsLock）。
pub const K_LOCK_MASK: u32 = 1 << 1;
/// `kControlMask`。
pub const K_CONTROL_MASK: u32 = 1 << 2;
/// `kMod1Mask` —— Windows 上就是 Alt。
pub const K_MOD1_MASK: u32 = 1 << 3;
/// `kMod2Mask`。
pub const K_MOD2_MASK: u32 = 1 << 4;
/// `kMod3Mask`。
pub const K_MOD3_MASK: u32 = 1 << 5;
/// `kMod4Mask`。
pub const K_MOD4_MASK: u32 = 1 << 6;
/// `kMod5Mask`。
pub const K_MOD5_MASK: u32 = 1 << 7;
/// `kButton1Mask`。
pub const K_BUTTON1_MASK: u32 = 1 << 8;
/// `kButton2Mask`。
pub const K_BUTTON2_MASK: u32 = 1 << 9;
/// `kButton3Mask`。
pub const K_BUTTON3_MASK: u32 = 1 << 10;
/// `kButton4Mask`。
pub const K_BUTTON4_MASK: u32 = 1 << 11;
/// `kButton5Mask`。
pub const K_BUTTON5_MASK: u32 = 1 << 12;
/// `kHandledMask`（ibus 约定）。
pub const K_HANDLED_MASK: u32 = 1 << 24;
/// `kForwardMask`。
pub const K_FORWARD_MASK: u32 = 1 << 25;
/// `kIgnoredMask` == `kForwardMask`。
pub const K_IGNORED_MASK: u32 = K_FORWARD_MASK;
/// `kSuperMask`（Win / Super 键）。
pub const K_SUPER_MASK: u32 = 1 << 26;
/// `kHyperMask`。
pub const K_HYPER_MASK: u32 = 1 << 27;
/// `kMetaMask`。
pub const K_META_MASK: u32 = 1 << 28;
/// `kReleaseMask`（按键抬起）。
pub const K_RELEASE_MASK: u32 = 1 << 30;
/// `kModifierMask` —— C++ 里 `repr()` 只看这些位。
pub const K_MODIFIER_MASK: u32 = 0x5f001fff;

/// Alt 的别名（librime 里 Alt = `kMod1Mask`）。
pub const K_ALT_MASK: u32 = K_MOD1_MASK;

/// 修饰键位 → 名字（顺序即位序，与 C++ `modifier_name[]` 一致；C++ 里 13..25 位没有名字）。
///
/// C++ 的 `RimeGetModifierName()` 返回**最低置位**的名字；`repr()` 的循环把每一位
/// 单独取出来，所以这里按位序排好即可。
const MODIFIER_NAMES: &[(u32, &str)] = &[
    (K_SHIFT_MASK, "Shift"),
    (K_LOCK_MASK, "Lock"),
    (K_CONTROL_MASK, "Control"),
    (K_MOD1_MASK, "Alt"),
    (K_MOD2_MASK, "Mod2"),
    (K_MOD3_MASK, "Mod3"),
    (K_MOD4_MASK, "Mod4"),
    (K_MOD5_MASK, "Mod5"),
    (K_BUTTON1_MASK, "Button1"),
    (K_BUTTON2_MASK, "Button2"),
    (K_BUTTON3_MASK, "Button3"),
    (K_BUTTON4_MASK, "Button4"),
    (K_BUTTON5_MASK, "Button5"),
    (K_SUPER_MASK, "Super"),
    (K_HYPER_MASK, "Hyper"),
    (K_META_MASK, "Meta"),
    (K_RELEASE_MASK, "Release"),
];

// ----------------------------------------------------------------- 键码（X11 keysym）

/// `XK_BackSpace`。
pub const XK_BACKSPACE: u32 = 0xff08;
/// `XK_Tab`。
pub const XK_TAB: u32 = 0xff09;
/// `XK_Return`（小键盘回车是 `0xff8d`）。
pub const XK_RETURN: u32 = 0xff0d;
/// `XK_KP_Enter`。
pub const XK_KP_ENTER: u32 = 0xff8d;
/// `XK_Escape`。
pub const XK_ESCAPE: u32 = 0xff1b;
/// `XK_space`。
pub const XK_SPACE: u32 = 0x0020;
/// `XK_Prior`（PageUp）。
pub const XK_PRIOR: u32 = 0xff55;
/// `XK_Next`（PageDown）。
pub const XK_NEXT: u32 = 0xff56;
/// `XK_End`。
pub const XK_END: u32 = 0xff57;
/// `XK_Home`。
pub const XK_HOME: u32 = 0xff50;
/// `XK_Left`。
pub const XK_LEFT: u32 = 0xff51;
/// `XK_Up`。
pub const XK_UP: u32 = 0xff52;
/// `XK_Right`。
pub const XK_RIGHT: u32 = 0xff53;
/// `XK_Down`。
pub const XK_DOWN: u32 = 0xff54;
/// `XK_Insert`。
pub const XK_INSERT: u32 = 0xff63;
/// `XK_Delete`。
pub const XK_DELETE: u32 = 0xffff;
/// `XK_0`。
pub const XK_0: u32 = 0x0030;
/// `XK_1`。
pub const XK_1: u32 = 0x0031;
/// `XK_2`。
pub const XK_2: u32 = 0x0032;
/// `XK_3`。
pub const XK_3: u32 = 0x0033;
/// `XK_4`。
pub const XK_4: u32 = 0x0034;
/// `XK_5`。
pub const XK_5: u32 = 0x0035;
/// `XK_6`。
pub const XK_6: u32 = 0x0036;
/// `XK_7`。
pub const XK_7: u32 = 0x0037;
/// `XK_8`。
pub const XK_8: u32 = 0x0038;
/// `XK_9`。
pub const XK_9: u32 = 0x0039;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct KeyEvent {
    pub keycode: u32,
    pub modifier: u32,
}

impl KeyEvent {
    pub fn new(keycode: u32, modifier: u32) -> Self {
        KeyEvent { keycode, modifier }
    }

    pub fn shift(&self) -> bool {
        self.modifier & K_SHIFT_MASK != 0
    }
    pub fn ctrl(&self) -> bool {
        self.modifier & K_CONTROL_MASK != 0
    }
    pub fn alt(&self) -> bool {
        self.modifier & K_MOD1_MASK != 0
    }
    pub fn release(&self) -> bool {
        self.modifier & K_RELEASE_MASK != 0
    }

    /// 是否是一个「可打印字符」按键（无 ctrl/alt/super 修饰）。
    ///
    /// 宿主对可打印键送的是**解码后的字符码**（`'n'` = 0x6e），所以这里直接看码位。
    /// 特殊键是 X11 keysym（≥ 0xff00），落在 ASCII 之外，自然不会被当成可打印字符。
    pub fn printable_char(&self) -> Option<char> {
        if self.ctrl() || self.alt() || (self.modifier & K_SUPER_MASK != 0) {
            return None;
        }
        let c = self.keycode;
        if (0x20..=0x7e).contains(&c) {
            Some(c as u8 as char)
        } else {
            None
        }
    }

    /// 键名（X11 keysym → 名字），取自与 C++ `keys_by_name[]` 同源的那张表。
    pub fn key_name(&self) -> Option<&'static str> {
        key_name_of(self.keycode)
    }

    /// 形如 `"Control+Alt+z"` 的文字表示（对照 C++ `KeyEvent::repr()`）。
    ///
    /// 修饰键按**位序**输出（Shift → Lock → Control → Alt → … → Release），
    /// 键名查不到时退回十六进制：`0x…`（4 位 / 6 位）或 `(unknown)`。
    pub fn repr(&self) -> String {
        let mut s = String::new();
        for (bit, name) in MODIFIER_NAMES {
            if self.modifier & bit != 0 {
                s.push_str(name);
                s.push('+');
            }
        }
        match self.key_name() {
            Some(name) => {
                s.push_str(name);
            }
            None => {
                s.push_str(&hex_keycode(self.keycode));
            }
        }
        s
    }

    /// 解析 `"Shift+Control+Release+A"` / `"Alt+F4"` / `"plus"` / `"+"` 这类表示。
    ///
    /// 语义与 C++ `KeyEvent::Parse()` 一致：
    /// * 长度为 1 → 直接把该字符当键码；
    /// * 否则按 `+` 切分，**除最后一段外都必须是修饰键名**（认不出就失败），
    ///   最后一段是键名（查不到也失败）。
    /// C++ `KeySequence::Parse`：一串按键，逐字符读；`{...}` 里的是多字符键名
    /// （如 `{Control+space}`）。遇到不认识的键整串作废。
    pub fn parse_sequence(repr: &str) -> Vec<KeyEvent> {
        let mut out = Vec::new();
        let bytes = repr.as_bytes();
        let mut i = 0usize;
        while i < repr.len() {
            let (token, next) = if bytes[i] == b'{' && i + 1 < repr.len() {
                match repr[i + 1..].find('}') {
                    Some(j) => (&repr[i + 1..i + 1 + j], i + 1 + j + 1),
                    None => return Vec::new(),
                }
            } else {
                // 多字节字符整体作为一个键（保留 u8 遍历，避免 panic）
                let ch_len = repr[i..].chars().next().map_or(1, |c| c.len_utf8());
                (&repr[i..i + ch_len], i + ch_len)
            };
            match KeyEvent::parse(token) {
                Some(k) => out.push(k),
                None => return Vec::new(),
            }
            i = next;
        }
        out
    }

    pub fn parse(repr: &str) -> Option<KeyEvent> {
        if repr.is_empty() {
            return None;
        }
        if repr.len() == 1 {
            return Some(KeyEvent::new(repr.as_bytes()[0] as u32, 0));
        }
        let mut modifier = 0u32;
        let mut start = 0usize;
        while let Some(pos) = repr[start..].find('+') {
            let found = start + pos;
            let token = &repr[start..found];
            modifier |= modifier_by_name(token)?;
            start = found + 1;
        }
        let keycode = keycode_by_name(&repr[start..])?;
        Some(KeyEvent::new(keycode, modifier))
    }
}

/// 键码 → 名字（C++ `RimeGetKeyName()`：取**第一个**匹配项）。
pub fn key_name_of(keycode: u32) -> Option<&'static str> {
    KEY_NAME_TABLE
        .iter()
        .find(|(code, _)| *code == keycode)
        .map(|(_, name)| *name)
}

/// 名字 → 键码（C++ `RimeGetKeycodeByName()`：取**第一个**匹配项，找不到返回"空符号"）。
pub fn keycode_by_name(name: &str) -> Option<u32> {
    KEY_NAME_TABLE
        .iter()
        .find(|(_, n)| *n == name)
        .map(|(code, _)| *code)
}

/// 修饰键名 → 位（C++ `RimeGetModifierByName()`）。
pub fn modifier_by_name(name: &str) -> Option<u32> {
    MODIFIER_NAMES
        .iter()
        .find(|(_, n)| *n == name)
        .map(|(bit, _)| *bit)
}

/// 查不到名字时的十六进制表示（C++ `KeyEvent::repr()` 的尾巴）。
fn hex_keycode(keycode: u32) -> String {
    if keycode <= 0xffff {
        format!("0x{keycode:04x}")
    } else if keycode <= 0xffffff {
        format!("0x{keycode:06x}")
    } else {
        "(unknown)".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_ascii_letters() {
        let k = KeyEvent::new(b'a' as u32, 0);
        assert_eq!(k.printable_char(), Some('a'));
        let k = KeyEvent::new(b'1' as u32, 0);
        assert_eq!(k.printable_char(), Some('1'));
        let k = KeyEvent::new(b'a' as u32, K_CONTROL_MASK);
        assert_eq!(k.printable_char(), None);
    }

    #[test]
    fn repr_roundtrip() {
        let k = KeyEvent::new(b'z' as u32, K_CONTROL_MASK | K_ALT_MASK);
        assert_eq!(k.repr(), "Control+Alt+z");
        assert_eq!(KeyEvent::parse(&k.repr()), Some(k));
        assert_eq!(KeyEvent::parse("space"), Some(KeyEvent::new(XK_SPACE, 0)));
        assert_eq!(KeyEvent::parse("Return"), Some(KeyEvent::new(XK_RETURN, 0)));
        assert_eq!(
            KeyEvent::parse("Control+a"),
            Some(KeyEvent::new(b'a' as u32, K_CONTROL_MASK))
        );
    }

    /// C++ `key_event_test.cc` 的原样期望值。
    #[test]
    fn matches_cpp_key_event_test() {
        // KeyName
        assert_eq!(KeyEvent::new(0x2c, 0).repr(), "comma");
        assert_eq!(KeyEvent::new(0x2e, 0).repr(), "period");
        // HexKeyName
        assert_eq!(KeyEvent::new(0xfffe, 0).repr(), "0xfffe");
        assert_eq!(KeyEvent::new(0xfffffe, 0).repr(), "0xfffffe");
        // BadKeyName
        assert_eq!(KeyEvent::new(0x1000000, 0).repr(), "(unknown)");
        // ModifiedKeyEventRepresentation
        assert_eq!(KeyEvent::new(0x61, K_CONTROL_MASK).repr(), "Control+a");
        assert_eq!(
            KeyEvent::new(0x3c, K_SHIFT_MASK | K_RELEASE_MASK).repr(),
            "Shift+Release+less"
        );
        // ParseKeyEventRepresentation
        let ke = KeyEvent::parse("Shift+Control+Release+A").expect("应能解析");
        assert_eq!(ke.keycode, 0x41);
        assert!(ke.shift());
        assert!(!ke.alt());
        assert!(ke.ctrl());
        assert!(ke.release());
        // ConstructKeyEventFromRepresentation
        let ke = KeyEvent::parse("Alt+F4").expect("应能解析");
        assert_eq!(ke.keycode, 0xffc1);
        assert!(ke.alt());
        assert!(!ke.release());
        // Equality：符号键的两种写法要等价
        assert_eq!(
            KeyEvent::parse("+").map(|k| k.keycode),
            KeyEvent::parse("plus").map(|k| k.keycode)
        );
    }

    /// 宿主送的是 X11 keysym，不是 Windows 虚拟键。
    #[test]
    fn keycodes_follow_x11_keysym_convention() {
        assert_eq!(XK_BACKSPACE, 0xff08);
        assert_eq!(XK_RETURN, 0xff0d);
        assert_eq!(XK_ESCAPE, 0xff1b);
        assert_eq!(XK_SPACE, 0x20);
        // 特殊键不是可打印字符
        assert_eq!(KeyEvent::new(XK_BACKSPACE, 0).printable_char(), None);
        assert_eq!(KeyEvent::new(XK_RETURN, 0).printable_char(), None);
        // 数字与 ASCII 同值
        assert_eq!(KeyEvent::new(XK_1, 0).printable_char(), Some('1'));
    }

    #[test]
    fn modifier_bit_positions_match_cpp() {
        // C++ RimeModifier（key_table.h:12-46）
        assert_eq!(K_SHIFT_MASK, 1 << 0);
        assert_eq!(K_CONTROL_MASK, 1 << 2);
        assert_eq!(K_MOD1_MASK, 1 << 3);
        assert_eq!(K_HANDLED_MASK, 1 << 24);
        assert_eq!(K_IGNORED_MASK, 1 << 25);
        assert_eq!(K_SUPER_MASK, 1 << 26);
        assert_eq!(K_HYPER_MASK, 1 << 27);
        assert_eq!(K_META_MASK, 1 << 28);
        assert_eq!(K_RELEASE_MASK, 1 << 30);
    }
}
