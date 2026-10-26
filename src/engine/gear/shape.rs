//! `gear/shape.cc`（77 行）移植：全角/半角形状转换。
//!
//! 两个组件共用一份转换逻辑：
//!
//! | 组件 | 类别 | 干什么 |
//! |---|---|---|
//! | `ShapeFormatter` | formatter | 把一段 ASCII 文本按 `full_shape` 转成全角 |
//! | `ShapeProcessor` | processor | 吃下 ASCII 键，转全角后**直接上屏** |
//!
//! `ShapeFormatter` 在本仓里没有独立的 formatter 装配点，它有两个使用者：
//!
//! * `ShapeProcessor`（本文件）；
//! * `PunctTranslator` —— `punctuator` 的 `punct_number` 分支要把 `3.14` 里的 `.`
//!   按 `full_shape` 变成 `．`（`punctuator.cc:333` 的 `formatter_.Format(&punct)`）。
//!
//! 所以它是一个**普通函数 + 零尺寸类型**，不挂 formatter 列表。

use crate::engine::gear::processor::{ProcessResult, Processor};
use crate::engine::key_event::{KeyEvent, K_SUPER_MASK};
use crate::engine::Engine;

/// C++ `ShapeFormatter`。
pub struct ShapeFormatter;

impl ShapeFormatter {
    /// `ShapeFormatter::Format`：把 `text` 就地转成全角。
    ///
    /// 三条与 C++ 逐字对应的细节：
    ///
    /// 1. `full_shape` 关着 ⇒ 原样返回（**不是**转半角，是整段不动）；
    /// 2. 整段都不在 `[0x20, 0x7e]` ⇒ 原样返回（已经是全角/汉字/emoji 的不动）；
    /// 3. 逐**字节**处理：空格 → U+3000，`0x21..=0x7e` → `U+FF00 + (ch - 0x20)`，
    ///    其余原样。公式就是 C++ 那三行拼 UTF-8 位移算出来的同一个结果
    ///    （`,` = 0x2C → U+FF0C 「，」）。
    pub fn format(text: &mut String, full_shape: bool) {
        if !full_shape {
            return;
        }
        let bytes = text.as_bytes();
        if bytes.iter().all(|&b| !(0x20..=0x7e).contains(&b)) {
            return;
        }
        // 按**字节**重拼：非 ASCII 字节原样拷回去，所以必须走字节缓冲，
        // 不能 `out.push(byte as char)` —— 那会把 UTF-8 多字节序列拆坏。
        let mut out: Vec<u8> = Vec::with_capacity(bytes.len() * 3);
        for &b in bytes {
            match b {
                0x20 => out.extend_from_slice("\u{3000}".as_bytes()),
                0x21..=0x7e => {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(from_ascii_full_width(b).encode_utf8(&mut buf).as_bytes());
                }
                _ => out.push(b),
            }
        }
        // 输入本来就是合法 UTF-8，只在中文段原样拷回去，所以必然成功
        *text = String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    }

    /// 便捷包装：返回新串。
    pub fn formatted(text: &str, full_shape: bool) -> String {
        let mut s = text.to_string();
        Self::format(&mut s, full_shape);
        s
    }
}

/// 半角 ASCII（`0x21..=0x7e`）→ 对应的全角码位（`U+FF01..=U+FF5E`）。
fn from_ascii_full_width(b: u8) -> char {
    char::from_u32(0xff00 + (b as u32 - 0x20)).unwrap_or(b as char)
}

/// C++ `ShapeProcessor`：全角模式下把 ASCII 键转全角直接上屏。
///
/// 与 `Punctuator` 的分工：`punctuator` 只管**方案里配了映射**的标点键；
/// `shape_processor` 是"不查表、一律变全角"的兜底，出厂方案里通常排在
/// `punctuator` 之后。
pub struct ShapeProcessor;

impl Processor for ShapeProcessor {
    fn process_key_event(&mut self, engine: &mut Engine, key: &KeyEvent) -> ProcessResult {
        if !engine.context.get_option("full_shape") {
            return ProcessResult::Noop;
        }
        if key.ctrl()
            || key.alt()
            || key.modifier & K_SUPER_MASK != 0
            || key.release()
        {
            return ProcessResult::Noop;
        }
        let ch = key.keycode;
        if !(0x20..0x7f).contains(&ch) {
            return ProcessResult::Noop;
        }
        let wide = ShapeFormatter::formatted(&(ch as u8 as char).to_string(), true);
        // C++ `engine_->sink()(wide)`：裸输出，**不进**提交历史
        // （对应 `Context::commit_direct`，不是 `commit_raw_text`）。
        engine.context.commit_direct(wide);
        ProcessResult::Accepted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::key_event::{K_ALT_MASK, K_CONTROL_MASK};

    #[test]
    fn half_shape_is_a_noop() {
        let mut s = "abc,.".to_string();
        ShapeFormatter::format(&mut s, false);
        assert_eq!(s, "abc,.", "full_shape 关着时整段不动");
    }

    #[test]
    fn ascii_becomes_full_width() {
        assert_eq!(ShapeFormatter::formatted(",.", true), "，．");
        assert_eq!(ShapeFormatter::formatted("abc", true), "ａｂｃ");
        assert_eq!(ShapeFormatter::formatted("a b", true), "ａ　ｂ", "空格 → U+3000");
        assert_eq!(ShapeFormatter::formatted("!", true), "！");
        assert_eq!(ShapeFormatter::formatted("~", true), "～");
    }

    #[test]
    fn already_wide_text_is_untouched() {
        // 整段没有 ASCII ⇒ 原样返回（C++ 的 `all_of(ch < 0x20 || ch > 0x7e)`）
        assert_eq!(ShapeFormatter::formatted("你好，", true), "你好，");
        assert_eq!(ShapeFormatter::formatted("ｈｅｌｌｏ", true), "ｈｅｌｌｏ");
        // 混排时只有 ASCII 部分被转（逐字节扫，不看整体）
        assert_eq!(ShapeFormatter::formatted("3。14", true), "３。１４");
    }

    #[test]
    fn mixed_text_converts_only_ascii() {
        assert_eq!(ShapeFormatter::formatted("3.14秒", true), "３．１４秒");
    }

    #[test]
    fn processor_requires_full_shape_option() {
        let mut engine = Engine::empty_for_test();
        let mut p = ShapeProcessor;
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(b',' as u32, 0)),
            ProcessResult::Noop,
            "full_shape 关着时不动"
        );
        assert_eq!(engine.context.commit_text, None);

        engine.set_option("full_shape", true);
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(b',' as u32, 0)),
            ProcessResult::Accepted
        );
        assert_eq!(engine.context.commit_text, Some("，".to_string()));
    }

    #[test]
    fn processor_ignores_modified_and_non_printable_keys() {
        let mut engine = Engine::empty_for_test();
        engine.set_option("full_shape", true);
        let mut p = ShapeProcessor;
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(b'a' as u32, K_CONTROL_MASK)),
            ProcessResult::Noop
        );
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(b'a' as u32, K_ALT_MASK)),
            ProcessResult::Noop
        );
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(0xff08, 0)),
            ProcessResult::Noop,
            "退格是 keysym 0xff08，不在可打印区"
        );
    }
}
