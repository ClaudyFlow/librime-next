//! librime `src/rime/dict/` 的 Rust 移植 —— **门面**。
//!
//! | C++ 文件 | 行数 | Rust 模块 | 状态 |
//! |---|---|---|---|
//! | `mapped_file.h`(173) + `mapped_file.cc`(168) | 341 | [`mapped_file`] | 已移植（读 + 写） |
//! | `prism.h`(106) + `prism.cc`(351) | 457 | [`prism`] | 已移植（读 + `Build`/`Save`） |
//! | （`darts.h`，第三方） | — | [`darts`] | 只读查询 + 从零构建（大 offset 支持到 2²⁹） |
//! | `table.h`(239) + `table.cc`(636) | 875 | [`table`] | 已移植（读 + `Build`/`Save`） |
//! | `string_table.h`(60) + `string_table.cc`(114) | 174 | [`string_table`] | 已移植（读 marisa） |
//! | `StringTableBuilder`（`string_table.cc:73-112`） | 40 | [`string_table_builder`] | 已移植（写 marisa 镜像） |
//! | （`marisa-trie`，第三方） | — | [`marisa`] | 读 + 写（`rsmarisa` 的 build / write） |
//! | `vocabulary.h`(113) + `vocabulary.cc`(152) | 265 | [`vocabulary`] | 已移植 |
//! | `corrector.h`(134) + `corrector.cc`(322) | 456 | [`corrector`] | 已移植 |
//! | `dictionary.h`(124) + `dictionary.cc`(480) | 604 | [`dictionary`] | 已移植 |
//! | `entry_collector.h`(82) + `entry_collector.cc`(261) | 343 | [`entry_collector`] | 已移植 |
//! | `dict_settings.h`(25) + `dict_settings.cc`(119) | 144 | [`dict_settings`] | 已移植 |
//! | `dict_compiler.h`(66) + `dict_compiler.cc`(424) | 490 | [`dict_compiler`] | 已移植 |
//! | `reverse_lookup_dictionary.h`(89) + `reverse_lookup_dictionary.cc`(261) | 350 | [`reverse_lookup_dictionary`] | 已移植 |
//! | `preset_vocabulary.h`(27) + `preset_vocabulary.cc`(116) | 143 | [`preset_vocabulary`] | 已移植 |
//! | `text_db.h`(80) + `text_db.cc`(233) | 313 | [`text_db`] | ❌ **仍是占位**（快照走 TSV，暂不需要 TextDb 后端） |
//! | `db.h`(119) + `db.cc`(60) + `db_utils`(124) + `tsv`(162) | 465 | [`db`] / [`tsv`] | 已移植（2026-10-26：`Source`/`Sink` + `TsvWriter` 补齐） |
//! | `user_db.h`(142) + `user_db.cc`(270) | 412 | [`user_db`] | 已移植（2026-10-26，`.userdb.txt` 快照 + merger/importer） |
//! | `level_db.h`(78) + `level_db.cc`(348) | 426 | [`level_db`] | 已移植（`rusty-leveldb`；**实测读得动 C++ 版写出的 .userdb**） |
//! | `user_dictionary.h`(121) + `user_dictionary.cc`(622) | 743 | [`user_dictionary`] | 已移植（2026-10-26，DFS 查询 + 学词 + 事务） |
//!
//! ## 三种 `.bin` 的格式（逆向结论都写在各自模块的文档里）
//!
//! ```text
//! xxx.prism.bin    Rime::Prism/5.0        metadata(320B) → darts 双数组 → spelling map
//! xxx.table.bin    Rime::Table/4.0        metadata(68B)  → 音节表 → 词条索引 → marisa-trie 字符串表
//! xxx.reverse.bin  Rime::Reverse/3.1      metadata(64B)  → 键/值双 marisa-trie + StringId index
//! ```
//!
//! 注意 `.reverse.bin` **不是** text_db：C++ `ReverseDb : public MappedFile`
//! （`reverse_lookup_dictionary.h:40`），是二进制镜像；text_db 只服务 preset vocabulary。
//!
//! 三者的共同前提是 [`mapped_file`] 的**相对偏移指针**（`OffsetPtr`）：
//! 指针存的是"相对自己那个字段地址的 i32 偏移"，0 表示 NULL。C++ 在 `MappedFile`
//! 里直接拿 mmap 基址当字节数组用，所以 Rust 侧的 `MappedFile` 就是一份 `Vec<u8>`
//! 镜像，所有访问都带边界检查（C++ 是裸指针，越界是 UB；这里返回 `None`）。
//!
//! ## 与 C++ 的接口差异（都是"接缝"，不是语义）
//!
//! * **mmap**：不加映射依赖。读 = 一次性 `fs::read`；写 = `Vec<u8>` 内存镜像 + 落盘。
//! * **写路径**：C++ 的 `Build()` 在已打开的可写 `MappedFile` 上原地建；Rust 侧
//!   [`table::Table::build`] / [`prism::Prism::build`] 自己 `Create` 镜像并返回已
//!   `Load` 的对象，`save()` 负责 `ShrinkToFit` + 落盘，`compile()` 一步到位。
//!   `.table.bin` 的**分配顺序、256 对齐、`OffsetPtr` 回填时机**都照 `table.cc` 抄
//!   （见 [`table`] 模块文档的"写路径"）。
//! * **`StringTableBuilder`**：C++ 用 `StringId*`（指向 mmap 的裸指针）记"回填地址"；
//!   Rust 侧记的是字段在镜像里的**绝对偏移**，`UpdateReferences` 等价于
//!   [`string_table_builder::StringTableBuilder::update_references`]。
//! * **`Corrector`**：`algo::syllabifier::Corrector` 的 trait 签名只保留
//!   `(syllable_id, 匹配串)`，C++ 的 `Corrections`（含 `distance` / `length`）在
//!   [`corrector`] 里另有完整实现；`syllabifier` 那条路走 trait。
//! * **`Prism::query_spelling`**：`algo::SpellingProperties` 的 `tips` /
//!   `is_correction` 是 `pub(super)`（`algo` 私有），本层**填不进去**。算法层
//!   只能看到 `type` + `credibility`；本层自己的完整描述符走
//!   [`prism::SpellingAccessor::descriptor`]（拿到 [`prism::SpellingDescriptor`]
//!   的四个字段）。这是 **trail 上的已知缺口**，不是没抄。
//!   **写路径同样受这条限制**：`Prism::build(syllabary, Some(&script))` 写出的
//!   描述符只有 `type` + `credibility`，`tips` 为空、纠错位为 0。
//! * **`userdb`**：C++ 用 LevelDB，Rust 侧用纯 Rust 的 `rusty-leveldb`，
//!   键的编解码逐字节照抄（见 [`user_db`]）。
#![allow(dead_code)]

pub mod darts;
pub mod db;
pub mod dict_compiler;
pub mod dict_entry_iterator;
pub mod dict_settings;
pub mod dictionary;
pub mod entry_collector;
pub mod gram_db;
pub mod level_db;
pub mod mapped_file;
pub mod preset_vocabulary;
pub mod prism;
pub mod reverse_lookup_dictionary;
pub mod text_db;
pub mod tsv;
pub mod user_db;
pub mod user_dictionary;
pub mod vocabulary;

// corrector / marisa / string_table / string_table_builder / table 均已补齐：
// `.table.bin` 读得出来也建得出来（2026-10-25 dict 编译链全通）。
pub mod corrector;
pub mod marisa;
pub mod opencc;
pub mod string_table;
pub mod string_table_builder;
pub mod table;
