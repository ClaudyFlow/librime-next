//! `dict/string_table.h`(60 行) + `dict/string_table.cc`(114 行) 的 Rust 移植 —— **读 + 写**。
//!
//! 本文件是**读侧**（`StringTable`）；写侧 `StringTableBuilder` 在
//! [`crate::gear::dict::string_table_builder`]。
//!
//! `.table.bin` 的最后一段是一棵 **双数组字典树（DoubleArray Trie）**：`字符串 ↔ id`。
//! `Table` 用它同时存音节串（`syllabary`）和词条文本（`table::Entry::text`）——两者在文件里
//! 都只留一个 `StringId`，正文全在这棵树里，所以 `.table.bin` 少了一棵树就只是些整数。
//!
//! # 文件里长什么样
//!
//! ```text
//! metadata->string_table        OffsetPtr<char>   —— 相对该字段自身的偏移
//! metadata->string_table_size   u32               —— 这段 blob 的字节数
//! ```
//!
//! 这段 blob 就是本文件 `StringTable::from_bytes` 能解析的镜像：
//!
//! ```text
//! [num_units: u32 LE]                      双数组单元数
//! [num_units × 4 B]                        双数组单元（每个小端 u32，见 `darts.rs`）
//! [num_strings: u32 LE]                   字符串条数（= 不同键数，id 连续 0..n-1）
//! for i in 0..num_strings:
//!     [len_i: u32 LE][len_i B]            第 i 条字符串（UTF-8）
//! ```
//!
//! 写侧（`StringTableBuilder`）把树建成 `DoubleArray`：键 → 值在 `distinct` 列表里的下标，
//! 再并行存一份 `strings`（按 id 索引）供反查。读侧 `lookup(key)` 走
//! [`crate::gear::dict::darts::DoubleArray::exact_match_search`] 拿到 id，`get_string(id)`
//! 直接取下标。
//!
//! id 的分配：`StringTableBuilder::Build` → `DoubleArray::build(distinct)`，**键首次出现的
//! 顺序**即 id 序（不是字典序、也不是 marisa 的 terminal 序；不同键数 = `num_strings`）。
//! 因为 writer 与 reader 都吃同一套"首次出现序 = id 序"约定，内部自洽——每次部署都由
//! `dict_compiler` 重新生成 `.table.bin`，不存在跨版本共享 id 的问题。
//!
//! # 与 C++ 的接口差异（接缝）
//!
//! * **`Lookup` 返回 `Option<StringId>`**：C++ 用 `kInvalidStringId` 当"没查到"的哨兵，
//!   Rust 用 `None`；要哨兵的话有 [`StringTable::lookup_or_invalid`]。
//! * **`GetString` 返回 `Option<String>`**：C++ 打一条 `LOG(ERROR)` 再返回空串。
//!   文件里 id 越界（含 `kInvalidStringId`）在 C++ 侧会走到 `catch`，这里提前判界 ——
//!   越界直接返回 `None`，与旧 `rsmarisa` 的 assert panic 行为相比更安全。
//! * **`CommonPrefixMatch` / `Predict` 的输出容器是返回值**（C++ 是 `vector<StringId>*`）。
//!   `CommonPrefixMatch` 的"长度升序"与 C++ 一致；`Predict` 只保证**集合**一致，
//!   顺序是 id 序（C++ 是键序遍历序），复杂度也不同。
//! * **构造失败是 `Result`**：C++ 的 `trie_.map()` 失败会抛 `marisa::Exception`，
//!   未捕获时进程直接走掉；这里返回 [`StringTableError`]。
//! * 旧实现用 `rsmarisa`（marisa-trie 的 Rust 移植）存这棵树；2026-11-15 起整套切到
//!   [`crate::gear::dict::darts::DoubleArray`]，与 `.prism.bin` 用同一套双数组后端，
//!   去掉了对 `rsmarisa` 的依赖（OpenCC 的 `.ocd2` 读取仍保留 marisa，那是上游二进制格式）。
#![allow(dead_code)]

use crate::gear::dict::darts::DoubleArray;

/// `using StringId = std::uint32_t;`（`string_table.h:19`）
pub type StringId = u32;

/// `const StringId kInvalidStringId = (StringId)(-1);`（`string_table.h:21`）
pub const K_INVALID_STRING_ID: StringId = StringId::MAX;

/// 字符串表读写错误（取代旧 `marisa::MarisaError`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringTableError(pub String);

impl std::fmt::Display for StringTableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "string table: {}", self.0)
    }
}

impl std::error::Error for StringTableError {}

/// `class StringTable`（`string_table.h:23-38`）—— 只读形态。
///
/// 对应 C++ 的 `StringTable(const char* ptr, size_t size)`：一段双数组镜像 + 一层
/// "字符串 ↔ id"的门面。
pub struct StringTable {
    /// 双数组 trie：键（UTF-8 字节串）→ id（= 该键在 `strings` 里的下标）。
    da: DoubleArray,
    /// 按 id 索引的字符串数组；`strings[id]` 即 id 对应的原始键。
    strings: Vec<String>,
    /// blob 的总字节数（`metadata->string_table_size`）。
    binary_size: usize,
}

impl std::fmt::Debug for StringTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StringTable")
            .field("num_keys", &self.num_keys())
            .field("binary_size", &self.binary_size)
            .finish()
    }
}

impl StringTable {
    /// `StringTable::StringTable(ptr, size)`（`string_table.cc:14-16`）：解析双数组镜像。
    ///
    /// `bytes` 就是 `metadata->string_table` 起、`metadata->string_table_size` 长的切片。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, StringTableError> {
        if bytes.len() < 4 {
            return Err(StringTableError("blob too short for unit count".into()));
        }
        let num_units =
            u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
        let da_bytes = &bytes[4..];
        let da = DoubleArray::from_bytes(da_bytes, num_units)
            .ok_or_else(|| StringTableError("invalid double-array units".into()))?;

        let mut cursor = 4 + num_units * 4;
        if cursor + 4 > bytes.len() {
            return Err(StringTableError("blob truncated before string count".into()));
        }
        let num_strings =
            u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
        cursor += 4;

        let mut strings = Vec::with_capacity(num_strings);
        for _ in 0..num_strings {
            if cursor + 4 > bytes.len() {
                return Err(StringTableError("blob truncated before string length".into()));
            }
            let len =
                u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
            cursor += 4;
            if cursor + len > bytes.len() {
                return Err(StringTableError("blob truncated inside string body".into()));
            }
            let text = String::from_utf8_lossy(&bytes[cursor..cursor + len]).into_owned();
            cursor += len;
            strings.push(text);
        }

        Ok(Self {
            da,
            strings,
            binary_size: bytes.len(),
        })
    }

    // ---------------------------------------------------------------- 查

    /// `StringTable::HasKey`（`string_table.cc:18-22`）
    pub fn has_key(&self, key: &str) -> bool {
        self.lookup(key).is_some()
    }

    /// `StringTable::Lookup`（`string_table.cc:24-32`）：未命中返回 `None`
    /// （C++ 返回 `kInvalidStringId`，见 [`Self::lookup_or_invalid`]）。
    pub fn lookup(&self, key: &str) -> Option<StringId> {
        self.da.exact_match_search(key.as_bytes())
    }

    /// [`Self::lookup`] 的 C++ 哨兵版：未命中返回 [`K_INVALID_STRING_ID`]。
    pub fn lookup_or_invalid(&self, key: &str) -> StringId {
        self.lookup(key).unwrap_or(K_INVALID_STRING_ID)
    }

    /// `StringTable::CommonPrefixMatch`（`string_table.cc:34-42`）：所有**是 `query`
    /// 前缀**的键，按长度升序，返回它们的 id。
    ///
    /// 对 `query` 的每个 UTF-8 字符边界前缀各做一次 `lookup`——结果集合与顺序都
    /// 与 C++（字节 trie 从根往下走、天然短→长）一致。O(len) 次 `lookup`。
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
    /// 退化成"遍历 `strings` 按前缀过滤"：**结果集合一致，但顺序是 id 序**（不保证与
    /// C++ 的键序一致）。`dict/` 层目前没有调用者（`Table` 只按 id 取串），所以代价没落到
    /// 热路径上。每次调用 O(num_keys)。
    pub fn predict(&self, query: &str) -> Vec<StringId> {
        self.strings
            .iter()
            .enumerate()
            .filter(|(_, key)| key.starts_with(query))
            .map(|(id, _)| id as StringId)
            .collect()
    }

    /// `StringTable::GetString`（`string_table.cc:53-63`）：按 id 反查字符串。
    ///
    /// id 越界（含 [`K_INVALID_STRING_ID`]）返回 `None`。
    pub fn get_string(&self, string_id: StringId) -> Option<String> {
        self.strings.get(string_id as usize).cloned()
    }

    /// C++ `GetString` 的原语义：坏 id → 空串（C++ 那边还打一条 `LOG(ERROR)`）。
    /// `Table::GetString` 用它。
    pub fn get_string_or_empty(&self, string_id: StringId) -> String {
        self.get_string(string_id).unwrap_or_default()
    }

    // ---------------------------------------------------------------- 计数

    /// `StringTable::NumKeys`（`string_table.cc:65-67`）= `strings.len()`。
    pub fn num_keys(&self) -> usize {
        self.strings.len()
    }

    /// `StringTable::BinarySize`（`string_table.cc:69-71`）= 镜像字节数。
    pub fn binary_size(&self) -> usize {
        self.binary_size
    }

    pub fn is_empty(&self) -> bool {
        self.num_keys() == 0
    }

    /// 把整张表摊成 `(id, 字符串)`（按 id 升序）。**仅供调试/交叉校验**：
    /// 底层是 `num_keys` 次克隆，O(n)。
    pub fn keys(&self) -> Vec<(StringId, String)> {
        self.strings
            .iter()
            .enumerate()
            .map(|(i, text)| (i as StringId, text.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gear::dict::string_table_builder::StringTableBuilder;

    /// 测试专用：现建一棵树塞进来（**没有** blob 可读，所以 `binary_size` 用镜像长度）。
    fn table_of(keys: &[&str]) -> StringTable {
        let mut builder = StringTableBuilder::new();
        for key in keys {
            builder.add(*key, 0.0, None);
        }
        builder.build().expect("建树失败");
        StringTable::from_bytes(builder.image()).expect("读回字符串表")
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
        // 顺序 = 长度升序
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
        // 集合对（顺序是 id 序，见方法文档）
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
