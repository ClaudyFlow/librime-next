//! `gear/key_binder.cc` 移植：按键 → 动作（翻页 / 开关 / 切方案 / 转发按键）。
//!
//! 配置形式（`key_binder/bindings`）：
//!
//! ```yaml
//! key_binder:
//!   bindings:
//!     - { when: paging, accept: minus, send: Page_Up }
//!     - { when: has_menu, accept: period, send: Page_Down }
//!     - { when: always, accept: Control+shift+4, toggle: ascii_punct }
//!     - { when: always, accept: Control+0, select: .next }
//! ```
//!
//! # 两个容易读错的点
//!
//! 1. **`when` 是"当前处于哪些状态"，不是"优先级"**。同一按键可以绑多条，C++ 把
//!    它们按 `whence` 升序排（Predicting < Paging < HasMenu < Composing < Always），
//!    取**第一条状态成立**的。所以 predicting 的绑定最优先。
//! 2. 翻页键会触发一个补丁版特有的补偿（[`KeyBinder::reinterpret_paging_key`]）：
//!    敲网址时 `.` 会被判成 Page_Down，后面紧跟字母则把 `.` 追补回输入串。

use crate::engine::context::Context;
use crate::engine::gear::processor::{ProcessResult, Processor};
use crate::engine::key_event::KeyEvent;
use crate::engine::switches::{SwitchOption, SwitchType, Switches};
use crate::engine::Engine;

/// C++ `KeyBindingCondition`（顺序即优先级，数值小的先看）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum KeyBindingCondition {
    Never = 0,
    WhenPredicting = 1,
    WhenPaging = 2,
    WhenHasMenu = 3,
    WhenComposing = 4,
    Always = 5,
}

impl KeyBindingCondition {
    fn translate(name: &str) -> Self {
        match name {
            "predicting" => KeyBindingCondition::WhenPredicting,
            "paging" => KeyBindingCondition::WhenPaging,
            "has_menu" => KeyBindingCondition::WhenHasMenu,
            "composing" => KeyBindingCondition::WhenComposing,
            "always" => KeyBindingCondition::Always,
            _ => KeyBindingCondition::Never,
        }
    }
}

/// 一条绑定要执行的动作（C++ 里是 `std::function` + `KeySequence target` 二选一）。
#[derive(Clone, Debug)]
enum KeyBindingAction {
    /// `send:` / `send_sequence:` —— 把别的按键塞回处理器链。
    Send(Vec<KeyEvent>),
    /// `toggle:` / `set_option:` / `unset_option:`
    ToggleOption(String),
    SetOption(String),
    UnsetOption(String),
    /// `select: <schema_id>`（`.next` 表示下一个方案）
    SelectSchema(String),
}

#[derive(Clone, Debug)]
struct KeyBinding {
    whence: KeyBindingCondition,
    action: KeyBindingAction,
}

impl KeyBinding {
    /// `KeyBinding::operator<`：只看 `whence`（决定插入位置）。
    fn target_keycodes(&self) -> &[KeyEvent] {
        match &self.action {
            KeyBindingAction::Send(keys) => keys,
            _ => &[],
        }
    }
}

pub struct KeyBinder {
    /// 按键 → 该键上的绑定列表（**已按 whence 升序**）。
    bindings: Vec<(KeyEvent, Vec<KeyBinding>)>,
    /// C++ `redirecting_`：正在转发按键时不再接受新绑定，防递归。
    redirecting: bool,
    /// 上一次触发翻页的键码（补丁补偿逻辑用）。
    last_paging_key: u32,
    /// 连续翻页次数（补丁补偿逻辑用）。
    paging_keystroke_count: usize,
}

const XK_PAGE_UP: u32 = 0xff55;
const XK_PAGE_DOWN: u32 = 0xff56;
const XK_KP_PAGE_UP: u32 = 0xffb8;
const XK_KP_PAGE_DOWN: u32 = 0xffb9;

fn is_paging_keycode(keycode: u32) -> bool {
    matches!(
        keycode,
        XK_PAGE_DOWN | XK_KP_PAGE_DOWN | XK_PAGE_UP | XK_KP_PAGE_UP
    )
}

impl KeyBinder {
    pub fn new(config: &serde_json::Value) -> Self {
        let mut binder = KeyBinder {
            bindings: Vec::new(),
            redirecting: false,
            last_paging_key: 0,
            paging_keystroke_count: 0,
        };
        binder.load_bindings(config);
        binder
    }

    /// C++ `LoadConfig` + `KeyBindings::LoadBindings`。
    fn load_bindings(&mut self, config: &serde_json::Value) {
        let Some(list) = config
            .pointer("/key_binder/bindings")
            .and_then(serde_json::Value::as_array)
        else {
            return;
        };
        for (index, item) in list.iter().enumerate() {
            let Some(whence) = item.get("when").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some(pattern) = item.get("accept").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let condition = KeyBindingCondition::translate(whence);
            if condition == KeyBindingCondition::Never {
                continue;
            }
            let Some(key) = KeyEvent::parse(pattern) else {
                eprintln!("[key_binder] invalid key binding #{index}, accept: '{pattern}'");
                continue;
            };
            let action = if let Some(target) = item.get("send").and_then(|v| v.as_str()) {
                match KeyEvent::parse(target) {
                    Some(k) => KeyBindingAction::Send(vec![k]),
                    None => {
                        eprintln!("[key_binder] invalid send #{index}: '{target}'");
                        continue;
                    }
                }
            } else if let Some(target) = item.get("send_sequence").and_then(|v| v.as_str()) {
                KeyBindingAction::Send(KeyEvent::parse_sequence(target))
            } else if let Some(option) = item.get("toggle").and_then(|v| v.as_str()) {
                KeyBindingAction::ToggleOption(option.to_string())
            } else if let Some(option) = item.get("set_option").and_then(|v| v.as_str()) {
                KeyBindingAction::SetOption(option.to_string())
            } else if let Some(option) = item.get("unset_option").and_then(|v| v.as_str()) {
                KeyBindingAction::UnsetOption(option.to_string())
            } else if let Some(schema) = item.get("select").and_then(|v| v.as_str()) {
                KeyBindingAction::SelectSchema(schema.to_string())
            } else {
                eprintln!(
                    "[key_binder] invalid key binding #{index}, accept: '{pattern}', when: '{whence}'"
                );
                continue;
            };
            self.bind(key, KeyBinding { whence: condition, action });
        }
    }

    /// C++ `KeyBindings::Bind`：按 `whence` 升序插入（同键多条绑定时的查找顺序）。
    fn bind(&mut self, key: KeyEvent, binding: KeyBinding) {
        if let Some((_, list)) = self.bindings.iter_mut().find(|(k, _)| k == &key) {
            let pos = list
                .iter()
                .position(|b| b.whence > binding.whence)
                .unwrap_or(list.len());
            list.insert(pos, binding);
        } else {
            self.bindings.push((key, vec![binding]));
        }
    }

    /// C++ `KeyBindingConditions`：算出"当前处于哪些状态"。
    fn current_conditions(ctx: &Context) -> Vec<KeyBindingCondition> {
        let mut v = vec![KeyBindingCondition::Always];
        if ctx.is_composing() {
            v.push(KeyBindingCondition::WhenComposing);
        }
        if ctx.has_menu() && !ctx.get_option("ascii_mode") {
            v.push(KeyBindingCondition::WhenHasMenu);
        }
        if let Some(last) = ctx.composition.segmentation.segments.last() {
            if last.has_tag("paging") {
                v.push(KeyBindingCondition::WhenPaging);
            }
            if last.has_tag("prediction") {
                v.push(KeyBindingCondition::WhenPredicting);
            }
        }
        v
    }

    /// 补丁版特有（`key_binder.cc` 尾部）：`.` 被判成翻页后紧跟字母 → 是敲网址，
    /// 把 `.` 追补回输入串。
    fn reinterpret_paging_key(&mut self, engine: &mut Engine, key: &KeyEvent) -> bool {
        if key.release() || key.modifier != 0 {
            return false;
        }
        let ch = key.keycode;
        if self.paging_keystroke_count == 1
            && self.last_paging_key == b'.' as u32
            && (ch as u8).is_ascii_alphabetic()
        {
            let input = engine.context.input.clone();
            if !input.is_empty() && !input.ends_with('.') {
                engine.context.push_input_ch('.');
                self.last_paging_key = 0;
                self.paging_keystroke_count = 0;
                return true;
            }
        }
        false
    }
}

impl Processor for KeyBinder {
    fn process_key_event(&mut self, engine: &mut Engine, key: &KeyEvent) -> ProcessResult {
        if self.redirecting || self.bindings.is_empty() {
            return ProcessResult::Noop;
        }
        if self.reinterpret_paging_key(engine, key) {
            return ProcessResult::Noop;
        }
        let Some(list) = self
            .bindings
            .iter()
            .find(|(k, _)| k.repr() == key.repr())
            .map(|(_, list)| list.clone())
        else {
            if !key.release() {
                self.last_paging_key = 0;
                self.paging_keystroke_count = 0;
            }
            return ProcessResult::Noop;
        };
        let conditions = KeyBinder::current_conditions(&engine.context);
        for binding in &list {
            if !conditions.contains(&binding.whence) {
                continue;
            }
            let is_paging = binding
                .target_keycodes()
                .iter()
                .any(|k| is_paging_keycode(k.keycode));
            if is_paging {
                self.last_paging_key = key.keycode;
                self.paging_keystroke_count += 1;
            } else {
                self.last_paging_key = 0;
                self.paging_keystroke_count = 0;
            }
            perform_binding(self, engine, binding);
            return ProcessResult::Accepted;
        }
        if !key.release() {
            self.last_paging_key = 0;
            self.paging_keystroke_count = 0;
        }
        ProcessResult::Noop
    }
}

fn perform_binding(binder: &mut KeyBinder, engine: &mut Engine, binding: &KeyBinding) {
    match &binding.action {
        KeyBindingAction::Send(keys) => {
            binder.redirecting = true;
            for k in keys.clone() {
                engine.enqueue_synthetic_key(k);
            }
            binder.redirecting = false;
        }
        KeyBindingAction::ToggleOption(option) => toggle_option(engine, option),
        KeyBindingAction::SetOption(option) => set_option(engine, option),
        KeyBindingAction::UnsetOption(option) => unset_option(engine, option),
        KeyBindingAction::SelectSchema(schema) => select_schema(engine, schema),
    }
}

/// C++ `radio_select_option`：单选组里只让目标为 1，其余一律置 0。
fn radio_select_option(ctx: &mut Context, the_option: &SwitchOption) {
    for (index, name) in the_option.group.iter().enumerate() {
        let value = index == the_option.option_index;
        if ctx.get_option(name) != value {
            ctx.set_option(name, value);
        }
    }
}

fn is_switch_index(option: &str) -> bool {
    option.starts_with('@')
}

fn switch_by_index(switches: &Switches, option: &str) -> SwitchOption {
    option[1..]
        .parse::<usize>()
        .ok()
        .map_or_else(SwitchOption::none, |index| switches.by_index(index))
}

/// C++ `toggle_option`。
fn toggle_option(engine: &mut Engine, option: &str) {
    let switches = Switches::new(&engine.schema.config);
    let the_option = if is_switch_index(option) {
        switch_by_index(&switches, option)
    } else {
        switches.option_by_name(option)
    };
    if !the_option.is_found() {
        let name = option.to_string();
        engine.context.set_option(&name, !engine.context.get_option(&name));
        return;
    }
    if the_option.kind == SwitchType::RadioGroup {
        // 先找组内当前选中的那一项（可能一个都没选 —— C++ 视为非法状态）
        let selected_index = the_option
            .group
            .iter()
            .position(|name| engine.context.get_option(name));
        match selected_index {
            Some(index) => {
                let current = SwitchOption {
                    option_index: index,
                    option_name: the_option.group[index].clone(),
                    ..the_option.clone()
                };
                let next = switches.cycle(&current);
                if next.is_found() {
                    radio_select_option(&mut engine.context, &next);
                }
            }
            // 非法状态（谁都没选）：直接选中本次指定的那一项
            None => radio_select_option(&mut engine.context, &the_option),
        }
    } else {
        let value = !engine.context.get_option(&the_option.option_name);
        engine.context.set_option(&the_option.option_name, value);
    }
}

/// C++ `set_option`。
fn set_option(engine: &mut Engine, option: &str) {
    let switches = Switches::new(&engine.schema.config);
    let the_option = switches.option_by_name(option);
    if the_option.is_found() && the_option.kind == SwitchType::RadioGroup {
        radio_select_option(&mut engine.context, &the_option);
    } else {
        engine.context.set_option(option, true);
    }
}

/// C++ `unset_option`。
fn unset_option(engine: &mut Engine, option: &str) {
    let switches = Switches::new(&engine.schema.config);
    let the_option = switches.option_by_name(option);
    if the_option.is_found() && the_option.kind == SwitchType::RadioGroup {
        if engine.context.get_option(option) {
            let default_option = switches.reset(&the_option);
            if default_option.is_found() {
                radio_select_option(&mut engine.context, &default_option);
            }
        }
    } else {
        engine.context.set_option(option, false);
    }
}

/// C++ `select_schema`：`.next` 要走 Switcher（还没移植），其余换方案。
fn select_schema(engine: &mut Engine, schema: &str) {
    if schema == ".next" {
        eprintln!("[key_binder] select: .next 需要 Switcher，尚未移植，忽略");
        return;
    }
    engine.apply_schema(schema);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::key_event::K_CONTROL_MASK;
    use serde_json::json;

    fn engine_with_config(config: serde_json::Value) -> Engine {
        let mut engine = Engine::empty_for_test();
        engine.schema = crate::engine::schema::Schema::from_config_for_test(config);
        engine
    }

    fn base_config() -> serde_json::Value {
        json!({
            "switches": [
                { "name": "ascii_mode", "reset": 0 },
                { "options": ["zh_simp", "zh_trad"], "reset": 0 },
                { "name": "full_shape" }
            ],
            "key_binder": {
                "bindings": [
                    { "when": "always", "accept": "Control+period", "toggle": "full_shape" },
                    { "when": "has_menu", "accept": "minus", "send": "Page_Up" },
                    { "when": "always", "accept": "Control+minus", "toggle": "@1" }
                ]
            }
        })
    }

    #[test]
    fn toggle_flips_a_plain_option() {
        let mut engine = engine_with_config(base_config());
        let mut binder = KeyBinder::new(&engine.schema.config);
        let key = KeyEvent::parse("Control+period").unwrap();
        assert_eq!(key.keycode, b'.' as u32);
        assert_eq!(key.modifier & K_CONTROL_MASK, K_CONTROL_MASK);

        assert!(!engine.context.get_option("full_shape"));
        assert_eq!(
            binder.process_key_event(&mut engine, &key),
            ProcessResult::Accepted
        );
        assert!(engine.context.get_option("full_shape"));
    }

    #[test]
    fn toggle_cycles_a_radio_group() {
        let mut engine = engine_with_config(base_config());
        let mut binder = KeyBinder::new(&engine.schema.config);
        let key = KeyEvent::parse("Control+minus").unwrap();
        binder.process_key_event(&mut engine, &key);
        assert!(engine.context.get_option("zh_simp"), "第一项应被选中");
        binder.process_key_event(&mut engine, &key);
        assert!(engine.context.get_option("zh_trad"), "应切到第二项");
        assert!(!engine.context.get_option("zh_simp"), "单选组应互斥");
    }

    #[test]
    fn bindings_are_checked_in_condition_order() {
        // 同一个键绑两条不同 when：按 Predicting < Paging < HasMenu < Composing < Always
        let config = json!({
            "key_binder": {
                "bindings": [
                    { "when": "always", "accept": "F1", "set_option": "loose" },
                    { "when": "composing", "accept": "F1", "set_option": "tight" }
                ]
            }
        });
        let mut engine = engine_with_config(config);
        let mut binder = KeyBinder::new(&engine.schema.config);
        let key = KeyEvent::parse("F1").unwrap();
        // 空输入 → 只有 always 成立
        binder.process_key_event(&mut engine, &key);
        assert!(engine.context.get_option("loose"));
        assert!(!engine.context.get_option("tight"));

        // 进入组字态 → composing 优先（它排在 always 前面）
        engine.context.push_input_str("abc");
        engine.set_option("loose", false);
        let mut binder = KeyBinder::new(&engine.schema.config);
        binder.process_key_event(&mut engine, &key);
        assert!(engine.context.get_option("tight"), "composing 绑定应优先");
        assert!(!engine.context.get_option("loose"));
    }

    #[test]
    fn unbound_key_is_noop() {
        let mut engine = engine_with_config(base_config());
        let mut binder = KeyBinder::new(&engine.schema.config);
        let key = KeyEvent::parse("F12").unwrap();
        assert_eq!(
            binder.process_key_event(&mut engine, &key),
            ProcessResult::Noop
        );
    }
}
