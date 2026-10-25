//! marisa-trie 的封装 —— Rime `.table.bin` 里的字符串表就是一棵 marisa trie。
//!
//! ## 用 `rsmarisa`（纯 Rust），不用 C++
//!
//! 2026-10-24 的结论：
//!
//! * crates.io 上 `marisa` / `marisa-sys` / `libmarisa` **都不存在**（HTTP 404）；
//! * 但存在 **`rsmarisa`**（tokuhirom，BSD-2-Clause，2026-10-14）——
//!   [marisa-trie](https://github.com/s-yata/marisa-trie) 的**纯 Rust 移植**，格式兼容。
//!
//! 所以不必"链接 librime deps 里那份 `libmarisa.a` 再套一个 C++ shim"（那条路能跑，
//! 但把 C++ 又带回来了，与"用 Rust 替掉 librime"的目标相悖）。
//!
//! ## 加载方式
//!
//! `rsmarisa::Trie::read(&mut Reader)` 可以从**内存**读：
//!
//! ```ignore
//! let mut reader = Reader::from_bytes(blob);   // blob 来自 MappedFile 的切片
//! let mut trie = Trie::new();
//! trie.read(&mut reader)?;
//! ```
//!
//! 注意它是**拷贝**语义（不是 `map` 那种零拷贝），换来的是"blob 不必活到 trie 的整个生命周期"。
//! 对 `.table.bin`（几 MB）来说这点拷贝可以接受，而且**省掉了一堆生命周期参数**。
//!
//! ## 写路径（2026-10-24 补）
//!
//! `rsmarisa` 同时也是构建器：`Keyset::push_back_bytes` 收键与权重、
//! `Trie::build(&mut keyset, flags)` 建树（并把最终 id 写回 `keyset.get(i).id()`）、
//! `Trie::write(&mut Writer)` 序列化成与 C++ marisa **同格式**的镜头。
//! 三者分别对应 [`MarisaTrie::build`] 与 [`MarisaTrie::to_bytes`]；
//! Rime 的 `StringTableBuilder`（见 `dict/string_table_builder.rs`）就是这套的组合。

use std::path::Path;

use rsmarisa::grimoire::io::reader::Reader;
use rsmarisa::grimoire::io::writer::Writer;
use rsmarisa::{Agent, Keyset, Trie};

/// marisa 相关错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarisaError(pub String);

impl std::fmt::Display for MarisaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "marisa: {}", self.0)
    }
}

impl std::error::Error for MarisaError {}

/// 一棵 marisa trie（Rime 的 `StringTable` 用它存"词条字符串 ↔ id"）。
///
/// 对应 C++ `marisa::Trie`；Rime 侧只用到 `lookup` / `reverse_lookup` / `num_keys`。
pub struct MarisaTrie {
    trie: Trie,
}

impl std::fmt::Debug for MarisaTrie {
    /// `Trie` 没实现 `Debug`，这里只报键数（与 `StringTable` 的 Debug 同款）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MarisaTrie")
            .field("num_keys", &self.len())
            .finish()
    }
}

impl MarisaTrie {
    /// 从 `.table.bin` 里那段 marisa blob 建树。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MarisaError> {
        if bytes.is_empty() {
            return Err(MarisaError("empty marisa blob".to_string()));
        }
        let mut reader = Reader::from_bytes(bytes);
        let mut trie = Trie::new();
        trie.read(&mut reader)
            .map_err(|error| MarisaError(format!("read: {error}")))?;
        Ok(Self { trie })
    }

    /// `marisa::Trie::build(keyset)`（C++ `string_table.cc:87` 的 `trie_.build(keys_)`）。
    ///
    /// `weights` 只用于 `MARISA_WEIGHT_ORDER`；Rime 用默认 flags（0 = 标签序），所以
    /// 传不传都对结果没影响。`weights` 比 `keys` 短时按 1.0 补（`Keyset::push_back_str`
    /// 的默认权重），这样调用方不必为"音节表权重恒 0"单独造一个数组。
    ///
    /// 返回值第二项是**按输入顺序**排的 id：`ids[i] == keyset.get(i).id()`，也就是
    /// `StringTableBuilder::UpdateReferences` 里 `keys_[i].id()` 的对照物。重复键会被
    /// marisa 合并成同一个 id（两个位置拿到同一个值），这正是 C++ 依赖的行为。
    ///
    /// 空 keyset 也允许：rsmarisa 是 marisa C++ 的忠实移植，空键集建成一棵
    /// **有效的空树**（3432 字节镜像，`num_keys() == 0`，可序列化/反序列化），
    /// 与 C++ `trie_.build(keys_)` 行为一致。Rime 的 `.reverse.bin` 空词库路径
    /// （`ReverseDb::Build` 的 rev_table/stems 都空）依赖这一点。
    pub fn build(keys: &[&str], weights: &[f32]) -> Result<(Self, Vec<u32>), MarisaError> {
        let mut keyset = Keyset::new();
        for (index, key) in keys.iter().enumerate() {
            let weight = weights.get(index).copied().unwrap_or(1.0);
            keyset
                .push_back_bytes(key.as_bytes(), weight)
                .map_err(|error| MarisaError(format!("keyset: {error}")))?;
        }
        let mut trie = Trie::new();
        trie.build(&mut keyset, 0);
        let ids = (0..keys.len())
            .map(|index| keyset.get(index).id() as u32)
            .collect();
        Ok((Self { trie }, ids))
    }

    /// `marisa::Trie::io_size()` = `StringTable::BinarySize()`（`string_table.cc:69-71`）。
    pub fn io_size(&self) -> usize {
        self.trie.io_size()
    }

    /// 把整棵树序列化成 `.table.bin` 尾部那段镜像（C++ `operator<<` / `Dump`）。
    ///
    /// C++ `StringTableBuilder::Dump` 是 `std::stringstream stream; stream << trie_;`
    /// 再把流读进 mmap；这里直接写进 `Vec<u8>`，由调用方拷到镜像里。
    pub fn to_bytes(&self) -> Result<Vec<u8>, MarisaError> {
        let mut writer = Writer::from_vec(Vec::new());
        self.trie
            .write(&mut writer)
            .map_err(|error| MarisaError(format!("write: {error}")))?;
        writer
            .into_inner()
            .map_err(|error| MarisaError(format!("write: {error}")))
    }

    /// `Trie::save(path)`：单独落一份 marisa 文件（调试用；`.table.bin` 由
    /// [`Self::to_bytes`] 嵌进去，不走这里）。
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), MarisaError> {
        self.trie
            .save(&path.as_ref().to_string_lossy())
            .map_err(|error| MarisaError(format!("save: {error}")))
    }

    /// `marisa::Trie::num_keys()`。
    pub fn len(&self) -> usize {
        self.trie.num_keys()
    }

    /// 空表（Rime 允许空字符串表）。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 按字符串查 id（`Trie::lookup`）。未命中返回 `None`。
    pub fn lookup(&self, key: &str) -> Option<u32> {
        let mut agent = Agent::new();
        agent.set_query_str(key);
        if self.trie.lookup(&mut agent) {
            Some(agent.key().id() as u32)
        } else {
            None
        }
    }

    /// 按 id 反查字符串（`Trie::reverse_lookup`）。
    pub fn reverse_lookup(&self, id: u32) -> Option<String> {
        let mut agent = Agent::new();
        agent.set_query_id(id as usize);
        self.trie.reverse_lookup(&mut agent);
        Some(String::from_utf8_lossy(agent.key().as_bytes()).into_owned())
    }

    /// 把整棵树摊成 `Vec<(id, 字符串)>`（按 id 顺序）。**调试/测试用**，`O(n)` 次反查。
    pub fn dump(&self) -> Vec<(u32, String)> {
        (0..self.len() as u32)
            .filter_map(|id| self.reverse_lookup(id).map(|text| (id, text)))
            .collect()
    }
}

/// 从一组 key 现场建树（**只给测试**）。
///
/// 生产路径是 [`MarisaTrie::build`]（`StringTableBuilder` 用它）+ [`MarisaTrie::from_bytes`]；
/// 这里只是让单测不必依赖真实 `.table.bin`。
pub fn build_for_test(keys: &[&str]) -> Result<MarisaTrie, MarisaError> {
    MarisaTrie::build(keys, &[]).map(|(trie, _ids)| trie)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_then_lookup_round_trips() {
        let trie = build_for_test(&["a", "ab", "abc", "b"]).expect("建树失败");
        assert_eq!(trie.len(), 4);
        let id = trie.lookup("ab").expect("`ab` 应命中");
        assert_eq!(trie.reverse_lookup(id).as_deref(), Some("ab"));
        assert!(trie.lookup("abcd").is_none(), "`abcd` 不存在");
        assert!(trie.lookup("").is_none(), "空串不该命中");
    }

    #[test]
    fn duplicate_keys_collapse() {
        let trie = build_for_test(&["x", "x", "y"]).expect("建树失败");
        assert_eq!(trie.len(), 2, "重复 key 只算一个");
    }

    #[test]
    fn empty_keyset_builds_valid_empty_trie() {
        assert!(MarisaTrie::from_bytes(&[]).is_err(), "空 blob 读不了");
        // 空 keyset **可以**建树：rsmarisa 是 marisa C++ 的忠实移植，空键集 → 有效空树
        let (trie, ids) = MarisaTrie::build(&[], &[]).expect("空 keyset 应能建树");
        assert!(ids.is_empty());
        assert_eq!(trie.len(), 0);
        assert!(trie.lookup("anything").is_none(), "空树查不到东西");
        // 序列化 + 读回（`.reverse.bin` 空词库路径依赖这条 roundtrip）
        let blob = trie.to_bytes().expect("序列化");
        assert!(!blob.is_empty(), "空树的镜像也有头部字节");
        let reread = MarisaTrie::from_bytes(&blob).expect("读回");
        assert_eq!(reread.len(), 0);
    }

    /// 写路径：建树 → 按输入顺序拿 id → 序列化 → 再读回来，id 必须原样保留。
    #[test]
    fn build_assigns_ids_in_input_order_and_round_trips_bytes() {
        let keys = ["apple", "apply", "banana", "你好"];
        let (trie, ids) = MarisaTrie::build(&keys, &[1.0, 2.0, 3.0, 4.0]).expect("建树失败");
        assert_eq!(ids.len(), keys.len());
        for (key, id) in keys.iter().zip(ids.iter()) {
            assert_eq!(trie.lookup(key), Some(*id), "{key} 的 id");
            assert_eq!(trie.reverse_lookup(*id).as_deref(), Some(*key));
        }
        // `io_size()` 与真正写出来的字节数必须相等（C++ `BinarySize()` 就是拿它分配空间）
        let blob = trie.to_bytes().expect("序列化");
        assert_eq!(blob.len(), trie.io_size());
        let reread = MarisaTrie::from_bytes(&blob).expect("读回");
        assert_eq!(reread.len(), keys.len());
        for (key, id) in keys.iter().zip(ids.iter()) {
            assert_eq!(reread.lookup(key), Some(*id), "{key} 读回后的 id");
        }
    }

    /// 重复键（C++ `StringTableBuilder::Add` 允许重复）只占一个 id，两个输入位置拿到同一个。
    #[test]
    fn duplicate_keys_share_one_id() {
        let (trie, ids) = MarisaTrie::build(&["x", "x", "y"], &[]).expect("建树失败");
        assert_eq!(trie.len(), 2);
        assert_eq!(ids[0], ids[1], "重复键应共享 id");
        assert_ne!(ids[0], ids[2]);
    }

    #[test]
    fn garbage_input_does_not_panic() {
        // 真实场景：blob 偏移算错时会喂进来一段垃圾 —— 必须返回 Err 而不是崩
        let garbage = vec![0xABu8; 64];
        assert!(MarisaTrie::from_bytes(&garbage).is_err());
    }

    #[test]
    fn dump_covers_every_key() {
        let trie = build_for_test(&["apple", "apply", "banana"]).expect("建树失败");
        let dumped = trie.dump();
        assert_eq!(dumped.len(), 3);
        let words: Vec<&str> = dumped.iter().map(|(_, text)| text.as_str()).collect();
        assert!(words.contains(&"apple") && words.contains(&"apply") && words.contains(&"banana"));
    }
}
