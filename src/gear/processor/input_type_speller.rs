//! 按「输入类型」切换字符集的 `speller` 包装器。
//!
//! 一套方案里同时挂全拼 / 地球拼音 / 双拼 / 注音时，各子方案对**按键字符集**的
//! 要求并不相同：
//!
//! * 全拼、自然码、小鹤：只认字母，数字键留给 `selector` 选候选；
//! * 地球拼音：字母之外还要认 `1`–`5`（声调进编码，如 `zhong1guo2`）；
//! * 注音：大千键盘把 `1 2 5 8 9 0` 当声母/韵母键，`3 4 6 7` 当声调键。
//!
//! 上游 `speller` 的 `alphabet` / `initials` / `finals` 是**方案级静态值**，一个方案
//! 只有一份。本组件保持 `Speller` 本体不动，只在外面按当前打开的输入类型开关
//! 选一份**合并过配置**的 `Speller` 来代理按键：
//!
//! ```yaml
//! speller:
//!   alphabet: zyxwvutsrqponmlkjihgfedcba        # 默认（全拼）字符集
//!   input_types:
//!     terra:
//!       alphabet: zyxwvutsrqponmlkjihgfedcba12345
//!       initials: zyxwvutsrqponmlkjihgfedcba
//!     zhuyin:
//!       alphabet: '1qaz2wsxedcrfv5tgbyhnujm8ik,9ol.0p;/- 6347'
//!       finals: " 6347"
//!       use_space: true
//! ```
//!
//! 合并规则：`speller/input_types/<开关名>` 里的键**浅覆盖**到 `speller/` 同名键；
//! 没有命中的开关回落到顶层 `speller`。开关之间必须互斥（与 `translator/prisms`
//! 的 input type 约定一致：运行时按 `prisms` 的键逐个查 option，第一个打开的生效）。

use serde_json::Value;

use crate::gear::processor::{ProcessResult, Processor};
use crate::engine::key_event::KeyEvent;
use crate::gear::processor::speller::Speller;
use crate::engine::runtime::Engine;

pub struct InputTypeSpeller {
    /// 没有输入类型开关打开时用的默认拼写器。
    default: Speller,
    /// `(开关名, 合并后的拼写器)`。
    profiles: Vec<(String, Speller)>,
}

impl InputTypeSpeller {
    pub fn new(config: &Value) -> Self {
        let default = Speller::new(config);
        let mut profiles = Vec::new();
        if let Some(overrides) = config
            .pointer("/speller/input_type")
            .and_then(|value| value.as_object())
        {
            for (option, overlay) in overrides {
                let Some(overlay) = overlay.as_object() else {
                    continue;
                };
                let mut merged = config.clone();
                if let Some(speller) = merged.pointer_mut("/speller").and_then(|v| v.as_object_mut())
                {
                    for (key, value) in overlay {
                        speller.insert(key.clone(), value.clone());
                    }
                }
                profiles.push((option.clone(), Speller::new(&merged)));
            }
        }
        Self { default, profiles }
    }

    /// 当前生效的输入类型开关名（诊断/测试用）。`None` = 用默认拼写器。
    pub fn active_type(&self, engine: &Engine) -> Option<&str> {
        self.profiles
            .iter()
            .find(|(option, _)| engine.context.get_option(option))
            .map(|(option, _)| option.as_str())
    }
}

impl Processor for InputTypeSpeller {
    fn process_key_event(&mut self, engine: &mut Engine, key: &KeyEvent) -> ProcessResult {
        let index = self
            .profiles
            .iter()
            .position(|(option, _)| engine.context.get_option(option));
        match index {
            Some(index) => self.profiles[index].1.process_key_event(engine, key),
            None => self.default.process_key_event(engine, key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::key_event::KeyEvent;
    use serde_json::json;

    const XK_1: u32 = 0x31;
    const XK_n: u32 = 0x6e;

    /// 空引擎 + 指定输入类型开关（拼写器由测试直接调用）。
    fn engine_with(_config: Value, option: &str) -> Engine {
        let mut engine = Engine::empty_for_test();
        if !option.is_empty() {
            engine.context.set_option(option, true);
        }
        engine
    }

    fn config() -> Value {
        json!({
            "speller": {
                "alphabet": "zyxwvutsrqponmlkjihgfedcba",
                "input_type": {
                    "terra": {
                        "alphabet": "zyxwvutsrqponmlkjihgfedcba12345",
                        "initials": "zyxwvutsrqponmlkjihgfedcba"
                    }
                }
            }
        })
    }

    #[test]
    fn default_profile_rejects_digit() {
        let mut engine = engine_with(config(), "");
        engine.context.push_input_str("zhong");
        let mut speller = InputTypeSpeller::new(&config());
        let result = speller.process_key_event(&mut engine, &KeyEvent::new(XK_1, 0));
        assert_eq!(result, ProcessResult::Noop);
        assert_eq!(engine.context.input, "zhong");
    }

    #[test]
    fn terra_profile_accepts_digit_while_composing() {
        let mut engine = engine_with(config(), "terra");
        engine.context.push_input_str("zhong");
        let mut speller = InputTypeSpeller::new(&config());
        let result = speller.process_key_event(&mut engine, &KeyEvent::new(XK_1, 0));
        assert_eq!(result, ProcessResult::Accepted);
        assert_eq!(engine.context.input, "zhong1");
    }

    #[test]
    fn terra_digit_is_not_an_initial() {
        let mut engine = engine_with(config(), "terra");
        let mut speller = InputTypeSpeller::new(&config());
        let result = speller.process_key_event(&mut engine, &KeyEvent::new(XK_1, 0));
        assert_eq!(result, ProcessResult::Noop, "空串上按 1 应留给 selector");
        assert_eq!(engine.context.input, "");
    }

    #[test]
    fn terra_profile_keeps_letters() {
        let mut engine = engine_with(config(), "terra");
        let mut speller = InputTypeSpeller::new(&config());
        let result = speller.process_key_event(&mut engine, &KeyEvent::new(XK_n, 0));
        assert_eq!(result, ProcessResult::Accepted);
        assert_eq!(engine.context.input, "n");
    }
}
