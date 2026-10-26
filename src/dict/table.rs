//! `dict/table.h`(239 行) + `dict/table.cc`(636 行) 的 Rust 移植 —— **读 + 写**。
//!
//! `.table.bin` 是「音节 code → 词条」的索引：prism 负责把输入串切成音节 id，table 负责
//! 拿音节 id 找回**词条文本 + 权重**。文本不在词条结构里，而是 `.table.bin` 末尾那棵
//! marisa trie（见 [`crate::dict::string_table`]）里的 id。
//!
//! # 文件布局（`Rime::Table/4.0`）
//!
//! `Table::Build`（`table.cc:330-390`）的**分配顺序就是磁盘布局**，但读的时候只信
//! metadata 里那几个 `OffsetPtr`：不按固定偏移硬啃，也不假设"段与段紧挨着"——
//! 索引的下一层和各页词条数组是**交错**分配的（`BuildHeadIndex` 里"先填本层词条、
//! 再递归建下一层"），所以只能顺着指针走。
//!
//! ```text
//! 0                          Metadata                  68 B（见 metadata_layout）
//! deref(44)                  Array<StringType>          u32 num_syllables + num_syllables × 4 B
//! deref(48)                  HeadIndex                  u32 num_syllables + num_syllables × 12 B
//! 之后（交错分配）             List<Entry> / TrunkIndex / TailIndex
//! deref(60), size=deref(64)  marisa trie 镜像           string_table_size B（文件最后一段）
//! ```
//!
//! 以 `%APPDATA%\Rime-Next\build\yuling\yuling.table.bin`（5,800,996 B）为例，四段起点
//! 全部由 metadata 推出来（`reads_real_yuling_table` 逐项校验，不是手抄的）：
//!
//! ```text
//! metadata        0 .. 68
//! syllabary       68 .. 68 + 4 + 123,913×4 = 495,724
//! index          495,724 .. 495,724 + 4 + 123,913×12 = 1,982,684
//! 词条页          1,982,684 .. 4,668,444（335,720 条 Entry × 8 B = 2,685,760 B，正好铺满）
//! string_table 4,668,444 .. 4,668,444 + 1,132,552 = 5,800,996（= 文件尾）
//! ```
//!
//! # 结构体与"union"
//!
//! `table.h` 用 `RIME_TABLE_UNION` 造了三个"同址不同型"的 union：
//!
//! | union | 两种解释 | 读法 |
//! |---|---|---|
//! | `StringType` | `String str`（v1，`OffsetPtr`）/ `StringId str_id`（v2+） | [`StringType`]，见下 |
//! | `PhraseIndex` | `TrunkIndex` / `TailIndex` | 由**层级**决定（第 4 个音节起才是 tail） |
//! | `Index` | = `HeadIndex` | 只有一个解释 |
//!
//! 三个 union 的成员都只有 4 字节、没有 tag，**只能靠 format 版本与层级**区分：
//!
//! * `StringType`：v1.0 里是 `String`（相对偏移），v2.0 起改存 marisa id
//!   （`table.h:34` 的 union 声明 + `table.cc:219-226` 的 `GetString_v1` 被注释掉）；
//!   本模块只支持 4.0，所以**一律按 id 读**，v1 的 `str()` 没移植（见「没做的」）。
//! * `PhraseIndex`：`HeadIndex` 的节点 `next_level` 指向的可能是 `TrunkIndex`，也可能是
//!   `TailIndex`。判据在 `BuildTrunkIndex`（`table.cc:442-459`）：`code.size() <
//!   kIndexCodeMaxLength` 时建 trunk，否则建 tail。而 `kIndexCodeMaxLength = 3`，
//!   所以**前 3 个音节各一层 trunk，第 4 个音节起落到 tail**（tail 的 `LongEntry`
//!   把多出来的音节存成 `extra_code`）。
//!
//! # 词条结构（照抄 `table.h`，不按 `DictEntry` 猜）
//!
//! ```text
//! table::Entry      = { StringType text; Weight weight; }             8 B
//! table::LongEntry  = { Code extra_code; Entry entry; }              16 B   (Code = List<SyllableId>)
//! HeadIndexNode     = { List<Entry> entries; OffsetPtr<PhraseIndex> next_level; }  12 B
//! TrunkIndexNode    = { SyllableId key; List<Entry> entries; OffsetPtr<PhraseIndex> next_level; }  16 B
//! ```
//!
//! 注意 `Entry` **没有**词频/comment/preedit/用户码这些字段——`table.cc:523-533` 的
//! `BuildEntry` 只写 `text` + `weight`。`DictEntry` 的其余字段（`comment`/`preedit`/
//! `commit_count`/`custom_code`…）在 `user_db` / `text_db` 那一侧，不在 `.table.bin` 里。
//! `weight` 是 **float32**（`using Weight = float`），进来就转成 `f32`；C++ 侧存的时候
//! 是 `static_cast<Weight>(dict_entry.weight)`（double → float），这里读出来就是原值。
//!
//! # 两种顺序（以及一个容易误解的点）
//!
//! * [`Table::entries_in_file_order`] —— `.table.bin` 的**物理顺序**：先按音节 id 升序，
//!   同一页里按 `List<Entry>` 的存放顺序；有短语时是"head → trunk1 → … → tail"的深度优先。
//! * [`Table::entries_sorted_by_weight`] —— 在读出来的集合上再排一次：`weight` 降序，
//!   同权重按 text 升序（稳定排序，`(weight, text)` 都相同的保留文件序）。
//!
//! ⚠️ **`.table.bin` 里的顺序不是 `*.dict.yaml` 的行序**。编译时
//! `entry_collector` 已经把词条按 code 收进 `Vocabulary`（同 code 再按权重降序
//! `sort_homophones`），写盘时又按 code 分层，所以 dict.yaml 里"第 3 行的词"完全可能
//! 排在文件里的第 200 条。想要 dict.yaml 的原样行序，只能读源 `.yaml` ——
//! 那是 `entry_collector` / `dict_compiler` 的事，不在本模块（也不在本轮）范围。
//!
//! C++ 的 `Table` 没有这两个"全量取词"的接口（它只按 code 查），两条都是 Rust 侧加的，
//! 为的是验收对拍与编译期自检。
//!
//! # 与 C++ 的接口差异（接缝）
//!
//! * **`metadata_` / `syllabary_` / `index_` 从裸指针换成"解析好的值 + 绝对偏移"**，
//!   `Load()` 的 `bool` 换成 `io::Result<()>`，错误文案沿用 C++ 的日志文案。
//! * **`TableAccessor` 借 `&Table`**：C++ 里它只存裸指针（`entries_` / `long_entries_`），
//!   回车键是 `Table::GetEntryText(entry)`；这里让 accessor 借一份 `&Table`，于是
//!   `entry_text()` 可以直接给（见 [`TableAccessor::entry_text`]，C++ 没这个方法）。
//! * **`TableQuery` 借 `&Table`**：C++ 只拿 `table::Index*`，靠裸指针读 `List`/`Array`；
//!   这里所有读都得经过 [`MappedFile`]，所以顺手把 table 一起借进来。
//! * **失败路径返回"空 accessor"**而不是默认构造的 `TableAccessor()`：
//!   语义一样（`exhausted() == true`），只是它仍然带着 `&Table`。
//! * **`Table::Query(SyllableGraph)` 已移植**（[`Table::query`]，`table.cc:571-630`）：
//!   它吃 `algo` 层的音节图，靠 [`SyllableGraph::indices`] 这个只读访问器拿
//!   "位置 → 音节 id → 边属性"的转置表，再读 `SpellingProperties::end_pos`。
//!   返回 [`TableQueryResult`]（`end_pos` → 若干页词条）而不是 C++ 的
//!   "填一个 `result` 指针 + 返回 `bool`"，理由见 [`Table::query`] 的文档。
//! * **`Build` / `Save` 是关联函数**（[`Table::build`] / [`Table::save`]）：
//!   C++ 在已打开的可写 `MappedFile` 上原地建；Rust 侧 `build` 自己
//!   `Create(4096 + 32*syllables + 64*entries)`，返回已 `Load` 的 `Table`
//!   （`file` 仍是可写模式，所以紧接着的 `save` 有效）。
//!   [`Table::compile`] = `Build` + `Save` 一步到位。
//!
//! # 写路径（`Table::Build`，`table.cc:330-533`）
//!
//! **分配顺序就是磁盘布局**，且与 C++ 逐字节对齐：
//!
//! ```text
//! 0                          metadata（68 B，Allocate<Metadata>）
//! 68                         Array<StringType>（u32 n + n×4）  ← AddString 只记引用
//! deref(48)                  HeadIndex（u32 n + n×12）
//! 之后（深度优先、交错）       HeadIndex 各页 List<Entry>、trunk 2/3 层数组与页、tail 数组
//! deref(60)                  marisa 镜像（最后一段）           ← OnBuildFinish
//! format 最后写
//! ```
//!
//! 三个关键点：
//!
//! 1. `BuildHeadIndex` 的循环是"**先填本层词条、再递归下一层**"，所以各页词条数组与
//!    trunk/tail 数组是交错分配的，不能"先摆完所有数组再填词条"；
//! 2. `BuildEntry` 只把字符串交给 `StringTableBuilder` 并记下 `StringType` 字段的
//!    **绝对偏移**；id 要等 `OnBuildFinish` 里 marisa 建完树才回填（`UpdateReferences`）；
//! 3. `format` 是最后一个字节（`table.cc:386-388`）：中途失败的文件 `Load` 读不出来，
//!    等于天然的事务标记。
//!
//! # 没做的（明确清单）
//!
//! * `Load()` 的两条 v1 兼容分支：`StringType::str()`（v1 的 `String` 偏移）与
//!   "`format < 4.0` 时没有 `reserved_*`/`string_table*` 四个字段"的短 metadata。
//!   4.0 以下本模块直接拒绝（和 C++ 一样拒绝），所以不必读短结构。
//! * `Table::Query(const SyllableGraph&, start_pos, result)`（`table.cc:571-630`）的
//!   `if (false)` 分支（`table.cc:601-609` 的歧义罚分 `kPenaltyForAmbiguousSyllable`）：
//!   C++ 里也是死代码，永不执行，所以没抄（[`Table::query`] 里留了注释）。
//! * `TableAccessor` 的 `Array<table::Entry>` 构造函数（`table.cc:34-42`）：加载路径
//!   只会遇到 `List<table::Entry>`；`Array<Entry>` 只出现在 `BuildEntryArray` 里
//!   （写路径有实现，但 `Table::Build` 自己的调用链不走它 —— C++ 也一样）。
#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};

use crate::algo::{SpellingType, SyllableGraph};
use crate::dict::mapped_file::{MappedFile, parse_format_version};
use crate::dict::string_table::{StringId, StringTable};
use crate::dict::string_table_builder::StringTableBuilder;
use crate::dict::vocabulary::{
    Code, ShortDictEntry, ShortDictEntryList, Syllabary, SyllableId, Vocabulary,
};

/// `const char kTableFormatLatest[] = "Rime::Table/4.0";`（`table.cc:18`）
pub const K_TABLE_FORMAT: &str = "Rime::Table/4.0";
/// `kTableFormatPrefix`（`table.cc:21`）：`Load()` 的第一道校验
pub const K_TABLE_FORMAT_PREFIX: &str = "Rime::Table/";
/// `const int kTableFormatLowestCompatible = 4.0;`（`table.cc:19`）
pub const K_TABLE_FORMAT_LOWEST_COMPATIBLE: f64 = 4.0;

/// `sizeof(table::StringType)`：`union { String str; StringId str_id; }` → 4
pub const STRING_TYPE_SIZE: usize = 4;
/// `sizeof(table::Entry)`：`StringType text` + `Weight weight`（float）= 8
pub const ENTRY_SIZE: usize = 8;
/// `sizeof(List<T>)`：`u32 size` + `OffsetPtr<T> at` = 8（见 `dict/mapped_file.rs`）
pub const LIST_SIZE: usize = 8;
/// `sizeof(table::LongEntry)`：`Code extra_code`（= `List<SyllableId>`，8）+ `Entry`（8）= 16
pub const LONG_ENTRY_SIZE: usize = 16;
/// `sizeof(table::HeadIndexNode)`：`List<Entry>`（8）+ `OffsetPtr`（4）= 12
pub const HEAD_INDEX_NODE_SIZE: usize = 12;
/// `sizeof(table::TrunkIndexNode)`：`SyllableId key`（4）+ `List<Entry>`（8）+ `OffsetPtr`（4）= 16
pub const TRUNK_INDEX_NODE_SIZE: usize = 16;

/// `table::Metadata`（`table.h:79-92`）的字节布局。
///
/// ```text
/// 偏移  字段                     类型                      备注
/// 0     format                   char[32]                  "Rime::Table/4.0"
/// 32    dict_file_checksum       u32                       .dict.yaml 的 crc32
/// 36    num_syllables            u32                       音节表长度
/// 40    num_entries              u32                       词条总数（含短语）
/// 44    syllabary                OffsetPtr<Syllabary>      → Array<StringType>
/// 48    index                    OffsetPtr<Index>          → HeadIndex
/// 52    reserved_1               int32                     v2 之前没有这四个字段
/// 56    reserved_2               int32
/// 60    string_table             OffsetPtr<char>           → marisa 镜像
/// 64    string_table_size        u32                       marisa 镜像的字节数
/// = 68
/// ```
///
/// 全 4 字节对齐，没有 padding（`char[32]` 之后全是 4 字节字段）。
pub mod metadata_layout {
    /// `sizeof(table::Metadata)`
    pub const SIZE: usize = 68;
    /// `table::Metadata::kFormatMaxLength`
    pub const FORMAT_MAX_LENGTH: usize = 32;
    pub const FORMAT_POS: usize = 0;
    pub const DICT_FILE_CHECKSUM_POS: usize = FORMAT_POS + FORMAT_MAX_LENGTH; // 32
    pub const NUM_SYLLABLES_POS: usize = DICT_FILE_CHECKSUM_POS + 4; // 36
    pub const NUM_ENTRIES_POS: usize = NUM_SYLLABLES_POS + 4; // 40
    pub const SYLLABARY_POS: usize = NUM_ENTRIES_POS + 4; // 44
    pub const INDEX_POS: usize = SYLLABARY_POS + 4; // 48
    pub const RESERVED_1_POS: usize = INDEX_POS + 4; // 52
    pub const RESERVED_2_POS: usize = RESERVED_1_POS + 4; // 56
    pub const STRING_TABLE_POS: usize = RESERVED_2_POS + 4; // 60
    pub const STRING_TABLE_SIZE_POS: usize = STRING_TABLE_POS + 4; // 64
}

/// `table::StringType`（`table.h:34`）：`union { String str; StringId str_id; }`。
///
/// C++ 用 `RIME_TABLE_UNION` 把它造成"4 字节 + 两个 reinterpret_cast 访问器"。这里
/// 只保留 v2+ 的解释：本模块最低支持 4.0，而 v2.0 起这一格存的是 marisa 的 `StringId`
/// （`table.cc:219-235` 里 `GetString_v1` / `AddString_v1` 已被注释掉）。
pub type StringType = StringId;

/// `table::Entry`（`table.h:42-45`）：`.table.bin` 里的一条词条。
///
/// 与 [`crate::dict::vocabulary::DictEntry`] 不是一回事：文件里只存这两格。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Entry {
    /// `StringType text`：marisa id，正文在字符串表里
    pub text: StringType,
    /// `Weight weight` = `float`
    pub weight: f32,
}

impl Entry {
    /// 读 `pos` 处的 8 字节。
    pub fn read(file: &MappedFile, pos: usize) -> Option<Self> {
        Some(Self {
            text: file.u32_at(pos)?,
            weight: file.f32_at(pos + STRING_TYPE_SIZE)?,
        })
    }

    /// `entry.text.str_id()` 的直白写法。
    pub fn str_id(self) -> StringId {
        self.text
    }
}

/// `table::LongEntry`（`table.h:47-50`）：tail 层的一条词条，多带一段 `extra_code`。
///
/// `extra_code` 是第 4 个音节起的部分（前 3 个在 `TrunkIndexNode::key` 路径上）。
#[derive(Debug, Clone, PartialEq)]
pub struct LongEntry {
    /// `Code extra_code`
    pub extra_code: Vec<SyllableId>,
    pub entry: Entry,
}

impl LongEntry {
    /// 读 `pos` 处的 16 字节。`extra_code` 的 `List::at` 为 NULL 且 size > 0 时返回 `None`。
    pub fn read(file: &MappedFile, pos: usize) -> Option<Self> {
        let size = file.list_size(pos)? as usize;
        let extra_code = if size == 0 {
            Vec::new()
        } else {
            let data = file.list_data(pos)?;
            let mut code = Vec::with_capacity(size);
            for index in 0..size {
                code.push(file.i32_at(data + index * 4)?);
            }
            code
        };
        Some(Self {
            extra_code,
            entry: Entry::read(file, pos + LIST_SIZE)?,
        })
    }
}

/// `table::Metadata`（`table.h:79-92`）解出来的**值**（C++ 那边是指向 mmap 的裸指针）。
#[derive(Debug, Clone, PartialEq)]
pub struct Metadata {
    /// `format[32]`：NUL 结尾，正常是 `"Rime::Table/4.0"`
    pub format: String,
    pub dict_file_checksum: u32,
    pub num_syllables: u32,
    pub num_entries: u32,
    /// `OffsetPtr<Syllabary>` 解出来的绝对偏移（`None` = NULL）
    pub syllabary: Option<usize>,
    /// `OffsetPtr<Index>` 解出来的绝对偏移（`None` = NULL）
    pub index: Option<usize>,
    /// v2 之前不存在；`Table::Build` 里从没写过（恒 0）
    pub reserved_1: i32,
    /// 同上
    pub reserved_2: i32,
    /// `OffsetPtr<char>` 解出来的绝对偏移（`None` = NULL）
    pub string_table: Option<usize>,
    /// marisa 镜像的字节数（`OffsetPtr` 只有起点，长度靠这个字段）
    pub string_table_size: u32,
}

impl Metadata {
    /// `Find<table::Metadata>(0)` + 逐字段读。整段越界 → `None`（C++ 是 UB）。
    pub fn read(file: &MappedFile, pos: usize) -> Option<Self> {
        Some(Self {
            format: file
                .fixed_str_at(
                    pos + metadata_layout::FORMAT_POS,
                    metadata_layout::FORMAT_MAX_LENGTH,
                )?
                .to_string(),
            dict_file_checksum: file.u32_at(pos + metadata_layout::DICT_FILE_CHECKSUM_POS)?,
            num_syllables: file.u32_at(pos + metadata_layout::NUM_SYLLABLES_POS)?,
            num_entries: file.u32_at(pos + metadata_layout::NUM_ENTRIES_POS)?,
            syllabary: file.deref(pos + metadata_layout::SYLLABARY_POS),
            index: file.deref(pos + metadata_layout::INDEX_POS),
            reserved_1: file.i32_at(pos + metadata_layout::RESERVED_1_POS)?,
            reserved_2: file.i32_at(pos + metadata_layout::RESERVED_2_POS)?,
            string_table: file.deref(pos + metadata_layout::STRING_TABLE_POS),
            string_table_size: file.u32_at(pos + metadata_layout::STRING_TABLE_SIZE_POS)?,
        })
    }

    /// `atof(&format[12])` 的结果（`table.cc:291`）。
    pub fn format_version(&self) -> Option<f64> {
        parse_format_version(&self.format, K_TABLE_FORMAT_PREFIX).map(|(version, _)| version)
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// `find_node()`（`table.cc:143-150`）：`TrunkIndexNode` 按 `key` 升序，二分。
///
/// C++ 的 `std::lower_bound` + `it == last || key < it->key ? last : it`：
/// 找不到就返回"末尾"，这里换成 `None`。
fn find_trunk_node(file: &MappedFile, array_pos: usize, key: SyllableId) -> Option<usize> {
    let size = file.u32_at(array_pos)? as usize;
    let base = array_pos + 4;
    let (mut low, mut high) = (0usize, size);
    while low < high {
        let mid = low + (high - low) / 2;
        let node_key = file.i32_at(base + mid * TRUNK_INDEX_NODE_SIZE)?;
        if node_key < key {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    if low >= size {
        return None;
    }
    let node = base + low * TRUNK_INDEX_NODE_SIZE;
    (file.i32_at(node)? == key).then_some(node)
}

/// `class Table : public MappedFile`（`table.h:183-236`）—— 只读形态。
#[derive(Debug)]
pub struct Table {
    /// C++ 的 `MappedFile` 基类
    file: MappedFile,
    /// C++ 的 `metadata_`（这里解析成值）
    metadata: Option<Metadata>,
    /// C++ 的 `syllabary_`：`Array<StringType>` 的绝对偏移
    syllabary: Option<usize>,
    /// C++ 的 `index_`：`HeadIndex` 的绝对偏移
    index: Option<usize>,
    /// C++ 的 `string_table_`
    string_table: Option<StringTable>,
    /// `Load()` 从 `format` 里解析出的版本号
    format: f64,
    /// C++ 的 `the<StringTableBuilder> string_table_builder_`（`table.h:234`）：
    /// **只在 [`Table::build`] 期间存在**。C++ 在 `OnBuildStart()` 里 `reset(new ...)`、
    /// 在 `OnBuildFinish()` 里用它建树 + 回填 + Dump；之后对象还挂着（白占内存），
    /// 这里在 Build 收尾时丢掉。
    string_table_builder: Option<StringTableBuilder>,
}

impl Table {
    // ---------------------------------------------------------------- 打开 / 加载

    /// `Table::Table(path)` + `Load()` 合体（C++ 构造完还要显式调一次 `Load()`）。
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let file = MappedFile::open_read_only(path)?;
        Self::from_mapped_file(file)
    }

    /// 从一份已经有字节的 [`MappedFile`] 加载（测试用；也是 `Build` 之后复用的入口）。
    pub fn from_mapped_file(file: MappedFile) -> io::Result<Self> {
        let mut table = Self::with_file(file);
        table.load()?;
        Ok(table)
    }

    /// 只装文件、不 Load（写路径用：镜像刚建出来时里面还没有合法 metadata）。
    fn with_file(file: MappedFile) -> Self {
        Self {
            file,
            metadata: None,
            syllabary: None,
            index: None,
            string_table: None,
            format: 0.0,
            string_table_builder: None,
        }
    }

    /// `Table::Load()`（`table.cc:269-313`）+ `OnLoad()`（`table.cc:259-263`）。
    ///
    /// 与 C++ 一一对应的六道检查：metadata 在 → `format` 前缀对 → 版本 >= 4.0 →
    /// `syllabary` 非空 → `index` 非空 → 字符串表可映射。失败时把内部状态清空
    /// （C++ 是 `Close()`）。
    pub fn load(&mut self) -> io::Result<()> {
        self.metadata = None;
        self.syllabary = None;
        self.index = None;
        self.string_table = None;
        self.format = 0.0;

        let Some(metadata) = Metadata::read(&self.file, 0) else {
            return Err(invalid("metadata not found"));
        };
        // C++：`strncmp(metadata_->format, kTableFormatPrefix, 12)`
        if !metadata.format.starts_with(K_TABLE_FORMAT_PREFIX) {
            return Err(invalid("invalid metadata"));
        }
        // C++：`atof(&format[12])`；解析不出数字时 atof 给 0.0，会落到"太旧"分支。
        let format = parse_format_version(&metadata.format, K_TABLE_FORMAT_PREFIX)
            .map(|(version, _)| version)
            .unwrap_or(0.0);
        if format < K_TABLE_FORMAT_LOWEST_COMPATIBLE - f64::EPSILON {
            return Err(invalid(format!(
                "table format version {format} is no longer supported. \
                 please upgrade to version {K_TABLE_FORMAT}"
            )));
        }
        let Some(syllabary) = metadata.syllabary else {
            return Err(invalid("syllabary not found"));
        };
        let Some(index) = metadata.index else {
            return Err(invalid("table index not found"));
        };
        // `OnLoad()`：C++ 直接拿 `metadata->string_table.get()` 去 map，NULL 时是 UB/异常。
        // 这里多一道边界检查，坏文件得到的是 Err 而不是崩。
        let Some(string_table_pos) = metadata
            .string_table
            .filter(|_| metadata.string_table_size > 0)
        else {
            return Err(invalid("string table not found"));
        };
        let blob = self
            .file
            .bytes_at(string_table_pos, metadata.string_table_size as usize)
            .ok_or_else(|| invalid("string table is out of range"))?;
        let string_table = StringTable::from_bytes(blob)
            .map_err(|error| invalid(format!("string table: {error}")))?;

        self.format = format;
        self.metadata = Some(metadata);
        self.syllabary = Some(syllabary);
        self.index = Some(index);
        self.string_table = Some(string_table);
        Ok(())
    }

    // ---------------------------------------------------------------- 写路径
    //
    // 对应 `table.cc:315-533`：`Save` / `Build` / `BuildIndex` / `BuildHeadIndex` /
    // `BuildTrunkIndex` / `BuildTailIndex` / `BuildEntryArray` / `BuildEntryList` /
    // `BuildEntry` / `AddString` / `OnBuildStart` / `OnBuildFinish`。
    //
    // **分配顺序就是磁盘布局**（`table.cc:330-390`）：metadata(0) → 音节表 →
    // HeadIndex → 按 `BuildHeadIndex` 的深度优先序交错分配 trunk/tail 数组与词条页 →
    // 最后 `OnBuildFinish` 分配 marisa 镜像。`format` 是最后写的（中途失败的文件读不回来）。

    /// `Table::Build`（`table.cc:330-390`）：**建表 + Load**。
    ///
    /// C++ 的形状是 `Table(path); table.Build(...); table.Save();`。这里 `build` 自己
    /// 负责 `Create(estimated_file_size)`（容量估算 `4096 + 32*syllables + 64*entries`
    /// 照抄 `table.cc:334-337`），返回的 `Table` 已经 `Load` 过、可以直接查询；
    /// 落盘由 [`Self::save`] 完成（[`Self::compile`] 是"Build + Save"一步到位）。
    ///
    /// `num_entries` 是**词条总数**（C++ 由 `EntryCollector` 统计），不是
    /// `vocabulary` 的页数：同码多条词条都算。`dict_file_checksum` 是源 `.dict.yaml`
    /// 的 CRC（`boost::crc_32_type`，初值 0）；测试里可以和真实产物对拍。
    pub fn build(
        path: impl Into<PathBuf>,
        syllabary: &Syllabary,
        vocabulary: &Vocabulary,
        num_entries: usize,
        dict_file_checksum: u32,
    ) -> io::Result<Self> {
        /// `const size_t kReservedSize = 4096;`（`table.cc:334`）
        const K_RESERVED_SIZE: usize = 4096;
        let num_syllables = syllabary.size();
        let estimated_file_size = K_RESERVED_SIZE + 32 * num_syllables + 64 * num_entries;
        let mut table = Self::with_file(MappedFile::create(path, estimated_file_size)?);
        table.build_in_place(syllabary, vocabulary, num_entries, dict_file_checksum)?;
        // builder 只在建树/回填时有用；keys 是 12 万级字符串，留着纯占内存。
        table.string_table_builder = None;
        table.load()?;
        Ok(table)
    }

    /// [`Self::build`] + [`Self::save`]：建好、落盘、可直接 `open` 的 `.table.bin`。
    pub fn compile(
        path: impl Into<PathBuf>,
        syllabary: &Syllabary,
        vocabulary: &Vocabulary,
        num_entries: usize,
        dict_file_checksum: u32,
    ) -> io::Result<Self> {
        let mut table = Self::build(
            path,
            syllabary,
            vocabulary,
            num_entries,
            dict_file_checksum,
        )?;
        table.save()?;
        Ok(table)
    }

    /// `Table::Save`（`table.cc:315-324`）：`ShrinkToFit()`（C++ 只 shrink，落盘靠 munmap；
    /// 这里就是 `Vec<u8>` 截到 `used` 再 `fs::write`）。
    pub fn save(&mut self) -> io::Result<()> {
        if self.index.is_none() {
            // C++：`if (!index_) { LOG(ERROR) << "the table has not been constructed!"; }`
            return Err(io::Error::other("the table has not been constructed!"));
        }
        self.file.save()
    }

    /// `Build()` 去掉 `Create` 的部分：写 metadata / 音节表 / 索引 / 字符串表。
    fn build_in_place(
        &mut self,
        syllabary: &Syllabary,
        vocabulary: &Vocabulary,
        num_entries: usize,
        dict_file_checksum: u32,
    ) -> io::Result<()> {
        let num_syllables = syllabary.size();
        // `metadata_ = Allocate<table::Metadata>()`：它一定是第一个分配，落在 0。
        let metadata_pos = self.file.allocate(metadata_layout::SIZE, 4);
        debug_assert_eq!(metadata_pos, 0, "metadata 必须落在偏移 0");
        // `table.cc:353-355`：计数在开始就写（`format` 留到最后）。
        self.file.put_u32(
            metadata_pos + metadata_layout::DICT_FILE_CHECKSUM_POS,
            dict_file_checksum,
        );
        self.file.put_u32(
            metadata_pos + metadata_layout::NUM_SYLLABLES_POS,
            num_syllables as u32,
        );
        self.file.put_u32(
            metadata_pos + metadata_layout::NUM_ENTRIES_POS,
            num_entries as u32,
        );

        // `OnBuildStart()`（`table.cc:239-242`）
        self.on_build_start()?;

        // `table.cc:361-372`：`CreateArray<table::StringType>(num_syllables)` 后逐个 AddString
        let syllabary_pos = self
            .file
            .create_array(num_syllables, STRING_TYPE_SIZE, 4);
        for (index, syllable) in syllabary.iter().enumerate() {
            let item = syllabary_pos + 4 + index * STRING_TYPE_SIZE;
            self.add_string(syllable, item, 0.0)?;
        }
        self.file
            .set_offset(metadata_pos + metadata_layout::SYLLABARY_POS, syllabary_pos);

        // `table.cc:374-380`
        let index_pos = self.build_index(vocabulary, num_syllables)?;
        self.file
            .set_offset(metadata_pos + metadata_layout::INDEX_POS, index_pos);

        // `OnBuildFinish()`（`table.cc:244-257`）
        self.on_build_finish(metadata_pos)?;

        // `table.cc:386-388`：**最后**才写 format（中途失败的文件 Load 会拒绝）。
        let format = K_TABLE_FORMAT.as_bytes();
        self.file
            .put_bytes(metadata_pos + metadata_layout::FORMAT_POS, format);
        self.file.put_u8(
            metadata_pos + metadata_layout::FORMAT_POS + format.len(),
            0,
        );
        Ok(())
    }

    /// `Table::BuildIndex`（`table.cc:392-396`）：`reinterpret_cast<table::Index*>(BuildHeadIndex(...))`。
    fn build_index(
        &mut self,
        vocabulary: &Vocabulary,
        num_syllables: usize,
    ) -> io::Result<usize> {
        self.build_head_index(vocabulary, num_syllables)
    }

    /// `Table::BuildHeadIndex`（`table.cc:398-422`）：
    /// `CreateArray<HeadIndexNode>(num_syllables)`，音节 id 直接当数组下标。
    fn build_head_index(
        &mut self,
        vocabulary: &Vocabulary,
        num_syllables: usize,
    ) -> io::Result<usize> {
        let index = self
            .file
            .create_array(num_syllables, HEAD_INDEX_NODE_SIZE, 4);
        for (syllable_id, page) in vocabulary.pages() {
            let node = self.head_node(index, *syllable_id, num_syllables)?;
            // `BuildEntryList(entries, &node.entries)`：`entries` 在 node 的 +0。
            self.build_entry_list(&page.entries, node)?;
            if let Some(next_level) = page.next_level.as_deref() {
                let mut code = Code::new();
                code.push(*syllable_id);
                let next_level_index = self.build_trunk_index(&code, next_level)?;
                // `node.next_level` 在 HeadIndexNode 的 +8（List 占 8 字节）。
                self.file.set_offset(node + 8, next_level_index);
            }
        }
        Ok(index)
    }

    /// `HeadIndexNode` 的地址。C++ `index->at[syllable_id]` 越界是 UB，这里报错。
    fn head_node(
        &self,
        index: usize,
        syllable_id: SyllableId,
        num_syllables: usize,
    ) -> io::Result<usize> {
        let id = usize::try_from(syllable_id)
            .ok()
            .filter(|id| *id < num_syllables)
            .ok_or_else(|| invalid(format!("invalid syllable id in vocabulary: {syllable_id}")))?;
        Ok(index + 4 + id * HEAD_INDEX_NODE_SIZE)
    }

    /// `Table::BuildTrunkIndex`（`table.cc:424-459`）：
    /// 第 2/3 个音节各一层 `Array<TrunkIndexNode>`（按 `key` 升序，`find_node` 要二分）；
    /// 到第 3 个音节之后（`code.size() >= kIndexCodeMaxLength`）改挂 tail。
    fn build_trunk_index(
        &mut self,
        prefix: &Code,
        vocabulary: &Vocabulary,
    ) -> io::Result<usize> {
        let index = self
            .file
            .create_array(vocabulary.len(), TRUNK_INDEX_NODE_SIZE, 4);
        for (count, (syllable_id, page)) in vocabulary.pages().iter().enumerate() {
            let node = index + 4 + count * TRUNK_INDEX_NODE_SIZE;
            self.file.put_i32(node, *syllable_id);
            // TrunkIndexNode = { key(4) + List<Entry>(8) + next_level(4) }
            self.build_entry_list(&page.entries, node + 4)?;
            if let Some(next_level) = page.next_level.as_deref() {
                let mut code = prefix.clone();
                code.push(*syllable_id);
                let next = if code.len() < Code::K_INDEX_CODE_MAX_LENGTH {
                    self.build_trunk_index(&code, next_level)?
                } else {
                    self.build_tail_index(&code, next_level)?
                };
                self.file.set_offset(node + 12, next);
            }
        }
        Ok(index)
    }

    /// `Table::BuildTailIndex`（`table.cc:461-490`）：
    /// 第 4 个音节起落在 `LongEntry::extra_code`（`List<SyllableId>`），每条一个。
    fn build_tail_index(
        &mut self,
        _prefix: &Code,
        vocabulary: &Vocabulary,
    ) -> io::Result<usize> {
        // C++：`if (vocabulary.find(-1) == vocabulary.end()) return NULL;`
        let page = vocabulary
            .pages()
            .get(&-1)
            .ok_or_else(|| invalid("tail index page (-1) not found"))?;
        let index = self
            .file
            .create_array(page.entries.len(), LONG_ENTRY_SIZE, 4);
        for (count, src) in page.entries.iter().enumerate() {
            let dest = index + 4 + count * LONG_ENTRY_SIZE;
            // `size_t extra_code_length = src->code.size() - Code::kIndexCodeMaxLength;`
            let extra_code_length = src
                .code
                .len()
                .checked_sub(Code::K_INDEX_CODE_MAX_LENGTH)
                .ok_or_else(|| {
                    invalid(format!(
                        "code too short for tail index: {}",
                        src.code
                    ))
                })?;
            self.file.put_u32(dest, extra_code_length as u32);
            // `dest.extra_code.at = Allocate<SyllableId>(extra_code_length);`
            // C++ 对 0 长度也返回非空指针（只是水位对齐），这里同样无条件 set_offset。
            let data = self.file.allocate(extra_code_length * 4, 4);
            for (offset, syllable) in src.code[Code::K_INDEX_CODE_MAX_LENGTH..]
                .iter()
                .enumerate()
            {
                self.file.put_i32(data + offset * 4, *syllable);
            }
            self.file.set_offset(dest + 4, data);
            // `BuildEntry(*src, &dest.entry)`：Entry 在 LongEntry 的 +8（Code 占 8 字节）。
            self.build_entry(src, dest + LIST_SIZE)?;
        }
        Ok(index)
    }

    /// `Table::BuildEntryArray`（`table.cc:492-503`）：`Array<table::Entry>`。
    ///
    /// 加载路径碰不到它（`TableAccessor` 的 Array 构造函数也没用上），这里为完整性保留。
    fn build_entry_array(&mut self, entries: &ShortDictEntryList) -> io::Result<usize> {
        let array = self.file.create_array(entries.len(), ENTRY_SIZE, 4);
        for (index, entry) in entries.iter().enumerate() {
            self.build_entry(entry, array + 4 + index * ENTRY_SIZE)?;
        }
        Ok(array)
    }

    /// `Table::BuildEntryList`（`table.cc:505-521`）：写 `List<table::Entry>`
    /// （`size` 在 `dest`、`at` 是 `dest+4` 的 `OffsetPtr`）。
    fn build_entry_list(
        &mut self,
        src: &ShortDictEntryList,
        dest: usize,
    ) -> io::Result<()> {
        if src.is_empty() {
            // C++ 也会 `Allocate<Entry>(0)`（返回非空指针，只把水位对齐到 4），
            // 这样空页的 `at` 是一个真偏移而不是 0；读侧只看 size，但布局要对齐。
            self.file.put_u32(dest, 0);
            let at = self.file.allocate(0, 4);
            self.file.set_offset(dest + 4, at);
            return Ok(());
        }
        self.file.put_u32(dest, src.len() as u32);
        let at = self.file.allocate(src.len() * ENTRY_SIZE, 4);
        self.file.set_offset(dest + 4, at);
        for (index, entry) in src.iter().enumerate() {
            self.build_entry(entry, at + index * ENTRY_SIZE)?;
        }
        Ok(())
    }

    /// `Table::BuildEntry`（`table.cc:523-533`）：文本进字符串表（**只记引用，id 回填**），
    /// 权重按 `float` 存。
    fn build_entry(&mut self, dict_entry: &ShortDictEntry, entry: usize) -> io::Result<()> {
        self.add_string(&dict_entry.text, entry, dict_entry.weight)?;
        self.file
            .put_f32(entry + STRING_TYPE_SIZE, dict_entry.weight as f32);
        Ok(())
    }

    /// `Table::AddString`（`table.cc:232-237`）：`string_table_builder_->Add(src, weight, &dest->str_id)`。
    fn add_string(&mut self, src: &str, dest: usize, weight: f64) -> io::Result<()> {
        let builder = self
            .string_table_builder
            .as_mut()
            .ok_or_else(|| io::Error::other("string table builder not started"))?;
        // C++ 存的是 `&dest->str_id`（镜像里的字段）；这里存它的绝对偏移，见
        // `dict/string_table_builder.rs` 的"接口差异"。
        builder.add(src, weight, Some(dest));
        Ok(())
    }

    /// `Table::OnBuildStart`（`table.cc:239-242`）
    fn on_build_start(&mut self) -> io::Result<()> {
        self.string_table_builder = Some(StringTableBuilder::new());
        Ok(())
    }

    /// `Table::OnBuildFinish`（`table.cc:244-257`）：
    /// `Build()`（建树 + 回填）→ `Allocate<char>(BinarySize())` → `Dump` → 写 metadata 指针。
    fn on_build_finish(&mut self, metadata_pos: usize) -> io::Result<()> {
        let builder = self
            .string_table_builder
            .as_mut()
            .ok_or_else(|| io::Error::other("string table builder not started"))?;
        // `StringTableBuilder::Build()` = `trie_.build(keys_)` + `UpdateReferences()`；
        // 后者要写镜像，所以两步都在这（C++ 里 UpdateReferences 在 Build 内）。
        builder
            .build_and_update(&mut self.file)
            .map_err(|error| invalid(error.to_string()))?;
        // `size_t image_size = string_table_builder_->BinarySize();`
        let image_size = builder.binary_size();
        let image = self.file.allocate(image_size, 1);
        builder
            .dump(&mut self.file, image)
            .map_err(|error| invalid(error.to_string()))?;
        self.file.set_offset(
            metadata_pos + metadata_layout::STRING_TABLE_POS,
            image,
        );
        self.file.put_u32(
            metadata_pos + metadata_layout::STRING_TABLE_SIZE_POS,
            image_size as u32,
        );
        Ok(())
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

    /// C++ 的 `string_table_`（`OnLoad()` 里建好）。
    pub fn string_table(&self) -> Option<&StringTable> {
        self.string_table.as_ref()
    }

    /// `Table::dict_file_checksum()`（`table.cc:326-328`）
    pub fn dict_file_checksum(&self) -> u32 {
        self.metadata
            .as_ref()
            .map(|metadata| metadata.dict_file_checksum)
            .unwrap_or(0)
    }

    /// 音节表长度（= `metadata->num_syllables` = `HeadIndex` 的元素数）。
    pub fn syllabary_len(&self) -> usize {
        self.metadata
            .as_ref()
            .map(|metadata| metadata.num_syllables as usize)
            .unwrap_or(0)
    }

    /// 索引顶层（`HeadIndex`）的元素数。正常 == [`Self::syllabary_len`]。
    pub fn index_len(&self) -> Option<usize> {
        let index = self.index?;
        Some(self.file.u32_at(index)? as usize)
    }

    /// `Table::GetString(const table::StringType&)`（`table.cc:228-230`）：
    /// `string_table_->GetString(x.str_id())`，坏 id 给空串（C++ 同样如此，只多打一条日志）。
    pub fn get_string(&self, text: &StringType) -> String {
        self.string_table
            .as_ref()
            .map(|table| table.get_string_or_empty(*text))
            .unwrap_or_default()
    }

    /// `Table::GetEntryText(const table::Entry&)`（`table.cc:632-634`）
    pub fn entry_text(&self, entry: &Entry) -> String {
        self.get_string(&entry.text)
    }

    /// `Table::GetSyllableById(SyllableId)`（`table.cc:543-548`）：越界/负数 → 空串。
    pub fn get_syllable_by_id(&self, syllable_id: SyllableId) -> String {
        self.syllable(syllable_id).unwrap_or_default()
    }

    /// [`Self::get_syllable_by_id`] 的 Rust 版：越界/负数 → `None`。
    pub fn syllable(&self, syllable_id: SyllableId) -> Option<String> {
        if syllable_id < 0 {
            return None;
        }
        let syllabary = self.syllabary?;
        let size = self.file.u32_at(syllabary)? as usize;
        if syllable_id as usize >= size {
            return None;
        }
        let item = self
            .file
            .array_item(syllabary, syllable_id as usize, STRING_TYPE_SIZE)?;
        let string_id = self.file.u32_at(item)?;
        self.string_table.as_ref()?.get_string(string_id)
    }

    /// `Table::GetSyllabary(Syllabary* result)`（`table.cc:535-541`）：整张音节表按 id 序。
    pub fn syllabary(&self) -> Option<Syllabary> {
        let syllabary = self.syllabary?;
        let size = self.file.u32_at(syllabary)? as usize;
        let mut result = Syllabary::new();
        for index in 0..size {
            let item = self.file.array_item(syllabary, index, STRING_TYPE_SIZE)?;
            let string_id = self.file.u32_at(item)?;
            result.insert(self.get_string(&string_id));
        }
        Some(result)
    }

    // ---------------------------------------------------------------- 查询

    /// `Table::QueryWords(SyllableId)`（`table.cc:550-553`）：单音节词条。
    pub fn query_words(&self, syllable_id: SyllableId) -> TableAccessor<'_> {
        TableQuery::new(self).access(syllable_id)
    }

    /// `Table::QueryPhrases(const Code&)`（`table.cc:555-569`）。
    ///
    /// 前 3 个音节逐层下沉，第 4 个音节起交给 tail；`code` 长度正好卡在某一层时
    /// 直接取那一层的 `List`。
    pub fn query_phrases(&self, code: &Code) -> TableAccessor<'_> {
        if code.is_empty() {
            return TableAccessor::empty(self);
        }
        let mut query = TableQuery::new(self);
        for index in 0..Code::K_INDEX_CODE_MAX_LENGTH {
            if code.len() == index + 1 {
                return query.access(code[index]);
            }
            if !query.advance(code[index]) {
                return TableAccessor::empty(self);
            }
        }
        query.access(-1)
    }

    /// `Table::Query(const SyllableGraph&, size_t start_pos, TableQueryResult*)`
    /// （`table.cc:571-630`）：把**音节图**上的每条可能切分拿去查表。
    ///
    /// # 输入
    ///
    /// * `syll_graph`：音节切分的产物（[`SyllableGraph`]），这里只读两样东西 ——
    ///   [`SyllableGraph::indices`]（`start_pos` → 音节 id → 该音节的一串边属性）与
    ///   `interpreted_length`（到哪为止算"输入完了"）。`vertices` / `edges` 一概不碰。
    /// * `start_pos`：从输入串的哪个字节位置开始查（translator 通常传 0；
    ///   拼音句子里前一段已经定字时会传下一段的起点）。
    ///
    /// # 算法（逐行照抄 `table.cc:577-630`）
    ///
    /// 广度优先地在音节图上走：队列元素是 `(当前位置, 走到这里的 TableQuery)`。
    ///
    /// 1. 取 `indices[current_pos]`：这个位置能起哪些音节、各自到哪个 `end_pos`；
    /// 2. 对每个 `(syll_id, props)`：先用**当前没下沉的** `query` 取一页词条
    ///    （`Access`，位置在这一页的 `head`/`trunk`/`tail`），非空就记到 `result[end_pos]`；
    /// 3. 若 `end_pos < interpreted_length` 且还能往下沉一层（`Advance`），
    ///    就把 `(end_pos, 下沉后的 query)` 压进队列，然后 `Backdate` 回退，继续试同层的
    ///    下一个音节 —— 所以同一层里所有分支互不污染；
    /// 4. 已经沉到第 3 层（`level == kIndexCodeMaxLength == 3`）时不再下沉，
    ///    直接 `Access(-1)` 取 tail（那里存的是 4 音节及以上的长词条，`extra_code`
    ///    补齐剩下的音节）。
    ///
    /// # 结果语义：**只取词条，不排序、不去重**
    ///
    /// 返回 `end_pos -> Vec<TableAccessor>`。每个 accessor 是**一页连续的词条**
    /// （`List<table::Entry>` 或 `TailIndex`），内部顺序就是 `.table.bin` 里的物理顺序。
    /// 本函数**不做**这些事（都是 `gear/script_translator.cc` 的活）：
    ///
    /// * 不按 `credibility` / `quality_len` / `weight` 排序；
    /// * 不在不同 `end_pos`、不同切分之间去重（同一个词条可能从多条路径被查到）；
    /// * 不合并同文本的候选，也不填 `comment` / `preedit`（那些不在 `.table.bin` 里）；
    /// * 不筛"输入是否走完"（`end_pos == interpreted_length` 由调用方自己判断）。
    ///
    /// 每个 accessor 都带着两笔账（`table.cc:610-616`，给排序用）：
    /// `credibility` = 这条路径上各音节 `props.credibility` 之和；
    /// `quality_len` = 其中**普通拼写**（`kNormalSpelling`）覆盖的字节数之和。
    ///
    /// # 与 C++ 的差异
    ///
    /// * 返回 `TableQueryResult` 而不是"填 `result` 指针 + 返回 `bool`"：
    ///   C++ 的返回值就是 `!result->empty()`，而空结果与"`start_pos` 越界/没索引"
    ///   在 C++ 里也区分不出来（都是 `false` + 空表），所以这里用空表表达同一件事。
    /// * `table.cc:600-608` 那段 `if (false)` 的歧义罚分是**死代码**（C++ 里也永不执行），
    ///   这里不抄；`props.ambiguous_source_positions` 因此也用不上。
    pub fn query(
        &self,
        syll_graph: &SyllableGraph,
        start_pos: usize,
    ) -> TableQueryResult<'_> {
        let mut result = TableQueryResult::new();
        if self.index.is_none() || start_pos >= syll_graph.interpreted_length {
            return result;
        }
        // `table.cc:577`：`std::queue<pair<size_t, TableQuery>>`
        let mut queue: VecDeque<(usize, TableQuery<'_>)> = VecDeque::new();
        queue.push_back((start_pos, TableQuery::new(self)));
        while let Some((current_pos, mut query)) = queue.pop_front() {
            // `table.cc:584-587`：这个位置没有起点信息就跳过
            let Some(index) = syll_graph.indices().get(&current_pos) else {
                continue;
            };
            // `table.cc:588-594`：已经沉到第 3 层，直接取 tail（`Access(-1)`）
            if query.level() == Code::K_INDEX_CODE_MAX_LENGTH {
                let accessor = query.access(-1);
                if !accessor.exhausted() {
                    result.entry(current_pos).or_default().push(accessor);
                }
                continue;
            }
            // `table.cc:595-627`：同一位置上的每个 (音节 id, 边属性) 都试一遍
            for (syll_id, spellings) in index {
                for props in spellings {
                    let end_pos = props.end_pos();
                    // `table.cc:601-609`：C++ 里被 `if (false)` 关掉的歧义罚分（死代码）
                    let next_credibility = props.credibility;
                    // `table.cc:611-614`：全码匹配长度积分
                    let is_normal_spelling = props.r#type == SpellingType::Normal;
                    let delta_quality_len = if is_normal_spelling {
                        end_pos.saturating_sub(current_pos) as f64
                    } else {
                        0.0
                    };
                    let accessor =
                        query.access_with(*syll_id, next_credibility, delta_quality_len);
                    if !accessor.exhausted() {
                        result.entry(end_pos).or_default().push(accessor);
                    }
                    // `table.cc:620-626`：还能往下沉就压队列，然后立刻回退
                    if end_pos < syll_graph.interpreted_length
                        && query.advance_with(
                            *syll_id,
                            next_credibility,
                            delta_quality_len,
                            current_pos,
                        )
                    {
                        queue.push_back((end_pos, query.clone()));
                        query.backdate();
                    }
                }
            }
        }
        result
    }

    // ---------------------------------------------------------------- 全量取词

    /// **C++ 没有这个 API**：把整个索引按**物理顺序**走一遍，返回 `(code, entry)`。
    ///
    /// 顺序 = `head` 按音节 id 升序 → 每个节点先自己的 `List<Entry>`、再递归下一层
    /// （trunk 的节点按 `key` 升序）→ 第 4 个音节起的 tail 按数组序。见模块文档
    /// 「两种顺序」：这是 `.table.bin` 的顺序，**不是** `*.dict.yaml` 的行序。
    ///
    /// 读不到的坏结构**静默跳过**（不会 panic），因此返回条数可以和
    /// `metadata->num_entries` 对账。
    pub fn entries_in_file_order(&self) -> Vec<(Code, Entry)> {
        let mut entries = Vec::new();
        self.walk_entries(|code, entry| entries.push((code.clone(), *entry)));
        entries
    }

    /// **C++ 没有这个 API**：[`Self::entries_in_file_order`] 的结果按 `weight` **降序**，
    /// 同权重按 text **升序**（稳定排序：`(weight, text)` 都相同的保留文件序）。
    ///
    /// `weight` 为 NaN 时 `partial_cmp` 给 `None`，按"相等"处理（C++ 那边是未定义行为）。
    /// text 一次算好再排，比较里不再查字符串表。
    pub fn entries_sorted_by_weight(&self) -> Vec<(Code, Entry)> {
        let mut rows: Vec<(Code, Entry, String)> = self
            .entries_in_file_order()
            .into_iter()
            .map(|(code, entry)| {
                let text = self.entry_text(&entry);
                (code, entry, text)
            })
            .collect();
        rows.sort_by(|a, b| {
            b.1
                .weight
                .partial_cmp(&a.1.weight)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.2.cmp(&b.2))
        });
        rows.into_iter()
            .map(|(code, entry, _)| (code, entry))
            .collect()
    }

    // ---------------------------------------------------------------- 遍历实现

    /// 把整个索引走一遍，按 `code()` 的语义（前 3 个音节 + tail 的 `extra_code`）
    /// 回调每一条词条，返回读到的条数。
    pub fn walk_entries(&self, mut visit: impl FnMut(&Code, &Entry)) -> usize {
        self.walk_head(&mut visit)
    }

    fn walk_head<F: FnMut(&Code, &Entry)>(&self, visit: &mut F) -> usize {
        let Some(index) = self.index else {
            return 0;
        };
        let Some(size) = self.file.u32_at(index) else {
            return 0;
        };
        let mut count = 0;
        for index_in_level in 0..size as usize {
            let node = index + 4 + index_in_level * HEAD_INDEX_NODE_SIZE;
            let code = Code::from(vec![index_in_level as SyllableId]);
            count += self.visit_entry_list(node, &code, visit);
            if let Some(next_level) = self.file.deref(node + 8) {
                // 这一层的节点带 2 个音节（head 贡献第 1 个，trunk 的 key 是第 2 个）
                count += self.walk_trunk(next_level, 2, &code, visit);
            }
        }
        count
    }

    fn walk_trunk<F: FnMut(&Code, &Entry)>(
        &self,
        array_pos: usize,
        depth: usize,
        prefix: &Code,
        visit: &mut F,
    ) -> usize {
        let Some(size) = self.file.u32_at(array_pos) else {
            return 0;
        };
        let mut count = 0;
        for index_in_level in 0..size as usize {
            let node = array_pos + 4 + index_in_level * TRUNK_INDEX_NODE_SIZE;
            let Some(key) = self.file.i32_at(node) else {
                continue;
            };
            let mut code = prefix.clone();
            code.push(key);
            count += self.visit_entry_list(node + 4, &code, visit);
            if let Some(next_level) = self.file.deref(node + 12) {
                // `table.cc:442-459`：`code.size() < 3` 继续 trunk，否则整段交给 tail
                if depth < Code::K_INDEX_CODE_MAX_LENGTH {
                    count += self.walk_trunk(next_level, depth + 1, &code, visit);
                } else {
                    count += self.walk_tail(next_level, &code, visit);
                }
            }
        }
        count
    }

    fn walk_tail<F: FnMut(&Code, &Entry)>(
        &self,
        array_pos: usize,
        prefix: &Code,
        visit: &mut F,
    ) -> usize {
        let Some(size) = self.file.u32_at(array_pos) else {
            return 0;
        };
        let mut count = 0;
        for index_in_level in 0..size as usize {
            let node = array_pos + 4 + index_in_level * LONG_ENTRY_SIZE;
            let Some(long_entry) = LongEntry::read(&self.file, node) else {
                continue;
            };
            let mut code = prefix.clone();
            code.0.extend(long_entry.extra_code);
            visit(&code, &long_entry.entry);
            count += 1;
        }
        count
    }

    /// `List<table::Entry>`（`list_pos` 指向 `u32 size`），回调每一条。
    fn visit_entry_list<F: FnMut(&Code, &Entry)>(
        &self,
        list_pos: usize,
        code: &Code,
        visit: &mut F,
    ) -> usize {
        let Some(size) = self.file.list_size(list_pos) else {
            return 0;
        };
        if size == 0 {
            return 0;
        }
        let Some(data) = self.file.list_data(list_pos) else {
            return 0;
        };
        let mut count = 0;
        for index in 0..size as usize {
            let Some(entry) = Entry::read(&self.file, data + index * ENTRY_SIZE) else {
                break;
            };
            visit(code, &entry);
            count += 1;
        }
        count
    }
}

/// `class TableAccessor`（`table.h:96-134`）：指向某一页里的一段词条，游标式前进。
///
/// C++ 存的是 `const table::Entry* entries_` / `const table::LongEntry* long_entries_`
/// 两个裸指针；这里存**绝对偏移 + 条数**，读的时候才碰 [`MappedFile`]。
#[derive(Debug, Clone)]
pub struct TableAccessor<'a> {
    table: &'a Table,
    index_code: Code,
    /// `entries_`：`List<table::Entry>` 的数据起点 + 条数（`None` = 空）
    entries: Option<(usize, usize)>,
    /// `long_entries_`：`TailIndex` 的数据起点 + 条数（`None` = 空）
    long_entries: Option<(usize, usize)>,
    cursor: usize,
    credibility: f64,
    quality_len: f64,
}

impl<'a> TableAccessor<'a> {
    /// 空 accessor（C++ 的 `TableAccessor()`）：`exhausted() == true`。
    fn empty(table: &'a Table) -> Self {
        Self {
            table,
            index_code: Code::new(),
            entries: None,
            long_entries: None,
            cursor: 0,
            credibility: 0.0,
            quality_len: 0.0,
        }
    }

    /// `TableAccessor(const Code&, const List<table::Entry>*, ...)`（`table.cc:24-32`）
    fn from_list(
        table: &'a Table,
        index_code: Code,
        list_pos: usize,
        credibility: f64,
        quality_len: f64,
    ) -> Self {
        let size = table.file.list_size(list_pos).unwrap_or(0) as usize;
        let entries = match (size, table.file.list_data(list_pos)) {
            (0, _) => None,
            (_, Some(data)) => Some((data, size)),
            (_, None) => None,
        };
        Self {
            table,
            index_code,
            entries,
            long_entries: None,
            cursor: 0,
            credibility,
            quality_len,
        }
    }

    /// `TableAccessor(const Code&, const table::TailIndex*, ...)`（`table.cc:44-52`）
    fn from_tail(
        table: &'a Table,
        index_code: Code,
        tail_pos: usize,
        credibility: f64,
        quality_len: f64,
    ) -> Self {
        let size = table.file.u32_at(tail_pos).unwrap_or(0) as usize;
        let data = tail_pos + 4;
        let long_entries = if size == 0 {
            None
        } else if table
            .file
            .bytes_at(data, size.saturating_mul(LONG_ENTRY_SIZE))
            .is_some()
        {
            Some((data, size))
        } else {
            None
        };
        Self {
            table,
            index_code,
            entries: None,
            long_entries,
            cursor: 0,
            credibility,
            quality_len,
        }
    }

    /// `TableAccessor::exhausted()`（`table.cc:54-59`）
    pub fn exhausted(&self) -> bool {
        self.remaining() == 0
    }

    /// `TableAccessor::remaining()`（`table.cc:61-66`）
    pub fn remaining(&self) -> usize {
        match (self.entries, self.long_entries) {
            (Some((_, size)), _) | (_, Some((_, size))) => size.saturating_sub(self.cursor),
            _ => 0,
        }
    }

    /// `TableAccessor::Next()`（`table.cc:95-100`）：前进一条，返回"还有没有下一条"。
    pub fn next(&mut self) -> bool {
        if self.exhausted() {
            return false;
        }
        self.cursor += 1;
        !self.exhausted()
    }

    /// `TableAccessor::entry()`（`table.cc:68-75`）：当前词条；耗尽时 `None`（C++ 是 NULL）。
    pub fn entry(&self) -> Option<Entry> {
        if self.exhausted() {
            return None;
        }
        if let Some((data, _)) = self.entries {
            Entry::read(&self.table.file, data + self.cursor * ENTRY_SIZE)
        } else if let Some((data, _)) = self.long_entries {
            LongEntry::read(&self.table.file, data + self.cursor * LONG_ENTRY_SIZE)
                .map(|long_entry| long_entry.entry)
        } else {
            None
        }
    }

    /// 当前词条的文本（**C++ 没这个方法**：那边写的是 `table->GetEntryText(*accessor.entry())`）。
    pub fn entry_text(&self) -> Option<String> {
        self.entry().map(|entry| self.table.entry_text(&entry))
    }

    /// `TableAccessor::extra_code()`（`table.cc:77-81`）：只有 tail 层有；否则 `None`。
    pub fn extra_code(&self) -> Option<Vec<SyllableId>> {
        let (data, size) = self.long_entries?;
        if self.cursor >= size {
            return None;
        }
        LongEntry::read(&self.table.file, data + self.cursor * LONG_ENTRY_SIZE)
            .map(|long_entry| long_entry.extra_code)
    }

    /// `const Code& index_code()`：这一页的索引码（不含 `extra_code`）。
    pub fn index_code(&self) -> &Code {
        &self.index_code
    }

    /// `TableAccessor::credibility()`（`table.h:127`）：这条路径上各音节 credibility 之和。
    ///
    /// C++ 是内联字段访问，这里补成方法：字段私有，`dict::dictionary` 的图查询要用。
    pub fn credibility(&self) -> f64 {
        self.credibility
    }

    /// `TableAccessor::quality_len()`（`table.h:128`）：其中**普通拼写**覆盖的字节数之和。
    pub fn quality_len(&self) -> f64 {
        self.quality_len
    }

    /// `TableAccessor::code()`（`table.cc:83-93`）：`index_code` + `extra_code`。
    pub fn code(&self) -> Code {
        let mut code = self.index_code.clone();
        if let Some(extra) = self.extra_code() {
            code.0.extend(extra);
        }
        code
    }
}

/// `using TableQueryResult = map<int, vector<TableAccessor>>;`（`table.h:133`）。
///
/// 键是**输入串里的位置**（`end_pos`），值是"走到这一步能取到的词条页"。
/// C++ 的键是 `int`，这里用 `usize`：音节图里的位置都是字节下标，不可能是负数。
///
/// 顺序：`BTreeMap` 按键升序（C++ 的 `std::map<int, ...>` 同序），
/// 同一个 `end_pos` 下的 `Vec` 保持 [`Table::query`] 的压入顺序。
pub type TableQueryResult<'a> = BTreeMap<usize, Vec<TableAccessor<'a>>>;

/// `class TableQuery`（`table.h:137-181`）：沿三级索引往下走的游标（含一层可回退的栈）。
///
/// C++ 里 `lv1_index_` / `lv2_index_` / `lv3_index_` / `lv4_index_` 是四个裸指针，
/// 这里换成四个绝对偏移。`Backdate()` 不会清 `lv2..lv4`（C++ 也不清）：下次
/// `Advance()` 的 `Walk()` 会把对应层覆盖掉。
#[derive(Debug, Clone)]
pub struct TableQuery<'a> {
    table: &'a Table,
    lv1_index: Option<usize>,
    lv2_index: Option<usize>,
    lv3_index: Option<usize>,
    lv4_index: Option<usize>,
    level: usize,
    index_code: Code,
    credibility: Vec<f64>,
    quality_len: Vec<f64>,
    last_pos: Vec<usize>,
}

impl<'a> TableQuery<'a> {
    /// `TableQuery(table::Index*)`（`table.h:139`）：从顶层索引起步。
    pub fn new(table: &'a Table) -> Self {
        Self {
            table,
            lv1_index: table.index,
            lv2_index: None,
            lv3_index: None,
            lv4_index: None,
            level: 0,
            index_code: Code::new(),
            credibility: Vec::new(),
            quality_len: Vec::new(),
            last_pos: Vec::new(),
        }
    }

    /// `TableQuery::Access(syllable_id)`（`table.cc:190-215`），两个权重参数默认 0。
    pub fn access(&self, syllable_id: SyllableId) -> TableAccessor<'a> {
        self.access_with(syllable_id, 0.0, 0.0)
    }

    /// 带 `credibility` / `quality_len` 的 `Access`。
    pub fn access_with(
        &self,
        syllable_id: SyllableId,
        credibility: f64,
        quality_len: f64,
    ) -> TableAccessor<'a> {
        let credibility = self.credibility_sum() + credibility;
        let quality_len = self.quality_len_sum() + quality_len;
        match self.level {
            0 => {
                let Some(lv1_index) = self.lv1_index else {
                    return TableAccessor::empty(self.table);
                };
                let Some(size) = self.table.file.u32_at(lv1_index) else {
                    return TableAccessor::empty(self.table);
                };
                if syllable_id < 0 || syllable_id as usize >= size as usize {
                    return TableAccessor::empty(self.table);
                }
                let node = lv1_index + 4 + syllable_id as usize * HEAD_INDEX_NODE_SIZE;
                let mut code = self.index_code.clone();
                code.push(syllable_id);
                TableAccessor::from_list(self.table, code, node, credibility, quality_len)
            }
            1 | 2 => {
                let index = if self.level == 1 {
                    self.lv2_index
                } else {
                    self.lv3_index
                };
                let Some(index) = index else {
                    return TableAccessor::empty(self.table);
                };
                let Some(node) = find_trunk_node(&self.table.file, index, syllable_id) else {
                    return TableAccessor::empty(self.table);
                };
                let mut code = self.index_code.clone();
                code.push(syllable_id);
                TableAccessor::from_list(self.table, code, node + 4, credibility, quality_len)
            }
            3 => {
                let Some(lv4_index) = self.lv4_index else {
                    return TableAccessor::empty(self.table);
                };
                // tail 层只有"一整条 extra_code 列表"，没有按键分的节点 —— 照抄 C++，
                // 把 `syllable_id`（`Query` 里恒为 -1）忽略掉。
                TableAccessor::from_tail(
                    self.table,
                    self.index_code.clone(),
                    lv4_index,
                    credibility,
                    quality_len,
                )
            }
            _ => TableAccessor::empty(self.table),
        }
    }

    /// `TableQuery::Advance(syllable_id)`（`table.cc:102-115`），两个权重参数默认 0。
    pub fn advance(&mut self, syllable_id: SyllableId) -> bool {
        self.advance_with(syllable_id, 0.0, 0.0, 0)
    }

    /// 带 `credibility` / `quality_len` / `last_pos` 的 `Advance`：成功才压栈。
    pub fn advance_with(
        &mut self,
        syllable_id: SyllableId,
        credibility: f64,
        quality_len: f64,
        last_pos: usize,
    ) -> bool {
        if !self.walk(syllable_id) {
            return false;
        }
        self.level += 1;
        self.index_code.push(syllable_id);
        let credibility_sum = self.credibility_sum() + credibility;
        let quality_len_sum = self.quality_len_sum() + quality_len;
        self.credibility.push(credibility_sum);
        self.quality_len.push(quality_len_sum);
        self.last_pos.push(last_pos);
        true
    }

    /// `TableQuery::Backdate()`（`table.cc:117-128`）：上一层；已在根返回 `false`。
    pub fn backdate(&mut self) -> bool {
        if self.level == 0 {
            return false;
        }
        self.level -= 1;
        if self.index_code.len() > self.level {
            self.index_code.0.pop();
            self.credibility.pop();
            self.quality_len.pop();
            self.last_pos.pop();
        }
        true
    }

    /// `TableQuery::Reset()`（`table.cc:130-137`）：回到根。
    pub fn reset(&mut self) {
        self.level = 0;
        self.index_code = Code::new();
        self.credibility.clear();
        self.quality_len.clear();
        self.last_pos.clear();
    }

    pub fn level(&self) -> usize {
        self.level
    }

    /// `credibility_sum()`（`table.h:159-161`）：栈顶（空栈 → 0）。
    pub fn credibility_sum(&self) -> f64 {
        self.credibility.last().copied().unwrap_or(0.0)
    }

    pub fn quality_len_sum(&self) -> f64 {
        self.quality_len.last().copied().unwrap_or(0.0)
    }

    pub fn last_pos(&self) -> usize {
        self.last_pos.last().copied().unwrap_or(0)
    }

    /// 当前索引码（走到的音节串，不含 tail 的 `extra_code`）。
    pub fn index_code(&self) -> &Code {
        &self.index_code
    }

    /// `TableQuery::Walk(SyllableId)`（`table.cc:152-183`）：只走一级，不压栈。
    fn walk(&mut self, syllable_id: SyllableId) -> bool {
        match self.level {
            0 => {
                let Some(lv1_index) = self.lv1_index else {
                    return false;
                };
                let Some(size) = self.table.file.u32_at(lv1_index) else {
                    return false;
                };
                if syllable_id < 0 || syllable_id as usize >= size as usize {
                    return false;
                }
                let node = lv1_index + 4 + syllable_id as usize * HEAD_INDEX_NODE_SIZE;
                let Some(next_level) = self.table.file.deref(node + 8) else {
                    return false;
                };
                self.lv2_index = Some(next_level);
                true
            }
            1 => {
                let Some(lv2_index) = self.lv2_index else {
                    return false;
                };
                let Some(node) = find_trunk_node(&self.table.file, lv2_index, syllable_id) else {
                    return false;
                };
                let Some(next_level) = self.table.file.deref(node + 12) else {
                    return false;
                };
                self.lv3_index = Some(next_level);
                true
            }
            2 => {
                let Some(lv3_index) = self.lv3_index else {
                    return false;
                };
                let Some(node) = find_trunk_node(&self.table.file, lv3_index, syllable_id) else {
                    return false;
                };
                let Some(next_level) = self.table.file.deref(node + 12) else {
                    return false;
                };
                self.lv4_index = Some(next_level);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    use super::*;
    use crate::algo::SpellingProperties;
    use crate::dict::darts::{DoubleArray, TRAVERSE_INVALID};
    use crate::dict::prism::Prism;

    // ------------------------------------------------------------------ 合成文件

    /// 合成表里的音节（顺序即音节 id）。
    const SYLLABLES: [&str; 4] = ["a", "b", "c", "d"];

    /// 合成表里的词条数（和 `metadata->num_entries` 对齐）。
    const SYNTHETIC_NUM_ENTRIES: u32 = 7;

    /// 测试专用：用 `rsmarisa` 现场建树并**序列化**成一段字节，冒充
    /// `StringTableBuilder::Build` + `Dump` 的产物。
    ///
    /// 生产代码只走 [`StringTable::from_bytes`]；这里绕到 `rsmarisa` 是因为
    /// `dict/marisa.rs` 的封装只有读侧（`build_for_test` 只建树、不吐字节）。
    /// 合成文件因此能走**完整**的 `Load()` 路径，而不是靠"注入一个 trie"绕过字符串表。
    fn marisa_blob(keys: &[&str]) -> Vec<u8> {
        use rsmarisa::grimoire::io::writer::Writer;
        let mut keyset = rsmarisa::Keyset::new();
        for key in keys {
            keyset.push_back_str(key).expect("放入 key");
        }
        let mut trie = rsmarisa::Trie::new();
        trie.build(&mut keyset, 0);
        let mut writer = Writer::from_vec(Vec::new());
        trie.write(&mut writer).expect("序列化 marisa trie");
        writer.into_inner().expect("取回 marisa 字节")
    }

    fn string_id(table: &StringTable, text: &str) -> StringId {
        table.lookup(text).expect("合成的字符串表里应有这个键")
    }

    /// 写一页 `List<table::Entry>`：`size` 在 `list_pos`、`at` 是 `list_pos + 4` 的 OffsetPtr。
    fn write_entry_list(
        file: &mut MappedFile,
        strings: &StringTable,
        list_pos: usize,
        entries: &[(&str, f32)],
    ) {
        file.put_u32(list_pos, entries.len() as u32);
        if entries.is_empty() {
            file.put_i32(list_pos + 4, 0);
            return;
        }
        let data = file.allocate(entries.len() * ENTRY_SIZE, 4);
        for (index, (text, weight)) in entries.iter().enumerate() {
            let pos = data + index * ENTRY_SIZE;
            file.put_u32(pos, string_id(strings, text));
            file.put_f32(pos + STRING_TYPE_SIZE, *weight);
        }
        file.set_offset(list_pos + 4, data);
    }

    /// 写一个 `TrunkIndexNode`：`key` + `List<Entry>` + `next_level`。
    fn write_trunk_node(
        file: &mut MappedFile,
        strings: &StringTable,
        node: usize,
        key: SyllableId,
        entries: &[(&str, f32)],
        next_level: Option<usize>,
    ) {
        file.put_i32(node, key);
        write_entry_list(file, strings, node + 4, entries);
        match next_level {
            Some(next) => file.set_offset(node + 12, next),
            None => file.put_i32(node + 12, 0),
        }
    }

    /// 合成一份 `.table.bin`（段与段的关系照 `Table::Build`），覆盖三层 trunk + tail：
    ///
    /// ```text
    /// [0] "a" → 「甲」(1.0)
    ///           next → trunk key=1 "b": 「甲乙」(2.0)
    ///                  next → trunk key=2 "c": 「甲乙丙」(3.0)
    ///                         next → tail: extra_code=[3] 「甲乙丙丁」(4.0)
    ///                 key=2 "c": 「甲丙」(2.5)
    ///                 key=3 "d": 「甲丁」(2.0)   ← 与「甲乙」同权重，用来验 text 次序
    /// [1] "b" → 「乙」(0.5)
    /// [2] "c" → 空页
    /// [3] "d" → 空页
    /// ```
    fn synthetic_table_file() -> MappedFile {
        let keys = [
            "a", "b", "c", "d", // 音节串
            "甲", "乙", "甲乙", "甲乙丙", "甲乙丙丁", "甲丙", "甲丁", // 词条文本
        ];
        let blob = marisa_blob(&keys);
        let strings = StringTable::from_bytes(&blob).expect("合成字符串表应能读");

        let mut file = MappedFile::create("unused", 4096).expect("建内存镜像");
        // ① metadata：C++ `Allocate<table::Metadata>()`，一定在偏移 0
        let metadata_pos = file.allocate(metadata_layout::SIZE, 4);
        assert_eq!(metadata_pos, 0);

        // ② 音节表：Array<StringType>(num_syllables)
        let syllabary = file.create_array(SYLLABLES.len(), STRING_TYPE_SIZE, 4);
        for (index, syllable) in SYLLABLES.iter().enumerate() {
            file.put_u32(
                syllabary + 4 + index * STRING_TYPE_SIZE,
                string_id(&strings, syllable),
            );
        }

        // ③ 头索引：Array<HeadIndexNode>(num_syllables)
        let index = file.create_array(SYLLABLES.len(), HEAD_INDEX_NODE_SIZE, 4);
        let node = |i: usize| index + 4 + i * HEAD_INDEX_NODE_SIZE;

        // ④ 下一层的数组（trunk 第 2 层 3 个节点、trunk 第 3 层 1 个节点、tail 1 条）
        let trunk_1 = file.create_array(3, TRUNK_INDEX_NODE_SIZE, 4);
        let trunk_2 = file.create_array(1, TRUNK_INDEX_NODE_SIZE, 4);
        let tail = file.create_array(1, LONG_ENTRY_SIZE, 4);

        // ⑤ 各页词条（C++ 是"先填本层词条、再递归建下一层"，这里同样交错分配）
        write_entry_list(&mut file, &strings, node(0), &[("甲", 1.0)]);
        write_entry_list(&mut file, &strings, node(1), &[("乙", 0.5)]);
        write_entry_list(&mut file, &strings, node(2), &[]);
        write_entry_list(&mut file, &strings, node(3), &[]);
        for (i, next) in [
            (0usize, Some(trunk_1)),
            (1, None),
            (2, None),
            (3, None),
        ] {
            match next {
                Some(next) => file.set_offset(node(i) + 8, next),
                None => file.put_i32(node(i) + 8, 0),
            }
        }
        write_trunk_node(
            &mut file,
            &strings,
            trunk_1 + 4 + 0 * TRUNK_INDEX_NODE_SIZE,
            1,
            &[("甲乙", 2.0)],
            Some(trunk_2),
        );
        write_trunk_node(
            &mut file,
            &strings,
            trunk_1 + 4 + 1 * TRUNK_INDEX_NODE_SIZE,
            2,
            &[("甲丙", 2.5)],
            None,
        );
        write_trunk_node(
            &mut file,
            &strings,
            trunk_1 + 4 + 2 * TRUNK_INDEX_NODE_SIZE,
            3,
            &[("甲丁", 2.0)],
            None,
        );
        write_trunk_node(
            &mut file,
            &strings,
            trunk_2 + 4,
            2,
            &[("甲乙丙", 3.0)],
            Some(tail),
        );
        // tail 的 LongEntry：Code extra_code(=List) + Entry
        let tail_extra = file.allocate(4, 4);
        file.put_i32(tail_extra, 3); // extra_code = [3] "d"
        let tail_entry = tail + 4;
        file.put_u32(tail_entry, 1); // Code::size = 1
        file.set_offset(tail_entry + 4, tail_extra);
        file.put_u32(tail_entry + LIST_SIZE, string_id(&strings, "甲乙丙丁"));
        file.put_f32(
            tail_entry + LIST_SIZE + STRING_TYPE_SIZE,
            4.0,
        );

        // ⑥ 字符串表镜像（C++ `OnBuildFinish`：`Allocate<char>(image_size)`）
        let blob_pos = file.allocate(blob.len(), 1);
        file.put_bytes(blob_pos, &blob);

        // ⑦ 最后写 metadata（C++ 也是最后才 `strncpy(format)`）
        file.put_bytes(
            metadata_pos + metadata_layout::FORMAT_POS,
            K_TABLE_FORMAT.as_bytes(),
        );
        file.put_u8(
            metadata_pos + metadata_layout::FORMAT_POS + K_TABLE_FORMAT.len(),
            0,
        );
        file.put_u32(
            metadata_pos + metadata_layout::DICT_FILE_CHECKSUM_POS,
            0xcafe_0001,
        );
        file.put_u32(
            metadata_pos + metadata_layout::NUM_SYLLABLES_POS,
            SYLLABLES.len() as u32,
        );
        file.put_u32(
            metadata_pos + metadata_layout::NUM_ENTRIES_POS,
            SYNTHETIC_NUM_ENTRIES,
        );
        file.put_i32(metadata_pos + metadata_layout::RESERVED_1_POS, 0);
        file.put_i32(metadata_pos + metadata_layout::RESERVED_2_POS, 0);
        file.set_offset(metadata_pos + metadata_layout::SYLLABARY_POS, syllabary);
        file.set_offset(metadata_pos + metadata_layout::INDEX_POS, index);
        file.set_offset(metadata_pos + metadata_layout::STRING_TABLE_POS, blob_pos);
        file.put_u32(
            metadata_pos + metadata_layout::STRING_TABLE_SIZE_POS,
            blob.len() as u32,
        );

        file.shrink_to_fit();
        file
    }

    fn load_error(file: MappedFile) -> String {
        match Table::from_mapped_file(file) {
            Ok(_) => "<没有报错>".to_string(),
            Err(error) => error.to_string(),
        }
    }

    fn texts_of(accessor: &mut TableAccessor<'_>) -> Vec<String> {
        let mut texts = Vec::new();
        while !accessor.exhausted() {
            texts.push(accessor.entry_text().expect("未耗尽时 entry 必须可读"));
            accessor.next();
        }
        texts
    }

    fn synthetic_table() -> Table {
        Table::from_mapped_file(synthetic_table_file()).expect("加载合成 table")
    }

    fn texts_in_file_order(table: &Table) -> Vec<String> {
        table
            .entries_in_file_order()
            .into_iter()
            .map(|(_, entry)| table.entry_text(&entry))
            .collect()
    }

    // ------------------------------------------------------------------ metadata

    #[test]
    fn metadata_layout_adds_up_to_68_bytes() {
        use metadata_layout::*;
        assert_eq!(FORMAT_POS, 0);
        assert_eq!(FORMAT_MAX_LENGTH, 32);
        assert_eq!(DICT_FILE_CHECKSUM_POS, 32);
        assert_eq!(NUM_SYLLABLES_POS, 36);
        assert_eq!(NUM_ENTRIES_POS, 40);
        assert_eq!(SYLLABARY_POS, 44);
        assert_eq!(INDEX_POS, 48);
        assert_eq!(RESERVED_1_POS, 52);
        assert_eq!(RESERVED_2_POS, 56);
        assert_eq!(STRING_TABLE_POS, 60);
        assert_eq!(STRING_TABLE_SIZE_POS, 64);
        assert_eq!(SIZE, STRING_TABLE_SIZE_POS + 4);
        assert_eq!(SIZE, 68);
        // 结构体尺寸也钉一下（`sizeof` 的对照值）
        assert_eq!(STRING_TYPE_SIZE, 4);
        assert_eq!(ENTRY_SIZE, 8);
        assert_eq!(LONG_ENTRY_SIZE, 16);
        assert_eq!(HEAD_INDEX_NODE_SIZE, 12);
        assert_eq!(TRUNK_INDEX_NODE_SIZE, 16);
        assert_eq!(LIST_SIZE, 8);
    }

    #[test]
    fn metadata_reads_each_field_at_its_offset() {
        let mut file = MappedFile::create("unused", 256).expect("建内存镜像");
        file.put_bytes(0, b"Rime::Table/4.0");
        file.put_u32(metadata_layout::DICT_FILE_CHECKSUM_POS, 0x1234_5678);
        file.put_u32(metadata_layout::NUM_SYLLABLES_POS, 7);
        file.put_u32(metadata_layout::NUM_ENTRIES_POS, 9);
        file.set_offset(metadata_layout::SYLLABARY_POS, 100);
        file.set_offset(metadata_layout::INDEX_POS, 120);
        file.put_i32(metadata_layout::RESERVED_1_POS, -1);
        file.put_i32(metadata_layout::RESERVED_2_POS, 2);
        file.set_offset(metadata_layout::STRING_TABLE_POS, 200);
        file.put_u32(metadata_layout::STRING_TABLE_SIZE_POS, 16);

        let metadata = Metadata::read(&file, 0).expect("metadata");
        assert_eq!(metadata.format, K_TABLE_FORMAT);
        assert_eq!(metadata.format_version(), Some(4.0));
        assert_eq!(metadata.dict_file_checksum, 0x1234_5678);
        assert_eq!(metadata.num_syllables, 7);
        assert_eq!(metadata.num_entries, 9);
        assert_eq!(metadata.syllabary, Some(100));
        assert_eq!(metadata.index, Some(120));
        assert_eq!(metadata.reserved_1, -1);
        assert_eq!(metadata.reserved_2, 2);
        assert_eq!(metadata.string_table, Some(200));
        assert_eq!(metadata.string_table_size, 16);
    }

    // ------------------------------------------------------------------ 合成文件加载

    #[test]
    fn loads_a_synthetic_table() {
        let table = synthetic_table();
        let metadata = table.metadata().expect("metadata");
        assert_eq!(metadata.format, K_TABLE_FORMAT);
        assert_eq!(table.format(), 4.0);
        assert_eq!(metadata.dict_file_checksum, 0xcafe_0001);
        assert_eq!(table.dict_file_checksum(), 0xcafe_0001);
        assert_eq!(metadata.num_syllables, SYLLABLES.len() as u32);
        assert_eq!(metadata.num_entries, SYNTHETIC_NUM_ENTRIES);
        // 段起点全由 metadata 推：音节表紧跟 68 字节 metadata；索引紧跟音节表
        assert_eq!(metadata.syllabary, Some(metadata_layout::SIZE));
        assert_eq!(
            metadata.index,
            Some(metadata_layout::SIZE + 4 + STRING_TYPE_SIZE * SYLLABLES.len())
        );
        // 字符串表在文件末尾
        assert_eq!(
            metadata.string_table.unwrap() + metadata.string_table_size as usize,
            table.file().used()
        );
        assert_eq!(table.syllabary_len(), SYLLABLES.len());
        assert_eq!(table.index_len(), Some(SYLLABLES.len()));
        // 音节表按 id 序
        for (index, syllable) in SYLLABLES.iter().enumerate() {
            assert_eq!(
                table.get_syllable_by_id(index as SyllableId),
                *syllable,
                "第 {index} 个音节"
            );
        }
        assert_eq!(table.get_syllable_by_id(-1), "");
        assert_eq!(table.get_syllable_by_id(9), "");
        let syllabary = table.syllabary().expect("音节表");
        assert_eq!(syllabary.size(), SYLLABLES.len());
        assert_eq!(syllabary.id_of("c"), Some(2));
    }

    #[test]
    fn query_words_reads_the_head_index() {
        let table = synthetic_table();
        // [0] "a" 只有一条
        let mut words = table.query_words(0);
        assert!(!words.exhausted());
        assert_eq!(words.remaining(), 1);
        assert_eq!(words.index_code().to_string(), "0");
        assert_eq!(words.code().to_string(), "0");
        assert_eq!(words.entry().expect("词条").weight, 1.0);
        assert!(!words.next(), "只有一条，Next() 之后应耗尽");
        assert!(words.exhausted());
        assert!(words.entry().is_none());
        assert_eq!(words.extra_code(), None);
        // [1] "b"
        let mut words = table.query_words(1);
        assert_eq!(texts_of(&mut words), vec!["乙".to_string()]);
        // [2] / [3] 空页：`exhausted()` 直接为真
        for syllable_id in [2, 3] {
            let words = table.query_words(syllable_id);
            assert!(words.exhausted(), "{syllable_id} 是空页");
            assert_eq!(words.remaining(), 0);
            assert_eq!(words.entry_text(), None);
        }
        // 越界 / 负数：空 accessor（C++ 的 `TableAccessor()`）
        for syllable_id in [-1, 4, 100] {
            assert!(table.query_words(syllable_id).exhausted());
        }
    }

    #[test]
    fn query_phrases_walks_trunk_levels_and_tail() {
        let table = synthetic_table();
        // 2 个音节：trunk 第 1 层（3 个节点，顺带验二分的命中/未命中）
        for (code, expected) in [
            (vec![0, 1], "甲乙"),
            (vec![0, 2], "甲丙"),
            (vec![0, 3], "甲丁"),
        ] {
            let expected_code = Code::from(code.clone()).to_string();
            let phrases = table.query_phrases(&Code::from(code));
            assert_eq!(phrases.index_code().to_string(), expected_code);
            assert_eq!(phrases.entry_text().as_deref(), Some(expected));
        }
        // 3 个音节：trunk 第 2 层
        let phrases = table.query_phrases(&Code::from(vec![0, 1, 2]));
        assert_eq!(phrases.code().to_string(), "0,1,2");
        assert_eq!(phrases.entry_text().as_deref(), Some("甲乙丙"));
        assert_eq!(phrases.entry().expect("词条").weight, 3.0);
        // 4 个音节：tail（index_code 只有 3 个 + extra_code）
        let phrases = table.query_phrases(&Code::from(vec![0, 1, 2, 3]));
        assert_eq!(phrases.index_code().to_string(), "0,1,2");
        assert_eq!(phrases.extra_code(), Some(vec![3]));
        assert_eq!(phrases.code().to_string(), "0,1,2,3");
        assert_eq!(phrases.entry_text().as_deref(), Some("甲乙丙丁"));
        assert_eq!(phrases.entry().expect("词条").weight, 4.0);
        // 不存在的分支 / 空码
        assert!(table.query_phrases(&Code::from(vec![0, 9])).exhausted());
        assert!(table.query_phrases(&Code::from(vec![0, 2, 1])).exhausted());
        assert!(table.query_phrases(&Code::from(vec![9])).exhausted());
        assert!(table.query_phrases(&Code::new()).exhausted());
        // 只有一层：`QueryPhrases([0])` 等价于 `QueryWords(0)`
        assert_eq!(
            table
                .query_phrases(&Code::from(vec![0]))
                .entry_text()
                .as_deref(),
            Some("甲")
        );
    }

    #[test]
    fn walk_entries_sees_every_level() {
        let table = synthetic_table();
        let mut seen: Vec<(String, String, f32)> = Vec::new();
        let count = table.walk_entries(|code, entry| {
            seen.push((code.to_string(), table.entry_text(entry), entry.weight));
        });
        assert_eq!(count, SYNTHETIC_NUM_ENTRIES as usize);
        assert_eq!(count, table.metadata().unwrap().num_entries as usize);
        // head 层按音节 id 升序；每个节点"先自己的词条、再下一层"；trunk 按 key 升序
        assert_eq!(
            seen,
            vec![
                ("0".to_string(), "甲".to_string(), 1.0),
                ("0,1".to_string(), "甲乙".to_string(), 2.0),
                ("0,1,2".to_string(), "甲乙丙".to_string(), 3.0),
                ("0,1,2,3".to_string(), "甲乙丙丁".to_string(), 4.0),
                ("0,2".to_string(), "甲丙".to_string(), 2.5),
                ("0,3".to_string(), "甲丁".to_string(), 2.0),
                ("1".to_string(), "乙".to_string(), 0.5),
            ]
        );
    }

    #[test]
    fn file_order_and_weight_order_are_two_different_orders() {
        let table = synthetic_table();
        // ① 文件序：见 `walk_entries_sees_every_level` 的期望
        let file_order = texts_in_file_order(&table);
        assert_eq!(
            file_order,
            vec!["甲", "甲乙", "甲乙丙", "甲乙丙丁", "甲丙", "甲丁", "乙"]
        );
        // ② 字频序：weight 降序；「甲乙」与「甲丁」同为 2.0，按 text 升序（甲丁 < 甲乙）
        let sorted = table.entries_sorted_by_weight();
        let texts: Vec<String> = sorted
            .iter()
            .map(|(_, entry)| table.entry_text(entry))
            .collect();
        assert_eq!(
            texts,
            vec!["甲乙丙丁", "甲乙丙", "甲丙", "甲丁", "甲乙", "甲", "乙"]
        );
        // 权重单调不增
        for window in sorted.windows(2) {
            assert!(
                window[0].1.weight >= window[1].1.weight,
                "权重应降序：{:?}",
                window
            );
        }
        // 两个顺序的元素集合完全一样（只是顺序不同）
        let mut a = file_order.clone();
        a.sort();
        let mut b = texts.clone();
        b.sort();
        assert_eq!(a, b);
        // code 也跟着走（不是只排了文本）
        assert_eq!(sorted[0].0.to_string(), "0,1,2,3");
        assert_eq!(sorted[6].0.to_string(), "1");
    }

    #[test]
    fn table_query_credibility_and_backdate_follow_cpp() {
        let table = synthetic_table();
        let mut query = TableQuery::new(&table);
        assert_eq!(query.level(), 0);
        assert_eq!(query.credibility_sum(), 0.0);
        // 走进第 1 层
        assert!(query.advance_with(0, 0.5, 1.0, 7));
        assert_eq!(query.level(), 1);
        assert_eq!(query.index_code().to_string(), "0");
        assert_eq!(query.credibility_sum(), 0.5);
        assert_eq!(query.quality_len_sum(), 1.0);
        assert_eq!(query.last_pos(), 7);
        // 第 2 层：累加
        assert!(query.advance_with(1, 0.25, 2.0, 9));
        assert_eq!(query.level(), 2);
        assert_eq!(query.credibility_sum(), 0.75);
        assert_eq!(query.quality_len_sum(), 3.0);
        assert_eq!(query.last_pos(), 9);
        // 第 3 层
        assert!(query.advance(2));
        assert_eq!(query.level(), 3);
        assert_eq!(query.index_code().to_string(), "0,1,2");
        // tail
        let accessor = query.access_with(-1, 0.0, 4.0);
        assert_eq!(accessor.credibility(), 0.75);
        assert_eq!(accessor.quality_len(), 7.0);
        assert_eq!(accessor.entry_text().as_deref(), Some("甲乙丙丁"));
        // 第 4 层再 Advance 必须失败（`Walk` 的 else 分支）
        assert!(!query.advance(3));
        // 回退：level 2 的栈顶是"走过 2 个音节"的累计值
        assert!(query.backdate());
        assert_eq!(query.level(), 2);
        assert_eq!(query.index_code().to_string(), "0,1");
        assert_eq!(query.credibility_sum(), 0.75);
        assert_eq!(query.quality_len_sum(), 3.0);
        assert_eq!(query.last_pos(), 9);
        assert!(query.backdate());
        assert_eq!(query.level(), 1);
        assert_eq!(query.index_code().to_string(), "0");
        assert_eq!(query.credibility_sum(), 0.5);
        assert_eq!(query.last_pos(), 7);
        assert!(query.backdate());
        assert_eq!(query.level(), 0);
        assert_eq!(query.credibility_sum(), 0.0);
        assert!(!query.backdate(), "根部不能回退");
        assert_eq!(query.index_code().to_string(), "");
        // Reset
        assert!(query.advance(0));
        query.reset();
        assert_eq!(query.level(), 0);
        assert_eq!(query.last_pos(), 0);
        assert_eq!(query.index_code().to_string(), "");
    }

    #[test]
    fn find_trunk_node_binary_searches_the_sorted_array() {
        // `Array<TrunkIndexNode>`：键 10 / 20 / 30
        let mut file = MappedFile::create("unused", 256).expect("建内存镜像");
        let array = file.create_array(3, TRUNK_INDEX_NODE_SIZE, 4);
        for (index, key) in [10i32, 20, 30].iter().enumerate() {
            file.put_i32(array + 4 + index * TRUNK_INDEX_NODE_SIZE, *key);
        }
        file.shrink_to_fit();
        assert_eq!(find_trunk_node(&file, array, 10), Some(array + 4));
        assert_eq!(
            find_trunk_node(&file, array, 20),
            Some(array + 4 + TRUNK_INDEX_NODE_SIZE)
        );
        assert_eq!(
            find_trunk_node(&file, array, 30),
            Some(array + 4 + 2 * TRUNK_INDEX_NODE_SIZE)
        );
        // 未命中：比最小的小、比最大的大、夹在中间
        for key in [-1, 5, 15, 25, 31] {
            assert_eq!(find_trunk_node(&file, array, key), None, "key {key}");
        }
        // 空数组
        let mut empty = MappedFile::create("unused", 8).expect("建内存镜像");
        let empty_array = empty.create_array(0, TRUNK_INDEX_NODE_SIZE, 4);
        empty.shrink_to_fit();
        assert_eq!(find_trunk_node(&empty, empty_array, 1), None);
    }

    // ------------------------------------------------------------------ 错误路径

    #[test]
    fn load_rejects_bad_metadata() {
        // 空文件 → metadata not found
        let empty = MappedFile::create("unused", 0).unwrap();
        assert_eq!(load_error(empty), "metadata not found");

        // 前缀不对 → invalid metadata
        let mut file = synthetic_table_file();
        file.put_bytes(0, b"Rime::Prism/5.0");
        assert_eq!(load_error(file), "invalid metadata");

        // 版本太旧 → 明确拒绝（`table.cc:292-297`）
        let mut file = synthetic_table_file();
        file.put_bytes(0, b"Rime::Table/3.0");
        let message = load_error(file);
        assert!(
            message.starts_with("table format version 3 is no longer supported"),
            "实际文案：{message}"
        );
        assert!(message.contains(K_TABLE_FORMAT));
    }

    #[test]
    fn load_rejects_missing_sections() {
        // 音节表指针为 NULL
        let mut file = synthetic_table_file();
        file.put_i32(metadata_layout::SYLLABARY_POS, 0);
        assert_eq!(load_error(file), "syllabary not found");

        // 索引指针为 NULL
        let mut file = synthetic_table_file();
        file.put_i32(metadata_layout::INDEX_POS, 0);
        assert_eq!(load_error(file), "table index not found");

        // 字符串表指针为 NULL
        let mut file = synthetic_table_file();
        file.put_i32(metadata_layout::STRING_TABLE_POS, 0);
        assert_eq!(load_error(file), "string table not found");

        // 长度为 0
        let mut file = synthetic_table_file();
        file.put_u32(metadata_layout::STRING_TABLE_SIZE_POS, 0);
        assert_eq!(load_error(file), "string table not found");

        // 长度越界
        let mut file = synthetic_table_file();
        let used = file.used() as u32;
        file.put_u32(metadata_layout::STRING_TABLE_SIZE_POS, used);
        assert_eq!(load_error(file), "string table is out of range");

        // 字符串表内容不是 marisa 镜像
        let mut file = synthetic_table_file();
        let string_table_pos = Metadata::read(&file, 0).unwrap().string_table.unwrap();
        for offset in 0..32 {
            file.put_u8(string_table_pos + offset, 0xAB);
        }
        assert!(
            load_error(file).starts_with("string table:"),
            "坏 blob 应报字符串表错误"
        );
    }

    #[test]
    fn entry_read_checks_bounds() {
        let mut file = MappedFile::create("unused", 8).expect("建内存镜像");
        file.put_u32(0, 42);
        file.put_f32(4, 1.5);
        let entry = Entry::read(&file, 0).expect("8 字节够一个 Entry");
        assert_eq!(entry.text, 42);
        assert_eq!(entry.str_id(), 42);
        assert_eq!(entry.weight, 1.5);
        // 从第 4 个字节开始越界
        assert_eq!(Entry::read(&file, 4), None);
        // LongEntry 的 extra_code：size 有、at 为 NULL → None
        file.put_u32(4, 2);
        assert_eq!(LongEntry::read(&file, 4), None);
    }

    /// 词条结构的字节尺寸（`table.h` 里没有 `static_assert`，这里钉住）。
    #[test]
    fn entry_sizes_match_the_c_header() {
        // 用真实字节布局反证：8 字节 Entry 里前 4 字节是 StringType、后 4 字节是 float
        let mut file = MappedFile::create("unused", 16).expect("建内存镜像");
        file.put_u32(0, 7);
        file.put_f32(4, 2.5);
        let entry = Entry::read(&file, 0).unwrap();
        assert_eq!((entry.text, entry.weight), (7, 2.5));
        // LongEntry：Code(8) + Entry(8)
        file.put_u32(0, 0); // extra_code.size = 0
        file.put_u32(4, 0); // extra_code.at = NULL
        file.put_u32(8, 9);
        file.put_f32(12, 3.5);
        let long_entry = LongEntry::read(&file, 0).expect("LongEntry");
        assert!(long_entry.extra_code.is_empty());
        assert_eq!(long_entry.entry.text, 9);
        assert_eq!(long_entry.entry.weight, 3.5);
        // 尺寸常量自洽
        assert_eq!(LONG_ENTRY_SIZE, LIST_SIZE + ENTRY_SIZE);
        assert_eq!(HEAD_INDEX_NODE_SIZE, LIST_SIZE + 4);
        assert_eq!(TRUNK_INDEX_NODE_SIZE, 4 + LIST_SIZE + 4);
    }

    // ------------------------------------------------------------------ 真实文件

    /// `%APPDATA%\Rime-Next\build\yuling\yuling.table.bin`（只读）。
    fn real_build_dir() -> Option<PathBuf> {
        let appdata = std::env::var_os("APPDATA")?;
        let dir = PathBuf::from(appdata)
            .join("Rime-Next")
            .join("build")
            .join("yuling");
        if dir.join("yuling.table.bin").is_file() {
            Some(dir)
        } else {
            None
        }
    }

    /// 确定性蓄水池抽样：不引入 `rand`，给定种子可复现。
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }
    }

    /// 读真实的 `yuling.table.bin`（只读），把 metadata / 布局 / 全部词条验一遍。
    ///
    /// 没有这份文件时**静默跳过**（不算失败），所以 `cargo test` 在别的机器上照样绿。
    #[test]
    fn reads_real_yuling_table() {
        let Some(build_dir) = real_build_dir() else {
            eprintln!(
                "跳过 reads_real_yuling_table：没有 \
                 %APPDATA%\\Rime-Next\\build\\yuling\\yuling.table.bin"
            );
            return;
        };
        let table = Table::open(build_dir.join("yuling.table.bin")).expect("加载真实 table.bin");
        let metadata = table.metadata().expect("metadata").clone();
        let file_size = table.file().used();
        let string_table = table.string_table().expect("字符串表");

        // ---- ① metadata 实际值 ------------------------------------------------
        eprintln!(
            "metadata: format={:?} version={:?} dict_checksum={:#010x} num_syllables={} \
             num_entries={} syllabary@={:?} index@={:?} reserved=({}, {}) \
             string_table@={:?} string_table_size={} file_size={file_size}",
            metadata.format,
            metadata.format_version(),
            metadata.dict_file_checksum,
            metadata.num_syllables,
            metadata.num_entries,
            metadata.syllabary,
            metadata.index,
            metadata.reserved_1,
            metadata.reserved_2,
            metadata.string_table,
            metadata.string_table_size,
        );
        eprintln!(
            "string_table: num_keys={} binary_size={}（{} + {} = {}）",
            string_table.num_keys(),
            string_table.binary_size(),
            metadata.string_table.unwrap(),
            metadata.string_table_size,
            metadata.string_table.unwrap() + metadata.string_table_size as usize,
        );

        // ---- ② 布局：三段起点都由 metadata 推出来 ------------------------------
        assert_eq!(metadata.format, K_TABLE_FORMAT);
        assert_eq!(table.format(), 4.0);
        assert_eq!(metadata.syllabary, Some(metadata_layout::SIZE));
        let syllabary_end =
            metadata_layout::SIZE + 4 + metadata.num_syllables as usize * STRING_TYPE_SIZE;
        assert_eq!(metadata.index, Some(syllabary_end), "索引应紧跟音节表");
        assert_eq!(
            table.index_len(),
            Some(metadata.num_syllables as usize),
            "HeadIndex 的元素数应等于音节数"
        );
        assert_eq!(
            metadata.string_table.unwrap() + metadata.string_table_size as usize,
            file_size,
            "字符串表镜像应正好铺到文件尾"
        );
        assert_eq!(
            string_table.binary_size(),
            metadata.string_table_size as usize
        );
        // 音节表逐条读：id 必须是合法 marisa id，且与 `Syllabary` 的序一致
        let syllabary = table.syllabary().expect("音节表");
        assert_eq!(syllabary.size(), metadata.num_syllables as usize);
        let mut syllable_ids: BTreeSet<StringId> = BTreeSet::new();
        let mut first_syllables: Vec<String> = Vec::new();
        for syllable_id in 0..metadata.num_syllables as SyllableId {
            let syllable = table.syllable(syllable_id).expect("音节必须可读");
            assert!(!syllable.is_empty(), "{syllable_id} 号音节不该是空串");
            // 独立不变量：`code` 是宇浩码（ASCII 字母），不是中文词条文本 ——
            // 这条能反过来证明"id → 字符串"没被错位（错位的话音节位置会读到汉字）。
            assert!(
                syllable.is_ascii() && !syllable.contains(char::is_whitespace),
                "{syllable_id} 号音节不像 code：{syllable:?}"
            );
            if first_syllables.len() < 10 {
                first_syllables.push(syllable.clone());
            }
            assert_eq!(
                table.get_syllable_by_id(syllable_id),
                syllable,
                "两个入口应一致"
            );
            let item = table
                .file()
                .array_item(
                    metadata.syllabary.unwrap(),
                    syllable_id as usize,
                    STRING_TYPE_SIZE,
                )
                .expect("音节表元素");
            let id = table.file().u32_at(item).expect("id");
            assert!((id as usize) < string_table.num_keys(), "音节 id 越界: {id}");
            syllable_ids.insert(id);
        }
        assert_eq!(
            syllable_ids.len(),
            metadata.num_syllables as usize,
            "音节串不应重复"
        );

        eprintln!(
            "音节表前 10 个: {:?}（共 {} 个，全部是 ASCII code）",
            first_syllables,
            metadata.num_syllables
        );

        // ---- ③ 全量遍历：条数必须和 metadata->num_entries 对上 -----------------
        let mut sample: Vec<(String, String, f32)> = Vec::new();
        let mut rng = Lcg(0x5eed_1234_abcd_0001);
        let mut entries_per_code: BTreeMap<String, usize> = BTreeMap::new();
        let mut texts: BTreeSet<String> = BTreeSet::new();
        let mut entry_ids: BTreeSet<StringId> = BTreeSet::new();
        let mut single_char_texts = 0usize;
        let mut counter = 0usize;
        let count = table.walk_entries(|code, entry| {
            counter += 1;
            let text = table.entry_text(entry);
            assert!(
                (entry.text as usize) < string_table.num_keys(),
                "词条文本 id 越界: {}",
                entry.text
            );
            assert!(!text.is_empty(), "词条 {} 的文本读不出来", code);
            if text.chars().count() == 1 {
                single_char_texts += 1;
            }
            entry_ids.insert(entry.text);
            texts.insert(text.clone());
            *entries_per_code.entry(code.to_string()).or_insert(0) += 1;
            if sample.len() < 10 {
                sample.push((code.to_string(), text, entry.weight));
            } else {
                let index = (rng.next() % counter as u64) as usize;
                if index < 10 {
                    sample[index] = (code.to_string(), text, entry.weight);
                }
            }
        });
        assert_eq!(
            count, metadata.num_entries as usize,
            "遍历条数应等于 metadata->num_entries"
        );
        let max_list = entries_per_code.values().copied().max().unwrap_or(0);
        let max_code = entries_per_code
            .iter()
            .max_by_key(|(_, size)| **size)
            .map(|(code, _)| code.clone())
            .unwrap();
        eprintln!(
            "词条: 读出 {count} 条（num_entries={}），有词条的音节 {}/{} 个，\
             单页最多 {max_list} 条（code {max_code}），不同文本 {} 个，不同文本 id {} 个，\
             单字文本 {single_char_texts} 条",
            metadata.num_entries,
            entries_per_code.len(),
            metadata.num_syllables,
            texts.len(),
            entry_ids.len()
        );
        eprintln!("抽样 10 条（file order 里随机取，code | text | weight）：");
        for (code, text, weight) in &sample {
            eprintln!("  {code:>10} | {text} | {weight}");
        }
        // 抽样必须自洽：同一条 code 的文本能在 `query_words`/`query_phrases` 里查回
        for (code, text, weight) in &sample {
            let code_value = Code::from(
                code.split(',')
                    .map(|part| part.parse::<SyllableId>().expect("code 里的音节 id"))
                    .collect::<Vec<_>>(),
            );
            let mut accessor = if code_value.len() == 1 {
                table.query_words(code_value[0])
            } else {
                table.query_phrases(&code_value)
            };
            let mut found = false;
            while !accessor.exhausted() {
                let entry = accessor.entry().expect("词条");
                if table.entry_text(&entry) == *text && (entry.weight - weight).abs() < 1e-6 {
                    found = true;
                    break;
                }
                accessor.next();
            }
            assert!(found, "抽样 {code} / {text} 没被查询查回");
        }
        // 字符串表的内容应该正好是"音节 ∪ 词条文本"，不多不少
        let union = syllable_ids.union(&entry_ids).count();
        eprintln!(
            "字符串表: {} 个键 = 音节 {} ∪ 词条文本 {} → 并集 {union}",
            string_table.num_keys(),
            syllable_ids.len(),
            entry_ids.len()
        );
        assert_eq!(
            union,
            string_table.num_keys(),
            "字符串表里不该有既不是音节也不是文本的键"
        );
    }

    /// 同一个 code 下，"文件序"与"字频序"前 5 条会长得不一样 —— 用真实文件给肉眼看的证据。
    ///
    /// 挑法是确定的：优先挑"文件序里权重不是降序"且候选最多的那个 code（这种页最能看出
    /// 两种顺序的差别），没有这种页就退化成"候选最多"的 code。
    #[test]
    fn real_file_order_differs_from_weight_order_for_one_code() {
        let Some(build_dir) = real_build_dir() else {
            eprintln!(
                "跳过 real_file_order_differs_from_weight_order_for_one_code：没有 \
                 %APPDATA%\\Rime-Next\\build\\yuling\\yuling.table.bin"
            );
            return;
        };
        let table = Table::open(build_dir.join("yuling.table.bin")).expect("加载真实 table.bin");
        let file_order = table.entries_in_file_order();
        let weight_order = table.entries_sorted_by_weight();
        assert_eq!(file_order.len(), weight_order.len());

        // 按 code 分组（文件序里同一个 code 的条目是连续的，这里仍用 map 保险）
        let mut groups: BTreeMap<String, Vec<(usize, f32)>> = BTreeMap::new();
        for (index, (code, entry)) in file_order.iter().enumerate() {
            groups
                .entry(code.to_string())
                .or_default()
                .push((index, entry.weight));
        }
        let (code, group) = groups
            .iter()
            .filter(|(_, group)| group.len() >= 5)
            .max_by_key(|(_, group)| {
                let descending = group
                    .windows(2)
                    .all(|window| window[0].1 >= window[1].1 - f32::EPSILON);
                (usize::from(!descending), group.len())
            })
            .expect("真实文件里应该有不止 5 个候选的 code");
        let descending = group
            .windows(2)
            .all(|window| window[0].1 >= window[1].1 - f32::EPSILON);
        eprintln!(
            "code {code}: 候选 {} 条，文件序权重{}降序",
            group.len(),
            if descending { "是" } else { "不是" }
        );

        // 文件序前 5 条
        let file_first: Vec<(String, f32)> = group
            .iter()
            .take(5)
            .map(|(index, weight)| (table.entry_text(&file_order[*index].1), *weight))
            .collect();
        // 字频序前 5 条（在全表排序结果里过滤同一个 code，相对顺序不变）
        let weight_first: Vec<(String, f32)> = weight_order
            .iter()
            .filter(|(entry_code, _)| entry_code.to_string() == *code)
            .take(5)
            .map(|(_, entry)| (table.entry_text(entry), entry.weight))
            .collect();
        assert_eq!(weight_first.len(), 5.min(group.len()));
        eprintln!("文件序前 5 条: {file_first:?}");
        eprintln!("字频序前 5 条: {weight_first:?}");
        if !descending {
            assert_ne!(
                file_first, weight_first,
                "文件序本来就不是权重降序，两种顺序不该一样"
            );
        }
    }

    /// 把 prism 的 darts 整棵走一遍（`Prism::trie()` 是公开的；`prism.rs` 里同名助手
    /// 在它的测试模块内，跨模块用不了，所以这里照抄一遍）。
    ///
    /// ⚠️ 枚举是按**字节**走 `alphabet` 的（和 `Prism::expand_search` 一样），所以中途
    /// 会经过非 UTF-8 的字节序列；键本身都是 UTF-8（音节串 / 拼写串），只收集能解码的。
    fn prism_keys(prism: &Prism) -> Vec<(String, SyllableId)> {
        let alphabet = prism.metadata().expect("metadata").alphabet.clone();
        let trie: &DoubleArray = prism.trie();
        let mut keys = Vec::new();
        let mut stack: Vec<(usize, Vec<u8>)> = vec![(0, Vec::new())];
        while let Some((node_pos, prefix)) = stack.pop() {
            for &c in &alphabet {
                let mut next_node_pos = node_pos;
                let mut next_key_pos = 0usize;
                let ret = trie.traverse(&[c], &mut next_node_pos, &mut next_key_pos);
                if ret <= TRAVERSE_INVALID {
                    continue;
                }
                let mut key = prefix.clone();
                key.push(c);
                if ret >= 0 {
                    if let Ok(text) = String::from_utf8(key.clone()) {
                        keys.push((text, ret));
                    }
                }
                stack.push((next_node_pos, key));
            }
        }
        keys
    }

    /// 数 prism 里有多少个键：**只走位置、不拼 key 字节**，所以内存是 O(节点数)。
    ///
    /// `prism_keys` 那种"带前缀枚举"在 370 万节点 / 80 字节键的长码词典上会吃掉几 GB
    /// 内存（每个前缀都 clone 一份）；这里只数 terminal（`has_leaf()` 的节点）。
    fn prism_key_count(prism: &Prism) -> usize {
        let alphabet = prism.metadata().expect("metadata").alphabet.clone();
        let trie: &DoubleArray = prism.trie();
        let mut visited = vec![false; trie.size()];
        let mut stack = vec![0usize];
        let mut count = 0usize;
        while let Some(node_pos) = stack.pop() {
            if node_pos >= visited.len() || visited[node_pos] {
                continue;
            }
            visited[node_pos] = true;
            if trie.units()[node_pos].has_leaf() {
                count += 1;
            }
            for &c in &alphabet {
                let mut next_node_pos = node_pos;
                let mut key_pos = 0usize;
                let ret = trie.traverse(&[c], &mut next_node_pos, &mut key_pos);
                if ret == TRAVERSE_INVALID {
                    continue;
                }
                stack.push(next_node_pos);
            }
        }
        count
    }

    /// 与 `yuling.prism.bin` 交叉验证：table 的 code 集合（音节串）应该被 prism 的
    /// 拼写集合覆盖。
    #[test]
    fn cross_checks_table_codes_against_prism_spellings() {
        let Some(build_dir) = real_build_dir() else {
            eprintln!(
                "跳过 cross_checks_table_codes_against_prism_spellings：没有 \
                 %APPDATA%\\Rime-Next\\build\\yuling\\yuling.table.bin"
            );
            return;
        };
        let table = Table::open(build_dir.join("yuling.table.bin")).expect("加载真实 table.bin");
        let prism = Prism::open(build_dir.join("yuling.prism.bin")).expect("加载真实 prism.bin");
        let table_metadata = table.metadata().expect("table metadata");
        let prism_metadata = prism.metadata().expect("prism metadata");
        assert_eq!(
            table_metadata.num_syllables, prism_metadata.num_syllables,
            "两份文件的音节数应该一样（同一张音节表）"
        );
        assert_eq!(
            table_metadata.dict_file_checksum, prism_metadata.dict_file_checksum,
            "同一个 .dict.yaml 编出来的 checksum 应该一样"
        );

        // table 侧的 code 集合 = 音节表里那些串
        let mut table_codes: BTreeSet<String> = BTreeSet::new();
        for syllable_id in 0..table_metadata.num_syllables as SyllableId {
            table_codes.insert(table.syllable(syllable_id).expect("音节"));
        }
        assert_eq!(table_codes.len(), table_metadata.num_syllables as usize);

        // prism 侧的拼写集合 = darts 的全部键
        let keys = prism_keys(&prism);
        let prism_spellings: BTreeSet<String> = keys.iter().map(|(key, _)| key.clone()).collect();
        assert_eq!(
            prism_spellings.len(),
            prism_metadata.num_spellings as usize,
            "枚举出的键数应等于 num_spellings"
        );
        for (key, value) in keys.iter().take(1000) {
            assert_eq!(prism.get_value(key), Some(*value), "{key}");
        }

        let both = table_codes.intersection(&prism_spellings).count();
        let table_only = table_codes.len() - both;
        let prism_only = prism_spellings.len() - both;
        eprintln!(
            "交叉验证: table 音节 {} 个，prism 拼写 {} 个，重合 {both} 个\
             （table 独有 {table_only}，prism 独有 {prism_only}）",
            table_codes.len(),
            prism_spellings.len(),
        );
        if let Some(sample) = table_codes
            .iter()
            .find(|code| !prism_spellings.contains(*code))
        {
            eprintln!("table 独有样例: {sample:?}");
        }
        assert_eq!(table_only, 0, "table 的每个 code 都应该是 prism 的拼写");
        assert_eq!(both, table_codes.len());
        // 反向：prism 多出来的应该正好是 num_spellings - 音节数（缩写/简拼/补全）
        let mut extra: Vec<&String> = prism_spellings
            .iter()
            .filter(|key| !table_codes.contains(*key))
            .collect();
        extra.sort();
        eprintln!(
            "prism 独有 {} 条（= num_spellings {} - 重合 {both}），样例 {:?}",
            extra.len(),
            prism_metadata.num_spellings,
            &extra[..extra.len().min(10)]
        );
        assert_eq!(extra.len(), prism_only);
    }

    // ==================================================================
    // 对拍 `librime/test/table_test.cc`（205 行 / 63 条断言）
    //
    // 文件里一共 63 条 `ASSERT_*` / `EXPECT_*`：
    //
    // | 出处 | 条数 | 状态 |
    // |---|---|---|
    // | `SetUp()`（`table_test.cc:20-21`，`Build` + `Save`） | 2 | **没移植**：写路径 |
    // | `IntegrityTest`（:96-97） | 2 | 已移植 |
    // | `SimpleQuery`（:101-154） | 36 | 已移植 |
    // | `QueryWithSyllableGraph`（:181-204） | 23 | 已移植 |
    //
    // 合计移植 **61 / 63**。没移植的两条依赖 `Table::Build` / `Table::Save`
    // （`table.cc:330-390` / `:315-324`，本 crate 还没做写路径）。替代方案：
    // `cc_table_file()` 按 `Table::Build` 的分配顺序在内存里摆出**同一份**文件
    // （metadata → 音节表 → HeadIndex → trunk → tail → 词条页 → marisa 镜像），
    // 再走**完整**的 `Table::load()`。所以"建出来的表能被读回来"这件事有覆盖，
    // 但那两条断言（`Build()` 返回 true / `Save()` 返回 true）本身没移植。
    //
    // 每个 `table_cc_*` 测试里的注释 `// table_test.cc:NN` 就是原断言的行号，
    // 一行对一条；`v.Next()` 这类**不是**断言的调用也照抄，但不计入 63。
    // ==================================================================

    /// `table_test.cc:28` 的 `total_num_entries`。
    const CC_NUM_ENTRIES: u32 = 8;

    /// `table_test.cc:43-70`：`syll.insert("0".."4")`，音节 id 就是 0..4。
    const CC_SYLLABLES: [&str; 5] = ["0", "1", "2", "3", "4"];

    /// 写一条 `LongEntry`（tail 层）：`Code extra_code`（`List<SyllableId>`）+ `Entry`。
    fn write_long_entry(
        file: &mut MappedFile,
        strings: &StringTable,
        node: usize,
        extra_code: &[SyllableId],
        text: &str,
        weight: f32,
    ) {
        let data = file.allocate(extra_code.len().max(1) * 4, 4);
        for (index, syllable_id) in extra_code.iter().enumerate() {
            file.put_i32(data + index * 4, *syllable_id);
        }
        file.put_u32(node, extra_code.len() as u32);
        file.set_offset(node + 4, data);
        file.put_u32(node + LIST_SIZE, string_id(strings, text));
        file.put_f32(node + LIST_SIZE + STRING_TYPE_SIZE, weight);
    }

    /// 合成 `table_test.cc` 的样例表：**逐段照 `Table::Build`**（`table.cc:330-390`）
    /// 在一份内存镜像里摆出同样的结构，等价于 C++ 测试 `SetUp()` 里
    /// `Build()` + `Save()` 之后落在磁盘上的那个 `table_test.bin`。
    ///
    /// 样例词典（`table_test.cc:40-92` 的 `PrepareSampleVocabulary`）：
    ///
    /// ```text
    /// 音节表 ["0"], ["1"], ["2"], ["3"], ["4"]      ← id 0..4（set 的字典序）
    ///
    /// head[0] 空页，无下一层                        ← voc[0] 不存在
    /// head[1] yi 1.0
    ///         └→ trunk key=2（空页）
    ///              └→ trunk key=3: yi-er-san 1.0
    ///                   └→ tail: extra=[4]   yi-er-san-si     1.0
    ///                            extra=[2,1] yi-er-san-er-yi  1.0
    /// head[2] er / liang / lia（各 1.0）
    /// head[3] san / sa（各 1.0）
    /// head[4] 空页，无下一层                        ← syll 里有 "4"，voc[4] 没有
    /// ```
    ///
    /// 关键细节都照 `BuildTrunkIndex` / `BuildTailIndex`：
    /// 前 3 个音节各一层 trunk（`code.size() < kIndexCodeMaxLength`），
    /// 第 4 个音节起落进 tail 的 `LongEntry.extra_code`。
    fn cc_table_file() -> MappedFile {
        let keys = [
            "0", "1", "2", "3", "4", // 音节串
            "yi", "er", "liang", "lia", "san", "sa", // 单音节词条
            "yi-er-san", "yi-er-san-si", "yi-er-san-er-yi", // 多音节短语
        ];
        let blob = marisa_blob(&keys);
        let strings = StringTable::from_bytes(&blob).expect("合成字符串表应能读");

        let mut file = MappedFile::create("unused", 8192).expect("建内存镜像");
        // ① metadata：C++ `Allocate<table::Metadata>()`，一定在偏移 0
        let metadata_pos = file.allocate(metadata_layout::SIZE, 4);
        assert_eq!(metadata_pos, 0);

        // ② 音节表：`Array<StringType>(num_syllables)`
        let syllabary = file.create_array(CC_SYLLABLES.len(), STRING_TYPE_SIZE, 4);
        for (index, syllable) in CC_SYLLABLES.iter().enumerate() {
            file.put_u32(
                syllabary + 4 + index * STRING_TYPE_SIZE,
                string_id(&strings, syllable),
            );
        }

        // ③ 头索引：`Array<HeadIndexNode>(num_syllables)`
        let index = file.create_array(CC_SYLLABLES.len(), HEAD_INDEX_NODE_SIZE, 4);
        let node = |i: usize| index + 4 + i * HEAD_INDEX_NODE_SIZE;

        // ④ 下一层的数组：第 2 层 trunk 1 个节点、第 3 层 trunk 1 个节点、tail 2 条
        let trunk_2 = file.create_array(1, TRUNK_INDEX_NODE_SIZE, 4);
        let trunk_3 = file.create_array(1, TRUNK_INDEX_NODE_SIZE, 4);
        let tail = file.create_array(2, LONG_ENTRY_SIZE, 4);

        // ⑤ 各页词条（`BuildHeadIndex` 是"先填本层词条、再递归建下一层"）
        write_entry_list(&mut file, &strings, node(0), &[]);
        write_entry_list(&mut file, &strings, node(1), &[("yi", 1.0)]);
        write_entry_list(
            &mut file,
            &strings,
            node(2),
            &[("er", 1.0), ("liang", 1.0), ("lia", 1.0)],
        );
        write_entry_list(&mut file, &strings, node(3), &[("san", 1.0), ("sa", 1.0)]);
        write_entry_list(&mut file, &strings, node(4), &[]);
        for (index_in_level, next) in [
            (0usize, None),
            (1, Some(trunk_2)),
            (2, None),
            (3, None),
            (4, None),
        ] {
            match next {
                Some(next) => file.set_offset(node(index_in_level) + 8, next),
                None => file.put_i32(node(index_in_level) + 8, 0),
            }
        }

        // 第 2 层：head[1] → trunk key=2（空页）→ 第 3 层
        write_trunk_node(&mut file, &strings, trunk_2 + 4, 2, &[], Some(trunk_3));
        // 第 3 层：key=3 带 "yi-er-san"，下一层是 tail
        write_trunk_node(
            &mut file,
            &strings,
            trunk_3 + 4,
            3,
            &[("yi-er-san", 1.0)],
            Some(tail),
        );
        // tail：`extra_code` = code[3..]（第 4 个音节起）
        write_long_entry(
            &mut file,
            &strings,
            tail + 4,
            &[4],
            "yi-er-san-si",
            1.0,
        );
        write_long_entry(
            &mut file,
            &strings,
            tail + 4 + LONG_ENTRY_SIZE,
            &[2, 1],
            "yi-er-san-er-yi",
            1.0,
        );

        // ⑥ 字符串表镜像（`OnBuildFinish` 里 `Allocate<char>(image_size)` + Dump）
        let blob_pos = file.allocate(blob.len(), 1);
        file.put_bytes(blob_pos, &blob);

        // ⑦ metadata 收尾（C++ 也是最后才 `strncpy(format)`）
        file.put_bytes(
            metadata_pos + metadata_layout::FORMAT_POS,
            K_TABLE_FORMAT.as_bytes(),
        );
        file.put_u8(
            metadata_pos + metadata_layout::FORMAT_POS + K_TABLE_FORMAT.len(),
            0,
        );
        file.put_u32(
            metadata_pos + metadata_layout::DICT_FILE_CHECKSUM_POS,
            0xcafe_0002,
        );
        file.put_u32(
            metadata_pos + metadata_layout::NUM_SYLLABLES_POS,
            CC_SYLLABLES.len() as u32,
        );
        file.put_u32(
            metadata_pos + metadata_layout::NUM_ENTRIES_POS,
            CC_NUM_ENTRIES,
        );
        file.put_i32(metadata_pos + metadata_layout::RESERVED_1_POS, 0);
        file.put_i32(metadata_pos + metadata_layout::RESERVED_2_POS, 0);
        file.set_offset(metadata_pos + metadata_layout::SYLLABARY_POS, syllabary);
        file.set_offset(metadata_pos + metadata_layout::INDEX_POS, index);
        file.set_offset(metadata_pos + metadata_layout::STRING_TABLE_POS, blob_pos);
        file.put_u32(
            metadata_pos + metadata_layout::STRING_TABLE_SIZE_POS,
            blob.len() as u32,
        );

        file.shrink_to_fit();
        file
    }

    /// C++ `SetUp()` 里 `Build()` + `Save()` + `Load()` 的等价物：
    /// 手工合成同一份镜像，然后走**完整**的 `Table::load()`。
    fn cc_table() -> Table {
        Table::from_mapped_file(cc_table_file()).expect("加载 table_test.cc 的合成表")
    }

    /// `table_test.cc:94-98` 的 `IntegrityTest` —— **2 条断言**。
    #[test]
    fn table_cc_integrity_test() {
        let mut table = cc_table();
        // table_test.cc:96 `ASSERT_TRUE(bool(table_))`：C++ 里 `the<Table>` 非空即真，
        // Rust 的等价物是"对象建出来且已加载"（`cc_table()` 的 `expect` + 这里的 metadata）。
        assert!(table.metadata().is_some());
        // table_test.cc:97 `ASSERT_TRUE(table_->Load())`：再显式 Load 一次。
        assert!(table.load().is_ok());
    }

    /// `table_test.cc:100-155` 的 `SimpleQuery` —— **36 条断言**。
    #[test]
    fn table_cc_simple_query() {
        let table = cc_table();

        // table_test.cc:101-103
        assert_eq!("0", table.get_syllable_by_id(0));
        assert_eq!("3", table.get_syllable_by_id(3));
        assert_eq!("4", table.get_syllable_by_id(4));

        // table_test.cc:105-111 `QueryWords(1)`
        let mut v = table.query_words(1);
        assert!(!v.exhausted()); // :106
        assert_eq!(1, v.remaining()); // :107
        assert!(v.entry().is_some()); // :108
        assert_eq!("yi", v.entry_text().expect("yi 那条")); // :109
        assert_eq!(1.0, v.entry().expect("yi 那条").weight); // :110
        assert!(!v.next()); // :111

        // table_test.cc:113-119 `QueryWords(2)`
        let mut v = table.query_words(2);
        assert_eq!(3, v.remaining()); // :114
        assert_eq!("er", v.entry_text().expect("er")); // :115
        v.next(); // :116（不是断言）
        assert_eq!("liang", v.entry_text().expect("liang")); // :117
        v.next(); // :118（不是断言）
        assert_eq!("lia", v.entry_text().expect("lia")); // :119

        // table_test.cc:121-125 `QueryWords(3)`
        let mut v = table.query_words(3);
        assert_eq!(2, v.remaining()); // :122
        assert_eq!("san", v.entry_text().expect("san")); // :123
        v.next(); // :124（不是断言）
        assert_eq!("sa", v.entry_text().expect("sa")); // :125

        // table_test.cc:127-137 `QueryPhrases({1,2,3})`
        let mut v = table.query_phrases(&Code::from(vec![1, 2, 3]));
        assert!(!v.exhausted()); // :132
        assert_eq!(1, v.remaining()); // :133
        assert!(v.entry().is_some()); // :134
        assert_eq!("yi-er-san", v.entry_text().expect("yi-er-san")); // :135
        assert!(v.extra_code().is_none()); // :136 `extra_code() == NULL`
        assert!(!v.next()); // :137

        // table_test.cc:139-154 `QueryPhrases({1,2,3,4})`
        let mut v = table.query_phrases(&Code::from(vec![1, 2, 3, 4]));
        assert!(!v.exhausted()); // :141 `EXPECT_FALSE`
        assert_eq!(2, v.remaining()); // :142
        assert!(v.entry().is_some()); // :143
        assert_eq!("yi-er-san-si", v.entry_text().expect("yi-er-san-si")); // :144
        let extra = v.extra_code().expect("tail 层必须有 extra_code"); // :145
        assert_eq!(1, extra.len()); // :146
        assert_eq!(4, extra[0]); // :147
        assert!(v.next()); // :148
        assert!(v.entry().is_some()); // :149
        assert_eq!("yi-er-san-er-yi", v.entry_text().expect("yi-er-san-er-yi")); // :150
        let extra = v.extra_code().expect("tail 层必须有 extra_code"); // :151
        assert_eq!(2, extra.len()); // :152
        assert_eq!(2, extra[0]); // :153
        assert_eq!(1, extra[1]); // :154
    }

    /// `table_test.cc:157-205` 的 `QueryWithSyllableGraph` —— **23 条断言**。
    ///
    /// # 图是怎么造出来的（与 C++ 的唯一差别）
    ///
    /// C++ 测试直接写 `SyllableGraph` 的公开字段（`:159-178`）：设 `input_length` /
    /// `interpreted_length`、填 5 个 `vertices`、4 条 `edges`，再手工把它们塞进
    /// `indices`。Rust 侧 `indices` 存的是**克隆值**而不是指向 `edges` 的指针，
    /// 且 `end_pos` 只有 algo 层能写，所以这里用
    /// [`SyllableGraph::add_edge`]（一次同时写 `edges` 与 `indices`）等价地造图：
    ///
    /// ```text
    /// 输入 "yiersansi"（9 字节）        edges: 0→2 (yi,  id 1)
    ///                                         2→4 (er,  id 2)
    ///                                         4→7 (san, id 3)
    /// vertices（C++ 设了，但 Query 不读）       7→9 (si,  id 4)
    /// ```
    ///
    /// `vertices` 没填：`Table::Query`（`table.cc:571-630`）只读
    /// `indices` + `interpreted_length`，`vertices` 一次都没碰 —— 填了也不会影响断言。
    #[test]
    fn table_cc_query_with_syllable_graph() {
        let table = cc_table();
        let input = "yiersansi";
        // table_test.cc:160-161
        let mut g = SyllableGraph::with_input_length(input.len());
        // table_test.cc:167-178：4 条边 + 转置（`add_edge` 一次写两边）
        for (start, end, syllable_id) in [(0usize, 2usize, 1), (2, 4, 2), (4, 7, 3), (7, 9, 4)] {
            let mut props = SpellingProperties::default();
            props.r#type = SpellingType::Normal; // kNormalSpelling
            assert!(
                g.add_edge(start, end, syllable_id, props),
                "边 [{start},{end}) 不该重复"
            );
        }
        // 造图自检（不是 C++ 的断言，只保证"和 C++ 手写的那张图同形"）
        assert_eq!(9, g.interpreted_length);
        assert_eq!(4, g.indices().len());
        assert_eq!(4, g.edges.len());

        // ---- 第一次查询：从 0 开始 --------------------------------------
        let result = table.query(&g, 0);
        assert!(!result.is_empty()); // :181 `ASSERT_TRUE(table_->Query(g, 0, &result))`
        assert_eq!(2, result.len()); // :182
        assert!(result.contains_key(&2)); // :183
        assert_eq!(1, result[&2].len()); // :184
        assert_eq!("yi", result[&2][0].entry_text().expect("yi")); // :185 `result[2].front()`
        assert!(result.contains_key(&7)); // :186
        assert_eq!(2, result[&7].len()); // :187
        assert_eq!(
            "yi-er-san",
            result[&7][0].entry_text().expect("yi-er-san")
        ); // :188 front
        assert_eq!(
            "yi-er-san-si",
            result[&7][1].entry_text().expect("yi-er-san-si")
        ); // :189 back
        let extra = result[&7][1].extra_code().expect("back 是 tail 层"); // :190 取到指针
        assert_eq!(1, extra.len()); // :190 `extra_code()->size == 1`
        assert_eq!(4, extra[0]); // :191
        assert!(!result.contains_key(&6)); // :192
        assert!(result.contains_key(&7)); // :193

        // ---- 第二次查询：从 2 开始 --------------------------------------
        let mut result = table.query(&g, 2);
        assert!(!result.is_empty()); // :195 `ASSERT_TRUE(table_->Query(g, 2, &result))`
        assert_eq!(1, result.len()); // :196
        assert!(result.contains_key(&4)); // :197
        assert_eq!(1, result[&4].len()); // :198
        assert_eq!("er", result[&4][0].entry_text().expect("er")); // :199
        assert!(result.get_mut(&4).expect("result[4]")[0].next()); // :200
        assert_eq!(
            "liang",
            result[&4][0].entry_text().expect("liang")
        ); // :201
        assert!(result.get_mut(&4).expect("result[4]")[0].next()); // :202
        assert_eq!("lia", result[&4][0].entry_text().expect("lia")); // :203
        assert!(!result.get_mut(&4).expect("result[4]")[0].next()); // :204
    }

    /// **任务要求的额外对拍**：真实 `yuling.table.bin` 上，
    /// [`Table::entries_in_file_order`] 与 [`Table::entries_sorted_by_weight`]
    /// 取到的是**同一个多重集**（条数相等、`(code, text)` 的计数逐键相等），
    /// 只是**顺序不同** —— 证明排序没丢条、没重条。
    #[test]
    fn real_file_order_and_weight_order_are_the_same_multiset() {
        let Some(build_dir) = real_build_dir() else {
            eprintln!(
                "跳过 real_file_order_and_weight_order_are_the_same_multiset：没有 \
                 %APPDATA%\\Rime-Next\\build\\yuling\\yuling.table.bin"
            );
            return;
        };
        let table = Table::open(build_dir.join("yuling.table.bin")).expect("加载真实 table.bin");
        let metadata = table.metadata().expect("metadata").clone();
        let file_order = table.entries_in_file_order();
        let weight_order = table.entries_sorted_by_weight();

        // ① 条数：两路都得等于 metadata->num_entries（遍历本身没丢没重）
        assert_eq!(file_order.len(), weight_order.len(), "两种顺序条数应相等");
        assert_eq!(
            file_order.len(),
            metadata.num_entries as usize,
            "文件序条数应等于 metadata->num_entries"
        );

        // ② 多重集：`(code, text)` 的计数必须逐键相等
        fn multiset(table: &Table, rows: &[(Code, Entry)]) -> BTreeMap<(String, String), usize> {
            let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
            for (code, entry) in rows {
                *counts
                    .entry((code.to_string(), table.entry_text(entry)))
                    .or_insert(0) += 1;
            }
            counts
        }
        let file_multiset = multiset(&table, &file_order);
        let weight_multiset = multiset(&table, &weight_order);
        assert_eq!(
            file_multiset, weight_multiset,
            "文件序与字频序必须是同一个 (code, text) 多重集"
        );
        assert_eq!(
            file_multiset.values().sum::<usize>(),
            metadata.num_entries as usize
        );

        // ③ 顺序：确实不同（同一个位置上 (code, entry) 不一样的个数）
        let differing = file_order
            .iter()
            .zip(weight_order.iter())
            .filter(|((code_a, entry_a), (code_b, entry_b))| {
                code_a != code_b || entry_a != entry_b
            })
            .count();
        let percent = differing as f64 * 100.0 / file_order.len().max(1) as f64;
        let distinct = file_multiset.len();
        let max_multiplicity = file_multiset.values().copied().max().unwrap_or(0);
        let weight_descending = weight_order
            .windows(2)
            .all(|window| window[0].1.weight >= window[1].1.weight);
        eprintln!(
            "真实 yuling.table.bin: 词条 {} 条（metadata->num_entries = {}）；\
             (code,text) 不同组合 {distinct} 个（单组合最多 {max_multiplicity} 条）；\
             字频序权重降序 = {weight_descending}；\
             两种顺序在 {differing} 个位置上不同（{percent:.1}%），多重集相等 = true",
            file_order.len(),
            metadata.num_entries,
        );
        assert!(weight_descending, "字频序应当权重降序");
        assert!(differing > 0, "两种顺序应当真的不同（否则这条对拍没意义）");
    }

    // ==================================================================
    // 写路径（`Table::Build` / `Save`）
    // ==================================================================

    /// 按 `table_test.cc:40-92` 造一份 `Vocabulary`（码 → 词条页），
    /// 插入顺序与 C++ 逐条一致 —— 同码词条的**页内顺序**必须对得上。
    fn cc_vocabulary() -> (Syllabary, Vocabulary) {
        let syllabary: Syllabary = CC_SYLLABLES.iter().copied().collect();
        let ids = syllabary.syllable_to_id();
        let mut vocabulary = Vocabulary::new();
        let mut push = |code: &[&str], text: &str| {
            let code = Code::from(
                code.iter()
                    .map(|syllable| ids[syllable])
                    .collect::<Vec<SyllableId>>(),
            );
            vocabulary
                .locate_entries(&code)
                .expect("locate_entries")
                .push(ShortDictEntry::new(text, code.clone(), 1.0));
        };
        // table_test.cc:53-86：yi / er+liang+lia / san+sa / yi-er-san /
        // yi-er-san-si / yi-er-san-er-yi（后三条在 trunk/tail 层）。
        push(&["1"], "yi");
        push(&["2"], "er");
        push(&["2"], "liang");
        push(&["2"], "lia");
        push(&["3"], "san");
        push(&["3"], "sa");
        push(&["1", "2", "3"], "yi-er-san");
        push(&["1", "2", "3", "4"], "yi-er-san-si");
        push(&["1", "2", "3", "2", "1"], "yi-er-san-er-yi");
        (syllabary, vocabulary)
    }

    /// `(code.to_string(), text, weight)` 的全文序列（文件序）。
    fn rows_of(table: &Table) -> Vec<(String, String, f32)> {
        table
            .entries_in_file_order()
            .into_iter()
            .map(|(code, entry)| (code.to_string(), table.entry_text(&entry), entry.weight))
            .collect()
    }

    /// `Table::build` 建出来的表与 `Table::Build` 的手写对照物（`cc_table_file`）
    /// 必须**语义完全一致**：metadata、音节表、文件序词条、所有查询路径。
    #[test]
    fn build_reproduces_the_table_test_sample() {
        let (syllabary, vocabulary) = cc_vocabulary();
        let built = Table::build(
            "unused",
            &syllabary,
            &vocabulary,
            CC_NUM_ENTRIES as usize,
            0xcafe_0002,
        )
        .expect("Table::build");
        let expected = cc_table();

        // ① metadata：format / checksum / 计数
        assert_eq!(built.format(), 4.0);
        assert_eq!(built.path(), Path::new("unused"));
        assert_eq!(built.dict_file_checksum(), expected.dict_file_checksum());
        assert_eq!(built.metadata().unwrap().num_syllables, 5);
        assert_eq!(built.metadata().unwrap().num_entries, CC_NUM_ENTRIES);
        // C++ 的 `reserved_*` 从来没被写过 → 恒 0
        assert_eq!(built.metadata().unwrap().reserved_1, 0);
        assert_eq!(built.metadata().unwrap().reserved_2, 0);

        // ② 布局：metadata@0、音节表紧跟其后、字符串表镜像铺到文件尾（未 save 前是 used）
        let metadata = built.metadata().unwrap();
        assert_eq!(metadata.syllabary, Some(metadata_layout::SIZE));
        assert_eq!(
            metadata.index,
            Some(metadata_layout::SIZE + 4 + 5 * STRING_TYPE_SIZE)
        );
        assert_eq!(
            metadata.string_table.unwrap() + metadata.string_table_size as usize,
            built.file().used(),
            "字符串表应是最后一段、正好铺到 used"
        );
        assert_eq!(
            built.string_table().unwrap().binary_size(),
            metadata.string_table_size as usize
        );

        // ③ 音节表与全部词条（含 trunk / tail）
        assert_eq!(built.syllabary(), expected.syllabary());
        assert_eq!(rows_of(&built), rows_of(&expected));
        // ⚠️ `table_test.cc:28` 的 `total_num_entries = 8`，但 `PrepareSampleVocabulary`
        // 实际插了 **9** 条（C++ 测试自己就少算了一条；`metadata->num_entries` 因此是 8，
        // 而表里确实有 9 条）。这里两个数字都钉住，免得以后误改。
        assert_eq!(rows_of(&built).len(), 9);
        assert_eq!(built.metadata().unwrap().num_entries, CC_NUM_ENTRIES);

        // ④ 查询路径：单音节页、trunk 层、tail 层（extra_code）
        let texts = |mut accessor: TableAccessor<'_>| texts_of(&mut accessor);
        for syllable_id in 0..5 {
            assert_eq!(
                texts(built.query_words(syllable_id)),
                texts(expected.query_words(syllable_id)),
                "QueryWords({syllable_id})"
            );
        }
        for code in [
            vec![1, 2, 3],
            vec![1, 2, 3, 4],
            vec![1, 2, 3, 2, 1],
        ] {
            let code = Code::from(code);
            assert_eq!(
                texts(built.query_phrases(&code)),
                texts(expected.query_phrases(&code)),
                "QueryPhrases({code})"
            );
        }
        // 字符串表必须正好是"音节 ∪ 词条文本"
        let mut expected_keys: BTreeSet<String> =
            built.syllabary().unwrap().iter().cloned().collect();
        for (_, text, _) in rows_of(&built) {
            expected_keys.insert(text);
        }
        assert_eq!(
            built.string_table().unwrap().num_keys(),
            expected_keys.len(),
            "字符串表键数 = 音节 ∪ 文本"
        );
        for key in &expected_keys {
            assert!(built.string_table().unwrap().has_key(key), "{key}");
        }
    }

    /// 落盘 → `open` 再读：`Table::save` 之后文件里的东西必须能被 `Table::open` 完整读回。
    #[test]
    fn build_save_and_open_round_trips_via_disk() {
        let (syllabary, vocabulary) = cc_vocabulary();
        let path = temp_file("table-roundtrip.table.bin");
        let _ = std::fs::remove_file(&path);

        let mut table = Table::build(
            &path,
            &syllabary,
            &vocabulary,
            CC_NUM_ENTRIES as usize,
            0x5eed_0001,
        )
        .expect("Table::build");
        assert!(
            !path.exists(),
            "`build` 只建内存镜像（C++ 的 Build 不落盘），save 才写文件"
        );
        table.save().expect("Table::save");
        assert!(path.is_file());

        let reopened = Table::open(&path).expect("Table::open");
        assert_eq!(reopened.dict_file_checksum(), 0x5eed_0001);
        assert_eq!(reopened.metadata().unwrap().num_entries, CC_NUM_ENTRIES);
        assert_eq!(rows_of(&reopened), rows_of(&table));
        // 文件大小 == used（ShrinkToFit）
        assert_eq!(
            std::fs::metadata(&path).unwrap().len() as usize,
            table.file().used()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn build_with_empty_vocabulary_still_loads() {
        // 只有音节表、没有任何词条的方案（合法：`.table.bin` 里字符串表仍然要有）。
        let syllabary: Syllabary = ["a", "b"].into_iter().collect();
        let vocabulary = Vocabulary::new();
        let table = Table::build("unused", &syllabary, &vocabulary, 0, 0).expect("build");
        assert_eq!(table.metadata().unwrap().num_entries, 0);
        assert_eq!(table.syllabary(), Some(syllabary));
        assert!(table.entries_in_file_order().is_empty());
        assert_eq!(table.string_table().unwrap().num_keys(), 2);
    }

    #[test]
    fn save_on_read_only_table_is_rejected() {
        // ① 已经载入的只读镜像：`MappedFile::save` 会拒绝
        let (syllabary, vocabulary) = cc_vocabulary();
        let path = temp_file("table-readonly.table.bin");
        let _ = std::fs::remove_file(&path);
        Table::compile(
            &path,
            &syllabary,
            &vocabulary,
            CC_NUM_ENTRIES as usize,
            0x5eed_0002,
        )
        .expect("compile");
        let mut reopened = Table::open(&path).expect("open");
        assert!(reopened.save().is_err(), "只读镜像不能 save");
        let _ = std::fs::remove_file(&path);
        // ② 还没 Load 的 Table（C++ 的 `if (!index_)` 分支）
        let mut unloaded = Table::with_file(MappedFile::create("unused", 64).expect("镜像"));
        assert!(unloaded.save().is_err(), "没建好的表不能 save");
        // ③ 空音节表：marisa 空 keyset 建出的是**有效空树**（rsmarisa 实证），
        // C++ `Table::Build` 也没有显式拒绝（一路走到 `trie_.build(keys_)`）。
        let empty_syllabary = Syllabary::new();
        let empty_vocabulary = Vocabulary::new();
        let empty_path = temp_file("table-empty.table.bin");
        let _ = std::fs::remove_file(&empty_path);
        assert!(
            Table::build(&empty_path, &empty_syllabary, &empty_vocabulary, 0, 0).is_ok(),
            "空音节表也能建表"
        );
        let _ = std::fs::remove_file(&empty_path);
    }

    // ------------------------------------------------------------------ 真实词库

    /// 临时目录（**绝不写 `%APPDATA%`**）：`%TEMP%\librime-next-dict-write-<pid>\`。
    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "librime-next-dict-write-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir.join(name)
    }

    /// 找一份真实的 `.dict.yaml`。优先级：
    ///
    /// 1. 环境变量 `RIME_DICT_YAML`（显式指定，测试/验收用）；
    /// 2. `%APPDATA%\Rime-Next\schema\tiger_sentence\tiger_sentence.dict.yaml`
    ///    —— 任务书点名的文件；但这个方案是 Lua 整句输入，**没有词库文件**，通常不存在；
    /// 3. `%APPDATA%\Rime-Next\schema\yuhao\yuling_chaifen.dict.yaml`
    ///    —— 验收④那份 `yuling_chaifen.table.bin` 的源文件（6.1 MB / 12.3 万条）；
    /// 4. 仓库里的同一份（`../Rime-Next/schema/yuhao/yuling_chaifen.dict.yaml`）。
    fn real_dict_yaml() -> Option<PathBuf> {
        let mut candidates = Vec::new();
        if let Some(path) = std::env::var_os("RIME_DICT_YAML") {
            candidates.push(PathBuf::from(path));
        }
        if let Some(appdata) = std::env::var_os("APPDATA") {
            let schema = PathBuf::from(appdata).join("Rime-Next").join("schema");
            candidates.push(
                schema
                    .join("tiger_sentence")
                    .join("tiger_sentence.dict.yaml"),
            );
            candidates.push(schema.join("yuhao").join("yuling_chaifen.dict.yaml"));
        }
        candidates.push(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../Rime-Next/schema/yuhao/yuling_chaifen.dict.yaml"),
        );
        candidates.into_iter().find(|path| path.is_file())
    }

    /// `ChecksumComputer`（`boost::crc_32_type`，初值 0）+ `Checksum()` 的对照实现。
    ///
    /// 与 `zlib.crc32` **不同**：`ChecksumComputer(0)` 把初始余数设成 0 而不是
    /// `0xFFFF_FFFF`，所以末尾才 `^ 0xFFFF_FFFF`。用它就能验证"我们读的是不是
    /// librime 编译时读的那份文件"。
    fn checksum_boost(bytes: &[u8]) -> u32 {
        let mut table = [0u32; 256];
        for (index, slot) in table.iter_mut().enumerate() {
            let mut value = index as u32;
            for _ in 0..8 {
                value = if value & 1 != 0 {
                    (value >> 1) ^ 0xEDB8_8320
                } else {
                    value >> 1
                };
            }
            *slot = value;
        }
        let mut crc = 0u32;
        for &byte in bytes {
            crc = table[((crc ^ u32::from(byte)) & 0xff) as usize] ^ (crc >> 8);
        }
        crc ^ 0xFFFF_FFFF
    }

    /// 一份 `.dict.yaml` 解析出来的词库（只实现验收需要的 `columns` / 排序 / 权重规则）。
    struct ParsedDict {
        syllabary: Syllabary,
        vocabulary: Vocabulary,
        num_entries: usize,
        /// 源文件头里的 `sort: original`（librime 见到它就不 `SortHomophones`）
        sort_original: bool,
        /// `(code,text) → 写入磁盘的权重`：u32 是 f32 的位模式（fp 比较不折腾）
        rows: BTreeMap<(String, String), u32>,
    }

    /// 解析 Rime 词典（`---` 头 + `...` 收尾 + `词\t码[\t权重]`）。
    ///
    /// 规则照 `entry_collector.cc`：`columns` 决定列号；无 `weight` 列时权重 0.0；
    /// **先学音节、再判重**（单音节且 `(text, code_str)` 重复的条目丢弃，但它的音节
    /// 已经在音节表里）；最后按 `dict_compiler.cc:313` 把权重换成 `log(w > 0 ? w : ε)`。
    fn parse_dict_yaml(path: &Path) -> std::io::Result<ParsedDict> {
        let text = std::fs::read_to_string(path)?;
        let mut sort_original = false;
        let mut columns: Vec<String> = Vec::new();
        let mut in_header = false;
        let mut in_columns = false;
        let mut body_start = 0usize;
        for (index, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if !in_header {
                if trimmed == "---" {
                    in_header = true;
                }
                continue;
            }
            if trimmed == "..." || trimmed == "---" {
                body_start = index + 1;
                break;
            }
            if let Some(value) = trimmed.strip_prefix("sort:") {
                sort_original = value.trim() == "original";
                continue;
            }
            if trimmed == "columns:" {
                in_columns = true;
                continue;
            }
            if in_columns {
                match trimmed.strip_prefix("- ") {
                    Some(name) => columns.push(name.trim().to_string()),
                    None => in_columns = false,
                }
            }
        }
        if body_start == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "missing dict header",
            ));
        }
        let column = |name: &str| columns.iter().position(|column| column == name);
        let text_column = column("text").unwrap_or(0);
        let code_column = column("code");
        let weight_column = column("weight");

        let mut syllabary = Syllabary::new();
        let mut rows: Vec<(String, Vec<String>, f64)> = Vec::new();
        let mut seen_single: BTreeSet<(String, String)> = BTreeSet::new();
        let mut num_entries = 0usize;
        let mut vocabulary = Vocabulary::new();
        for line in text.lines().skip(body_start) {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let cells: Vec<&str> = line.split('\t').collect();
            let Some(word) = cells.get(text_column).copied().filter(|w| !w.is_empty()) else {
                continue;
            };
            let code_str = code_column
                .and_then(|index| cells.get(index).copied())
                .unwrap_or("");
            let weight_str = weight_column
                .and_then(|index| cells.get(index).copied())
                .unwrap_or("");
            let syllables: Vec<String> = code_str
                .split_whitespace()
                .map(std::string::ToString::to_string)
                .collect();
            if syllables.is_empty() {
                continue; // C++ 会进 encode_queue（需要 preset vocabulary），这里不支持
            }
            // C++ `CreateEntry`：先 `syllabary.insert`（重复条目也一样），再判重
            for syllable in &syllables {
                syllabary.insert(syllable.clone());
            }
            // 权重：绝对权重直接解析；`%` 缩放需要 preset vocabulary，这里不支持。
            let weight = if weight_str.ends_with('%') {
                0.0
            } else if weight_str.is_empty() {
                0.0
            } else {
                weight_str.parse::<f64>().unwrap_or(0.0)
            };
            if syllables.len() == 1
                && !seen_single.insert((word.to_string(), code_str.to_string()))
            {
                continue; // 重复的单字定义
            }
            rows.push((word.to_string(), syllables, weight));
            num_entries += 1;
        }

        let ids = syllabary.syllable_to_id();
        let mut expected_rows: BTreeMap<(String, String), u32> = BTreeMap::new();
        for (word, syllables, weight) in rows {
            let code = Code::from(
                syllables
                    .iter()
                    .map(|syllable| ids[syllable.as_str()])
                    .collect::<Vec<SyllableId>>(),
            );
            // `dict_compiler.cc:313`：`e->weight = log(r->weight > 0 ? r->weight : DBL_EPSILON)`
            let weight = if weight > 0.0 {
                weight.ln()
            } else {
                f64::EPSILON.ln()
            };
            expected_rows.insert(
                (code.to_string(), word.clone()),
                (weight as f32).to_bits(),
            );
            vocabulary
                .locate_entries(&code)
                .expect("locate_entries")
                .push(ShortDictEntry::new(word, code.clone(), weight));
        }
        if !sort_original {
            vocabulary.sort_homophones();
        }
        Ok(ParsedDict {
            syllabary,
            vocabulary,
            num_entries,
            sort_original,
            rows: expected_rows,
        })
    }

    /// **验收③+④**：拿真实 `.dict.yaml` 重编一次（写到 `%TEMP%`，不碰 `%APPDATA%`），
    /// 再读回来，并与 librime 生成的 `.table.bin` 比语义。
    ///
    /// 没有源文件 / 没有 librime 产物时**静默跳过**（其他机器上 `cargo test` 照样绿）。
    #[test]
    fn real_dict_round_trip_and_librime_comparison() {
        let Some(dict_path) = real_dict_yaml() else {
            eprintln!("跳过真实词库往返：没有可用的 .dict.yaml（见 real_dict_yaml 的候选列表）");
            return;
        };
        let source = std::fs::read(&dict_path).expect("读 dict.yaml");
        let checksum = checksum_boost(&source);
        let parsed = parse_dict_yaml(&dict_path).expect("解析 dict.yaml");
        eprintln!(
            "源 {}：{} 字节，syllables={}，entries={}，sort_original={}，checksum={checksum:#010x}",
            dict_path.display(),
            source.len(),
            parsed.syllabary.size(),
            parsed.num_entries,
            parsed.sort_original,
        );

        // ---- ① 编到临时目录，再读回 -------------------------------------------
        let table_path = temp_file("roundtrip.table.bin");
        let prism_path = temp_file("roundtrip.prism.bin");
        let started = std::time::Instant::now();
        let built = Table::compile(
            &table_path,
            &parsed.syllabary,
            &parsed.vocabulary,
            parsed.num_entries,
            checksum,
        )
        .expect("Table::compile");
        let table_build_ms = started.elapsed().as_millis();
        let reopened = Table::open(&table_path).expect("重新打开 .table.bin");
        let reopened_rows = rows_of(&reopened);
        assert_eq!(
            reopened_rows.len(),
            parsed.num_entries,
            "读回条数必须等于编译条数"
        );
        assert_eq!(
            reopened.metadata().unwrap().num_syllables as usize,
            parsed.syllabary.size()
        );
        assert_eq!(reopened.dict_file_checksum(), checksum);
        assert_eq!(built.file().used(), reopened.file().used());

        // 抽样 10 条（确定性等差数列，不是随机）逐个核对源词库里的 (code,text,weight)
        let step = (reopened_rows.len() / 10).max(1);
        let mut mismatch = 0usize;
        eprintln!("往返抽样 10 条（code（音节串） | text | weight）：");
        for index in (0..reopened_rows.len()).step_by(step).take(10) {
            let (code, text, weight) = &reopened_rows[index];
            let syllables: Vec<String> = code
                .split(',')
                .filter_map(|id| reopened.syllable(id.parse().ok()?))
                .collect();
            let expected = parsed.rows.get(&(code.clone(), text.clone()));
            let same = expected.copied() == Some(weight.to_bits());
            if !same {
                mismatch += 1;
            }
            eprintln!(
                "  [{index:>6}] [{}] | {text} | {weight}  ← {}",
                syllables.join(" "),
                if same { "与源一致" } else { "不一致" }
            );
        }
        assert_eq!(mismatch, 0, "抽样条目的 (code,text,weight) 必须与源词库一致");

        // ---- ② prism 往返：键数 / 值 / max_key_length --------------------------
        let started = std::time::Instant::now();
        let prism = Prism::compile(&prism_path, &parsed.syllabary, None, checksum, 0)
            .expect("Prism::compile");
        let prism_build_ms = started.elapsed().as_millis();
        let prism_reread = Prism::open(&prism_path).expect("重新打开 .prism.bin");
        let metadata = prism_reread.metadata().expect("prism metadata").clone();
        // 键数：文件里写的 num_spellings 必须等于音节数；再把每个音节都查一遍
        // （script=None 时键就是音节表，写进去多少、读回来就得是多少）。
        assert_eq!(metadata.num_spellings as usize, parsed.syllabary.size());
        assert_eq!(metadata.num_syllables as usize, parsed.syllabary.size());
        assert_eq!(prism_reread.max_key_length(), prism.max_key_length());
        assert!(prism_reread.spelling_map_pos().is_none(), "script=None 时不写拼写表");
        let mut looked_up = 0usize;
        for (id, syllable) in parsed.syllabary.iter().enumerate() {
            assert_eq!(
                prism_reread.get_value(syllable),
                Some(id as SyllableId),
                "prism 的 value 必须等于音节 id"
            );
            assert!(prism_reread.has_key(syllable));
            looked_up += 1;
        }
        // 小 trie 才做全量枚举（大 trie 有 370 万节点，枚举只为数一遍键，性价比太低）。
        let enumerated = if prism_reread.array_size() <= 600_000 {
            prism_key_count(&prism_reread)
        } else {
            metadata.num_spellings as usize
        };
        assert_eq!(enumerated, parsed.syllabary.size());
        eprintln!(
            "prism 往返：keys={enumerated}（= 音节数 {}，逐个 lookup 命中 {looked_up}），\
             array_size={}（{} KB），max_key_length={}；table 编译 {table_build_ms} ms，\
             prism 编译 {prism_build_ms} ms",
            metadata.num_spellings,
            metadata.double_array_size,
            prism_reread.array_size() * 4 / 1024,
            metadata.max_key_length,
        );

        // ---- ③ 与 librime 的产物比语义（词条集合 / 权重） ---------------------
        let Some(librime_path) = librime_table_for(&dict_path) else {
            eprintln!("跳过 librime 对比：没有对应的 .table.bin");
            let _ = std::fs::remove_file(&table_path);
            let _ = std::fs::remove_file(&prism_path);
            return;
        };
        let librime = Table::open(&librime_path).expect("打开 librime 的 .table.bin");
        let librime_metadata = librime.metadata().expect("librime metadata").clone();
        eprintln!(
            "librime 产物 {}：num_syllables={} num_entries={} checksum={:#010x}",
            librime_path.display(),
            librime_metadata.num_syllables,
            librime_metadata.num_entries,
            librime_metadata.dict_file_checksum,
        );
        assert_eq!(
            librime_metadata.num_syllables as usize,
            parsed.syllabary.size(),
            "音节数必须一致"
        );
        assert_eq!(
            librime_metadata.num_entries as usize,
            parsed.num_entries,
            "词条数必须一致"
        );
        assert_eq!(
            librime_metadata.dict_file_checksum, checksum,
            "读的必须是 librime 编译时读的那份 dict.yaml"
        );

        let librime_rows = rows_of(&librime);
        assert_eq!(librime_rows.len(), reopened_rows.len());
        assert_eq!(
            librime.syllabary(),
            reopened.syllabary(),
            "音节表（含 id 顺序）必须一致"
        );
        // 多重集：同一个 (code,text) 不能多也不能少；权重逐条相等（f32 位模式）
        let multiset = |rows: &[(String, String, f32)]| {
            let mut counts: BTreeMap<(String, String, u32), usize> = BTreeMap::new();
            for (code, text, weight) in rows {
                *counts
                    .entry((code.clone(), text.clone(), weight.to_bits()))
                    .or_insert(0) += 1;
            }
            counts
        };
        assert_eq!(
            multiset(&reopened_rows),
            multiset(&librime_rows),
            "(code,text,weight) 多重集必须完全相同"
        );
        let order_equal = reopened_rows == librime_rows;
        let differing = reopened_rows
            .iter()
            .zip(librime_rows.iter())
            .filter(|(a, b)| a != b)
            .count();
        eprintln!(
            "与 librime 对比：条数 {} vs {}；多重集相等=true；文件序逐条相等={order_equal}\
             （{differing} 条位置不同）",
            reopened_rows.len(),
            librime_rows.len(),
        );
        // 额外观察（不作为断言）：字节层面是否也一致 —— marisa 镜像由 rsmarisa 重建，
        // 格式兼容但不保证逐字节相同，所以任务只要求"语义"。
        let ours_bytes = std::fs::read(&table_path).ok();
        let their_bytes = std::fs::read(&librime_path).ok();
        if let (Some(ours), Some(theirs)) = (&ours_bytes, &their_bytes) {
            let diff = ours
                .iter()
                .zip(theirs.iter())
                .filter(|(a, b)| a != b)
                .count();
            eprintln!(
                "字节对比（只观察）：ours={} B，librime={} B，逐字节相同={}，不同字节位置 {diff} 个",
                ours.len(),
                theirs.len(),
                ours == theirs,
            );
        }

        for path in [&table_path, &prism_path] {
            let _ = std::fs::remove_file(path);
        }
    }

    /// 真实方案的 librime 产物：`%APPDATA%\Rime-Next\build\<方案>\<方案>.table.bin`。
    ///
    /// 方案名取 `.dict.yaml` 的文件名（`yuling_chaifen.dict.yaml` → `yuling_chaifen`），
    /// 与 `dict_compiler` 的目录约定一致；不在 `%APPDATA%` 时返回 `None`。
    fn librime_table_for(dict_path: &Path) -> Option<PathBuf> {
        let name = dict_path.file_name()?.to_str()?;
        // `yuling_chaifen.dict.yaml` → `yuling_chaifen`（`file_stem` 只会去掉 `.yaml`）
        let stem = name
            .strip_suffix(".dict.yaml")
            .map(str::to_string)
            .or_else(|| dict_path.file_stem()?.to_str().map(str::to_string))?;
        let appdata = std::env::var_os("APPDATA")?;
        let path = PathBuf::from(appdata)
            .join("Rime-Next")
            .join("build")
            .join(&stem)
            .join(format!("{stem}.table.bin"));
        path.is_file().then_some(path)
    }
}
