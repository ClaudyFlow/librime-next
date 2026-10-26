//! `src/rime/gear/streaming_chord_processor.{h,cc}`：流式并击。
//!
//! # 与经典版的分工
//!
//! 经典版（[`crate::engine::gear::chord_composer`]）把一串并击键**攒成一个和弦**再统一
//! 结算，输入串在结算前始终是空的。流式版换个思路：**一边敲一边往输入串里写**，
//! 靠"时间"和"手系方向"两个边界判断音节在哪断开。
//!
//! 三个判定维度来自方案配置：
//!
//! * `initial_keys` / `final_keys` —— 声母键集、韵母键集；
//! * `chord_duration_ms`（默认 60）—— 同一个和弦内两手落键的生理容差；
//! * `chord_timeout_ms`（默认 120）—— 连打时停顿多久算换音节。
//!
//! 边界规则（`HandleChordKey`）：前一个和弦还开着、且
//! **`Δt > chord_timeout_ms`** 或 **「前键是韵母、这一键是声母、且 Δt > chord_duration_ms」**
//! ⇒ 认定前一音节结束，插一个可见隔音符（`'`）帮 Translator 分詞。
//!
//! # 双功能键（空格 / 分号）
//!
//! 空格这类键在两个角色间摆动：**单独敲**是「上屏 / 选词」，**和别人一起按**是「和弦码」。
//! 判据只能是时间：落下时先**挂起**（`pending_solo_key_`）并显示 `␣` 提示，
//! 之后
//!
//! * `chord_duration_ms` 内有别的并击键落下 ⇒ 解冻成和弦的一部分；
//! * 同键再敲一次 ⇒ 确证"单击"，回放原始键；
//! * 超时 / 来了个非并击键 ⇒ 回放原始键；
//! * 抬键 ⇒ 立刻回放（有 KeyUp 的平台走这条，手感最好）。
//!
//! # 为什么需要"回放抑制"
//!
//! C++ 用 `is_replaying_` 在回放期间让自己 `return kNoop`，否则会把自己吐出去的键
//! 再吃回来（无限循环）。LRN 的合成键是**排队重放**（见
//! [`crate::engine::runtime::Engine::enqueue_synthetic_key`]），回放不在同一个调用栈里，
//! 一个布尔量撑不住 —— 改成记下"我发出去的是哪个键"，等它绕回来时认领并放行。
//! 认领同时挂在 `process_key_event` 与 `on_unhandled_key` 上，覆盖"被别人吃掉的"那一支。

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use serde_json::Value;

use crate::algo::algebra::Projection;
use crate::engine::gear::processor::{ProcessResult, Processor};
use crate::engine::key_event::{
    KeyEvent, XK_BACKSPACE, XK_ESCAPE, XK_RETURN, XK_SPACE,
};
use crate::engine::runtime::Engine;
use crate::engine::segmentation::Segment;

/// 挂起提示的标签（与经典版共用同一个字符串，宿主按它渲染）。
pub const CHORD_PROMPT_TAG: &str = "chord_prompt";
/// 为了承载提示而垫的虚拟段。
pub const PHONY_TAG: &str = "phony";
/// 空格的专用提示符（`streaming_chord_processor.cc:257`）。
const SPACE_SYMBOL: &str = "\u{2423}";

/// C++ `ChordKeyEvent`：一次并击按键 + 到达时刻。
#[derive(Clone, Copy, Debug)]
pub struct ChordKeyEvent {
    pub keycode: u32,
    /// 映射后的并击码（宫保代码）；没配 `key_map` 时就是 keycode 本身。
    pub key: char,
    pub time: Instant,
}

impl Default for ChordKeyEvent {
    fn default() -> Self {
        Self {
            keycode: 0,
            key: '\0',
            time: Instant::now(),
        }
    }
}

impl ChordKeyEvent {
    /// C++ `operator bool()`：`keycode != 0` 即"有效"。
    pub fn is_valid(&self) -> bool {
        self.keycode != 0
    }
}

/// 流式并击处理器。
pub struct StreamingChordProcessor {
    /// 物理键码 → 并击码。
    key_map: BTreeMap<u32, char>,
    /// 双功能键（单击回放原始键，并击充当和弦码）。
    dual_role_keys: BTreeSet<u32>,
    initial_keys: Vec<char>,
    final_keys: Vec<char>,
    chord_timeout_ms: i64,
    chord_duration_ms: i64,
    delimiter: String,
    canonicalizer: Option<Projection>,
    /// 动作和弦后缀，**按长度降序**（长后缀优先命中）。
    action_suffixes: Vec<(String, KeyEvent)>,

    last_key_event: ChordKeyEvent,
    pending_solo_key: ChordKeyEvent,

    pressed_chord_keys: BTreeSet<u32>,
    current_chord_start: usize,
    is_chord_open: bool,

    /// 我发出去、还在等它绕回来的键（见模块文档「回放抑制」）。
    replay_pending: Vec<KeyEvent>,
}

impl StreamingChordProcessor {
    /// C++ 构造函数：读 `streaming_chord/*` 那一节。
    pub fn new(config: &Value) -> Self {
        let pointer = |key: &str| config.pointer(&format!("/streaming_chord/{key}"));

        // 1. 并击分区定义与时序阈值
        let initials = pointer("initial_keys")
            .and_then(Value::as_str)
            .unwrap_or("SCZHLFGDBKTP");
        let finals = pointer("final_keys")
            .and_then(Value::as_str)
            .unwrap_or("IUVANREO");
        let chord_timeout_ms = pointer("chord_timeout_ms")
            .and_then(Value::as_i64)
            .unwrap_or(120);
        let chord_duration_ms = pointer("chord_duration_ms")
            .and_then(Value::as_i64)
            .unwrap_or(60);

        // 隔音符：先看 `streaming_chord/delimiter`（允许手工设成 `'` 或空串），
        // 没有就向 `speller/delimiter` 要**第一个非空格字符**
        let delimiter = match pointer("delimiter").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => config
                .pointer("/speller/delimiter")
                .and_then(Value::as_str)
                .and_then(|s| s.chars().find(|c| *c != ' '))
                .map(|c| c.to_string())
                .unwrap_or_default(),
        };

        // 2. 键位映射（物理键码 → 宫保字母）
        let mut key_map = BTreeMap::new();
        if let Some(map) = pointer("key_map").and_then(Value::as_object) {
            for (key_str, target) in map {
                let Some(target_str) = target.as_str() else {
                    continue;
                };
                if key_str.is_empty() || target_str.is_empty() {
                    continue;
                }
                if let Some(ke) = KeyEvent::parse(key_str) {
                    if ke.keycode != 0 {
                        if let Some(ch) = target_str.chars().next() {
                            key_map.insert(ke.keycode, ch);
                        }
                    }
                }
            }
        }

        // 3. 双功能键：显式声明优先，没声明就自动把"非字母的并击键"登记进来
        let mut dual_role_keys = BTreeSet::new();
        match pointer("dual_role_keys").and_then(Value::as_array) {
            Some(list) => {
                for item in list {
                    let Some(text) = item.as_str() else {
                        continue;
                    };
                    if let Some(ke) = KeyEvent::parse(text) {
                        if ke.keycode != 0 {
                            dual_role_keys.insert(ke.keycode);
                        }
                    }
                }
            }
            None => {
                for &keycode in key_map.keys() {
                    if (0x20..=0x7e).contains(&keycode)
                        && !(keycode as u8 as char).is_ascii_alphabetic()
                    {
                        dual_role_keys.insert(keycode);
                    }
                }
            }
        }

        // 4. 和弦标准化运算
        let canonicalizer = Projection::from_config_path(config, "streaming_chord/canonicalize").ok();

        // 5. 动作和弦后缀
        let mut action_suffixes: Vec<(String, KeyEvent)> = Vec::new();
        if let Some(map) = pointer("actions").and_then(Value::as_object) {
            for (suffix, target) in map {
                let Some(target_str) = target.as_str() else {
                    continue;
                };
                if suffix.is_empty() {
                    continue;
                }
                if let Some(key) = KeyEvent::parse(target_str) {
                    action_suffixes.push((suffix.clone(), key));
                }
            }
            // 长后缀优先（`u1s1` 要先于 `s1`）。等长的后缀不可能同时命中同一条尾巴，
            // 所以它们之间谁先无所谓 —— 稳定排序即可。
            action_suffixes.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        }

        StreamingChordProcessor {
            key_map,
            dual_role_keys,
            initial_keys: initials.chars().collect(),
            final_keys: finals.chars().collect(),
            chord_timeout_ms,
            chord_duration_ms,
            delimiter,
            canonicalizer,
            action_suffixes,
            last_key_event: ChordKeyEvent::default(),
            pending_solo_key: ChordKeyEvent::default(),
            pressed_chord_keys: BTreeSet::new(),
            current_chord_start: 0,
            is_chord_open: false,
            replay_pending: Vec::new(),
        }
    }

    /// 当前键位映射（测试 / 诊断用）。
    pub fn key_map(&self) -> &BTreeMap<u32, char> {
        &self.key_map
    }

    /// 当前双功能键名单（测试用）。
    pub fn dual_role_keys(&self) -> &BTreeSet<u32> {
        &self.dual_role_keys
    }

    pub fn chord_timeout_ms(&self) -> i64 {
        self.chord_timeout_ms
    }

    pub fn chord_duration_ms(&self) -> i64 {
        self.chord_duration_ms
    }

    fn is_initial(&self, key: char) -> bool {
        self.initial_keys.contains(&key)
    }

    fn is_final(&self, key: char) -> bool {
        self.final_keys.contains(&key)
    }

    fn is_mapped_key(&self, keycode: u32) -> bool {
        self.key_map.contains_key(&keycode)
    }

    /// 必须**既在映射表里、又登记在双功能名单里**，二者缺一不可。
    fn is_dual_role_key(&self, keycode: u32) -> bool {
        self.is_mapped_key(keycode) && self.dual_role_keys.contains(&keycode)
    }

    /// 物理键码 → 并击码（没配映射就原样当字符用）。
    fn convert_to_chord_key(&self, keycode: u32) -> char {
        match self.key_map.get(&keycode) {
            Some(ch) => *ch,
            None => char::from_u32(keycode).unwrap_or('\u{fffd}'),
        }
    }

    /// C++ `ResetTracking`：时序追踪全部归零（不动输入串）。
    fn reset_tracking(&mut self, engine: &mut Engine) {
        self.clear_pending_prompt(engine);
        self.last_key_event = ChordKeyEvent::default();
        self.pending_solo_key = ChordKeyEvent::default();
        self.pressed_chord_keys.clear();
        self.current_chord_start = 0;
        self.is_chord_open = false;
    }

    /// C++ `FlushChordKey`：把并击码写进输入串。
    fn flush_chord_key(&mut self, engine: &mut Engine, ev: ChordKeyEvent) {
        // 当前和弦还没开（上一个已闭合，或是全新输入）⇒ 锚定新起点
        if !self.is_chord_open {
            self.current_chord_start = engine.context.input.len();
            self.is_chord_open = true;
        }
        let s = ev.key.to_string();
        engine.context.push_input_str(&s);
        self.last_key_event = ev;
        self.pressed_chord_keys.insert(ev.keycode);
    }

    /// C++ `HandleChordKey`：时序边界 + 写入。
    fn handle_chord_key(&mut self, engine: &mut Engine, ev: ChordKeyEvent) -> ProcessResult {
        let is_initial = self.is_initial(ev.key);
        let is_final = self.is_final(ev.key);
        // 既不是声母也不是韵母、又不在映射表里 ⇒ 不是并击键，交给后面的组件
        if !is_initial && !is_final && !self.is_mapped_key(ev.keycode) {
            return ProcessResult::Noop;
        }
        // 只有"前一个和弦还敞着"时，时差才代表音节切分
        if self.is_chord_open && self.last_key_event.is_valid() {
            let delta_t = ev
                .time
                .saturating_duration_since(self.last_key_event.time)
                .as_millis() as i64;
            // 条件一 超时边界：连打思考停顿 / 同手同区连打超时切分
            let is_timeout_boundary = delta_t > self.chord_timeout_ms;
            // 条件二 手系逆转边界：右手韵母 → 左手声母，且超出微时差容差
            let is_phase_inversion = self.is_final(self.last_key_event.key)
                && is_initial
                && delta_t > self.chord_duration_ms;
            if is_timeout_boundary || is_phase_inversion {
                // 给无抬键平台（Emacs 那类只送 keydown 的）注入可见隔音符，
                // 帮 Translator 分詞。防御：末尾已经是分隔符就别再追加，免得出现 `''`
                if !self.delimiter.is_empty()
                    && !engine.context.input.ends_with(&self.delimiter)
                {
                    let d = self.delimiter.clone();
                    engine.context.push_input_str(&d);
                }
                // 宣告前一和弦在流式时序上已被强制截断
                self.pressed_chord_keys.clear();
                self.is_chord_open = false;
                self.current_chord_start = 0;
            }
        }
        self.flush_chord_key(engine, ev);
        ProcessResult::Accepted
    }

    /// C++ `ReplayPendingKey`：把挂起的双功能键当**原始按键**重发一次。
    fn replay_pending_key(&mut self, engine: &mut Engine) {
        if !self.pending_solo_key.is_valid() {
            return;
        }
        let keycode = self.pending_solo_key.keycode;
        self.pending_solo_key = ChordKeyEvent::default();
        self.clear_pending_prompt(engine);

        let raw_key = KeyEvent::new(keycode, 0);
        // 组词态交给 selector 选词 / punctuator 上屏标点；空闲态若下游不收、
        // 而这又是个可打印字符，就直接上屏原生字符（→ `commit_if_unhandled`）。
        if engine.context.is_composing() && !engine.context.input.is_empty() {
            engine.enqueue_synthetic_key(raw_key);
        } else {
            engine.enqueue_synthetic_key_with_fallback(raw_key);
        }
        self.mark_replay(raw_key);
    }

    /// C++ `GetPendingPrompt`。
    fn pending_prompt(keycode: u32) -> String {
        if keycode == XK_SPACE {
            return SPACE_SYMBOL.to_string();
        }
        // 可打印 ASCII 标点包上方括号，明确提示"此键待判定"
        if (0x20..=0x7e).contains(&keycode) {
            return format!("[{}]", keycode as u8 as char);
        }
        "\u{00b7}".to_string()
    }

    /// C++ `DisplayPendingPrompt`。
    fn display_pending_prompt(&mut self, engine: &mut Engine, keycode: u32) {
        let prompt = Self::pending_prompt(keycode);
        let ctx = &mut engine.context;
        if ctx.composition.empty() {
            // 空闲态：垫一个长 0 的虚拟段承载提示符
            let mut placeholder = Segment::new(0, ctx.input.len());
            placeholder.tags.push(PHONY_TAG.to_string());
            ctx.composition.segmentation.add_segment(placeholder);
        }
        if let Some(last) = ctx.composition.back_mut() {
            if !last.has_tag(CHORD_PROMPT_TAG) {
                last.tags.push(CHORD_PROMPT_TAG.to_string());
            }
            last.prompt = prompt;
        }
    }

    /// C++ `ClearPendingPrompt`。
    fn clear_pending_prompt(&mut self, engine: &mut Engine) {
        let ctx = &mut engine.context;
        if ctx.composition.empty() {
            return;
        }
        let is_phony = ctx
            .composition
            .back()
            .map_or(false, |s| s.has_tag(PHONY_TAG));
        if is_phony {
            // 撤掉当初为承载提示而临时垫的虚拟段
            ctx.composition.segmentation.pop_back();
            // 可能还留着没翻译的并击码
            if ctx.input.is_empty() {
                ctx.clear();
            }
        } else if let Some(last) = ctx.composition.back_mut() {
            if last.has_tag(CHORD_PROMPT_TAG) {
                last.prompt.clear();
                last.tags.retain(|t| t != CHORD_PROMPT_TAG);
            }
        }
    }

    /// C++ `CanonicalizeCurrentChord`：和弦闭合时把"生码"换成"标准化码"。
    fn canonicalize_current_chord(&mut self, engine: &mut Engine) {
        let start = self.current_chord_start;
        if start >= engine.context.input.len() {
            return;
        }
        let mut chord = engine.context.input[start..].to_string();
        if chord.is_empty() {
            return;
        }
        if self.canonicalizer.is_none() {
            return;
        }
        // 将生和弦替换为标准闭包（如 `[ZFURO]` 或拼音）
        let len = chord.len();
        engine.context.pop_input(len);
        if let Some(canonicalizer) = self.canonicalizer.as_mut() {
            canonicalizer.apply_str(&mut chord);
        }
        // 剥掉动作后缀（若有），此时 `chord` 已还原成纯编码
        let action = self.pop_action(&mut chord);
        engine.context.push_input_str(&chord);
        // 发出下游动作（如空格确认上屏）
        if action.keycode != 0 {
            engine.enqueue_synthetic_key(action);
            self.mark_replay(action);
        }
    }

    /// C++ `PopAction`：匹配并剔除和弦尾部的动作后缀。
    fn pop_action(&mut self, chord: &mut String) -> KeyEvent {
        if chord.is_empty() || self.action_suffixes.is_empty() {
            return KeyEvent::default();
        }
        for (suffix, key) in &self.action_suffixes {
            if chord.len() >= suffix.len() && chord.ends_with(suffix.as_str()) {
                let new_len = chord.len() - suffix.len();
                chord.truncate(new_len);
                return *key;
            }
        }
        KeyEvent::default()
    }

    /// 记下"这个键是我发出去的"，等它绕回来时放行（见模块文档）。
    fn mark_replay(&mut self, key: KeyEvent) {
        self.replay_pending.push(key);
    }

    /// 认领回放标记：命中即移除并返回 true（调用方应立刻 `Noop` 放行）。
    fn take_replay_marker(&mut self, key: &KeyEvent) -> bool {
        match self.replay_pending.iter().position(|k| k == key) {
            Some(index) => {
                self.replay_pending.swap_remove(index);
                true
            }
            None => false,
        }
    }
}

impl Processor for StreamingChordProcessor {
    fn process_key_event(&mut self, engine: &mut Engine, key_event: &KeyEvent) -> ProcessResult {
        // 自己吐出去的键绕回来了 —— 放行，别二次加工
        if self.take_replay_marker(key_event) {
            return ProcessResult::Noop;
        }

        let keycode = key_event.keycode;

        // 1. 抬键处理（有 KeyUp 事件的 GUI 平台）
        if key_event.release() {
            // 孤立双功能键抬起（单击空格出空格/选词），手指一抬即时结算
            if self.pending_solo_key.is_valid() && keycode == self.pending_solo_key.keycode {
                self.replay_pending_key(engine);
                self.reset_tracking(engine);
                return ProcessResult::Accepted;
            }
            if self.pressed_chord_keys.remove(&keycode) {
                // 所有按下的并击键尽数释放 ⇒ 和弦闭合
                if self.pressed_chord_keys.is_empty() {
                    if engine.context.is_composing() {
                        self.canonicalize_current_chord(engine);
                    }
                    self.is_chord_open = false;
                    self.current_chord_start = 0;
                }
                return ProcessResult::Accepted;
            }
            return ProcessResult::Noop;
        }

        // 忽略功能组合修饰键
        if key_event.ctrl() || key_event.alt() || key_event.super_key() {
            return ProcessResult::Noop;
        }

        // 编辑与确认键到达：此前若有挂起键，直接取消
        if keycode == XK_BACKSPACE || keycode == XK_RETURN || keycode == XK_ESCAPE {
            if self.pending_solo_key.is_valid() {
                // 退格且只是"待定提示"状态：吞掉这次退格仅撤销待定键，体验最平滑
                if keycode == XK_BACKSPACE {
                    self.reset_tracking(engine);
                    return ProcessResult::Accepted;
                }
            }
            self.reset_tracking(engine);
            return ProcessResult::Noop;
        }

        let chord_key = self.convert_to_chord_key(keycode);
        let now = Instant::now();

        // 2. 双功能键（空格、分号等）
        if self.is_dual_role_key(keycode) {
            let solo_key = ChordKeyEvent {
                keycode,
                key: chord_key,
                time: now,
            };
            // 2.1 前次敲的某个双功能键还在挂起
            if self.pending_solo_key.is_valid() {
                // 分支 A：同键连击吞噬（无论间隔多久，同键再次落下即确证为单击）
                if self.pending_solo_key.keycode == keycode {
                    self.replay_pending_key(engine);
                    self.reset_tracking(engine);
                    return ProcessResult::Accepted;
                }
                let delta_t = now
                    .saturating_duration_since(self.pending_solo_key.time)
                    .as_millis() as i64;
                // 分支 B：异键微时差并击（空格 + 分号，Δt <= 容差）
                if delta_t <= self.chord_duration_ms {
                    let saved = self.pending_solo_key;
                    self.pending_solo_key = ChordKeyEvent::default();
                    self.clear_pending_prompt(engine);
                    self.handle_chord_key(engine, saved);
                    return self.handle_chord_key(engine, solo_key);
                }
                // 分支 C：异键超时 ⇒ 前键确证为单击，本键另行挂起
                self.replay_pending_key(engine);
                self.reset_tracking(engine);
                self.pending_solo_key = solo_key;
                self.display_pending_prompt(engine, keycode);
                return ProcessResult::Accepted;
            }
            // 2.2 检验是否与前序按键构成同和弦并击
            if self.last_key_event.is_valid() {
                let delta_t = now
                    .saturating_duration_since(self.last_key_event.time)
                    .as_millis() as i64;
                if delta_t <= self.chord_duration_ms {
                    return self.handle_chord_key(engine, solo_key);
                }
            }
            // 2.3 孤立双功能键落键：挂起等后续键裁决（支持抬键即上屏）
            self.pending_solo_key = solo_key;
            self.display_pending_prompt(engine, keycode);
            return ProcessResult::Accepted;
        }

        // 3. 结算孤立挂起的双功能键（常规字母/符号键到达）
        if self.pending_solo_key.is_valid() {
            let delta_t = now
                .saturating_duration_since(self.pending_solo_key.time)
                .as_millis() as i64;
            let is_chord_key = self.is_initial(chord_key) || self.is_final(chord_key);
            if is_chord_key && delta_t <= self.chord_duration_ms {
                // 双功能键超前落键并击（空格遇后续并击键解冻为和弦的一部分）
                let saved = self.pending_solo_key;
                self.pending_solo_key = ChordKeyEvent::default();
                self.clear_pending_prompt(engine);
                self.handle_chord_key(engine, saved);
            } else {
                // 超时或按下非并击键 ⇒ 确证为独立单击，重发结算
                self.replay_pending_key(engine);
            }
        }

        if !engine.context.is_composing() || engine.context.input.is_empty() {
            self.reset_tracking(engine);
        }

        // 4. 常规并击按键
        self.handle_chord_key(
            engine,
            ChordKeyEvent {
                keycode,
                key: chord_key,
                time: now,
            },
        )
    }

    fn on_attach(&mut self, engine: &mut Engine) {
        // `streaming_chord_processor.cc:132`：声明方案使用并击特性（启用抬键转发通道）
        engine.context.set_option("_chord_typing", true);
    }

    fn on_unhandled_key(&mut self, _engine: &mut Engine, key: &KeyEvent) {
        // 没人吃下的可能正是我刚发出去的回放键 —— 顺手把标记消掉，别留给后面的键。
        self.take_replay_marker(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn processor(config: Value) -> StreamingChordProcessor {
        StreamingChordProcessor::new(&config)
    }

    #[test]
    fn defaults_match_the_cpp_constructor() {
        let p = processor(json!({}));
        assert_eq!(p.chord_timeout_ms(), 120);
        assert_eq!(p.chord_duration_ms(), 60);
        assert!(p.key_map().is_empty());
        assert!(p.dual_role_keys().is_empty());
        assert_eq!(p.delimiter, "", "两处都没配就是空串");
        // 出厂声母/韵母表（`streaming_chord_processor.cc:27-28`）
        assert_eq!(
            p.initial_keys.iter().collect::<String>(),
            "SCZHLFGDBKTP"
        );
        assert_eq!(p.final_keys.iter().collect::<String>(), "IUVANREO");
    }

    #[test]
    fn config_overrides_initial_final_and_thresholds() {
        let p = processor(json!({"streaming_chord": {
            "initial_keys": "ABC",
            "final_keys": "xy",
            "chord_timeout_ms": 200,
            "chord_duration_ms": 80
        }}));
        assert_eq!(p.initial_keys.iter().collect::<String>(), "ABC");
        assert_eq!(p.final_keys.iter().collect::<String>(), "xy");
        assert_eq!(p.chord_timeout_ms(), 200);
        assert_eq!(p.chord_duration_ms(), 80);
        assert!(p.is_initial('B'));
        assert!(!p.is_initial('b'), "区分大小写");
        assert!(p.is_final('x'));
    }

    #[test]
    fn delimiter_prefers_its_own_key_then_falls_back_to_speller() {
        // `streaming_chord/delimiter` 没写 ⇒ 取 `speller/delimiter` 的第一个非空格字符
        let p = processor(json!({"speller": {"delimiter": " '"}}));
        assert_eq!(p.delimiter, "'");
        // 写了就用它（空串也照收，表示"不插隔音符"）
        let p = processor(json!({
            "streaming_chord": {"delimiter": ""},
            "speller": {"delimiter": " '"}
        }));
        assert_eq!(p.delimiter, "");
        let p = processor(json!({"streaming_chord": {"delimiter": "-"}}));
        assert_eq!(p.delimiter, "-");
    }

    #[test]
    fn key_map_reads_repr_keys_and_takes_the_first_char() {
        // 键名走 `KeyEvent::Parse` 的**单键**入口（不是 `KeySequence::Parse`），
        // 所以只认 `semicolon` 这种裸键名，写成 `{semicolon}` 反而解析不出来。
        let p = processor(json!({"streaming_chord": {
            "key_map": {
                "q": "Z",
                "w": "F",
                "semicolon": "U"
            }
        }}));
        assert_eq!(p.key_map().get(&(b'q' as u32)), Some(&'Z'));
        assert_eq!(p.key_map().get(&(b'w' as u32)), Some(&'F'));
        assert_eq!(p.key_map().get(&0x003b), Some(&'U'));
        assert_eq!(p.key_map().len(), 3);
        // 目标取第一个字符：`"Zabc"` 只保留 'Z'
        let p = processor(json!({"streaming_chord": {"key_map": {"q": "Zabc"}}}));
        assert_eq!(p.key_map().get(&(b'q' as u32)), Some(&'Z'));
    }

    #[test]
    fn dual_role_keys_are_derived_from_non_alpha_mapped_keys() {
        // 没显式声明 ⇒ 映射表里"非字母"的键自动成为双功能键（空格 / 分号）
        let p = processor(json!({"streaming_chord": {
            "key_map": {"a": "Z", "space": " ", "semicolon": ";"}
        }}));
        let derived: Vec<u32> = p.dual_role_keys().iter().copied().collect();
        assert_eq!(derived, vec![XK_SPACE, 0x003b]);
        assert!(p.is_dual_role_key(XK_SPACE));
        assert!(!p.is_dual_role_key(b'a' as u32), "字母不是双功能键");
    }

    #[test]
    fn explicit_dual_role_list_wins_over_derivation() {
        let p = processor(json!({"streaming_chord": {
            "key_map": {"a": "Z", "space": " ", "semicolon": ";"},
            "dual_role_keys": ["space"]
        }}));
        let listed: Vec<u32> = p.dual_role_keys().iter().copied().collect();
        assert_eq!(listed, vec![XK_SPACE]);
    }

    #[test]
    fn dual_role_requires_also_being_mapped() {
        // 只登记名字但映射表里没有 ⇒ 不算双功能键（两者缺一不可）
        let p = processor(json!({"streaming_chord": {
            "key_map": {"a": "Z"},
            "dual_role_keys": ["space"]
        }}));
        assert!(p.dual_role_keys().contains(&XK_SPACE));
        assert!(!p.is_dual_role_key(XK_SPACE));
    }

    #[test]
    fn pop_action_strips_the_longest_suffix() {
        let mut p = processor(json!({"streaming_chord": {
            "actions": {
                "s1": "space",
                "u1s1": "Return",
                "x": "Escape"
            }
        }}));
        // 长后缀优先：`Zu1s1` 先命中 `u1s1`（Return），而不是被更短的 `s1` 截走
        let mut chord = "Zu1s1".to_string();
        let action = p.pop_action(&mut chord);
        assert_eq!(chord, "Z");
        assert_eq!(action, KeyEvent::parse("Return").unwrap());
        // `Zs1` 命中 `s1`
        let mut chord = "Zs1".to_string();
        let action = p.pop_action(&mut chord);
        assert_eq!(chord, "Z");
        assert_eq!(action.keycode, XK_SPACE);
        // 没后缀 ⇒ 原样返回，动作为空
        let mut chord = "Zabc".to_string();
        let action = p.pop_action(&mut chord);
        assert_eq!(chord, "Zabc");
        assert_eq!(action.keycode, 0);
        // 后缀比整串还长 ⇒ 不匹配
        let mut chord = "s".to_string();
        p.pop_action(&mut chord);
        assert_eq!(chord, "s");
    }

    #[test]
    fn pending_prompt_uses_space_and_bracket_symbols() {
        assert_eq!(StreamingChordProcessor::pending_prompt(XK_SPACE), "\u{2423}");
        assert_eq!(StreamingChordProcessor::pending_prompt(0x003b), "[;]");
        assert_eq!(StreamingChordProcessor::pending_prompt(0x002c), "[,]");
        assert_eq!(StreamingChordProcessor::pending_prompt(XK_BACKSPACE), "\u{00b7}");
    }

    #[test]
    fn convert_to_chord_key_falls_back_to_the_keycode() {
        let p = processor(json!({"streaming_chord": {"key_map": {"q": "Z"}}}));
        assert_eq!(p.convert_to_chord_key(b'q' as u32), 'Z');
        assert_eq!(p.convert_to_chord_key(b'w' as u32), 'w');
    }

    #[test]
    fn replay_markers_are_consumed_once() {
        let mut p = processor(json!({}));
        let key = KeyEvent::new(XK_SPACE, 0);
        p.mark_replay(key);
        assert!(p.take_replay_marker(&key), "第一次认领成功");
        assert!(!p.take_replay_marker(&key), "认领过就没了");
    }
}
