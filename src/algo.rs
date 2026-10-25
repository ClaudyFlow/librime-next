//! librime `src/rime/algo/`（16 个文件，1,829 行）的 Rust 移植 —— **门面**。
//!
//! | C++ 文件 | 行数 | Rust 模块 | 说明 |
//! |---|---|---|---|
//! | `algebra.h` + `algebra.cc` | 43 + 142 | [`algebra`] | `Script` / `Projection`，规则引擎 |
//! | `calculus.h` + `calculus.cc` | 112 + 372 | [`calculus`] | 7 种 `Calculation` + `boost::regex` 替身 |
//! | `syllabifier.h` + `syllabifier.cc` | 78 + 327 | [`syllabifier`] | `SyllableGraph` / 切分 / 剪枝 / 转置 |
//! | `spelling.h` + `spelling.cc` | 50 + 41 | [`spelling`] | `Spelling` / `SpellingProperties` |
//! | `encoder.h` + `encoder.cc` | 118 + 336 | [`encoder`] | `TableEncoder` / `ScriptEncoder` |
//! | `strings.h` + `strings.cc` | 45 + 38 | [`strings`] | `split`（两种行为）/ `join` |
//! | `utilities.h` + `utilities.cc` | 37 + 53 | [`utilities`] | 版本比较 / CRC32 |
//! | `dynamics.h` | 18 | [`dynamics`] | `formula_d` / `formula_p` |
//! | `fs.h` | 19 | [`utilities::to_time_t`] | `filesystem::to_time_t` |
//!
//! ## 与 C++ 的接口差异（都是"接缝"而非语义）
//!
//! * **规则来源**：C++ 的 `Projection::Load(an<ConfigList>)` 吃 `Config*`；
//!   这里主入口是 [`algebra::Projection::from_rules`]（吃 `&[String]`），
//!   配置树那条路留成薄包装 [`algebra::Projection::from_config`] /
//!   [`algebra::Projection::from_config_path`]（树类型 = `src/config.rs` 用的 `serde_json::Value`）。
//! * **`Prism` / `Corrector`**：`syllabifier` 在 C++ 里依赖 `dict/Prism`、`dict/Corrector`，
//!   这两层还没移植，所以抽成 trait（[`syllabifier::Prism`]、[`syllabifier::Corrector`]）
//!   并给了一个纯内存实现 [`syllabifier::InMemoryPrism`]（行为对齐 `Prism::Build`）。
//! * **`boost::regex`**：本 crate 不许加依赖，所以 [`calculus::regex`] 手写了一个
//!   按字节匹配的 Boost 子集引擎（`xform` / `erase` / `exclude_patterns` 都用它）。
//!   子集之外的语法（环视、命名组、`(?i)`、懒惰/占有量词、POSIX 字符类、`\p{..}` …）
//!   **编译期直接报 [`calculus::RuleError`]**（带规则原文 + 出错片段 + 字节位置），
//!   不静默降级。真实方案的 `speller/algebra` 里一条都没用到（`test/algebra_real_rules.rs`）。
//!
//! ## 移植时踩到的"必须照抄"的点
//!
//! 每条都写进了对应模块的文档与被钉住的测试里，最值钱的 6 条：
//!
//! 1. `Spelling::operator==` 只比 `str`，属性不参与（[`spelling`]）；
//! 2. `Compose` 取**类型最大值**、`Update` 取**最小值**，方向相反（[`spelling`]）；
//! 3. `Projection::Apply(Script)` 每轮用 `Merge` 重建整张表，撞键时 `Update` 会**清空 tips**（[`algebra`]）；
//! 4. `Calculus::Parse` 的分隔符是"第一个非小写字母"，不是 `/`（[`calculus`]）；
//! 5. 音节图剪枝的 `last_type` 下限是 `kFuzzySpelling`，且循环里**不再更新**（[`syllabifier`]）；
//! 6. `TableEncoder::ParseFormula` 的字母表在 `U`/`u` 处**断裂**成负数下标（[`encoder`]）。
#![allow(dead_code)]

// 注：algebra / encoder / strings / utilities 对 dict 层公开（`dict/` 的
// tsv / entry_collector / dict_compiler 按 C++ 的跨目录引用走这些路径），
// spelling / syllabifier 仍保持私有。
pub mod algebra;
pub mod calculus;
pub mod dynamics;
pub mod encoder;
mod spelling;
pub mod strings;
mod syllabifier;
pub mod utilities;

pub use algebra::{Projection, Script};
pub use calculus::{
    Abbreviation, Calculation, Calculus, Correction, Derivation, Erasion, Factory, Fuzzing, Reorder,
    Transformation, Transliteration, K_ABBREVIATION_PENALTY, K_CORRECTION_PENALTY,
    K_FUZZY_SPELLING_PENALTY,
};
pub use dynamics::{formula_d, formula_p};
pub use encoder::{
    CodeCoords, Encoder, PhraseCollector, RawCode, ScriptEncoder, TableEncoder, TableEncodingRule,
    K_ENCODER_DFS_LIMIT, K_MAX_PHRASE_LENGTH,
};
pub use spelling::{Spelling, SpellingProperties, SpellingType};
pub use strings::{join, split, split_default, SplitBehavior};
pub use syllabifier::{
    Corrector, EdgeMap, EdgeProperties, EndVertexMap, InMemoryPrism, Prism, PrismMatch,
    SpellingIndices, SpellingMap, SyllableGraph, SyllableId, Syllabifier, VertexMap,
    K_COMPLETION_PENALTY, K_CORRECTION_CREDIBILITY,
};
pub use utilities::{compare_version_string, file_modified_time, to_time_t, ChecksumComputer};
