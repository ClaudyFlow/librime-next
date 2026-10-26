//! `gear/punctuator.cc`（439 行）移植：标点处理。
//!
//! 一个文件里有三个组件，共用同一份 [`PunctConfig`]：
//!
//! | 组件 | 类别 | 干什么 |
//! |---|---|---|
//! | `PunctSegmentor` | segmentor | 给"末尾那个标点字符"切一段，打 `punct` 标签 |
//! | `PunctTranslator` | translator | 按 `punctuator/half_shape|full_shape` 映射产出候选 |
//! | `Punctuator` | processor | 吃掉标点键：入栈、轮换（连按换符号）、成对符号、直接上屏 |
//!
//! # 本轮刻意没移植的部分
//!
//! 原版有一整套 **数字分隔符**逻辑（`digit_separators` / `punct_number` /
//! `ConvertDigitSeparator` / `ReconvertDigitSeparatorAsPunct`），用来让
//! 「3.14」「1,000」「12:30」在原样键入时**不被翻成标点**。
//!
//! 它的前置条件（`Context::commit_history()`，"上一条提交是不是数字"）**已经就位**
//! （`src/engine/commit_history.rs`），剩下挡路的是接口而不是数据：
//!
//! * `PunctSegmentor::Proceed` 要给"落在数字后面的分隔符"打 `punct_number` 标签，
//!   而 [`Segmentor`] trait 只收 `&mut Segmentation`，拿不到 `Context`；
//! * `Punctuator::ConvertDigitSeparator` 反过来要在**没有组合**时读历史，
//!   这条 processor 侧倒是拿得到。
//!
//! 所以整块暂时不做，配置照读不误。影响：数字后紧跟 `.` `,` 这类符号时，
//! 原版会"原样上屏"，lrn 走普通标点路径（`3.14` 会变成 `3。14`）。

use serde_json::Value;

use crate::engine::candidate::Candidate;
use crate::engine::gear::processor::{ProcessResult, Processor};
use crate::engine::gear::segmentor::{Segmentor, SegmentorView};
use crate::engine::gear::translator::Translator;
use crate::engine::key_event::{KeyEvent, XK_SPACE};
use crate::engine::segmentation::{Segment, Segmentation, SegStatus};
use crate::engine::Engine;

/// `punctuator/full_shape` 与 `punctuator/half_shape` 的映射表。
///
/// C++ 每次按键都 `LoadConfig` 一次，好跟 `full_shape` 开关同步；这里同样
/// 靠 `shape_` 记住当前形状，形状变了才重读。
struct PunctConfig {
    shape: Option<String>,
    mapping: Option<Value>,
    symbols: Option<Value>,
}

impl PunctConfig {
    fn new() -> Self {
        PunctConfig {
            shape: None,
            mapping: None,
            symbols: None,
        }
    }

    /// C++ `PunctConfig::LoadConfig(engine, load_symbols)`。
    fn load_config(&mut self, config: &Value, full_shape: bool, load_symbols: bool) {
        let shape = if full_shape { "full_shape" } else { "half_shape" };
        if self.shape.as_deref() == Some(shape) {
            return;
        }
        self.shape = Some(shape.to_string());
        self.mapping = config
            .pointer(&format!("/punctuator/{shape}"))
            .cloned()
            .filter(Value::is_object);
        if self.mapping.is_none() {
            eprintln!("[punctuator] missing punctuation mapping: {shape}");
        }
        if load_symbols {
            self.symbols = config
                .pointer("/punctuator/symbols")
                .cloned()
                .filter(Value::is_object);
        }
    }

    /// C++ `GetPunctDefinition`：先查形状映射，miss 再查 `symbols`。
    fn get_punct_definition(&self, key: &str) -> Option<&Value> {
        self.mapping
            .as_ref()
            .and_then(|m| m.get(key))
            .or_else(|| self.symbols.as_ref().and_then(|s| s.get(key)))
    }
}

// ------------------------------------------------------------ segmentor

pub struct PunctSegmentor {
    config: PunctConfig,
}

impl PunctSegmentor {
    pub fn new() -> Self {
        PunctSegmentor {
            config: PunctConfig::new(),
        }
    }
}

impl Segmentor for PunctSegmentor {
    fn proceed(&self, segmentation: &mut Segmentation, _view: SegmentorView) -> bool {
        let input = segmentation.input.clone();
        let k = segmentation.current_start();
        if k == input.len() {
            return false; // 没机会了，后面的人也别试
        }
        let Some(ch) = input.as_bytes().get(k).copied() else {
            return true;
        };
        if !(0x20..0x7f).contains(&ch) {
            return true;
        }
        let mut config = PunctConfig::new();
        config.load_config(&Value::Null, false, true);
        // 真正的配置在引擎侧；这里只做"是不是已知标点"的判断，配置由
        // Punctuator/PunctTranslator 各自持有。见 [`PunctSegmentor::with_config`]。
        if !self.is_known_punct(&(ch as char).to_string()) {
            return true;
        }
        let mut segment = Segment::new(k, k + 1);
        segment.tags.push("punct".to_string());
        segmentation.add_segment(segment);
        false // exclusive
    }
}

impl PunctSegmentor {
    fn is_known_punct(&self, key: &str) -> bool {
        self.config.get_punct_definition(key).is_some()
    }

    /// 装配时把方案配置灌进来（我们的组件在 `initialize_components` 里一次性拿到配置）。
    pub fn with_config(config: &Value, full_shape: bool) -> Self {
        let mut punct = PunctSegmentor::new();
        punct.config.load_config(config, full_shape, false);
        punct
    }
}

// ------------------------------------------------------------ translator

pub struct PunctTranslator {
    config: PunctConfig,
}

impl PunctTranslator {
    pub fn with_config(config: &Value, full_shape: bool) -> Self {
        let mut punct = PunctTranslator {
            config: PunctConfig::new(),
        };
        punct.config.load_config(config, full_shape, true);
        punct
    }
}

/// C++ `CreatePunctCandidate`：候选上带 〔半角〕/〔全角〕 注释。
fn shape_comment(punct: &str) -> &'static str {
    let Some(ch) = punct.chars().next() else {
        return "";
    };
    if punct.chars().count() != 1 {
        return "";
    }
    let c = ch as u32;
    let is_ascii = (0x20..0x7f).contains(&c);
    let is_half_shape_kana = (0xff61..=0xff9f).contains(&c);
    let is_half_shape_hangul = (0xffa0..=0xffdc).contains(&c);
    let is_narrow_symbol = matches!(c, 0xa2 | 0xa3 | 0xa5 | 0xa6 | 0xac | 0xaf | 0x2985 | 0x2986);
    let is_half_shape_wide_symbol = (0xffe8..=0xffee).contains(&c);
    let is_full_shape_ascii = (0xff01..=0xff5e).contains(&c);
    let is_ideographic_space = c == 0x3000;
    let is_kana = (0x30a1..=0x30fc).contains(&c)
        || matches!(c, 0x3001 | 0x3002 | 0x300c | 0x300d | 0x309b | 0x309c);
    let is_hangul = (0x3131..=0x3164).contains(&c);
    let is_full_shape_narrow_symbol =
        c == 0xff5f || c == 0xff60 || (0xffe0..=0xffe6).contains(&c);
    let is_wide_symbol = (0x2190..=0x2193).contains(&c)
        || matches!(c, 0x2502 | 0x25a0 | 0x25cb);
    let is_half_shape = is_ascii
        || is_half_shape_kana
        || is_half_shape_hangul
        || is_narrow_symbol
        || is_half_shape_wide_symbol;
    let is_full_shape = is_ideographic_space
        || is_full_shape_ascii
        || is_kana
        || is_hangul
        || is_full_shape_narrow_symbol
        || is_wide_symbol;
    if is_half_shape {
        "〔半角〕"
    } else if is_full_shape {
        "〔全角〕"
    } else {
        ""
    }
}

fn punct_candidate(text: &str, start: usize, end: usize) -> Candidate {
    let mut c = Candidate::new("punct", start, end, text.to_string(), 0.0);
    let comment = shape_comment(text);
    if !comment.is_empty() {
        c = c.with_comment(comment.to_string());
    }
    c
}

impl Translator for PunctTranslator {
    fn query(
        &mut self,
        input: &str,
        _segment: &mut Segment,
        _history: &crate::engine::commit_history::CommitHistory,
    ) -> Vec<Candidate> {
        let Some(definition) = self.config.get_punct_definition(input) else {
            return Vec::new();
        };
        // 候选坐标**相对本段**（`runtime.rs` 会加 `segment.start`）。原来这里传的是
        // 绝对位置，多段输入时会被加两次。
        let start = 0;
        let end = input.len();
        // ① 唯一映射（`'` : '、'）
        if let Some(text) = definition.as_str() {
            return vec![punct_candidate(text, start, end)];
        }
        // ② 轮换列表（连按同一个键依次取用）
        if let Some(list) = definition.as_array() {
            return list
                .iter()
                .filter_map(Value::as_str)
                .map(|text| punct_candidate(text, start, end))
                .collect();
        }
        // ③ `{ commit: "…" }` 直接上屏
        if let Some(map) = definition.as_object() {
            if let Some(text) = map.get("commit").and_then(Value::as_str) {
                return vec![punct_candidate(text, start, end)];
            }
            // ④ `{ pair: ["「", "」"] }` 成对，交替左右
            if let Some(pair) = map.get("pair").and_then(Value::as_array) {
                if pair.len() == 2 {
                    return pair
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|text| punct_candidate(text, start, end))
                        .collect();
                }
            }
        }
        Vec::new()
    }
}

// ------------------------------------------------------------ processor

pub struct Punctuator {
    config: PunctConfig,
    use_space: bool,
    /// C++ `oddness_`：成对符号的左/右奇偶。
    oddness: std::collections::HashMap<String, usize>,
}

impl Punctuator {
    pub fn with_config(config: &Value, full_shape: bool) -> Self {
        let mut punct = Punctuator {
            config: PunctConfig::new(),
            use_space: config
                .pointer("/punctuator/use_space")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            oddness: std::collections::HashMap::new(),
        };
        punct.config.load_config(config, full_shape, true);
        punct
    }

    /// C++ `AlternatePunct`：同一个键再按一次 → 候选往后挪一位。
    fn alternate_punct(&self, engine: &mut Engine, key: &str) -> bool {
        let Some(list) = self
            .config
            .get_punct_definition(key)
            .and_then(Value::as_array)
        else {
            return false;
        };
        let comp = &mut engine.context.composition;
        let Some(segment) = comp.segmentation.segments.last_mut() else {
            return false;
        };
        if segment.status <= SegStatus::Void
            || !segment.has_tag("punct")
            || segment.end - segment.start != key.len()
        {
            return false;
        }
        if segment.start + key.len() > engine.context.input.len() {
            return false;
        }
        if engine.context.input[segment.start..segment.start + key.len()] != *key {
            return false;
        }
        let Some(menu) = segment.menu.as_ref() else {
            return false;
        };
        if menu.candidate_count() == 0 {
            return false;
        }
        segment.selected_index = (segment.selected_index + 1) % menu.candidate_count();
        segment.status = SegStatus::Guess;
        true
    }

    /// C++ `ConfirmUniquePunct`：唯一映射直接确认。
    fn confirm_unique_punct(&self, engine: &mut Engine, key: &str) -> bool {
        if self
            .config
            .get_punct_definition(key)
            .and_then(Value::as_str)
            .is_none()
        {
            return false;
        }
        engine.context.confirm_current_selection()
    }

    /// C++ `AutoCommitPunct`：`{ commit: … }` 直接上屏。
    fn auto_commit_punct(&self, engine: &mut Engine, key: &str) -> bool {
        if self
            .config
            .get_punct_definition(key)
            .and_then(Value::as_object)
            .map_or(false, |m| m.contains_key("commit"))
        {
            engine.context.commit();
            return true;
        }
        false
    }

    /// C++ `PairPunct`：`{ pair: [...] }` 按奇偶数左右交替并确认。
    fn pair_punct(&mut self, engine: &mut Engine, key: &str) -> bool {
        let is_pair = self
            .config
            .get_punct_definition(key)
            .and_then(Value::as_object)
            .map_or(false, |m| m.contains_key("pair"));
        if !is_pair {
            return false;
        }
        let comp = &engine.context.composition;
        let Some(segment) = comp.segmentation.segments.last() else {
            return false;
        };
        if segment.status <= SegStatus::Void || !segment.has_tag("punct") {
            return false;
        }
        let count = segment.menu.as_ref().map_or(0, |m| m.candidate_count());
        if count < 2 {
            return false;
        }
        let oddness = *self.oddness.get(key).unwrap_or(&0);
        let comp = &mut engine.context.composition;
        if let Some(segment) = comp.segmentation.segments.last_mut() {
            segment.selected_index = (segment.selected_index + oddness) % 2;
        }
        self.oddness.insert(key.to_string(), 1 - oddness);
        engine.context.confirm_current_selection();
        true
    }
}

/// C++ `punctuation_is_translated`：末段带 `punct` 标签、且当前选中的候选类型是 `punct`。
fn punctuation_is_translated(engine: &Engine) -> bool {
    let Some(segment) = engine.context.composition.segmentation.segments.last() else {
        return false;
    };
    if !segment.has_tag("punct") {
        return false;
    }
    segment
        .menu
        .as_ref()
        .and_then(|m| m.candidate_at(segment.selected_index))
        .map_or(false, |c| c.kind == "punct")
}

impl Processor for Punctuator {
    fn process_key_event(&mut self, engine: &mut Engine, key: &KeyEvent) -> ProcessResult {
        if key.release() || key.ctrl() || key.alt() || (key.modifier & 0x0400_0000 != 0) {
            return ProcessResult::Noop;
        }
        let ch = key.keycode;
        if !(0x20..0x7f).contains(&ch) {
            return ProcessResult::Noop;
        }
        if engine.context.get_option("ascii_punct") {
            return ProcessResult::Noop;
        }
        if !self.use_space && ch == XK_SPACE && engine.context.is_composing() {
            return ProcessResult::Noop;
        }
        // 与 full_shape 开关同步
        let full_shape = engine.context.get_option("full_shape");
        self.config
            .load_config(&engine.schema.config.clone(), full_shape, true);

        let key_str = (ch as u8 as char).to_string();
        if self
            .config
            .get_punct_definition(&key_str)
            .is_none()
        {
            return ProcessResult::Noop;
        }
        if self.alternate_punct(engine, &key_str) {
            return ProcessResult::Accepted;
        }
        if engine.context.push_input_ch(ch as u8 as char) {
            // ⚠️ 这里必须**自己**重算一次组合。C++ 靠 `Context::PushInput` 触发
            // `update_notifier` → `Compose` 同步完成；lrn 的 compose 是在
            // `Engine::process_key` 末尾"输入变了才重算"，此刻还没发生 ——
            // 不补这一下，下面的确认/上屏看到的会是空段（实测提交出空串）。
            engine.compose();
            if punctuation_is_translated(engine) {
                let _ = self.confirm_unique_punct(engine, &key_str)
                    || self.auto_commit_punct(engine, &key_str)
                    || self.pair_punct(engine, &key_str);
            }
        }
        ProcessResult::Accepted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::commit_history::CommitHistory;

    fn config() -> Value {
        serde_json::json!({
            "punctuator": {
                "half_shape": {
                    "'": ["、", "‘", "’"],
                    ",": "，",
                    ".": "。",
                    "\"": { "pair": ["“", "”"] },
                    "`": { "commit": "`" }
                },
                "full_shape": {
                    ",": "，",
                    ".": "。"
                }
            }
        })
    }

    #[test]
    fn translator_expands_alternating_list() {
        let mut t = PunctTranslator::with_config(&config(), false);
        let cands = t.query("'", &mut Segment::new(0, 0), &CommitHistory::new());
        let texts: Vec<&str> = cands.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["、", "‘", "’"]);
    }

    #[test]
    fn translator_expands_unique_mapping_and_pair() {
        let mut t = PunctTranslator::with_config(&config(), false);
        assert_eq!(t.query(",", &mut Segment::new(0, 0), &CommitHistory::new())[0].text, "，");
        let pair: Vec<String> = t
            .query("\"", &mut Segment::new(0, 0), &CommitHistory::new())
            .into_iter()
            .map(|c| c.text.clone())
            .collect();
        assert_eq!(pair, vec!["“".to_string(), "”".to_string()]);
        // `{ commit: … }`
        assert_eq!(t.query("`", &mut Segment::new(0, 0), &CommitHistory::new())[0].text, "`");
    }

    #[test]
    fn full_shape_option_switches_the_mapping() {
        let mut half = PunctTranslator::with_config(&config(), false);
        let mut full = PunctTranslator::with_config(&config(), true);
        // 半角表里有 `'` 的多选，全角表里没有 → 全角时该键不出候选
        assert_eq!(half.query("'", &mut Segment::new(0, 0), &CommitHistory::new()).len(), 3);
        assert!(full.query("'", &mut Segment::new(0, 0), &CommitHistory::new()).is_empty());
        assert_eq!(full.query(",", &mut Segment::new(0, 0), &CommitHistory::new())[0].text, "，");
    }

    #[test]
    fn unknown_punct_is_noop() {
        let mut t = PunctTranslator::with_config(&config(), false);
        assert!(t.query("z", &mut Segment::new(0, 0), &CommitHistory::new()).is_empty());
    }

    #[test]
    fn segmentor_tags_trailing_punctuation() {
        let s = PunctSegmentor::with_config(&config(), false);
        let mut seg = Segmentation {
            input: "abc,".to_string(),
            segments: vec![Segment::new(0, 3)],
        };
        seg.forward();
        assert!(!s.proceed(&mut seg, SegmentorView::default())); // exclusive
        let (start, end, tags) = {
            let last = seg.segments.last().unwrap();
            (last.start, last.end, last.tags.clone())
        };
        assert_eq!((start, end), (3, 4));
        assert!(tags.contains(&"punct".to_string()));
    }

    #[test]
    fn processor_ignores_ascii_punct_option() {
        let mut engine = Engine::empty_for_test();
        engine.schema = crate::engine::schema::Schema::from_config_for_test(config());
        engine.set_option("ascii_punct", true);
        let mut p = Punctuator::with_config(&config(), false);
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(b',' as u32, 0)),
            ProcessResult::Noop
        );
        assert_eq!(engine.context.input, "");
    }

    #[test]
    fn processor_accepts_known_punct_and_pushes_input() {
        let mut engine = Engine::empty_for_test();
        engine.schema = crate::engine::schema::Schema::from_config_for_test(config());
        let mut p = Punctuator::with_config(&config(), false);
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(b',' as u32, 0)),
            ProcessResult::Accepted
        );
        assert_eq!(engine.context.input, ",");
    }
}
