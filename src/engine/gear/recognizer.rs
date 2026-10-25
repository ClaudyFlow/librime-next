//! `gear/recognizer.cc`（processor 侧）+ `gear/matcher.cc`（segmentor 侧）共用的**模式表**。
//!
//! 两个组件读的是同一个配置节点（`<name_space>/patterns`），语义也一样：
//! 一条 `tag: regex`，匹配从"已确认位置"到串尾的那段活动输入。

use std::collections::BTreeMap;

use fancy_regex::Regex;

use crate::engine::segmentation::Segmentation;

/// `RecognizerMatch`：一段被识别出来的输入（`tag` + 区间）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RecognizerMatch {
    pub tag: String,
    pub start: usize,
    pub end: usize,
}

impl RecognizerMatch {
    pub fn new(tag: &str, start: usize, end: usize) -> Self {
        RecognizerMatch {
            tag: tag.to_string(),
            start,
            end,
        }
    }

    /// C++ `found()`。
    pub fn found(&self) -> bool {
        self.start < self.end
    }
}

/// `RecognizerPatterns`：tag → 正则。
///
/// C++ 的容器是 `std::map<string, boost::regex>` —— **按 tag 字典序遍历**，
/// 顺序会影响 `GetMatch` 谁先命中，所以用 `BTreeMap` 而不是保持插入序的容器
/// （配置文件本身保序，直接照抄插入序会和 C++ 结果不一致）。
pub struct RecognizerPatterns {
    patterns: BTreeMap<String, Regex>,
}

impl RecognizerPatterns {
    pub fn new() -> Self {
        RecognizerPatterns {
            patterns: BTreeMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// `LoadConfig`：读 `<name_space>/patterns` 的每个 `tag: expression`。
    ///
    /// 表达式编译失败的处理与 C++ 一致：**记日志并跳过该项**，不整体失败。
    pub fn load_config(&mut self, config: &serde_json::Value, name_space: &str) {
        let path = format!("/{}/patterns", name_space.replace('.', "/"));
        let Some(map) = config.pointer(&path).and_then(|v| v.as_object()) else {
            return;
        };
        for (tag, value) in map {
            let Some(expr) = value.as_str() else { continue };
            match Regex::new(expr) {
                Ok(re) => {
                    self.patterns.insert(tag.clone(), re);
                }
                Err(err) => eprintln!("[recognizer] error parsing pattern /{expr}/: {err}"),
            }
        }
    }

    /// `GetMatch`：在"已确认位置之后的活动输入"上做匹配（逐字照抄 C++）。
    ///
    /// 三步过滤，一步都不能少：
    /// 1. 匹配到的**终点必须落在输入串末端**（`end != input.len()` 直接跳过）；
    /// 2. 起点等于当前轮起点 → 命中；
    /// 3. 否则逐个比对已有段的起点（段按起点升序，越界就 break）。
    pub fn get_match(&self, input: &str, segmentation: &Segmentation) -> RecognizerMatch {
        let j = segmentation.current_end();
        let k = segmentation.confirmed_position();
        let active_input = input.get(k..).unwrap_or("");
        for (tag, re) in &self.patterns {
            let Ok(Some(m)) = re.find(active_input) else { continue };
            let start = k + m.start();
            let end = start + (m.end() - m.start());
            if end != input.len() {
                continue;
            }
            if start == j {
                return RecognizerMatch::new(tag, start, end);
            }
            for seg in &segmentation.segments {
                if start < seg.start {
                    break;
                }
                if start == seg.start {
                    return RecognizerMatch::new(tag, start, end);
                }
            }
        }
        RecognizerMatch::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::segmentation::{Segment, Segmentation};

    fn patterns_of(pairs: &[(&str, &str)]) -> RecognizerPatterns {
        let mut p = RecognizerPatterns::new();
        for (tag, expr) in pairs {
            p.patterns
                .insert(tag.to_string(), Regex::new(expr).unwrap());
        }
        p
    }

    fn segs_of(items: &[(usize, usize)]) -> Segmentation {
        Segmentation {
            input: "abc@example.com".to_string(),
            segments: items.iter().map(|(a, b)| Segment::new(*a, *b)).collect(),
        }
    }

    #[test]
    fn iterates_tags_in_lexicographic_order() {
        // C++ 用 std::map —— 插入顺序无关，取的是字典序。
        // 两条都对同一段命中，取字典序在前的那个。
        let p = patterns_of(&[("zzz", "com$"), ("aaa", "com$")]);
        let m = p.get_match("abc@example.com", &segs_of(&[(0, 3), (3, 12), (12, 15)]));
        assert_eq!(m.tag, "aaa");
    }

    #[test]
    fn match_must_end_at_input_tail() {
        // `example` 也能匹配到，但终点不在串尾 → 不算命中
        let p = patterns_of(&[("middle", "example")]);
        assert!(!p.get_match("abc@example.com", &segs_of(&[])).found());
    }

    #[test]
    fn email_pattern_matches_active_input() {
        let p = patterns_of(&[("email", "^[a-z]+@[a-z.]+$")]);
        let m = p.get_match("abc@example.com", &segs_of(&[]));
        assert!(m.found());
        assert_eq!(m.tag, "email");
        assert_eq!((m.start, m.end), (0, 15));
    }

    #[test]
    fn start_must_equal_current_or_a_segment_start() {
        let p = patterns_of(&[("tail", "com$")]);
        // 当前轮起点在 0，而匹配起点在 12，且没有任何段的起点是 12 → 不命中
        assert!(!p.get_match("abc@example.com", &segs_of(&[(0, 3)])).found());
        // 加一个起点正好是 12 的段 → 命中
        let m = p.get_match("abc@example.com", &segs_of(&[(0, 3), (12, 15)]));
        assert!(m.found());
        assert_eq!((m.start, m.end), (12, 15));
    }
}

// ------------------------------------------------------------ processor 侧

/// `gear/recognizer.cc` 的 processor：`Recognizer`。
///
/// 作用只有一个：**放行**。拼写的字符集（`speller/alphabet`）通常只含字母，
/// 但邮箱、网址里 `@`、`.`、`/` 这些键会被 speller 挡掉。Recognizer 在
/// "当前输入 + 这个键" 上试一遍模式表，命中就先记账（push_input）并返回 `Accepted`，
/// 于是后面的 speller 看到的已经是合法形态。真正的打标签由 [`crate::engine::gear::matcher::Matcher`] 完成。
pub struct Recognizer {
    patterns: RecognizerPatterns,
    use_space: bool,
}

impl Recognizer {
    pub fn new(config: &serde_json::Value, name_space: &str) -> Self {
        // C++：命名空间是 "processor" 时按 "recognizer" 读
        let ns = if name_space == "processor" {
            "recognizer"
        } else {
            name_space
        };
        let mut patterns = RecognizerPatterns::new();
        patterns.load_config(config, ns);
        let use_space = config
            .pointer(&format!("/{ns}/use_space"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        Recognizer { patterns, use_space }
    }
}

impl super::processor::Processor for Recognizer {
    fn process_key_event(
        &mut self,
        engine: &mut crate::engine::Engine,
        key: &crate::engine::key_event::KeyEvent,
    ) -> super::processor::ProcessResult {
        if self.patterns.is_empty()
            || key.ctrl()
            || key.alt()
            || key.modifier & crate::engine::key_event::K_SUPER_MASK != 0
            || key.release()
        {
            return super::processor::ProcessResult::Noop;
        }
        let ch = key.keycode;
        let is_printable = (0x20..0x80).contains(&ch);
        if (self.use_space && ch == 0x20) || (ch > 0x20 && is_printable) {
            let mut input = engine.context.input.clone();
            input.push(char::from_u32(ch).unwrap_or('\u{fffd}'));
            let match_ = self
                .patterns
                .get_match(&input, &engine.context.composition.segmentation);
            if match_.found() {
                engine.context.push_input_ch(char::from_u32(ch).unwrap_or('\u{fffd}'));
                return super::processor::ProcessResult::Accepted;
            }
        }
        super::processor::ProcessResult::Noop
    }
}

#[cfg(test)]
mod processor_tests {
    use super::*;
    use crate::engine::gear::processor::{ProcessResult, Processor};
    use crate::engine::key_event::KeyEvent;
    use serde_json::json;
    use crate::engine::Engine;

    fn config() -> serde_json::Value {
        json!({
            "recognizer": {
                "patterns": { "email": "^[a-z]+@[a-z]*$" },
                "use_space": true
            }
        })
    }

    #[test]
    fn accepts_key_that_would_complete_a_pattern() {
        let cfg = config();
        let mut r = Recognizer::new(&cfg, "recognizer");
        let mut engine = Engine::empty_for_test();
        engine.context.push_input_str("abc");
        // '@' 不在 speller 的 alphabet 里，但能让 `abc@` 命中 email 模式
        let k = KeyEvent::new(b'@' as u32, 0);
        let outcome = r.process_key_event(&mut engine, &k);
        assert_eq!(outcome, ProcessResult::Accepted);
        assert_eq!(engine.context.input, "abc@");
    }

    #[test]
    fn noop_when_no_match() {
        let cfg = config();
        let mut r = Recognizer::new(&cfg, "recognizer");
        let mut engine = Engine::empty_for_test();
        engine.context.push_input_str("zzz");
        let k = KeyEvent::new(b'!' as u32, 0);
        assert_eq!(r.process_key_event(&mut engine, &k), ProcessResult::Noop);
        assert_eq!(engine.context.input, "zzz");
    }

    #[test]
    fn noop_with_modifier_keys() {
        let cfg = config();
        let mut r = Recognizer::new(&cfg, "recognizer");
        let mut engine = Engine::empty_for_test();
        engine.context.push_input_str("abc");
        let k = KeyEvent::new(b'@' as u32, crate::engine::key_event::K_CONTROL_MASK);
        assert_eq!(r.process_key_event(&mut engine, &k), ProcessResult::Noop);
    }
}
