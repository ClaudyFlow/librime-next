//! `src/rime/dict/preset_vocabulary.{h,cc}`（27 + 116 行）的 Rust 移植：
//! 预置词库（`use_preset_vocabulary: true` 的词典编译时喂进来的那本"大词典"）。
//!
//! # C++ 的存储是 `map<string, string>`，不是 LevelDB
//!
//! `VocabularyDb : public TextDb`，而 `TextDbData = map<string, string>`
//! （`text_db.h:17`）。两个后果，这里都照抄：
//!
//! 1. **迭代是键序**（`TextDbAccessor` 走 `map::const_iterator`），不是文件序；
//! 2. **重复键后写覆盖**（`data_[key] = value`）。
//!
//! 所以 Rust 侧用 `BTreeMap<String, String>` 装数据，迭代时取一份排好序的
//! 快照（`get_next_entry` 要 `&mut self` 且返回owned，BTreeMap 没有便宜的第
//! n 个元素访问）。
//!
//! # 与 C++ 的接口差异
//!
//! * C++ `PresetVocabulary(vocabulary)` 自己按 `{"vocabulary", "", ".txt"}`
//!   规则解析路径（Weasel-Next 补丁后 → `eassy/<id>.txt`）。解析需要知道数据根，
//!   由调用方（`dict_compiler`）传路径进来，本模块只负责读文件。
//! * C++ 的 `GetWeightForEntry` 每次 `std::stod`；这里预解析成 `f64`
//!   （解析是确定函数，值语义相同）。
//! * C++ `IsQualifiedPhrase` 里 `std::stod(weight_str)` 抛异常会**直接崩**；
//!   这里解析失败按"不合格"跳过（坏数据不该让整个编译挂掉）。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::dict::tsv::{Tsv, TsvReader, TsvSink};

/// `preset_vocabulary.cc:29-38` 的 parser：row[0] = key，row[1] 或 "0" = value。
fn vocabulary_entry_parser(row: &Tsv) -> Option<(String, String)> {
    if row.is_empty() || row[0].is_empty() {
        return None;
    }
    let value = if row.len() > 1 { row[1].clone() } else { "0".to_string() };
    Some((row[0].clone(), value))
}

/// C++ `PresetVocabulary::DictFilePath`：ResourceResolver{"vocabulary","",".txt"}
/// → Weasel-Next 补丁后 = `<root>/eassy/<id>.txt`（**平铺**，不建系列夹，
/// `resource.cc:110-111`）。多根按顺序找，都不在就返回第一个候选
/// （C++ 不判存在，读失败按空库处理）。
/// id 含 '/' 时视为显式相对路径，不套 eassy 规则（`resource.cc:78-81`）。
pub fn resolve_dict_file_path(roots: &[PathBuf], vocabulary: &str) -> PathBuf {
    let candidate = |root: &Path| -> PathBuf {
        if vocabulary.contains('/') || vocabulary.contains('\\') {
            root.join(format!("{vocabulary}.txt"))
        } else {
            root.join("eassy").join(format!("{vocabulary}.txt"))
        }
    };
    for root in roots {
        let path = candidate(root);
        if path.exists() {
            return path;
        }
    }
    roots.first().map(|root| candidate(root.as_path())).unwrap_or_default()
}

/// 收集 TSV 行的 Sink（`text_db.cc` 的 `DbSink`：`data_[key] = value`）。
#[derive(Default)]
struct DictSink {
    data: BTreeMap<String, String>,
}

impl TsvSink for DictSink {
    fn put(&mut self, key: &str, value: &str) -> bool {
        self.data.insert(key.to_string(), value.to_string());
        true
    }
}

/// 预置词库（`PresetVocabulary`）。
#[derive(Debug, Default)]
pub struct PresetVocabulary {
    /// 键序数据（C++ 的 `TextDbData`）。
    data: BTreeMap<String, String>,
    /// 预解析的权重（`GetWeightForEntry` 用）。
    weights: HashMap<String, f64>,
    /// 迭代快照 + 游标（`db_->cursor`）。
    sorted: Vec<(String, String)>,
    cursor: usize,
    max_phrase_length: i32,
    min_phrase_weight: f64,
}

impl PresetVocabulary {
    /// 从词库文件构造（C++ `PresetVocabulary(vocabulary)` 去掉路径解析的部分）。
    /// 文件不存在 / 读失败 → 空词库（调用方按 C++ 的
    /// "OpenReadOnly 失败" 路径处理：查不到权重、遍历不到东西）。
    pub fn from_file(path: &Path) -> Self {
        let mut sink = DictSink::default();
        let reader = TsvReader::new(path);
        // C++：reader >> sink 抛异常时返回 false（空库）；这里 IO 错误同样按空库处理。
        let _ = reader.read(&mut sink, vocabulary_entry_parser);
        let mut vocab = Self {
            data: sink.data,
            weights: HashMap::new(),
            sorted: Vec::new(),
            cursor: 0,
            max_phrase_length: 0,
            min_phrase_weight: 0.0,
        };
        for (key, value) in &vocab.data {
            if let Ok(weight) = value.parse::<f64>() {
                vocab.weights.insert(key.clone(), weight);
            }
        }
        vocab.sorted = vocab.data.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        vocab
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// `GetWeightForEntry`：查权重（C++ 每次 stod，这里查预解析表）。
    pub fn get_weight_for_entry(&self, key: &str) -> Option<f64> {
        self.weights.get(key).copied()
    }

    /// `Reset()`：遍历游标归零。
    pub fn reset(&mut self) {
        self.cursor = 0;
    }

    /// `GetNextEntry`：按下一条**合格**的词库记录；遍历结束返回 `None`。
    /// 顺序 = 键序（C++ 的 map 迭代序）。
    pub fn get_next_entry(&mut self) -> Option<(String, String)> {
        while self.cursor < self.sorted.len() {
            let entry = self.sorted[self.cursor].clone();
            self.cursor += 1;
            if self.is_qualified_phrase(&entry.0, &entry.1) {
                return Some(entry);
            }
        }
        None
    }

    /// `IsQualifiedPhrase`（`preset_vocabulary.cc:100-114`）：
    /// 超过 `max_phrase_length` 个字符、或权重低于 `min_phrase_weight` 都不要。
    /// 权重解析失败 → 不合格（C++ 在这里会抛异常崩掉；坏数据让它安静跳过）。
    pub fn is_qualified_phrase(&self, phrase: &str, weight_str: &str) -> bool {
        if self.max_phrase_length > 0 {
            let length = phrase.chars().count() as i32;
            if length > self.max_phrase_length {
                return false;
            }
        }
        if self.min_phrase_weight > 0.0 {
            match weight_str.parse::<f64>() {
                Ok(weight) if weight >= self.min_phrase_weight => {}
                _ => return false,
            }
        }
        true
    }

    pub fn set_max_phrase_length(&mut self, length: i32) {
        self.max_phrase_length = length;
    }

    pub fn set_min_phrase_weight(&mut self, weight: f64) {
        self.min_phrase_weight = weight;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(name: &str, text: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("librime-preset-vocab-{name}.txt"));
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn loads_sorted_and_last_write_wins() {
        let path = write_temp(
            "sorted",
            "# comment\n\
             你好\t100\n\
             世界\t200\n\
             阿\t50\n\
             你好\t999\n", // 重复键：后写覆盖（C++ data_[key] = value）
        );
        let vocab = PresetVocabulary::from_file(&path);
        // map 迭代是键序（按字节）：世 E4B896 < 你 E4BDA0 < 阿 E998BF
        assert_eq!(
            vocab.sorted.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            vec!["世界", "你好", "阿"]
        );
        // 后写覆盖
        assert_eq!(vocab.get_weight_for_entry("你好"), Some(999.0));
        assert_eq!(vocab.get_weight_for_entry("世界"), Some(200.0));
        assert_eq!(vocab.get_weight_for_entry("不存在"), None);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn get_next_entry_iterates_in_key_order() {
        // 插入序是 c / a / " b"，迭代必须换成字节序（" b" 的 0x20 < 'a' < 'c'）
        let path = write_temp("iter", "c\t3\na\t2\n b\t1\n");
        let mut vocab = PresetVocabulary::from_file(&path);
        vocab.reset();
        let mut got = Vec::new();
        while let Some((key, value)) = vocab.get_next_entry() {
            got.push((key, value));
        }
        assert_eq!(
            got,
            vec![
                (" b".to_string(), "1".to_string()),
                ("a".to_string(), "2".to_string()),
                ("c".to_string(), "3".to_string()),
            ]
        );
        // 再取一次 → None（游标在尾部）
        assert!(vocab.get_next_entry().is_none());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn phrase_filters_follow_cpp() {
        let path = write_temp("filter", "短\t1\n适当长度的词\t100\n超长超长超长超长的一个词\t100\n");
        let mut vocab = PresetVocabulary::from_file(&path);
        // 按字符数（UTF-8 码点）截断
        vocab.set_max_phrase_length(6);
        // 权重下限
        vocab.set_min_phrase_weight(50.0);
        vocab.reset();
        let mut got = Vec::new();
        while let Some((key, _)) = vocab.get_next_entry() {
            got.push(key);
        }
        assert_eq!(got, vec!["适当长度的词"], "短的不够权重，超长的超字数");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn unparseable_weight_is_not_qualified() {
        let path = write_temp("badweight", "坏权重\tabc\n好\t100\n");
        let mut vocab = PresetVocabulary::from_file(&path);
        vocab.set_min_phrase_weight(1.0);
        vocab.reset();
        let got: Vec<String> = std::iter::from_fn(|| vocab.get_next_entry())
            .map(|(k, _)| k)
            .collect();
        assert_eq!(got, vec!["好"], "权重列坏掉的安静跳过（C++ 在这里会抛异常）");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn missing_file_yields_empty_vocabulary() {
        let vocab = PresetVocabulary::from_file(Path::new("definitely-not-here.txt"));
        assert!(vocab.is_empty());
        assert_eq!(vocab.get_weight_for_entry("x"), None);
    }
}
