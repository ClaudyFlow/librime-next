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
        }
    }

    /// C++ `found()`。
    pub fn is_found(&self) -> bool {
        self.found
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
        if let Some(name) = item.get("name").and_then(Value::as_str) {
            let option = SwitchOption {
                found: true,
                kind: SwitchType::ToggleOption,
                option_name: name.to_string(),
                reset_value: item.get("reset").and_then(Value::as_i64).unwrap_or(-1) as i32,
                switch_index,
                option_index: 0,
                group: Vec::new(),
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
                };
                if callback(&option) {
                    return Some(option);
                }
            }
        }
        None
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
            group: current.group.clone(),
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
            group: current.group.clone(),
        }
    }
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
}
