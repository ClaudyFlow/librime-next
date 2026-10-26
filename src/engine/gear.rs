//! `src/rime/gear/`：一次按键的核心链路（处理器 / 翻译器 / 选择器）。
//!
//! 对照 C++ `src/rime/gear/`：Processor（按键处理器）/ Translator（翻译器）/
//! Selector（候选导航）。为控制范围，本目录先把**主链路**做实：
//! * `processor` —— `ProcessResult` 与 `Processor` trait
//! * `translator` —— `Translator` trait + `ScriptTranslator`（拼音分词 + 查表）
//! * `speller`   —— 主处理器：可打印键入栈 / 退格 / Esc / 空格回车提交 / 数字选词
//! * `selector`  —— 上下 / 翻页导航高亮

//! 本目录按 C++ `src/rime/gear/` 的划分对齐：
//! * `processor`   —— `ProcessResult` 与 `Processor` trait
//! * `registry`    —— `gear/gears_module.cc` 的组件工厂 + `engine.cc` 的 `Condition` 装配
//! * `key_binding` —— `gear/key_binding_processor.h`：`KeyEvent → action` 绑定表
//! * `ascii_composer` —— 中西文切换（含单击 Shift / 临时西文）
//! * `editor`      —— `express_editor` / `fluid_editor`（空格确认、回车上屏原始串…）
//! * `navigator`   —— 插入符导航（左右 / Home / End / 音节跳）
//! * `translator`  —— `Translator` trait + `ScriptTranslator`（拼音分词 + 查表）
//! * `speller`     —— 主处理器：可打印键入栈 / 退格 / Esc / 空格回车提交 / 数字选词
//! * `selector`    —— 上下 / 翻页导航高亮

pub mod abc_segmentor;
pub mod ascii_composer;
pub mod ascii_segmentor;
pub mod editor;
pub mod fallback_segmentor;
pub mod filter;
pub mod grammar;
pub mod key_binder;
pub mod key_binding;
pub mod matcher;
pub mod navigator;
pub mod poet;
pub mod processor;
pub mod punctuator;
pub mod recognizer;
pub mod registry;
pub mod reverse_lookup_filter;
pub mod reverse_lookup_translator;
pub mod segmentor;
pub mod selector;
pub mod simplifier;
pub mod speller;
pub mod table_translator;
pub mod translator;
pub mod translator_options;
pub mod unity_table_encoder;
