//! `src/rime/gear/table_translator.cc`：形码翻译器（五笔 / 郑码 / 宇浩 / 仓颉…）。
//!
//! 与 `script_translator` 的分工：拼音靠**音节图切分组合**（一份输入有多种切法），
//! 形码靠**整码命中 + 前缀补全**（prism 里的拼写就是用户的按键码本身）。
//! 两者共享 [`TranslatorOptions`]（认领段标签 / 削分隔符 / 补全开关 / 质量基数 /
//! preedit 与 comment 格式化 / 黑名单）。
//!
//! # 候选的三条来源（`table_translator.cc:244-309`）
//!
//! | 来源 | 键 | 说明 |
//! |---|---|---|
//! | 用户词·精确 | `code` | 用户打过、学过的词条 |
//! | 用户词·构造 | `\x7f` `enc` `\x1f` + `code` | [`UnityTableEncoder`] 算出来的码 |
//! | 词表·整码/补全 | `code` 的前缀族（`Prism::ExpandSearch`） | `.table.bin` |
//!
//! 三路**交错**输出，谁先出由 [`TableTranslator::prefer_user_phrase`] 决定
//! （对应 C++ `TableTranslation::PreferUserPhrase`）。交错而不是拼接很重要：
//! 用户词要顶到词表同码词前面，否则学过的词会沉到第 3 页去。
//!
//! # 与 C++ 的三处记账差异（都是"急切模型"逼出来的，语义等价）
//!
//! 1. **懒式翻页 → 一次取够**。C++ 的 `LazyTableTranslation` 先取 10 个键，不够再
//!    扩到 100、1000…；LRN 的候选是一次性铺平的，所以固定用 C++
//!    **首屏那一次**的额度（`kInitialSearchLimit = 10` 个键）。首屏候选与 C++ 一致，
//!    代价是翻到很后面时不会再有新键补进来。
//! 2. **`quality` 编码了候选的类别**。C++ 的菜单保持翻译器的出词顺序，`quality`
//!    只用于跨翻译器合并；LRN 的 [`crate::engine::menu::Menu`] 是**按 quality 排序**的
//!    单一列表，所以必须把"类别"（整码 / 用户词 / 补全）写进分数。见
//!    [`TableTranslator::quality_of`]。
//! 3. **`exp(weight)` → `weight`**。C++ 写 `std::exp(e->weight)`；实测 wubi86 的
//!    `weight` 能到 `+1.8`（`exp ≈ 6`）、生僻字到 `-54`，值域跨 30 个数量级。
//!    直接 `exp` 再加类别基数会被高权重吃掉，所以改用**同序**的 `weight`（对数尺度）。
//!
//! 还没移植的 C++ 部分：
//!
//! * `MakeSentence`（`table_translator.cc:383-478`）—— 形码整句输入（多词连打）。
//!   `enable_sentence` 默认 true，但缺了它只是"多词连打不出整句候选"，逐词输入不受影响。
//! * `encode_commit_history`（`table_translator.cc:327-353`）—— 从提交历史里学多词短语。
//!   它要 `Context::commit_history()`，而 LRN 的 `Context` 还没记这份历史。
//! * `enable_charset_filter` / `ContextualWeighted`（grammar 加权）。

use std::collections::HashSet;

use crate::dict::dict_entry_iterator::DictEntryIterator;
use crate::dict::dictionary::Dictionary;
use crate::dict::user_dictionary::{UserDictEntryIterator, UserDictionary};
use crate::dict::vocabulary::DictEntry;
use crate::engine::candidate::Candidate;
use crate::engine::gear::translator::Translator;
use crate::engine::gear::translator_options::TranslatorOptions;
use crate::engine::gear::unity_table_encoder::UnityTableEncoder;

/// C++ `kInitialSearchLimit`（`table_translator.cc:119`）：首屏取几个 prism 键。
const K_INITIAL_SEARCH_LIMIT: usize = 10;

/// C++ `kUnitySymbol`（`table_translator.cc:29`）= `" ☯ "`：
/// "这个词的码是我算出来的"提示。
const K_UNITY_SYMBOL: &str = " \u{262f} ";

/// 类别基数：**整码命中的表词条**。压过下面所有类别。
const TABLE_EXACT_BASE: f64 = 3.0e6;
/// 类别基数：**用户词（整码命中）**。C++ `PreferUserPhrase` 里用户词胜过同码的表词条。
const USER_EXACT_BASE: f64 = 4.0e6;
/// 类别基数：**用户词（还要补全）**。C++ 里它让位给"打满的表词条"，但压过表补全。
const USER_COMPLETION_BASE: f64 = 2.0e6;
/// 类别基数：**表词条的补全**。
const TABLE_COMPLETION_BASE: f64 = 1.0e6;

/// 形码翻译器（`class TableTranslator`，`table_translator.h:25-53`）。
pub struct TableTranslator {
    pub dict: Dictionary,
    pub options: TranslatorOptions,
    /// `user_dict_`：`None` = 方案关了用户词典或建不起来。
    pub user_dict: Option<UserDictionary>,
    /// `encoder_`：规则编码器（`enable_encoder` 且用户词典可用时才有）。
    pub encoder: Option<UnityTableEncoder>,
}

impl TableTranslator {
    pub fn new(dict: Dictionary, options: TranslatorOptions) -> Self {
        Self {
            dict,
            options,
            user_dict: None,
            encoder: None,
        }
    }

    /// 挂上用户词典（`Translator::user_dict_`）。
    pub fn with_user_dict(mut self, user_dict: UserDictionary) -> Self {
        self.user_dict = Some(user_dict);
        self
    }

    /// 挂上规则编码器（`encoder_`）。
    ///
    /// C++ 的条件是 `enable_encoder_ && user_dict_`（`table_translator.cc:231`）：
    /// 没有用户词典就没地方存构造词条，编码器装了也是空转。这条判断在
    /// [`crate::engine::gear::registry`] 那边做。
    pub fn with_encoder(mut self, encoder: UnityTableEncoder) -> Self {
        self.encoder = Some(encoder);
        self
    }

    /// `is_constructed(e)`（`table_translator.cc:69-71`）：码是我们算出来的。
    fn is_constructed(entry: &DictEntry) -> bool {
        UnityTableEncoder::has_prefix(&entry.custom_code)
    }

    /// 用户词典这一侧取词（`table_translator.cc:163-187` 的急切版）。
    ///
    /// 精确一遍 + 补全一遍，两遍都往同一个 `uter` 里追加 —— 与 C++
    /// `FetchUserPhrases` / `FetchMoreUserPhrases` 的累计语义一致。
    fn collect_user_entries(&mut self, code: &str) -> UserDictEntryIterator {
        let mut uter = UserDictEntryIterator::new();
        let Some(user_dict) = self.user_dict.as_ref() else {
            return uter;
        };
        if !user_dict.loaded() {
            return uter;
        }
        // ① 精确：`LookupWords(&uter_, input_, false, 0, &user_dict_key_)`
        user_dict.lookup_words(&mut uter, code, false, 0, None);
        // ② 构造词条：同样的码，但键带 `\x7fenc\x1f` 前缀
        if let Some(encoder) = self.encoder.as_ref().filter(|e| e.loaded()) {
            encoder.lookup_phrases(user_dict, &mut uter, code, false, 0, None);
        }
        // ③ 补全：`LookupWords(&uter_, input_, true, user_dict_limit_, ...)`
        //    C++ 是 10 → 100 → 1000 逐级扩；急切模型只走 C++ 的**第一级**。
        if self.options.enable_completion {
            user_dict.lookup_words(&mut uter, code, true, K_INITIAL_SEARCH_LIMIT, None);
            if let Some(encoder) = self.encoder.as_ref().filter(|e| e.loaded()) {
                encoder.lookup_phrases(
                    user_dict,
                    &mut uter,
                    code,
                    true,
                    K_INITIAL_SEARCH_LIMIT,
                    None,
                );
            }
        }
        uter
    }

    /// `TableTranslation::PreferUserPhrase`（`table_translator.cc:102-113`）：
    /// 下一条该从用户侧还是词表侧取。
    ///
    /// 规则（逐字照抄）：
    /// * 用户侧空了 → 取词表；
    /// * 词表侧空了 → 取用户；
    /// * 词表侧**打满了码**，而用户侧没打满**或者**是构造词条 → 取词表；
    /// * 其余 → 取用户。
    ///
    /// 第 3 条的用意：用户学过的词优先级高，但"打满的码"是用户的明确意图，
    /// 不能被一个同码的模糊用户词顶掉。
    fn prefer_user_phrase(table: &mut DictEntryIterator, user: &UserDictEntryIterator) -> bool {
        let Some(user_entry) = user.peek() else {
            return false;
        };
        let Some(table_entry) = table.peek() else {
            return true;
        };
        !(table_entry.remaining_code_length == 0
            && (user_entry.remaining_code_length != 0 || Self::is_constructed(user_entry)))
    }

    /// 候选的 `quality`（见模块文档第 2、3 条差异）。
    ///
    /// ```text
    /// quality = 类别基数 + entry.weight + initial_quality
    /// ```
    ///
    /// 类别基数保证"整码 > 用户词 > 补全"的大方向（C++ 靠出词顺序表达），
    /// `entry.weight` 在类别内部按对数概率排（`exp` 的同序替身）。
    fn quality_of(&self, entry: &DictEntry, is_user: bool) -> f64 {
        let incomplete = entry.remaining_code_length != 0;
        let base = match (is_user, incomplete) {
            (true, false) => USER_EXACT_BASE,
            (false, false) => TABLE_EXACT_BASE,
            (true, true) => USER_COMPLETION_BASE,
            (false, true) => TABLE_COMPLETION_BASE,
        };
        base + entry.weight + self.options.initial_quality
    }

    /// 把一个词条包成候选（`TableTranslation::Peek`，`table_translator.cc:73-94`）。
    fn make_candidate(
        &mut self,
        entry: &DictEntry,
        is_user: bool,
        start: usize,
        end: usize,
        preedit: &str,
    ) -> Candidate {
        let constructed = Self::is_constructed(entry);
        let mut comment = if constructed {
            K_UNITY_SYMBOL.to_string()
        } else {
            entry.comment.clone()
        };
        self.options.format_comment(&mut comment);
        let quality = self.quality_of(entry, is_user);
        // C++ 的 `type`：补全 → "completion"，用户词 → "user_table"，其余 → "table"
        let kind = if entry.remaining_code_length != 0 {
            "completion"
        } else if is_user {
            "user_table"
        } else {
            "table"
        };
        // 词条挂到候选上：选中提交后靠它回写 userdb（`Translator::Memorize`）
        Candidate::new(kind, start, end, entry.text.clone(), quality)
            .with_comment(comment)
            .with_preedit(preedit.to_string())
            .with_entry(entry.clone())
    }
}

impl Translator for TableTranslator {
    fn query(&mut self, input: &str, start: usize, seg_tags: &[String]) -> Vec<Candidate> {
        if !self.options.has_any_tag(seg_tags) {
            return Vec::new();
        }
        // C++：`boost::trim_right_if(code, is_any_of(delimiters_))`
        let code = self.options.trim_delimiters(input).to_string();
        if code.is_empty() {
            return Vec::new();
        }
        // C++ 的 `end` 是 `segment.start + input.length()`；`start` 是段起点。
        let end = start + input.len();
        let preedit = input.to_string();

        // ① 用户侧（精确 + 构造 + 补全）
        let mut uter = self.collect_user_entries(&code);
        let user_ready = !uter.exhausted();

        // ② 词表侧
        let mut iter = DictEntryIterator::new();
        let blacklist: &HashSet<String> = &self.options.blacklist;
        if self.options.enable_completion {
            // 补全模式：`ExpandSearch(code, 10)` —— 首屏额度与 C++ 一致
            self.dict.lookup_words(
                &mut iter,
                &code,
                true,
                K_INITIAL_SEARCH_LIMIT,
                Some(blacklist),
            );
        } else {
            self.dict
                .lookup_words(&mut iter, &code, false, 0, Some(blacklist));
        }

        // ③ 交错合流
        let mut out: Vec<Candidate> = Vec::new();
        if !user_ready && iter.exhausted() {
            return out;
        }
        loop {
            let take_user = Self::prefer_user_phrase(&mut iter, &uter);
            if take_user {
                let Some(entry) = uter.peek().cloned() else {
                    break;
                };
                out.push(self.make_candidate(&entry, true, start, end, &preedit));
                if !uter.next() {
                    // 用户侧读完了，剩下的全从词表取
                    while let Some(entry) = iter.peek().cloned() {
                        out.push(self.make_candidate(&entry, false, start, end, &preedit));
                        if !iter.next() {
                            break;
                        }
                    }
                    break;
                }
            } else {
                let Some(entry) = iter.peek().cloned() else {
                    break;
                };
                out.push(self.make_candidate(&entry, false, start, end, &preedit));
                if !iter.next() {
                    while let Some(entry) = uter.peek().cloned() {
                        out.push(self.make_candidate(&entry, true, start, end, &preedit));
                        if !uter.next() {
                            break;
                        }
                    }
                    break;
                }
            }
        }
        out
    }

    /// `TableTranslator::Memorize`（`table_translator.cc:311-356`）。
    ///
    /// 两步：
    /// 1. 逐条写用户词典；**构造词条要先摘掉前缀**，否则会被当成又一条构造词
    ///    （键里再套一层 `\x7fenc\x1f`，下次查不到）。
    /// 2. 多字词再交给编码器算一个"按规则拼出来的码"存进去
    ///    （`elements.len() > 1` 才算 —— 单字不需要造词）。
    fn memorize(&mut self, elements: &[DictEntry]) -> bool {
        if self.user_dict.is_none() {
            return false;
        }
        // ① 逐条写库
        for entry in elements {
            let mut blessed = entry.clone();
            let constructed = Self::is_constructed(&blessed);
            if constructed {
                UnityTableEncoder::remove_prefix(&mut blessed.custom_code);
            }
            if let Some(user_dict) = self.user_dict.as_mut() {
                user_dict.update_entry(&self.dict, &blessed, 1);
            }
        }

        // ② 造词：多字短语交给规则编码器
        let encoder_ready = self.encoder.as_ref().is_some_and(|e| e.loaded());
        if !encoder_ready || elements.len() <= 1 {
            return true;
        }
        let phrase: String = elements.iter().map(|e| e.text.as_str()).collect();
        if phrase.is_empty() {
            return true;
        }
        if let Some(encoder) = self.encoder.as_ref() {
            if let Some(user_dict) = self.user_dict.as_mut() {
                encoder.encode_phrase(&self.dict, user_dict, &phrase, "1");
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::vocabulary::Code;

    fn entry(text: &str, weight: f64, remaining_code_length: i32, custom_code: &str) -> DictEntry {
        DictEntry {
            text: text.to_string(),
            weight,
            remaining_code_length,
            custom_code: custom_code.to_string(),
            code: Code::from_slice(&[1]),
            ..Default::default()
        }
    }

    /// 类别基数的**大小关系**就是这个模块的核心契约：
    /// 用户整码 > 表整码 > 用户补全 > 表补全。
    #[test]
    fn category_bases_are_ordered() {
        assert!(USER_EXACT_BASE > TABLE_EXACT_BASE);
        assert!(TABLE_EXACT_BASE > USER_COMPLETION_BASE);
        assert!(USER_COMPLETION_BASE > TABLE_COMPLETION_BASE);
        // 类别间距必须远大于 weight 的值域（实测 [-54, +25]）
        assert!(TABLE_EXACT_BASE - USER_COMPLETION_BASE > 1.0e5);
    }

    /// `is_constructed` 只认那个前缀。
    #[test]
    fn constructed_entries_are_detected_by_prefix() {
        let plain = entry("甲乙", 0.0, 0, "ab ");
        assert!(!TableTranslator::is_constructed(&plain));
        let constructed = entry("甲乙", 0.0, 0, "\u{7f}enc\u{1f}ab ");
        assert!(TableTranslator::is_constructed(&constructed));
    }
}
