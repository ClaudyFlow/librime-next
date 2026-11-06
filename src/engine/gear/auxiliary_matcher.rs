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
//! | 来源 | 键 | 谁写 | 用途 |
//! |---|---|---|---|
//! | **prism 拼写属性** | `SpellingDescriptor::type` 的 bit8 + tips 信封 | 编译期（evo `dict_compiler`） | **直接辅助码**：识别 `shir` 这种带 aux 的音节拼写 |
//! | **反查词典** | `"\x1faux" + 字形` | 编译期 | **后置辅助码**：判断候选里的字能不能被用户敲的 aux 串命中 |
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

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::dict::dictionary::Dictionary;
use crate::dict::reverse_lookup_dictionary::ReverseDb;
use crate::dict::vocabulary::SyllableId;
/// `AuxGraphMatch`：一条带辅助码的拼写匹配。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuxGraphMatch {
    /// 匹配到的输入字节数
    pub length: usize,
    /// 该拼写对应的**基础音节 id**（辅助码拼写映射到的正常音节）
    pub syllable_id: SyllableId,
    /// 这个音节允许的辅助码字符集
    pub allowed_chars: BTreeSet<char>,
}

/// `class AuxiliaryMatcher`：辅助码查表 + 匹配。
///
/// C++ 版本持 `Dictionary*` / `ReverseDb*` 两个裸指针。
/// Rust 侧不持引用（生命周期会污染整个翻译器），改成每次调用把词典传进来，
/// 只把**缓存**留在结构里 —— 缓存的都是纯字符串，与词典生命周期无关。
#[derive(Default)]
pub struct AuxiliaryMatcher {
    /// 音节串 → 主词典的音节 id（第一次 `lookup` 时建）。
    main_ids: Option<BTreeMap<String, SyllableId>>,
    /// 辅助码字符集串 → 字符集（同一字符集在几千个音节间复用）。
    permitted_chars: HashMap<String, BTreeSet<char>>,
    /// 字形 → 辅助码序列（反查结果缓存）。
    auxiliary_cache: HashMap<String, Vec<String>>,
}

impl AuxiliaryMatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// 建一次「音节串 → 音节 id」的表（`main_ids_`）。
    ///
    /// 必须按**音节表的下标**编号：`Syllabary` 在 lrn 里是 `BTreeSet`
    /// （按字典序），而 C++ 的 `Syllabary` 是 `vector`，id 就是插入序。
    /// 词库文件里的 id 与字典序并不一致，所以这里用 `Table::syllable(id)`
    /// 逐个反查真实字符串，而不是拿 `BTreeSet` 的顺序当 id。
    fn ensure_main_ids(&mut self, dict: &Dictionary) {
        if self.main_ids.is_some() {
            return;
        }
        let mut map = BTreeMap::new();
        let count = dict.table.syllabary_len();
        for id in 0..count as SyllableId {
            if let Some(text) = dict.table.syllable(id)
                && !text.is_empty()
            {
                map.insert(text, id);
            }
        }
        self.main_ids = Some(map);
    }

    /// 把一条 spelling 描述符翻成 [`AuxGraphMatch`]；不是辅助码拼写就 `None`。
    fn build_match(
        &mut self,
        dict: &Dictionary,
        descriptor: &crate::dict::prism::SpellingDescriptor,
        length: usize,
    ) -> Option<AuxGraphMatch> {
        if !descriptor.is_auxiliary() {
            return None;
        }
        let chars_str = descriptor.auxiliary_characters();
        if chars_str.is_empty() {
            return None;
        }
        // 辅助码拼写经 `syllable_id` 映射回**普通音节**；映射不上就跳过
        // （说明 prism 里的音节表与 .table.bin 不是同一份，继续用会查错词）。
        let base_code = dict.table.get_syllable_by_id(descriptor.syllable_id);
        if base_code.is_empty() {
            return None;
        }
        let base_id = *self.main_ids.as_ref()?.get(&base_code)?;
        let allowed = self
            .permitted_chars
            .entry(chars_str.to_string())
            .or_insert_with(|| chars_str.chars().collect())
            .clone();
        if allowed.is_empty() {
            return None;
        }
        Some(AuxGraphMatch {
            length,
            syllable_id: base_id,
            allowed_chars: allowed,
        })
    }

    /// `AuxiliaryMatcher::Lookup(input, &result)`：找出输入前缀里**带辅助码的拼写**。
    ///
    /// 产出的每一条是「这个前缀能被解释成一个带 aux 的音节」，`length` 是吃掉的字节数。
    /// 音节图据此多出边来 —— 那就是"直接辅助码"能让 `shir` 命中「是」的原因。
    pub fn lookup(&mut self, dict: &Dictionary, input: &str) -> Vec<AuxGraphMatch> {
        if input.is_empty() {
            return Vec::new();
        }
        self.ensure_main_ids(dict);
        let mut out = Vec::new();
        for m in dict.prism.common_prefix_search(input) {
            let mut accessor = dict.prism.query_spelling(m.value);
            while !accessor.exhausted() {
                if let Some(descriptor) = accessor.descriptor()
                    && let Some(hit) = self.build_match(dict, &descriptor, m.length)
                {
                    out.push(hit);
                }
                accessor.next();
            }
        }
        out
    }

    /// `LookupAuxiliary(text, &result)`：字形 → 辅助码序列（带缓存）。
    ///
    /// 返回 `None` 表示"这个字没有辅助码数据"（上游词库就是这样）。
    pub fn lookup_auxiliary(
        &mut self,
        reverse: Option<&ReverseDb>,
        text: &str,
    ) -> Option<Vec<String>> {
        if text.is_empty() {
            return None;
        }
        if let Some(cached) = self.auxiliary_cache.get(text) {
            return Some(cached.clone());
        }
        let reverse = reverse?;
        let encoded = reverse.lookup_auxiliary(text)?;
        let codes = split_codes(&encoded);
        self.auxiliary_cache.insert(text.to_string(), codes.clone());
        Some(codes)
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
            match self.lookup_auxiliary(reverse, &key) {
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

    fn codes(items: &[&[&str]]) -> Vec<Vec<String>> {
        items
            .iter()
            .map(|group| group.iter().map(|s| s.to_string()).collect())
            .collect()
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

    /// 没有 aux 数据的字不构成"不匹配"：`matches` 对空 reverse 一律返回 false，
    /// 但调用方要理解成"无法判定"（见 [`AuxiliaryMatcher::matches`] 的文档）。
    #[test]
    fn lookup_auxiliary_without_reverse_is_none() {
        let mut matcher = AuxiliaryMatcher::new();
        assert_eq!(matcher.lookup_auxiliary(None, "国"), None);
        assert!(!matcher.matches(None, "国", "g"));
    }
}
