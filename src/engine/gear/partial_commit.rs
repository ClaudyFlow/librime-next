//! `gear/partial_commit.cc`（librime-evo，amzxyz）移植：**按需提交**。
//!
//! 上游 librime 只能「整段上屏」或「逐音节确认」，evo 补了一条中间路线：
//! 在整句候选上直接**切前几个字**上屏，剩下的输入留在组合里继续选。
//!
//! ```yaml
//! engine:
//!   processors:
//!     - partial_commit        # 放在 selector / express_editor 之前
//!     - selector
//! partial_commit:
//!   control_digits: true      # Ctrl+1…9 提交前 N 个音节（Ctrl+0 = 10）
//!   first_key: ""             # 以词定字：按此键上屏**首字**
//!   last_key: ""              # 以词定字：按此键上屏**尾字**
//! ```
//!
//! # 两条能力
//!
//! | 能力 | 触发 | 效果 |
//! |---|---|---|
//! | 前缀提交 | `Ctrl+数字` | 把候选的前 N 个音节上屏，剩余输入串（去掉一个分隔符）继续组字 |
//! | 以词定字 | `first_key` / `last_key` | 只上屏当前选中候选的**首字**或**尾字**，清空组合 |
//!
//! # 为什么需要 `Candidate::spans`
//!
//! 「前 N 个音节」要翻译成输入串上的切点，而候选文本的字符数并不等于音节数
//! （「西安」是 2 字 2 音节，「中华人民共和国」是 7 字 7 音节，但「西安」也可能
//! 被一条 1 音节的词条产出）。C++ 用 `Phrase::spans()` 的顶点做这件事，
//! Rust 侧对应 [`crate::engine::candidate::Candidate::spans`]。
//!
//! 顶点是**绝对字节偏移**，`stops[0] == segment.start`、`stops[n] == segment.end`，
//! 所以 `stops[count]` 就是「前 count 个音节之后」的切点。没有 spans 的候选
//! （标点、兜底字符）一律放行 —— 那不是能切的东西。

use serde_json::Value;

use crate::engine::candidate::Candidate;
use crate::engine::context::Context;
use crate::engine::gear::processor::{ProcessResult, Processor};
use crate::engine::key_event::KeyEvent;
use crate::engine::runtime::Engine;
use crate::engine::segmentation::Segment;

/// 主键盘数字 0 / 9 的 keysym。
const XK_0: u32 = 0x30;
const XK_9: u32 = 0x39;
/// 小键盘数字 0 / 9 的 keysym。
const XK_KP_0: u32 = 0xffb0;
const XK_KP_9: u32 = 0xffb9;

/// 这些段是「功能性」输入（数字 / Unicode / 计算器 / 时间），
/// 不参与按需提交 —— 在数字串上按 Ctrl+3 应该让计算器去处理，而不是提交三位。
const FUNCTION_TAGS: [&str; 4] = ["number", "unicode", "calculator", "shijian"];

pub struct PartialCommit {
    /// `partial_commit/control_digits`，默认 **false**（与 evo 一致：要方案作者主动开）。
    control_digits: bool,
    /// `partial_commit/first_key`，空串 = 不启用。
    first_key: String,
    /// `partial_commit/last_key`，空串 = 不启用。
    last_key: String,
}

impl PartialCommit {
    pub fn new(config: &Value) -> Self {
        let control_digits = config
            .pointer("/partial_commit/control_digits")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let key_of = |name: &str| {
            config
                .pointer(&format!("/partial_commit/{name}"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        Self {
            control_digits,
            first_key: key_of("first_key"),
            last_key: key_of("last_key"),
        }
    }

    /// `Matches(key_event, configured_key)`（`partial_commit.cc:83-92`）。
    ///
    /// 两种写法都认：单字符 = 直接比 keysym（`f` / `;`），
    /// 其余按 `repr()` 比整串（`Control+BackSpace`）。
    fn matches(key: &KeyEvent, configured: &str) -> bool {
        if configured.is_empty() {
            return false;
        }
        let bytes = configured.as_bytes();
        if bytes.len() == 1 && key.keycode == u32::from(bytes[0]) {
            return true;
        }
        key.repr() == configured
    }

    /// `IsFunctionMode(context)`：末段带功能性 tag 就不接管。
    fn is_function_mode(context: &Context) -> bool {
        Self::active_segment(context)
            .map(|segment| segment.has_any_tag(&FUNCTION_TAGS))
            .unwrap_or(false)
    }

    /// `ControlDigit(key_event, &count)`：Ctrl + 主键盘/小键盘数字。
    ///
    /// `Ctrl+0` 读作 10（C++ 同样的约定，`partial_commit.cc:113-120`）。
    fn control_digit(key: &KeyEvent) -> Option<usize> {
        if !key.ctrl() || key.release() || key.alt() || key.super_key() {
            return None;
        }
        if (XK_0..=XK_9).contains(&key.keycode) {
            return Some(if key.keycode == XK_0 {
                10
            } else {
                (key.keycode - XK_0) as usize
            });
        }
        if (XK_KP_0..=XK_KP_9).contains(&key.keycode) {
            return Some(if key.keycode == XK_KP_0 {
                10
            } else {
                (key.keycode - XK_KP_0) as usize
            });
        }
        None
    }

    /// `ConfirmedPrefix(context)`：**末段之前**各段已经确定下来的文本。
    ///
    /// 有选中候选就取候选文本；没有选中（且不是 `phony` 占位段）就照抄
    /// 该段覆盖的那截输入串 —— 与 C++ 一字不差。
    ///
    /// ⚠️ 与 C++ 的一处**必要差异**：C++ 直接遍历 `composition[..size-1]`，
    /// 而 lrn 的 `Segmentation::forward()` 会补一个 `start == end` 的尾段
    /// （C++ 那边由 `Context::SetInput` 的 `Reset` 收口）。若照抄 C++，这个空段
    /// 会让「活动段」被当成已确认前缀拼进来（实测会提交出 `你hao`）。
    /// 所以这里先剥掉尾部空段，再取"活动段之前的全部"。
    fn confirmed_prefix(context: &Context) -> String {
        let segments = Self::non_empty_segments(context);
        if segments.len() < 2 {
            return String::new();
        }
        let mut out = String::new();
        for segment in &segments[..segments.len() - 1] {
            if let Some(candidate) = segment.get_selected_candidate() {
                out.push_str(&candidate.text);
            } else if !segment.tags.iter().any(|t| t == "phony") {
                let start = segment.start.min(context.input.len());
                let end = segment.end.min(context.input.len());
                if start < end {
                    out.push_str(&context.input[start..end]);
                }
            }
        }
        out
    }

    /// 去掉尾部空段后的段列表；末尾那个就是**当前活动段**。
    fn non_empty_segments(context: &Context) -> &[Segment] {
        let segments = &context.composition.segmentation.segments;
        let mut end = segments.len();
        while end > 0 && segments[end - 1].start >= segments[end - 1].end {
            end -= 1;
        }
        &segments[..end]
    }

    /// 当前活动段（= 最后一个非空段）。
    fn active_segment(context: &Context) -> Option<&Segment> {
        Self::non_empty_segments(context).last()
    }

    /// `GetPhysicalStops(segment, candidate, &stops)`：候选的音节顶点，
    /// 只保留落在**本段范围内**的那些。
    fn physical_stops(segment: &Segment, candidate: &Candidate) -> Vec<usize> {
        candidate
            .spans
            .iter()
            .copied()
            .filter(|vertex| *vertex >= segment.start && *vertex <= segment.end)
            .collect()
    }

    /// `CommitPrefix(context, count)`：提交前 `count` 个音节，其余输入继续组字。
    fn commit_prefix(&self, engine: &mut Engine, count: usize) -> bool {
        if count == 0 {
            return false;
        }
        let Some(segment) = Self::active_segment(&engine.context).cloned() else {
            return false;
        };
        let Some(candidate) = segment.get_selected_candidate() else {
            return false;
        };
        if candidate.text.is_empty() {
            return false;
        }
        let stops = Self::physical_stops(&segment, &candidate);
        if stops.len() < 2 {
            return false;
        }
        // 可选音节数 = 顶点数 − 1；再受候选字符数限制（一个音节一个字是常见情形，
        // 但多字音节 / 单字多音节都要防越界 —— C++ 同样取这两个 min）。
        let available = stops.len() - 1;
        let char_count = candidate.text.chars().count();
        let count = count.min(available).min(char_count);
        if count == 0 {
            return false;
        }
        let cut = stops[count];
        if cut > engine.context.input.len() {
            return false;
        }
        let mut rest = engine.context.input[cut..].to_string();
        // 剩余输入开头的分隔符要去掉：`ni'hao` 提交 `你` 之后应剩 `hao` 而不是 `'hao`
        // （C++ 只剥一个字符，`'` 与空格都算）。
        if rest.starts_with('\'') || rest.starts_with(' ') {
            rest.remove(0);
        }
        let committed = format!(
            "{}{}",
            Self::confirmed_prefix(&engine.context),
            first_characters(&candidate.text, count)
        );
        engine.context.commit_raw_text(committed);
        engine.context.clear();
        if !rest.is_empty() {
            engine.context.set_input(&rest);
        }
        // 输入变了，下一轮必须重新组字（processor 内改 input 不会自动触发 compose）。
        engine.context.needs_recompose = true;
        true
    }

    /// `CommitCharacter(context, first)`：只上屏首字 / 尾字。
    fn commit_character(&self, engine: &mut Engine, first: bool) -> bool {
        if !engine.context.is_composing() || Self::is_function_mode(&engine.context) {
            return false;
        }
        let mut text = engine.context.input.clone();
        if let Some(candidate) = Self::active_segment(&engine.context)
            .and_then(|segment| segment.get_selected_candidate())
        {
            text = candidate.text;
        }
        if text.is_empty() {
            return false;
        }
        let Some(character) = (if first {
            text.chars().next()
        } else {
            text.chars().next_back()
        }) else {
            return false;
        };
        // 只有一个字时，以词定字与直接上屏等价 —— 放行给下面的 editor，
        // 否则用户按 `f` 会"只能出单字"却不知道为什么。
        if character.to_string() == text {
            return false;
        }
        engine.context.commit_raw_text(character.to_string());
        engine.context.clear();
        true
    }
}

/// `FirstCharacters(text, count)`：按**字符**（不是字节）取前 `count` 个。
fn first_characters(text: &str, count: usize) -> String {
    text.chars().take(count).collect()
}

impl Processor for PartialCommit {
    fn process_key_event(&mut self, engine: &mut Engine, key: &KeyEvent) -> ProcessResult {
        if key.release() || key.alt() || key.super_key() {
            return ProcessResult::Noop;
        }
        if !engine.context.is_composing() || Self::is_function_mode(&engine.context) {
            return ProcessResult::Noop;
        }
        if self.control_digits
            && let Some(count) = Self::control_digit(key)
        {
            return if self.commit_prefix(engine, count) {
                ProcessResult::Accepted
            } else {
                ProcessResult::Noop
            };
        }
        // 其余 Ctrl 组合不归这里管（Ctrl+Delete 删词、Ctrl+Space 切方案……）
        if key.ctrl() {
            return ProcessResult::Noop;
        }
        let select_first = Self::matches(key, &self.first_key);
        let select_last = Self::matches(key, &self.last_key);
        if !select_first && !select_last {
            return ProcessResult::Noop;
        }
        if self.commit_character(engine, select_first) {
            ProcessResult::Accepted
        } else {
            ProcessResult::Noop
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::candidate::Candidate;
    use crate::engine::segmentation::{SegStatus, Segmentation};

    #[test]
    fn helper_first_characters_counts_chars_not_bytes() {
        // 「候选0」是 3 个字符 / 7 个字节，取 2 个字符必须是「候选」而不是切碎字节。
        assert_eq!(first_characters("候选0", 2), "候选");
        assert_eq!(first_characters("候选0", 0), "");
        assert_eq!(first_characters("候选0", 99), "候选0");
    }

    #[test]
    fn control_digit_reads_both_keypads() {
        let mut key = KeyEvent::default();
        key.modifier = crate::engine::key_event::K_CONTROL_MASK;
        key.keycode = 0x33; // '3'
        assert_eq!(PartialCommit::control_digit(&key), Some(3));
        key.keycode = XK_KP_0;
        assert_eq!(PartialCommit::control_digit(&key), Some(10));
        key.keycode = XK_0;
        assert_eq!(PartialCommit::control_digit(&key), Some(10));
        // 没有 Ctrl 就不是按需提交
        key.modifier = 0;
        assert_eq!(PartialCommit::control_digit(&key), None);
    }

    #[test]
    fn physical_stops_are_clipped_to_segment() {
        let segment = Segment::new(2, 6);
        let candidate = Candidate::new("script", 2, 6, "你好", 1.0)
            .with_spans(vec![0, 2, 4, 6]);
        // 0 / 2 里只有 2 是段起点，0 在段外 ⇒ 被裁掉
        assert_eq!(
            PartialCommit::physical_stops(&segment, &candidate),
            vec![2, 4, 6]
        );
    }

    #[test]
    fn matches_accepts_single_char_and_repr() {
        let mut key = KeyEvent::default();
        key.keycode = u32::from(b'f');
        assert!(PartialCommit::matches(&key, "f"));
        assert!(!PartialCommit::matches(&key, "g"));
        assert!(!PartialCommit::matches(&key, ""));
        key.modifier = crate::engine::key_event::K_CONTROL_MASK;
        key.keycode = 0xff08;
        assert!(PartialCommit::matches(&key, &key.repr()));
    }

    #[test]
    fn config_defaults_are_off() {
        let config: Value = serde_json::from_str("{}").unwrap();
        let processor = PartialCommit::new(&config);
        assert!(!processor.control_digits, "control_digits 默认关（与 evo 一致）");
        assert!(processor.first_key.is_empty());
        assert!(processor.last_key.is_empty());

        let config: Value = serde_json::from_str(
            r#"{"partial_commit":{"control_digits":true,"first_key":"f","last_key":"l"}}"#,
        )
        .unwrap();
        let processor = PartialCommit::new(&config);
        assert!(processor.control_digits);
        assert_eq!(processor.first_key, "f");
        assert_eq!(processor.last_key, "l");
    }

    /// 用一段真组合验证 `ConfirmedPrefix`：前一段已选「你」，末段是「好」的输入。
    #[test]
    fn confirmed_prefix_reads_selected_candidates() {
        use crate::engine::gear::translation::VecTranslation;
        use crate::engine::menu::Menu;

        let mut context = Context::new();
        context.input = "nihao".into();
        let mut segmentation = Segmentation::default();
        let mut first = Segment::new(0, 2);
        first.status = SegStatus::Selected;
        let menu = Menu::new();
        menu.add_translation(Box::new(VecTranslation::new(vec![Candidate::new(
            "script", 0, 2, "你", 1.0,
        )])));
        menu.prepare(1);
        first.menu = Some(menu);
        let second = Segment::new(2, 5);
        segmentation.push_back(first);
        segmentation.push_back(second);
        segmentation.forward();
        context.composition.segmentation = segmentation;
        assert_eq!(PartialCommit::confirmed_prefix(&context), "你");
    }
}
