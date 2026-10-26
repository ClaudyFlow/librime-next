//! `dict/vocabulary.h`(113 行) + `dict/vocabulary.cc`(152 行) 的 Rust 移植。
//!
//! 这一层是 `dict/` 的**词条容器**：音节表（[`Syllabary`]）、编码（[`Code`]）、
//! 词条（[`ShortDictEntry`] / [`DictEntry`]）与三级索引（[`Vocabulary`]）。
//! `prism` 只从这里面拿 [`SyllableId`] 与 [`Syllabary`]。
//!
//! # 三个不能想当然的地方
//!
//! 1. **`Syllabary` 的迭代序就是 id 序**。C++ 是 `set<string>`，`Prism::Build` 里
//!    `syllable_to_id[*it] = syll_id++`（`prism.cc:216-220`）—— id 按**字典序**分配，
//!    不是插入序。[`crate::dict::prism`] 读出来的 `syllable_id` 只有配上同一张
//!    排序表才有意义，所以这里用 `BTreeSet` 钉住"唯一 + 有序"。
//! 2. **`Code` 的比较先比长度**，再逐元素（`vocabulary.cc:15-23`）。这和 `Vec` 的
//!    字典序不同：`[2] < [1, 1]` 在 C++ 里成立（长度 1 < 2），Rust 的 `Vec` 会给出相反答案。
//! 3. **词条排序只看 `weight`，而且是降序**；权重相等时 `operator<` 直接 `return 0`
//!    （`vocabulary.cc:60-74`，注释写着 "reduce carbon emission"）。它是个
//!    **strict weak ordering**，不是全序 —— 所以同权词条之间的相对顺序靠
//!    `stable_sort` 保持输入序，这里用 `sort_by`（Rust 的 `sort_by` 也是稳定排序）。
//!
//! # 与 C++ 的接口差异（接缝）
//!
//! * **`of<T>` → 直接持有值**。C++ 的 `ShortDictEntryList` 是
//!   `vector<of<ShortDictEntry>>`（`shared_ptr`），排序时只换指针；这里
//!   [`ShortDictEntryList`] 直接持有 `Vec<ShortDictEntry>`。等 `dictionary` 层要用
//!   "同一条词条被两处引用"时再改成 `Rc<RefCell<..>>`，现在不需要。
//! * **`CreateIndex(Code* out)` → `create_index(&self) -> Code`**：C++ 用输出参数，
//!   这里返回新值（`vocabulary.cc:35-44` 的语义不变）。
//! * **`ToString()` → `Display`**：Rust 的 `to_string()` 来自 `ToString`，输出与
//!   C++ 一致（`"1,2,3"`）。
//! * **没移植**：`DictEntryFilterBinder`（C++ 的 `function` 组合子，等 `dictionary`
//!   层一起做）。`Vocabulary::LocateEntries` 的递归助手是私有函数，不占公共面。
//!
//! 2026-10-26 补上 [`DictEntryFilter`] / [`DictEntryFilterBinder`]：
//! `user_dictionary` 的 `UserDictEntryIterator` 需要它（`vocabulary.h:85-111`）。
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::slice::Iter;

/// `using DictEntryFilter = function<bool(const an<DictEntry>&)>`（`vocabulary.h:85`）。
pub type DictEntryFilter = Box<dyn Fn(&DictEntry) -> bool>;

/// `class DictEntryFilterBinder`（`vocabulary.h:87-111`）的可组合形态。
///
/// C++ 的 binder 让一个迭代器持有一串 filter，`AddFilter` 追加。Rust 侧同样用
/// `Option<Box<dyn Fn>>` 表示"有没有 filter"，追加时用闭包套一层即可：
///
/// ```ignore
/// binder = Some(Box::new(move |e| old(e) && new(e)));
/// ```
pub type DictEntryFilterBinder = Option<DictEntryFilter>;

/// `using SyllableId = int32_t;`（`vocabulary.h:19`）。
///
/// 负数有特殊含义：`Prism` 的拼写 id 用 `-1` 表示"耗尽"，音节表的 id 恒 `>= 0`。
pub type SyllableId = i32;

/// `using Syllabary = set<string>;`（`vocabulary.h:17`）—— 音节串 ↔ id 双向。
///
/// C++ 里它就是个 `set<string>`，id 是**遍历时的下标**；这里把"查 id"和"按 id 取串"
/// 补成方法，但唯一真源仍然是那个有序集合。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Syllabary {
    syllables: BTreeSet<String>,
}

impl Syllabary {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Syllabary::insert`：返回 `true` 表示新插入（C++ `set::insert` 的 `second`）。
    pub fn insert(&mut self, syllable: impl Into<String>) -> bool {
        self.syllables.insert(syllable.into())
    }

    pub fn contains(&self, syllable: &str) -> bool {
        self.syllables.contains(syllable)
    }

    /// `size()`：音节数（= [`SyllableId`] 的上界）。
    pub fn size(&self) -> usize {
        self.syllables.len()
    }

    pub fn is_empty(&self) -> bool {
        self.syllables.is_empty()
    }

    /// C++ `for (auto it = syllabary.begin(); ...)`：**字典序**，也就是 id 序。
    pub fn iter(&self) -> impl Iterator<Item = &String> + '_ {
        self.syllables.iter()
    }

    /// "串 → id"的方向。C++ 在 `Prism::Build` 里现搭一张 `map<string, SyllableId>`
    /// （`prism.cc:216-220`），这里按同一规则现算。
    ///
    /// ⚠️ O(n)（线性扫到它为止）：调用方应像 C++ 那样**先取整张表**
    /// （[`Self::syllable_to_id`]），别在循环里逐条查。
    pub fn id_of(&self, syllable: &str) -> Option<SyllableId> {
        self.syllables
            .iter()
            .position(|candidate| candidate == syllable)
            .map(|index| index as SyllableId)
    }

    /// "id → 串"的方向。越界或负数 → `None`。
    pub fn syllable(&self, id: SyllableId) -> Option<&str> {
        let index = usize::try_from(id).ok()?;
        self.syllables.iter().nth(index).map(String::as_str)
    }

    /// 整张"串 → id"表（C++ `Prism::Build` 里那张局部 map 的对应物）。
    pub fn syllable_to_id(&self) -> BTreeMap<&str, SyllableId> {
        self.syllables
            .iter()
            .enumerate()
            .map(|(i, s)| (s.as_str(), i as SyllableId))
            .collect()
    }

    /// 整张"id → 串"表（下标即 id）。
    pub fn id_to_syllable(&self) -> Vec<&str> {
        self.syllables.iter().map(String::as_str).collect()
    }
}

impl<S: Into<String>> FromIterator<S> for Syllabary {
    fn from_iter<I: IntoIterator<Item = S>>(iter: I) -> Self {
        let mut syllabary = Self::new();
        for syllable in iter {
            syllabary.insert(syllable);
        }
        syllabary
    }
}

/// `class Code : public vector<SyllableId>`（`vocabulary.h:21-35`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Code(pub Vec<SyllableId>);

impl Code {
    /// `static const size_t kIndexCodeMaxLength = 3;`（`vocabulary.h:27`）
    pub const K_INDEX_CODE_MAX_LENGTH: usize = 3;

    pub fn new() -> Self {
        Self::default()
    }

    /// `Code(begin, end)`：从一段音节 id 切片构造。
    pub fn from_slice(syllables: &[SyllableId]) -> Self {
        Self(syllables.to_vec())
    }

    pub fn push(&mut self, syllable_id: SyllableId) {
        self.0.push(syllable_id);
    }

    /// `Code::CreateIndex(Code* index_code)`（`vocabulary.cc:35-44`）：
    /// 取前 3 个音节（不足则全取）作为一级索引键。
    pub fn create_index(&self) -> Code {
        let size = self.0.len().min(Self::K_INDEX_CODE_MAX_LENGTH);
        Code(self.0[..size].to_vec())
    }
}

impl Deref for Code {
    type Target = [SyllableId];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Code {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// `Code::ToString()`（`vocabulary.cc:46-58`）：`"1,2,3"`。
impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, syllable_id) in self.0.iter().enumerate() {
            if i > 0 {
                write!(f, ",")?;
            }
            write!(f, "{syllable_id}")?;
        }
        Ok(())
    }
}

/// `Code::operator<`（`vocabulary.cc:15-23`）：**先比长度**，再逐元素。
impl Ord for Code {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .len()
            .cmp(&other.0.len())
            .then_with(|| self.0.cmp(&other.0))
    }
}

impl PartialOrd for Code {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl From<Vec<SyllableId>> for Code {
    fn from(syllables: Vec<SyllableId>) -> Self {
        Self(syllables)
    }
}

/// `struct ShortDictEntry`（`vocabulary.h:37-46`）：词典页里的词条。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShortDictEntry {
    pub text: String,
    /// 多音节编码（来自 prism）
    pub code: Code,
    pub weight: f64,
}

impl ShortDictEntry {
    pub fn new(text: impl Into<String>, code: Code, weight: f64) -> Self {
        Self {
            text: text.into(),
            code,
            weight,
        }
    }

    /// `ShortDictEntry::operator<`（`vocabulary.cc:60-66`）：**只按 weight 降序**，
    /// 权重相等返回 `Equal`。
    ///
    /// 这**不能**实现成 `Ord`：Rust 的 `Ord` 要求与 `Eq` 一致且是全序，而这里
    /// `weight` 相同的两个不同词条 `cmp == Equal`、`eq == false`，还有 `NaN`。
    /// C++ 的 `operator<` 也只给 `stable_sort` 用，所以这里给一个比较函数。
    pub fn cmp_weight_desc(&self, other: &Self) -> std::cmp::Ordering {
        // C++：`if (weight != other.weight) return weight > other.weight; return 0;`
        // NaN 时 `partial_cmp` 给 None —— 退化成 Equal（C++ 那边是未定义行为）。
        other
            .weight
            .partial_cmp(&self.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

/// `struct DictEntry`（`vocabulary.h:48-70`）：词典页里的完整词条。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DictEntry {
    pub text: String,
    pub comment: String,
    pub preedit: String,
    /// 多音节编码（来自 prism）
    pub code: Code,
    /// 用户自定义编码
    pub custom_code: String,
    pub weight: f64,
    /// 全码匹配长度积分：该词条对应路径里属于「全拼」部分的字符总长度
    pub quality_len: f64,
    pub commit_count: i32,
    pub remaining_code_length: i32,
    pub matching_code_size: i32,
}

impl DictEntry {
    /// `ShortDictEntry ToShort() const`（`vocabulary.h:62`）
    pub fn to_short(&self) -> ShortDictEntry {
        ShortDictEntry::new(self.text.clone(), self.code.clone(), self.weight)
    }

    /// `IsExactMatch()`：没标匹配长度（0）或者整条编码都匹配上了。
    pub fn is_exact_match(&self) -> bool {
        self.matching_code_size == 0 || self.matching_code_size as usize == self.code.len()
    }

    /// `IsPredictiveMatch()`：匹配了一部分（前缀预测）。
    pub fn is_predictive_match(&self) -> bool {
        self.matching_code_size != 0 && (self.matching_code_size as usize) < self.code.len()
    }

    /// `DictEntry::operator<`（`vocabulary.cc:68-74`）：同 [`ShortDictEntry`]，
    /// 只按 weight 降序；理由见 [`ShortDictEntry::cmp_weight_desc`]。
    pub fn cmp_weight_desc(&self, other: &Self) -> std::cmp::Ordering {
        other
            .weight
            .partial_cmp(&self.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

/// `class ShortDictEntryList : public vector<of<ShortDictEntry>>`（`vocabulary.h:72-75`）。
///
/// 见模块文档：这里直接持有值，不套 `shared_ptr`。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShortDictEntryList(pub Vec<ShortDictEntry>);

impl ShortDictEntryList {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Sort()`（`vocabulary.cc:96-98`）：稳定排序，按 weight 降序。
    pub fn sort(&mut self) {
        self.0.sort_by(|a, b| a.cmp_weight_desc(b));
    }

    /// `SortRange(start, count)`（`vocabulary.cc:100-102` + 87-94）：
    /// 只排 `[start, start+count)`；`start` 越界直接返回，`count` 超出就排到尾部。
    pub fn sort_range(&mut self, start: usize, count: usize) {
        if start >= self.0.len() {
            return;
        }
        let end = start.saturating_add(count).min(self.0.len());
        self.0[start..end].sort_by(|a, b| a.cmp_weight_desc(b));
    }
}

impl Deref for ShortDictEntryList {
    type Target = Vec<ShortDictEntry>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ShortDictEntryList {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl FromIterator<ShortDictEntry> for ShortDictEntryList {
    fn from_iter<I: IntoIterator<Item = ShortDictEntry>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// `class DictEntryList : public vector<of<DictEntry>>`（`vocabulary.h:78-81`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DictEntryList(pub Vec<DictEntry>);

impl DictEntryList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sort(&mut self) {
        self.0.sort_by(|a, b| a.cmp_weight_desc(b));
    }

    pub fn sort_range(&mut self, start: usize, count: usize) {
        if start >= self.0.len() {
            return;
        }
        let end = start.saturating_add(count).min(self.0.len());
        self.0[start..end].sort_by(|a, b| a.cmp_weight_desc(b));
    }
}

impl Deref for DictEntryList {
    type Target = Vec<DictEntry>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for DictEntryList {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl FromIterator<DictEntry> for DictEntryList {
    fn from_iter<I: IntoIterator<Item = DictEntry>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// `struct VocabularyPage`（`vocabulary.h:97-100`）
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VocabularyPage {
    pub entries: ShortDictEntryList,
    pub next_level: Option<Box<Vocabulary>>,
}

/// `class Vocabulary : public map<int, VocabularyPage>`（`vocabulary.h:102-106`）。
///
/// 三级索引：第 1/2/3 个音节各一层，**第 4 个音节起固定用键 `-1`**
/// （`vocabulary.cc:123-141` 的 `if (i < Code::kIndexCodeMaxLength) key = code[i];`，
/// 否则 `key` 保持 `-1`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Vocabulary {
    pages: BTreeMap<i32, VocabularyPage>,
}

impl Vocabulary {
    pub fn new() -> Self {
        Self::default()
    }

    /// 直接看底层 `map<int, VocabularyPage>`（C++ 里 Vocabulary 本身就是这张表）。
    pub fn pages(&self) -> &BTreeMap<i32, VocabularyPage> {
        &self.pages
    }

    pub fn pages_mut(&mut self) -> &mut BTreeMap<i32, VocabularyPage> {
        &mut self.pages
    }

    pub fn len(&self) -> usize {
        self.pages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// `Vocabulary::LocateEntries(code)`（`vocabulary.cc:123-141`）：
    /// 沿索引走到该编码对应的那页词条（顺路把缺的页建出来，和 C++ `(*v)[key]` 一样）。
    ///
    /// 空编码返回 `None`（C++ 循环不执行，落到底部 `return NULL`）。
    pub fn locate_entries(&mut self, code: &Code) -> Option<&mut ShortDictEntryList> {
        if code.is_empty() {
            return None;
        }
        Self::locate_at(self, code, 0)
    }

    fn locate_at<'a>(
        vocabulary: &'a mut Vocabulary,
        code: &Code,
        level: usize,
    ) -> Option<&'a mut ShortDictEntryList> {
        let key = if level < Code::K_INDEX_CODE_MAX_LENGTH {
            code[level]
        } else {
            -1
        };
        let page = vocabulary.pages.entry(key).or_default();
        if level + 1 == code.len() || level == Code::K_INDEX_CODE_MAX_LENGTH {
            return Some(&mut page.entries);
        }
        let next_level = page
            .next_level
            .get_or_insert_with(|| Box::new(Vocabulary::new()));
        Self::locate_at(next_level, code, level + 1)
    }

    /// `Vocabulary::SortHomophones()`（`vocabulary.cc:143-150`）：每页按 weight 降序，
    /// 递归到下一层。
    pub fn sort_homophones(&mut self) {
        for page in self.pages.values_mut() {
            page.entries.sort();
            if let Some(next_level) = page.next_level.as_mut() {
                next_level.sort_homophones();
            }
        }
    }
}

impl Deref for Vocabulary {
    type Target = BTreeMap<i32, VocabularyPage>;

    fn deref(&self) -> &Self::Target {
        &self.pages
    }
}

/// `using ReverseLookupTable = hash_map<string, set<string>>;`（`vocabulary.h:109`）
///
/// C++ 用哈希表；这里用 `BTreeMap` / `BTreeSet` 让遍历序稳定（`reverse_lookup` 的
/// 产物要能逐字节对拍）。
pub type ReverseLookupTable = BTreeMap<String, BTreeSet<String>>;

/// C++ `vector<of<ShortDictEntry>>::const_iterator` 的对照物：借用视图。
pub type ShortDictEntryIter<'a> = Iter<'a, ShortDictEntry>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syllabary_assigns_ids_in_sorted_order() {
        // C++ `set<string>` 的遍历序 + `prism.cc:216-220` 的 `syll_id++`
        let syllabary: Syllabary = ["zhe", "a", "zhong", "ni"].into_iter().collect();
        assert_eq!(syllabary.size(), 4);
        assert_eq!(syllabary.id_of("a"), Some(0));
        assert_eq!(syllabary.id_of("ni"), Some(1));
        assert_eq!(syllabary.id_of("zhe"), Some(2));
        assert_eq!(syllabary.id_of("zhong"), Some(3));
        assert_eq!(syllabary.id_of("guo"), None);
        // 双向
        for (i, syllable) in syllabary.id_to_syllable().iter().enumerate() {
            assert_eq!(syllabary.id_of(syllable), Some(i as SyllableId));
            assert_eq!(syllabary.syllable(i as SyllableId), Some(*syllable));
        }
        assert_eq!(syllabary.syllable(-1), None);
        assert_eq!(syllabary.syllable(4), None);
        // 重复插入不改变 id
        let mut syllabary = syllabary;
        assert!(!syllabary.insert("a"));
        assert_eq!(syllabary.size(), 4);
        assert_eq!(syllabary.id_of("a"), Some(0));
        // 映射表与 id_of 一致
        for (syllable, id) in syllabary.syllable_to_id() {
            assert_eq!(syllabary.id_of(syllable), Some(id));
        }
    }

    #[test]
    fn code_orders_by_length_before_elements() {
        // `vocabulary.cc:15-23`：先比 size
        let short = Code::from(vec![2]);
        let long = Code::from(vec![1, 1]);
        assert!(short < long, "[2] < [1,1]：长度优先");
        // 长度相同才逐元素
        assert!(Code::from(vec![1, 2]) < Code::from(vec![2, 1]));
        assert_eq!(Code::from(vec![1, 2]), Code::from(vec![1, 2]));
        // Vec 的字典序会给出相反答案，这条测试就是防止哪天顺手改成 derive(Ord)
        assert!(vec![2] > vec![1, 1]);
    }

    #[test]
    fn code_create_index_caps_at_three_syllables() {
        let code = Code::from(vec![7, 8, 9, 10, 11]);
        assert_eq!(code.create_index(), Code::from(vec![7, 8, 9]));
        assert_eq!(
            Code::from(vec![7, 8]).create_index(),
            Code::from(vec![7, 8])
        );
        assert_eq!(Code::new().create_index(), Code::new());
    }

    #[test]
    fn code_to_string_matches_cpp_tostring() {
        assert_eq!(Code::from(vec![1, 2, 3]).to_string(), "1,2,3");
        assert_eq!(Code::from(vec![-1]).to_string(), "-1");
        assert_eq!(Code::new().to_string(), "");
    }

    fn entry(text: &str, weight: f64) -> ShortDictEntry {
        ShortDictEntry::new(text, Code::from(vec![0]), weight)
    }

    #[test]
    fn entries_sort_by_weight_descending_and_keep_input_order_on_ties() {
        let mut list: ShortDictEntryList = vec![
            entry("a", 1.0),
            entry("b", 3.0),
            entry("c", 3.0),
            entry("d", 2.0),
        ]
        .into_iter()
        .collect();
        list.sort();
        let texts: Vec<&str> = list.iter().map(|e| e.text.as_str()).collect();
        // 降序；同权保持输入序（stable）
        assert_eq!(texts, vec!["b", "c", "d", "a"]);
        // 只排区间
        let mut list: ShortDictEntryList = vec![entry("x", 1.0), entry("y", 3.0), entry("z", 2.0)]
            .into_iter()
            .collect();
        list.sort_range(1, 2);
        let texts: Vec<&str> = list.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["x", "y", "z"]);
        // start 越界是 no-op（`vocabulary.cc:88-89`）
        list.sort_range(9, 2);
        let texts: Vec<&str> = list.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["x", "y", "z"]);
    }

    #[test]
    fn dict_entry_match_kinds() {
        let mut entry = DictEntry {
            code: Code::from(vec![1, 2, 3]),
            ..Default::default()
        };
        // matching_code_size == 0：没标注 = 精确匹配
        assert!(entry.is_exact_match());
        assert!(!entry.is_predictive_match());
        // 部分匹配 = 预测
        entry.matching_code_size = 2;
        assert!(!entry.is_exact_match());
        assert!(entry.is_predictive_match());
        // 全部匹配 = 精确
        entry.matching_code_size = 3;
        assert!(entry.is_exact_match());
        assert!(!entry.is_predictive_match());
        // ToShort 只带 text/code/weight
        entry.text = "你好".into();
        entry.weight = 1.5;
        let short = entry.to_short();
        assert_eq!(short.text, "你好");
        assert_eq!(short.code, entry.code);
        assert_eq!(short.weight, 1.5);
    }

    #[test]
    fn locate_entries_builds_three_level_index_then_fixed_key() {
        let mut vocabulary = Vocabulary::new();
        let codes = [
            vec![1],
            vec![1, 2],
            vec![1, 2, 3],
            vec![1, 2, 3, 4],
            vec![1, 2, 3, 5],
            vec![9, 9],
        ];
        for (i, code) in codes.iter().enumerate() {
            let page = vocabulary
                .locate_entries(&Code::from(code.clone()))
                .expect("非空编码总能定位");
            page.push(entry(&format!("t{i}"), 1.0));
        }
        // 前 3 个音节各占一层；第 4/5 个都落在最后一层的 -1 页
        let last = vocabulary
            .locate_entries(&Code::from(vec![1, 2, 3, 4]))
            .unwrap();
        assert_eq!(last.len(), 2, "第 4 个音节起共用 -1 页");
        assert_eq!(last[0].text, "t3");
        assert_eq!(last[1].text, "t4");
        // 长度 3 的编码落在第 3 层的 3 号页，与 -1 页不同
        let third = vocabulary
            .locate_entries(&Code::from(vec![1, 2, 3]))
            .unwrap();
        assert_eq!(third.len(), 1);
        assert_eq!(third[0].text, "t2");
        // 空编码 → None（C++ 落到 return NULL）
        assert!(vocabulary.locate_entries(&Code::new()).is_none());
        // 顶层键序（BTreeMap）：1 / 9
        let keys: Vec<i32> = vocabulary.pages().keys().copied().collect();
        assert_eq!(keys, vec![1, 9]);
    }

    #[test]
    fn sort_homophones_recurses_into_next_levels() {
        let mut vocabulary = Vocabulary::new();
        vocabulary
            .locate_entries(&Code::from(vec![1]))
            .unwrap()
            .extend([entry("low", 0.1), entry("high", 9.0)]);
        vocabulary
            .locate_entries(&Code::from(vec![1, 2]))
            .unwrap()
            .extend([entry("low2", 0.2), entry("high2", 8.0)]);
        vocabulary.sort_homophones();
        assert_eq!(
            vocabulary.locate_entries(&Code::from(vec![1])).unwrap()[0].text,
            "high"
        );
        assert_eq!(
            vocabulary.locate_entries(&Code::from(vec![1, 2])).unwrap()[0].text,
            "high2"
        );
    }
}
