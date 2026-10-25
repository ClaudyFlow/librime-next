//! `src/rime/gear/`：一次按键的核心链路（处理器 / 翻译器 / 选择器）。
//!
//! 对照 C++ `src/rime/gear/`：Processor（按键处理器）/ Translator（翻译器）/
//! Selector（候选导航）。为控制范围，本目录先把**主链路**做实：
//! * `processor` —— `ProcessResult` 与 `Processor` trait
//! * `translator` —— `Translator` trait + `ScriptTranslator`（拼音分词 + 查表）
//! * `speller`   —— 主处理器：可打印键入栈 / 退格 / Esc / 空格回车提交 / 数字选词
//! * `selector`  —— 上下 / 翻页导航高亮

pub mod processor;
pub mod selector;
pub mod speller;
pub mod translator;
