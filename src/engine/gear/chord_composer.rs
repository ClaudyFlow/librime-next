//! `src/rime/gear/chord_composer.{h,cc}`：并击输入（经典版）。
//!
//! # 它解决什么问题
//!
//! 并击方案（宫保拼音那类）把"一个音节"摊到**同时按下的几个键**上：左手声母、右手韵母，
//! 两三个键一起落下才是一个音节。于是引擎看到的不再是"按键 → 输入串"的流水线，而是：
//!
//! 1. 记下当前按下集合 `pressed_keys`，以及参与成 chord 的键 `recognized_chord`；
//! 2. 有变化就重算 `recognized_chord` → 经 `algebra` 变成编码串 → 当作 **prompt** 显示
//!    （此时输入串还是空的，靠一个 `phony` 段骗过 `IsComposing()`）；
//! 3. 抬键、且"该结束了"（`finish_chord_on_first_key_release` 或所有键都抬起）
//!    ⇒ `FinishChord`：编码串过 `output_format`，再**逐键转发**给引擎，让 speller 照常
//!    收进输入串。
//!
//! 换句话说，并击并没有另起一条输入通路 —— 它只是把"多键"重新压成"顺序按键"，
//! 后面的一切（切音节、查词、上屏）照旧。
//!
//! # 两个必须自己补的东西
//!
//! 1. **`raw_sequence`**：并击期间用户敲的**原始**字符（如 `ni`）不会被任何组件收进
//!    输入串（chord 键都被本组件 `Accepted` 了），所以本组件自己攒一份，供
//!    `commit_raw_input`（通常绑在 Return 上）原样上屏。
//! 2. **`ClearChord` 的 `phony` 收尾**：菜单提示是挂在最后一段上的。如果那段是当初为了
//!    "骗 `IsComposing()`"垫进去的 `phony` 段，就要整段清掉（`Context::Clear()`）；
//!    否则只抹掉 prompt 文字，让正常组字继续。
//!
//! # 工厂
//!
//! C++ `ChordComposerComponent::Create`（`chord_composer.cc:279-289`）是个**分派器**：
//! 方案里有 `streaming_chord` 段 ⇒ 造 [`StreamingChordProcessor`]，否则造本类。
//! 分支写在 [`crate::engine::gear::registry`] 的工厂里。
//!
//! [`StreamingChordProcessor`]: crate::engine::gear::streaming_chord_processor::StreamingChordProcessor

use std::collections::BTreeSet;

use serde_json::Value;

use crate::algo::algebra::Projection;
use crate::engine::gear::key_binding::{fallback, KeyBindingMap};
use crate::engine::gear::processor::{ProcessResult, Processor};
use crate::engine::key_event::{
    key_sequence_repr, KeyEvent, K_SHIFT_MASK, XK_BACKSPACE, XK_ESCAPE, XK_SPACE,
};
use crate::engine::runtime::Engine;
use crate::engine::segmentation::Segment;

/// 为了"骗 `IsComposing()`"而垫进去的虚拟段标签。
pub const PHONY_TAG: &str = "phony";
/// 承载并击提示的那一段的标签。
pub const CHORD_PROMPT_TAG: &str = "chord_prompt";
/// 本组件唯一的内置动作名（`chord_composer.cc:20-23` 的 `action_definitions`）。
pub const ACTION_COMMIT_RAW_INPUT: &str = "commit_raw_input";

/// C++ `Chord = set<int>`。用 `BTreeSet`：C++ 是字典序迭代，`Vec` 不保序。
pub type Chord = BTreeSet<u32>;

/// QWERTY 布局的"基态键位"表（`chord_composer.cc:91-97`）。
///
/// 以 `ch - 0x20` 索引，覆盖 `0x20..=0x7e` 共 95 个位置（表里 96 字节，最后一格不用）。
/// 作用：`Shift+a` 送来的 keycode 是 `'A'`，但并击方案认的是 `'a'` ——
/// 大写字母、上档符号都要先落回基态键位再比对 `alphabet`。
///
/// ```text
/// " 1'3457'908=,-./"   0x20-0x2f   ! → 1，$ → 4 ……
/// "0123456789;;,=./"   0x30-0x3f   : → ;，< → , ……
/// "2abcdefghijklmno"   0x40-0x4f   @ → 2，A → a ……
/// "pqrstuvwxyz[\]6-"   0x50-0x5f   P → p，^ → 6 ……
/// "`abcdefghijklmno"   0x60-0x6f
/// "pqrstuvwxyz[\]`"    0x70-0x7e
/// ```
const MAP_TO_BASE_LAYER: &[u8] = b" 1'3457'908=,-./\
0123456789;;,=./\
2abcdefghijklmnopqrstuvwxyz[\\]6-\
`abcdefghijklmnopqrstuvwxyz[\\]`";

/// C++ `get_base_layer_key_code`：带 Shift 的可打印键落回基态键位。
pub fn base_layer_keycode(key: &KeyEvent) -> u32 {
    let ch = key.keycode;
    if key.shift() && (0x20..=0x7e).contains(&ch) {
        u32::from(MAP_TO_BASE_LAYER[(ch - 0x20) as usize])
    } else {
        ch
    }
}

/// C++ `ChordingState`：按住集合 + 已识别和弦。
#[derive(Clone, Debug, Default)]
pub struct ChordingState {
    pub pressed_keys: Chord,
    pub recognized_chord: Chord,
}

impl ChordingState {
    pub fn is_pressed(&self, ch: u32) -> bool {
        self.pressed_keys.contains(&ch)
    }
    /// 返回"这次真的按下了"（重复 keydown 返回 false）。
    pub fn press_key(&mut self, ch: u32) -> bool {
        self.pressed_keys.insert(ch)
    }
    /// 返回"这次真的抬起了"。
    pub fn release_key(&mut self, ch: u32) -> bool {
        self.pressed_keys.remove(&ch)
    }
    pub fn add_key_to_chord(&mut self, ch: u32) -> bool {
        self.recognized_chord.insert(ch)
    }
    pub fn clear(&mut self) {
        self.pressed_keys.clear();
        self.recognized_chord.clear();
    }
}

/// 并击处理器（经典版）。
pub struct ChordComposer {
    /// `chord_composer/alphabet` 解析出来的键序列。**顺序决定编码串顺序**
    /// ——`SerializeChord` 按这张表的顺序取，不是按按下的顺序。
    chording_keys: Vec<KeyEvent>,
    /// 实际没用上（C++ 读了 `speller/delimiter` 存进 `delimiter_` 但经典版从不读它），
    /// 保留字段只为两版对齐，免得读源码的人以为漏了。
    #[allow(dead_code)]
    delimiter: String,
    algebra: Projection,
    output_format: Projection,
    prompt_format: Projection,
    use_control: bool,
    use_alt: bool,
    use_shift: bool,
    use_super: bool,
    use_caps: bool,
    finish_chord_on_first_key_release: bool,
    bindings: KeyBindingMap,

    state: ChordingState,
    editing_chord: bool,
    sending_chord: bool,
    composing: bool,
    /// 并击期间攒下的原始按键（给 `commit_raw_input` 用）。
    raw_sequence: String,
    /// 我转发出去、还在等它绕回来的键。
    ///
    /// C++ 用 `sending_chord_` 这个布尔量：`FinishChord` 里同步转发，回放期间本组件
    /// 走 `ProcessFunctionKey` 分支而不是再并击一次。Rust 的合成键是**排队重放**
    /// （见 [`crate::engine::runtime::Engine::enqueue_synthetic_key`]），不在同一个调用栈里，
    /// 布尔量早就复位了 —— 于是改成"记下我发的是哪个键，等它绕回来时认领"。
    replay_pending: Vec<KeyEvent>,
}

impl ChordComposer {
    /// C++ `ChordComposer::ChordComposer`：读 `chord_composer/*` 那一节。
    ///
    /// 注意命名空间是**字面量** `"chord_composer"`，不是 `ticket.name_space`
    /// （`chord_composer.cc:34`）—— 所以 `- chord_composer@foo` 这种别名写法
    /// 在 C++ 里读的仍然是 `chord_composer/bindings`。
    pub fn new(config: &Value) -> Self {
        let pointer = |key: &str| config.pointer(&format!("/chord_composer/{key}"));
        let flag = |key: &str| pointer(key).and_then(Value::as_bool).unwrap_or(false);
        let alphabet = pointer("alphabet")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let projection = |path: &str| {
            // 节点缺失**不算错**：C++ `GetList` 返回空指针，`Load` 直接 false，投影保持空
            Projection::from_config_path(config, path).unwrap_or_default()
        };
        let mut bindings = KeyBindingMap::new();
        bindings.load_config(config, "chord_composer");
        ChordComposer {
            chording_keys: KeyEvent::parse_sequence(alphabet),
            delimiter: config
                .pointer("/speller/delimiter")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            algebra: projection("chord_composer/algebra"),
            output_format: projection("chord_composer/output_format"),
            prompt_format: projection("chord_composer/prompt_format"),
            use_control: flag("use_control"),
            use_alt: flag("use_alt"),
            use_shift: flag("use_shift"),
            use_super: flag("use_super"),
            use_caps: flag("use_caps"),
            finish_chord_on_first_key_release: flag("finish_chord_on_first_key_release"),
            bindings,
            state: ChordingState::default(),
            editing_chord: false,
            sending_chord: false,
            composing: false,
            raw_sequence: String::new(),
            replay_pending: Vec::new(),
        }
    }

    /// 攒下的原始输入（测试用）。
    pub fn raw_sequence(&self) -> &str {
        &self.raw_sequence
    }

    pub fn state(&self) -> &ChordingState {
        &self.state
    }

    /// C++ `CommitRawInput`：把攒下的原始输入原样上屏。
    ///
    /// # 这里修了 C++ 的一个死胡同
    ///
    /// C++ 的三步是 `set_input(raw)` → `ClearNonConfirmedComposition()` → `Commit()`。
    /// 问题是 `set_input` 会发 `update_notifier` ⇒ 引擎立刻 `Compose` 重算分段
    /// （`context.cc:280-284` 的 `set_input` 只改 `input_`，重算靠这条通知），
    /// 于是刚建出来的分段**全是未确认的**，紧接着被 `ClearNonConfirmedComposition`
    /// 一条不剩地拆掉 —— `Commit()` 里 `if (!IsComposing()) return false;` 直接拒绝。
    /// 净结果：输入串被改成了 raw，但一个字符都没上屏，用户敲的东西凭空消失。
    ///
    /// 所以这里保留前三步（含那一步重算，C++ 是靠通知隐式做的），
    /// **只在 `Commit()` 拒绝时补一次直上屏**，把原始串交出去。
    fn commit_raw_input(&mut self, engine: &mut Engine) -> bool {
        if self.raw_sequence.is_empty() {
            return false;
        }
        let raw = std::mem::take(&mut self.raw_sequence);
        engine.context.set_input(&raw);
        // C++ 是 `set_input` 里发通知、引擎同步 Compose；Rust 的 `set_input` 只是
        // 改字段（`Context` 没有回调槽），所以这里显式补一次。
        engine.compose();
        let ctx = &mut engine.context;
        ctx.clear_non_confirmed_composition();
        if !ctx.commit() {
            // 组合串已被拆空（见函数文档），直接上屏原始串。
            // 走 `commit_raw_text` 而不是 `commit_direct`：它按 `raw` 记一条提交历史，
            // 与 `ConcreteEngine::CommitText` 一致 —— 打完数字再打标点那种判断要靠它。
            ctx.commit_raw_text(raw);
        }
        true
    }

    /// C++ `ProcessFunctionKey`：先看绑定表，再处理"清空原始输入"的那两个键。
    fn process_function_key(&mut self, engine: &mut Engine, key_event: &KeyEvent) -> ProcessResult {
        // C++ `KeyBindingProcessor::ProcessKeyEvent(key_event, ctx, 0)`：keymap 0，
        // **不带** Shift 兜底（`fallback_options` 默认 `None`），因为并按时的 Shift
        // 有意义，不能随手抹掉。
        let action = self.bindings.lookup(key_event, fallback::NONE).map(str::to_string);
        if let Some(action) = action {
            if action == ACTION_COMMIT_RAW_INPUT && self.commit_raw_input(engine) {
                return ProcessResult::Accepted;
            }
        }
        if !key_event.release() {
            let ch = key_event.keycode;
            if ch == XK_BACKSPACE || ch == XK_ESCAPE {
                // 退格/取消 ⇒ 那份原始输入作废
                self.raw_sequence.clear();
            }
        }
        ProcessResult::Noop
    }

    /// C++ `FinishChordConditionIsMet`。
    fn finish_chord_condition_is_met(&self) -> bool {
        self.finish_chord_on_first_key_release || self.state.pressed_keys.is_empty()
    }

    /// C++ `ProcessChordingKey`：并击键的按下/抬起状态机。
    fn process_chording_key(&mut self, engine: &mut Engine, key_event: &KeyEvent) -> ProcessResult {
        // 方案没开对应修饰键 ⇒ 带该修饰的组合键不是并击键，直接把状态清干净
        if (key_event.ctrl() && !self.use_control)
            || (key_event.alt() && !self.use_alt)
            || (key_event.shift() && !self.use_shift)
            || (key_event.super_key() && !self.use_super)
            || (key_event.caps() && !self.use_caps)
        {
            self.clear_chord(engine);
            self.state.clear();
            return ProcessResult::Noop;
        }
        let ch = base_layer_keycode(key_event);
        // 不在 `alphabet` 里 ⇒ 根本不是并击键
        if !self.chording_keys.iter().any(|k| k.keycode == ch) {
            self.clear_chord(engine);
            self.state.clear();
            return ProcessResult::Noop;
        }
        // 是并击键：**无论后面成不成功，这个键都不许再往下传**
        let mut result = ProcessResult::Accepted;
        self.editing_chord = true;
        if key_event.release() {
            let released = self.state.release_key(ch);
            if released && self.finish_chord_condition_is_met() && !self.state.recognized_chord.is_empty()
            {
                let chord = std::mem::take(&mut self.state.recognized_chord);
                self.finish_chord(engine, &chord);
                self.state.recognized_chord.clear();
            }
            // 抬起的不是按下过的键（重复 keyup）⇒ `Rejected`，交回 OS
            result = if released {
                ProcessResult::Accepted
            } else {
                ProcessResult::Rejected
            };
        } else if self.state.press_key(ch) && self.state.add_key_to_chord(ch) {
            // 重复 keydown 不重算（`PressKey` 返回 false）
            let chord = self.state.recognized_chord.clone();
            self.update_chord(engine, &chord);
        }
        self.editing_chord = false;
        result
    }

    /// C++ `SerializeChord`：和弦 → 编码串。
    ///
    /// 顺序取自 `chording_keys`（即 `alphabet` 的书写顺序），再过 `algebra`。
    fn serialize_chord(&mut self, chord: &Chord) -> String {
        let keys: Vec<KeyEvent> = self
            .chording_keys
            .iter()
            .filter(|k| chord.contains(&k.keycode))
            .copied()
            .collect();
        let mut code = key_sequence_repr(&keys);
        self.algebra.apply_str(&mut code);
        code
    }

    /// C++ `UpdateChord`：把和弦的**提示**挂到最后一段上。
    fn update_chord(&mut self, engine: &mut Engine, chord: &Chord) {
        // 空和弦、或者只按了空格 ⇒ 没什么可提示的
        if chord.is_empty() || (chord.len() == 1 && chord.contains(&XK_SPACE)) {
            self.clear_chord(engine);
            return;
        }
        let mut code = self.serialize_chord(chord);
        self.prompt_format.apply_str(&mut code);
        let ctx = &mut engine.context;
        if ctx.composition.empty() {
            // 垫一个 `phony` 段，两个用处：
            // 1. 骗过 `IsComposing()` —— 此时输入串是空的，宿主否则以为在空闲态；
            // 2. 给提示一个能挂的地方（`GetPrompt` 读末段）。
            let mut placeholder = Segment::new(0, ctx.input.len());
            placeholder.tags.push(PHONY_TAG.to_string());
            ctx.composition.segmentation.add_segment(placeholder);
        }
        if let Some(last) = ctx.composition.back_mut() {
            if !last.has_tag(CHORD_PROMPT_TAG) {
                last.tags.push(CHORD_PROMPT_TAG.to_string());
            }
            last.prompt = code;
        }
    }

    /// C++ `FinishChord`：和弦成码，逐键转发。
    fn finish_chord(&mut self, engine: &mut Engine, chord: &Chord) {
        let mut code = self.serialize_chord(chord);
        self.output_format.apply_str(&mut code);
        self.clear_chord(engine);

        let keys = KeyEvent::parse_sequence(&code);
        if keys.is_empty() {
            return;
        }
        // 转发期间若又触发本组件，`sending_chord_` 让它走"功能键"分支而不是再并击一次
        self.sending_chord = true;
        for key in keys {
            // C++ 在这里同步 `ProcessSyntheticKey`，返回 false 就 `CommitText`。
            // Rust 是排队重放，所以把"没人吃就上屏"这个意图一起登记进去，
            // 同时记下"这个键是我发的"，免得绕回来时把自己吐出的键又并击一次。
            engine.enqueue_synthetic_key_with_fallback(key);
            self.replay_pending.push(key);
        }
        self.sending_chord = false;
    }

    /// C++ `ClearChord`：撤掉提示。
    fn clear_chord(&mut self, engine: &mut Engine) {
        let ctx = &mut engine.context;
        if ctx.composition.empty() {
            return;
        }
        let only_phony = ctx.composition.segmentation.segments.len() == 1
            && ctx
                .composition
                .back()
                .map_or(false, |s| s.has_tag(PHONY_TAG));
        if only_phony {
            // 整段都是垫出来的 ⇒ 连输入一起清（否则会留下 IsComposing() == true 的空转状态）
            ctx.clear();
        } else if let Some(last) = ctx.composition.back_mut() {
            if last.has_tag(CHORD_PROMPT_TAG) {
                last.prompt.clear();
                last.tags.retain(|t| t != CHORD_PROMPT_TAG);
            }
        }
    }

    /// C++ `OnContextUpdate`：只关心「刚才在组字、现在不组了」这一刻。
    ///
    /// 组合态若不是本组件自己结束的（用户 Escape、切了西文、别处上屏），
    /// 那份原始输入就没意义了，要清掉。唯一的例外是正在并击中（`editing_chord_`）
    /// 或正在转发（`sending_chord_`）—— 那时组合态的变化是预期内的。
    fn handle_context_update(&mut self, engine: &Engine) {
        if engine.context.is_composing() {
            self.composing = true;
        } else if self.composing {
            self.composing = false;
            if !self.editing_chord || self.sending_chord {
                self.raw_sequence.clear();
            }
        }
    }

    /// C++ `OnUnhandledKey`：直接上屏的 ASCII 不该被算进原始输入。
    ///
    /// 用例（源码注释里写的那条）：`3.14{Return}` 不该额外上屏一份 `14`。
    fn handle_unhandled_key(&mut self, key: &KeyEvent) {
        if (key.modifier & !K_SHIFT_MASK) == 0 && (0x20..=0x7e).contains(&key.keycode) {
            self.raw_sequence.clear();
        }
    }

    /// 认领回放标记：命中即移除并返回 true（说明这个键是我自己转发出去的）。
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

impl Processor for ChordComposer {
    fn process_key_event(&mut self, engine: &mut Engine, key_event: &KeyEvent) -> ProcessResult {
        // 自己转发出去的键绕回来了 ⇒ 走"功能键"分支（等价 C++ 的
        // `if (sending_chord_) return ProcessFunctionKey(key)`），别当成并击键再吃一次
        if self.take_replay_marker(key_event) {
            return self.process_function_key(engine, key_event);
        }
        if engine.context.get_option("ascii_mode") {
            return ProcessResult::Noop;
        }
        if self.sending_chord {
            return self.process_function_key(engine, key_event);
        }
        let is_key_up = key_event.release();
        let ch = key_event.keycode;
        if !is_key_up && (0x20..=0x7e).contains(&ch) {
            if key_event.ctrl() || key_event.alt() || key_event.super_key() || key_event.caps() {
                // 这种组合会把字符序列打断，攒着的原始输入作废
                self.raw_sequence.clear();
            } else if !engine.context.is_composing() || !self.raw_sequence.is_empty() {
                // 只在「还没开始组字」或「已经在攒」时缓冲 —— 组字态下打字是别人在处理
                self.raw_sequence.push(ch as u8 as char);
            }
        }
        let result = self.process_chording_key(engine, key_event);
        if result != ProcessResult::Noop {
            return result;
        }
        self.process_function_key(engine, key_event)
    }

    fn on_attach(&mut self, engine: &mut Engine) {
        // `chord_composer.cc:48`：告诉宿主"本方案要抬键事件"。没有它，Windows
        // 侧不会转发 KeyUp，`finish_chord_on_all_keys_released` 这条路径永远走不到。
        engine.context.set_option("_chord_typing", true);
    }

    fn on_context_update(&mut self, engine: &mut Engine) {
        self.handle_context_update(engine);
    }

    fn on_unhandled_key(&mut self, _engine: &mut Engine, key: &KeyEvent) {
        // 没人吃下的可能正是我刚转发出去的那个键 —— 顺手把标记消掉
        self.take_replay_marker(key);
        self.handle_unhandled_key(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn composer(config: Value) -> ChordComposer {
        ChordComposer::new(&config)
    }

    fn key(code: u32, modifier: u32) -> KeyEvent {
        KeyEvent::new(code, modifier)
    }

    fn c(ch: char) -> KeyEvent {
        key(ch as u32, 0)
    }

    #[test]
    fn base_layer_maps_shifted_keys() {
        // Shift+a 送 'A'，落回 'a'
        assert_eq!(base_layer_keycode(&key('A' as u32, K_SHIFT_MASK)), 'a' as u32);
        // 上档符号也落回基态：'!' → '1'，'@' → '2'，'^' → '6'，'~' → '`'
        assert_eq!(base_layer_keycode(&key('!' as u32, K_SHIFT_MASK)), '1' as u32);
        assert_eq!(base_layer_keycode(&key('@' as u32, K_SHIFT_MASK)), '2' as u32);
        assert_eq!(base_layer_keycode(&key('^' as u32, K_SHIFT_MASK)), '6' as u32);
        assert_eq!(base_layer_keycode(&key('~' as u32, K_SHIFT_MASK)), '`' as u32);
        // 无 Shift 原样返回
        assert_eq!(base_layer_keycode(&c('a')), 'a' as u32);
        // 非 ASCII 不受影响
        assert_eq!(base_layer_keycode(&key(XK_BACKSPACE, K_SHIFT_MASK)), XK_BACKSPACE);
    }

    #[test]
    fn map_table_covers_printable_ascii() {
        // 0x20..=0x7e 共 95 个位置，表必须够长（C++ 是 96，最后一格不用）
        assert!(MAP_TO_BASE_LAYER.len() >= 95, "表长 {}", MAP_TO_BASE_LAYER.len());
        for ch in 0x20u32..=0x7e {
            let mapped = base_layer_keycode(&key(ch, K_SHIFT_MASK));
            assert!(
                (0x20..=0x7e).contains(&mapped),
                "0x{ch:x} 映射到了 0x{mapped:x}"
            );
        }
    }

    #[test]
    fn chording_state_tracks_press_and_release() {
        let mut st = ChordingState::default();
        assert!(st.press_key('a' as u32));
        assert!(!st.press_key('a' as u32), "重复 keydown 返回 false");
        assert!(st.is_pressed('a' as u32));
        assert!(st.release_key('a' as u32));
        assert!(!st.release_key('a' as u32), "重复 keyup 返回 false");
        assert!(!st.is_pressed('a' as u32));
        assert!(st.add_key_to_chord('a' as u32));
        assert!(!st.add_key_to_chord('a' as u32), "重复入和弦返回 false");
        st.clear();
        assert!(st.pressed_keys.is_empty() && st.recognized_chord.is_empty());
    }

    #[test]
    fn serialize_chord_follows_alphabet_order_not_press_order() {
        // alphabet 顺序 a,b,c；和弦里同时有 c 和 a，出来的仍是 "ac"
        let mut cp = composer(json!({"chord_composer": {"alphabet": "abc"}}));
        let mut chord = Chord::new();
        chord.insert('c' as u32);
        chord.insert('a' as u32);
        assert_eq!(cp.serialize_chord(&chord), "ac");
        // 不在 alphabet 里的键不参与
        chord.insert('z' as u32);
        assert_eq!(cp.serialize_chord(&chord), "ac");
    }

    #[test]
    fn serialize_chord_applies_algebra() {
        // derive/^ab$/(ab)/ ⇒ 和弦 {a,b} 出 "(ab)"（宫保拼音就靠这类规则把并击摊成音节）
        let mut cp = composer(json!({"chord_composer": {
            "alphabet": "abc",
            "algebra": ["derive/^ab$/(ab)/"]
        }}));
        let mut chord = Chord::new();
        chord.insert('a' as u32);
        chord.insert('b' as u32);
        assert_eq!(cp.serialize_chord(&chord), "(ab)");
        // 规则不匹配时原样返回
        let mut only_a = Chord::new();
        only_a.insert('a' as u32);
        assert_eq!(cp.serialize_chord(&only_a), "a");
    }

    #[test]
    fn config_flags_are_read_from_the_chord_composer_section() {
        let cp = composer(json!({"chord_composer": {
            "alphabet": "abc",
            "use_shift": true,
            "finish_chord_on_first_key_release": true,
            "bindings": {"Return": "commit_raw_input"}
        }}));
        assert!(cp.use_shift);
        assert!(!cp.use_control, "没写就是 false");
        assert!(cp.finish_chord_on_first_key_release);
        assert_eq!(cp.chording_keys.len(), 3);
        assert_eq!(
            cp.bindings.get(&KeyEvent::new(0xff0d, 0)),
            Some(ACTION_COMMIT_RAW_INPUT)
        );
    }

    #[test]
    fn output_and_prompt_formats_are_separate_projections() {
        let mut cp = composer(json!({"chord_composer": {
            "alphabet": "abc",
            "prompt_format": ["xlit/abc/ABC/"],
            "output_format": ["xlit/abc/xyz/"]
        }}));
        let mut chord = Chord::new();
        chord.insert('a' as u32);
        // prompt 只影响提示串，编码串本身不变
        let code = cp.serialize_chord(&chord);
        assert_eq!(code, "a");
        let mut prompt = code.clone();
        cp.prompt_format.apply_str(&mut prompt);
        assert_eq!(prompt, "A");
        let mut out = code;
        cp.output_format.apply_str(&mut out);
        assert_eq!(out, "x");
    }
}
