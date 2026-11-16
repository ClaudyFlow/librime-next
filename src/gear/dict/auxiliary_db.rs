//! evo `src/rime/dict/auxiliary_db.{h,cc}` 的 Rust 移植 —— 辅助码独立字典。
//!
//! 这是 evo `444ebc1` 之后的形态：辅助码**不再**塞进 prism 的拼写表，而是一个独立
//! 编译单元、独立产物 `<group>.aux.bin`。prism 只负责「切音节 + 给 syllable id」，
//! 本模块只负责「码 ↔ 字」。
//!
//! # 文件里长什么样（`auxiliary_db.cc:75-89`）
//!
//! ```text
//! metadata（76 字节，全部 4 字节对齐）
//!   0  char[32]        format       "Rime::Auxiliary/1.0"
//!  32  u32             checksum
//!  36  u32             character_key_size
//!  40  u32             character_value_size
//!  44  u32             code_key_size
//!  48  u32             code_value_size
//!  52  OffsetPtr<char> character_keys     —— 双数组树镜像（字 → 记录串）
//!  56  OffsetPtr<char> character_values
//!  60  OffsetPtr<char> code_keys          —— 双数组树镜像（码 → 记录串）
//!  64  OffsetPtr<char> code_values
//!  68  OffsetPtr<u32>  character_index    —— 裸数组，按 key id 索引；**没有 size 头**
//!  72  OffsetPtr<u32>  code_index
//! ```
//!
//! 两张表都是「键 trie + 值 trie + id 索引」的三件套，和 `.reverse.bin` 同构。
//!
//! # 记录编码
//!
//! 一个键可以对应多条记录（一个码可能命中多个字；一个字可能有多个码）。每条记录是
//!
//! ```text
//! 文本 \x1f type \x1f credibility
//! ```
//!
//! 多条之间用 `\x1e` 连接，正好占用 C++ `auxiliary_db.h` 里的
//! `kAuxiliaryRecordSep` / `kAuxiliaryFieldSep` 两个常量。
//!
//! # 与 C++ 的接缝
//!
//! * 读 = [`MappedFile::open_read_only`]（Windows 真 mmap）；C++ 是 `mmap` + 裸指针。
//! * `credibility` 用 Rust 的最短往返十进制写法。C++ 用 `setprecision(17)`，两边
//!   **数值**相互可读（`istringstream >> double` 与 `str::parse` 都能吃对方的写法），
//!   但不保证逐字节相同 —— 需要字节级互通的场景只可能是直接交换 `.aux.bin`，届时再说。
//! * `build` 的分配顺序、对齐（trie blob 按 1 字节、索引数组按 4 字节）照抄 C++。
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::algo::SpellingType;
use crate::gear::dict::mapped_file::MappedFile;
use crate::gear::dict::string_table::{StringId, StringTable, StringTableError};
use crate::gear::dict::string_table_builder::StringTableBuilder;

/// `kAuxiliaryFormat`（`auxiliary_db.cc:10`）
pub const K_AUXILIARY_FORMAT: &str = "Rime::Auxiliary/1.0";
/// C++ `sizeof(Metadata)` = 76。
pub const AUXILIARY_METADATA_SIZE: usize = 76;

const FIELD_SIZE: usize = 4;

const META_FORMAT: usize = 0;
const META_CHECKSUM: usize = 32;
const META_CHARACTER_KEY_SIZE: usize = 36;
const META_CHARACTER_VALUE_SIZE: usize = 40;
const META_CODE_KEY_SIZE: usize = 44;
const META_CODE_VALUE_SIZE: usize = 48;
const META_CHARACTER_KEYS: usize = 52;
const META_CHARACTER_VALUES: usize = 56;
const META_CODE_KEYS: usize = 60;
const META_CODE_VALUES: usize = 64;
const META_CHARACTER_INDEX: usize = 68;
const META_CODE_INDEX: usize = 72;

/// 记录内字段分隔符（`kAuxiliaryFieldSep`）
const FIELD_SEP: char = '\x1f';
/// 记录间分隔符（`kAuxiliaryRecordSep`）
const RECORD_SEP: char = '\x1e';

/// 反查（字 → 码）的一条记录，对应 C++ `AuxiliaryDb::AuxiliaryCode`。
#[derive(Debug, Clone, PartialEq)]
pub struct AuxiliaryCode {
    /// 辅助码，如 `"bs"`
    pub code: String,
    /// 拼写类型（`abbrev` 规则产出的是 [`SpellingType::Abbreviation`]）
    pub spelling_type: SpellingType,
    /// 可信度；`abbrev` 会带 `-ln2`
    pub credibility: f64,
}

/// 正查（码 → 字）的一条记录，对应 C++ `AuxiliaryDb::AuxiliaryCharacter`。
#[derive(Debug, Clone, PartialEq)]
pub struct AuxiliaryCharacter {
    /// 单字
    pub text: String,
    /// 拼写类型
    pub spelling_type: SpellingType,
    /// 可信度
    pub credibility: f64,
}

/// 辅助码字典（读 + 写）。对应 C++ `class AuxiliaryDb : public MappedFile`。
pub struct AuxiliaryDb {
    file: MappedFile,
    character_keys: Option<StringTable>,
    character_values: Option<StringTable>,
    code_keys: Option<StringTable>,
    code_values: Option<StringTable>,
}

impl std::fmt::Debug for AuxiliaryDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuxiliaryDb")
            .field("path", &self.file.path())
            .field("loaded", &self.is_loaded())
            .field("checksum", &self.checksum())
            .finish()
    }
}

impl AuxiliaryDb {
    // ---------------------------------------------------------------- 构建

    /// `AuxiliaryDb::Build(path, by_character, by_code, checksum)`。
    ///
    /// 两个 map 都用 [`BTreeMap`]：C++ 是 `std::map`，遍历序即字典序，产物里的
    /// 记录顺序依赖这一点（虽然查询不依赖，但逐字节对比时有用）。
    pub fn build(
        path: impl Into<PathBuf>,
        by_character: &BTreeMap<String, Vec<AuxiliaryCode>>,
        by_code: &BTreeMap<String, Vec<AuxiliaryCharacter>>,
        checksum: u32,
    ) -> io::Result<Self> {
        // 先把 4 棵树建好，才知道要开多大的镜像（`auxiliary_db.cc:115-150`）。
        let mut character_keys_builder = StringTableBuilder::new();
        let mut character_values_builder = StringTableBuilder::new();
        let mut character_values_text: Vec<String> = Vec::with_capacity(by_character.len());
        for (key, records) in by_character {
            let value = encode_codes(records);
            character_keys_builder.add(key.clone(), 0.0, None);
            character_values_builder.add(value.clone(), 0.0, None);
            character_values_text.push(value);
        }

        let mut code_keys_builder = StringTableBuilder::new();
        let mut code_values_builder = StringTableBuilder::new();
        let mut code_values_text: Vec<String> = Vec::with_capacity(by_code.len());
        for (key, records) in by_code {
            let value = encode_characters(records);
            code_keys_builder.add(key.clone(), 0.0, None);
            code_values_builder.add(value.clone(), 0.0, None);
            code_values_text.push(value);
        }

        let mut build_group = |keys: &mut StringTableBuilder,
                               values: &mut StringTableBuilder|
         -> io::Result<()> {
            keys.build()
                .and_then(|_| values.build())
                .map_err(string_table_error_to_io)
        };
        build_group(&mut character_keys_builder, &mut character_values_builder)?;
        build_group(&mut code_keys_builder, &mut code_values_builder)?;

        let capacity = AUXILIARY_METADATA_SIZE
            + (character_values_text.len() + code_values_text.len()) * FIELD_SIZE
            + character_keys_builder.binary_size()
            + character_values_builder.binary_size()
            + code_keys_builder.binary_size()
            + code_values_builder.binary_size()
            + 1024;

        let mut file = MappedFile::create(path, capacity)?;
        let metadata = file.alloc_struct(AUXILIARY_METADATA_SIZE, FIELD_SIZE);
        debug_assert_eq!(metadata, 0, "metadata 必须落在文件开头");

        // format 是 `char[32]`，必须 NUL 结尾（C++ `strncmp` 连 NUL 一起比）。
        file.put_bytes(META_FORMAT, K_AUXILIARY_FORMAT.as_bytes());
        file.put_u8(META_FORMAT + K_AUXILIARY_FORMAT.len(), 0);
        file.put_u32(META_CHECKSUM, checksum);
        file.put_u32(
            META_CHARACTER_KEY_SIZE,
            character_keys_builder.binary_size() as u32,
        );
        file.put_u32(
            META_CHARACTER_VALUE_SIZE,
            character_values_builder.binary_size() as u32,
        );
        file.put_u32(META_CODE_KEY_SIZE, code_keys_builder.binary_size() as u32);
        file.put_u32(
            META_CODE_VALUE_SIZE,
            code_values_builder.binary_size() as u32,
        );

        // trie blob 用 `Allocate<char>`，对齐 1（与 C++ 一致）。
        let position = file.allocate(character_keys_builder.binary_size(), 1);
        character_keys_builder
            .dump(&mut file, position)
            .map_err(string_table_error_to_io)?;
        file.set_offset(META_CHARACTER_KEYS, position);

        let position = file.allocate(character_values_builder.binary_size(), 1);
        character_values_builder
            .dump(&mut file, position)
            .map_err(string_table_error_to_io)?;
        file.set_offset(META_CHARACTER_VALUES, position);

        let position = file.allocate(code_keys_builder.binary_size(), 1);
        code_keys_builder
            .dump(&mut file, position)
            .map_err(string_table_error_to_io)?;
        file.set_offset(META_CODE_KEYS, position);

        let position = file.allocate(code_values_builder.binary_size(), 1);
        code_values_builder
            .dump(&mut file, position)
            .map_err(string_table_error_to_io)?;
        file.set_offset(META_CODE_VALUES, position);

        // 裸索引数组：长度 = 值条数（键值一一对应）。
        let character_index = file.allocate(character_values_text.len() * FIELD_SIZE, FIELD_SIZE);
        file.set_offset(META_CHARACTER_INDEX, character_index);
        let code_index = file.allocate(code_values_text.len() * FIELD_SIZE, FIELD_SIZE);
        file.set_offset(META_CODE_INDEX, code_index);

        let mut db = Self {
            file,
            character_keys: None,
            character_values: None,
            code_keys: None,
            code_values: None,
        };
        db.load()?;

        // 回填索引：`key id` → `value id`（`auxiliary_db.cc:191-205`）。
        //
        // ⚠️ value id **不能**用插入序：id 由建树时的键序决定
        // （见 `string_table.rs` 模块文档），插入序与 id 序不一致。C++ 是按**字符串**
        // 回查值表拿 id（`character_values.Lookup(...)`），这里照做。
        let character_keys = db.character_keys.as_ref().ok_or_else(unloaded_error)?;
        let character_values = db.character_values.as_ref().ok_or_else(unloaded_error)?;
        for (key, records) in by_character {
            let Some(key_id) = character_keys.lookup(key) else {
                continue;
            };
            let Some(value_id) = character_values.lookup(&encode_codes(records)) else {
                continue;
            };
            db.file
                .put_u32(character_index + key_id as usize * FIELD_SIZE, value_id);
        }
        let code_keys = db.code_keys.as_ref().ok_or_else(unloaded_error)?;
        let code_values = db.code_values.as_ref().ok_or_else(unloaded_error)?;
        for (key, records) in by_code {
            let Some(key_id) = code_keys.lookup(key) else {
                continue;
            };
            let Some(value_id) = code_values.lookup(&encode_characters(records)) else {
                continue;
            };
            db.file
                .put_u32(code_index + key_id as usize * FIELD_SIZE, value_id);
        }
        Ok(db)
    }

    /// `compile(path, ...)` = `Build` + `Save`，与 `Prism::compile` / `Table::compile` 同款。
    pub fn compile(
        path: impl Into<PathBuf>,
        by_character: &BTreeMap<String, Vec<AuxiliaryCode>>,
        by_code: &BTreeMap<String, Vec<AuxiliaryCharacter>>,
        checksum: u32,
    ) -> io::Result<Self> {
        let mut db = Self::build(path, by_character, by_code, checksum)?;
        db.save()?;
        Ok(db)
    }

    /// `MappedFile::Save()`
    pub fn save(&mut self) -> io::Result<()> {
        self.file.save()
    }

    // ---------------------------------------------------------------- 打开

    /// `MappedFile::Exists()` + `Load()`：文件存在且 format 认识。
    pub fn exists(path: impl AsRef<Path>) -> bool {
        MappedFile::exists(path)
    }

    /// `AuxiliaryDb::Open(path)`：打不开 / format 不认识都返回错误。
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let file = MappedFile::open_read_only(path)?;
        let mut db = Self {
            file,
            character_keys: None,
            character_values: None,
            code_keys: None,
            code_values: None,
        };
        db.load()?;
        Ok(db)
    }

    /// `AuxiliaryDb::Load()`（`auxiliary_db.cc:52-66`）。
    ///
    /// 与 C++ 一样是幂等的：已经加载过就直接成功。
    pub fn load(&mut self) -> io::Result<()> {
        if self.is_loaded() {
            return Ok(());
        }
        let format = self
            .file
            .fixed_str_at(META_FORMAT, K_AUXILIARY_FORMAT.len() + 1)
            .ok_or_else(invalid_metadata_error)?;
        if format != K_AUXILIARY_FORMAT {
            return Err(invalid_metadata_error());
        }
        self.character_keys = Some(self.table(META_CHARACTER_KEYS, META_CHARACTER_KEY_SIZE)?);
        self.character_values =
            Some(self.table(META_CHARACTER_VALUES, META_CHARACTER_VALUE_SIZE)?);
        self.code_keys = Some(self.table(META_CODE_KEYS, META_CODE_KEY_SIZE)?);
        self.code_values = Some(self.table(META_CODE_VALUES, META_CODE_VALUE_SIZE)?);
        Ok(())
    }

    /// 两张表 + 索引是否都在。
    pub fn is_loaded(&self) -> bool {
        self.character_keys.is_some()
            && self.character_values.is_some()
            && self.code_keys.is_some()
            && self.code_values.is_some()
    }

    fn table(&self, offset_field: usize, size_field: usize) -> io::Result<StringTable> {
        let position = self
            .file
            .deref(offset_field)
            .ok_or_else(invalid_metadata_error)?;
        let size = self
            .file
            .u32_at(size_field)
            .ok_or_else(invalid_metadata_error)? as usize;
        let bytes = self
            .file
            .bytes_at(position, size)
            .ok_or_else(invalid_metadata_error)?;
        StringTable::from_bytes(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
    }

    /// `auxiliary_db.cc:68-73`
    pub fn checksum(&self) -> u32 {
        self.file.u32_at(META_CHECKSUM).unwrap_or(0)
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    // ---------------------------------------------------------------- 查

    /// 码 → 字（`AuxiliaryDb::Lookup`）。未命中返回空 `Vec`。
    pub fn lookup(&self, code: &str) -> Vec<AuxiliaryCharacter> {
        let Some(keys) = &self.code_keys else {
            return Vec::new();
        };
        let Some(values) = &self.code_values else {
            return Vec::new();
        };
        let Some(key_id) = keys.lookup(code) else {
            return Vec::new();
        };
        let Some(value_id) = self.index_value(META_CODE_INDEX, key_id) else {
            return Vec::new();
        };
        values
            .get_string(value_id)
            .map(|text| decode_characters(&text))
            .unwrap_or_default()
    }

    /// 字 → 码（`AuxiliaryDb::ReverseLookup`）。未命中返回空 `Vec`。
    pub fn reverse_lookup(&self, text: &str) -> Vec<AuxiliaryCode> {
        let Some(keys) = &self.character_keys else {
            return Vec::new();
        };
        let Some(values) = &self.character_values else {
            return Vec::new();
        };
        let Some(key_id) = keys.lookup(text) else {
            return Vec::new();
        };
        let Some(value_id) = self.index_value(META_CHARACTER_INDEX, key_id) else {
            return Vec::new();
        };
        values
            .get_string(value_id)
            .map(|value| decode_codes(&value))
            .unwrap_or_default()
    }

    /// `AuxiliaryDb::CommonPrefixLookup`：`input` 的所有前缀里能查到的码。
    ///
    /// 返回 `(码的字节长度, 字列表)`，**长度升序**（`StringTable::common_prefix_match`
    /// 短→长）。C++ 的 `len` 也是 `code.size()` 即字节数。
    pub fn common_prefix_lookup(&self, input: &str) -> Vec<(usize, Vec<AuxiliaryCharacter>)> {
        let Some(keys) = &self.code_keys else {
            return Vec::new();
        };
        let mut result = Vec::new();
        for key_id in keys.common_prefix_match(input) {
            let Some(code) = keys.get_string(key_id) else {
                continue;
            };
            let characters = self.lookup(&code);
            if !characters.is_empty() {
                result.push((code.len(), characters));
            }
        }
        result
    }

    fn index_value(&self, index_field: usize, key_id: StringId) -> Option<StringId> {
        let index = self.file.deref(index_field)?;
        let position = index.checked_add(key_id as usize * FIELD_SIZE)?;
        self.file.u32_at(position)
    }

    /// 索引数组长度（= 值条数），仅供调试 / 测试。
    pub fn num_character_entries(&self) -> usize {
        self.character_values
            .as_ref()
            .map(StringTable::num_keys)
            .unwrap_or(0)
    }

    /// 索引数组长度（= 值条数），仅供调试 / 测试。
    pub fn num_code_entries(&self) -> usize {
        self.code_values
            .as_ref()
            .map(StringTable::num_keys)
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// 记录编解码
// ---------------------------------------------------------------------------

fn encode_codes(records: &[AuxiliaryCode]) -> String {
    records
        .iter()
        .map(|record| {
            format!(
                "{}\x1f{}\x1f{}",
                record.code, record.spelling_type as u8, record.credibility
            )
        })
        .collect::<Vec<_>>()
        .join("\x1e")
}

fn encode_characters(records: &[AuxiliaryCharacter]) -> String {
    records
        .iter()
        .map(|record| {
            format!(
                "{}\x1f{}\x1f{}",
                record.text, record.spelling_type as u8, record.credibility
            )
        })
        .collect::<Vec<_>>()
        .join("\x1e")
}

/// 每条记录必须恰好 3 段（C++ `SplitFields` 后判 `fields.size() != 3`）。
fn split_record(record: &str) -> Option<(&str, SpellingType, f64)> {
    let fields: Vec<&str> = record.split(FIELD_SEP).collect();
    if fields.len() != 3 {
        return None;
    }
    Some((
        fields[0],
        spelling_type_from_u8(fields[1].parse().ok()?),
        fields[2].parse().ok()?,
    ))
}

fn decode_codes(value: &str) -> Vec<AuxiliaryCode> {
    value
        .split(RECORD_SEP)
        .filter_map(|record| {
            let (code, spelling_type, credibility) = split_record(record)?;
            Some(AuxiliaryCode {
                code: code.to_string(),
                spelling_type,
                credibility,
            })
        })
        .collect()
}

fn decode_characters(value: &str) -> Vec<AuxiliaryCharacter> {
    value
        .split(RECORD_SEP)
        .filter_map(|record| {
            let (text, spelling_type, credibility) = split_record(record)?;
            Some(AuxiliaryCharacter {
                text: text.to_string(),
                spelling_type,
                credibility,
            })
        })
        .collect()
}

fn spelling_type_from_u8(value: u8) -> SpellingType {
    match value {
        0 => SpellingType::Normal,
        1 => SpellingType::Fuzzy,
        2 => SpellingType::Abbreviation,
        3 => SpellingType::Completion,
        4 => SpellingType::Ambiguous,
        _ => SpellingType::Invalid,
    }
}

fn string_table_error_to_io(error: StringTableError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn invalid_metadata_error() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid auxiliary metadata")
}

fn unloaded_error() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "auxiliary db is not loaded")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "librime_next_aux_{}_{}_{}.aux.bin",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    fn sample() -> (
        BTreeMap<String, Vec<AuxiliaryCode>>,
        BTreeMap<String, Vec<AuxiliaryCharacter>>,
    ) {
        let by_character = BTreeMap::from([
            (
                "中".to_string(),
                vec![
                    AuxiliaryCode {
                        code: "b".to_string(),
                        spelling_type: SpellingType::Normal,
                        credibility: 0.0,
                    },
                    AuxiliaryCode {
                        code: "bs".to_string(),
                        spelling_type: SpellingType::Abbreviation,
                        credibility: crate::algo::K_ABBREVIATION_PENALTY,
                    },
                ],
            ),
            (
                "国".to_string(),
                vec![AuxiliaryCode {
                    code: "g".to_string(),
                    spelling_type: SpellingType::Normal,
                    credibility: 0.0,
                }],
            ),
        ]);
        let by_code = BTreeMap::from([
            (
                "b".to_string(),
                vec![AuxiliaryCharacter {
                    text: "中".to_string(),
                    spelling_type: SpellingType::Normal,
                    credibility: 0.0,
                }],
            ),
            (
                "bs".to_string(),
                vec![AuxiliaryCharacter {
                    text: "中".to_string(),
                    spelling_type: SpellingType::Abbreviation,
                    credibility: crate::algo::K_ABBREVIATION_PENALTY,
                }],
            ),
            (
                "g".to_string(),
                vec![AuxiliaryCharacter {
                    text: "国".to_string(),
                    spelling_type: SpellingType::Normal,
                    credibility: 0.0,
                }],
            ),
        ]);
        (by_character, by_code)
    }

    #[test]
    fn build_save_load_round_trip() {
        let path = temp_path("round_trip");
        let (by_character, by_code) = sample();
        let db = AuxiliaryDb::compile(&path, &by_character, &by_code, 0x1234_5678).expect("编译");
        assert_eq!(db.checksum(), 0x1234_5678);
        assert_eq!(db.num_character_entries(), 2);
        assert_eq!(db.num_code_entries(), 3);
        drop(db);

        let db = AuxiliaryDb::open(&path).expect("加载");
        assert_eq!(db.checksum(), 0x1234_5678);
        assert_eq!(db.lookup("bs").len(), 1);
        assert_eq!(db.lookup("bs")[0].text, "中");
        assert_eq!(db.lookup("missing"), Vec::new());
        assert_eq!(db.reverse_lookup("中").len(), 2);
        assert_eq!(db.reverse_lookup("中")[1].code, "bs");
        assert_eq!(db.reverse_lookup("国")[0].code, "g");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn common_prefix_lookup_orders_short_first() {
        let path = temp_path("common_prefix");
        let (by_character, by_code) = sample();
        let db = AuxiliaryDb::compile(&path, &by_character, &by_code, 1).expect("编译");
        let matches = db.common_prefix_lookup("bs");
        assert_eq!(
            matches.iter().map(|(len, _)| *len).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(matches[0].1[0].text, "中");
        assert_eq!(matches[1].1[0].text, "中");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_bad_metadata() {
        let path = temp_path("bad_metadata");
        std::fs::write(&path, b"not an auxiliary db").expect("写坏文件");
        let error = AuxiliaryDb::open(&path).expect_err("坏文件必须报错");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        let _ = std::fs::remove_file(&path);
    }
}
