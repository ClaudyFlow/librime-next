//! `gear/punctuator.cc`（439 行）移植：标点处理。
//!
//! 一个文件里有三个组件，共用同一份 [`PunctConfig`]：
//!
//! | 组件 | 类别 | 干什么 |
//! |---|---|---|
//! | `PunctSegmentor` | segmentor | 给"末尾那个标点字符"切一段，打 `punct` / `punct_number` 标签 |
//! | `PunctTranslator` | translator | 按 `punctuator/half_shape|full_shape` 映射产出候选 |
//! | `Punctuator` | processor | 吃掉标点键：入栈、轮换（连按换符号）、成对符号、直接上屏 |
//!
//! # 数字分隔符（`digit_separators`）
//!
//! 让「3.14」「1,000」「12:30」在原样键入时**不被翻成标点**。三处协作：
//!
//! 1. **`Punctuator`（processor）**：上一个提交是数字结尾（`is_after_number`）且当前
//!    没有组合时，分隔符 `.` 直接进输入串、立刻走一次分段 —— 这样一来
//!    `PunctSegmentor` 会给它打 `punct_number` 标签（而不是 `punct`），
//!    `PunctTranslator` 于是产出"字符本身"而不是「。」。
//! 2. **连按同一个分隔符**（`ReconvertDigitSeparatorAsPunct`）：把 `punct_number`
//!    改回 `punct` 并重开上一段，用户想要「3。」时敲两下 `.` 即可。
//! 3. **分隔符后面紧跟数字或空格**：说明还在数字里，把已组合的 `.` 和数字拼成一串
//!    直接提交，避免 `.` 又一次被翻译。
//!
//! 判"上一条是数字"读的是 [`CommitHistory`]（`cr.text` 末位是数字，且
//! `cr.kind` 是 `thru` 或 `raw`），所以 `3.` 之后打 `1`、`2` 才会连着走。

use crate::engine::gear::translation::{Translation as TranslationTrait, VecTranslation};
use serde_json::Value;

use crate::engine::candidate::Candidate;
use crate::engine::commit_history::CommitHistory;
use crate::engine::gear::processor::{ProcessResult, Processor};
use crate::engine::gear::segmentor::{Segmentor, SegmentorView};
use crate::engine::gear::shape::ShapeFormatter;
use crate::engine::gear::translator::Translator;
use crate::engine::key_event::{KeyEvent, K_SUPER_MASK, XK_SPACE};
use crate::engine::segmentation::{Segment, Segmentation, SegStatus};
use crate::engine::{Context, Engine};

/// `punct_number` 标签：这一段的字符是"数字里的分隔符"，要原样保留。
const TAG_PUNCT_NUMBER: &str = "punct_number";
/// `punct` 标签：普通标点段。
const TAG_PUNCT: &str = "punct";
/// 标点候选的 `kind`（C++ `New<SimpleCandidate>("punct", ...)`）。
const KIND_PUNCT: &str = "punct";

/// C++ `PunctConfig::digit_separators_` 的默认值。
const DEFAULT_DIGIT_SEPARATORS: &str = ".:";

/// `punctuator/half_shape`+`full_shape`+`symbols` 三张表 + 数字分隔符设置。
///
/// C++ 是**懒加载**：`shape_` 变了才重读（`punctuator.cc:27-31`）。lrn 的配置本来
/// 就在内存里（`Schema::config` 是 `serde_json::Value`），所以直接三张表全载，
/// 查询时按 `full_shape` 挑一张 —— 省掉"谁来触发重载"这个麻烦，语义等价。
pub struct PunctConfig {
    half_shape: Option<Value>,
    full_shape: Option<Value>,
    symbols: Option<Value>,
    /// `punctuator/digit_separators`，默认 `".:"`；显式配成空串即**关闭**该功能
    /// （C++ `has_digit_separators()` 就是判空）。
    digit_separators: String,
    /// `punctuator/digit_separator_action == "commit"`：分隔符进组合后立刻提交。
    digit_separator_commit: bool,
}

fn map_at(config: &Value, path: &str) -> Option<Value> {
    config.pointer(path).cloned().filter(Value::is_object)
}

impl PunctConfig {
    /// C++ `PunctConfig::LoadConfig(engine, load_symbols)`。
    ///
    /// `load_symbols`：只有 `Punctuator` 与 `PunctTranslator` 需要 `punctuator/symbols`
    /// 兜底表，`PunctSegmentor` 传 `false`。
    pub fn load(config: &Value, load_symbols: bool) -> Self {
        PunctConfig {
            half_shape: map_at(config, "/punctuator/half_shape"),
            full_shape: map_at(config, "/punctuator/full_shape"),
            symbols: if load_symbols {
                map_at(config, "/punctuator/symbols")
            } else {
                None
            },
            digit_separators: config
                .pointer("/punctuator/digit_separators")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_DIGIT_SEPARATORS)
                .to_string(),
            digit_separator_commit: config
                .pointer("/punctuator/digit_separator_action")
                .and_then(Value::as_str)
                == Some("commit"),
        }
    }

    /// C++ `GetPunctDefinition`：先查形状映射，miss 再查 `symbols`。
    pub fn get_punct_definition(&self, key: &str, full_shape: bool) -> Option<&Value> {
        let mapping = if full_shape {
            self.full_shape.as_ref()
        } else {
            self.half_shape.as_ref()
        };
        mapping
            .and_then(|m| m.get(key))
            .or_else(|| self.symbols.as_ref().and_then(|s| s.get(key)))
    }

    /// C++ `is_digit_separator(ch)`：`digit_separators_.find(ch) != npos`（按**字节**比）。
    pub fn is_digit_separator(&self, ch: u8) -> bool {
        self.digit_separators.as_bytes().contains(&ch)
    }

    /// C++ `has_digit_separators()`。
    pub fn has_digit_separators(&self) -> bool {
        !self.digit_separators.is_empty()
    }
}

// ------------------------------------------------------------ 数字判定

/// C++ `ends_with_digit`。
fn ends_with_digit(text: &str) -> bool {
    text.as_bytes().last().map_or(false, u8::is_ascii_digit)
}

/// C++ `is_after_number`：上一条提交是「数字结尾的 `thru` / `raw`」。
///
/// 为什么只认这两种：`.
/// `3` 是没被任何 processor 吃掉的键（`thru`），原样上屏的串是 `raw`；
/// 而"从候选里选出来的数字"（比如输入 `san` 选「3」）是 `table` 之类，
/// 那种情况直接接着打字更符合预期，不算数字串。
fn is_after_number(history: &CommitHistory) -> bool {
    let Some(record) = history.last() else {
        return false;
    };
    ends_with_digit(&record.text) && (record.is_thru() || record.is_raw())
}

/// C++ `is_after_digit_separator`：整个输入就是一段 `punct_number`。
fn is_after_digit_separator(ctx: &Context) -> bool {
    let segs = &ctx.composition.segmentation.segments;
    match segs.first() {
        Some(first) => first.has_tag(TAG_PUNCT_NUMBER) && first.length() == ctx.input.len(),
        None => false,
    }
}

// ------------------------------------------------------------ segmentor

pub struct PunctSegmentor {
    config: PunctConfig,
}

impl PunctSegmentor {
    pub fn new(config: &Value) -> Self {
        PunctSegmentor {
            config: PunctConfig::load(config, false),
        }
    }
}

impl Segmentor for PunctSegmentor {
    fn proceed(
        &self,
        segmentation: &mut Segmentation,
        view: SegmentorView,
        history: &CommitHistory,
    ) -> bool {
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
        let key = (ch as char).to_string();
        if self.config.get_punct_definition(&key, view.full_shape).is_none() {
            return true;
        }
        let mut segment = Segment::new(k, k + 1);
        if k == 0 && self.config.is_digit_separator(ch) && is_after_number(history) {
            // 数字里的分隔符：连同后面的空格一起吃掉（`3. 14` 打空格也不该变句号）。
            if input.len() > k + 1 && input.as_bytes()[k + 1] == b' ' {
                segment.end += 1;
            }
            segment.tags.push(TAG_PUNCT_NUMBER.to_string());
        } else {
            segment.tags.push(TAG_PUNCT.to_string());
        }
        segmentation.add_segment(segment);
        false // exclusive
    }
}

// ------------------------------------------------------------ translator

pub struct PunctTranslator {
    config: PunctConfig,
    /// 当前的 `full_shape` 开关。C++ 靠 `engine_` 现读；`Translator::query` 拿不到
    /// `Context`，所以由 [`Translator::sync_context`] 在每次翻译前刷新。
    full_shape: bool,
}

impl PunctTranslator {
    pub fn new(config: &Value) -> Self {
        PunctTranslator {
            config: PunctConfig::load(config, true),
            full_shape: false,
        }
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

/// `CreatePunctCandidate(punct, segment)`。
///
/// 坐标为**相对本段**（`runtime.rs` 会加 `segment.start`）。C++ 里传的是段自身的
/// 绝对 `start`/`end`，早先这里跟着传绝对坐标，多段输入时被加两次 —— 已统一成相对。
///
/// `one_key` = 段只覆盖一个字符 ⇔ preedit 显示标点本身（否则留空，让输入串原样显示）。
fn punct_candidate(punct: &str, segment: &Segment) -> Candidate {
    let end = segment.length();
    let mut c = Candidate::new(KIND_PUNCT, 0, end, punct.to_string(), 0.0);
    let comment = shape_comment(punct);
    if !comment.is_empty() {
        c = c.with_comment(comment.to_string());
    }
    if end == 1 {
        c = c.with_preedit(punct.to_string());
    }
    c
}

impl Translator for PunctTranslator {
    fn sync_context(&mut self, ctx: &Context) {
        self.full_shape = ctx.get_option("full_shape");
    }

    fn query(
        &mut self,
        input: &str,
        segment: &mut Segment,
        _history: &CommitHistory,
    ) -> Option<Box<dyn TranslationTrait>> {
        // ① 数字里的分隔符：候选就是"这个字符本身"（按 full_shape 转形状）。
        //    C++ 分支在 tag 检查之前，且**不看映射表**。
        if segment.has_tag(TAG_PUNCT_NUMBER) {
            if input.is_empty() {
                return None;
            }
            let punct = ShapeFormatter::formatted(input, self.full_shape);
            return Some(Box::new(crate::engine::gear::translation::VecTranslation::new(vec![punct_candidate(&punct, segment)])));
        }
        // ② 普通标点段
        if !segment.has_tag(TAG_PUNCT) {
            return None;
        }
        let Some(definition) = self.config.get_punct_definition(input, self.full_shape) else {
            return None;
        };
        // ②-a 唯一映射（`'` : '、'）
        if let Some(text) = definition.as_str() {
            return Some(Box::new(crate::engine::gear::translation::VecTranslation::new(vec![punct_candidate(text, segment)])));
        }
        // ②-b 轮换列表（连按同一个键依次取用）
        if let Some(list) = definition.as_array() {
            return Some(Box::new(crate::engine::gear::translation::VecTranslation::new(
                list.iter().filter_map(Value::as_str).map(|text| punct_candidate(text, segment)).collect(),
            )));
        }
        // ②-c `{ commit: "…" }` 直接上屏
        if let Some(map) = definition.as_object() {
            if let Some(text) = map.get("commit").and_then(Value::as_str) {
                return Some(Box::new(crate::engine::gear::translation::VecTranslation::new(vec![punct_candidate(text, segment)])));
            }
            // ②-d `{ pair: ["「", "」"] }` 成对，交替左右
            if let Some(pair) = map.get("pair").and_then(Value::as_array) {
                if pair.len() == 2 {
                    return Some(Box::new(crate::engine::gear::translation::VecTranslation::new(
                        pair.iter().filter_map(Value::as_str).map(|text| punct_candidate(text, segment)).collect(),
                    )));
                }
            }
        }
        None
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
    pub fn new(config: &Value) -> Self {
        Punctuator {
            config: PunctConfig::load(config, true),
            use_space: config
                .pointer("/punctuator/use_space")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            oddness: std::collections::HashMap::new(),
        }
    }

    /// C++ `AlternatePunct`：同一个键再按一次 → 候选往后挪一位。
    fn alternate_punct(&self, engine: &mut Engine, key: &str, full_shape: bool) -> bool {
        // 只有轮换列表（`[a, b, c]`）才有"下一位"可换
        if self
            .config
            .get_punct_definition(key, full_shape)
            .and_then(Value::as_array)
            .is_none()
        {
            return false;
        }
        let comp = &mut engine.context.composition;
        let Some(segment) = comp.segmentation.segments.last_mut() else {
            return false;
        };
        if segment.status <= SegStatus::Void
            || !segment.has_tag(TAG_PUNCT)
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
    fn confirm_unique_punct(&self, engine: &mut Engine, key: &str, full_shape: bool) -> bool {
        if self
            .config
            .get_punct_definition(key, full_shape)
            .and_then(Value::as_str)
            .is_none()
        {
            return false;
        }
        engine.context.confirm_current_selection()
    }

    /// C++ `AutoCommitPunct`：`{ commit: … }` 直接上屏。
    fn auto_commit_punct(&self, engine: &mut Engine, key: &str, full_shape: bool) -> bool {
        if self
            .config
            .get_punct_definition(key, full_shape)
            .and_then(Value::as_object)
            .map_or(false, |m| m.contains_key("commit"))
        {
            engine.context.commit();
            return true;
        }
        false
    }

    /// C++ `PairPunct`：`{ pair: [...] }` 按奇偶数左右交替并确认。
    fn pair_punct(&mut self, engine: &mut Engine, key: &str, full_shape: bool) -> bool {
        let is_pair = self
            .config
            .get_punct_definition(key, full_shape)
            .and_then(Value::as_object)
            .map_or(false, |m| m.contains_key("pair"));
        if !is_pair {
            return false;
        }
        let comp = &engine.context.composition;
        let Some(segment) = comp.segmentation.segments.last() else {
            return false;
        };
        if segment.status <= SegStatus::Void || !segment.has_tag(TAG_PUNCT) {
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

    /// C++ `ConvertDigitSeparator`：数字后面紧跟的分隔符，原样进组合。
    ///
    /// 关键点：`push_input_ch` 在 lrn 里**不触发** `Compose`（C++ 靠
    /// `update_notifier_` 同步触发），所以必须自己调一次 [`Engine::compose`]，
    /// 否则分段/翻译还没发生，`punct_number` 标签就无从判断。
    fn convert_digit_separator(&self, engine: &mut Engine, ch: u8) -> bool {
        if !self.config.is_digit_separator(ch) {
            return false;
        }
        {
            let ctx = &mut engine.context;
            if !ctx.composition.empty() || !is_after_number(&ctx.commit_history) {
                return false;
            }
            ctx.push_input_ch(ch as char);
        }
        engine.compose();
        if punctuation_is_translated(engine, TAG_PUNCT_NUMBER) {
            if self.config.digit_separator_commit {
                engine.context.commit();
            } else {
                engine.context.composition.segmentation.forward();
            }
        }
        true
    }

    /// C++ `ReconvertDigitSeparatorAsPunct`：再按一次同一个分隔符 → 变回普通标点。
    ///
    /// 触发条件是"整个输入就是这一个分隔符"，此时把 `punct_number` 换成 `punct`、
    /// 状态打回 `Void`，再 `ReopenPreviousSegment()`（这一步只顺带 `Trim` 掉
    /// `ConvertDigitSeparator` 留下的空段，段够长时什么也不做）。
    ///
    /// 注意它**不重译**：菜单里留着上一轮的候选，所以随后上屏的还是那一个字符
    /// （`3..` → `3.`）。这是 C++ 的既成行为，照抄。
    fn reconvert_digit_separator_as_punct(&self, engine: &mut Engine, key: &str) -> bool {
        if !self.config.has_digit_separators() {
            return false;
        }
        if engine.context.input != key {
            return false;
        }
        let Some(segment) = engine.context.composition.segmentation.segments.first_mut() else {
            return false;
        };
        if !segment.has_tag(TAG_PUNCT_NUMBER) {
            return false;
        }
        segment.tags.retain(|t| t != TAG_PUNCT_NUMBER);
        segment.tags.push(TAG_PUNCT.to_string());
        segment.status = SegStatus::Void;
        engine.context.reopen_previous_segment();
        true
    }
}

/// C++ `punctuation_is_translated`：末段（或首段，见调用点）带指定标签、
/// 且当前选中的候选类型是 `punct`。
fn punctuation_is_translated(engine: &Engine, tag: &str) -> bool {
    let Some(segment) = engine.context.composition.segmentation.segments.last() else {
        return false;
    };
    if !segment.has_tag(tag) {
        return false;
    }
    segment
        .menu
        .as_ref()
        .and_then(|m| m.candidate_at(segment.selected_index))
        .map_or(false, |c| c.kind == KIND_PUNCT)
}

impl Processor for Punctuator {
    fn process_key_event(&mut self, engine: &mut Engine, key: &KeyEvent) -> ProcessResult {
        if key.release() || key.ctrl() || key.alt() || (key.modifier & K_SUPER_MASK != 0) {
            return ProcessResult::Noop;
        }
        let ch = key.keycode;
        if !(0x20..0x7f).contains(&ch) {
            return ProcessResult::Noop;
        }
        if engine.context.get_option("ascii_punct") {
            return ProcessResult::Noop;
        }
        let byte = ch as u8;

        // ① 分隔符后面紧跟数字或空格 ⇒ 还在数字里：拼成一串直接提交。
        //    不能只 push 一个字符，否则 `3.` + `1` 会各自走一遍分段，`.` 又被翻译成「。」。
        if (byte.is_ascii_digit() || ch == XK_SPACE) && is_after_digit_separator(&engine.context) {
            let text = {
                let ctx = &engine.context;
                let Some(first) = ctx.composition.segmentation.segments.first() else {
                    return ProcessResult::Accepted;
                };
                let body = ctx
                    .input
                    .get(first.start..first.end)
                    .unwrap_or_default()
                    .to_string();
                format!("{body}{}", byte as char)
            };
            engine.context.abort_composition();
            engine.context.push_input_str(&text);
            // C++ 的 `PushInput` 会经 `update_notifier_` 触发 `Compose`，lrn 没有这条链，
            // 必须显式补一次 —— 否则 `commit()` 看到的组合是空的，什么都不会上屏。
            engine.compose();
            engine.context.commit();
            return ProcessResult::Accepted;
        }

        if !self.use_space && ch == XK_SPACE && engine.context.is_composing() {
            return ProcessResult::Noop;
        }
        // ② 数字分隔符
        if self.convert_digit_separator(engine, byte) {
            return ProcessResult::Accepted;
        }
        // 与 full_shape 开关同步（C++ 在这里 `LoadConfig`；lrn 已把两张表都载好了）
        let full_shape = engine.context.get_option("full_shape");

        let key_str = (byte as char).to_string();
        if self
            .config
            .get_punct_definition(&key_str, full_shape)
            .is_none()
        {
            return ProcessResult::Noop;
        }
        if self.alternate_punct(engine, &key_str, full_shape) {
            return ProcessResult::Accepted;
        }
        // ③ 再按一次同一个分隔符 ⇒ 改回普通标点。
        //
        // 这里**不能**再 `compose()`：段标签已被就地改成 `punct`，重新分段会立刻
        // 又被打回 `punct_number`（C++ 也不重算，`ReopenPreviousSegment` 的
        // `update_notifier_` 在段不空时根本不触发）。菜单里留着的仍是上一轮的候选，
        // `punctuation_is_translated` 就靠它判定，随后 `AutoCommitPunct` 直接上屏。
        if self.reconvert_digit_separator_as_punct(engine, &key_str) {
            if punctuation_is_translated(engine, TAG_PUNCT) {
                let _ = self.confirm_unique_punct(engine, &key_str, full_shape)
                    || self.auto_commit_punct(engine, &key_str, full_shape)
                    || self.pair_punct(engine, &key_str, full_shape);
            }
            return ProcessResult::Accepted;
        }
        if engine.context.push_input_ch(byte as char) {
            // ⚠️ 这里必须**自己**重算一次组合。C++ 靠 `Context::PushInput` 触发
            // `update_notifier` → `Compose` 同步完成；lrn 的 compose 是在
            // `Engine::process_key` 末尾"输入变了才重算"，此刻还没发生 ——
            // 不补这一下，下面的确认/上屏看到的会是空段（实测提交出空串）。
            engine.compose();
            if punctuation_is_translated(engine, TAG_PUNCT) {
                let _ = self.confirm_unique_punct(engine, &key_str, full_shape)
                    || self.auto_commit_punct(engine, &key_str, full_shape)
                    || self.pair_punct(engine, &key_str, full_shape);
            }
        }
        ProcessResult::Accepted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::gear::translation::drain_opt;

    fn config() -> Value {
        serde_json::json!({
            "punctuator": {
                "half_shape": {
                    "'": ["、", "‘", "’"],
                    ",": "，",
                    ".": "。",
                    ":": "：",
                    "\"": { "pair": ["“", "”"] },
                    "`": { "commit": "`" }
                },
                "full_shape": {
                    ",": "，",
                    ".": "．"
                }
            }
        })
    }

    /// 只配 half/full 两张表、数字分隔符开关走默认值的方案（`.:`）。
    fn engine_with(config: &Value) -> Engine {
        let mut engine = Engine::empty_for_test();
        engine.schema = crate::engine::schema::Schema::from_config_for_test(config.clone());
        engine
    }

    #[test]
    fn translator_expands_alternating_list() {
        let mut t = PunctTranslator::new(&config());
        let mut seg = Segment::new(0, 1);
        seg.tags.push(TAG_PUNCT.to_string());
        let cands = drain_opt(t.query("'", &mut seg, &CommitHistory::new()));
        let texts: Vec<&str> = cands.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["、", "‘", "’"]);
    }

    #[test]
    fn translator_expands_unique_mapping_and_pair() {
        let mut t = PunctTranslator::new(&config());
        let mut seg = Segment::new(0, 1);
        seg.tags.push(TAG_PUNCT.to_string());
        assert_eq!(drain_opt(t.query(",", &mut seg, &CommitHistory::new()))[0].text, "，");
        let pair: Vec<String> = drain_opt(t.query("\"", &mut seg, &CommitHistory::new()))
            .into_iter()
            .map(|c| c.text.clone())
            .collect();
        assert_eq!(pair, vec!["“".to_string(), "”".to_string()]);
        // `{ commit: … }`
        assert_eq!(drain_opt(t.query("`", &mut seg, &CommitHistory::new()))[0].text, "`");
    }

    #[test]
    fn translator_ignores_segments_without_punct_tags() {
        let mut t = PunctTranslator::new(&config());
        let mut seg = Segment::new(0, 1); // 无标签
        assert!(drain_opt(t.query(",", &mut seg, &CommitHistory::new())).is_empty());
        let mut seg = Segment::new(0, 2);
        seg.tags.push("punct_number".to_string());
        // punct_number 段不看映射表：输入什么就给什么
        assert_eq!(drain_opt(t.query(",.", &mut seg, &CommitHistory::new()))[0].text, ",.");
    }

    #[test]
    fn full_shape_option_switches_the_mapping() {
        let mut half = PunctTranslator::new(&config());
        let mut full = PunctTranslator::new(&config());
        let mut ctx = Context::new();
        ctx.set_option("full_shape", true);
        full.sync_context(&ctx);
        let mut seg = Segment::new(0, 1);
        seg.tags.push(TAG_PUNCT.to_string());
        // 半角表里有 `'` 的多选，全角表里没有 → 全角时该键不出候选
        assert_eq!(drain_opt(half.query("'", &mut seg, &CommitHistory::new())).len(), 3);
        assert!(drain_opt(full.query("'", &mut seg, &CommitHistory::new())).is_empty());
        assert_eq!(drain_opt(full.query(",", &mut seg, &CommitHistory::new()))[0].text, "，");
    }

    #[test]
    fn unknown_punct_is_noop() {
        let mut t = PunctTranslator::new(&config());
        let mut seg = Segment::new(0, 1);
        seg.tags.push(TAG_PUNCT.to_string());
        assert!(drain_opt(t.query("z", &mut seg, &CommitHistory::new())).is_empty());
    }

    #[test]
    fn segmentor_tags_trailing_punctuation() {
        let s = PunctSegmentor::new(&config());
        let mut seg = Segmentation {
            input: "abc,".to_string(),
            segments: vec![Segment::new(0, 3)],
        };
        seg.forward();
        assert!(!s.proceed(&mut seg, SegmentorView::default(), &CommitHistory::new())); // exclusive
        let (start, end, tags) = {
            let last = seg.segments.last().unwrap();
            (last.start, last.end, last.tags.clone())
        };
        assert_eq!((start, end), (3, 4));
        assert!(tags.contains(&TAG_PUNCT.to_string()));
    }

    /// 上一条提交是数字 ⇒ 分隔符打 `punct_number`，且尾随空格一并吃进段里。
    #[test]
    fn segmentor_tags_digit_separator_as_punct_number() {
        let s = PunctSegmentor::new(&config());
        let mut history = CommitHistory::new();
        history.push(crate::engine::commit_history::CommitRecord::thru('3'));
        let mut seg = Segmentation {
            input: ". ".to_string(),
            segments: Vec::new(),
        };
        assert!(!s.proceed(&mut seg, SegmentorView::default(), &history));
        let last = seg.segments.last().unwrap();
        assert_eq!((last.start, last.end), (0, 2), "分隔符 + 尾随空格");
        assert!(last.has_tag(TAG_PUNCT_NUMBER));

        // 上一条不是数字 ⇒ 普通标点
        let mut seg = Segmentation {
            input: ".".to_string(),
            segments: Vec::new(),
        };
        assert!(!s.proceed(&mut seg, SegmentorView::default(), &CommitHistory::new()));
        assert!(seg.segments.last().unwrap().has_tag(TAG_PUNCT));
    }

    #[test]
    fn processor_ignores_ascii_punct_option() {
        let mut engine = engine_with(&config());
        engine.set_option("ascii_punct", true);
        let mut p = Punctuator::new(&config());
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(b',' as u32, 0)),
            ProcessResult::Noop
        );
        assert_eq!(engine.context.input, "");
    }

    #[test]
    fn processor_accepts_known_punct_and_pushes_input() {
        let mut engine = engine_with(&config());
        let mut p = Punctuator::new(&config());
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(b',' as u32, 0)),
            ProcessResult::Accepted
        );
        assert_eq!(engine.context.input, ",");
    }

    /// `is_after_number` 只认 `thru` / `raw` 两种记录（数字是"直接穿过去"的键）。
    #[test]
    fn is_after_number_reads_thru_and_raw_records_only() {
        use crate::engine::commit_history::CommitRecord;
        let mut h = CommitHistory::new();
        assert!(!is_after_number(&h), "空历史不算");
        h.push(CommitRecord::thru('3'));
        assert!(is_after_number(&h), "thru '3' ⇒ 是数字");
        h.push(CommitRecord::thru('.'));
        assert!(!is_after_number(&h), "末位不是数字 ⇒ 否");
        // 从候选里选出来的数字（kind 是 table）不算
        let mut h = CommitHistory::new();
        h.push(CommitRecord::new("table", "3"));
        assert!(!is_after_number(&h), "候选来的数字不触发数字串");
        // raw 串的末位是数字 ⇒ 算（`abc123` 这种原样上屏）
        let mut h = CommitHistory::new();
        h.push(CommitRecord::new("raw", "abc123"));
        assert!(is_after_number(&h));
    }

    #[test]
    fn digit_separators_can_be_disabled() {
        let cfg = serde_json::json!({
            "punctuator": {
                "half_shape": { ".": "。" },
                "digit_separators": ""
            }
        });
        let mut engine = engine_with(&cfg);
        let mut p = Punctuator::new(&cfg);
        engine.context.commit_history.push(crate::engine::commit_history::CommitRecord::thru('3'));
        assert_eq!(
            p.process_key_event(&mut engine, &KeyEvent::new(b'.' as u32, 0)),
            ProcessResult::Accepted
        );
        // 关了数字分隔符 ⇒ 不进数字串路径，字符照常进输入串
        assert_eq!(engine.context.input, ".");
    }

    /// `digit_separators` 默认是 `".:"`，可以配成别的字符集。
    #[test]
    fn digit_separator_set_is_configurable() {
        let cfg = serde_json::json!({
            "punctuator": { "half_shape": { "-": "-" }, "digit_separators": "-'" }
        });
        let c = PunctConfig::load(&cfg, false);
        assert!(c.is_digit_separator(b'-'));
        assert!(c.is_digit_separator(b'\''));
        assert!(!c.is_digit_separator(b'.'), "被配置覆盖掉了");
        assert!(c.has_digit_separators());

        let c = PunctConfig::load(&serde_json::json!({"punctuator": {}}), false);
        assert!(c.is_digit_separator(b'.') && c.is_digit_separator(b':'), "默认 .:");
        assert!(!c.digit_separator_commit, "默认不立即提交");
    }
}
