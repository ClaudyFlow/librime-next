//! `gear/auxiliary_matcher.{h,cc}`（librime-evo，amzxyz）移植：辅助码的**查表与匹配**。
//!
//! # 辅助码是什么
//!
//! 拼音/双拼打出音，再用一两个字母指定字形（部首、笔画、首字母…），
//! 目的是把同音字从几十个压到一两个：
//!
//! ```text
//! 打 "shi" → 是/时/事/使/…（几十个）
//! 打 "shir" → 是（r = 日字旁）        ← r 就是辅助码
//! ```
//!
//! # 两条数据来源
//!
//! | 来源 | 键 | 用途 |
//! |---|---|---|
//! | **`<group>.aux.bin`**（[`AuxiliaryDb`]） | 码 ↔ 单字，双表 + 索引 | **主路径**：`lookup` 的后缀匹配、`lookup_auxiliary` 的反查 |
//! | **反查词典**（`.reverse.bin` 的 `"\x1faux"` 键） | 字 → 空格分隔的码 | **回退**：旧词库没有独立 aux 产物时仍可用 |
//!
//! # 匹配算法（`MatchSequence`）
//!
//! 每个**字**有一组允许的辅助码（`["m", "mq"]`）。用户敲的 aux 串要能被
//! 这些码串**按顺序消费掉**：逐字扫，谁的码里含用户输入的下一个字符就前进一格；
//! 也可以整串跳过某个字（辅助码可能只描述词里的任意一个字：「国家」的 aux
//! 可能只对应「国」）。这条"可跳过"是 evo 的语义，`MatchSequence` 的最后一次
//! 递归就是它 —— 少了这一步，二字词以上的辅助码全都匹配不上。
//!
//! 单字走的是简化的 [`match_prefix`]（任一码串以输入开头即可），与 C++ 一致。
//! 码按"精确优先"排序（`Normal` 在 `Abbreviation` 前、同类型可信度高者先），
//! 所以单字拿到的是最贴近用户输入的那条码。

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::algo::SpellingType;
use crate::gear::dict::auxiliary_db::{AuxiliaryCharacter, AuxiliaryDb};
use crate::gear::dict::dictionary::Dictionary;
use crate::gear::dict::prism::SpellingDescriptor;
use crate::gear::dict::reverse_lookup_dictionary::ReverseDb;
use crate::gear::dict::vocabulary::SyllableId;

/// `AuxGraphMatch`（evo `algo/syllabifier.h:45-51`）：一条带辅助码的拼写匹配。
#[derive(Debug, Clone, PartialEq)]
pub struct AuxGraphMatch {
    /// 匹配到的输入字节数（基音节 + 后置辅助码）
    pub length: usize,
    /// 该拼写对应的**基础音节 id**（辅助码拼写映射到的正常音节）
    pub syllable_id: SyllableId,
    /// 这个音节允许的辅助码字符集
    pub allowed_chars: BTreeSet<char>,
    /// 这条边的拼写类型（基音节与辅助码取更模糊者）
    pub spelling_type: SpellingType,
    /// 基音节与辅助码的可信度之和
    pub credibility: f64,
}

/// `class AuxiliaryMatcher`：辅助码查表 + 匹配。
///
/// C++ 版本持 `Dictionary*` / `AuxiliaryDb*` 两个裸指针。
/// Rust 侧不持引用（生命周期会污染整个翻译器），改成每次调用把词典/字典传进来，
/// 只把**缓存**留在结构里 —— 缓存的都是纯字符串/字符集，与来源生命周期无关。
#[derive(Default)]
pub struct AuxiliaryMatcher {
    /// 音节 id → 该音节能拼出的单字集合（`Dictionary::LookupCharacters`）。
    ///
    /// key 是 prism/音节图用的**全局**音节 id（v6）；单字集合由各表按自己的局部 id 查，
    /// 见 [`Self::characters_for`]。
    characters_by_syllable: HashMap<SyllableId, BTreeSet<char>>,
    /// 字形 → 辅助码序列（反查结果缓存）；`None` = 这个字确实没有数据。
    auxiliary_cache: HashMap<String, Option<Vec<String>>>,
}

impl AuxiliaryMatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Dictionary::LookupCharacters(id, &result)`（`dictionary.cc:631-643`）：
    /// 某个音节 id 能拼出的所有**单字**。
    ///
    /// 主表 + 分包都要遍历（C++ `for (const auto& table : tables_)`）。
    /// v6：`syllable_id` 是 prism 的全局 id，各表用自己的映射换成本表局部 id。
    fn characters_for(
        &mut self,
        dict: &Dictionary,
        syllable_id: SyllableId,
    ) -> BTreeSet<char> {
        if let Some(cached) = self.characters_by_syllable.get(&syllable_id) {
            return cached.clone();
        }
        let mut characters = BTreeSet::new();
        for (table, map) in dict.tables_with_maps() {
            let mut accessor = table.query_words_with_map(syllable_id, map);
            while !accessor.exhausted() {
                if let Some(text) = accessor.entry_text() {
                    let mut chars = text.chars();
                    if let (Some(character), None) = (chars.next(), chars.next()) {
                        characters.insert(character);
                    }
                }
                accessor.next();
            }
        }
        self.characters_by_syllable
            .insert(syllable_id, characters.clone());
        characters
    }

    /// 一条拼写描述符的**基音节**：辅助码拼写的 id 映射回普通音节。
    ///
    /// 简拼/补全/歧义/纠错拼写都不当基音节（`auxiliary_matcher.cc:54-55`）。
    fn base_of(
        &self,
        dict: &Dictionary,
        descriptor: &SpellingDescriptor,
    ) -> Option<(SyllableId, SpellingType, f64)> {
        let base_type = descriptor.spelling_type();
        if base_type >= SpellingType::Abbreviation || descriptor.is_correction() {
            return None;
        }
        // prism 的 syllable id 就是音节图用的 id（v6 是**全局** id，直接返回）；
        // 只校验这个音节确实存在于音节表里。
        if dict.syllable_by_id(descriptor.syllable_id).is_empty() {
            return None;
        }
        Some((
            descriptor.syllable_id,
            base_type,
            descriptor.credibility as f64,
        ))
    }

    /// `AuxiliaryMatcher::Lookup(input, &result)`（`auxiliary_matcher.cc:37-83`）：
    /// **段器 + 后缀匹配**。
    ///
    /// 先用 prism 把输入前缀切成音节，剩下的部分交给 `AuxiliaryDb` 做**码前缀查找**
    /// （后置辅助码），再按"字是否是基音节能拼出的字"筛一遍。`length` 是
    /// `基音节长度 + 辅助码长度`，音节图据此多出一条带 aux 的边。
    pub fn lookup(
        &mut self,
        dict: &Dictionary,
        db: &AuxiliaryDb,
        input: &str,
    ) -> Vec<AuxGraphMatch> {
        if input.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for m in dict.prism.common_prefix_search(input) {
            // 整段都是基音节时没有"后置"部分，这条边与普通音节边重复。
            if m.length >= input.len() {
                continue;
            }
            let Some(rest) = input.get(m.length..) else {
                continue;
            };
            let suffixes = db.common_prefix_lookup(rest);
            if suffixes.is_empty() {
                continue;
            }
            let mut accessor = dict.prism.query_spelling(m.value);
            while !accessor.exhausted() {
                // 优先用完整描述符；没有拼写表的 prism（最小词库 / 旧产物）里
                // accessor 会退化成"拼写 id 即音节 id、属性取默认" —— 此时仍要
                // 产出一条 aux 匹配，与 evo `auxiliary_matcher.cc:51-56` 的
                // `properties()` + `syllable_id()` 路径一致。
                let base = match accessor.descriptor() {
                    Some(descriptor) => self.base_of(dict, &descriptor),
                    None => {
                        let base_id = accessor.syllable_id();
                        let properties = accessor.properties();
                        if properties.r#type >= SpellingType::Abbreviation
                            || dict.syllable_by_id(base_id).is_empty()
                        {
                            None
                        } else {
                            Some((base_id, properties.r#type, properties.credibility))
                        }
                    }
                };
                if let Some((base_id, base_type, base_credibility)) = base {
                    let characters = self.characters_for(dict, base_id);
                    if !characters.is_empty() {
                        for (suffix_length, suffix) in &suffixes {
                            for (spelling_type, credibility, allowed_chars) in
                                group_suffix_matches(&characters, base_type, base_credibility, suffix)
                            {
                                out.push(AuxGraphMatch {
                                    length: m.length + suffix_length,
                                    syllable_id: base_id,
                                    allowed_chars,
                                    spelling_type,
                                    credibility,
                                });
                            }
                        }
                    }
                }
                accessor.next();
            }
        }
        out
    }

    /// `LookupAuxiliary(text, &result)`：字形 → 辅助码序列（带缓存）。
    ///
    /// 优先读独立 aux 字典（`AuxiliaryDb::ReverseLookup`），没有数据才回退旧
    /// `.reverse.bin` 的 `\x1faux` 键。返回 `None` 表示"这个字没有辅助码数据"。
    ///
    /// 多条码按**精确优先**排序：`type` 升序（`Normal` 在 `Abbreviation` 前）、
    /// 同类型可信度高者在前。
    pub fn lookup_auxiliary(
        &mut self,
        db: Option<&AuxiliaryDb>,
        reverse: Option<&ReverseDb>,
        text: &str,
    ) -> Option<Vec<String>> {
        if text.is_empty() {
            return None;
        }
        if let Some(cached) = self.auxiliary_cache.get(text) {
            return cached.clone();
        }
        let mut codes = Vec::new();
        if let Some(db) = db {
            let mut records = db.reverse_lookup(text);
            records.sort_by(|a, b| {
                a.spelling_type
                    .cmp(&b.spelling_type)
                    .then(b.credibility.total_cmp(&a.credibility))
            });
            codes.extend(records.into_iter().map(|record| record.code));
        }
        if codes.is_empty()
            && let Some(reverse) = reverse
            && let Some(encoded) = reverse.lookup_auxiliary(text)
        {
            codes = split_codes(&encoded);
        }
        let result = if codes.is_empty() { None } else { Some(codes) };
        self.auxiliary_cache
            .insert(text.to_string(), result.clone());
        result
    }

    /// `Match(text, input)`：候选文本 `text` 的辅助码能否匹配用户敲的 `input`。
    ///
    /// * 单字：任一码串以 `input` 开头即可（`MatchPrefix`）；
    /// * 多字：走 [`match_sequence`]（可跳过某些字）。
    ///
    /// 任何一个字**没有**辅助码数据都不算失败 —— 没数据 ≠ 不匹配，
    /// 否则上游词库（完全没有 aux 数据）会把所有候选都滤掉。
    pub fn matches(
        &mut self,
        db: Option<&AuxiliaryDb>,
        reverse: Option<&ReverseDb>,
        text: &str,
        input: &str,
    ) -> bool {
        if text.is_empty() || input.is_empty() {
            return false;
        }
        let characters: Vec<char> = text.chars().collect();
        let mut codes: Vec<Vec<String>> = Vec::with_capacity(characters.len());
        for character in &characters {
            let key = character.to_string();
            match self.lookup_auxiliary(db, reverse, &key) {
                Some(list) => codes.push(list),
                None => codes.push(Vec::new()),
            }
        }
        if codes.len() == 1 {
            return match_prefix(&codes[0], input);
        }
        let mut memo = BTreeMap::new();
        match_sequence(&codes, 0, input, 0, &mut memo)
    }
}

/// 把后缀命中的字按 `(类型, 可信度)` 分组（`auxiliary_matcher.cc:64-79`）。
///
/// 分组键带着拼写元数据：同一个字在 `Normal` 与 `Abbreviation` 两条码里都出现时
/// **不会**被合成一组（否则简拼会冒充精确码）。输出按 `(type 升序, credibility 降序)`
/// —— 也就是"单字精确优先"：更贴近用户输入、可信度更高的码先出。
fn group_suffix_matches(
    characters: &BTreeSet<char>,
    base_type: SpellingType,
    base_credibility: f64,
    suffix: &[AuxiliaryCharacter],
) -> Vec<(SpellingType, f64, BTreeSet<char>)> {
    let mut groups: Vec<(SpellingType, f64, BTreeSet<char>)> = Vec::new();
    for item in suffix {
        let mut chars = item.text.chars();
        let (Some(character), None) = (chars.next(), chars.next()) else {
            continue;
        };
        if !characters.contains(&character) {
            continue;
        }
        let spelling_type = base_type.max(item.spelling_type);
        let credibility = base_credibility + item.credibility;
        match groups
            .iter_mut()
            .find(|(t, c, _)| *t == spelling_type && *c == credibility)
        {
            Some((_, _, set)) => {
                set.insert(character);
            }
            None => groups.push((spelling_type, credibility, BTreeSet::from([character]))),
        }
    }
    groups.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.total_cmp(&a.1)));
    groups
}

/// 空格分隔的码串拆开（`"m mq"` → `["m", "mq"]`）。
fn split_codes(text: &str) -> Vec<String> {
    text.split(' ')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

/// `MatchPrefix(codes, input)`：任一码串以 `input` 开头。
fn match_prefix(codes: &[String], input: &str) -> bool {
    codes.iter().any(|code| code.starts_with(input))
}

/// `MatchSequence(codes, index, input, input_index, memo)`：记忆化 DFS。
///
/// 每层尝试用第 `index` 个字的某个码串**按顺序**消费 `input` 的字符
/// （码串里没被输入用到的字符可以跳过 —— 辅助码是子序列，不是前缀），
/// 也可以整字跳过（`index + 1`），因为辅助码可能只描述词里的某一个字。
fn match_sequence(
    codes: &[Vec<String>],
    index: usize,
    input: &str,
    input_index: usize,
    memo: &mut BTreeMap<(usize, usize), bool>,
) -> bool {
    if input_index >= input.len() {
        return true;
    }
    if index >= codes.len() {
        return false;
    }
    if let Some(found) = memo.get(&(index, input_index)) {
        return *found;
    }
    let bytes = input.as_bytes();
    for code in &codes[index] {
        let mut next_input = input_index;
        let mut code_bytes = code.as_bytes().iter();
        while next_input < bytes.len() {
            let Some(byte) = code_bytes.next() else {
                break;
            };
            if bytes[next_input] == *byte {
                next_input += 1;
            }
        }
        if match_sequence(codes, index + 1, input, next_input, memo) {
            memo.insert((index, input_index), true);
            return true;
        }
    }
    // 辅助码可能只对应词里任意一个字 ⇒ 允许整字跳过
    let result = match_sequence(codes, index + 1, input, input_index, memo);
    memo.insert((index, input_index), result);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::K_ABBREVIATION_PENALTY;
    use crate::gear::dict::auxiliary_db::{AuxiliaryCharacter, AuxiliaryCode, AuxiliaryDb};

    fn codes(items: &[&[&str]]) -> Vec<Vec<String>> {
        items
            .iter()
            .map(|group| group.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "librime_next_aux_matcher_{}_{}_{}.aux.bin",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    /// 中：`b`（精确）/ `bs`（简拼缩写）；国：`g`。
    fn sample_db(tag: &str) -> (AuxiliaryDb, std::path::PathBuf) {
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
                        credibility: K_ABBREVIATION_PENALTY,
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
                    credibility: K_ABBREVIATION_PENALTY,
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
        let path = temp_path(tag);
        let db = AuxiliaryDb::compile(&path, &by_character, &by_code, 7).expect("编译 aux 字典");
        (db, path)
    }

    #[test]
    fn split_codes_ignores_empty_fields() {
        assert_eq!(split_codes("m mq"), vec!["m", "mq"]);
        assert_eq!(split_codes("  "), Vec::<String>::new());
        assert_eq!(split_codes("a"), vec!["a"]);
    }

    #[test]
    fn single_character_matches_by_prefix() {
        let table = codes(&[&["m", "mq"]]);
        assert!(match_prefix(&table[0], "m"), "m 是两个码的共同前缀");
        assert!(match_prefix(&table[0], "mq"), "整串也算前缀");
        assert!(!match_prefix(&table[0], "x"));
        assert!(!match_prefix(&table[0], "mqx"), "比码串还长就不匹配");
    }

    /// 二字词：「国家」的辅助码可以是 g / j 两种，输入 `gj` 应命中。
    #[test]
    fn two_character_phrase_consumes_input_in_order() {
        let table = codes(&[&["g"], &["j"]]);
        let mut memo = BTreeMap::new();
        assert!(match_sequence(&table, 0, "gj", 0, &mut memo));
    }

    /// 顺序错就不能匹配：`jg` 不成立（辅助码是**有序**的）。
    #[test]
    fn order_matters() {
        let table = codes(&[&["g"], &["j"]]);
        assert!(!match_sequence(&table, 0, "jg", 0, &mut BTreeMap::new()));
    }

    /// 辅助码只描述词里的第二个字时，第一个字要被跳过。
    #[test]
    fn character_without_codes_can_be_skipped() {
        let table = codes(&[&[], &["j"]]);
        assert!(
            match_sequence(&table, 0, "j", 0, &mut BTreeMap::new()),
            "第一个字没码，应能跳过它去匹配第二个字"
        );
    }

    /// 码串内部的字符可以被跳过（子序列语义）：码 `mq` 也能被输入 `q` 命中。
    #[test]
    fn code_characters_may_be_skipped() {
        let table = codes(&[&["mq"]]);
        assert!(match_sequence(&table, 0, "q", 0, &mut BTreeMap::new()));
        assert!(match_sequence(&table, 0, "mq", 0, &mut BTreeMap::new()));
        assert!(!match_sequence(&table, 0, "qm", 0, &mut BTreeMap::new()));
    }

    /// 输入用完即成功：`g` 对「国家」成立（后面的字不必有码）。
    #[test]
    fn exhausted_input_succeeds() {
        let table = codes(&[&["g"], &["j"]]);
        assert!(match_sequence(&table, 0, "g", 0, &mut BTreeMap::new()));
    }

    /// 没有 aux 数据的字不构成"不匹配"：`matches` 对空来源一律返回 false，
    /// 但调用方要理解成"无法判定"（见 [`AuxiliaryMatcher::matches`] 的文档）。
    #[test]
    fn lookup_auxiliary_without_sources_is_none() {
        let mut matcher = AuxiliaryMatcher::new();
        assert_eq!(matcher.lookup_auxiliary(None, None, "国"), None);
        assert!(!matcher.matches(None, None, "国", "g"));
    }

    /// aux 字典优先于反查表，且码按「精确优先」排序（Normal 在 Abbreviation 前）。
    #[test]
    fn lookup_auxiliary_reads_db_in_exact_first_order() {
        let (db, path) = sample_db("reverse");
        let mut matcher = AuxiliaryMatcher::new();
        assert_eq!(
            matcher.lookup_auxiliary(Some(&db), None, "中"),
            Some(vec!["b".to_string(), "bs".to_string()])
        );
        // 缓存命中后仍然一样
        assert_eq!(
            matcher.lookup_auxiliary(Some(&db), None, "中"),
            Some(vec!["b".to_string(), "bs".to_string()])
        );
        assert_eq!(matcher.lookup_auxiliary(Some(&db), None, "无"), None);
        // 后置辅助码过滤：单字精确/缩写都命中，乱码不命中
        assert!(matcher.matches(Some(&db), None, "中", "b"));
        assert!(matcher.matches(Some(&db), None, "中", "bs"));
        assert!(!matcher.matches(Some(&db), None, "中", "g"));
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// 分组：同一个字在精确码与简码里各出现一次时**不合并**，Normal 组排在前面。
    #[test]
    fn group_suffix_matches_keeps_precise_group_first() {
        let characters = BTreeSet::from(['中', '国']);
        let suffix = vec![
            AuxiliaryCharacter {
                text: "中".to_string(),
                spelling_type: SpellingType::Abbreviation,
                credibility: K_ABBREVIATION_PENALTY,
            },
            AuxiliaryCharacter {
                text: "中".to_string(),
                spelling_type: SpellingType::Normal,
                credibility: 0.0,
            },
            // 不在字集里的字要被丢掉
            AuxiliaryCharacter {
                text: "人".to_string(),
                spelling_type: SpellingType::Normal,
                credibility: 0.0,
            },
        ];
        let groups = group_suffix_matches(&characters, SpellingType::Normal, 0.0, &suffix);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, SpellingType::Normal, "精确组先出");
        assert_eq!(groups[0].1, 0.0);
        assert_eq!(groups[0].2, BTreeSet::from(['中']));
        assert_eq!(groups[1].0, SpellingType::Abbreviation);
        assert!((groups[1].1 - K_ABBREVIATION_PENALTY).abs() < f64::EPSILON);
        // 基音节的类型/可信度参与合成
        let merged = group_suffix_matches(&characters, SpellingType::Fuzzy, -0.5, &suffix);
        assert_eq!(merged[0].0, SpellingType::Fuzzy);
        assert!((merged[0].1 + 0.5).abs() < f64::EPSILON);
    }
}
