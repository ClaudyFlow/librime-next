//! `src/rime/gear/unity_table_encoder.{h,cc}`：形码方案的**规则编码器**。
//!
//! # 它解决什么问题
//!
//! 形码方案（仓颉 / 郑码 / 五笔）里，一个"词"的码不是把每个字的码拼起来就完事 ——
//! 仓颉的 `AaAzBaBbBz` 这种公式规定"取第 1 字首码 + 第 1 字尾码 + 第 2 字首码 …"。
//! 用户打出一个词库外的短语（比如自造的两个字），输入法要能**按同一套规则给这个词编码**
//! 并记住，否则下次打同样的码就找不回来。
//!
//! 这条链路是：
//!
//! ```text
//! 用户提交「甲乙」
//!   → TableTranslator::memorize
//!      → UnityTableEncoder::encode_phrase("甲乙", "1")
//!         → TableEncoder 按 encoder/rules 公式拼码
//!            → PhraseCollector::translate_word("甲") = 甲的反查码集合（取一个）
//!            → PhraseCollector::create_entry("甲乙", "ab", "1")
//!               → UserDictionary::update_entry_with_prefix(..., "\x7f" "enc\x1f")
//!                  → userdb 里的键 = "\x7f" "enc\x1f" "ab \t甲乙"
//! ```
//!
//! 关键在最后那步的**前缀**：构造出来的词条用一个不可见前缀
//! [`K_ENCODED_PREFIX`] 隔开，查询时也带同样的前缀，于是这类"算出来的码"
//! 永远不会和用户显式输入的词条混在一起。
//!
//! # 数据从哪来
//!
//! `translate_word` 要"某个字有哪些码"，这来自 `.reverse.bin` 的反查表
//! （[`ReverseDb::lookup`]）；规则本身来自同一份词典头部的 `encoder:` 节点，
//! 编译期被抄进了 `.reverse.bin` 的 metadata（[`ReverseDb::dict_settings`]）。
//! 所以运行期**只需要** `.reverse.bin`，不需要 `*.dict.yaml`。
//!
//! # 与 C++ 的接口差异
//!
//! C++ 的 `UnityTableEncoder` 同时是 `PhraseCollector`，构造时 `set_collector(this)`
//! 形成**自引用**（它既是编码器又是收集方）。Rust 不能这么写，于是：
//!
//! * 收集方做成临时结构 [`UnityCollector`]，**每次调用时**才把
//!   "反查表 + 目标用户词典"两把借用装进去；
//! * `user_dict` 因此是**方法参数**而不是字段（C++ 是构造时传进来的裸指针）。
//!
//! 语义完全一致，只是换个把借用拼起来的时机。

use std::path::Path;

use crate::algo::encoder::{Encoder, PhraseCollector, TableEncoder};
use crate::dict::dictionary::Dictionary;
use crate::dict::reverse_lookup_dictionary::ReverseDb;
use crate::dict::user_dictionary::{UserDictEntryIterator, UserDictionary};
use crate::dict::vocabulary::DictEntry;

/// `kEncodedPrefix`（`unity_table_encoder.cc:15-17`）= `"\x7fenc\x1f"`。
///
/// 为什么是这个怪串：`\x7f` 与 `\x1f` 都是控制字符，正常人不可能把它们打进码串，
/// 所以带这个前缀的键一定是我们自己算出来的。`\x7f` 还顺带让它在字典序里排在
/// 所有可见字符**之后**，扫 `userdb` 时不会插进用户的正常词条中间。
pub const K_ENCODED_PREFIX: &str = "\x7fenc\x1f";

/// `class UnityTableEncoder : public TableEncoder, public PhraseCollector`
/// （`unity_table_encoder.h:19-42`）。
pub struct UnityTableEncoder {
    /// `TableEncoder` 那部分：`encoder/rules`、`exclude_patterns`、`tail_anchor`。
    encoder: TableEncoder,
    /// `rev_dict_`：反查表（`词 → 码…`），也是规则设置的来源。
    rev_dict: ReverseDb,
}

impl UnityTableEncoder {
    /// `UnityTableEncoder::Load(const Ticket&)`（`unity_table_encoder.cc:26-43`）。
    ///
    /// 三步：
    /// 1. 打开 `<dict_name>.reverse.bin`（`ReverseLookupDictionaryComponent` 的产物）；
    /// 2. 读它内嵌的词典头部 YAML，**没有 `use_rule_based_encoder` 就不启用**
    ///    （`unity_table_encoder.cc:37-41` 打的是 WARNING 而不是 ERROR）；
    /// 3. 用那份 YAML 的 `encoder/*` 节点装规则。
    ///
    /// 任何一步失败都返回 `None` —— 调用方照 C++ 的做法把 `encoder_` 留空，
    /// 翻译器照常工作，只是学不了新词。
    pub fn load(build_dir: &Path, dict_name: &str) -> Option<Self> {
        let reverse_path =
            crate::dict::dictionary::bin_resource_path(build_dir, dict_name, ".reverse");
        let mut rev_dict = ReverseDb::open(&reverse_path).ok()?;
        if !rev_dict.load() {
            eprintln!(
                "[unity_table_encoder] cannot load reverse db {reverse_path:?}, \
                 skipping phrase encoding."
            );
            return None;
        }
        let settings = rev_dict.load_dict_settings()?;
        if !settings.use_rule_based_encoder() {
            // C++：`LOG(WARNING) << "unity table encoder is not enabled in dict settings.";`
            return None;
        }
        let mut encoder = TableEncoder::default();
        if !encoder.load_settings(settings.config()) {
            return None;
        }
        Some(Self { encoder, rev_dict })
    }

    /// `UnityTableEncoder::loaded()`：有规则才算装好（`TableEncoder::loaded_`）。
    pub fn loaded(&self) -> bool {
        self.encoder.loaded()
    }

    /// `UnityTableEncoder::LookupPhrases`（`unity_table_encoder.cc:71-80`）：
    /// 从用户词典里取**构造出来的**词条（键带 [`K_ENCODED_PREFIX`]）。
    ///
    /// 注意 `input` 是用户的**按键码**，不是音节串 —— 形码方案里两者是一回事。
    pub fn lookup_phrases(
        &self,
        user_dict: &UserDictionary,
        result: &mut UserDictEntryIterator,
        input: &str,
        predictive: bool,
        limit: usize,
        resume_key: Option<&str>,
    ) -> (usize, Option<String>) {
        let key = format!("{K_ENCODED_PREFIX}{input}");
        user_dict.lookup_words(result, &key, predictive, limit, resume_key)
    }

    /// `UnityTableEncoder::EncodePhrase`（`unity_table_encoder.cc:45-55` 的桥接）：
    /// 给短语编码并写进用户词典。
    ///
    /// `value` 是"权重串"：`"1"` 表示用户刚提交过（`commits = 1`），
    /// `"0"` 表示只是"占位记住"（`commits = 0`，不涨热度）——
    /// C++ 在 `encode_commit_history` 里正是用 `"0"`。
    pub fn encode_phrase(
        &self,
        dict: &Dictionary,
        user_dict: &mut UserDictionary,
        phrase: &str,
        value: &str,
    ) -> bool {
        let mut collector = UnityCollector {
            rev_dict: &self.rev_dict,
            dict,
            user_dict,
        };
        self.encoder
            .encode_phrase_with(&mut collector, phrase, value)
    }

    /// `UnityTableEncoder::HasPrefix`（`unity_table_encoder.cc:82-84`）。
    pub fn has_prefix(key: &str) -> bool {
        key.starts_with(K_ENCODED_PREFIX)
    }

    /// `UnityTableEncoder::AddPrefix`（`:86-89`）。
    pub fn add_prefix(key: &mut String) {
        key.insert_str(0, K_ENCODED_PREFIX);
    }

    /// `UnityTableEncoder::RemovePrefix`（`:91-96`）：没有前缀返回 `false`（不改动）。
    pub fn remove_prefix(key: &mut String) -> bool {
        if !Self::has_prefix(key) {
            return false;
        }
        key.drain(..K_ENCODED_PREFIX.len());
        true
    }
}

/// `PhraseCollector` 的临时实现（C++ 里就是 `UnityTableEncoder` 自己）。
struct UnityCollector<'a> {
    rev_dict: &'a ReverseDb,
    /// `UpdateEntry` 的 `source` 参数：只有 `custom_code` 为空时才用来反查码串，
    /// 而这里恒非空；留着只为满足签名。
    dict: &'a Dictionary,
    user_dict: &'a mut UserDictionary,
}

impl PhraseCollector for UnityCollector<'_> {
    /// `UnityTableEncoder::CreateEntry`（`unity_table_encoder.cc:45-55`）。
    fn create_entry(&mut self, phrase: &str, code_str: &str, value: &str) {
        let entry = DictEntry {
            text: phrase.to_string(),
            // C++：`code_str + ' '` —— 尾巴上那个空格是 userdb 键的分隔符，
            // 少了它 `LookupWords` 的 `key[len] == ' '` 判定就不成立。
            custom_code: format!("{code_str} "),
            ..Default::default()
        };
        let commits = if value == "0" { 0 } else { 1 };
        self.user_dict
            .update_entry_with_prefix(self.dict, &entry, commits, K_ENCODED_PREFIX);
    }

    /// `UnityTableEncoder::TranslateWord`（`unity_table_encoder.cc:57-69`）：
    /// 先查"词根"（`stem` 列收集的），再查反查表；两者都给空格分隔的码串。
    fn translate_word(&mut self, word: &str, code: &mut Vec<String>) -> bool {
        let str_list = match self.rev_dict.lookup_stems(word) {
            Some(list) => list,
            None => match self.rev_dict.lookup(word) {
                Some(list) => list,
                None => return false,
            },
        };
        // C++：`boost::split(*code, str_list, boost::is_any_of(" "))`
        code.clear();
        code.extend(str_list.split(' ').map(str::to_string));
        !code.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_roundtrip() {
        assert_eq!(K_ENCODED_PREFIX, "\u{7f}enc\u{1f}");
        let mut key = "ab".to_string();
        assert!(!UnityTableEncoder::has_prefix(&key));
        UnityTableEncoder::add_prefix(&mut key);
        assert!(UnityTableEncoder::has_prefix(&key));
        assert_eq!(key, "\u{7f}enc\u{1f}ab");
        assert!(UnityTableEncoder::remove_prefix(&mut key));
        assert_eq!(key, "ab");
        // 没有前缀时不动，且返回 false
        assert!(!UnityTableEncoder::remove_prefix(&mut key));
        assert_eq!(key, "ab");
    }

    #[test]
    fn encoded_prefix_sorts_last() {
        // `\x7f` 是 0x7f，比所有可见 ASCII 都大 —— 扫 userdb 时构造词条排在最后
        assert!((K_ENCODED_PREFIX.as_bytes()[0]) > b'z');
    }
}
