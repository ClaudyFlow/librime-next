//! `src/rime/dict/entry_collector.{h,cc}`（82 + 261 行）的 Rust 移植：
//! 词典编译的**两遍扫描**收集器。
//!
//! # 它干什么
//!
//! ```text
//! Configure(settings)          选 encoder（ScriptEncoder / TableEncoder）、装预置词库
//! Collect(dict_files)          第一遍：逐文件逐行读词条
//!   ├─ 有 code 列   → CreateEntry 直接入库（顺便学长音节、记单字多码）
//!   └─ 没 code 列   → 进 encode_queue，等第二遍
//! Finish()                     第二遍：queue 逐词 encode_phrase（encoder 回调
//!                              create_entry / translate_word 形成递归造词）；
//!                              再过一遍预置词库里没收集过的词
//! ```
//!
//! 产物给 `dict_compiler`：`syllabary`（音节表，id = 排序位次）、`entries`
//! （`RawDictEntry` 列表）、`num_entries`、`stems`（词根表，喂 reverse db）。
//!
//! # 自引用：encoder 持 collector、collector 持 encoder
//!
//! C++ 里 `TableEncoder(this)` / `ScriptEncoder(this)` 双向指（`encoder.h` 的
//! `PhraseCollector*`）。Rust 不能自引用，解法是把收集器的**数据**放进
//! `Rc<RefCell<CollectorData>>`，encoder 持一个 [`CollectorHandle`]（`Rc` 克隆），
//! [`EntryCollector`] 持 encoder + 同一个 `Rc`。借用法纪：任何
//! `encode_phrase` 调用点都不能持有 `data` 的借用（handle 内部会 `borrow_mut`）。
//!
//! # 与 C++ 的接口差异
//!
//! * `RawCode` 的 `FromString` / `ToString` 在 `algo::encoder` 里是私有方法，
//!   这里经 `Deref<Target = Vec<String>>` + `algo::strings::{split, join}` 等价实现。
//! * C++ 的 `Collect(path)` 是"流式从头读"：`LoadDictHeader` 吃完 YAML 头后流停在
//!   词条表。Rust 侧一次读全文件，由 [`DictSettings::load_dict_header`] 切头。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::algo::encoder::{Encoder, PhraseCollector, RawCode};
use crate::algo::strings::{self, SplitBehavior};
use crate::dict::dict_settings::{DictSettings, parse_double_prefix};
use crate::dict::preset_vocabulary::{PresetVocabulary, resolve_dict_file_path};
use crate::dict::vocabulary::{ReverseLookupTable, Syllabary};

/// `entry_collector.h:18-22` 的 `RawDictEntry`。
#[derive(Debug, Clone)]
pub struct RawDictEntry {
    pub raw_code: RawCode,
    pub text: String,
    pub weight: f64,
}

/// `entry_collector.h:24-31`：word -> [{code, weight}]。
type WordMap = HashMap<String, Vec<(String, f64)>>;

/// `K_MINIMAL_WEIGHT`（`entry_collector.cc:236`）：单字多码的 5% 权重下限。
const K_MINIMAL_WEIGHT: f64 = 0.05;

/// 收集器的全部数据（C++ `EntryCollector` 的成员，除去 encoder）。
#[derive(Default)]
struct CollectorData {
    syllabary: Syllabary,
    build_syllabary: bool,
    entries: Vec<RawDictEntry>,
    num_entries: usize,
    stems: ReverseLookupTable,
    // ---- 编码期状态 ----
    encode_queue: VecDeque<(String, String)>,
    collection: HashSet<String>,
    words: WordMap,
    total_weight: HashMap<String, f64>,
    preset_vocabulary: Option<PresetVocabulary>,
    // ---- 错误消息上下文 ----
    current_dict_file: String,
    line_number: usize,
}

impl CollectorData {
    /// `EntryCollector::CreateEntry`（`entry_collector.cc:161-221`）。
    fn create_entry(&mut self, word: &str, code_str: &str, weight_str: &str) {
        // `e->raw_code.FromString(code_str)`：按空格切、跳空 token
        let mut raw_code = RawCode::default();
        raw_code.extend(strings::split(code_str, " ", SplitBehavior::SkipToken));
        // 权重：百分数按预置词库权重打折；绝对值直接 stod
        let mut weight = 0.0f64;
        let scaled = weight_str.ends_with('%');
        if (weight_str.is_empty() || scaled)
            && let Some(preset) = self.preset_vocabulary.as_ref()
            && let Some(w) = preset.get_weight_for_entry(word)
        {
            weight = w;
        }
        if scaled {
            // C++：stod 失败 → percentage = 100.0
            let percentage =
                parse_double_prefix(&weight_str[..weight_str.len() - 1]).unwrap_or(100.0);
            weight *= percentage / 100.0;
        } else if !weight_str.is_empty() {
            // C++：stod 失败 → 0.0
            weight = parse_double_prefix(weight_str).unwrap_or(0.0);
        }
        // 音节表：build_syllabary = false 时不在表里的音节直接丢掉整个词条
        for syllable in raw_code.iter() {
            if !self.syllabary.contains(syllable) {
                if self.build_syllabary {
                    self.syllabary.insert(syllable.clone());
                } else {
                    log_error(&format!(
                        "dropping entry '{word}' with invalid syllable: {syllable}"
                    ));
                    return;
                }
            }
        }
        // 单字（raw_code 长度 1）：记多码 + 累计权重，重复码定义跳过
        let is_word = raw_code.len() == 1;
        if is_word {
            let weights = self.words.entry(word.to_string()).or_default();
            if weights.iter().any(|(code, _)| code == code_str) {
                log_warn(&format!(
                    "duplicate word definition '{word}': [{code_str}]."
                ));
                return;
            }
            weights.push((code_str.to_string(), weight));
            *self.total_weight.entry(word.to_string()).or_insert(0.0) += weight;
        }
        self.entries.push(RawDictEntry {
            raw_code,
            text: word.to_string(),
            weight,
        });
        self.num_entries += 1;
    }

    /// `EntryCollector::TranslateWord`（`entry_collector.cc:223-245`）。
    fn translate_word(&self, word: &str, result: &mut Vec<String>) -> bool {
        // 1. 词根表优先（`stem` 列收集来的）
        if let Some(stems) = self.stems.get(word) {
            result.extend(stems.iter().cloned());
            return true;
        }
        // 2. 单字多码：按编码排序，滤掉权重不足总量 5% 的码
        if let Some(weights) = self.words.get(word) {
            let mut sorted: Vec<&(String, f64)> = weights.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            let min_weight = self.total_weight.get(word).copied().unwrap_or(0.0) * K_MINIMAL_WEIGHT;
            for (code, weight) in sorted {
                if *weight < min_weight {
                    continue;
                }
                result.push(code.clone());
            }
            return true;
        }
        false
    }
}

/// encoder 的回调句柄（C++ 的 `PhraseCollector* = this`）。
struct CollectorHandle(Rc<RefCell<CollectorData>>);

impl PhraseCollector for CollectorHandle {
    fn create_entry(&mut self, phrase: &str, code_str: &str, value: &str) {
        self.0.borrow_mut().create_entry(phrase, code_str, value);
    }

    fn translate_word(&mut self, word: &str, code: &mut Vec<String>) -> bool {
        self.0.borrow().translate_word(word, code)
    }
}

/// 词典收集器（`EntryCollector`）。
pub struct EntryCollector {
    data: Rc<RefCell<CollectorData>>,
    encoder: Option<Box<dyn Encoder>>,
    /// 预置词库的搜索根（用户目录优先、共享目录回退；`Service::CreateResourceResolver` 同序）。
    data_roots: Vec<PathBuf>,
}

impl Default for EntryCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl EntryCollector {
    /// C++ `EntryCollector()`：自建音节表。
    pub fn new() -> Self {
        Self::with_data(CollectorData {
            build_syllabary: true,
            ..Default::default()
        })
    }

    /// C++ `EntryCollector(Syllabary&&)`：用固定音节表（pack 表的收集器）。
    pub fn with_fixed_syllabary(syllabary: Syllabary) -> Self {
        Self::with_data(CollectorData {
            syllabary,
            build_syllabary: false,
            ..Default::default()
        })
    }

    fn with_data(data: CollectorData) -> Self {
        Self {
            data: Rc::new(RefCell::new(data)),
            encoder: None,
            data_roots: Vec::new(),
        }
    }

    /// 预置词库搜索根（用户目录优先、共享目录回退）。
    pub fn set_data_roots(&mut self, roots: Vec<PathBuf>) {
        self.data_roots = roots;
    }

    /// `EntryCollector::Configure`（`entry_collector.cc:25-36`）。
    pub fn configure(&mut self, settings: &DictSettings) {
        if settings.use_preset_vocabulary() {
            self.load_preset_vocabulary(settings);
        }
        let handle: Box<dyn PhraseCollector> = Box::new(CollectorHandle(self.data.clone()));
        let mut encoder: Box<dyn Encoder> = if settings.use_rule_based_encoder() {
            Box::new(crate::algo::encoder::BoundTableEncoder::new(handle))
        } else {
            Box::new(crate::algo::encoder::ScriptEncoder::new(handle))
        };
        encoder.load_settings(settings.config());
        self.encoder = Some(encoder);
    }

    /// `LoadPresetVocabulary`（`entry_collector.cc:45-55`）。
    fn load_preset_vocabulary(&mut self, settings: &DictSettings) {
        let vocabulary = settings.vocabulary();
        log_info(&format!("loading preset vocabulary: {vocabulary}"));
        let path = resolve_dict_file_path(&self.data_roots, &vocabulary);
        let mut preset = PresetVocabulary::from_file(&path);
        let max_phrase_length = settings.max_phrase_length();
        if max_phrase_length > 0 {
            preset.set_max_phrase_length(max_phrase_length);
        }
        let min_phrase_weight = settings.min_phrase_weight();
        if min_phrase_weight > 0.0 {
            preset.set_min_phrase_weight(min_phrase_weight);
        }
        self.data.borrow_mut().preset_vocabulary = Some(preset);
    }

    /// `EntryCollector::Collect(dict_files)`（`entry_collector.cc:38-43`）：
    /// 逐文件收集后统一 `Finish()`。
    pub fn collect(&mut self, dict_files: &[PathBuf]) {
        for file in dict_files {
            self.collect_file(file);
        }
        self.finish();
    }

    /// `EntryCollector::Collect(dict_file)`（`entry_collector.cc:57-132`）：第一遍。
    pub fn collect_file(&mut self, dict_file: &Path) {
        log_info(&format!("collecting entries from {}", dict_file.display()));
        {
            let mut data = self.data.borrow_mut();
            data.current_dict_file = dict_file.to_string_lossy().into_owned();
            data.line_number = 0;
        }
        let Ok(text) = std::fs::read_to_string(dict_file) else {
            log_error(&format!("missing dict settings; cannot read {dict_file:?}"));
            return;
        };
        let Some((settings, rest)) = DictSettings::load_dict_header(&text) else {
            log_error("missing dict settings.");
            return;
        };
        // 列定义
        let text_column = settings.get_column_index("text");
        let code_column = settings.get_column_index("code");
        let weight_column = settings.get_column_index("weight");
        let stem_column = settings.get_column_index("stem");
        if text_column == -1 {
            log_error(&format!(
                "missing text column definition in file: {}.",
                dict_file.display()
            ));
            return;
        }
        let mut enable_comment = true;
        for line in rest.lines() {
            let line = line.trim_end_matches([' ', '\t', '\x0b', '\x0c', '\r']);
            self.data.borrow_mut().line_number += 1;
            if line.is_empty() {
                continue;
            }
            if enable_comment && line.starts_with('#') {
                if line == "# no comment" {
                    enable_comment = false;
                }
                continue;
            }
            let row = strings::split(line, "\t", SplitBehavior::KeepToken);
            let num_columns = row.len() as i32;
            let text_column = text_column as usize;
            if num_columns <= text_column as i32 || row[text_column].is_empty() {
                let line_number = self.data.borrow().line_number;
                log_warn(&format!(
                    "Missing entry text at #{} of file: {}.",
                    line_number,
                    self.data.borrow().current_dict_file
                ));
                continue;
            }
            let word = row[text_column].clone();
            let mut code_str = String::new();
            let mut weight_str = String::new();
            let mut stem_str = String::new();
            if code_column != -1 && num_columns > code_column {
                let value = &row[code_column as usize];
                if !value.is_empty() {
                    code_str = value.clone();
                }
            }
            if weight_column != -1 && num_columns > weight_column {
                let value = &row[weight_column as usize];
                if !value.is_empty() {
                    weight_str = value.clone();
                }
            }
            if stem_column != -1 && num_columns > stem_column {
                let value = &row[stem_column as usize];
                if !value.is_empty() {
                    stem_str = value.clone();
                }
            }
            self.data.borrow_mut().collection.insert(word.clone());
            if !code_str.is_empty() {
                self.data
                    .borrow_mut()
                    .create_entry(&word, &code_str, &weight_str);
            } else {
                self.data
                    .borrow_mut()
                    .encode_queue
                    .push_back((word.clone(), weight_str));
            }
            if !stem_str.is_empty() && !code_str.is_empty() {
                self.data
                    .borrow_mut()
                    .stems
                    .entry(word)
                    .or_default()
                    .insert(stem_str);
            }
        }
        let num_entries = self.data.borrow().num_entries;
        let num_syllables = self.data.borrow().syllabary.size();
        log_info(&format!(
            "Pass 1: total {num_entries} entries collected; num unique syllables: {num_syllables}"
        ));
    }

    /// `EntryCollector::Finish`（`entry_collector.cc:134-159`）：第二遍 + 预置词库。
    ///
    /// ⚠️ 借用法纪：`encode_phrase` 内部会经 handle `borrow_mut()` 数据，
    /// 所以调用点**不能**持有 `self.data` 的借用（先取布尔值/弹出队列再调）。
    pub fn finish(&mut self) {
        let Some(encoder) = self.encoder.as_mut() else {
            return; // 没 configure 过：没有 encoder 可跑
        };
        loop {
            let next = { self.data.borrow_mut().encode_queue.pop_front() };
            let Some((phrase, weight_str)) = next else {
                break;
            };
            if !encoder.encode_phrase(&phrase, &weight_str) {
                log_error(&format!("Encode failure: '{phrase}'."));
            }
        }
        // 预置词库补录：收集过的词不再进
        let has_preset = self.data.borrow().preset_vocabulary.is_some();
        if has_preset {
            self.data
                .borrow_mut()
                .preset_vocabulary
                .as_mut()
                .unwrap()
                .reset();
            loop {
                let next = {
                    self.data
                        .borrow_mut()
                        .preset_vocabulary
                        .as_mut()
                        .unwrap()
                        .get_next_entry()
                };
                let Some((phrase, weight_str)) = next else {
                    break;
                };
                let already_collected = self.data.borrow().collection.contains(&phrase);
                if already_collected {
                    continue;
                }
                if !encoder.encode_phrase(&phrase, &weight_str) {
                    log_warn(&format!("Encode failure: '{phrase}'."));
                }
            }
        }
        // 释放中间状态（C++ 的三个 swap(empty)）
        let mut data = self.data.borrow_mut();
        data.collection.clear();
        data.words.clear();
        data.total_weight.clear();
    }

    // ---------------------------------------------------------------- 产物出口

    /// 取走音节表（C++ `syllabary = std::move(collector.syllabary)`）。
    pub fn take_syllabary(&mut self) -> Syllabary {
        std::mem::take(&mut self.data.borrow_mut().syllabary)
    }

    /// 取走词条（C++ `vector<of<RawDictEntry>>().swap(collector.entries)`）。
    pub fn take_entries(&mut self) -> Vec<RawDictEntry> {
        std::mem::take(&mut self.data.borrow_mut().entries)
    }

    pub fn num_entries(&self) -> usize {
        self.data.borrow().num_entries
    }

    /// 取走词根表（喂 `ReverseDb::Build`）。
    pub fn take_stems(&mut self) -> ReverseLookupTable {
        std::mem::take(&mut self.data.borrow_mut().stems)
    }
}

fn log_info(message: &str) {
    println!("[entry_collector] {message}");
}

fn log_warn(message: &str) {
    eprintln!("[entry_collector] {message}");
}

fn log_error(message: &str) {
    eprintln!("[entry_collector] {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(name: &str, text: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("librime-entry-collector-{name}.dict.yaml"));
        std::fs::write(&path, text).unwrap();
        path
    }

    /// 单字 + 词组，无 code 列（走 encoder 造词）。
    const PINYIN_DICT: &str = "\
---
name: test_dict
version: \"1.0\"
sort: original
...
你\tni
好\thao
你好\t
";

    #[test]
    fn two_pass_collection_builds_syllabary_and_entries() {
        let path = write_temp("basic", PINYIN_DICT);
        let mut collector = EntryCollector::new();
        let (settings, _) = DictSettings::load_dict_header(PINYIN_DICT).unwrap();
        collector.configure(&settings);
        collector.collect(std::slice::from_ref(&path));
        // 单字直接入库；"你好" 无 code → 第二遍由 ScriptEncoder 拼 "ni hao"
        assert_eq!(collector.num_entries(), 3);
        let syllabary = collector.take_syllabary();
        // C++ `Syllabary = set<string>`：id 按**字典序**，不是插入序 → hao=0, ni=1
        assert_eq!(syllabary.id_of("hao"), Some(0));
        assert_eq!(syllabary.id_of("ni"), Some(1));
        let entries = collector.take_entries();
        let texts: Vec<&str> = entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["你", "好", "你好"]);
        // 造出来的码是 "ni hao"（单音节拼接）
        let composed = entries.last().unwrap();
        assert_eq!(strings::join(composed.raw_code.iter(), " "), "ni hao");
        assert_eq!(composed.weight, 0.0, "无权重列 → 0");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn fixed_syllabary_drops_unknown_syllables() {
        let path = write_temp("fixed", PINYIN_DICT);
        // 固定音节表里只有 ni：含 hao 的词条被丢掉
        let mut fixed = Syllabary::new();
        fixed.insert("ni");
        let mut collector = EntryCollector::with_fixed_syllabary(fixed);
        let (settings, _) = DictSettings::load_dict_header(PINYIN_DICT).unwrap();
        collector.configure(&settings);
        collector.collect(std::slice::from_ref(&path));
        let entries = collector.take_entries();
        let texts: Vec<&str> = entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["你"], "好/你好 的码含表外音节 hao，整条丢弃");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn duplicate_single_char_codes_are_skipped() {
        let text = "\
---
name: test_dict
version: \"1.0\"
...
行\txing
行\thang
行\txing
";
        let path = write_temp("dup", text);
        let mut collector = EntryCollector::new();
        let (settings, _) = DictSettings::load_dict_header(text).unwrap();
        collector.configure(&settings);
        collector.collect(std::slice::from_ref(&path));
        let entries = collector.take_entries();
        let codes: Vec<String> = entries
            .iter()
            .map(|e| strings::join(e.raw_code.iter(), " "))
            .collect();
        assert_eq!(codes, vec!["xing", "hang"], "第三个与第一个重复，跳过");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn stems_are_collected_for_reverse_db() {
        let text = "\
---
name: test_dict
version: \"1.0\"
columns:
  - text
  - code
  - stem
...
你好\tnihao\t你好
";
        let path = write_temp("stems", text);
        let mut collector = EntryCollector::new();
        let (settings, _) = DictSettings::load_dict_header(text).unwrap();
        collector.configure(&settings);
        collector.collect(std::slice::from_ref(&path));
        let stems = collector.take_stems();
        assert_eq!(
            stems
                .get("你好")
                .map(|codes| codes.iter().cloned().collect::<Vec<_>>()),
            Some(vec!["你好".to_string()]),
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn percent_weight_scales_preset_weight() {
        // 预置词库 + 百分数权重：weight * pct / 100
        // 词库按分类规则落 `<root>/eassy/<vocabulary>.txt`（resource.cc 补丁）
        let eassy = std::env::temp_dir().join("eassy");
        std::fs::create_dir_all(&eassy).unwrap();
        let vocab = eassy.join("vocab_test.txt");
        std::fs::write(&vocab, "你好\t1000\n").unwrap();
        let text = "\
---
name: test_dict
version: \"1.0\"
use_preset_vocabulary: true
vocabulary: vocab_test
...
你好\tni\t50%\n";
        let path = write_temp("percent", text);
        let mut collector = EntryCollector::new();
        // 词库文件搜索根指向临时目录
        collector.set_data_roots(vec![std::env::temp_dir()]);
        let (settings, _) = DictSettings::load_dict_header(text).unwrap();
        collector.configure(&settings);
        collector.collect(std::slice::from_ref(&path));
        let entries = collector.take_entries();
        assert_eq!(entries.len(), 1);
        assert!(
            (entries[0].weight - 500.0).abs() < 1e-9,
            "1000 * 50% = 500，实际 {}",
            entries[0].weight
        );
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&vocab).ok();
    }
}
