//! `dict/string_table.cc:73-112` 的 **写路径**：`StringTableBuilder`。
//!
//! `.table.bin` 里的 marisa trie 是"字符串 ↔ id"的唯一权威：`Table::Build` 先把每一条
//! 音节串、词条文本塞进 builder，等**所有**引用点（`StringType` 那 4 字节）都已经分配好，
//! 才建树、拿 id、回填。这个"先记地址、后回填"的顺序不能改：id 是 marisa 建树时按
//! **terminal 顺序**编的（不是插入序、也不是字典序），建树之前算不出来。
//!
//! # 与 C++ 的对应（逐函数）
//!
//! | C++ | Rust |
//! |---|---|
//! | `StringTableBuilder::Add(key, weight, &reference)` | [`StringTableBuilder::add`] |
//! | `Clear()` | [`StringTableBuilder::clear`] |
//! | `Build()`（建树 + `UpdateReferences`） | [`StringTableBuilder::build`] + [`StringTableBuilder::update_references`] |
//! | `UpdateReferences()` | [`StringTableBuilder::update_references`] |
//! | `Dump(ptr, size)` | [`StringTableBuilder::dump`] |
//! | `BinarySize()` | [`StringTableBuilder::binary_size`] |
//!
//! # 唯一的接口差异：`StringId*` → `usize` 绝对偏移
//!
//! C++ 的 `Add` 收的是 `&dest->str_id`（指向 mmap 的 `StringId*`），`UpdateReferences`
//! 直接 `*references_[i] = keys_[i].id()`。Rust 侧没有"指向镜像内部的稳定指针"，
//! 所以引用记成**字段在 [`MappedFile`] 里的绝对偏移**，回填时走
//! [`MappedFile::put_u32`]。语义完全一样（都是"建完树再写那 4 字节"），只是写的时候
//! 需要把镜像一起传进来。
//!
//! 因此 `Build()` 被拆成两步：`build()` 只建树拿 id（不碰镜像），
//! `update_references(&mut file)` 才回填。C++ 的调用点用
//! [`StringTableBuilder::build_and_update`] 一步等价（`Table::OnBuildFinish` 就是这么用的）。
//!
//! # 两个必须照抄的细节
//!
//! * **允许重复键**：`Add` 同一个字符串两次是合法的，marisa 把它们合并成一个 id，
//!   两个引用位置拿到**同一个** id（C++ 测试 `duplicate_keys_share_one_id` 钉住）。
//!   所以 `references.len()` 可能大于 `trie.len()`。
//! * **`Dump` 不分配空间**：镜像由调用方先 `Allocate<char>(BinarySize())` 再交给
//!   `dump`（C++ `Dump(ptr, size)` 也是"给块内存，你来写"）。空间不够时 C++ 只打一条
//!   `LOG(ERROR)` 就返回，这里返回 `Err`。
#![allow(dead_code)]

use crate::dict::marisa::{MarisaError, MarisaTrie};
use crate::dict::mapped_file::MappedFile;
use crate::dict::string_table::StringId;

/// `class StringTableBuilder`（`string_table.h` 尾部的写侧）的 Rust 形态。
///
/// 生命周期：`new` → 若干次 `add` → `build` → `update_references` / `dump`。
#[derive(Debug, Default)]
pub struct StringTableBuilder {
    /// `marisa::Keyset keys_`：插入序。`dump` 之后仍保留，方便测试核对。
    keys: Vec<String>,
    /// `Keyset` 里每个键的权重（Rime 传音节 0.0、词条 ln(weight)）。
    /// 默认 flags 下 marisa 不拿它排序，留着只为与 C++ `Add` 的参数完全对齐。
    weights: Vec<f32>,
    /// `vector<StringId*> references_`：换成"镜像里的绝对偏移"；`None` = C++ 的空指针。
    references: Vec<Option<usize>>,
    /// `Build()` 之后每个输入键的 id（与 `keys` 同序，重复键同 id）。
    ids: Vec<StringId>,
    /// `marisa::Trie trie_`
    trie: Option<MarisaTrie>,
    /// `trie_.io_size()` 那段镜像；`Build` 时就序列化好，`Dump` 只负责拷。
    image: Vec<u8>,
}

impl StringTableBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// `StringTableBuilder::Add`（`string_table.cc:73-78`）：
    /// `keys_.push_back(key, len, (float)weight); references_.push_back(reference);`
    ///
    /// `reference` 是 `table::StringType` 在镜像里的绝对偏移（见模块文档）。
    pub fn add(&mut self, key: impl Into<String>, weight: f64, reference: Option<usize>) {
        self.keys.push(key.into());
        self.weights.push(weight as f32);
        self.references.push(reference);
    }

    /// `StringTableBuilder::Clear`（`string_table.cc:80-84`）：trie / keys / references 全清。
    pub fn clear(&mut self) {
        self.keys.clear();
        self.weights.clear();
        self.references.clear();
        self.ids.clear();
        self.trie = None;
        self.image.clear();
    }

    /// 已收集的键数（C++ 是 `keys_.size()`，没有单独的方法）。
    pub fn num_keys(&self) -> usize {
        self.keys.len()
    }

    /// 建好的树（`Build` 之前是 `None`）。
    pub fn trie(&self) -> Option<&MarisaTrie> {
        self.trie.as_ref()
    }

    /// `Build()` 的**前半**（`string_table.cc:86-89`）：`trie_.build(keys_)` 并取回 id。
    ///
    /// C++ 的 `Build()` 紧接着调 `UpdateReferences()`；那一步要写镜像，所以拆开。
    pub fn build(&mut self) -> Result<(), MarisaError> {
        let keys: Vec<&str> = self.keys.iter().map(String::as_str).collect();
        let (trie, ids) = MarisaTrie::build(&keys, &self.weights)?;
        // C++ 的 `BinarySize()` = `trie_.io_size()`；这里顺手把镜像也序列化好，
        // `BinarySize` 与 `Dump` 都直接用（两者相等由 `marisa.rs` 的测试钉住）。
        self.image = trie.to_bytes()?;
        self.ids = ids;
        self.trie = Some(trie);
        Ok(())
    }

    /// `StringTableBuilder::UpdateReferences`（`string_table.cc:91-101`）：
    /// `*references_[i] = keys_[i].id()`。
    ///
    /// 长度不一致（还没 `build` / 手工 `clear` 过）时静默返回 —— 与 C++ 的
    /// `if (keys_.size() != references_.size()) return;` 一致。
    pub fn update_references(&mut self, file: &mut MappedFile) {
        if self.references.len() != self.ids.len() {
            return;
        }
        for (reference, id) in self.references.iter().zip(self.ids.iter()) {
            if let Some(position) = reference {
                file.put_u32(*position, *id);
            }
        }
    }

    /// C++ `Build()` 的完整语义：建树 + 回填（`Table::OnBuildFinish` 用这个）。
    pub fn build_and_update(&mut self, file: &mut MappedFile) -> Result<(), MarisaError> {
        self.build()?;
        self.update_references(file);
        Ok(())
    }

    /// `StringTableBuilder::BinarySize`（`string_table.cc:105` 里 `Dump` 的 `size` 上界）：
    /// `trie_.io_size()`。`Build` 之前是 0。
    pub fn binary_size(&self) -> usize {
        self.image.len()
    }

    /// `StringTableBuilder::Dump(ptr, size)`（`string_table.cc:103-112`）：
    /// 把 marisa 镜像写进已经分好的那片内存。
    ///
    /// C++ 在 `size < BinarySize()` 时只打一条 `LOG(ERROR)` 并返回；这里报错。
    pub fn dump(&self, file: &mut MappedFile, position: usize) -> Result<(), MarisaError> {
        if self.image.is_empty() {
            return Err(MarisaError("string table has not been built".to_string()));
        }
        let end = position
            .checked_add(self.image.len())
            .ok_or_else(|| MarisaError("string table image overflows".to_string()))?;
        if end > file.capacity() {
            return Err(MarisaError(format!(
                "insufficient memory to dump string table: need {end} bytes, file has {}",
                file.capacity()
            )));
        }
        file.put_bytes(position, &self.image);
        Ok(())
    }

    /// 镜像本体（`Dump` 的源）。测试 / 交叉校验用。
    pub fn image(&self) -> &[u8] {
        &self.image
    }

    /// 每个输入键的 id（插入序）。
    pub fn ids(&self) -> &[StringId] {
        &self.ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::string_table::StringTable;

    /// 在镜像里给 `n` 个 `StringId` 字段开位置（模拟 `table::StringType` 数组）。
    fn reference_slots(file: &mut MappedFile, n: usize) -> Vec<usize> {
        (0..n).map(|_| file.allocate(4, 4)).collect()
    }

    #[test]
    fn build_dump_and_update_references_round_trip() {
        let keys = ["a", "ab", "abc", "b", "你好"];
        let mut file = MappedFile::create("unused", 512).expect("镜像");
        let slots = reference_slots(&mut file, keys.len());
        let mut builder = StringTableBuilder::new();
        for (key, slot) in keys.iter().zip(slots.iter()) {
            builder.add(*key, 0.0, Some(*slot));
        }
        assert_eq!(builder.num_keys(), keys.len());
        builder
            .build_and_update(&mut file)
            .expect("建树 + 回填应当成功");

        // ① 镜像长度 == BinarySize == trie.io_size()
        assert!(!builder.image().is_empty());
        assert_eq!(builder.binary_size(), builder.image().len());
        let trie = builder.trie().expect("trie");
        assert_eq!(builder.binary_size(), trie.io_size());
        assert_eq!(trie.len(), keys.len());

        // ② 回填：每个引用点现在都是该键的 id，且能被读侧 StringTable 读回原串
        let table = StringTable::from_bytes(builder.image()).expect("读回字符串表");
        for (key, slot) in keys.iter().zip(slots.iter()) {
            let id = file.u32_at(*slot).expect("引用字段");
            assert_eq!(table.lookup(key), Some(id), "{key} 的 id 应已回填");
            assert_eq!(table.get_string(id).as_deref(), Some(*key));
        }

        // ③ dump：把同一段镜像写进另一份文件，读侧结果一致
        let mut other = MappedFile::create("unused", 512).expect("镜像");
        let at = other.allocate(builder.binary_size(), 1);
        builder.dump(&mut other, at).expect("dump");
        assert_eq!(other.bytes_at(at, builder.binary_size()), Some(builder.image()));
    }

    #[test]
    fn duplicate_keys_share_one_id_but_keep_two_references() {
        let mut file = MappedFile::create("unused", 128).expect("镜像");
        let slots = reference_slots(&mut file, 3);
        let mut builder = StringTableBuilder::new();
        for (key, slot) in ["x", "x", "y"].iter().zip(slots.iter()) {
            builder.add(*key, 0.0, Some(*slot));
        }
        builder.build_and_update(&mut file).expect("建树");
        // trie 只认 2 个键，但 3 个引用位置都写了
        assert_eq!(builder.trie().expect("trie").len(), 2);
        assert_eq!(builder.ids().len(), 3);
        let x0 = file.u32_at(slots[0]).expect("slot 0");
        let x1 = file.u32_at(slots[1]).expect("slot 1");
        let y = file.u32_at(slots[2]).expect("slot 2");
        assert_eq!(x0, x1, "重复键共享 id");
        assert_ne!(x0, y);
    }

    #[test]
    fn references_may_be_absent_and_update_is_a_noop_before_build() {
        let mut file = MappedFile::create("unused", 128).expect("镜像");
        let slot = file.allocate(4, 4);
        let mut builder = StringTableBuilder::new();
        builder.add("only", 1.0, Some(slot));
        builder.add("null-ref", 1.0, None);
        // 还没 build：ids 为空，静默返回（C++ 的长度检查）
        builder.update_references(&mut file);
        assert_eq!(file.u32_at(slot), Some(0));
        builder.build_and_update(&mut file).expect("建树");
        assert_ne!(file.u32_at(slot), Some(0), "有引用时必须回填");
    }

    #[test]
    fn clear_resets_everything() {
        let mut file = MappedFile::create("unused", 256).expect("镜像");
        let mut builder = StringTableBuilder::new();
        builder.add("a", 0.0, Some(file.allocate(4, 4)));
        builder.build_and_update(&mut file).expect("建树");
        assert_eq!(builder.num_keys(), 1);
        builder.clear();
        assert_eq!(builder.num_keys(), 0);
        assert_eq!(builder.binary_size(), 0);
        assert!(builder.trie().is_none());
        assert!(builder.ids().is_empty());
    }

    #[test]
    fn dump_requires_build_and_enough_room() {
        let mut builder = StringTableBuilder::new();
        builder.add("a", 0.0, None);
        let mut file = MappedFile::create("unused", 4096).expect("镜像");
        assert!(builder.dump(&mut file, 0).is_err(), "没 build 不能 dump");
        builder.build_and_update(&mut file).expect("建树");
        let mut tiny = MappedFile::create("unused", 4).expect("镜像");
        assert!(builder.dump(&mut tiny, 0).is_err(), "空间不够必须报错");
        assert!(builder.dump(&mut file, 0).is_ok());
    }

    #[test]
    fn empty_builder_builds_valid_empty_trie() {
        // 空 keyset 建有效空树（rsmarisa 实证 3432 字节，与 C++ `trie_.build` 一致），
        // `.reverse.bin` 空词库路径（rev_table/stems 都空）依赖这一点。
        let mut builder = StringTableBuilder::new();
        builder.build().expect("空 builder 也应能建树");
        assert_eq!(builder.num_keys(), 0);
        assert!(builder.ids().is_empty());
        assert!(builder.binary_size() > 0, "空树镜像非零");
        let mut file = MappedFile::create("unused", 8192).expect("镜像");
        assert!(builder.dump(&mut file, 0).is_ok(), "空树也能 dump");
    }
}
