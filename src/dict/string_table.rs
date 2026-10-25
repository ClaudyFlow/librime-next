//! `dict/string_table.h`(60 行) + `dict/string_table.cc`(114 行) 的 Rust 移植 —— **读 + 写**。
//!
//! 本文件是**读侧**（`StringTable`）；写侧 `StringTableBuilder` 在
//! [`crate::dict::string_table_builder`]（marisa 的构建 + 序列化走 [`crate::dict::marisa`]）。
//!
//! `.table.bin` 的最后一段是一棵 **marisa trie**：`字符串 ↔ id`。`Table` 用它同时存
//! 音节串（`syllabary`）和词条文本（`table::Entry::text`）——两者在文件里都只留一个
//! `StringId`，正文全在这棵 trie 里，所以 `.table.bin` 少了一棵 trie 就只是些整数。
//!
//! # 文件里长什么样
//!
//! ```text
//! metadata->string_table        OffsetPtr<char>   —— 相对该字段自身的偏移
//! metadata->string_table_size   u32               —— 这段 blob 的字节数
//! ```
//!
//! 这段 blob 就是 `marisa::Trie` 的**内存镜像**（开头 16 字节是 `"We love Marisa.\0"`）。
//! C++ 是 `trie_.map(ptr, size)` 零拷贝；这里走 [`crate::dict::marisa::MarisaTrie`]，
//! 它是**拷贝**语义（`rsmarisa` 的 `Trie::read`），换来 blob 不必活到 trie 的生命周期。
//!
//! id 的分配：`StringTableBuilder::Build` → `trie_.build(keys_)`，id 由 marisa 按自己的
//! terminal 顺序编 —— **不是**键的字典序（实测 `yuling.table.bin` 的音节 id 就不单调，
//! 因为同一棵树里还混着词条文本）。所以 [`StringTable::predict`] 只能按 id 序给结果。
//! `kInvalidStringId` 是 `(StringId)(-1)`，也就是 `u32::MAX`。
//!
//! # 写路径：`StringTableBuilder`
//!
//! C++ 的同名文件里还有半个文件是 `StringTableBuilder`：`Add(key, weight, &reference)`
//! 收集 key（顺带记下"建完树把 id 写回哪个字段"）、`Build()` 建树、`Dump(ptr, size)`
//! 把 `trie_` 序列化进 `MappedFile`、`UpdateReferences()` 回填 id。
//! Rust 侧整套在 [`crate::dict::string_table_builder`]，底层用
//! [`crate::dict::marisa::MarisaTrie::build`]（`rsmarisa` 的 `Trie::build`）+
//! [`crate::dict::marisa::MarisaTrie::to_bytes`]（`Trie::write`）。
//!
//! # 与 C++ 的接口差异（接缝）
//!
//! * **`Lookup` 返回 `Option<StringId>`**：C++ 用 `kInvalidStringId` 当"没查到"的哨兵，
//!   Rust 用 `None`；要哨兵的话有 [`StringTable::lookup_or_invalid`]。
//! * **`GetString` 返回 `Option<String>`**：C++ 打一条 `LOG(ERROR)` 再返回空串。
//!   文件里 id 越界（含 `kInvalidStringId`）在 C++ 侧会走到 `catch`，这里提前判界 ——
//!   这一条不只是风格：`rsmarisa` 的 `Trie::reverse_lookup` 对越界 id 是 **assert panic**，
//!   所以边界检查必须在进 `reverse_lookup` 之前做。C++ 那套"空串"语义由
//!   [`StringTable::get_string_or_empty`] 提供（`Table::GetString` 用的就是它）。
//! * **`CommonPrefixMatch` / `Predict` 的输出容器是返回值**（C++ 是 `vector<StringId>*`）。
//!   实现方式见各自文档：封装没暴露 agent 级遍历，所以用 `lookup` / `dump` 组合。
//!   `CommonPrefixMatch` 的"长度升序"与 C++ 一致；`Predict` 只保证**集合**一致，
//!   顺序是 id 序（C++ 是键序遍历序），复杂度也不同。
//! * **构造失败是 `Result`**：C++ 的 `trie_.map()` 失败会抛 `marisa::Exception`，
//!   未捕获时进程直接走掉；这里返回 [`MarisaError`]。
#![allow(dead_code)]

use crate::dict::marisa::{MarisaError, MarisaTrie};

/// `using StringId = std::uint32_t;`（`string_table.h:19`）
pub type StringId = u32;

/// `const StringId kInvalidStringId = (StringId)(-1);`（`string_table.h:21`）
pub const K_INVALID_STRING_ID: StringId = StringId::MAX;

/// `class StringTable`（`string_table.h:23-38`）—— 只读形态。
///
/// 对应 C++ 的 `StringTable(const char* ptr, size_t size)`：一段 marisa 镜像 + 一层
/// "字符串 ↔ id"的门面。
pub struct StringTable {
    /// C++ 的 `marisa::Trie trie_`
    trie: MarisaTrie,
    /// C++ `BinarySize()` = `trie_.io_size()`；map 进来的 trie 就是那段 blob 的长度。
    /// （`rsmarisa` 的 `io_size()` 要有 `Trie` 才给得出来，而这里连 blob 长度一起存着，
    /// 省得为了一个数字再去问 trie。）
    binary_size: usize,
}

impl std::fmt::Debug for StringTable {
    /// `MarisaTrie` 没实现 `Debug`（在 `dict/marisa.rs` 里，本次不动它），这里手写。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StringTable")
            .field("num_keys", &self.num_keys())
            .field("binary_size", &self.binary_size)
            .finish()
    }
}

impl StringTable {
    /// `StringTable::StringTable(ptr, size)`（`string_table.cc:14-16`）：`trie_.map(ptr, size)`。
    ///
    /// `bytes` 就是 `metadata->string_table` 起、`metadata->string_table_size` 长的切片。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MarisaError> {
        Ok(Self {
            trie: MarisaTrie::from_bytes(bytes)?,
            binary_size: bytes.len(),
        })
    }

    // ---------------------------------------------------------------- 查

    /// `StringTable::HasKey`（`string_table.cc:18-22`）
    pub fn has_key(&self, key: &str) -> bool {
        self.trie.lookup(key).is_some()
    }

    /// `StringTable::Lookup`（`string_table.cc:24-32`）：未命中返回 `None`
    /// （C++ 返回 `kInvalidStringId`，见 [`Self::lookup_or_invalid`]）。
    pub fn lookup(&self, key: &str) -> Option<StringId> {
        self.trie.lookup(key)
    }

    /// [`Self::lookup`] 的 C++ 哨兵版：未命中返回 [`K_INVALID_STRING_ID`]。
    pub fn lookup_or_invalid(&self, key: &str) -> StringId {
        self.lookup(key).unwrap_or(K_INVALID_STRING_ID)
    }

    /// `StringTable::CommonPrefixMatch`（`string_table.cc:34-42`）：所有**是 `query`
    /// 前缀**的键，按长度升序，返回它们的 id。
    ///
    /// C++ 是 `trie_.common_prefix_search(agent)` 循环（marisa 从根往下走，天然短→长）；
    /// 封装没暴露这个 API，所以这里对 `query` 的**每个字节前缀**各做一次 `lookup` ——
    /// 结果集合与顺序都与 marisa 一致（marisa 是字节 trie，而键都是 UTF-8，所以按
    /// `char_indices` 走边界就够：非 UTF-8 边界的字节串不可能被 `Add` 进去过）。
    /// 代价是 O(len) 次 `lookup`，不是 O(len) 次节点步进。
    pub fn common_prefix_match(&self, query: &str) -> Vec<StringId> {
        let mut result = Vec::new();
        for (end, _) in query
            .char_indices()
            .chain(std::iter::once((query.len(), '\0')))
        {
            if end == 0 {
                continue; // 空键：Rime 从不往 trie 里放空串
            }
            if let Some(id) = self.lookup(&query[..end]) {
                result.push(id);
            }
        }
        result
    }

    /// `StringTable::Predict`（`string_table.cc:44-51`）：所有**以 `query` 为前缀**的键，
    /// 返回它们的 id。
    ///
    /// C++ 是 `trie_.predictive_search(agent)` 循环（marisa 按键序走）；封装没暴露该 API，
    /// 这里退化成"把整棵树 `dump()` 出来按前缀过滤"：
    /// **结果集合一致，但顺序是 id 序**（= trie 里 terminal 的存放顺序），不保证与 C++
    /// 的键序一致 —— 真要逐条同序，得先把 `dict/marisa.rs` 里的 `predictive_search`
    /// 放出来。另外**每次调用 O(num_keys)** 次反查；`dict/` 层目前没有调用者
    /// （`Table` 只按 id 取串），所以这个代价还没落到热路径上。
    pub fn predict(&self, query: &str) -> Vec<StringId> {
        self.trie
            .dump()
            .into_iter()
            .filter(|(_, key)| key.starts_with(query))
            .map(|(id, _)| id)
            .collect()
    }

    /// `StringTable::GetString`（`string_table.cc:53-63`）：按 id 反查字符串。
    ///
    /// id 越界（含 [`K_INVALID_STRING_ID`]）返回 `None`。**必须先判界**：C++ 靠
    /// `try/catch` 兜 marisa 的异常，而 `rsmarisa` 的 `Trie::reverse_lookup` 对越界 id
    /// 直接 `assert!` panic。
    pub fn get_string(&self, string_id: StringId) -> Option<String> {
        if string_id as usize >= self.trie.len() {
            return None;
        }
        self.trie.reverse_lookup(string_id)
    }

    /// C++ `GetString` 的原语义：坏 id → 空串（C++ 那边还打一条 `LOG(ERROR)`）。
    /// `Table::GetString` 用它。
    pub fn get_string_or_empty(&self, string_id: StringId) -> String {
        self.get_string(string_id).unwrap_or_default()
    }

    // ---------------------------------------------------------------- 计数

    /// `StringTable::NumKeys`（`string_table.cc:65-67`）= `trie_.size()`。
    pub fn num_keys(&self) -> usize {
        self.trie.len()
    }

    /// `StringTable::BinarySize`（`string_table.cc:69-71`）= `trie_.io_size()`。
    pub fn binary_size(&self) -> usize {
        self.binary_size
    }

    pub fn is_empty(&self) -> bool {
        self.num_keys() == 0
    }

    /// 把整棵树摊成 `(id, 字符串)`（按 id 升序，不是键序）。**仅供调试/交叉校验**：
    /// 底层是 `num_keys` 次 `reverse_lookup`，O(n)。
    pub fn keys(&self) -> Vec<(StringId, String)> {
        self.trie.dump()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::marisa::build_for_test;

    /// 测试专用：拿 `build_for_test` 现建一棵树塞进来（**没有** blob 可读，
    /// 所以 `binary_size` 记 0 —— 只影响 `BinarySize()` 那个数字）。
    fn table_of(keys: &[&str]) -> StringTable {
        StringTable {
            trie: build_for_test(keys).expect("建树失败"),
            binary_size: 0,
        }
    }

    #[test]
    fn lookup_and_get_string_round_trip() {
        let table = table_of(&["a", "ab", "abc", "b", "你好"]);
        assert_eq!(table.num_keys(), 5);
        assert!(!table.is_empty());

        for key in ["a", "ab", "abc", "b", "你好"] {
            let id = table.lookup(key).expect("键应该命中");
            assert!(id < table.num_keys() as StringId);
            assert_eq!(table.get_string(id).as_deref(), Some(key));
            assert!(table.has_key(key));
        }
        // 未命中
        assert_eq!(table.lookup("abcd"), None);
        assert_eq!(table.lookup_or_invalid("abcd"), K_INVALID_STRING_ID);
        assert!(table.lookup_or_invalid("abc") != K_INVALID_STRING_ID);
        assert!(!table.has_key(""));
    }

    #[test]
    fn invalid_ids_do_not_panic() {
        // `rsmarisa` 的 reverse_lookup 对越界 id 是 assert panic，所以这个守卫是硬要求：
        // 一份坏的 `.table.bin`（v1 的 String 指针被当成 id 用）就能造出 u32::MAX。
        let table = table_of(&["x", "y"]);
        assert_eq!(table.get_string(K_INVALID_STRING_ID), None);
        assert_eq!(table.get_string(table.num_keys() as StringId), None);
        assert_eq!(table.get_string(u32::MAX - 1), None);
        assert_eq!(table.get_string_or_empty(K_INVALID_STRING_ID), "");
        assert!(!table.get_string_or_empty(0).is_empty(), "0 号键必须存在");
    }

    #[test]
    fn common_prefix_match_returns_prefixes_by_increasing_length() {
        let table = table_of(&["a", "ab", "abc", "abcd", "b"]);
        let query = "abcdef";
        let ids: Vec<StringId> = table.common_prefix_match(query);
        let texts: Vec<String> = ids
            .iter()
            .filter_map(|id| table.get_string(*id))
            .collect();
        // 顺序 = 长度升序（marisa 从根往下走）
        assert_eq!(texts, vec!["a", "ab", "abc", "abcd"]);
        // 不是前缀的键不出现
        assert!(!texts.contains(&"b".to_string()));
        // 没有命中 → 空
        assert!(table.common_prefix_match("zzz").is_empty());
        assert!(table.common_prefix_match("").is_empty());
        // 整串就是键
        assert_eq!(table.common_prefix_match("abc").len(), 3);
    }

    #[test]
    fn predict_returns_all_keys_with_prefix() {
        let table = table_of(&["a", "ab", "abc", "b", "ba"]);
        let ids = table.predict("ab");
        let mut texts: Vec<String> = ids.iter().filter_map(|id| table.get_string(*id)).collect();
        texts.sort();
        // 集合对（顺序是 id 序，见方法文档：不保证等于 marisa 的键序）
        assert_eq!(texts, vec!["ab", "abc"]);
        assert!(ids.windows(2).all(|window| window[0] < window[1]), "id 升序");
        let mut all: Vec<String> = table
            .predict("")
            .iter()
            .filter_map(|id| table.get_string(*id))
            .collect();
        all.sort();
        // predict("") 命中全部键
        assert_eq!(all, vec!["a", "ab", "abc", "b", "ba"]);
        assert!(table.predict("c").is_empty());
    }

    #[test]
    fn keys_dump_covers_every_key() {
        let table = table_of(&["apple", "apply", "banana"]);
        let keys = table.keys();
        assert_eq!(keys.len(), 3);
        // id 升序，且 id 与 lookup 回的 id 一致
        for (index, (id, text)) in keys.iter().enumerate() {
            assert_eq!(table.lookup(text), Some(*id));
            assert!(index == 0 || keys[index - 1].0 < *id);
        }
        let texts: Vec<&str> = keys.iter().map(|(_, text)| text.as_str()).collect();
        assert!(texts.contains(&"apple") && texts.contains(&"apply") && texts.contains(&"banana"));
    }

    #[test]
    fn blob_errors_are_reported_instead_of_panicking() {
        assert!(StringTable::from_bytes(&[]).is_err(), "空 blob 必须报错");
        assert!(
            StringTable::from_bytes(&[0xAB; 64]).is_err(),
            "垃圾字节必须报错"
        );
    }
}
