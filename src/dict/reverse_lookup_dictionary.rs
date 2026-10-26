//! `src/rime/dict/reverse_lookup_dictionary.{h,cc}`（89 + 261 行）的 Rust 移植：
//! 反查数据库 `.reverse.bin`（编译期 Build/Save，运行期 Load/Lookup）。
//!
//! # 文件格式（`reverse::Metadata`，共 64 字节）
//!
//! ```text
//!  0  char     format[32]                  "Rime::Reverse/3.1"
//! 32  uint32   dict_file_checksum
//! 36  OffsetPtr<char>  dict_settings       规则编码器的 YAML（可空）
//! 40  List<StringId>   index               size@40, at@44（OffsetPtr）
//! 48  OffsetPtr<char>  key_trie            键（词）→ id 的 marisa 镜像
//! 52  uint32   key_trie_size
//! 56  OffsetPtr<char>  value_trie          id → 值（音节串）的 marisa 镜像
//! 60  uint32   value_trie_size
//! ```
//!
//! 注意 `index` 的 `at` 指向的是**裸 StringId 数组**（C++ `Allocate<StringId>(n)`，
//! 没有 `Array` 的 size 头；`List::at` 另外存 size）。这与 prism 的
//! `CreateArray`（带 size 头）不同，别混。
//!
//! # 内容是什么（`reverse_lookup_dictionary.cc:90-100`）
//!
//! 对音节表里每个音节，取词库页里以它为**单音节码**的词条，建
//! `词 → {音节…}` 的反查表；再加上 `stem` 列收集来的词根
//! （键 = `词 + "\x1fstem"`）。键值都进 marisa trie，index 把键 id 映到值 id。
//!
//! # 与 C++ 的接口差异
//!
//! * C++ `ReverseDb : public MappedFile`，Build 在可写镜像上原地建；Rust 侧
//!   持有 `MappedFile`，语义相同。
//! * `ReverseLookupDictionaryComponent`（DbPool、按 Ticket 创建）属于引擎侧组件，
//!   未移植；这里只到 `ReverseDb` + `ReverseLookupDictionary` 的查询部分。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::dict::dict_settings::DictSettings;
use crate::dict::mapped_file::MappedFile;
use crate::dict::string_table::{K_INVALID_STRING_ID, StringTable};
use crate::dict::string_table_builder::StringTableBuilder;
use crate::dict::vocabulary::{ReverseLookupTable, Syllabary, Vocabulary};

/// `kReverseFormat`（`reverse_lookup_dictionary.cc:22`）。
pub const K_REVERSE_FORMAT: &str = "Rime::Reverse/3.1";
/// `kReverseFormatCompatible`：版本差在 [3.0, 4.0) 内算兼容。
pub const K_REVERSE_FORMAT_COMPATIBLE: f64 = 3.0;
/// `kReverseFormatPrefix`。
pub const K_REVERSE_FORMAT_PREFIX: &str = "Rime::Reverse/";
/// `kStemKeySuffix`（`reverse_lookup_dictionary.cc:28`）：词根键的后缀。
pub const K_STEM_KEY_SUFFIX: &str = "\x1fstem";

/// `reverse::Metadata` 的字段偏移（布局见模块文档）。
mod metadata_layout {
    pub const SIZE: usize = 64;
    pub const FORMAT_POS: usize = 0;
    pub const FORMAT_MAX_LENGTH: usize = 32;
    pub const DICT_FILE_CHECKSUM_POS: usize = 32;
    /// `String`：一个 OffsetPtr。
    pub const DICT_SETTINGS_POS: usize = 36;
    /// `List<StringId>`：size + OffsetPtr。
    pub const INDEX_POS: usize = 40;
    pub const KEY_TRIE_POS: usize = 48;
    pub const KEY_TRIE_SIZE_POS: usize = 52;
    pub const VALUE_TRIE_POS: usize = 56;
    pub const VALUE_TRIE_SIZE_POS: usize = 60;
}

/// `kReservedSize`（`reverse_lookup_dictionary.cc:135`）。
const K_RESERVED_SIZE: usize = 1024;

/// `.reverse.bin`（`ReverseDb`）。
#[derive(Debug)]
pub struct ReverseDb {
    file: MappedFile,
    key_trie: Option<StringTable>,
    value_trie: Option<StringTable>,
}

impl ReverseDb {
    /// `ReverseDb(path)` + `Build(...)` + `Save()` 一步到位。
    ///
    /// * `settings` —— 词典设置；`use_rule_based_encoder()` 时把 YAML 存进
    ///   metadata（运行期 `GetDictSettings` 用）。
    /// * `syllabary` / `vocabulary` —— 主表刚建好的音节表和词库（反查表现在
    ///   单音节词条，见模块文档）。
    /// * `stems` —— `stem` 列收集来的词根表。
    #[allow(clippy::too_many_arguments)]
    pub fn compile(
        path: impl Into<PathBuf>,
        settings: Option<&DictSettings>,
        syllabary: &Syllabary,
        vocabulary: &Vocabulary,
        stems: &ReverseLookupTable,
        dict_file_checksum: u32,
    ) -> std::io::Result<Self> {
        let mut db = Self::build(
            path,
            settings,
            syllabary,
            vocabulary,
            stems,
            dict_file_checksum,
        )?;
        db.save()?;
        Ok(db)
    }

    /// `ReverseDb::Build`（`reverse_lookup_dictionary.cc:84-194`）：建镜像不落盘。
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        path: impl Into<PathBuf>,
        settings: Option<&DictSettings>,
        syllabary: &Syllabary,
        vocabulary: &Vocabulary,
        stems: &ReverseLookupTable,
        dict_file_checksum: u32,
    ) -> std::io::Result<Self> {
        // ① 反查表：音节 → 以它为单音节码的词（C++:90-100）
        let mut rev_table: ReverseLookupTable = BTreeMap::new();
        for (syllable_id, syllable) in syllabary.id_to_syllable().iter().enumerate() {
            let Some(page) = vocabulary.pages().get(&(syllable_id as i32)) else {
                continue;
            };
            for entry in &page.entries.0 {
                rev_table
                    .entry(entry.text.clone())
                    .or_default()
                    .insert((*syllable).to_string());
            }
        }
        // ② 键值双 trie（C++:101-124）
        let mut key_builder = StringTableBuilder::new();
        let mut value_builder = StringTableBuilder::new();
        let entry_count = rev_table.len() + stems.len();
        for (text, syllables) in &rev_table {
            key_builder.add(text.clone(), 0.0, None);
            value_builder.add(
                syllables.iter().cloned().collect::<Vec<_>>().join(" "),
                0.0,
                None,
            );
        }
        for (text, stem_set) in stems {
            let mut key = text.clone();
            key.push_str(K_STEM_KEY_SUFFIX);
            key_builder.add(key, 0.0, None);
            value_builder.add(
                stem_set.iter().cloned().collect::<Vec<_>>().join(" "),
                0.0,
                None,
            );
        }
        key_builder
            .build()
            .map_err(|e| std::io::Error::other(format!("key trie: {e}")))?;
        value_builder
            .build()
            .map_err(|e| std::io::Error::other(format!("value trie: {e}")))?;
        let key_ids = key_builder.ids().to_vec();
        let value_ids = value_builder.ids().to_vec();
        // ③ 规则编码器的 dict settings（C++:126-131）
        let dict_settings = match settings {
            Some(s) if s.use_rule_based_encoder() => serde_norway::to_string(s.config())
                .map_err(|e| std::io::Error::other(format!("dict settings yaml: {e}")))?,
            _ => String::new(),
        };
        // ④ 建文件（C++:135-144）
        let estimated = K_RESERVED_SIZE
            + dict_settings.len()
            + entry_count * 4
            + key_builder.binary_size()
            + value_builder.binary_size();
        let mut file = MappedFile::create(path, estimated)?;
        // ⑤ metadata（C++:147-158）
        let metadata_pos = file.allocate(metadata_layout::SIZE, 4);
        debug_assert_eq!(metadata_pos, 0, "metadata 必须落在偏移 0");
        file.put_u32(
            metadata_pos + metadata_layout::DICT_FILE_CHECKSUM_POS,
            dict_file_checksum,
        );
        if !dict_settings.is_empty() {
            file.copy_string(
                &dict_settings,
                metadata_pos + metadata_layout::DICT_SETTINGS_POS,
            );
        }
        // ⑥ index：`List<StringId>` 的头（size + OffsetPtr）在 metadata **内部**的
        // INDEX_POS（C++ `metadata_->index` 是结构体字段），数据数组另分配。
        // ⚠️ 不能把 8 字节头单独 allocate 到别处——读侧按 INDEX_POS 取 size。
        let data = file.allocate(entry_count * 4, 4);
        for i in 0..entry_count {
            let position = data + key_ids[i] as usize * 4;
            file.put_u32(position, value_ids[i]);
        }
        file.put_u32(
            metadata_pos + metadata_layout::INDEX_POS,
            entry_count as u32,
        );
        file.set_offset(metadata_pos + metadata_layout::INDEX_POS + 4, data);
        // ⑦ 键值 trie 镜像（C++:171-188）
        let key_image = file.allocate(key_builder.binary_size(), 1);
        key_builder
            .dump(&mut file, key_image)
            .map_err(|e| std::io::Error::other(format!("key trie dump: {e}")))?;
        file.set_offset(metadata_pos + metadata_layout::KEY_TRIE_POS, key_image);
        file.put_u32(
            metadata_pos + metadata_layout::KEY_TRIE_SIZE_POS,
            key_builder.binary_size() as u32,
        );
        let value_image = file.allocate(value_builder.binary_size(), 1);
        value_builder
            .dump(&mut file, value_image)
            .map_err(|e| std::io::Error::other(format!("value trie dump: {e}")))?;
        file.set_offset(metadata_pos + metadata_layout::VALUE_TRIE_POS, value_image);
        file.put_u32(
            metadata_pos + metadata_layout::VALUE_TRIE_SIZE_POS,
            value_builder.binary_size() as u32,
        );
        // ⑧ format 最后写（C++:191-193）
        let format = K_REVERSE_FORMAT.as_bytes();
        file.put_bytes(metadata_pos + metadata_layout::FORMAT_POS, format);
        file.put_u8(metadata_pos + metadata_layout::FORMAT_POS + format.len(), 0);
        Ok(Self {
            file,
            key_trie: None,
            value_trie: None,
        })
    }

    /// `ReverseDb::Save`：`ShrinkToFit()`。
    pub fn save(&mut self) -> std::io::Result<()> {
        self.file.save()
    }

    /// `ReverseDb(path)` + `Exists()` 的读侧入口。
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let file = MappedFile::open_read_only(path)?;
        Ok(Self {
            file,
            key_trie: None,
            value_trie: None,
        })
    }

    pub fn exists(path: impl AsRef<Path>) -> bool {
        path.as_ref().exists()
    }

    /// `ReverseDb::Load`（`reverse_lookup_dictionary.cc:32-69`）：读 metadata +
    /// 两个 StringTable。格式不兼容返回 `false`。
    pub fn load(&mut self) -> bool {
        let Some(metadata_pos) = self.file.pos(0) else {
            return false;
        };
        let Some(format) = self.file.fixed_str_at(
            metadata_pos + metadata_layout::FORMAT_POS,
            metadata_layout::FORMAT_MAX_LENGTH,
        ) else {
            return false;
        };
        let Some(rest) = format.strip_prefix(K_REVERSE_FORMAT_PREFIX) else {
            return false;
        };
        let Ok(version) = rest.parse::<f64>() else {
            return false;
        };
        // C++：`format - 3.0 < -DBL_EPSILON || format - 3.0 > 1.0 + DBL_EPSILON`
        if version - K_REVERSE_FORMAT_COMPATIBLE < -f64::EPSILON
            || version - K_REVERSE_FORMAT_COMPATIBLE > 1.0 + f64::EPSILON
        {
            return false;
        }
        let key_size = self
            .file
            .u32_at(metadata_pos + metadata_layout::KEY_TRIE_SIZE_POS)
            .unwrap_or(0) as usize;
        let value_size = self
            .file
            .u32_at(metadata_pos + metadata_layout::VALUE_TRIE_SIZE_POS)
            .unwrap_or(0) as usize;
        let Some(key_pos) = self
            .file
            .deref(metadata_pos + metadata_layout::KEY_TRIE_POS)
        else {
            return false;
        };
        let Some(value_pos) = self
            .file
            .deref(metadata_pos + metadata_layout::VALUE_TRIE_POS)
        else {
            return false;
        };
        let Some(key_bytes) = self.file.bytes_at(key_pos, key_size) else {
            return false;
        };
        let Some(value_bytes) = self.file.bytes_at(value_pos, value_size) else {
            return false;
        };
        let Ok(key_trie) = StringTable::from_bytes(key_bytes) else {
            return false;
        };
        let Ok(value_trie) = StringTable::from_bytes(value_bytes) else {
            return false;
        };
        self.key_trie = Some(key_trie);
        self.value_trie = Some(value_trie);
        true
    }

    /// `dict_file_checksum()`：镜像里直接读（没 Load 过也能用）。
    pub fn dict_file_checksum(&self) -> u32 {
        self.file
            .u32_at(metadata_layout::DICT_FILE_CHECKSUM_POS)
            .unwrap_or(0)
    }

    /// `ReverseDb::Lookup`（`reverse_lookup_dictionary.cc:71-82`）：
    /// `key_id = key_trie_->Lookup(text)` → `value_id = index[key_id]` →
    /// `result = value_trie_->GetString(value_id)`。
    ///
    /// ⚠️ index 数组**不能跳过**：key trie 的 id 与 value trie 的 id 是两个
    /// 独立空间，中间的 `List<StringId>` 才是两者的映射表。
    pub fn lookup(&self, text: &str) -> Option<String> {
        let key_trie = self.key_trie.as_ref()?;
        let value_trie = self.value_trie.as_ref()?;
        // C++：`!metadata_->index.size` → false
        let index_size = self.file.u32_at(metadata_layout::INDEX_POS).unwrap_or(0);
        if index_size == 0 {
            return None;
        }
        let key_id = key_trie.lookup_or_invalid(text);
        if key_id == K_INVALID_STRING_ID {
            return None;
        }
        if key_id >= index_size {
            return None;
        }
        let index_data = self.file.deref(metadata_layout::INDEX_POS + 4)?;
        let value_id = self.file.u32_at(index_data + key_id as usize * 4)?;
        let value = value_trie.get_string_or_empty(value_id);
        if value.is_empty() { None } else { Some(value) }
    }

    /// `ReverseLookupDictionary::LookupStems`：查词根（键带 `\x1fstem` 后缀）。
    pub fn lookup_stems(&self, text: &str) -> Option<String> {
        let mut key = text.to_string();
        key.push_str(K_STEM_KEY_SUFFIX);
        self.lookup(&key)
    }

    /// `ReverseDb::dict_settings()`（`reverse_lookup_dictionary.h:60`）：
    /// 建库时**内嵌**的词典 YAML（`dict_settings` 是 `OffsetPtr<char>`，空 = NULL）。
    ///
    /// 为什么内嵌：反查表是给"规则编码器"用的，而规则写在词典头部的
    /// `encoder:` 节点里。运行期的 `UnityTableEncoder` 只拿得到 `.reverse.bin`，
    /// 所以编译期就把那段 YAML 抄进去了（`reverse_lookup_dictionary.cc:126-131`）。
    pub fn dict_settings(&self) -> Option<&str> {
        self.file
            .string_at(metadata_layout::DICT_SETTINGS_POS)
            .filter(|text| !text.is_empty())
    }

    /// `ReverseLookupDictionary::GetDictSettings()`：把内嵌 YAML 解析成
    /// [`DictSettings`]。没内嵌（词典没用规则编码器）时 `None`。
    pub fn load_dict_settings(&self) -> Option<DictSettings> {
        DictSettings::load_dict_header(self.dict_settings()?).map(|(settings, _)| settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::vocabulary::{Code, ShortDictEntry};

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("librime-reverse-db-{name}.reverse.bin"))
    }

    /// 造一个小词库：音节 [ni, hao]，单音节词条 你(ni)、好(hao)。
    fn sample_vocabulary(syllabary: &Syllabary) -> Vocabulary {
        let mut vocabulary = Vocabulary::new();
        let ni = syllabary.id_of("ni").unwrap();
        let hao = syllabary.id_of("hao").unwrap();
        let page_ni = vocabulary.pages_mut().entry(ni).or_default();
        page_ni
            .entries
            .0
            .push(ShortDictEntry::new("你", Code::from_slice(&[ni]), 1.0));
        let page_hao = vocabulary.pages_mut().entry(hao).or_default();
        page_hao
            .entries
            .0
            .push(ShortDictEntry::new("好", Code::from_slice(&[hao]), 1.0));
        vocabulary
    }

    #[test]
    fn build_save_load_roundtrip() {
        let path = temp("roundtrip");
        let _ = std::fs::remove_file(&path);
        let mut syllabary = Syllabary::new();
        syllabary.insert("ni");
        syllabary.insert("hao");
        let vocabulary = sample_vocabulary(&syllabary);
        // 一个词根：你好 → 你好
        let mut stems: ReverseLookupTable = BTreeMap::new();
        stems.insert(
            "你好".to_string(),
            ["你好".to_string()].into_iter().collect(),
        );

        let checksum = 0xDEADBEEF;
        let db = ReverseDb::compile(&path, None, &syllabary, &vocabulary, &stems, checksum)
            .expect("build");
        assert_eq!(db.dict_file_checksum(), checksum);

        // 重新打开 + Load
        let mut reloaded = ReverseDb::open(&path).expect("open");
        assert!(reloaded.load(), "格式应该兼容");
        assert_eq!(reloaded.dict_file_checksum(), checksum);
        // 反查：你 → ni
        assert_eq!(reloaded.lookup("你").as_deref(), Some("ni"));
        assert_eq!(reloaded.lookup("好").as_deref(), Some("hao"));
        assert_eq!(reloaded.lookup("不存在").as_deref(), None);
        // 词根：你好 + \x1fstem → 你好
        assert_eq!(reloaded.lookup_stems("你好").as_deref(), Some("你好"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn metadata_layout_offsets() {
        let path = temp("layout");
        let _ = std::fs::remove_file(&path);
        let mut syllabary = Syllabary::new();
        syllabary.insert("a");
        let vocabulary = Vocabulary::new();
        let stems = BTreeMap::new();
        let db = ReverseDb::build(&path, None, &syllabary, &vocabulary, &stems, 7).expect("build");
        // format 写在 0，checksum 在 32
        assert!(
            db.file
                .fixed_str_at(
                    metadata_layout::FORMAT_POS,
                    metadata_layout::FORMAT_MAX_LENGTH
                )
                .unwrap()
                .starts_with("Rime::Reverse/3.1")
        );
        assert_eq!(db.dict_file_checksum(), 7);
        // index：size = 0（没有反查项也没有词根）
        assert_eq!(
            db.file.u32_at(metadata_layout::INDEX_POS),
            Some(0),
            "空反查表 → index.size = 0"
        );
        std::fs::remove_file(&path).ok();
    }
}
