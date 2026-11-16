//! marisa-trie —— **只剩 OpenCC 在用**。
//!
//! ## 历史
//!
//! 2026-10-24 引入 `rsmarisa`（tokuhirom，BSD-2-Clause）——
//! [marisa-trie](https://github.com/s-yata/marisa-trie) 的**纯 Rust 移植**，格式兼容 C++。
//! 当时 `.table.bin` 的字符串表、`.aux.bin`、OpenCC 全走它。
//!
//! 2026-11-15 用户定：**词典层全套切双数组 DAT**（[`crate::gear::dict::darts`]，与
//! `.prism.bin` 同款后端）——`.table.bin` 字符串表、`.aux.bin`、eassy 索引全部改用
//! [`crate::gear::dict::darts::DoubleArray`]，不再依赖 marisa。
//!
//! ## 为什么还留着
//!
//! OpenCC 的上游二进制词典 `.ocd2`（`OPENCC_MARISA_0.2.5` 魔数，见
//! [`crate::gear::dict::ocd2`]）**本身就是 marisa 格式**——上游产物改不了，读它就必须
//! 有 marisa。所以本模块只剩一个职责：把 `rsmarisa::Trie` 再导出给 `ocd2.rs`
//! （OpenCC 读取器）。`opencc.rs` 直接 use `rsmarisa`，不经这里。

pub use rsmarisa::Trie;
