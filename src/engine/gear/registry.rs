//! `src/rime/registry.h` + `component.h` + `gear/gears_module.cc`：按名创建组件。
//!
//! # C++ 是怎么做的
//!
//! `ConcreteEngine::InitializeComponents`（`engine.cc:549-575`）读方案里的四个列表，
//! 逐项交给全局 `Registry`：
//!
//! ```cpp
//! CreateComponentsFromList<Processor>(this, config, "engine/processors", "processor", processors_);
//! CreateComponentsFromList<Segmentor>(..., "engine/segmentors", ...);
//! CreateComponentsFromList<Translator>(..., "engine/translators", ...);
//! CreateComponentsFromList<Filter>(..., "engine/filters", ...);
//! ```
//!
//! 每个列表项有两种写法：
//!
//! * 字符串：`- ascii_composer`
//! * map：`- { name: xxx, when: <条件> }` —— `when` 是一个开关表达式，
//!   每次 option 变化都要重算组件的 `enabled`（`UpdateComponentConditions`）。
//!
//! # Rust 侧的取舍
//!
//! C++ 靠**静态注册表 + 宏**注册组件类（谁 link 进来谁就有）；Rust 这里用一个显式的
//! `match` 工厂 —— 没移植的组件返回 `None`，行为等同 C++ 里 `Require` 失败
//! （打日志 + 跳过该列表项），**不会让整个链条断掉**。

use std::path::Path;

use serde_json::Value;

use crate::engine::context::Context;
use crate::engine::gear::abc_segmentor::AbcSegmentor;
use crate::engine::gear::ascii_composer::AsciiComposer;
use crate::engine::gear::editor::Editor;
use crate::engine::gear::fallback_segmentor::FallbackSegmentor;
use crate::engine::gear::key_binder::KeyBinder;
use crate::engine::gear::filter::{Filter, SingleCharFilter, Uniquifier};
use crate::engine::gear::matcher::Matcher;
use crate::engine::gear::navigator::Navigator;
use crate::engine::gear::punctuator::{PunctSegmentor, PunctTranslator, Punctuator};
use crate::engine::gear::processor::Processor;
use crate::engine::gear::recognizer::Recognizer;
use crate::engine::gear::segmentor::Segmentor;
use crate::engine::gear::selector::Selector;
use crate::engine::gear::speller::Speller;
use crate::engine::gear::translator::{ScriptTranslator, Translator};

/// C++ `ParseCondition` 的递归深度上限（`engine.cc:kMaxConditionDepth`）。
const MAX_CONDITION_DEPTH: usize = 32;

/// C++ `Ticket`：创建组件时的身份证件。
///
/// * `klass` —— 类名（`- lua_processor@promote_candidate` 取 `@` 前的部分）
/// * `name_space` —— 完整 prescription（也就是列表项本身）
pub struct Ticket<'a> {
    pub klass: String,
    pub name_space: String,
    /// 编译后的方案配置树（`Schema::config`）。
    pub config: &'a Value,
    /// 运行时产物根目录（`build_dir`），translator 要在这里打开词典。
    pub build_dir: &'a Path,
}

impl<'a> Ticket<'a> {
    /// 从列表项的 prescription 里分出 `klass`（`@` 之前）与命名空间（整条）。
    fn from_prescription(prescription: &str, name_space: &str, config: &'a Value, build_dir: &'a Path) -> Self {
        let klass = prescription
            .split('@')
            .next()
            .unwrap_or(prescription)
            .to_string();
        Ticket {
            klass,
            name_space: name_space.to_string(),
            config,
            build_dir,
        }
    }
}

// --------------------------------------------------------------- Condition

/// C++ `Condition`：组件的启用开关表达式。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Condition {
    /// `- some_option` / `- !some_option` / `{ option: x, value: true }`
    Option { name: String, expected: bool },
    AllOf(Vec<Condition>),
    AnyOf(Vec<Condition>),
    NoneOf(Vec<Condition>),
}

impl Condition {
    /// C++ `ParseCondition`。
    pub fn parse(item: &Value, depth: usize) -> Option<Condition> {
        if depth > MAX_CONDITION_DEPTH {
            return None;
        }
        if let Some(s) = item.as_str() {
            let (name, expected) = match s.strip_prefix('!') {
                Some(rest) => (rest.to_string(), false),
                None => (s.to_string(), true),
            };
            if name.is_empty() {
                return None;
            }
            return Some(Condition::Option { name, expected });
        }
        let map = item.as_object()?;
        // `{ option: x, value: true }` —— 要求**恰好**这两个键
        if map.contains_key("option") {
            if !has_exact_keys(map, &["option", "value"]) {
                return None;
            }
            let name = map.get("option")?.as_str()?.to_string();
            let expected = map.get("value")?.as_bool()?;
            if name.is_empty() {
                return None;
            }
            return Some(Condition::Option { name, expected });
        }
        let (operation, variant) = if map.contains_key("all_of") {
            ("all_of", 0u8)
        } else if map.contains_key("any_of") {
            ("any_of", 1)
        } else if map.contains_key("none_of") {
            ("none_of", 2)
        } else {
            return None;
        };
        if !has_exact_keys(map, &[operation]) {
            return None;
        }
        let list = map.get(operation)?.as_array()?;
        if list.is_empty() {
            return None;
        }
        let mut children = Vec::with_capacity(list.len());
        for child in list {
            children.push(Condition::parse(child, depth + 1)?);
        }
        Some(match variant {
            0 => Condition::AllOf(children),
            1 => Condition::AnyOf(children),
            _ => Condition::NoneOf(children),
        })
    }

    /// C++ `EvaluateCondition`。
    pub fn evaluate(&self, ctx: &Context) -> bool {
        match self {
            Condition::Option { name, expected } => ctx.get_option(name) == *expected,
            Condition::AllOf(children) => children.iter().all(|c| c.evaluate(ctx)),
            Condition::AnyOf(children) => children.iter().any(|c| c.evaluate(ctx)),
            Condition::NoneOf(children) => !children.iter().any(|c| c.evaluate(ctx)),
        }
    }
}

fn has_exact_keys(
    map: &serde_json::Map<String, Value>,
    expected: &[&str],
) -> bool {
    map.len() == expected.len() && expected.iter().all(|k| map.contains_key(*k))
}

// ---------------------------------------------------------- ComponentEntry

/// C++ `ComponentEntry<T>`：组件 + 它的启用条件。
pub struct ComponentEntry<T> {
    pub component: T,
    /// 组件在方案列表里的 prescription（诊断 / 测试用：`processor_names()`）。
    pub name: String,
    condition: Condition,
    has_condition: bool,
    /// `update_enabled` 的缓存结果（无条件时恒真）。
    enabled: bool,
}

impl<T> ComponentEntry<T> {
    pub fn new(component: T, name: &str) -> Self {
        ComponentEntry {
            component,
            name: name.to_string(),
            condition: Condition::AnyOf(Vec::new()), // 未使用时的占位
            has_condition: false,
            enabled: true,
        }
    }

    pub fn with_condition(mut self, condition: Condition) -> Self {
        self.has_condition = true;
        self.condition = condition;
        self
    }

    /// C++ `UpdateComponentEnabled`：无条件 ⇒ 恒真；有条件 ⇒ 每次求值。
    pub fn update_enabled(&mut self, ctx: &Context) {
        self.enabled = !self.has_condition || self.condition.evaluate(ctx);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// 让调用方能在上下文变化后刷新（对照 `UpdateComponentConditions`）。
    pub fn refresh(&mut self, ctx: &Context) {
        self.update_enabled(ctx);
    }
}

// ------------------------------------------------------------ 列表解析

/// 列表项 → `(prescription, 可选条件)`（C++ `ParseComponentSpec`）。
fn parse_component_spec(item: &Value) -> Option<(String, Option<Condition>)> {
    if let Some(name) = item.as_str() {
        return Some((name.to_string(), None));
    }
    let map = item.as_object()?;
    if !map.keys().all(|k| k == "name" || k == "when") {
        return None;
    }
    let name = map.get("name")?.as_str()?.to_string();
    if name.is_empty() {
        return None;
    }
    let condition = match map.get("when") {
        Some(when) => Some(Condition::parse(when, 0)?),
        None => None,
    };
    Some((name, condition))
}

/// C++ `CreateComponentsFromList`：读 `<config_key>` 列表，逐项构造 + 解析条件。
///
/// `build_dir` 留给 translator（要在运行时产物目录里开词典）；processor 传空路径。
pub fn create_components_from_list<T, F>(
    config: &Value,
    build_dir: &Path,
    config_key: &str,
    component_type: &str,
    mut make: F,
) -> Vec<ComponentEntry<T>>
where
    F: FnMut(&Ticket) -> Option<T>,
{
    let mut out = Vec::new();
    let path = format!("/{}", config_key.replace('.', "/"));
    let Some(list) = config.pointer(&path).and_then(Value::as_array) else {
        return out;
    };
    for (index, item) in list.iter().enumerate() {
        let Some((prescription, condition)) = parse_component_spec(item) else {
            eprintln!(
                "[registry] invalid {component_type} prescription at index {index} in '{config_key}'"
            );
            continue;
        };
        let ticket = Ticket::from_prescription(&prescription, &prescription, config, build_dir);
        let Some(component) = make(&ticket) else {
            eprintln!("[registry] error creating {component_type}: '{prescription}'");
            continue;
        };
        let entry = match condition {
            Some(cond) => ComponentEntry::new(component, &prescription).with_condition(cond),
            None => ComponentEntry::new(component, &prescription),
        };
        out.push(entry);
    }
    out
}

// ------------------------------------------------------------ 组件工厂

/// C++ `gears_module.cc` 的 processor 部分（当前已移植的那些）。
/// C++ `gears_module.cc` 的 filter 部分（当前已移植的那些）。
pub fn create_filter(ticket: &Ticket) -> Option<Box<dyn Filter + Send + Sync>> {
    match ticket.klass.as_str() {
        "uniquifier" => Some(Box::new(Uniquifier::new())),
        "single_char" | "single_char_filter" => Some(Box::new(SingleCharFilter::new())),
        _ => None,
    }
}

pub fn create_processor(ticket: &Ticket) -> Option<Box<dyn Processor + Send + Sync>> {
    match ticket.klass.as_str() {
        "ascii_composer" => Some(Box::new(AsciiComposer::new(ticket.config))),
        "express_editor" => Some(Box::new(Editor::express(ticket.config))),
        "fluid_editor" | "fluency_editor" => Some(Box::new(Editor::fluid(ticket.config))),
        "key_binder" => Some(Box::new(KeyBinder::new(ticket.config))),
        "navigator" => Some(Box::new(Navigator::new(ticket.config))),
        "punctuator" => Some(Box::new(Punctuator::with_config(ticket.config, false))),
        "recognizer" => Some(Box::new(Recognizer::new(ticket.config, &ticket.name_space))),
        "selector" => Some(Box::new(Selector)),
        "speller" => Some(Box::new(Speller::new(ticket.config))),
        _ => None,
    }
}

/// C++ `gears_module.cc` 的 segmentor 部分（当前已移植的那些）。
pub fn create_segmentor(ticket: &Ticket) -> Option<Box<dyn Segmentor + Send + Sync>> {
    match ticket.klass.as_str() {
        "abc_segmentor" => Some(Box::new(AbcSegmentor::new(ticket.config))),
        "matcher" => Some(Box::new(Matcher::new(ticket.config, &ticket.name_space))),
        "fallback_segmentor" => Some(Box::new(FallbackSegmentor::new())),
        "punct_segmentor" => Some(Box::new(PunctSegmentor::with_config(ticket.config, false))),
        _ => None,
    }
}

/// C++ `gears_module.cc` 的 translator 部分（当前已移植的那些）。
pub fn create_translator(ticket: &Ticket) -> Option<Box<dyn Translator + Send + Sync>> {
    match ticket.klass.as_str() {
        "punct_translator" => Some(Box::new(PunctTranslator::with_config(ticket.config, false))),
        "script_translator" | "r10n_translator" => {
            let (dict_name, prism_name) = translator_dict_name(ticket)?;
            let dict = crate::dict::dictionary::Dictionary::open(
                ticket.build_dir,
                &dict_name,
                &prism_name,
            )?;
            Some(Box::new(ScriptTranslator::new(dict)))
        }
        _ => None,
    }
}

/// translator 的字典名：先查 `<prescription>` 命名空间，回退顶层 `translator:`
/// （librime 把 `script_translator` 的配置放在顶层，见 [`crate::engine::schema::Schema::first_translator_dict`]）。
fn translator_dict_name(ticket: &Ticket) -> Option<(String, String)> {
    if !ticket.name_space.is_empty() {
        // prescription 形如 `script_translator` 或 `translator@xxx`；librime 里
        // engine/translators 列的就是**命名空间名**，配置节点在 `/<ns>/dictionary`。
        if let Some(dict) = ticket
            .config
            .pointer(&format!("/{}/dictionary", ticket.name_space))
            .and_then(|v| v.as_str())
        {
            let prism = ticket
                .config
                .pointer(&format!("/{}/prism", ticket.name_space))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| dict.to_string());
            return Some((dict.to_string(), prism));
        }
    }
    if let Some(dict) = ticket.config.pointer("/translator/dictionary").and_then(|v| v.as_str()) {
        let prism = ticket
            .config
            .pointer("/translator/prism")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| dict.to_string());
        return Some((dict.to_string(), prism));
    }
    None
}

/// Wrapper[registry]方案里的四个列表都装配一遍（对照 `InitializeComponents`）。
pub fn install_engine_components(
    config: &Value,
    build_dir: &Path,
) -> (
    Vec<ComponentEntry<Box<dyn Processor + Send + Sync>>>,
    Vec<ComponentEntry<Box<dyn Segmentor + Send + Sync>>>,
    Vec<ComponentEntry<Box<dyn Translator + Send + Sync>>>,
    Vec<ComponentEntry<Box<dyn Filter + Send + Sync>>>,
) {
    let processors = create_components_from_list(
        config,
        build_dir,
        "engine/processors",
        "processor",
        create_processor,
    );
    let segmentors = create_components_from_list(
        config,
        build_dir,
        "engine/segmentors",
        "segmentor",
        create_segmentor,
    );
    let translators = create_components_from_list(
        config,
        build_dir,
        "engine/translators",
        "translator",
        create_translator,
    );
    let filters = create_components_from_list(
        config,
        build_dir,
        "engine/filters",
        "filter",
        create_filter,
    );
    (processors, segmentors, translators, filters)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mk_ctx(options: &[(&str, bool)]) -> Context {
        let mut c = Context::new();
        for (name, value) in options {
            c.set_option(name, *value);
        }
        c
    }

    #[test]
    fn parses_bang_option() {
        let v = json!("!ascii_mode");
        let cond = Condition::parse(&v, 0).expect("应能解析");
        assert_eq!(
            cond,
            Condition::Option {
                name: "ascii_mode".to_string(),
                expected: false
            }
        );
        let ctx = mk_ctx(&[]);
        assert!(cond.evaluate(&ctx), "ascii_mode 关着 ⇒ !ascii_mode 为真");
        let ctx = mk_ctx(&[("ascii_mode", true)]);
        assert!(!cond.evaluate(&ctx));
    }

    #[test]
    fn parses_nested_conditions() {
        let v = json!({"all_of": ["simplification", {"any_of": ["zh_trad", "!extended_charset"]}]});
        let cond = Condition::parse(&v, 0).expect("应能解析");
        assert!(!cond.evaluate(&mk_ctx(&[])));
        assert!(cond.evaluate(&mk_ctx(&[("simplification", true)])));
        assert!(cond.evaluate(&mk_ctx(&[("simplification", true), ("zh_trad", true)])));
        // none_of
        let v = json!({"none_of": ["a", "b"]});
        let cond = Condition::parse(&v, 0).expect("应能解析");
        assert!(cond.evaluate(&mk_ctx(&[])));
        assert!(!cond.evaluate(&mk_ctx(&[("a", true)])));
    }

    #[test]
    fn rejects_malformed_condition() {
        assert!(Condition::parse(&json!({}), 0).is_none());
        assert!(Condition::parse(&json!({"all_of": []}), 0).is_none());
        // 混合键不被允许（C++ HasExactKeys）
        assert!(Condition::parse(&json!({"all_of": ["a"], "any_of": ["b"]}), 0).is_none());
        assert!(Condition::parse(&json!({"option": "a"}), 0).is_none());
        assert!(Condition::parse(&json!(""), 0).is_none());
    }

    #[test]
    fn creates_processors_from_schema_list() {
        let config = json!({"engine": {"processors": [
            "ascii_composer",
            "speller",
            "some_unimplemented_thing",
            {"name": "navigator", "when": "!ascii_mode"}
        ]}});
        let list = create_components_from_list(
            &config,
            Path::new(""),
            "engine/processors",
            "processor",
            create_processor,
        );
        // 未移植的组件被跳过，链条不断
        assert_eq!(list.len(), 3);
        let mut e = list.into_iter().nth(2).expect("第三个");
        let mut ctx = mk_ctx(&[]);
        e.refresh(&ctx);
        assert!(e.is_enabled(), "!ascii_mode 为真 ⇒ 启用");
        ctx.set_option("ascii_mode", true);
        e.refresh(&ctx);
        assert!(!e.is_enabled());
    }

    #[test]
    fn unknown_klass_returns_none() {
        let config = json!({});
        let ticket = Ticket::from_prescription("lua_processor@foo", "lua_processor@foo", &config, Path::new(""));
        assert_eq!(ticket.klass, "lua_processor");
        assert!(create_processor(&ticket).is_none());
    }
}
