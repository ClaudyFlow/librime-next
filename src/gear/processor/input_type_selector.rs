//! 按「输入类型」切换候选选重键的 `selector` 包装器。
//!
//! `menu/alternative_select_keys` 是**方案级**配置（`Schema::select_keys` 只有一份），
//! 而本方案是「一个方案挂多个子方案、共用一份配置」。矛盾点：
//!
//! * 注音（大千键盘）的数字行 `1 2 5 8 9 0` 与声调键 `3 4 6 7` 全是**编码**，
//!   被 `input_type_speller` 吃进输入串，数字本来就没法选重；给它开
//!   `ABCDEFGHIJ` 是纯增益。
//! * 但同一份 `select_keys` 对全拼/双拼/地拼生效后，数字 1–6 选重就全废了
//!   （`selector.rs`：`select_keys` 非空就**只用它**，不再回落到数字映射）。
//!
//! 所以照 `input_type_speller` 的思路：`Selector` 本体不动，外面按当前打开的输入类型
//! 开关换一份选重键再代理过去：
//!
//! ```yaml
//! speller:
//!   input_types:
//!     zhuyin:
//!       select_keys: ABCDEFGHIJ   # 只对注音生效
//! ```
//!
//! 没写 `select_keys` 的子方案（全拼 / 地球拼音 / 自然码 / 小鹤）直接走原生
//! `Selector`，即 `menu/alternative_select_keys`（没配就是数字 1–0 映射）。

use serde_json::Value;

use crate::gear::processor::{ProcessResult, Processor};
use crate::gear::processor::selector::Selector;
use crate::engine::key_event::KeyEvent;
use crate::engine::runtime::Engine;

pub struct InputTypeSelector {
    /// 没有输入类型开关打开（或该子方案没配 `select_keys`）时用的原生选择器。
    default: Selector,
    /// `(开关名, 该子方案的选重键)`。
    profiles: Vec<(String, String)>,
}

impl InputTypeSelector {
    pub fn new(config: &Value) -> Self {
        let mut profiles = Vec::new();
        if let Some(overrides) = config
            .pointer("/speller/input_type")
            .and_then(|value| value.as_object())
        {
            for (option, overlay) in overrides {
                // 空串 = 显式回落数字映射，等同没配
                let Some(keys) = overlay.get("select_keys").and_then(|v| v.as_str()) else {
                    continue;
                };
                if keys.is_empty() {
                    continue;
                }
                profiles.push((option.clone(), keys.to_string()));
            }
        }
        Self {
            default: Selector,
            profiles,
        }
    }

    /// 当前生效的输入类型开关名（诊断/测试用）。`None` = 用原生选择器。
    pub fn active_type<'a>(&'a self, engine: &Engine) -> Option<&'a str> {
        self.profiles
            .iter()
            .find(|(option, _)| engine.context.get_option(option))
            .map(|(option, _)| option.as_str())
    }
}

impl Processor for InputTypeSelector {
    fn process_key_event(&mut self, engine: &mut Engine, key: &KeyEvent) -> ProcessResult {
        let keys = self
            .profiles
            .iter()
            .find(|(option, _)| engine.context.get_option(option))
            .map(|(_, keys)| keys.clone());
        let Some(keys) = keys else {
            return self.default.process_key_event(engine, key);
        };
        // `Selector` 从 `engine.schema.select_keys` 读选重表：按键期间临时替换成
        // 该子方案的一份，处理完立刻还原（方案配置对其它组件仍是原值）。
        let saved = std::mem::replace(&mut engine.schema.select_keys, keys);
        let result = self.default.process_key_event(engine, key);
        engine.schema.select_keys = saved;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> Value {
        json!({
            "speller": {
                "input_type": {
                    "pinyin": { "alphabet": "zyxwvutsrqponmlkjihgfedcba" },
                    "zhuyin": {
                        "alphabet": "1qaz2wsxedcrfv5tgbyhnujm8ik,9ol.0p;/- 6347",
                        "select_keys": "ABCDEFGHIJ"
                    }
                }
            }
        })
    }

    #[test]
    fn only_profiles_with_select_keys_are_registered() {
        let selector = InputTypeSelector::new(&config());
        assert_eq!(selector.profiles.len(), 1);
        assert_eq!(selector.profiles[0].0, "zhuyin");
        assert_eq!(selector.profiles[0].1, "ABCDEFGHIJ");
    }

    #[test]
    fn resolves_active_input_type() {
        let selector = InputTypeSelector::new(&config());
        let mut engine = Engine::empty_for_test();
        assert_eq!(selector.active_type(&engine), None);
        engine.context.set_option("zhuyin", true);
        assert_eq!(selector.active_type(&engine), Some("zhuyin"));
        engine.context.set_option("zhuyin", false);
        engine.context.set_option("pinyin", true);
        assert_eq!(selector.active_type(&engine), None, "全拼不该有自己的选重表");
    }

    #[test]
    fn inactive_profile_keeps_schema_select_keys() {
        let mut selector = InputTypeSelector::new(&config());
        let mut engine = Engine::empty_for_test();
        engine.schema.select_keys = String::new();
        let result = selector.process_key_event(&mut engine, &KeyEvent::new(0x41, 0));
        assert_eq!(result, ProcessResult::Noop, "空组合态下选择器不处理任何键");
        assert_eq!(engine.schema.select_keys, "");
    }
}
