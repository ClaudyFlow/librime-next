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

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::engine::context::Context;
use crate::engine::gear::abc_segmentor::AbcSegmentor;
use crate::engine::gear::ascii_composer::AsciiComposer;
use crate::engine::gear::ascii_segmentor::AsciiSegmentor;
use crate::engine::gear::editor::Editor;
use crate::engine::gear::fallback_segmentor::FallbackSegmentor;
use crate::engine::gear::affix_segmentor::AffixSegmentor;
use crate::engine::gear::charset_filter::CharsetFilter;
use crate::engine::gear::chord_composer::ChordComposer;
use crate::engine::gear::echo_translator::EchoTranslator;
use crate::engine::gear::filter::{Filter, SingleCharFilter, Uniquifier};
use crate::engine::gear::history_translator::HistoryTranslator;
use crate::engine::gear::key_binder::KeyBinder;
use crate::engine::gear::matcher::Matcher;
use crate::engine::gear::navigator::Navigator;
use crate::engine::gear::poet::Poet;
use crate::engine::gear::processor::Processor;
use crate::engine::gear::punctuator::{PunctSegmentor, PunctTranslator, Punctuator};
use crate::engine::gear::recognizer::Recognizer;
use crate::engine::gear::reverse_lookup_filter::ReverseLookupFilter;
use crate::engine::gear::reverse_lookup_translator::ReverseLookupTranslator;
use crate::engine::gear::segmentor::Segmentor;
use crate::engine::gear::selector::Selector;
use crate::engine::gear::shape::ShapeProcessor;
use crate::engine::gear::simplifier::Simplifier;
use crate::engine::gear::speller::Speller;
use crate::engine::gear::streaming_chord_processor::StreamingChordProcessor;
use crate::engine::gear::table_translator::TableTranslator;
use crate::engine::gear::translator::{ScriptTranslator, Translator};
use crate::engine::gear::translator_options::TranslatorOptions;

/// C++ `ParseCondition` 的递归深度上限（`engine.cc:kMaxConditionDepth`）。
const MAX_CONDITION_DEPTH: usize = 32;

/// C++ `Ticket`：创建组件时的身份证件。
///
/// * `klass` —— 类名（`- lua_processor@promote_candidate` 取 `@` 前的部分）
/// * `name_space` —— 配置命名空间。**默认是组件类名**（`processor` / `segmentor` /
///   `translator` / `filter`），只有 `klass@alias` 的 `alias` 才会覆盖它
///   （`src/rime/ticket.cc:16-22`）。
///
/// # 为什么这条容易搞错
///
/// 直觉上 `- table_translator` 的命名空间该是 `table_translator`，于是去读
/// `table_translator/dict`；但方案文件里配置节点一律叫 `translator:`，
/// 所以 C++ 让命名空间**退回到组件类名**。形如 `- table_translator@my_table`
/// 才是显式指定读 `my_table/dict`。
pub struct Ticket<'a> {
    pub klass: String,
    pub name_space: String,
    /// 编译后的方案配置树（`Schema::config`）。
    pub config: &'a Value,
    /// 运行时产物根目录（`build_dir`），translator 要在这里打开词典。
    pub build_dir: &'a Path,
    /// 数据根列表，按 C++ `ResourceResolver` 的顺序：**用户目录优先、共享目录其次**。
    ///
    /// `opencc/` 与 `model/<语言>.gram` 不是编译产物，不在 `build_dir` 里，只能沿这串根找。
    pub data_roots: &'a [PathBuf],
}

impl<'a> Ticket<'a> {
    /// 从列表项的 prescription 里分出 `klass`（`@` 之前）与命名空间（`@` 之后的
    /// alias，没有就用 `component_type`）。
    fn from_prescription(
        component_type: &str,
        prescription: &str,
        config: &'a Value,
        build_dir: &'a Path,
        data_roots: &'a [PathBuf],
    ) -> Self {
        let (klass, alias) = match prescription.split_once('@') {
            Some((k, a)) => (k.to_string(), a.to_string()),
            None => (prescription.to_string(), String::new()),
        };
        let name_space = if alias.is_empty() {
            component_type.to_string()
        } else {
            alias
        };
        Ticket {
            klass,
            name_space,
            config,
            build_dir,
            data_roots,
        }
    }
}

// --------------------------------------------------------------- Condition

/// C++ `Condition`：组件的启用开关表达式。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Condition {
    /// `- some_option` / `- !some_option` / `{ option: x, value: true }`
    Option {
        name: String,
        expected: bool,
    },
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

fn has_exact_keys(map: &serde_json::Map<String, Value>, expected: &[&str]) -> bool {
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
/// `build_dir` 留给 translator（要在运行时产物目录里开词典）；
/// `data_roots` 留给需要外部数据目录的组件（`simplifier` 的 opencc、`script_translator` 的 `.gram`）。
pub fn create_components_from_list<T, F>(
    config: &Value,
    build_dir: &Path,
    data_roots: &[PathBuf],
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
        let ticket =
            Ticket::from_prescription(component_type, &prescription, config, build_dir, data_roots);
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
        // `charset_filter` 与它的别名 `cjk_minifier` 是同一个组件
        // （`gears_module.cc:85-88`：`if (!r.Find("charset_filter"))` 先让给改进版）。
        "charset_filter" | "cjk_minifier" => Some(Box::new(CharsetFilter::load(
            ticket.config,
            &ticket.name_space,
        ))),
        "reverse_lookup_filter" => Some(Box::new(ReverseLookupFilter::load(
            ticket.config,
            &ticket.name_space,
            ticket.build_dir,
        ))),
        "simplifier" => {
            // 配置节点：带 alias 时优先读 `alias:` 块，否则回退到 `simplifier:` 块
            // （与原版 `Simplifier` 读 `ticket.config` 的方式对齐）。
            let node = ticket
                .config
                .pointer(&format!("/{}", ticket.name_space))
                .or_else(|| ticket.config.pointer("/simplifier"));
            let config_name = node
                .and_then(|n| n.get("opencc_config"))
                .and_then(|v| v.as_str())
                .unwrap_or("t2s.json");
            let option_name = node
                .and_then(|n| n.get("option_name"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            // opencc 数据在 `<root>/opencc`。`build_dir` 在生产里是 `<用户目录>/build`，
            // 而 opencc 落在**共享目录** —— 所以必须按 roots 逐个试，不能只看 `build_dir.parent()`。
            for root in ticket.data_roots {
                if let Some(simplifier) =
                    Simplifier::new(&root.join(crate::lever::paths::OPENCC_DIR), config_name, option_name.clone())
                {
                    return Some(Box::new(simplifier));
                }
            }
            // 单根布局（roots 为空）时的回退：兼容直接构造 Ticket 的调用方。
            let opencc_dir = ticket.build_dir.parent()?.join(crate::lever::paths::OPENCC_DIR);
            Simplifier::new(&opencc_dir, config_name, option_name)
                .map(|s| Box::new(s) as Box<dyn Filter + Send + Sync>)
        }
        _ => None,
    }
}

pub fn create_processor(ticket: &Ticket) -> Option<Box<dyn Processor + Send + Sync>> {
    match ticket.klass.as_str() {
        "ascii_composer" => Some(Box::new(AsciiComposer::new(ticket.config))),
        // C++ `ChordComposerComponent::Create`（`chord_composer.cc:279-289`）是个**分派器**：
        // 方案里声明了 `streaming_chord` 段就造流式版，否则造经典版。两者对外都叫
        // `chord_composer`，配置段却是分开的（`chord_composer/` 与 `streaming_chord/`）。
        "chord_composer" => {
            if ticket.config.get("streaming_chord").is_some() {
                Some(Box::new(StreamingChordProcessor::new(ticket.config)))
            } else {
                Some(Box::new(ChordComposer::new(ticket.config)))
            }
        }
        "express_editor" => Some(Box::new(Editor::express(ticket.config))),
        "fluid_editor" | "fluency_editor" => Some(Box::new(Editor::fluid(ticket.config))),
        "key_binder" => Some(Box::new(KeyBinder::new(ticket.config))),
        "navigator" => Some(Box::new(Navigator::new(ticket.config))),
        "punctuator" => Some(Box::new(Punctuator::new(ticket.config))),
        "recognizer" => Some(Box::new(Recognizer::new(ticket.config, &ticket.name_space))),
        "selector" => Some(Box::new(Selector)),
        "shape_processor" => Some(Box::new(ShapeProcessor)),
        "speller" => Some(Box::new(Speller::new(ticket.config))),
        _ => None,
    }
}

/// C++ `gears_module.cc` 的 segmentor 部分（当前已移植的那些）。
pub fn create_segmentor(ticket: &Ticket) -> Option<Box<dyn Segmentor + Send + Sync>> {
    match ticket.klass.as_str() {
        "abc_segmentor" => Some(Box::new(AbcSegmentor::new(ticket.config))),
        "ascii_segmentor" => Some(Box::new(AsciiSegmentor::new())),
        "matcher" => Some(Box::new(Matcher::new(ticket.config, &ticket.name_space))),
        "fallback_segmentor" => Some(Box::new(FallbackSegmentor::new())),
        "punct_segmentor" => Some(Box::new(PunctSegmentor::new(ticket.config))),
        "affix_segmentor" => Some(Box::new(AffixSegmentor::load(
            ticket.config,
            &ticket.name_space,
        ))),
        _ => None,
    }
}

/// C++ `gears_module.cc` 的 translator 部分（当前已移植的那些）。
pub fn create_translator(ticket: &Ticket) -> Option<Box<dyn Translator + Send + Sync>> {
    let options = TranslatorOptions::from_config(ticket.config, &ticket.name_space);
    match ticket.klass.as_str() {
        "punct_translator" => Some(Box::new(PunctTranslator::new(ticket.config))),
        "echo_translator" => Some(Box::new(EchoTranslator::new())),
        "history_translator" => {
            // 命名空间改写（`translator` → `history`）发生在组件内部，
            // 所以这里把原始 `name_space` 原样递进去。
            Some(Box::new(HistoryTranslator::load(
                ticket.config,
                &ticket.name_space,
            )))
        }
        "reverse_lookup_translator" => {
            // 反查翻译器的命名空间默认就从 `translator` 改写成 `reverse_lookup`
            // （`reverse_lookup_translator.cc:62-64`），改名发生在组件内部，
            // 所以这里把原始 `name_space` 原样递进去。
            ReverseLookupTranslator::load(ticket.config, &ticket.name_space, ticket.build_dir)
                .map(|t| Box::new(t) as Box<dyn Translator + Send + Sync>)
        }
        "table_translator" => {
            let (dict_name, prism_name) = translator_dict_name(ticket)?;
            let dict = crate::dict::dictionary::Dictionary::open(
                ticket.build_dir,
                &dict_name,
                &prism_name,
            )?;
            let mut translator = TableTranslator::new(dict, options);
            // 学词时顺带编码"历史上连续几个词"（`table_translator.cc:222`，默认 true）
            let pointer = |key: &str| format!("/{}/{key}", ticket.name_space);
            if let Some(v) = ticket
                .config
                .pointer(&pointer("encode_commit_history"))
                .and_then(|v| v.as_bool())
            {
                translator.encode_commit_history = v;
            }
            if let Some(v) = ticket
                .config
                .pointer(&pointer("max_phrase_length"))
                .and_then(|v| v.as_i64())
            {
                translator.max_phrase_length = v as i32;
            }
            // 组句三开关（`table_translator.cc:218-224`）。
            // `enable_sentence` 默认 **true**、`sentence_over_completion` 默认 **false**、
            // `max_homographs` 默认 **1**（每格只留最优词）。
            if let Some(v) = ticket
                .config
                .pointer(&pointer("enable_sentence"))
                .and_then(|v| v.as_bool())
            {
                translator.enable_sentence = v;
            }
            if let Some(v) = ticket
                .config
                .pointer(&pointer("sentence_over_completion"))
                .and_then(|v| v.as_bool())
            {
                translator.sentence_over_completion = v;
            }
            if let Some(v) = ticket
                .config
                .pointer(&pointer("max_homographs"))
                .and_then(|v| v.as_i64())
            {
                translator.max_homographs = v.max(1) as usize;
            }
            // `enable_charset_filter` 默认 **false**，但 wubi86 / cangjie5 /
            // cangjie5_express 出厂都开了 —— 不开的话 `wqvbwq` 会冒出「𠛈」。
            if let Some(v) = ticket
                .config
                .pointer(&pointer("enable_charset_filter"))
                .and_then(|v| v.as_bool())
            {
                translator.enable_charset_filter = v;
            }
            // 语法模型：与 `script_translator` 同一套 `model:` 解析。
            // 形码方案（wubi86 / cangjie5）出厂都没配 model，走动态规划。
            let roots: Vec<&Path> = ticket.data_roots.iter().map(PathBuf::as_path).collect();
            let section = translator_section(&ticket.name_space);
            if let Some(view) = grammar_view(ticket.config, &section) {
                if let Some(octagram) = crate::engine::gear::grammar::create(&view, &roots) {
                    translator = translator.with_poet(Poet::with_left_associative(Some(Box::new(
                        octagram,
                    ))));
                }
            }
            // 用户词典（学词 / 调频）：C++ 里是 `Translator` 基类的 `user_dict_`
            if let Some(user_dict) = create_user_dictionary(ticket, &dict_name) {
                translator = translator.with_user_dict(user_dict);
            }
            // 规则编码器：`enable_encoder_ && user_dict_`（`table_translator.cc:231`），
            // 且默认 **false** —— 只有仓颉/郑码这类"按公式取码"的方案才开。
            let enable_encoder = ticket
                .config
                .pointer(&format!("/{}/enable_encoder", ticket.name_space))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if enable_encoder && translator.user_dict.is_some() {
                if let Some(encoder) =
                    crate::engine::gear::unity_table_encoder::UnityTableEncoder::load(
                        ticket.build_dir,
                        &dict_name,
                    )
                {
                    translator = translator.with_encoder(encoder);
                }
            }
            Some(Box::new(translator))
        }
        "script_translator" | "r10n_translator" => {
            let (dict_name, prism_name) = translator_dict_name(ticket)?;
            // 词库**单文件直接用**：`translator/dict` 指到哪本就编译哪本，
            // 不做任何 opencc 处理（既不部署期生成、也不运行时转换）。
            // 带 `.simp` / `.trad` 标记的是变体词库，同样各自一份现成文件。
            let dict = crate::dict::dictionary::Dictionary::open(
                ticket.build_dir,
                &dict_name,
                &prism_name,
            )?;
            let mut translator = ScriptTranslator::with_options(dict, options);
            // 语法模型：只认 `model:`（`translator/simp/model: han.simp` →
            // `model/han.simp.gram`）。没有就不装，`Poet` 走动态规划。
            let roots: Vec<&Path> = ticket.data_roots.iter().map(PathBuf::as_path).collect();
            let section = translator_section(&ticket.name_space);
            if let Some(view) = grammar_view(ticket.config, &section) {
                if let Some(octagram) = crate::engine::gear::grammar::create(&view, &roots) {
                    translator = translator.with_poet(Poet::new(Some(Box::new(octagram))));
                }
            }
            // 用户词典（学词 / 调频）：`Translator::user_dict_`
            if let Some(user_dict) = create_user_dictionary(ticket, &dict_name) {
                translator = translator.with_user_dict(user_dict);
            }
            Some(Box::new(translator))
        }
        _ => None,
    }
}

/// C++ `UserDictionaryComponent::Create(ticket)`（`user_dictionary.cc:595-620`）。
///
/// 取名字的顺序照抄：
///
/// 1. `enable_user_dict` 默认 **true**，显式 `false` 就不建；
/// 2. 名字先看 `user_dict`，没有再取 `dictionary` 并**去掉 `.extra` 后缀**
///    （`{dictionary: luna_pinyin.extra}` 隐含 `{user_dict: luna_pinyin}`，
///    C++ 用 `Language::get_language_component` 干这事）；
/// 3. `db_class` 默认 `userdb`；只实现了 userdb 后端，别的类名直接不建。
///
/// 目录是 `<用户文件夹>/userdb/<名字>.userdb`（`lever::paths::Layout::user_db_dir`）。
fn create_user_dictionary(
    ticket: &Ticket,
    dict_name: &str,
) -> Option<crate::dict::user_dictionary::UserDictionary> {
    let pointer = |key: &str| format!("/{}/{key}", ticket.name_space);
    let enable = ticket
        .config
        .pointer(&pointer("enable_user_dict"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if !enable {
        return None;
    }
    let db_class = ticket
        .config
        .pointer(&pointer("db_class"))
        .and_then(|v| v.as_str())
        .unwrap_or("userdb");
    if db_class != "userdb" {
        eprintln!("[registry] unsupported user db class '{db_class}', skipping user dict.");
        return None;
    }
    let name = ticket
        .config
        .pointer(&pointer("user_dict"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| language_component(dict_name));
    // `data_roots[0]` = 用户文件夹（见 `c_api::Global::data_roots`）
    let user_dir = ticket.data_roots.first()?;
    let db_dir = user_dir.join(crate::lever::paths::USERDB_DIR);
    if let Err(e) = std::fs::create_dir_all(&db_dir) {
        eprintln!("[registry] cannot create userdb dir {db_dir:?}: {e}");
        return None;
    }
    let db = std::sync::Arc::new(crate::dict::level_db::LevelDb::new_user_db(&db_dir, &name));
    let mut user_dict = crate::dict::user_dictionary::UserDictionary::new(&name, db);
    if !user_dict.load() {
        eprintln!("[registry] failed to load user dict '{name}', continuing without it.");
        return None;
    }
    Some(user_dict)
}

/// `Language::get_language_component(name)`：砍掉第一个 `.` 及其后缀。
///
/// `luna_pinyin.extra` → `luna_pinyin`；`luna_pinyin` → `luna_pinyin`。
fn language_component(name: &str) -> String {
    match name.split_once('.') {
        Some((head, _)) => head.to_string(),
        None => name.to_string(),
    }
}

/// 语言模型（octagram / `Poet`）的配置视图：`<section>/model`（如 `zh-han.simp`）
/// → `{ "grammar": { "language": "<前缀>", … } }`。
///
/// `model:` 写的是**前缀**：引擎按 `paths.rs` 的 `.gram` 规则扫
/// `<数据根>/model/<前缀>*.gram`（用户目录优先、其次程序文件夹，按 id 平铺不建
/// 系列夹），**全部挂载**，查询取各本最高分。
/// 模型名规则：`zh-han.<simp|trad>.<语料>-<级别>`，如
/// `zh-han.simp.essay-bgc`（字级 29 MB）/`zh-han.trad.essay-bgw`（词级 208 MB）；
/// 写全名则只挂那一本。
///
/// 2026-10-28 起 **`model:` 是唯一写法**：旧的顶层 `grammar/language`（以及
/// `model.yaml` 的 `hans`/`hant` preset、`zh-hans-t-essay-bgw` 那串官方名）
/// 都不再识别。没有 `model:` 就不装 `Poet`，走动态规划。
///
/// 调参（惩罚系数等）可写在同一个节下的 `grammar:` 映射里，覆盖在 `language` 之上。
fn grammar_view(config: &Value, section: &str) -> Option<Value> {
    let model = config
        .pointer(&format!("{section}/model"))?
        .as_str()?
        .to_string();
    let mut node = serde_json::Map::new();
    node.insert("language".to_string(), Value::String(model));
    if let Some(tuning) = config
        .pointer(&format!("{section}/grammar"))
        .and_then(Value::as_object)
    {
        for (key, value) in tuning {
            node.insert(key.clone(), value.clone());
        }
    }
    Some(serde_json::json!({ "grammar": Value::Object(node) }))
}

/// 变体翻译器（simp/trad）配置**只能**挂在 `translator/` 下（Rime-Next 双变体设计，
/// 2026-10-28）：`translator/simp`、`translator/trad`。给定命名空间，返回它实际
/// 读配置用的 JSON 指针前缀。
///
/// 不设任何回退：旧式顶层 `simp:` / `trad:` 一节不再识别，写错位置即视为无此变体。
fn translator_section(name_space: &str) -> String {
    if name_space == "simp" || name_space == "trad" {
        return format!("/translator/{name_space}");
    }
    format!("/{name_space}")
}

/// translator 的字典名：C++ `Dictionary::Create` 只读 `ticket.name_space/dictionary`
/// （`src/rime/dict/dictionary.cc:430`），读不到就组件创建失败。
///
/// Rime-Next 的键是单数 **`dict`**（2026-10-28 20:5x 定，与 `id`/`column` 同一套方言）；
/// **不再认** `dictionary:`（兼容已按要求删除）。
///
/// 因为 [`Ticket::name_space`] 默认是组件类名 `translator`，这条实际读的是方案里
/// 的 `translator/dict`，与 `translator:` 节点吻合。`prism` 不给时与词典同名
/// （librime 里拼音方案常另行指定，如 `luna_pinyin` + `luna_pinyin_simp`）。
///
/// 变体节 simp:/trad: 经 [`translator_section`] 解析到 `translator/<ns>`。
fn translator_dict_name(ticket: &Ticket) -> Option<(String, String)> {
    let section = translator_section(&ticket.name_space);
    let dict = ticket
        .config
        .pointer(&format!("{section}/dict"))
        .and_then(|v| v.as_str())?;
    let prism = ticket
        .config
        .pointer(&format!("{section}/prism"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| dict.to_string());
    Some((dict.to_string(), prism))
}

/// Rime-Next 双变体扩展（2026-10-28）：`translator:` 下挂 `simp:`/`trad:` 子模块时，
/// 引擎自动识别并把 `script_translator` 拆成两个按繁简开关切换的变体翻译器，无需在
/// `engine/translators` 里显式写 `script_translator@simp/@trad`。
///
/// 返回**修改后的配置副本**（变体节已并入父 `translator` 的共享配置、列表已替换）；
/// 非变体方案返回 `None`（原样使用）。
fn synthesize_variant_translators(config: &Value) -> Option<Value> {
    let translator = config.pointer("/translator")?;
    // 变体由 `simp`/`trad` 键的存在决定（不再要求变体里写 dict）：
    // 这样 dict/prism 等共享配置可提到 translator 顶层，变体只写各自不同的（如 model）。
    let is_variant = ["simp", "trad"].iter().any(|v| translator.get(v).is_some());
    if !is_variant {
        return None;
    }
    let mut cfg = config.clone();
    // 变体节并入父 `translator` 的共享配置（dict / prism / preedit_format / delimiter …），
    // 变体自身只写各自不同的（如 model），深合并到父层之上。
    if let Some(translator) = cfg.pointer_mut("/translator") {
        if let Some(obj) = translator.as_object_mut() {
            let mut parent = serde_json::Value::Object(serde_json::Map::new());
            for (k, v) in obj.iter() {
                if k != "simp" && k != "trad" {
                    parent[k.clone()] = v.clone();
                }
            }
            for v in ["simp", "trad"] {
                if let Some(variant) = obj.get(v).cloned() {
                    let mut merged = parent.clone();
                    deep_merge(&mut merged, &variant);
                    obj.insert(v.to_string(), merged);
                }
            }
        }
    }
    // 列表里去掉 script_translator / r10n_translator（含 @simp/@trad），统一用变体替换。
    let list = config
        .pointer("/engine/translators")
        .and_then(serde_json::Value::as_array)?;
    let mut out: Vec<serde_json::Value> = list
        .iter()
        .filter(|item| {
            let prescription = match item.as_str() {
                Some(s) => s.to_string(),
                None => item
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            };
            let (klass, _) = match prescription.split_once('@') {
                Some((k, _)) => (k, ""),
                None => (prescription.as_str(), ""),
            };
            !(klass == "script_translator" || klass == "r10n_translator")
        })
        .cloned()
        .collect();
    // 繁简开关名：优先 `simplifier/option_name`（方案显式声明，如 flypy 的 zh_simp）；
    // 否则从 `switch:` 列表里挑一个简繁开关（zh_hant / zh_simp / simplification）——
    // 这样方案只要在 switch 里声明 `zh_hant`，就不必再在 simplifier 里重复一遍名字。
    // 都找不到才回退到 rime 传统的 `simplification`。
    let switch = config
        .pointer("/simplifier/option_name")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .or_else(|| {
            config.pointer("/switch").and_then(Value::as_array).and_then(|arr| {
                arr.iter().find_map(|s| {
                    s.get("name")
                        .and_then(Value::as_str)
                        .filter(|n| matches!(n, &"zh_hant" | &"zh_simp" | &"simplification"))
                        .map(|n| n.to_string())
                })
            })
        })
        .unwrap_or_else(|| "simplification".to_string());
    out.push(serde_json::json!({
        "name": "script_translator@simp",
        "when": switch.clone()
    }));
    out.push(serde_json::json!({
        "name": "script_translator@trad",
        "when": format!("!{switch}")
    }));
    if let Some(translators) = cfg.pointer_mut("/engine/translators") {
        *translators = serde_json::Value::Array(out);
    }
    Some(cfg)
}

/// 深合并：对象递归合并，标量/数组整体覆盖（C++ 对应 `Config::Merge` 的语义）。
fn deep_merge(base: &mut serde_json::Value, overlay: &serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(b), serde_json::Value::Object(o)) => {
            for (k, v) in o {
                deep_merge(b.entry(k.clone()).or_insert(serde_json::Value::Null), v);
            }
        }
        (base, overlay) => {
            *base = overlay.clone();
        }
    }
}

/// Wrapper[registry]方案里的四个列表都装配一遍（对照 `InitializeComponents`）。
pub fn install_engine_components(
    config: &Value,
    build_dir: &Path,
    data_roots: &[PathBuf],
) -> (
    Vec<ComponentEntry<Box<dyn Processor + Send + Sync>>>,
    Vec<ComponentEntry<Box<dyn Segmentor + Send + Sync>>>,
    Vec<ComponentEntry<Box<dyn Translator + Send + Sync>>>,
    Vec<ComponentEntry<Box<dyn Filter + Send + Sync>>>,
) {
    let processors = create_components_from_list(
        config,
        build_dir,
        data_roots,
        "engine/processors",
        "processor",
        create_processor,
    );
    let segmentors = create_components_from_list(
        config,
        build_dir,
        data_roots,
        "engine/segmentors",
        "segmentor",
        create_segmentor,
    );
    let translators = {
        let variant_cfg = synthesize_variant_translators(config);
        let cfg = variant_cfg.as_ref().unwrap_or(config);
        create_components_from_list(
            cfg,
            build_dir,
            data_roots,
            "engine/translators",
            "translator",
            create_translator,
        )
    };
    let filters = create_components_from_list(
        config,
        build_dir,
        data_roots,
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
    fn synthesizes_variant_translators_for_nested_simp_trad() {
        // 模拟 pinyin / double_pinyin 的嵌套 translator.simp/trad 布局：引擎自动识别
        // 变体子项，把 script_translator 拆成 @simp/@trad，并把父 translator 的共享
        // 配置（preedit_format）并入变体节。
        let config = json!({
            "engine": { "translators": [
                "punct_translator",
                "script_translator",
                "reverse_lookup_translator"
            ]},
            "translator": {
                "preedit_format": ["xlit/ab/AB/"],
                "simp": { "dict": "x.simp", "prism": "x", "model": "han.simp" },
                "trad": { "dict": "x.trad", "prism": "x", "model": "han.trad" }
            }
        });
        let synthesized = synthesize_variant_translators(&config).expect("应识别变体");
        // 列表：去掉 script_translator，注入 @simp/@trad
        let list = synthesized
            .pointer("/engine/translators")
            .and_then(Value::as_array)
            .expect("应有 translators 列表");
        let names: Vec<String> = list
            .iter()
            .map(|i| {
                if let Some(s) = i.as_str() {
                    s.to_string()
                } else {
                    i.get("name").and_then(Value::as_str).unwrap_or("?").to_string()
                }
            })
            .collect();
        assert_eq!(
            names,
            vec![
                "punct_translator",
                "reverse_lookup_translator",
                "script_translator@simp",
                "script_translator@trad"
            ]
        );
        // 变体节并入父 translator 的 preedit_format
        assert_eq!(
            synthesized.pointer("/translator/simp/preedit_format"),
            Some(&json!(["xlit/ab/AB/"]))
        );
        // 变体自身的 dict 保留
        assert_eq!(
            synthesized.pointer("/translator/simp/dict").and_then(Value::as_str),
            Some("x.simp")
        );
        // 条件取方案的 `simplifier/option_name`，否则从 `switch:` 列表推断
        // （zh_hant / zh_simp / simplification），都无则回退 `simplification`。
        let simp_when = synthesized
            .pointer("/engine/translators/2/when")
            .expect("simp 应有 when 条件");
        assert!(Condition::parse(simp_when, 0).is_some());
    }

    #[test]
    fn variant_switch_inferred_from_switch_list_when_no_option_name() {
        // 2026-10-28：方案只要在 switch 里声明 `zh_hant`，引擎就直接绑定变体 when，
        // 不必再写 `simplifier/option_name`（那是给 flypy 之类想用别的开关名时显式覆盖用的）。
        let config = json!({
            "switch": [
                { "name": "ascii_mode", "state": ["中文", "西文"], "reset": 0 },
                { "name": "zh_hant", "state": ["簡體", "繁體"], "reset": 0 }
            ],
            "engine": { "translators": [
                "punct_translator",
                "script_translator",
                "reverse_lookup_translator"
            ]},
            "translator": {
                "dict": "pinyin.luna",
                "prism": "pinyin",
                "simp": { "model": "zh-han.simp" },
                "trad": { "model": "zh-han.trad" }
            }
        });
        let synthesized = synthesize_variant_translators(&config).expect("应识别变体");
        let simp_when = synthesized
            .pointer("/engine/translators/2/when")
            .and_then(Value::as_str)
            .expect("simp 应有 when 条件");
        let trad_when = synthesized
            .pointer("/engine/translators/3/when")
            .and_then(Value::as_str)
            .expect("trad 应有 when 条件");
        assert_eq!(simp_when, "zh_hant");
        assert_eq!(trad_when, "!zh_hant");
        // 共享层 dict/prism 由深合并并进变体
        assert_eq!(
            synthesized.pointer("/translator/simp/dict").and_then(Value::as_str),
            Some("pinyin.luna")
        );
        assert_eq!(
            synthesized.pointer("/translator/trad/prism").and_then(Value::as_str),
            Some("pinyin")
        );
    }

    #[test]
    fn model_key_is_the_only_way_to_name_the_language_model() {
        // 2026-10-28：语言模型只认 `model:`（→ model/<名>.gram），
        // 旧的 `grammar/language` 与 model.yaml 的 hans/hant 预设不再识别。
        let config = json!({
            "translator": {
                "simp": { "model": "zh-han.simp.essay-bgc" },
                "trad": { "model": "zh-han.trad.essay-bgc" }
            }
        });
        assert_eq!(
            grammar_view(&config, "/translator/simp"),
            Some(json!({ "grammar": { "language": "zh-han.simp.essay-bgc" } }))
        );
        assert_eq!(
            grammar_view(&config, "/translator/trad"),
            Some(json!({ "grammar": { "language": "zh-han.trad.essay-bgc" } }))
        );
        // 只有 `grammar/language` 而没有 `model:` → 不装模型
        let legacy = json!({ "grammar": { "language": "zh-hans-t-essay-bgw" } });
        assert_eq!(grammar_view(&legacy, "/translator"), None);
        // 调参写在 `<section>/grammar` 里，覆盖在 language 之上
        let tuned = json!({
            "translator": {
                "model": "zh-han.simp.essay-bgc",
                "grammar": { "collocation_max_length": 5 }
            }
        });
        assert_eq!(
            grammar_view(&tuned, "/translator"),
            Some(json!({ "grammar": {
                "language": "zh-han.simp.essay-bgc",
                "collocation_max_length": 5
            } }))
        );
    }

    #[test]
    fn legacy_top_level_variant_is_not_recognized() {
        // 不许兼容：旧式顶层 `simp:` / `trad:` 一节不再被识别——既不合成变体翻译器，
        // 也不回落到顶层读配置。
        let config = json!({
            "engine": { "translators": ["script_translator"] },
            "translator": { "dict": "luna_pinyin", "prism": "x" },
            "simp": { "dict": "x.simp" },
            "trad": { "dict": "x.trad" }
        });
        assert!(
            synthesize_variant_translators(&config).is_none(),
            "顶层 simp/trad 不应被识别为变体"
        );
        assert_eq!(translator_section("simp"), "/translator/simp");
        assert_eq!(translator_section("trad"), "/translator/trad");
        assert_eq!(translator_section("translator"), "/translator");
    }

    #[test]
    fn variant_switch_follows_simplifier_option_name() {
        // 繁简开关名跟方案的 simplifier/option_name 走（luna 家族是 zh_simp）
        let config = json!({
            "engine": { "translators": ["script_translator"] },
            "translator": {
                "simp": { "dict": "x.simp" },
                "trad": { "dict": "x.trad" }
            },
            "simplifier": { "option_name": "zh_simp" }
        });
        let synthesized = synthesize_variant_translators(&config).expect("应识别变体");
        assert_eq!(
            synthesized.pointer("/engine/translators/0/when").and_then(Value::as_str),
            Some("zh_simp")
        );
        assert_eq!(
            synthesized.pointer("/engine/translators/1/when").and_then(Value::as_str),
            Some("!zh_simp")
        );
    }

    #[test]
    fn no_synthesis_for_non_variant_schema() {
        let config = json!({
            "engine": { "translators": ["script_translator"] },
            "translator": { "dict": "luna_pinyin", "prism": "x" }
        });
        assert!(
            synthesize_variant_translators(&config).is_none(),
            "非变体方案不合成"
        );
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
            &[],
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
        let ticket = Ticket::from_prescription(
            "processor",
            "lua_processor@foo",
            &config,
            Path::new(""),
            &[],
        );
        assert_eq!(ticket.klass, "lua_processor");
        assert_eq!(ticket.name_space, "foo", "@ 后的 alias 覆盖默认命名空间");
        assert!(create_processor(&ticket).is_none());
    }

    /// `charset_filter` 与它的别名 `cjk_minifier` 是同一个组件（`gears_module.cc:85-88`）。
    #[test]
    fn charset_filter_is_registered_under_both_names() {
        let config = json!({"filter": {"tags": ["abc"]}});
        for name in ["charset_filter", "cjk_minifier"] {
            let ticket =
                Ticket::from_prescription("filter", name, &config, Path::new(""), &[]);
            assert!(
                create_filter(&ticket).is_some(),
                "{name} 应能构造（别名也要认）"
            );
        }
    }

    /// C++ `Ticket(engine, ns, prescription)`：命名空间**默认是组件类名**，
    /// 所以方案里 `- table_translator` 读的是 `translator/dict`。
    #[test]
    fn name_space_defaults_to_component_type() {
        let config = json!({});
        let t = Ticket::from_prescription(
            "translator",
            "table_translator",
            &config,
            Path::new(""),
            &[],
        );
        assert_eq!(t.klass, "table_translator");
        assert_eq!(t.name_space, "translator");

        let t = Ticket::from_prescription("processor", "recognizer", &config, Path::new(""), &[]);
        assert_eq!(t.name_space, "processor");
    }

    /// 修这条之前的 bug：命名空间被写成整条 prescription，于是去读
    /// `table_translator/dict`（方案里根本没有这个节点）。
    #[test]
    fn table_translator_reads_the_translator_node() {
        let config = json!({"translator": {"dict": "stroke", "prism": "stroke"}});
        let t = Ticket::from_prescription(
            "translator",
            "table_translator",
            &config,
            Path::new(""),
            &[],
        );
        assert_eq!(
            translator_dict_name(&t),
            Some(("stroke".to_string(), "stroke".to_string()))
        );
    }
}
