//! `src/rime/switches.{h,cc}`：方案 `switches:` 清单的读取与切换逻辑。
//!
//! 方案里长这样：
//!
//! ```yaml
//! switches:
//!   - name: ascii_mode
//!     reset: 0
//!     states: [ 中文, 西文 ]
//!   - options: [ zh_simp, zh_trad ]
//!     reset: 0
//!   - name: full_shape
//!     states: [ 半角, 全角 ]
//! ```
//!
//! `name:` = 开关开关（toggle）；`options:` = 单选组（radio group，互斥）。
//!
//! ## 与 C++ 的一处结构差异
//!
//! C++ 的 `SwitchOption` 持有 `an<ConfigMap> the_switch`（回指配置节点），
//! `Cycle` / `Reset` 靠它再读一次 `options`。Rust 这里把整组的名字**一并存进**
//! [`SwitchOption::group`]，于是这两个函数不再需要回指配置 —— 结果一致，
//! 但避免了借用生命周期穿过组件边界。

use serde_json::Value;

/// 开关种类（C++ `Switches::SwitchType`）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SwitchType {
    #[default]
    ToggleOption,
    RadioGroup,
}

/// 一个"可切换的项"（C++ `Switches::SwitchOption`）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SwitchOption {
    /// 是否找到 —— C++ 用 `bool(the_switch)` 表达同一件事。
    pub found: bool,
    pub kind: SwitchType,
    pub option_name: String,
    /// `reset:` 的初值；**-1 表示没配**（C++ 同样用 -1 作哨兵）。
    pub reset_value: i32,
    /// 该开关在 `switches:` 列表里的下标。
    pub switch_index: usize,
    /// 该项在单选组里的下标（toggle 恒为 0）。
    pub option_index: usize,
    /// 单选组的全部选项名（便于 `Cycle` / `Reset` 不再回查配置）。
    pub group: Vec<String>,
    /// `states:` 数组：每个状态一个标签。缺省为空 ⇒ 该项在菜单里不显示
    /// （C++ `has_state_label(option, 0)` 就是查这个）。
    pub states: Vec<String>,
    /// `abbrev:` 数组：`abbreviate_options` 打开时用的短标签，缺省时取 `states`
    /// 每项的第一个字符。
    pub abbrev: Vec<String>,
}

impl SwitchOption {
    pub fn none() -> Self {
        SwitchOption {
            found: false,
            kind: SwitchType::ToggleOption,
            option_name: String::new(),
            reset_value: -1,
            switch_index: 0,
            option_index: 0,
            group: Vec::new(),
            states: Vec::new(),
            abbrev: Vec::new(),
        }
    }

    /// C++ `found()`。
    pub fn is_found(&self) -> bool {
        self.found
    }

    /// C++ 静态 `Switches::GetStateLabel(the_switch, state_index, abbreviated)`。
    ///
    /// `abbreviated` 时优先用 `abbrev:`，没配就取 `states[i]` 的**第一个字符**
    /// （C++ 的 `first_unicode_byte_length` —— 按 UTF-8 字节切，落在字符边界上，
    /// 与 Rust 的 `chars().next()` 等价）。
    pub fn state_label(&self, state_index: usize, abbreviated: bool) -> Option<String> {
        if !self.found || self.states.len() <= state_index {
            return None;
        }
        if abbreviated {
            if let Some(label) = self.abbrev.get(state_index) {
                return Some(label.clone());
            }
            // 取第一个字符（不是第一个字节）
            return self.states[state_index]
                .chars()
                .next()
                .map(|c| c.to_string());
        }
        Some(self.states[state_index].clone())
    }

    /// 有标签才算"这一项该出现在菜单里"（C++ `has_state_label`）。
    pub fn has_state_label(&self, state_index: usize) -> bool {
        self.state_label(state_index, false).is_some()
    }
}

/// `Switches`：方案配置的只读视图。
pub struct Switches<'a> {
    config: &'a Value,
}

impl<'a> Switches<'a> {
    pub fn new(config: &'a Value) -> Self {
        Switches { config }
    }

    fn list(&self) -> Option<&Vec<Value>> {
        self.config.pointer("/switches").and_then(Value::as_array)
    }

    /// C++ `FindOption(callback)`：逐个开关项试，命中即返回。
    fn find_option(&self, mut callback: impl FnMut(&SwitchOption) -> bool) -> SwitchOption {
        let Some(list) = self.list() else {
            return SwitchOption::none();
        };
        for (switch_index, item) in list.iter().enumerate() {
            if let Some(option) = self.option_from_item(item, switch_index, &mut callback) {
                return option;
            }
        }
        SwitchOption::none()
    }

    /// C++ `FindOptionFromConfigItem`：一个开关项可能是 toggle 也可能是单选组。
    fn option_from_item(
        &self,
        item: &Value,
        switch_index: usize,
        callback: &mut impl FnMut(&SwitchOption) -> bool,
    ) -> Option<SwitchOption> {
        let states = string_list(item.get("states"));
        let abbrev = string_list(item.get("abbrev"));
        if let Some(name) = item.get("name").and_then(Value::as_str) {
            let option = SwitchOption {
                found: true,
                kind: SwitchType::ToggleOption,
                option_name: name.to_string(),
                reset_value: item.get("reset").and_then(Value::as_i64).unwrap_or(-1) as i32,
                switch_index,
                option_index: 0,
                group: Vec::new(),
                states,
                abbrev,
            };
            if callback(&option) {
                return Some(option);
            }
            return None;
        }
        if let Some(options) = item.get("options").and_then(Value::as_array) {
            let names: Vec<String> = options
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            if names.is_empty() {
                return None;
            }
            let reset_value = item.get("reset").and_then(Value::as_i64).unwrap_or(-1) as i32;
            for (option_index, option_name) in names.iter().enumerate() {
                let option = SwitchOption {
                    found: true,
                    kind: SwitchType::RadioGroup,
                    option_name: option_name.clone(),
                    reset_value,
                    switch_index,
                    option_index,
                    group: names.clone(),
                    states: states.clone(),
                    abbrev: abbrev.clone(),
                };
                if callback(&option) {
                    return Some(option);
                }
            }
        }
        None
    }

    /// C++ `SwitchOption::found()` 的批量版：把 `switches:` 列表**全部**展开。
    ///
    /// 单选组的每个选项各算一项（与 `FindOption` 的回调语义一致），顺序保持配置
    /// 顺序。`switch_translator` 建菜单时就是靠这个拿全量 —— C++ 那边是
    /// `FindOption` 传一个永远返回 `kContinue` 的回调，Rust 里直接给个明确的遍历
    /// 入口比复用"找到第一个"的 `find_option` 更不容易误用。
    pub fn all_options(&self) -> Vec<SwitchOption> {
        let Some(list) = self.list() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (switch_index, item) in list.iter().enumerate() {
            let states = string_list(item.get("states"));
            let abbrev = string_list(item.get("abbrev"));
            let reset_value = item.get("reset").and_then(Value::as_i64).unwrap_or(-1) as i32;
            if let Some(name) = item.get("name").and_then(Value::as_str) {
                out.push(SwitchOption {
                    found: true,
                    kind: SwitchType::ToggleOption,
                    option_name: name.to_string(),
                    reset_value,
                    switch_index,
                    option_index: 0,
                    group: Vec::new(),
                    states,
                    abbrev,
                });
                continue;
            }
            if let Some(options) = item.get("options").and_then(Value::as_array) {
                let names: Vec<String> = options
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                for (option_index, option_name) in names.iter().enumerate() {
                    out.push(SwitchOption {
                        found: true,
                        kind: SwitchType::RadioGroup,
                        option_name: option_name.clone(),
                        reset_value,
                        switch_index,
                        option_index,
                        group: names.clone(),
                        states: states.clone(),
                        abbrev: abbrev.clone(),
                    });
                }
            }
        }
        out
    }

    /// C++ `OptionByName`。
    pub fn option_by_name(&self, option_name: &str) -> SwitchOption {
        self.find_option(|option| option.option_name == option_name)
    }

    /// C++ `ByIndex`：第 `switch_index` 个开关的第一项
    /// （单选组则取组内第一项）。
    pub fn by_index(&self, switch_index: usize) -> SwitchOption {
        let Some(item) = self.list().and_then(|l| l.get(switch_index)) else {
            return SwitchOption::none();
        };
        let mut first = None;
        self.option_from_item(item, switch_index, &mut |option| {
            first = Some(option.clone());
            true
        });
        first.unwrap_or_else(SwitchOption::none)
    }

    /// C++ `Cycle`：单选组里取下一项（环形）。组内只有一项时返回空。
    pub fn cycle(&self, current: &SwitchOption) -> SwitchOption {
        if current.group.len() <= 1 {
            return SwitchOption::none();
        }
        let next = (current.option_index + 1) % current.group.len();
        if next == current.option_index {
            return SwitchOption::none();
        }
        SwitchOption {
            found: true,
            kind: SwitchType::RadioGroup,
            option_name: current.group[next].clone(),
            reset_value: current.reset_value,
            switch_index: current.switch_index,
            option_index: next,
            ..current.clone()
        }
    }

    /// C++ `Reset`：回到 `reset:` 指定的那一项；没配 reset 就按 0 处理。
    pub fn reset(&self, current: &SwitchOption) -> SwitchOption {
        if current.group.is_empty() {
            return SwitchOption::none();
        }
        let default_state = if current.reset_value >= 0 {
            current.reset_value as usize
        } else {
            0
        };
        if default_state >= current.group.len() || default_state == current.option_index {
            return SwitchOption::none();
        }
        SwitchOption {
            found: true,
            kind: SwitchType::RadioGroup,
            option_name: current.group[default_state].clone(),
            reset_value: current.reset_value,
            switch_index: current.switch_index,
            option_index: default_state,
            ..current.clone()
        }
    }
}

/// 从配置节点读字符串数组（`states:` / `abbrev:`）。非数组或缺省 → 空。
fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> Value {
        json!({
            "switches": [
                { "name": "ascii_mode", "reset": 0, "states": ["中文", "西文"] },
                { "options": ["zh_simp", "zh_trad", "zh_hant"], "reset": 0 },
                { "name": "full_shape", "states": ["半角", "全角"] }
            ]
        })
    }

    #[test]
    fn option_by_name_finds_toggle_and_radio() {
        let cfg = config();
        let s = Switches::new(&cfg);
        let ascii = s.option_by_name("ascii_mode");
        assert!(ascii.is_found());
        assert_eq!(ascii.kind, SwitchType::ToggleOption);
        assert_eq!(ascii.switch_index, 0);

        let trad = s.option_by_name("zh_trad");
        assert_eq!(trad.kind, SwitchType::RadioGroup);
        assert_eq!(trad.option_index, 1);
        assert_eq!(trad.group, vec!["zh_simp", "zh_trad", "zh_hant"]);

        assert!(!s.option_by_name("nonexistent").is_found());
    }

    #[test]
    fn by_index_returns_first_option_of_the_switch() {
        let cfg = config();
        let s = Switches::new(&cfg);
        let opt = s.by_index(1);
        assert!(opt.is_found());
        assert_eq!(opt.option_name, "zh_simp"); // 单选组的第一项
        assert!(!s.by_index(9).is_found());
    }

    #[test]
    fn cycle_wraps_around_the_radio_group() {
        let cfg = config();
        let s = Switches::new(&cfg);
        let trad = s.option_by_name("zh_trad");
        let hant = s.cycle(&trad);
        assert_eq!(hant.option_name, "zh_hant");
        // 环形：倒数第二项（索引 2）的下一项回到索引 0
        assert_eq!(s.cycle(&hant).option_name, "zh_simp");
        // toggle 没有组，不可 cycle
        let ascii = s.option_by_name("ascii_mode");
        assert!(!s.cycle(&ascii).is_found());
    }

    #[test]
    fn reset_goes_to_configured_default() {
        let cfg = config();
        let s = Switches::new(&cfg);
        let trad = s.option_by_name("zh_trad");
        let back = s.reset(&trad);
        assert_eq!(back.option_name, "zh_simp"); // reset: 0
        // 已经在默认项上时 Reset 返回空（C++ 语义）
        assert!(!s.reset(&back).is_found());
    }

    #[test]
    fn state_labels_come_from_the_states_array() {
        let cfg = config();
        let s = Switches::new(&cfg);
        let ascii = s.option_by_name("ascii_mode");
        assert_eq!(ascii.state_label(0, false).as_deref(), Some("中文"));
        assert_eq!(ascii.state_label(1, false).as_deref(), Some("西文"));
        assert_eq!(ascii.state_label(2, false), None, "越界给空");
        assert!(ascii.has_state_label(0));
        assert!(!ascii.has_state_label(2));
    }

    #[test]
    fn abbreviated_labels_fall_back_to_first_char() {
        let cfg = json!({
            "switches": [
                { "name": "full_shape", "states": ["半角", "全角"] },
                { "name": "ascii_punct", "states": ["。，", "．，"], "abbrev": ["简", "全"] }
            ]
        });
        let s = Switches::new(&cfg);
        let full = s.option_by_name("full_shape");
        // 没配 abbrev ⇒ 取第一个字符
        assert_eq!(full.state_label(0, true).as_deref(), Some("半"));
        assert_eq!(full.state_label(1, true).as_deref(), Some("全"));
        // 配了 abbrev ⇒ 用配的
        let punct = s.option_by_name("ascii_punct");
        assert_eq!(punct.state_label(0, true).as_deref(), Some("简"));
        assert_eq!(punct.state_label(1, true).as_deref(), Some("全"));
    }

    #[test]
    fn switches_without_states_have_no_label() {
        // 没 `states:` 的开关在菜单里不显示（C++ `has_state_label(option, 0)`）
        let cfg = json!({ "switches": [{ "name": "no_label" }] });
        let s = Switches::new(&cfg);
        let opt = s.option_by_name("no_label");
        assert!(opt.is_found(), "开关本身是找得到的");
        assert!(!opt.has_state_label(0), "但没有标签，菜单里要跳过");
    }

    #[test]
    fn all_options_flattens_radio_groups_in_config_order() {
        let cfg = config();
        let s = Switches::new(&cfg);
        let all = s.all_options();
        let names: Vec<&str> = all.iter().map(|o| o.option_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["ascii_mode", "zh_simp", "zh_trad", "zh_hant", "full_shape"],
            "toggle 一项、单选组三项、再一项，保持配置顺序"
        );
        assert_eq!(all[1].switch_index, 1);
        assert_eq!(all[3].option_index, 2);
    }
}
