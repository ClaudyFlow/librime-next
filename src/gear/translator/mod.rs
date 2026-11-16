//! 翻译器（Translator 链路）。

pub mod echo_translator;
pub mod history_translator;
pub mod reverse_lookup_translator;
pub mod switch_translator;
pub mod table_translator;
pub mod translation;
pub mod translator;
pub mod translator_options;
pub mod unity_table_encoder;

// 同名叶子模块 `translator` 的导出项在 `gear/translator/translator.rs`，再导出一层。
// `input_type_option_names` 是 `pub(crate)`，引用走全路径 `crate::gear::translator::translator::input_type_option_names`，无需再导出。
pub use translator::{ScriptTranslator, Translator};
