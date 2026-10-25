//! `gear/speller.cc` 全量移植：拼写处理器。
//!
//! 可打印键（在 alphabet/delimiter 内）入栈组字；带 auto_select / max_code_length /
//! auto_clear / auto_select_pattern 的各类自动选定与自动清空路径与 C++ 逐条对齐。

use crate::engine::candidate::Candidate;
use crate::engine::context::Context;
use crate::engine::gear::processor::{ProcessResult, Processor};
use crate::engine::key_event::{KeyEvent, XK_SPACE};
use crate::engine::Engine;

const DEFAULT_ALPHABET: &str = "zyxwvutsrqponmlkjihgfedcba";

#[derive(Clone, Copy, PartialEq, Eq)]
enum AutoClear {
    None,
    Auto,
    Manual,
    MaxLength,
}

pub struct Speller {
    alphabet: String,
    delimiters: String,
    initials: String,
    finals: String,
    max_code_length: i32,
    auto_select: bool,
    use_space: bool,
    auto_clear: AutoClear,
    auto_select_pattern: Option<fancy_regex::Regex>,
}

fn belongs_to(ch: char, charset: &str) -> bool {
    charset.contains(ch)
}

/// fancy_regex 没有 `Regex::is_full_match`；`auto_select_pattern` 在
/// [`Speller::new`] 里已加 `^(?:…)$` 锚点，因此 find 命中即整串匹配。
fn regex_full_match(re: &fancy_regex::Regex, s: &str) -> bool {
    re.find(s).ok().flatten().is_some()
}

fn reached_max_code_length(cand: Option<&Candidate>, max_code_length: i32) -> bool {
    match cand {
        Some(c) => (c.end.saturating_sub(c.start)) as i32 >= max_code_length,
        None => false,
    }
}

fn is_auto_selectable(
    cand: Option<&Candidate>,
    input: &str,
    delimiters: &str,
) -> bool {
    match cand {
        Some(c) => {
            if c.end != input.len() {
                return false;
            }
            // C++ 还要求 table/simple 候选；我们的候选都等价 simple，恒真。
            !input[c.start.min(input.len())..c.end.min(input.len())]
                .contains(|ch: char| delimiters.contains(ch))
        }
        None => false,
    }
}

impl Speller {
    pub fn new(config: &serde_json::Value) -> Self {
        let alphabet = config
            .pointer("/speller/alphabet")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_ALPHABET)
            .to_string();
        let initials = config
            .pointer("/speller/initials")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map_or_else(|| alphabet.clone(), str::to_string);
        let auto_clear = match config
            .pointer("/speller/auto_clear")
            .and_then(|v| v.as_str())
            .unwrap_or("")
        {
            "auto" => AutoClear::Auto,
            "manual" => AutoClear::Manual,
            "max_length" => AutoClear::MaxLength,
            _ => AutoClear::None,
        };
        let auto_select_pattern = config
            .pointer("/speller/auto_select_pattern")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            // 加 `^(?:…)$` 锚点：把「整串匹配」化为「find 命中」（fancy_regex 无 is_full_match）
            .map(|s| format!("^(?:{s})$"))
            .and_then(|s| fancy_regex::Regex::new(&s).ok());
        Self {
            initials,
            alphabet,
            delimiters: config
                .pointer("/speller/delimiter")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            finals: config
                .pointer("/speller/finals")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            max_code_length: config
                .pointer("/speller/max_code_length")
                .and_then(|v| v.as_i64())
                .map_or(0, |n| n as i32),
            auto_select: config
                .pointer("/speller/auto_select")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            use_space: config
                .pointer("/speller/use_space")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            auto_clear,
            auto_select_pattern,
        }
    }

    fn expecting_an_initial(&self, ctx: &Context) -> bool {
        let caret_pos = ctx.caret_pos;
        if caret_pos == 0
            || caret_pos == ctx.composition.segmentation.current_start()
        {
            return true;
        }
        let previous = ctx.input.as_bytes()[caret_pos - 1] as char;
        belongs_to(previous, &self.finals) || !belongs_to(previous, &self.alphabet)
    }

    fn auto_select_at_max_code_length(&self, ctx: &mut Context) -> bool {
        if self.max_code_length <= 0 || !ctx.has_menu() {
            return false;
        }
        let cand = ctx.get_selected_candidate().cloned();
        if reached_max_code_length(cand.as_ref(), self.max_code_length)
            && is_auto_selectable(cand.as_ref(), &ctx.input, &self.delimiters)
        {
            ctx.confirm_current_selection();
            return true;
        }
        false
    }

    fn auto_select_unique_candidate(&self, ctx: &mut Context) -> bool {
        if !self.auto_select || !ctx.has_menu() {
            return false;
        }
        let Some(seg) = ctx.composition.back() else {
            return false;
        };
        let count = seg
            .menu
            .as_ref()
            .map_or(0, |m| m.candidate_count());
        if count != 1 {
            return false;
        }
        let cand = seg.get_selected_candidate().cloned();
        let matches_input_pattern = if self.auto_select_pattern.is_none() {
            self.max_code_length == 0
                || reached_max_code_length(cand.as_ref(), self.max_code_length)
        } else {
            match &cand {
                Some(c) => {
                    let code = &ctx.input[c.start.min(ctx.input.len())..c.end.min(ctx.input.len())];
                    self.auto_select_pattern
                        .as_ref()
                        .map_or(false, |re| regex_full_match(re, code))
                }
                None => false,
            }
        };
        if matches_input_pattern
            && is_auto_selectable(cand.as_ref(), &ctx.input, &self.delimiters)
        {
            ctx.confirm_current_selection();
            return true;
        }
        false
    }

    fn auto_clear(&self, ctx: &mut Context) -> bool {
        if !ctx.has_menu()
            && self.auto_clear != AutoClear::None
            && (self.auto_clear != AutoClear::MaxLength
                || self.max_code_length == 0
                || ctx.input.len() as i32 >= self.max_code_length)
        {
            ctx.clear();
            return true;
        }
        false
    }
}

impl Processor for Speller {
    fn process_key_event(&mut self, engine: &mut Engine, key: &KeyEvent) -> ProcessResult {
        if key.release() || key.ctrl() || key.alt()
            || key.modifier & crate::engine::key_event::K_SUPER_MASK != 0
        {
            return ProcessResult::Noop;
        }
        let ch = match key.printable_char() {
            Some(ch) if (ch as u32) >= 0x20 && (ch as u32) < 0x7f => ch,
            _ => return ProcessResult::Noop,
        };
        if ch as u32 == XK_SPACE && (!self.use_space || key.shift()) {
            return ProcessResult::Noop;
        }
        if !belongs_to(ch, &self.alphabet) && !belongs_to(ch, &self.delimiters) {
            return ProcessResult::Noop;
        }
        let ctx = &mut engine.context;
        let is_initial = belongs_to(ch, &self.initials);
        if !is_initial && self.expecting_an_initial(ctx) {
            return ProcessResult::Noop;
        }
        // 超长自动选定 / 自动清空（改输入前）
        if is_initial && self.auto_select_at_max_code_length(ctx) {
            // auto-select at max code length
        } else if (self.auto_clear == AutoClear::MaxLength
            || self.auto_clear == AutoClear::Manual)
            && self.auto_clear(ctx)
        {
            // auto-clear at max code when no candidate
        }
        // 备份当前段（auto_select 回退用）
        let previous_segment = if self.auto_select && ctx.has_menu() {
            ctx.composition.back().cloned()
        } else {
            None
        };
        ctx.push_input_ch(ch);
        ctx.begin_editing();
        // auto_select：输入不再匹配时自动选定上一段
        if self.auto_select
            && self.max_code_length == 0
            && self.auto_select_pattern.is_none()
        {
            let has_menu = ctx.has_menu();
            if !has_menu {
                if let Some(prev) = previous_segment.filter(|p| p.menu.is_some()) {
                    let end = prev.end;
                    let converted = ctx.input[..end.min(ctx.input.len())].to_string();
                    if is_auto_selectable(
                        prev.get_selected_candidate(),
                        &converted,
                        &self.delimiters,
                    ) {
                        // 复用上一段匹配
                        if let Some(back) = ctx.composition.back_mut() {
                            *back = prev;
                        }
                        ctx.confirm_current_selection();
                        if ctx.get_option("_auto_commit") {
                            let input = ctx.input.clone();
                            ctx.set_input(&converted);
                            ctx.commit();
                            let rest = input[end.min(input.len())..].to_string();
                            ctx.set_input(&rest);
                        }
                        // compose 之后不再用 ctx（改用 engine.context），
                        // 让 `ctx = &mut engine.context` 的借用在此结束，
                        // 否则 `engine.compose()` 会撞上二次可变借用。
                        engine.compose();
                        if !is_initial
                            && engine
                                .context
                                .composition
                                .segmentation
                                .current_segment_length()
                                == 1
                        {
                            engine.context.pop_input(1);
                            return ProcessResult::Noop;
                        }
                        return ProcessResult::Accepted;
                    }
                }
            }
        }
        if self.auto_select_unique_candidate(ctx) {
            // auto-select unique candidate；选择已置 Selected，提交交给 on_select 流程
        } else if self.auto_clear == AutoClear::Auto && self.auto_clear(ctx) {
            // auto-clear when no candidate
        }
        ProcessResult::Accepted
    }
}
