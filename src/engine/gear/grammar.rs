//! `plugins/octagram/src/gram_encoding.cc` + `octagram.cc`：n-gram 编码与打分。
//!
//! # 键编码（`gram_encoding.cc:9-46`）
//!
//! `.gram` 的 trie 键是**紧凑编码**，不是 UTF-8 —— 目的是让常用汉字只占 2 字节：
//!
//! | 码位 | 编码 | 长度 |
//! |---|---|---|
//! | `U+0000` | `0xE0` | 1 |
//! | ASCII `0x01..0x7F` | 原样 | 1 |
//! | `0x4000..0xA000`（CJK 主体） | `(u>>8)+0x40, u&0xFF` | 2 |
//! | 其余 | `0xE0\|n, n×7bit` | 2..6 |
//!
//! `0xE0` 分两类：单独出现表示 `U+0000`；带 `0xE1..0xE8` 则是变长头
//! （低 4 位 = 后续字节数）。解码走 [`advance`]。
//!
//! # 打分（`octagram.cc:107-162`）
//!
//! 对 context 的**每个后缀**（最多 `collocation_max_length - 1 = 3` 个字符起）问
//! `gram_db`：`context 的后缀 + word 的前缀`这条 n-gram 在模型里有多大概率。
//! 取所有命中里的最高分，再按"搭配长度够不够"扣罚：
//!
//! * 搭配长度 ≥ `collocation_min_length`(3) 或整词命中 → `collocation_penalty`(-12)
//! * 否则 → `weak_collocation_penalty`(-24)
//! * 完全没有命中 → `non_collocation_penalty`(-12)
//! * 整句末尾（`is_rear`）再查一次 `word + "$"`，命中则给 `rear_penalty`(-18)

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::dict::gram_db::{GramDb, K_VALUE_SCALE};
use crate::engine::gear::poet::Grammar;

/// `grammar::kMaxEncodedUnicode`（`gram_encoding.h:12`）：一次最多看 8 个字符。
pub const K_MAX_ENCODED_UNICODE: usize = 8;

/// `kGrammarDefaultLanguage`（`octagram.cc:22`）。
pub const K_GRAMMAR_DEFAULT_LANGUAGE: &str = "zh-hant";

/// `struct GrammarConfig`（`octagram.cc:12-19`）：默认值与 C++ 逐字一致。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GrammarConfig {
    pub collocation_max_length: i32,
    pub collocation_min_length: i32,
    pub collocation_penalty: f64,
    pub non_collocation_penalty: f64,
    pub weak_collocation_penalty: f64,
    pub rear_penalty: f64,
}

impl Default for GrammarConfig {
    fn default() -> Self {
        Self {
            collocation_max_length: 4,
            collocation_min_length: 3,
            collocation_penalty: -12.0,
            non_collocation_penalty: -12.0,
            weak_collocation_penalty: -24.0,
            rear_penalty: -18.0,
        }
    }
}

impl GrammarConfig {
    /// `Octagram::Octagram(Config*, ...)`（`octagram.cc:27-45`）：读 `grammar/` 节点。
    ///
    /// 返回 `None` = 配置里没有 `grammar/language`，即这个方案不要语法模型
    /// （C++ 里就是 `return`，`db_` 留空）。
    pub fn from_config(config: &Value) -> Option<(Self, String)> {
        let node = config.get("grammar")?;
        let language = node.get("language")?.as_str()?.to_string();
        let mut out = Self::default();
        if let Some(v) = node.get("collocation_max_length").and_then(Value::as_i64) {
            out.collocation_max_length = v as i32;
        }
        if let Some(v) = node.get("collocation_min_length").and_then(Value::as_i64) {
            out.collocation_min_length = v as i32;
        }
        if let Some(v) = node.get("collocation_penalty").and_then(Value::as_f64) {
            out.collocation_penalty = v;
        }
        if let Some(v) = node.get("non_collocation_penalty").and_then(Value::as_f64) {
            out.non_collocation_penalty = v;
        }
        if let Some(v) = node.get("weak_collocation_penalty").and_then(Value::as_f64) {
            out.weak_collocation_penalty = v;
        }
        if let Some(v) = node.get("rear_penalty").and_then(Value::as_f64) {
            out.rear_penalty = v;
        }
        Some((out, language))
    }
}

/// `grammar::encode`（`gram_encoding.cc:9-46`）：UTF-8 串 → trie 键。
pub fn encode(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() * 2);
    for ch in text.chars() {
        let u = ch as u32;
        if u < 0x80 {
            // `u == 0` 要写成 0xE0，否则会和字符串结束符混淆。
            out.push(if u == 0 { 0xE0 } else { u as u8 });
        } else if (0x4000..0xA000).contains(&u) {
            // CJK 主体：两字节。低字节为 0 时借 0x40 占位，避免产生 0x00。
            out.push(((u >> 8) + 0x40) as u8);
            out.push(if (u & 0xFF) == 0 { 0x40 } else { (u & 0xFF) as u8 });
        } else {
            // 变长：先数需要几个 7bit 组。
            let mut bits = 32i32;
            let mut v = u;
            while bits > 0 && (v & 0xFE00_0000) == 0 {
                bits -= 7;
                v = v.wrapping_shl(7);
            }
            let bytes_to_encode = ((bits + 6) / 7) as usize;
            out.push(0xE0 | bytes_to_encode as u8);
            let mut n = bytes_to_encode;
            while n > 0 {
                n -= 1;
                out.push((((v >> 25) & 0x7F) | 0x80) as u8);
                v = v.wrapping_shl(7);
            }
        }
    }
    out
}

/// `grammar::advance`（`gram_encoding.cc:48-52`）：从编码字节推下一个字符的步长。
#[inline]
pub fn advance(byte: u8) -> usize {
    if byte & 0x80 == 0 {
        1
    } else if byte & 0xF0 == 0xE0 {
        // 0xE0 = U+0000（1 字节）；0xE1..0xE8 = 2..9 字节
        usize::from(byte & 0x0F) + 1
    } else {
        2
    }
}

/// `grammar::next_unicode`（`gram_encoding.cc:54-56`）：编码串里下一个字符的位置。
pub fn next_unicode(encoded: &[u8], pos: usize) -> usize {
    pos + advance(encoded[pos])
}

/// `grammar::unicode_length`（`gram_encoding.cc:58-66`）：`length` 个字节里有多少字符。
pub fn unicode_length(encoded: &[u8], length: usize) -> usize {
    let end = length.min(encoded.len());
    let mut len = 0;
    let mut p = 0;
    while p < end {
        p += advance(encoded[p]);
        len += 1;
    }
    len
}

/// `scale_value`（`octagram.cc:53-55`）：trie 里的整数 → 对数概率。
///
/// 负数表示"这条 n-gram 不存在"的哨兵值，统一折算成 `-1`。
#[inline]
fn scale_value(value: i32) -> f64 {
    if value >= 0 {
        f64::from(value) / K_VALUE_SCALE
    } else {
        -1.0
    }
}

/// `last_n_unicode`（`octagram.cc:73-84`）：从尾部取最多 `max` 个字符。
///
/// 返回 `(起始字节位置, 实际取到的字符数)`。
fn last_n_unicode(text: &str, max: usize) -> (usize, usize) {
    let mut count = 0;
    let mut start = text.len();
    for (i, _) in text.char_indices().rev() {
        if count >= max {
            break;
        }
        start = i;
        count += 1;
    }
    (start, count)
}

/// `first_n_unicode`（`octagram.cc:86-97`）：从头部取最多 `max` 个字符。
///
/// 返回 `(结束字节位置, 实际取到的字符数)`。
fn first_n_unicode(text: &str, max: usize) -> (usize, usize) {
    let mut count = 0;
    for (i, _) in text.char_indices() {
        if count >= max {
            return (i, count);
        }
        count += 1;
    }
    (text.len(), count)
}

/// 语法模型实例（`class Octagram`，`octagram.h:19-33`）。
pub struct Octagram {
    config: GrammarConfig,
    db: Option<GramDb>,
    language: String,
}

impl Octagram {
    /// 用已加载的数据库造一个；`db` 为 `None` 时退化成"永远给 `non_collocation_penalty`"。
    pub fn new(config: GrammarConfig, language: impl Into<String>, db: Option<GramDb>) -> Self {
        Self {
            config,
            db,
            language: language.into(),
        }
    }

    /// 按 `grammar/language` 到 `model/` 里找 `<language>.gram` 并加载。
    ///
    /// 与 C++ 的 `ResourceResolver` 一致：**用户目录优先，其次共享目录**。
    /// `roots` 的顺序就是查找顺序。
    pub fn load(config: GrammarConfig, language: &str, roots: &[&Path]) -> Option<Self> {
        let file_name = format!("{language}.gram");
        let mut db = None;
        for root in roots {
            let candidate = root.join("model").join(&file_name);
            if !candidate.is_file() {
                continue;
            }
            match GramDb::open(&candidate) {
                Ok(opened) => {
                    eprintln!(
                        "[librime-next] 已加载语法模型：{}（{} 单元）",
                        candidate.display(),
                        opened.array_size()
                    );
                    db = Some(opened);
                    break;
                }
                Err(e) => {
                    eprintln!(
                        "[librime-next] 语法模型加载失败 {}：{e}",
                        candidate.display()
                    );
                }
            }
        }
        if db.is_none() {
            eprintln!("[librime-next] 找不到语法模型 {file_name}，语法分退化为常量");
        }
        Some(Self::new(config, language, db))
    }

    pub fn language(&self) -> &str {
        &self.language
    }

    pub fn has_db(&self) -> bool {
        self.db.is_some()
    }

    pub fn config(&self) -> &GrammarConfig {
        &self.config
    }
}

impl Grammar for Octagram {
    /// `Octagram::Query(context, word, is_rear)`（`octagram.cc:107-162`）。
    fn query(&self, context: &str, word: &str, is_rear: bool) -> f64 {
        let Some(db) = &self.db else {
            return self.config.non_collocation_penalty;
        };
        if context.is_empty() {
            return self.config.non_collocation_penalty;
        }
        let mut result = self.config.non_collocation_penalty;

        // `n = min(8, collocation_max_length - 1)`
        let n = K_MAX_ENCODED_UNICODE.min((self.config.collocation_max_length - 1).max(0) as usize);
        if n == 0 {
            return result;
        }

        // 只看 context 的最后 n 个字符、word 的最前 n 个字符。
        let (ctx_start, mut context_len) = last_n_unicode(context, n);
        let context_query = encode(&context[ctx_start..]);
        let (word_end, word_query_len) = first_n_unicode(word, n);
        let word_query = encode(&word[..word_end]);

        // 对 context 的每个后缀问一次：这个后缀 + word 前缀是否成 n-gram。
        let mut context_ptr = 0usize;
        while context_len > 0 {
            let matches = db.lookup_bytes(&context_query[context_ptr..], &word_query);
            for m in &matches {
                let match_len = unicode_length(&word_query, m.length);
                let collocation_len = context_len as i32 + match_len as i32;
                // `matches_whole_query`：从 context 开头算起且把 word 整个匹配上
                let whole_query = context_ptr == 0 && m.length == word_query.len();
                let penalty = if collocation_len >= self.config.collocation_min_length
                    || whole_query
                {
                    self.config.collocation_penalty
                } else {
                    self.config.weak_collocation_penalty
                };
                let candidate = scale_value(m.value) + penalty;
                if candidate > result {
                    result = candidate;
                }
            }
            context_len -= 1;
            if context_len > 0 {
                context_ptr = next_unicode(&context_query, context_ptr);
            }
        }

        // 句末：查 `word + "$"`，命中则视为"可以收尾"。
        if is_rear {
            let word_len = word.chars().count();
            if word_query_len == word_len {
                let matches = db.lookup_bytes(&word_query, b"$");
                if let Some(first) = matches.first() {
                    let candidate = scale_value(first.value) + self.config.rear_penalty;
                    if candidate > result {
                        result = candidate;
                    }
                }
            }
        }
        result
    }
}

/// 从方案配置造一个语法模型；没有 `grammar/language` 时返回 `None`。
pub fn create(
    config: &Value,
    roots: &[&Path],
) -> Option<Octagram> {
    let (grammar_config, language) = GrammarConfig::from_config(config)?;
    Octagram::load(grammar_config, &language, roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_ascii_is_identity() {
        assert_eq!(encode("abc"), b"abc".to_vec());
        // U+0000 特例：写成 0xE0，不能是 0x00
        assert_eq!(encode("\0"), vec![0xE0]);
    }

    #[test]
    fn encode_cjk_uses_two_bytes() {
        // U+4F60「你」→ (0x4F+0x40)=0x8F, 0x60
        assert_eq!(encode("你"), vec![0x8F, 0x60]);
        // U+597D「好」→ (0x59+0x40)=0x99, 0x7D
        assert_eq!(encode("好"), vec![0x99, 0x7D]);
        // 整串拼起来
        assert_eq!(encode("你好"), vec![0x8F, 0x60, 0x99, 0x7D]);
    }

    #[test]
    fn encode_roundtrip_lengths() {
        for text in ["你好世界", "abc", "你好abc", "\0x"] {
            let encoded = encode(text);
            assert_eq!(
                unicode_length(&encoded, encoded.len()),
                text.chars().count(),
                "{text:?} 的字符数应能被 unicode_length 还原"
            );
        }
    }

    #[test]
    fn advance_walks_encoded_string() {
        let encoded = encode("你好ab");
        let mut p = 0;
        let mut chars = 0;
        while p < encoded.len() {
            p = next_unicode(&encoded, p);
            chars += 1;
        }
        assert_eq!(p, encoded.len());
        assert_eq!(chars, 4);
    }

    #[test]
    fn last_and_first_n_unicode() {
        assert_eq!(last_n_unicode("你好世界", 2), ("你".len() + "好".len(), 2));
        assert_eq!(last_n_unicode("你好", 8), (0, 2));
        assert_eq!(last_n_unicode("", 3), (0, 0));
        assert_eq!(first_n_unicode("你好世界", 1), ("你".len(), 1));
        assert_eq!(first_n_unicode("你好", 8), ("你好".len(), 2));
        assert_eq!(first_n_unicode("", 3), (0, 0));
    }

    #[test]
    fn grammar_config_defaults_match_cpp() {
        let c = GrammarConfig::default();
        assert_eq!(c.collocation_max_length, 4);
        assert_eq!(c.collocation_min_length, 3);
        assert_eq!(c.collocation_penalty, -12.0);
        assert_eq!(c.non_collocation_penalty, -12.0);
        assert_eq!(c.weak_collocation_penalty, -24.0);
        assert_eq!(c.rear_penalty, -18.0);
    }

    #[test]
    fn config_without_language_yields_none() {
        let config: Value = serde_json::json!({ "grammar": {} });
        assert!(GrammarConfig::from_config(&config).is_none());
        let config: Value = serde_json::json!({});
        assert!(GrammarConfig::from_config(&config).is_none());
    }

    #[test]
    fn config_reads_language_and_overrides() {
        let config: Value = serde_json::json!({
            "grammar": {
                "language": "zh-hans-t-essay-bgw",
                "collocation_min_length": 2,
                "rear_penalty": -20.0
            }
        });
        let (c, language) = GrammarConfig::from_config(&config).expect("应有 grammar 节点");
        assert_eq!(language, "zh-hans-t-essay-bgw");
        assert_eq!(c.collocation_min_length, 2);
        assert_eq!(c.rear_penalty, -20.0);
        // 没覆盖的保持默认
        assert_eq!(c.collocation_max_length, 4);
    }

    #[test]
    fn query_without_db_returns_non_collocation_penalty() {
        let octagram = Octagram::new(GrammarConfig::default(), "zh-hant", None);
        assert_eq!(octagram.query("你好", "世界", false), -12.0);
    }

    #[test]
    fn query_with_empty_context_returns_penalty() {
        let octagram = Octagram::new(GrammarConfig::default(), "zh-hant", None);
        assert_eq!(octagram.query("", "世界", true), -12.0);
    }

    /// 真实模型落在用户目录；没有就跳过（CI 上不该因为缺 200MB 模型而红）。
    fn load_real(language: &str) -> Option<Octagram> {
        let appdata = std::env::var_os("APPDATA")?;
        let root = std::path::PathBuf::from(appdata).join("Rime-Next");
        Octagram::load(GrammarConfig::default(), language, &[root.as_path()])
            .filter(Octagram::has_db)
    }

    #[test]
    fn real_model_scores_common_collocation_higher() {
        let Some(octagram) = load_real("zh-hans-t-essay-bgc") else {
            return;
        };
        let common = octagram.query("中", "国", true);
        let rare = octagram.query("中", "果", true);
        eprintln!("grammar: 中国 = {common}, 中果 = {rare}");
        assert!(
            common > rare,
            "「中国」的语法分应高于「中果」：{common} vs {rare}"
        );
        assert!(common > -12.0, "常见搭配应高于 non_collocation_penalty");
    }

    /// 组句层验证：两个**词条权重相同**的分支，grammar 应把常见搭配选出来。
    #[test]
    fn poet_with_grammar_picks_common_collocation() {
        use crate::dict::vocabulary::DictEntry;
        use crate::engine::gear::poet::{Poet, WordGraph};

        let Some(octagram) = load_real("zh-hans-t-essay-bgc") else {
            return;
        };
        let poet = Poet::new(Some(Box::new(octagram)));
        let entry = |text: &str| DictEntry {
            text: text.to_string(),
            weight: -10.0,
            ..Default::default()
        };
        let mut graph = WordGraph::new();
        graph.entry(0).or_default().entry(2).or_default().push(entry("中"));
        let tail = graph.entry(2).or_default().entry(4).or_default();
        tail.push(entry("国"));
        tail.push(entry("果"));

        let sentence = poet.make_sentence(&graph, 4, "").expect("应能组句");
        assert_eq!(
            sentence.text, "中国",
            "两条分支词条权重相同，应靠语法分选「中国」，实际「{}」",
            sentence.text
        );
    }

    #[test]
    fn poet_without_grammar_still_makes_sentence() {
        use crate::dict::vocabulary::DictEntry;
        use crate::engine::gear::poet::{Poet, WordGraph};

        let poet = Poet::new(None);
        let entry = |text: &str| DictEntry {
            text: text.to_string(),
            weight: -10.0,
            ..Default::default()
        };
        let mut graph = WordGraph::new();
        graph.entry(0).or_default().entry(2).or_default().push(entry("中"));
        graph.entry(2).or_default().entry(4).or_default().push(entry("国"));
        let sentence = poet.make_sentence(&graph, 4, "").expect("应能组句");
        assert_eq!(sentence.text, "中国");
    }
}
