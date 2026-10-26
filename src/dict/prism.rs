//! `dict/prism.h`(106 行) + `dict/prism.cc`(351 行) 的 Rust 移植 —— **读 + 写**。
//!
//! `.prism.bin` 是「拼写串 → 拼写 id」的 darts 双数组 trie，外加一张
//! 「拼写 id → 一组 (音节 id, 属性)」的拼写表。音节切分（`algo::syllabifier`）靠它。
//!
//! # 文件布局
//!
//! `Prism::Build`（`prism.cc:142-261`）的**分配顺序就是磁盘布局**；读的时候只信
//! metadata 里的两个 `OffsetPtr`，不按固定偏移硬啃：
//!
//! ```text
//! 0                        Metadata                   320 B（字段全 4 字节对齐，无 padding）
//! deref(52)                 darts 镜像                 double_array_size × 4 B（小端 u32）
//! deref(56)                 Array<SpellingMapItem>     u32 count + count × (u32 size + i32 at)
//! 第一个 List::at           描述符数组                 每条 16 B
//! CopyString 分配            tips 字符串                NUL 结尾
//! ```
//!
//! 四段的起点**全部由 metadata 推出来**：
//!
//! * `format` @0..32、5 个 u32 @32..52、`double_array`/`spelling_map` 两个 `OffsetPtr`
//!   @52/@56、`alphabet` @60..316、`max_key_length` @316..320（见 [`metadata_layout`]）；
//! * darts 段起点 = `deref(52)`（i32，**相对该字段自身**，见 `dict/mapped_file.rs`），
//!   长度 = `double_array_size × 4`；
//! * 拼写表起点 = `deref(56)`；第 `i` 个 `List` 在 `拼写表 + 4 + i×8`，它的描述符数组
//!   起点 = `deref(List + 4)`；
//! * 描述符 16 B：`syllable_id i32` + `type i32`（bit30 = `is_correction`）+
//!   `credibility f32` + `String tips`（4 B `OffsetPtr`）。
//!
//! 以 `%APPDATA%\Rime-Next\build\yuling\yuling.prism.bin`（6,074,404 B）为例：
//! metadata 0..320；`double_array_size` = 279,552 → darts 320..1,118,528；
//! `spelling_map` 指针恰好指到 1,118,528（= 320 + 279,552×4）；134,042 个 `List` 头占
//! 4 + 134,042×8 = 1,072,340 B，所以描述符数组从 2,190,868 起；
//! 这个文件的 242,721 条描述符 `tips` 全为空，于是描述符区正好铺到文件尾 6,074,404
//! （真实文件由 `reads_real_yuling_prism` 这条测试逐项校验，不是手抄的）。
//!
//! # 版本与两个"老格式"
//!
//! * `format` 必须是 `Rime::Prism/` 前缀，版本 < 5.0 直接判旧、拒绝加载
//!   （`prism.cc:102-115`；`parse_format_version` 对应 C++ 的 `atof`）。
//! * 拼写表是 **v1.0** 加的：只有 `format > 1.0` 才 `spelling_map_ = metadata->spelling_map`
//!   （`prism.cc:127-130`）。没有拼写表时（或 `spelling_id` 越界时）C++ 的
//!   [`SpellingAccessor`] **不空**：它把拼写 id 自己当音节 id 返回，属性全默认 —— 这里照抄。
//! * `alphabet` 是 **v1.0** 加的，`max_key_length` 是 **v5.0** 加的；
//!   [`Prism::expand_search`] 在 `format <= 1.0` 时退回常量
//!   `"abcdefghijklmnopqrstuvwxyz"`（`prism.cc:307-308`）。
//!
//! # 与 C++ 的接口差异（接缝）
//!
//! * **mmap 换成 `Vec<u8>` 镜像**：所有访问都带边界检查，C++ 的裸指针越界是 UB，
//!   这里返回 `None`（坏文件不会把进程带走）。对应地，`Load()` 的 `bool` 换成了
//!   `io::Result<()>`，错误文案沿用 C++ 的日志文案。
//! * **`metadata_` / `spelling_map_` 从指针换成"解析好的值 + 绝对偏移"**：
//!   C++ 把 mmap 里的结构体当结构体用，这里解成 [`Metadata`]（纯值），
//!   两个 `OffsetPtr` 解出来的绝对偏移留着，后面的 `List` 还得顺着它走。
//! * **`SpellingProperties` 的 `tips` / `is_correction` 填不进去**：它们在
//!   `algo::spelling` 里是 `pub(super)`，dict 层只拿得到 `type` + `credibility`。
//!   完整信息走 [`SpellingAccessor::descriptor`]（[`SpellingDescriptor`] 带全部四个字段）。
//!   这是 `dict.rs` 模块文档里记的那条"已知缺口"，不是漏抄。
//! * **`SpellingAccessor` 的下标是 `Option<usize>`**（C++ 是 `iter_` / `end_` 两个裸指针）：
//!   `None` 对应 `iter_ == NULL`，`Some(i)` 对应第 `i` 条。
//! * **`Save` / `Build` 是关联函数**（[`Prism::build`] / [`Prism::save`]）：
//!   C++ 在已打开的可写 `MappedFile` 上原地建；Rust 侧 `build` 自己 `Create` 镜像，
//!   返回已 `Load` 的 `Prism`（`file` 仍是可写模式，所以紧接着的 `save` 有效）。
//!   [`Prism::compile`] = `Build` + `Save` 一步到位。
//!
//! # 写路径（`Prism::Build`，`prism.cc:142-261`）
//!
//! 六个阶段的**顺序**就是磁盘布局：
//!
//! 1. 收键：`script` 有就用它的键（`std::map` 键序 = 拼写 id 序），否则用音节表；
//! 2. `trie_->build()`：darts 的**值 = 键下标**（`darts.rs::build` 的约定）；
//! 3. `Create(image_size + estimated_map_size + 1024)`；
//! 4. `Allocate<Metadata>()`（320B @0）+ `alphabet`（键字节的 `set<char>`，有符号序）；
//! 5. `Allocate<char>(image_size)` 拷 darts 镜像；
//! 6. `CreateArray<SpellingMapItem>(num_spellings)` + 每个 `List` 的描述符数组
//!    （`syllable_id` 走 `syllable_to_id`，`type` 打包纠错位，`tips` 走 `CopyString`）。
//!
//! `format` 最后才写（`prism.cc:257-259`）：中途失败的文件没有合法 `format`，
//! `Load` 会拒绝，等于天然的事务标记。
//!
//! ⚠️ 拼写描述符里的 `tips` / `is_correction` 在 `algo::SpellingProperties` 里是
//! `pub(super)`，`dict` 层读不到，所以 script 路径写出的这两格是空的（见
//! [`Prism::build`] 的"已知缺口"）。`type` / `credibility` / `syllable_id` 都照抄。
//!
//! # 读侧的边界（写路径不影响它）
//!
//! 加载路径仍然只认 v5.0：`format` 前缀、版本、`double_array` 三段检查缺一不可。
#![allow(dead_code)]

use std::collections::{BTreeSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};

use crate::algo::{Script, SpellingProperties, SpellingType};
use crate::dict::darts::{DoubleArray, TRAVERSE_INVALID, TRAVERSE_NO_VALUE};
use crate::dict::mapped_file::{MappedFile, parse_format_version};
use crate::dict::vocabulary::{Syllabary, SyllableId};

/// `kPrismFormat`（`prism.cc:32`）
pub const K_PRISM_FORMAT: &str = "Rime::Prism/5.0";
/// `kPrismFormatPrefix`（`prism.cc:35`）：`Load()` 的第一道校验
pub const K_PRISM_FORMAT_PREFIX: &str = "Rime::Prism/";
/// `kPrismFormatVersion`（`prism.cc:33`）：低于它的文件一律要求重建
pub const K_PRISM_FORMAT_VERSION: f64 = 5.0;
/// 允许加载的最低格式版本。真实 librime 产出的 `.prism.bin` 多为 **4.0**，
/// 其 metadata 布局与 5.0 一致（差异仅在 `max_key_length` 字段，读侧不依赖它做搜索），
/// 所以这里接受 4.0+。低于此版本（v3 及更早）的布局元数据不同，仍拒绝。
pub const K_PRISM_FORMAT_MIN_VERSION: f64 = 4.0;
/// `kDefaultAlphabet`（`prism.cc:38`）：v1.0 之前的文件没有 `alphabet` 字段
pub const K_DEFAULT_ALPHABET: &str = "abcdefghijklmnopqrstuvwxyz";
/// `kTypeIsCorrectionMask`（`prism.cc:27`）：`type` 的 bit 30 记 `is_correction`，
/// 避开符号位（`1 << 31` 会把 `type` 变成负数）
pub const K_TYPE_IS_CORRECTION_MASK: i32 = 1 << 30;
/// `kSpellingTypeMask`（`prism.cc:28`）：掩掉 bit 30 后剩下的就是 `SpellingType`
pub const K_SPELLING_TYPE_MASK: i32 = !K_TYPE_IS_CORRECTION_MASK;

/// `using Credibility = float;`（`prism.h:23`）
pub type Credibility = f32;

/// `sizeof(prism::SpellingDescriptor)`：`int32 + int32 + float + String(4)` = 16
pub const SPELLING_DESCRIPTOR_SIZE: usize = 16;

/// `Array<SpellingMapItem>` 里一个元素的字节数：`List` = `u32 size` + `i32 at` = 8
pub const SPELLING_MAP_ITEM_SIZE: usize = 8;

/// `prism::Metadata`（`prism.h:36-50`）的字节布局。
///
/// ```text
/// 偏移  字段                     类型
/// 0     format                   char[32]
/// 32    dict_file_checksum       u32
/// 36    schema_file_checksum     u32
/// 40    num_syllables            u32
/// 44    num_spellings            u32
/// 48    double_array_size        u32   （单位数，不是字节数！）
/// 52    double_array             OffsetPtr<char>
/// 56    spelling_map             OffsetPtr<SpellingMap>   （v1.0）
/// 60    alphabet                 char[256]                （v1.0）
/// 316   max_key_length           u32                      （v5.0）
/// = 320
/// ```
///
/// 没有 padding：`char[32]` 之后全是 4 字节对齐的字段，`char[256]` 之后跟一个 u32。
pub mod metadata_layout {
    /// `sizeof(prism::Metadata)`
    pub const SIZE: usize = 320;
    /// `Metadata::kFormatMaxLength`
    pub const FORMAT_MAX_LENGTH: usize = 32;
    pub const FORMAT_POS: usize = 0;
    pub const DICT_FILE_CHECKSUM_POS: usize = FORMAT_POS + FORMAT_MAX_LENGTH; // 32
    pub const SCHEMA_FILE_CHECKSUM_POS: usize = DICT_FILE_CHECKSUM_POS + 4; // 36
    pub const NUM_SYLLABLES_POS: usize = SCHEMA_FILE_CHECKSUM_POS + 4; // 40
    pub const NUM_SPELLINGS_POS: usize = NUM_SYLLABLES_POS + 4; // 44
    pub const DOUBLE_ARRAY_SIZE_POS: usize = NUM_SPELLINGS_POS + 4; // 48
    pub const DOUBLE_ARRAY_POS: usize = DOUBLE_ARRAY_SIZE_POS + 4; // 52
    pub const SPELLING_MAP_POS: usize = DOUBLE_ARRAY_POS + 4; // 56
    pub const ALPHABET_POS: usize = SPELLING_MAP_POS + 4; // 60
    pub const ALPHABET_MAX_LENGTH: usize = 256;
    pub const MAX_KEY_LENGTH_POS: usize = ALPHABET_POS + ALPHABET_MAX_LENGTH; // 316
}

/// `prism::Metadata` 解出来的**值**（C++ 那边是指向 mmap 的裸指针）。
#[derive(Debug, Clone, PartialEq)]
pub struct Metadata {
    /// `format[32]`：NUL 结尾，正常是 `"Rime::Prism/5.0"`
    pub format: String,
    pub dict_file_checksum: u32,
    pub schema_file_checksum: u32,
    pub num_syllables: u32,
    pub num_spellings: u32,
    /// darts 的**单位数**（不是字节数）：`× 4` 才是镜像长度
    pub double_array_size: u32,
    /// `OffsetPtr<char>` 解出来的绝对偏移（`None` = NULL）
    pub double_array: Option<usize>,
    /// `OffsetPtr<SpellingMap>` 解出来的绝对偏移（`None` = NULL）
    pub spelling_map: Option<usize>,
    /// `alphabet[256]` 里 NUL 之前的字节（`set<char>` 的排序结果，可能含 `/` 之类的符号）
    pub alphabet: Vec<u8>,
    pub max_key_length: u32,
}

impl Metadata {
    /// `Find<prism::Metadata>(0)` + 逐字段读。整段越界 → `None`（C++ 是 UB）。
    pub fn read(file: &MappedFile, pos: usize) -> Option<Self> {
        let format = file
            .fixed_str_at(
                pos + metadata_layout::FORMAT_POS,
                metadata_layout::FORMAT_MAX_LENGTH,
            )?
            .to_string();
        // alphabet 是 char[256]，不是 String：NUL 之前可能有不连续的"乱七八糟"字节
        // （真实文件里就是 `/abcdefghijklmnopqrstuvwxyz`），所以按字节读，不做 UTF-8 校验。
        let raw = file.bytes_at(
            pos + metadata_layout::ALPHABET_POS,
            metadata_layout::ALPHABET_MAX_LENGTH,
        )?;
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        Some(Self {
            format,
            dict_file_checksum: file.u32_at(pos + metadata_layout::DICT_FILE_CHECKSUM_POS)?,
            schema_file_checksum: file.u32_at(pos + metadata_layout::SCHEMA_FILE_CHECKSUM_POS)?,
            num_syllables: file.u32_at(pos + metadata_layout::NUM_SYLLABLES_POS)?,
            num_spellings: file.u32_at(pos + metadata_layout::NUM_SPELLINGS_POS)?,
            double_array_size: file.u32_at(pos + metadata_layout::DOUBLE_ARRAY_SIZE_POS)?,
            double_array: file.deref(pos + metadata_layout::DOUBLE_ARRAY_POS),
            spelling_map: file.deref(pos + metadata_layout::SPELLING_MAP_POS),
            alphabet: raw[..end].to_vec(),
            max_key_length: file.u32_at(pos + metadata_layout::MAX_KEY_LENGTH_POS)?,
        })
    }

    /// `atof(&format[13])` 的结果（`prism.cc:107`）。
    pub fn format_version(&self) -> Option<f64> {
        parse_format_version(&self.format, K_PRISM_FORMAT_PREFIX).map(|(version, _)| version)
    }

    /// `alphabet` 的字符串形态（合法 UTF-8 才有；真实方案都是 ASCII）。
    pub fn alphabet_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.alphabet).ok()
    }
}

/// `prism::SpellingDescriptor`（`prism.h:25-31`）。
///
/// `String tips` 在文件里是 `OffsetPtr`；这里读成 `String`（NULL → 空串）。
#[derive(Debug, Clone, PartialEq)]
pub struct SpellingDescriptor {
    /// 音节 id（`< num_syllables`）
    pub syllable_id: SyllableId,
    /// 拼写类型 + bit30 的 `is_correction`，**原样**保留（见 [`Self::is_correction`]）
    pub r#type: i32,
    pub credibility: Credibility,
    pub tips: String,
}

impl SpellingDescriptor {
    /// `(packed_type & kTypeIsCorrectionMask) != 0`（`prism.cc:74`）
    pub fn is_correction(&self) -> bool {
        self.r#type & K_TYPE_IS_CORRECTION_MASK != 0
    }

    /// `packed_type & kSpellingTypeMask`：剥掉纠错位之后的 `SpellingType` 数值。
    pub fn unpacked_type(&self) -> i32 {
        self.r#type & K_SPELLING_TYPE_MASK
    }

    /// 转成 `algo::SpellingType`。
    ///
    /// C++ 是 `static_cast<SpellingType>(type & mask)`：越界值是 UB。这里把
    /// 0..=4 映射到对应枚举，其余（含负数）一律 [`SpellingType::Invalid`]。
    pub fn spelling_type(&self) -> SpellingType {
        match self.unpacked_type() {
            0 => SpellingType::Normal,
            1 => SpellingType::Fuzzy,
            2 => SpellingType::Abbreviation,
            3 => SpellingType::Completion,
            4 => SpellingType::Ambiguous,
            _ => SpellingType::Invalid,
        }
    }
}

/// `Prism::Match`（`prism.h:72`）= darts 的 `result_pair_type`：`{ value, length }`。
///
/// `length` 是**字节**长度（`key.length()` 的语义），不是字符数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    /// darts 叶子里的值 = 拼写 id
    pub value: SyllableId,
    pub length: usize,
}

/// `class Prism : public MappedFile`（`prism.h:70-102`）—— 只读形态。
#[derive(Debug, Clone)]
pub struct Prism {
    /// C++ 的 `MappedFile` 基类
    file: MappedFile,
    /// C++ 的 `metadata_`（这里解析成值）
    metadata: Option<Metadata>,
    /// C++ 的 `spelling_map_`：**只有 `format > 1.0` 才非 NULL**（`prism.cc:127-130`）
    spelling_map: Option<usize>,
    /// C++ 的 `trie_`
    trie: DoubleArray,
    /// C++ 的 `format_`
    format: f64,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

impl Prism {
    // ---------------------------------------------------------------- 打开 / 加载

    /// `Prism::Prism(path)` + `Load()` 合体（C++ 构造完还要显式调一次 `Load()`）。
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let file = MappedFile::open_read_only(path)?;
        Self::from_mapped_file(file)
    }

    /// 从一份已经有字节的 [`MappedFile`] 加载（测试用；也是将来 `Build` 之后复用的入口）。
    pub fn from_mapped_file(file: MappedFile) -> io::Result<Self> {
        let mut prism = Self {
            file,
            metadata: None,
            spelling_map: None,
            trie: DoubleArray::default(),
            format: 0.0,
        };
        prism.load()?;
        Ok(prism)
    }

    /// `Prism::Load()`（`prism.cc:85-132`）。
    ///
    /// 与 C++ 一一对应的五道检查：metadata 在 → `format` 前缀对 → 版本 >= 5.0 →
    /// `double_array` 非空 → 镜像在文件内。失败时把内部状态清空（C++ 是 `Close()`）。
    pub fn load(&mut self) -> io::Result<()> {
        self.metadata = None;
        self.spelling_map = None;
        self.trie = DoubleArray::default();
        self.format = 0.0;

        let Some(metadata) = Metadata::read(&self.file, 0) else {
            return Err(invalid("metadata not found"));
        };
        // C++：`strncmp(metadata_->format, kPrismFormatPrefix, 13)`
        if !metadata.format.starts_with(K_PRISM_FORMAT_PREFIX) {
            return Err(invalid("invalid metadata"));
        }
        // C++：`atof(&format[13])`；解析不出数字时 atof 给 0.0，会落到"太旧"分支。
        let format = parse_format_version(&metadata.format, K_PRISM_FORMAT_PREFIX)
            .map(|(version, _)| version)
            .unwrap_or(0.0);
        if format < K_PRISM_FORMAT_MIN_VERSION - f64::EPSILON {
            return Err(invalid(format!(
                "prism format {format} is too old. upgrading to {K_PRISM_FORMAT_VERSION}"
            )));
        }
        let Some(array_pos) = metadata.double_array else {
            return Err(invalid("double array image not found"));
        };
        let units = metadata.double_array_size as usize;
        // `double_array_size` 是**单位数**：镜像字节数 = units × 4
        let bytes = units
            .checked_mul(4)
            .and_then(|len| self.file.bytes_at(array_pos, len))
            .ok_or_else(|| invalid("double array image is out of range"))?;
        let Some(trie) = DoubleArray::from_bytes(bytes, units) else {
            return Err(invalid("double array image is out of range"));
        };

        self.spelling_map = if format > 1.0 - f64::EPSILON {
            metadata.spelling_map
        } else {
            None
        };
        self.format = format;
        self.metadata = Some(metadata);
        self.trie = trie;
        Ok(())
    }

    // ---------------------------------------------------------------- 写路径
    //
    // 对应 `prism.cc:134-261`：`Save` / `Build`。C++ 的 6 个阶段顺序不能换：
    // ① 收键（script 优先，否则音节表）→ ② `trie_->build()` → ③ `Create(估算容量)` →
    // ④ metadata（320B @0）→ ⑤ 拷 darts 镜像 → ⑥ 拼写表 → 最后才写 `format`。

    /// `Prism::Build`（`prism.cc:142-261`）：建一份**可写**的 `.prism.bin` 镜像并 `Load`。
    ///
    /// * `script == None`：键 = 音节表（C++ `Prism::Build(syllabary, nullptr, ...)`），
    ///   不写拼写表（`metadata->spelling_map` 保持 NULL），读侧退化成"拼写 id 即音节 id"；
    /// * `script == Some(...)`：键 = 拼写串（`std::map` 的键序 = id 序），逐条写
    ///   `SpellingDescriptor`。
    ///
    /// ⚠️ **已知缺口**：`algo::SpellingProperties` 的 `tips` / `is_correction` 是
    /// `pub(super)`（`algo` 私有），`dict` 层读不到，所以 script 路径写出的描述符
    /// 只带 `type` + `credibility`，`tips` 为空、纠错位为 0。不是漏抄，是模块边界；
    /// 完整属性在 `prism::SpellingAccessor::descriptor`（读侧）里是可得的。
    pub fn build(
        path: impl Into<PathBuf>,
        syllabary: &Syllabary,
        script: Option<&Script>,
        dict_file_checksum: u32,
        schema_file_checksum: u32,
    ) -> io::Result<Self> {
        // ① 收键：`keys` 的顺序就是 darts 的 value（键下标）；script 用 `std::map` 的键序。
        let num_syllables = syllabary.size();
        let (keys, map_size, max_key_length): (Vec<&str>, usize, usize) = match script {
            Some(script) => {
                let mut keys = Vec::with_capacity(script.len());
                let mut map_size = 0usize;
                let mut max_key_length = 0usize;
                for (spelling, properties_list) in script.iter() {
                    keys.push(spelling.as_str());
                    max_key_length = max_key_length.max(spelling.len());
                    map_size += properties_list.len();
                }
                (keys, map_size, max_key_length)
            }
            None => {
                let keys: Vec<&str> = syllabary.iter().map(String::as_str).collect();
                let max_key_length = keys.iter().map(|key| key.len()).max().unwrap_or(0);
                (keys, 0, max_key_length)
            }
        };
        let num_spellings = keys.len();
        // ② `trie_->build(num_spellings, &keys[0])`：值 = 键在 keys 里的下标。
        let key_bytes: Vec<&[u8]> = keys.iter().map(|key| key.as_bytes()).collect();
        let trie = DoubleArray::build(&key_bytes)
            .ok_or_else(|| invalid("Error building double-array trie."))?;
        let array_size = trie.size();
        let image_size = trie.total_size();

        // ③ `Create(image_size + estimated_map_size + kReservedSize)`。
        /// `const size_t kDescriptorExtraSize = 12;`（`prism.cc:172`）
        const K_DESCRIPTOR_EXTRA_SIZE: usize = 12;
        /// `const size_t kReservedSize = 1024;`（`prism.cc:176`）
        const K_RESERVED_SIZE: usize = 1024;
        let estimated_map_size = num_spellings * 12
            + map_size * (4 + SPELLING_DESCRIPTOR_SIZE + K_DESCRIPTOR_EXTRA_SIZE);
        let mut file =
            MappedFile::create(path, image_size + estimated_map_size + K_RESERVED_SIZE)?;

        // ④ `Allocate<prism::Metadata>()`：一定落在 0（`prism.cc:182`）。
        let metadata_pos = file.allocate(metadata_layout::SIZE, 4);
        debug_assert_eq!(metadata_pos, 0, "metadata 必须落在偏移 0");
        file.put_u32(
            metadata_pos + metadata_layout::DICT_FILE_CHECKSUM_POS,
            dict_file_checksum,
        );
        file.put_u32(
            metadata_pos + metadata_layout::SCHEMA_FILE_CHECKSUM_POS,
            schema_file_checksum,
        );
        file.put_u32(
            metadata_pos + metadata_layout::NUM_SYLLABLES_POS,
            num_syllables as u32,
        );
        file.put_u32(
            metadata_pos + metadata_layout::NUM_SPELLINGS_POS,
            num_spellings as u32,
        );
        file.put_u32(
            metadata_pos + metadata_layout::MAX_KEY_LENGTH_POS,
            max_key_length as u32,
        );

        // `set<char> alphabet`（`prism.cc:194-204`）：所有键字节的**有序去重**。
        // C++ 的 `char` 是有符号的，所以高位字节排在 ASCII 前面；`BTreeSet<i8>` 同序。
        let mut alphabet: BTreeSet<i8> = BTreeSet::new();
        for key in &keys {
            for &byte in key.as_bytes() {
                alphabet.insert(byte as i8);
            }
        }
        let mut cursor = metadata_pos + metadata_layout::ALPHABET_POS;
        for byte in alphabet {
            file.put_u8(cursor, byte as u8);
            cursor += 1;
        }
        file.put_u8(cursor, 0);

        // ⑤ `metadata->double_array` + `double_array_size`（`prism.cc:206-213`）。
        let array = file.allocate(image_size, 1);
        for (index, unit) in trie.units().iter().enumerate() {
            file.put_u32(array + index * 4, unit.0);
        }
        file.set_offset(metadata_pos + metadata_layout::DOUBLE_ARRAY_POS, array);
        file.put_u32(
            metadata_pos + metadata_layout::DOUBLE_ARRAY_SIZE_POS,
            array_size as u32,
        );

        // ⑥ 拼写表（`prism.cc:215-256`）：只有 script 才建（v1.0 起才有这张表）。
        if let Some(script) = script {
            let syllable_to_id = syllabary.syllable_to_id();
            let spelling_map = file.create_array(num_spellings, SPELLING_MAP_ITEM_SIZE, 4);
            for (index, (_spelling, properties_list)) in script.iter().enumerate() {
                let item = spelling_map + 4 + index * SPELLING_MAP_ITEM_SIZE;
                file.put_u32(item, properties_list.len() as u32);
                let descriptors =
                    file.allocate(properties_list.len() * SPELLING_DESCRIPTOR_SIZE, 4);
                file.set_offset(item + 4, descriptors);
                for (offset, properties) in properties_list.iter().enumerate() {
                    let descriptor = descriptors + offset * SPELLING_DESCRIPTOR_SIZE;
                    // `desc->syllable_id = syllable_to_id[j->str]`
                    let syllable_id = syllable_to_id
                        .get(properties.str.as_str())
                        .copied()
                        .ok_or_else(|| {
                            invalid(format!("syllable not in syllabary: {}", properties.str))
                        })?;
                    file.put_i32(descriptor, syllable_id);
                    // `packed_type = type | (is_correction ? kTypeIsCorrectionMask : 0)`
                    // （纠错位读不到，见方法文档的"已知缺口"）
                    file.put_i32(descriptor + 4, properties.properties.r#type as i32);
                    file.put_f32(descriptor + 8, properties.properties.credibility as f32);
                    // `if (!tips.empty()) CopyString(tips, &desc->tips)`：tips 读不到 → 保持 NULL。
                }
            }
            file.set_offset(metadata_pos + metadata_layout::SPELLING_MAP_POS, spelling_map);
        }

        // ⑦ 最后才写 `format`（`prism.cc:257-259`）：中途失败的文件 Load 会拒绝。
        let format = K_PRISM_FORMAT.as_bytes();
        file.put_bytes(metadata_pos + metadata_layout::FORMAT_POS, format);
        file.put_u8(
            metadata_pos + metadata_layout::FORMAT_POS + format.len(),
            0,
        );

        Self::from_mapped_file(file)
    }

    /// [`Self::build`] + [`Self::save`]：建好、落盘、可直接 `open` 的 `.prism.bin`。
    pub fn compile(
        path: impl Into<PathBuf>,
        syllabary: &Syllabary,
        script: Option<&Script>,
        dict_file_checksum: u32,
        schema_file_checksum: u32,
    ) -> io::Result<Self> {
        let mut prism = Self::build(
            path,
            syllabary,
            script,
            dict_file_checksum,
            schema_file_checksum,
        )?;
        prism.save()?;
        Ok(prism)
    }

    /// `Prism::Save`（`prism.cc:134-141`）：`if (!trie_->total_size()) return false;` 后
    /// `ShrinkToFit()`（C++ 只 shrink，落盘靠 munmap；这里是截断 + `fs::write`）。
    pub fn save(&mut self) -> io::Result<()> {
        if self.trie.total_size() == 0 {
            // C++：`LOG(ERROR) << "the trie has not been constructed!";`
            return Err(io::Error::other("the trie has not been constructed!"));
        }
        self.file.save()
    }

    // ---------------------------------------------------------------- 只读视图

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    /// 底层字节镜像（测试 / 布局校验用）。
    pub fn file(&self) -> &MappedFile {
        &self.file
    }

    /// `format_`：`Load()` 从 `format` 字符串里解析出的版本号。
    pub fn format(&self) -> f64 {
        self.format
    }

    pub fn metadata(&self) -> Option<&Metadata> {
        self.metadata.as_ref()
    }

    /// `Darts::DoubleArray& trie()`（`prism.h:95`）
    pub fn trie(&self) -> &DoubleArray {
        &self.trie
    }

    // ---------------------------------------------------------------- 查询

    /// `Prism::HasKey`（`prism.cc:263-266`）：`exactMatchSearch != -1`。
    pub fn has_key(&self, key: &str) -> bool {
        self.get_value(key).is_some()
    }

    /// `Prism::GetValue`（`prism.cc:268-275`）。
    ///
    /// ⚠️ C++ 用 `-1` 当"没找到"的哨兵值；这里把**文件里真的存了 `0xFFFF_FFFF`**
    /// 的情况也当没找到（与 C++ 逐字一致）。真实 prism 的值都是 `0..num_spellings`。
    pub fn get_value(&self, key: &str) -> Option<SyllableId> {
        let value = self.trie.exact_match_search(key.as_bytes())? as SyllableId;
        if value == -1 { None } else { Some(value) }
    }

    /// `Prism::CommonPrefixSearch`（`prism.cc:279-286`）：所有"是 `key` 前缀"的键，
    /// 按长度升序。C++ 空 key 直接返回。
    pub fn common_prefix_search(&self, key: &str) -> Vec<Match> {
        if key.is_empty() {
            return Vec::new();
        }
        // C++ 把 `key.length()` 同时当 max_num_results 传；darts 每个字符最多产出一条，
        // 所以这个上界永远不会截断真实结果。
        self.trie
            .common_prefix_search(key.as_bytes(), key.len())
            .into_iter()
            .map(|(value, length)| Match {
                value: value as SyllableId,
                length,
            })
            .collect()
    }

    /// `Prism::ExpandSearch`（`prism.cc:288-329`）：以 `key` 为前缀的键（补全用），
    /// 按 **BFS 序**返回；`limit == 0` 表示不限。
    pub fn expand_search(&self, key: &str, limit: usize) -> Vec<Match> {
        let mut result = Vec::new();
        let mut count = 0usize;
        let mut node_pos = 0usize;
        let mut key_pos = 0usize;
        let ret = self
            .trie
            .traverse(key.as_bytes(), &mut node_pos, &mut key_pos);
        // -2：这条路径根本不存在（`prism.cc:300-301`）
        if ret == TRAVERSE_INVALID {
            return result;
        }
        // != -1：key 自己就是一个键
        if ret != TRAVERSE_NO_VALUE {
            result.push(Match {
                value: ret,
                length: key_pos,
            });
            if limit != 0 {
                count += 1;
                if count >= limit {
                    return result;
                }
            }
        }
        let alphabet: &[u8] = if self.format > 1.0 - f64::EPSILON {
            self.metadata
                .as_ref()
                .map(|metadata| metadata.alphabet.as_slice())
                .unwrap_or_else(|| K_DEFAULT_ALPHABET.as_bytes())
        } else {
            K_DEFAULT_ALPHABET.as_bytes()
        };
        // C++ 用 `std::queue<node_t>`，是标准 BFS（先宽后深）
        let mut queue: VecDeque<Node> = VecDeque::new();
        queue.push_back(Node {
            node_pos,
            key_length: key_pos,
        });
        while let Some(node) = queue.pop_front() {
            // `for (const char* c = alphabet; *c; ++c)`：按**字节**遍历，不是字符
            for &c in alphabet {
                let mut next_node_pos = node.node_pos;
                let mut next_key_pos = 0usize;
                // C++：`trie_->traverse(c, next_node_pos, next_key_pos, 1)`
                let ret = self
                    .trie
                    .traverse(&[c], &mut next_node_pos, &mut next_key_pos);
                if ret <= TRAVERSE_INVALID {
                    continue;
                }
                let matched_length = node.key_length + next_key_pos;
                queue.push_back(Node {
                    node_pos: next_node_pos,
                    key_length: matched_length,
                });
                if ret >= 0 {
                    result.push(Match {
                        value: ret,
                        length: matched_length,
                    });
                    if limit != 0 {
                        count += 1;
                        if count >= limit {
                            return result;
                        }
                    }
                }
            }
        }
        result
    }

    /// `Prism::QuerySpelling`（`prism.cc:331-333`）
    pub fn query_spelling(&self, spelling_id: SyllableId) -> SpellingAccessor<'_> {
        SpellingAccessor::new(self, spelling_id)
    }

    /// `Prism::array_size`（`prism.cc:335-337`）：darts 的**单位数**。
    pub fn array_size(&self) -> usize {
        self.trie.size()
    }

    /// `Prism::max_key_length`（`prism.cc:339-341`）：v5.0 字段，没有 metadata 时 0。
    pub fn max_key_length(&self) -> usize {
        self.metadata
            .as_ref()
            .map(|metadata| metadata.max_key_length as usize)
            .unwrap_or(0)
    }

    pub fn dict_file_checksum(&self) -> u32 {
        self.metadata
            .as_ref()
            .map(|metadata| metadata.dict_file_checksum)
            .unwrap_or(0)
    }

    pub fn schema_file_checksum(&self) -> u32 {
        self.metadata
            .as_ref()
            .map(|metadata| metadata.schema_file_checksum)
            .unwrap_or(0)
    }

    // ---------------------------------------------------------------- 拼写表

    /// 拼写表起点（`spelling_map_`；没有表时为 `None`）。
    pub fn spelling_map_pos(&self) -> Option<usize> {
        self.spelling_map
    }

    /// `spelling_map->size` = `metadata->num_spellings`（`Array<SpellingMapItem>` 的元素数）。
    pub fn spelling_map_len(&self) -> usize {
        self.spelling_map
            .and_then(|pos| self.file.array_size(pos))
            .unwrap_or(0) as usize
    }

    /// 某个拼写 id 对应的 `List<SpellingDescriptor>` 里有多少条描述符。
    ///
    /// `None` = 没有拼写表 / id 越界（C++ 里 accessor 这时退化成"id 即音节 id"）。
    pub fn spelling_list_len(&self, spelling_id: SyllableId) -> Option<usize> {
        let item = self.spelling_item_pos(spelling_id)?;
        self.file.list_size(item).map(|size| size as usize)
    }

    /// 某个拼写 id 的描述符数组起点（`List<SpellingDescriptor>::at`）。
    pub fn spelling_list_pos(&self, spelling_id: SyllableId) -> Option<usize> {
        let item = self.spelling_item_pos(spelling_id)?;
        self.file.list_data(item)
    }

    /// `spelling_map->at[id]` 这个 `List` 头本身的位置。
    fn spelling_item_pos(&self, spelling_id: SyllableId) -> Option<usize> {
        let index = usize::try_from(spelling_id).ok()?;
        let map = self.spelling_map?;
        if index >= self.file.array_size(map)? as usize {
            return None;
        }
        self.file.array_item(map, index, SPELLING_MAP_ITEM_SIZE)
    }

    /// 读第 `index` 条描述符（`SpellingMap[id].at[index]`）。
    pub fn spelling_descriptor(
        &self,
        spelling_id: SyllableId,
        index: usize,
    ) -> Option<SpellingDescriptor> {
        let item = self.spelling_item_pos(spelling_id)?;
        if index >= self.file.list_size(item)? as usize {
            return None;
        }
        let pos = self.file.list_item(item, index, SPELLING_DESCRIPTOR_SIZE)?;
        self.read_descriptor(pos)
    }

    /// 拼写 id → 音节 id（取描述符列表第一条；多数拼音拼写一一对应一个音节）。
    ///
    /// 没有拼写表时退化成「id 即音节 id」，照 C++ `Prism::spelling_map_` 为 NULL 时的行为。
    /// 拼音类的拼写 prism（`common_prefix_search` 返回的是**拼写 id**）必须过这一道才能拿到
    /// 音节 id，而 `Table` 的词条编码用的是音节 id —— 不做转换就查不到多音节词。
    pub fn spelling_syllable_id(&self, spelling_id: SyllableId) -> SyllableId {
        match self.spelling_descriptor(spelling_id, 0) {
            Some(d) => d.syllable_id,
            None => spelling_id,
        }
    }

    /// 从描述符的绝对偏移读一条（`prism.h:25-31` 的字段顺序）。
    pub fn read_descriptor(&self, pos: usize) -> Option<SpellingDescriptor> {
        Some(SpellingDescriptor {
            syllable_id: self.file.i32_at(pos)?,
            r#type: self.file.i32_at(pos + 4)?,
            credibility: self.file.f32_at(pos + 8)?,
            // `String tips`：NULL → 空串；坏 UTF-8 也当空串（C++ 直接给 char*）
            tips: self.file.string_at(pos + 12).unwrap_or("").to_string(),
        })
    }

}

/// `struct node_t`（`prism.cc:18-24`）：`ExpandSearch` 的 BFS 队列元素。
#[derive(Debug, Clone, Copy)]
struct Node {
    node_pos: usize,
    key_length: usize,
}

/// `class SpellingAccessor`（`prism.h:54-66` + `prism.cc:40-80`）：
/// 遍历某个拼写 id 下的所有 [`SpellingDescriptor`]。
///
/// 用完即弃：构造 → 读当前 → [`Self::next`] → ... → [`Self::exhausted`]。
/// 注意 C++ 的语义：**构造完就停在第一条**（不需要先 `Next()`），
/// 而 `Next()` 返回的是"是否已耗尽"（不是"是否还有下一条"）。
#[derive(Debug, Clone)]
pub struct SpellingAccessor<'a> {
    prism: &'a Prism,
    /// C++ `spelling_id_`：`-1` = 已耗尽（`prism.cc:58-60`）
    spelling_id: SyllableId,
    /// C++ `iter_`：`None` = NULL；`Some(i)` = 第 i 条
    current: Option<usize>,
    /// C++ `end_`：列表长度
    end: usize,
}

impl<'a> SpellingAccessor<'a> {
    /// `SpellingAccessor::SpellingAccessor`（`prism.cc:40-48`）。
    ///
    /// C++ 的条件是 `spelling_map && spelling_id < map->size`；负数在 C++ 里会通过这个
    /// 检查然后越界读（UB），这里直接判成"没有表"。
    pub fn new(prism: &'a Prism, spelling_id: SyllableId) -> Self {
        match prism.spelling_list_len(spelling_id) {
            Some(end) => Self {
                prism,
                spelling_id,
                current: Some(0),
                end,
            },
            None => Self {
                prism,
                spelling_id,
                current: None,
                end: 0,
            },
        }
    }

    /// `exhausted()`（`prism.cc:58-60`）：`spelling_id_ == -1`
    pub fn exhausted(&self) -> bool {
        self.spelling_id == -1
    }

    /// `Next()`（`prism.cc:50-56`）：推进一格，返回 [`Self::exhausted`]。
    ///
    /// C++ 是"先 `++iter_` 再比 `end_`；`iter_` 为 NULL 时直接判耗尽"，这里照抄。
    pub fn next(&mut self) -> bool {
        if self.exhausted() {
            return false;
        }
        match self.current {
            None => self.spelling_id = -1,
            Some(index) => {
                let index = index + 1;
                self.current = Some(index);
                if index >= self.end {
                    self.spelling_id = -1;
                }
            }
        }
        self.exhausted()
    }

    /// `syllable_id()`（`prism.cc:62-67`）：当前描述符的音节 id；
    /// 不在有效位置时返回拼写 id 自己（C++ 的退化行为）。
    pub fn syllable_id(&self) -> SyllableId {
        match self.descriptor() {
            Some(descriptor) => descriptor.syllable_id,
            None => self.spelling_id,
        }
    }

    /// 当前描述符（完整四字段）；不在有效位置时 `None`。
    pub fn descriptor(&self) -> Option<SpellingDescriptor> {
        let index = self.current?;
        if index >= self.end {
            return None;
        }
        self.prism.spelling_descriptor(self.spelling_id, index)
    }

    /// `properties()`（`prism.cc:69-80`）。
    ///
    /// ⚠️ 只填 `type` + `credibility`：`tips` / `is_correction` 在 `algo::spelling`
    /// 里是 `pub(super)`，dict 层写不进去（`dict.rs` 记的那条已知缺口）。
    /// 需要完整信息用 [`Self::descriptor`]。
    pub fn properties(&self) -> SpellingProperties {
        let mut properties = SpellingProperties::default();
        if let Some(descriptor) = self.descriptor() {
            properties.r#type = descriptor.spelling_type();
            properties.credibility = f64::from(descriptor.credibility);
        }
        properties
    }

    /// 当前下标（`iter_ - begin`）；耗尽/无表时 `None`。测试与诊断用。
    pub fn index(&self) -> Option<usize> {
        match self.current {
            Some(index) if index < self.end => Some(index),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::mapped_file::MappedFile;

    // ------------------------------------------------------------------ 辅助

    /// 写一份 metadata（对应 `prism.cc:182-213` + `:257-259` 的字段填写顺序）。
    fn write_metadata(
        file: &mut MappedFile,
        pos: usize,
        alphabet: &[u8],
        numbers: [u32; 3], // num_syllables, num_spellings, max_key_length
        double_array_size: u32,
        double_array: Option<usize>,
        spelling_map: Option<usize>,
    ) {
        file.put_bytes(pos + metadata_layout::FORMAT_POS, K_PRISM_FORMAT.as_bytes());
        file.put_u8(pos + metadata_layout::FORMAT_POS + K_PRISM_FORMAT.len(), 0);
        file.put_u32(pos + metadata_layout::DICT_FILE_CHECKSUM_POS, 0xcafe_0001);
        file.put_u32(pos + metadata_layout::SCHEMA_FILE_CHECKSUM_POS, 0xcafe_0002);
        file.put_u32(pos + metadata_layout::NUM_SYLLABLES_POS, numbers[0]);
        file.put_u32(pos + metadata_layout::NUM_SPELLINGS_POS, numbers[1]);
        file.put_u32(
            pos + metadata_layout::DOUBLE_ARRAY_SIZE_POS,
            double_array_size,
        );
        if let Some(double_array) = double_array {
            file.set_offset(pos + metadata_layout::DOUBLE_ARRAY_POS, double_array);
        }
        if let Some(spelling_map) = spelling_map {
            file.set_offset(pos + metadata_layout::SPELLING_MAP_POS, spelling_map);
        }
        file.put_bytes(pos + metadata_layout::ALPHABET_POS, alphabet);
        file.put_u32(pos + metadata_layout::MAX_KEY_LENGTH_POS, numbers[2]);
    }

    /// 造一份完整、合法的 `.prism.bin` 镜像（不落盘）。
    ///
    /// 键 = `["a", "ab", "abc", "b", "ba"]`（值 = 下标），
    /// 拼写表 = `[[(7, Normal, -0.5, "gong")], [], [(1, Abbreviation+纠错, -1.25), (3, Normal, 0, "tip3")], [], [(9, Normal, 0)]]`。
    fn synthetic_prism() -> Prism {
        let keys: [&[u8]; 5] = [b"a", b"ab", b"abc", b"b", b"ba"];
        let trie = DoubleArray::build(&keys).expect("darts build");
        let mut file = MappedFile::create("unused", 8192).expect("create");

        // 1) metadata 占位（偏移 0）
        let metadata_pos = file.allocate(metadata_layout::SIZE, 4);
        assert_eq!(metadata_pos, 0);
        // 2) darts 镜像
        let image_pos = file.allocate(trie.total_size(), 1);
        let mut image = Vec::with_capacity(trie.total_size());
        for unit in trie.units() {
            image.extend_from_slice(&unit.0.to_le_bytes());
        }
        file.put_bytes(image_pos, &image);
        // 3) 拼写表
        let lists: Vec<Vec<(SyllableId, i32, f32, &str)>> = vec![
            vec![(7, 0, -0.5, "gong")],
            vec![],
            vec![
                (1, K_TYPE_IS_CORRECTION_MASK | 2, -1.25, ""),
                (3, 0, 0.0, "tip3"),
            ],
            vec![],
            vec![(9, 0, 0.0, "")],
        ];
        let map_pos = file.create_array(lists.len(), SPELLING_MAP_ITEM_SIZE, 4);
        for (i, list) in lists.iter().enumerate() {
            let item = file.array_item(map_pos, i, SPELLING_MAP_ITEM_SIZE).unwrap();
            file.put_u32(item, list.len() as u32);
            let data = file.allocate(list.len() * SPELLING_DESCRIPTOR_SIZE, 4);
            file.set_offset(item + 4, data);
            for (j, &(syllable_id, r#type, credibility, tips)) in list.iter().enumerate() {
                let descriptor = data + j * SPELLING_DESCRIPTOR_SIZE;
                file.put_i32(descriptor, syllable_id);
                file.put_i32(descriptor + 4, r#type);
                file.put_f32(descriptor + 8, credibility);
                if !tips.is_empty() {
                    file.copy_string(tips, descriptor + 12);
                }
            }
        }
        // 4) 最后写 metadata 本体
        write_metadata(
            &mut file,
            metadata_pos,
            b"abc",
            [10, lists.len() as u32, 3],
            trie.size() as u32,
            Some(image_pos),
            Some(map_pos),
        );

        Prism::from_mapped_file(file).expect("Load() 应该成功")
    }

    // ------------------------------------------------------------------ metadata

    #[test]
    fn metadata_layout_adds_up_to_320_bytes() {
        use metadata_layout::*;
        assert_eq!(FORMAT_POS, 0);
        assert_eq!(DICT_FILE_CHECKSUM_POS, 32);
        assert_eq!(SCHEMA_FILE_CHECKSUM_POS, 36);
        assert_eq!(NUM_SYLLABLES_POS, 40);
        assert_eq!(NUM_SPELLINGS_POS, 44);
        assert_eq!(DOUBLE_ARRAY_SIZE_POS, 48);
        assert_eq!(DOUBLE_ARRAY_POS, 52);
        assert_eq!(SPELLING_MAP_POS, 56);
        assert_eq!(ALPHABET_POS, 60);
        assert_eq!(MAX_KEY_LENGTH_POS, 316);
        assert_eq!(ALPHABET_POS + ALPHABET_MAX_LENGTH, MAX_KEY_LENGTH_POS);
        assert_eq!(MAX_KEY_LENGTH_POS + 4, SIZE);
        // 和 prism.h:36-50 的字段顺序对得上：5 个 u32 从 32 连排到 52
        assert_eq!(NUM_SPELLINGS_POS, NUM_SYLLABLES_POS + 4);
        assert_eq!(DOUBLE_ARRAY_POS, DOUBLE_ARRAY_SIZE_POS + 4);
        assert_eq!(SPELLING_MAP_POS, DOUBLE_ARRAY_POS + 4);
    }

    #[test]
    fn metadata_reads_each_field_at_its_offset() {
        let mut file = MappedFile::create("unused", 1024).unwrap();
        // double_array 指到 700，spelling_map 指到 800
        write_metadata(
            &mut file,
            0,
            b"/abc",
            [123_913, 134_042, 10],
            279_552,
            Some(700),
            Some(800),
        );
        let metadata = Metadata::read(&file, 0).expect("metadata");
        assert_eq!(metadata.format, "Rime::Prism/5.0");
        assert_eq!(metadata.format_version(), Some(5.0));
        assert_eq!(metadata.dict_file_checksum, 0xcafe_0001);
        assert_eq!(metadata.schema_file_checksum, 0xcafe_0002);
        assert_eq!(metadata.num_syllables, 123_913);
        assert_eq!(metadata.num_spellings, 134_042);
        assert_eq!(metadata.double_array_size, 279_552);
        assert_eq!(metadata.double_array, Some(700));
        assert_eq!(metadata.spelling_map, Some(800));
        assert_eq!(metadata.alphabet, b"/abc".to_vec());
        assert_eq!(metadata.alphabet_str(), Some("/abc"));
        assert_eq!(metadata.max_key_length, 10);
        // 偏移不是注释：700 处写个字节，deref 的结果必须对得上
        file.put_u8(700, 1);
        assert_eq!(Metadata::read(&file, 0).unwrap().double_array, Some(700));
    }

    // ------------------------------------------------------------------ Load

    fn load_error(file: MappedFile) -> String {
        match Prism::from_mapped_file(file) {
            Ok(_) => panic!("这份镜像不该加载成功"),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn load_rejects_missing_invalid_and_old_metadata() {
        // 空文件 → metadata not found
        let file = MappedFile::create("unused", 0).unwrap();
        assert_eq!(load_error(file), "metadata not found");

        // 前缀不对 → invalid metadata（照 prism.cc:102）
        let mut file = MappedFile::create("unused", 4096).unwrap();
        file.put_bytes(0, b"Rime::Table/4.0\0");
        assert_eq!(load_error(file), "invalid metadata");

        // 版本太旧（低于最低可加载版本 4.0）→ 强制重建（照 prism.cc:110-115）
        let mut file = MappedFile::create("unused", 4096).unwrap();
        file.put_bytes(0, b"Rime::Prism/3.0\0");
        assert_eq!(
            load_error(file),
            "prism format 3 is too old. upgrading to 5"
        );

        // 版本>=5 但 double_array 是 NULL → "double array image not found"
        let mut file = MappedFile::create("unused", 4096).unwrap();
        file.put_bytes(0, b"Rime::Prism/5.0\0");
        assert_eq!(load_error(file), "double array image not found");

        // double_array 指到文件外 → out of range（C++ 这里是 UB）
        let mut file = MappedFile::create("unused", 512).unwrap();
        file.put_bytes(0, b"Rime::Prism/5.0\0");
        file.put_u32(metadata_layout::DOUBLE_ARRAY_SIZE_POS, 100);
        file.set_offset(metadata_layout::DOUBLE_ARRAY_POS, 400);
        assert_eq!(load_error(file), "double array image is out of range");
    }

    #[test]
    fn load_fills_trie_and_metadata_for_a_synthetic_file() {
        let prism = synthetic_prism();
        let metadata = prism.metadata().expect("metadata");
        assert_eq!(prism.format(), 5.0);
        assert_eq!(metadata.num_syllables, 10);
        assert_eq!(metadata.num_spellings, 5);
        assert_eq!(metadata.alphabet, b"abc".to_vec());
        assert_eq!(metadata.max_key_length, 3);
        // darts 段紧跟 metadata
        assert_eq!(metadata.double_array, Some(metadata_layout::SIZE));
        // 拼写表紧跟 darts 镜像
        assert_eq!(
            metadata.spelling_map,
            Some(metadata_layout::SIZE + 4 * metadata.double_array_size as usize)
        );
        assert_eq!(prism.array_size(), metadata.double_array_size as usize);
        assert_eq!(prism.max_key_length(), 3);
        assert_eq!(prism.dict_file_checksum(), 0xcafe_0001);
        assert_eq!(prism.schema_file_checksum(), 0xcafe_0002);
        assert_eq!(prism.spelling_map_len(), 5);
        assert_eq!(prism.spelling_list_len(0), Some(1));
        assert_eq!(prism.spelling_list_len(1), Some(0));
        assert_eq!(prism.spelling_list_len(2), Some(2));
        assert_eq!(prism.spelling_list_len(5), None);
        assert_eq!(prism.spelling_list_len(-1), None);
    }

    // ------------------------------------------------------------------ 查询

    #[test]
    fn get_value_has_key_and_prefix_search_follow_prism_cc() {
        let prism = synthetic_prism();
        // 值 = 键在 keys 数组里的下标（darts 的约定）
        for (i, key) in ["a", "ab", "abc", "b", "ba"].iter().enumerate() {
            assert_eq!(prism.get_value(key), Some(i as i32), "key={key}");
            assert!(prism.has_key(key));
        }
        assert_eq!(prism.get_value("abcd"), None);
        assert_eq!(prism.get_value("z"), None);
        assert_eq!(prism.get_value(""), None, "根节点没有值");
        assert!(!prism.has_key("abcd"));

        // 前缀搜索：长度升序
        assert_eq!(
            prism.common_prefix_search("abc"),
            vec![
                Match {
                    value: 0,
                    length: 1
                },
                Match {
                    value: 1,
                    length: 2
                },
                Match {
                    value: 2,
                    length: 3
                },
            ]
        );
        // 空 key 直接返回（prism.cc:280-281）
        assert!(prism.common_prefix_search("").is_empty());
        // 只有 "b" 是 "b" 的前缀
        assert_eq!(
            prism.common_prefix_search("b"),
            vec![Match {
                value: 3,
                length: 1
            }]
        );
    }

    #[test]
    fn expand_search_matches_cpp_bfs_order() {
        let prism = synthetic_prism();
        // key 自己就是键 → 先放自己，再 BFS
        assert_eq!(
            prism.expand_search("a", 0),
            vec![
                Match {
                    value: 0,
                    length: 1
                },
                Match {
                    value: 1,
                    length: 2
                },
                Match {
                    value: 2,
                    length: 3
                },
            ]
        );
        // 只有 "ab"/"abc" 以 "ab" 开头
        assert_eq!(
            prism.expand_search("ab", 0),
            vec![
                Match {
                    value: 1,
                    length: 2
                },
                Match {
                    value: 2,
                    length: 3
                },
            ]
        );
        // limit：C++ 是 `if (limit && ++count >= limit) return;`
        assert_eq!(
            prism.expand_search("a", 2),
            vec![
                Match {
                    value: 0,
                    length: 1
                },
                Match {
                    value: 1,
                    length: 2
                },
            ]
        );
        // 不是合法路径 → 空（prism.cc:300-301）
        assert!(prism.expand_search("x", 0).is_empty());
        // 空 key = 从根开始枚举，BFS 序：第一层 a、b，第二层 ab、ba，第三层 abc
        assert_eq!(
            prism.expand_search("", 0),
            vec![
                Match {
                    value: 0,
                    length: 1
                },
                Match {
                    value: 3,
                    length: 1
                },
                Match {
                    value: 1,
                    length: 2
                },
                Match {
                    value: 4,
                    length: 2
                },
                Match {
                    value: 2,
                    length: 3
                },
            ]
        );
    }

    // ------------------------------------------------------------------ 拼写表

    #[test]
    fn spelling_accessor_walks_descriptors() {
        let prism = synthetic_prism();

        // id 0：一条
        let mut accessor = prism.query_spelling(0);
        assert!(!accessor.exhausted());
        assert_eq!(accessor.index(), Some(0));
        assert_eq!(accessor.syllable_id(), 7);
        let descriptor = accessor.descriptor().expect("descriptor");
        assert_eq!(descriptor.syllable_id, 7);
        assert_eq!(descriptor.r#type, 0);
        assert_eq!(descriptor.credibility, -0.5);
        assert_eq!(descriptor.tips, "gong");
        assert!(!descriptor.is_correction());
        assert_eq!(descriptor.spelling_type(), SpellingType::Normal);
        // properties() 只带 type + credibility（tips/is_correction 是 pub(super)）
        let properties = accessor.properties();
        assert_eq!(properties.r#type, SpellingType::Normal);
        assert_eq!(properties.credibility, -0.5);
        // Next()：返回的是 exhausted()，不是"还有下一条"
        assert!(accessor.next());
        assert!(accessor.exhausted());
        assert_eq!(accessor.syllable_id(), -1);
        assert_eq!(accessor.index(), None);
        assert!(!accessor.next(), "耗尽之后 Next() 恒 false");

        // id 2：两条，第一条带纠错位
        let mut accessor = prism.query_spelling(2);
        let first = accessor.descriptor().unwrap();
        assert!(first.is_correction());
        assert_eq!(first.unpacked_type(), 2);
        assert_eq!(first.spelling_type(), SpellingType::Abbreviation);
        assert_eq!(accessor.properties().r#type, SpellingType::Abbreviation);
        assert_eq!(accessor.properties().credibility, -1.25);
        assert!(!accessor.next());
        assert_eq!(accessor.index(), Some(1));
        assert_eq!(accessor.syllable_id(), 3);
        assert_eq!(accessor.descriptor().unwrap().tips, "tip3");
        assert!(accessor.next());
        assert_eq!(accessor.syllable_id(), -1);
    }

    #[test]
    fn empty_and_out_of_range_spelling_ids_degrade_like_cpp() {
        let prism = synthetic_prism();
        // id 1：空列表 —— C++ 里 iter_/end_ 都指向 begin()，`iter_ < end_` 为假，
        // 于是 syllable_id() 返回拼写 id 自己
        let mut accessor = prism.query_spelling(1);
        assert_eq!(accessor.syllable_id(), 1);
        assert!(accessor.descriptor().is_none());
        // 属性全默认：Normal / 0.0
        let properties = accessor.properties();
        assert_eq!(properties.r#type, SpellingType::Normal);
        assert_eq!(properties.credibility, 0.0);
        assert!(accessor.next());
        assert!(accessor.exhausted());

        // id 越界：accessor 不空，属性默认（prism.cc:43-47 的 if 不成立）
        let mut accessor = prism.query_spelling(99);
        assert!(!accessor.exhausted());
        assert_eq!(accessor.syllable_id(), 99);
        assert!(accessor.descriptor().is_none());
        assert!(accessor.next());
        assert!(accessor.exhausted());

        // spelling_id == -1：C++ 里构造完就 exhausted（spelling_id_ == -1）
        let accessor = prism.query_spelling(-1);
        assert!(accessor.exhausted());
    }

    #[test]
    fn descriptor_type_mask_keeps_bit30_out_of_the_sign() {
        // kTypeIsCorrectionMask = 1 << 30：不打进符号位
        assert!(K_TYPE_IS_CORRECTION_MASK > 0);
        assert_eq!(K_SPELLING_TYPE_MASK, K_TYPE_IS_CORRECTION_MASK ^ -1);
        let descriptor = SpellingDescriptor {
            syllable_id: 0,
            r#type: K_TYPE_IS_CORRECTION_MASK | 4,
            credibility: 0.0,
            tips: String::new(),
        };
        assert!(descriptor.is_correction());
        assert_eq!(descriptor.unpacked_type(), 4);
        assert_eq!(descriptor.spelling_type(), SpellingType::Ambiguous);
        // 越界值（C++ 是 UB）落到 Invalid
        let bogus = SpellingDescriptor {
            r#type: 99,
            ..descriptor
        };
        assert_eq!(bogus.spelling_type(), SpellingType::Invalid);
    }

    // ------------------------------------------------------------------ 真实文件

    /// `%APPDATA%\Rime-Next\build\yuling` —— 没有就跳过（CI / 别的机器上没有这份文件）。
    fn real_build_dir() -> Option<PathBuf> {
        let appdata = std::env::var_os("APPDATA")?;
        let dir = PathBuf::from(appdata)
            .join("Rime-Next")
            .join("build")
            .join("yuling");
        if dir.join("yuling.prism.bin").is_file() {
            Some(dir)
        } else {
            None
        }
    }

    /// 从词典源文件里捞真实编码（第一列 text、第二列 code，`columns: text, code`）。
    fn collect_codes(path: &Path, out: &mut std::collections::BTreeSet<String>) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        for line in text.lines() {
            if line.starts_with('#')
                || line.starts_with("---")
                || line.starts_with("...")
                || line.trim().is_empty()
            {
                continue;
            }
            let mut fields = line.split('\t');
            let _text = fields.next();
            let Some(code) = fields.next() else { continue };
            let code = code.trim();
            if !code.is_empty() && !code.contains(char::is_whitespace) {
                out.insert(code.to_string());
            }
        }
    }

    /// 用 darts 的 `traverse` 把整棵 trie 走一遍（C++ 没有这个 API；
    /// `ExpandSearch` 的 BFS 同款步进，只是这里把 key 串也记下来）。
    fn enumerate_keys(prism: &Prism) -> Vec<(String, i32)> {
        let alphabet = prism
            .metadata()
            .map(|metadata| metadata.alphabet.clone())
            .unwrap_or_default();
        let mut keys = Vec::new();
        // 深度上限 = metadata 的 max_key_length（合法 trie 的任何 key 都不超过它）。
        // 没有 2026-10-26 深夜重建的 yuling.prism.bin 时，这层防御是必须的：
        // double-array 的稀疏单元里可能存在 self-loop 弧（traverse 返回有效值但
        // node_pos 不前进），无界 BFS 会以前缀无限增长的方式吃光内存。
        let max_key_length = prism
            .metadata()
            .map(|metadata| metadata.max_key_length as usize)
            .unwrap_or(64)
            .max(1);
        let mut stack: Vec<(usize, Vec<u8>)> = vec![(0, Vec::new())];
        while let Some((node_pos, prefix)) = stack.pop() {
            if prefix.len() >= max_key_length {
                continue;
            }
            for &c in &alphabet {
                let mut next_node_pos = node_pos;
                let mut next_key_pos = 0usize;
                let ret = prism
                    .trie()
                    .traverse(&[c], &mut next_node_pos, &mut next_key_pos);
                if ret <= TRAVERSE_INVALID {
                    continue;
                }
                let mut key = prefix.clone();
                key.push(c);
                if ret >= 0 {
                    keys.push((
                        String::from_utf8(key.clone()).expect("alphabet 是 ASCII"),
                        ret,
                    ));
                }
                stack.push((next_node_pos, key));
            }
        }
        keys
    }

    /// 读真实的 `yuling.prism.bin`（只读），把 metadata / 命中率 / 拼写表全量验一遍。
    ///
    /// 没有这份文件时**静默跳过**（不算失败），所以 `cargo test` 在别的机器上照样绿。
    #[test]
    fn reads_real_yuling_prism() {
        let Some(build_dir) = real_build_dir() else {
            eprintln!(
                "跳过 reads_real_yuling_prism：没有 %APPDATA%\\Rime-Next\\build\\yuling\\yuling.prism.bin"
            );
            return;
        };
        let prism_path = build_dir.join("yuling.prism.bin");
        let prism = Prism::open(prism_path.clone()).expect("加载真实 prism.bin");
        let metadata = prism.metadata().expect("metadata").clone();
        let file_size = prism.file().used();

        // ---- ① metadata 实际值 ------------------------------------------------
        eprintln!(
            "metadata: format={:?} dict_checksum={:#010x} schema_checksum={:#010x} \
             num_syllables={} num_spellings={} double_array_size={} \
             double_array@={:?} spelling_map@={:?} max_key_length={} file_size={file_size}",
            metadata.format,
            metadata.dict_file_checksum,
            metadata.schema_file_checksum,
            metadata.num_syllables,
            metadata.num_spellings,
            metadata.double_array_size,
            metadata.double_array,
            metadata.spelling_map,
            metadata.max_key_length,
        );
        eprintln!(
            "alphabet({} B): {:?} = {:?}",
            metadata.alphabet.len(),
            &metadata.alphabet[..metadata.alphabet.len().min(16)],
            metadata.alphabet_str().unwrap_or("<非 UTF-8>")
        );

        // ---- ② 布局：三段起点都由 metadata 推出来 ------------------------------
        assert_eq!(metadata.format, K_PRISM_FORMAT);
        assert_eq!(prism.format(), K_PRISM_FORMAT_VERSION);
        assert_eq!(metadata.double_array, Some(metadata_layout::SIZE));
        assert_eq!(prism.array_size(), metadata.double_array_size as usize);
        let expected_map = metadata_layout::SIZE + 4 * metadata.double_array_size as usize;
        assert_eq!(
            metadata.spelling_map,
            Some(expected_map),
            "拼写表应紧跟 darts 镜像"
        );
        assert_eq!(prism.spelling_map_len(), metadata.num_spellings as usize);
        assert_eq!(prism.max_key_length(), metadata.max_key_length as usize);
        let item_array_len = 4 + prism.spelling_map_len() * SPELLING_MAP_ITEM_SIZE;
        let descriptors_start = prism.spelling_list_pos(0).expect("第 0 个拼写列表");
        assert_eq!(
            descriptors_start,
            expected_map + item_array_len,
            "描述符数组应紧跟 List 头数组"
        );
        assert!(metadata.alphabet.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(
            metadata
                .alphabet
                .iter()
                .all(|byte| byte.is_ascii() && *byte != 0),
            "alphabet 是 set<char> 排序出来的 ASCII"
        );

        // ---- ③ 用词典源里的真实编码查 get_value / has_key ----------------------
        let schema_dir = build_dir
            .parent()
            .and_then(|path| path.parent())
            .map(|root| root.join("schema").join("yuhao"))
            .expect("Rime-Next 根目录");
        let mut codes = std::collections::BTreeSet::new();
        for name in [
            "yuling.quick.dict.yaml",
            "yuling.pop.dict.yaml",
            "yuling.full.dict.yaml",
            "yuling.roots.dict.yaml",
            "yuhao.symbols.dict.yaml",
        ] {
            collect_codes(&schema_dir.join(name), &mut codes);
        }
        let (mut queried, mut hit) = (0usize, 0usize);
        let mut misses: Vec<&str> = Vec::new();
        for code in &codes {
            queried += 1;
            match prism.get_value(code) {
                Some(value) => {
                    hit += 1;
                    assert!(value >= 0, "{code}: 值不该是负数");
                    assert!(prism.has_key(code), "{code}: has_key 应与 get_value 一致");
                }
                None => {
                    if misses.len() < 8 {
                        misses.push(code);
                    }
                }
            }
        }
        eprintln!(
            "真实编码查询: 查了 {queried} 条，命中 {hit} 条（{:.2}%），未命中样例 {misses:?}",
            100.0 * hit as f64 / queried as f64
        );
        assert!(
            hit * 100 >= queried * 99,
            "真实编码命中率应 >= 99%，实际 {hit}/{queried}"
        );

        // ---- ④ 把整棵 trie 枚举出来，回查每一条 -------------------------------
        let enumerated = enumerate_keys(&prism);
        let mut by_length = std::collections::BTreeMap::new();
        for (key, value) in &enumerated {
            assert_eq!(
                prism.get_value(key),
                Some(*value),
                "枚举出来的键必须能原值查回: {key}"
            );
            *by_length.entry(key.len()).or_insert(0usize) += 1;
        }
        let longest = enumerated
            .iter()
            .map(|(key, _)| key.len())
            .max()
            .unwrap_or(0);
        eprintln!(
            "trie 枚举: {} 个键（num_spellings={}），最长 {longest} B（max_key_length={}），长度分布 {:?}",
            enumerated.len(),
            metadata.num_spellings,
            metadata.max_key_length,
            by_length
        );
        // Prism 5.0 的 trie 里除了主拼写还存**纠错拼写**（correction/fuzzy 展开），
        // 枚举总数 ≥ num_spellings（真机 yuling 2026-10-26：829,502 键 vs 134,042 主拼写，
        // 探针验证：真实编码 2000/2000 命中、value 全在界内、非拼写前缀零误命中）。
        // 真正的不变式是：枚举出的键按 value 去重后**恰好**是 num_spellings 个 id
        // （④ 里已断言"枚举出来的键必须能原值查回"），此处只对去重数断言。
        let distinct_values: std::collections::BTreeSet<i32> =
            enumerated.iter().map(|(_, value)| *value).collect();
        assert_eq!(distinct_values.len(), metadata.num_spellings as usize);
        assert_eq!(longest, metadata.max_key_length as usize);

        // ---- ⑤ 遍历 spelling_map（0..num_spellings） --------------------------
        let (mut lists, mut descriptors, mut in_range) = (0usize, 0usize, 0usize);
        let (mut corrections, mut with_tips, mut max_list) = (0usize, 0usize, 0usize);
        let mut first_descriptors = Vec::new();
        for id in 0..prism.spelling_map_len() {
            let id = id as SyllableId;
            let len = prism.spelling_list_len(id).expect("每个 id 都有列表");
            lists += 1;
            max_list = max_list.max(len);
            for index in 0..len {
                let descriptor = prism
                    .spelling_descriptor(id, index)
                    .expect("描述符必须可读");
                descriptors += 1;
                if descriptor.syllable_id < metadata.num_syllables as SyllableId {
                    in_range += 1;
                }
                if descriptor.is_correction() {
                    corrections += 1;
                }
                if !descriptor.tips.is_empty() {
                    with_tips += 1;
                }
                assert!(
                    (0..=5).contains(&descriptor.unpacked_type()),
                    "type 越界: {}",
                    descriptor.r#type
                );
                if first_descriptors.len() < 3 {
                    first_descriptors.push(descriptor);
                }
            }
        }
        eprintln!(
            "spelling_map: {lists} 个列表、{descriptors} 条描述符（syllable_id < num_syllables: \
             {in_range}），纠错 {corrections} 条、带 tips {with_tips} 条、单列表最多 {max_list} 条"
        );
        eprintln!("前 3 条描述符: {first_descriptors:?}");
        assert_eq!(lists, metadata.num_spellings as usize);
        assert_eq!(in_range, descriptors, "所有 syllable_id 都应落在音节表内");
        let descriptors_end = descriptors_start + descriptors * SPELLING_DESCRIPTOR_SIZE;
        eprintln!(
            "布局: metadata 0..{} | darts {}..{} | spelling_map {}..{}（item 数组 {}..{}）| 描述符 {}..{} | 文件 {}",
            metadata_layout::SIZE,
            metadata.double_array.unwrap(),
            expected_map,
            expected_map,
            expected_map + item_array_len,
            expected_map + 4,
            expected_map + item_array_len,
            descriptors_start,
            descriptors_end,
            file_size
        );
        assert!(descriptors_end <= file_size, "描述符区不能越过文件尾");
    }

    // ------------------------------------------------------------------ 写路径

    /// 临时文件（`%TEMP%\librime-next-dict-write-<pid>\`，**不写 `%APPDATA%`**）。
    fn temp_file(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "librime-next-dict-write-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir.join(name)
    }

    /// `Prism::Build(syllabary, nullptr)`：不写拼写表，键 = 音节表，值 = 音节 id。
    #[test]
    fn build_without_script_round_trips_via_disk() {
        let syllabary: Syllabary = ["a", "ab", "b"].into_iter().collect();
        let path = temp_file("prism-roundtrip.prism.bin");
        let _ = std::fs::remove_file(&path);

        let mut prism =
            Prism::build(&path, &syllabary, None, 0x1111, 0x2222).expect("Prism::build");
        assert!(!path.exists(), "`build` 只建镜像，`save` 才落盘");
        assert_eq!(prism.metadata().unwrap().num_spellings, 3);
        assert_eq!(prism.metadata().unwrap().num_syllables, 3);
        assert!(prism.spelling_map_pos().is_none(), "没有 script 就没有拼写表");
        prism.save().expect("Prism::save");

        let reread = Prism::open(&path).expect("Prism::open");
        let metadata = reread.metadata().expect("metadata").clone();
        assert_eq!(metadata.num_spellings, 3);
        assert_eq!(metadata.num_syllables, 3);
        assert_eq!(metadata.max_key_length, 2);
        assert_eq!(reread.dict_file_checksum(), 0x1111);
        assert_eq!(reread.schema_file_checksum(), 0x2222);
        // alphabet = 键字节的升序去重
        assert_eq!(metadata.alphabet_str(), Some("ab"));
        // darts 段紧跟 metadata
        assert_eq!(metadata.double_array, Some(metadata_layout::SIZE));
        // 字符串表：值 = 音节 id（读侧退化成"拼写 id 即音节 id"）
        for (id, syllable) in ["a", "ab", "b"].iter().enumerate() {
            assert_eq!(reread.get_value(syllable), Some(id as SyllableId), "{syllable}");
            assert_eq!(
                reread.query_spelling(id as SyllableId).syllable_id(),
                id as SyllableId,
                "没有拼写表时 syllable_id 回落到拼写 id"
            );
        }
        assert_eq!(reread.get_value("abc"), None);
        assert_eq!(reread.common_prefix_search("ab").len(), 2);
        assert_eq!(reread.expand_search("a", 0).len(), 2);
        // 文件尾 = metadata + darts 镜像（script=None 没有别的段）
        assert_eq!(
            std::fs::metadata(&path).unwrap().len() as usize,
            metadata_layout::SIZE + reread.array_size() * 4
        );
        let _ = std::fs::remove_file(&path);
    }

    /// `Prism::Build(syllabary, script)`：逐条写拼写表。
    #[test]
    fn build_with_script_writes_the_spelling_map() {
        let syllabary: Syllabary = ["a", "b"].into_iter().collect();
        let mut script = Script::new();
        assert!(script.add_syllable("a"));
        assert!(script.add_syllable("b"));
        let path = temp_file("prism-script.prism.bin");
        let _ = std::fs::remove_file(&path);

        Prism::compile(&path, &syllabary, Some(&script), 7, 9).expect("Prism::compile");
        let reread = Prism::open(&path).expect("Prism::open");
        assert_eq!(reread.metadata().unwrap().num_spellings, 2);
        assert!(reread.spelling_map_pos().is_some());
        assert_eq!(reread.spelling_map_len(), 2);
        // `Script::add_syllable` 每条只有一个 Normal、credibility 0 的拼写
        let descriptor = reread.spelling_descriptor(0, 0).expect("描述符");
        assert_eq!(descriptor.syllable_id, 0);
        assert_eq!(descriptor.r#type, 0);
        assert_eq!(descriptor.credibility, 0.0);
        assert!(!descriptor.is_correction());
        assert!(descriptor.tips.is_empty(), "tips 在 algo 层是私有的，写不进去");
        assert_eq!(reread.spelling_descriptor(1, 0).unwrap().syllable_id, 1);
        // 拼写 id 的顺序 = script 的键序（BTreeMap，与 C++ std::map 一致）
        assert_eq!(reread.get_value("a"), Some(0));
        assert_eq!(reread.get_value("b"), Some(1));
        // 文件大小：metadata + darts + 拼写表数组 + 2 条描述符
        assert_eq!(
            std::fs::metadata(&path).unwrap().len() as usize,
            metadata_layout::SIZE
                + reread.array_size() * 4
                + 4
                + 2 * SPELLING_MAP_ITEM_SIZE
                + 2 * SPELLING_DESCRIPTOR_SIZE
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 空音节表：darts 只产出根单元（`trie_->total_size()` 不能是 0，否则 `Save` 会拒）。
    #[test]
    fn empty_syllabary_builds_a_root_only_prism() {
        let syllabary = Syllabary::new();
        let path = temp_file("prism-empty.prism.bin");
        let _ = std::fs::remove_file(&path);
        let prism = Prism::compile(&path, &syllabary, None, 0, 0).expect("空表也应能写出");
        let reread = Prism::open(&path).expect("Prism::open");
        assert_eq!(reread.metadata().unwrap().num_spellings, 0);
        assert_eq!(reread.array_size(), 1, "只有根单元");
        assert_eq!(reread.max_key_length(), 0);
        assert_eq!(prism.array_size(), 1);
        assert_eq!(reread.get_value("a"), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_on_read_only_prism_is_rejected() {
        // 读路径的 Prism 是只读镜像：`Prism::save` 先看 `trie_->total_size()`，
        // 再落盘；落盘这一步会因 `MappedFile` 只读而报错。
        let path = temp_file("prism-readonly.prism.bin");
        let _ = std::fs::remove_file(&path);
        let syllabary: Syllabary = ["a"].into_iter().collect();
        Prism::compile(&path, &syllabary, None, 0, 0).expect("compile");
        let mut reopened = Prism::open(&path).expect("open");
        assert!(reopened.save().is_err(), "只读镜像不能 save");
        let _ = std::fs::remove_file(&path);
    }
}
