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
//! # 整句（`MakeSentence`，`table_translator.cc:543-693`）
//!
//! 形码的整句不是"音节切分组合"，而是**在码串上做最短路**：把输入里每个位置当成
//! 图的顶点，凡是"从 `start_pos` 到 `end_pos` 这截码能查到词"就连一条边，然后交给
//! [`Poet`] 选一条总分最高的路径。三条边的来源与词级候选相同（用户词 / 构造词 / 词表），
//! 区别只在**不做补全**（`predictive = false`）—— 组句要的是精确落脚点。
//!
//! `enable_sentence` 在出厂方案里是 **true**（`wubi86`、`cangjie5`、`yujoy` 系），
//! 但 C++ 只在**词级候选全空**时才组句（`if (enable_sentence_ && !translation)`），
//! 所以单字/单词输入走的仍是原来的路，逐词输入行为不受影响。
//!
//! # 字符集过滤（`enable_charset_filter`）
//!
//! `wubi86` / `cangjie5` / `cangjie5_express` 出厂都开了这个开关：候选里只要有字符
//! 落在 CJK 扩展区 / 兼容汉字区就丢掉（判据见 [`super::charset_filter`]）。它**不是**
//! 一个过滤组件，而是翻译器自己装的 —— 词级候选走"包一层文本过滤"，组句走"建图时
//! 给迭代器挂 `filter_dict_entry`"（后者必须做，否则会用生僻字拼出一条整句再整条丢掉）。
//! 用户打开 `extended_charset` 后整个失效。
//!
//! 还没移植的 C++ 部分：`ContextualWeighted`（grammar 上下文加权；出厂方案没开
//! `contextual_suggestions`）。

use std::collections::{BTreeMap, HashSet};

use crate::dict::dict_entry_iterator::DictEntryIterator;
use crate::dict::dictionary::Dictionary;
use crate::dict::user_dictionary::{UserDictEntryIterator, UserDictionary};
use crate::dict::vocabulary::{Code, DictEntry};
use crate::engine::gear::charset_filter;
use crate::engine::gear::poet::{Poet, Sentence, WordGraph};
use crate::engine::candidate::Candidate;
use crate::engine::commit_history::CommitHistory;
use crate::engine::gear::translator::Translator;
use crate::engine::gear::translator_options::TranslatorOptions;
use crate::engine::gear::unity_table_encoder::UnityTableEncoder;
use crate::engine::segmentation::Segment;

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

/// 整句候选的基数。要压过上面所有词级基数（`USER_EXACT_BASE`），
/// 因为 C++ 里 `SentenceTranslation` 把整句天然排在候选列表最前。
const SENTENCE_BASE: f64 = 5.0e6;

/// 整句之后"手选首词"候选的**递减步长**。
///
/// C++ 的顺序是结构性的（先整句、再首词码长从长到短、同长用户词优先），
/// LRN 的菜单按 `quality` 排序，所以把顺序直接编码成严格递减的分数：
/// 每多一条就减 `1e-3`，1e6 条以内都不会掉出 [`SENTENCE_BASE`] 量级。
const PHRASE_STEP: f64 = 1.0e-3;

/// 形码翻译器（`class TableTranslator`，`table_translator.h:25-53`）。
pub struct TableTranslator {
    pub dict: Dictionary,
    pub options: TranslatorOptions,
    /// `user_dict_`：`None` = 方案关了用户词典或建不起来。
    pub user_dict: Option<UserDictionary>,
    /// `encoder_`：规则编码器（`enable_encoder` 且用户词典可用时才有）。
    pub encoder: Option<UnityTableEncoder>,
    /// `encode_commit_history`：学短语时顺手把"历史上连续提交的几个词"也编码。
    /// C++ 默认 **true**（`table_translator.h:45`）。
    pub encode_commit_history: bool,
    /// `max_phrase_length`：`encode_commit_history` 拼到多长就停。默认 5。
    pub max_phrase_length: i32,
    /// `enable_sentence`：词级候选全空时是否组整句。默认 **true**（`table_translator.h:41`）。
    pub enable_sentence: bool,
    /// `sentence_over_completion`：词级候选**只**有补全时，是否把整句拼到前面。
    /// 默认 **false**。
    pub sentence_over_completion: bool,
    /// `max_homographs`：组句用的词图里，每个 `[start, end)` 格子最多留几个同码词。
    /// 默认 **1**（`table_translator.h:47`）—— 也就是"每格取最优"。
    pub max_homographs: usize,
    /// `enable_charset_filter`：过滤扩展区汉字（`𠛈` 这种一辈子不会打的字）。
    /// 默认 **false**，但 `wubi86` / `cangjie5` / `cangjie5_express` 出厂都开了。
    pub enable_charset_filter: bool,
    /// `extended_charset` 开关的镜像（`Translator::sync_context` 每轮刷新）。
    /// 用户打开"显示生僻字"后，上面的过滤器整个失效。
    pub extended_charset: bool,
    /// `poet_`：组句器。C++ 用 `Poet::LeftAssociateCompare`（同权重时词少的赢）。
    pub poet: Poet,
}

impl TableTranslator {
    pub fn new(dict: Dictionary, options: TranslatorOptions) -> Self {
        Self {
            dict,
            options,
            user_dict: None,
            encoder: None,
            encode_commit_history: true,
            max_phrase_length: 5,
            enable_sentence: true,
            sentence_over_completion: false,
            max_homographs: 1,
            enable_charset_filter: false,
            extended_charset: false,
            poet: Poet::with_left_associative(None),
        }
    }

    /// `filter_by_charset`（`table_translator.cc:546-547`）：开关开着、且用户没要生僻字。
    fn filter_by_charset(&self) -> bool {
        self.enable_charset_filter && !self.extended_charset
    }

    /// 装语法模型（`Poet(language(), config, LeftAssociateCompare)` 的 `grammar_`）。
    pub fn with_poet(mut self, poet: Poet) -> Self {
        self.poet = poet;
        self
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
    /// 刷 `extended_charset`（`enable_charset_filter` 的否决开关）。
    /// C++ 是在 `Query` / `MakeSentence` 里现读 `engine_->context()`，LRN 的 `query`
    /// 拿不到 `Context`，只能在这里同步。
    fn sync_context(&mut self, ctx: &crate::engine::context::Context) {
        self.extended_charset = ctx.get_option("extended_charset");
    }

    fn query(
        &mut self,
        input: &str,
        segment: &mut Segment,
        _history: &crate::engine::commit_history::CommitHistory,
    ) -> Vec<Candidate> {
        if !self.options.has_any_tag(&segment.tags) {
            return Vec::new();
        }
        // C++：`boost::trim_right_if(code, is_any_of(delimiters_))`
        let code = self.options.trim_delimiters(input).to_string();
        if code.is_empty() {
            return Vec::new();
        }
        // 候选坐标一律**相对本段**：`Engine::translate_segment` 会统一加上
        // `segment.start` 还原成绝对偏移（见 `runtime.rs` 里那句 `c.start += seg.start`）。
        // 这里原来写的是 `segment.start + input.len()`，和调用方约定撞车 ——
        // 单段输入（`start == 0`）看不出来，多段输入会把偏移算成两倍。
        let end = input.len();
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
            return self.append_sentence(input, out);
        }
        loop {
            let take_user = Self::prefer_user_phrase(&mut iter, &uter);
            if take_user {
                let Some(entry) = uter.peek().cloned() else {
                    break;
                };
                out.push(self.make_candidate(&entry, true, 0, end, &preedit));
                if !uter.next() {
                    // 用户侧读完了，剩下的全从词表取
                    while let Some(entry) = iter.peek().cloned() {
                        out.push(self.make_candidate(&entry, false, 0, end, &preedit));
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
                out.push(self.make_candidate(&entry, false, 0, end, &preedit));
                if !iter.next() {
                    while let Some(entry) = uter.peek().cloned() {
                        out.push(self.make_candidate(&entry, true, 0, end, &preedit));
                        if !uter.next() {
                            break;
                        }
                    }
                    break;
                }
            }
        }
        self.append_sentence(input, out)
    }

    /// `TableTranslator::Memorize`（`table_translator.cc:311-356`）。
    ///
    /// 两步：
    /// 1. 逐条写用户词典；**构造词条要先摘掉前缀**，否则会被当成又一条构造词
    ///    （键里再套一层 `\x7fenc\x1f`，下次查不到）。
    /// 2. 多字词再交给编码器算一个"按规则拼出来的码"存进去
    ///    （`elements.len() > 1` 才算 —— 单字不需要造词）。
    fn memorize(&mut self, elements: &[DictEntry], history: &CommitHistory) -> bool {
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
        if !encoder_ready {
            return true;
        }
        if elements.len() > 1 {
            let phrase: String = elements.iter().map(|e| e.text.as_str()).collect();
            if !phrase.is_empty() {
                if let Some(encoder) = self.encoder.as_ref() {
                    if let Some(user_dict) = self.user_dict.as_mut() {
                        encoder.encode_phrase(&self.dict, user_dict, &phrase, "1");
                    }
                }
            }
        }

        // ③ `encode_commit_history`：从历史里把"连续几个形码词"拼成短语也编一遍
        self.encode_from_history(history);
        true
    }
}

impl TableTranslator {
    /// `encode_commit_history`（`table_translator.cc:326-354`）。
    ///
    /// 场景：用户连着打了「中」「国」「人」三个单字（三次提交），各自学过了，
    /// 但「中国人」这个**短语**没学过。这里从历史尾部往前凑，凑到 2、3、4、5 个字
    /// 就各编一次码（`"0"` 表示"这几个字连起来用"），下次打整串码就能直接出短语。
    ///
    /// 三个照抄的细节：
    ///
    /// 1. **末尾的标点先跳过**（`if (it->type == "punct") ++it`）——「中国！」之后
    ///    仍然能凑出「中国」；
    /// 2. 只接受 `table` / `user_table` / `sentence` / `uniquified` 四种记录，
    ///    遇到别的（`script`、`raw`、`punct`）立即停 —— 形码方案里混进拼音词
    ///    拼出来的短语没有意义；
    /// 3. **第一次循环不编码**，只把最后一个词记成 `phrase`，从第二次起才
    ///    "前面再叠一个词"然后编码。
    fn encode_from_history(&mut self, history: &CommitHistory) {
        if !self.encode_commit_history || history.is_empty() {
            return;
        }
        let mut records = history.iter_rev().peekable();
        if records.peek().map(|r| r.kind == "punct").unwrap_or(false) {
            records.next();
        }
        let mut phrase = String::new();
        for record in records {
            if !matches!(
                record.kind.as_str(),
                "table" | "user_table" | "sentence" | "uniquified"
            ) {
                break;
            }
            if phrase.is_empty() {
                // 最后一个词：只记下，不编码
                phrase = record.text.clone();
                continue;
            }
            phrase = format!("{}{}", record.text, phrase);
            if phrase.chars().count() as i32 > self.max_phrase_length {
                break;
            }
            if let (Some(encoder), Some(user_dict)) = (self.encoder.as_ref(), self.user_dict.as_mut())
            {
                encoder.encode_phrase(&self.dict, user_dict, &phrase, "0");
            }
        }
    }

    /// 词级候选算完之后的两条整句分支（`table_translator.cc:296-300`）。
    ///
    /// * `enable_sentence && 词级全空` → 整句独占（`include_prefix_phrases = true`，
    ///   因为这时用户没有别的路可走，必须给他"手选首词"的余地）；
    /// * `sentence_over_completion && 首条是补全` → 整句拼在补全候选**前面**
    ///   （`include_prefix_phrases = false`：补全候选本身就是手选首词的余地，
    ///   再给一份就重复了）。
    ///
    /// 顺序靠 `quality` 表达：整句基数是 [`SENTENCE_BASE`]，压过所有词级基数。
    fn append_sentence(&mut self, input: &str, mut word_candidates: Vec<Candidate>) -> Vec<Candidate> {
        // C++ 的顺序是"先包 CharsetFilterTranslation、再判 exhausted"，
        // 所以被字符集过滤光了的词级候选等同于"没有词级候选"，照样要试组句。
        if self.filter_by_charset() {
            word_candidates.retain(|candidate| charset_filter::filter_text(&candidate.text));
        }
        if self.enable_sentence && word_candidates.is_empty() {
            let mut candidates = self.make_sentence(input, true);
            if self.filter_by_charset() {
                candidates.retain(|candidate| charset_filter::filter_text(&candidate.text));
            }
            return candidates;
        }
        if self.sentence_over_completion
            && word_candidates
                .first()
                .is_some_and(|candidate| candidate.kind == "completion")
        {
            let mut merged = self.make_sentence(input, false);
            if self.filter_by_charset() {
                merged.retain(|candidate| charset_filter::filter_text(&candidate.text));
            }
            merged.append(&mut word_candidates);
            return merged;
        }
        word_candidates
    }

    /// `MakeSentence`（`table_translator.cc:543-693`）：在码串上建词图，交给 [`Poet`] 求最短路。
    ///
    /// 返回顺序照抄 C++：**整句一条**，随后是"手选首词"候选 —— 首段码长**从长到短**，
    /// 同长度时用户词优先（`SentenceTranslation::PreferUserPhrase`）。
    ///
    /// # 三处照抄的细节
    ///
    /// 1. **只有精确命中能连边**。三个循环全部 `predictive = false`：组句要的是落脚点，
    ///    拿补全词去拼会让"半截码"也被当成一个词，整句立刻散架。
    /// 2. **`vertices` 才决定要不要从这个位置展开**。从 0 出发，只有真的查到词的那一截
    ///    末尾才插进 `vertices`，所以图是从左到右"水波扩散"建起来的，不是全枚举。
    /// 3. **首词候选要不要跳过头一条**，取决于 `max_homographs`：`> 1` 时建图用副本、
    ///    候选从第 1 条起；`== 1`（出厂默认）时建图先吃掉 1 条、候选从第 2 条起。
    ///    理由是第 1 条已经被整句选进去了，候选列表负责给"换一个首词"的余地。
    fn make_sentence(&mut self, input: &str, include_prefix_phrases: bool) -> Vec<Candidate> {
        let n = input.len();
        if n == 0 {
            return Vec::new();
        }
        let delimiters = self.options.delimiters.clone();
        let blacklist = self.options.blacklist.clone();
        let max_homographs = self.max_homographs.max(1);
        let copy_for_phrases = max_homographs > 1;

        let mut graph = WordGraph::new();
        let mut vertices: HashSet<usize> = HashSet::from([0]);
        // 首段码长 → 同码词条（C++ 的 `DictEntryCollector` / `UserDictEntryCollector`）。
        let mut table_phrases: BTreeMap<usize, Vec<DictEntry>> = BTreeMap::new();
        let mut user_phrases: BTreeMap<usize, Vec<DictEntry>> = BTreeMap::new();

        for start_pos in 0..n {
            if !vertices.contains(&start_pos) {
                continue;
            }
            let Some(active_input) = input.get(start_pos..) else {
                continue;
            };
            let need_phrases = include_prefix_phrases && start_pos == 0;

            // ① 用户词（精确）
            if let Some(user_dict) = self.user_dict.as_ref().filter(|dict| dict.loaded()) {
                for len in 1..=active_input.len() {
                    let Some(key) = active_input.get(..len) else {
                        continue;
                    };
                    let consumed = consume_trailing_delimiters(len, active_input, &delimiters);
                    let end_pos = start_pos + consumed;
                    if cell_len(&graph, start_pos, end_pos) >= max_homographs {
                        continue;
                    }
                    let mut uter = UserDictEntryIterator::new();
                    user_dict.lookup_words(&mut uter, key, false, 0, None);
                    if self.filter_by_charset() {
                        uter.add_filter(Box::new(charset_filter::filter_dict_entry));
                    }
                    if uter.exhausted() {
                        continue;
                    }
                    let entries = drain_user_entries(&mut uter);
                    vertices.insert(end_pos);
                    let (cells, phrases) =
                        split_entries(entries, max_homographs, need_phrases, copy_for_phrases);
                    if !cells.is_empty() {
                        graph.entry(start_pos).or_default().insert(end_pos, cells);
                    }
                    if !phrases.is_empty() {
                        user_phrases.insert(consumed, phrases);
                    }
                }
            }

            // ② 构造词（编码器算出来的码），只在格子还空着时补
            if let Some(encoder) = self.encoder.as_ref().filter(|encoder| encoder.loaded()) {
                let user_dict = self.user_dict.as_ref();
                for len in 1..=active_input.len() {
                    let Some(key) = active_input.get(..len) else {
                        continue;
                    };
                    let consumed = consume_trailing_delimiters(len, active_input, &delimiters);
                    let end_pos = start_pos + consumed;
                    if cell_len(&graph, start_pos, end_pos) > 0 {
                        continue;
                    }
                    let Some(user_dict) = user_dict else {
                        break;
                    };
                    let mut uter = UserDictEntryIterator::new();
                    encoder.lookup_phrases(user_dict, &mut uter, key, false, 0, None);
                    if self.filter_by_charset() {
                        uter.add_filter(Box::new(charset_filter::filter_dict_entry));
                    }
                    if uter.exhausted() {
                        continue;
                    }
                    let entries = drain_user_entries(&mut uter);
                    vertices.insert(end_pos);
                    let (cells, phrases) =
                        split_entries(entries, max_homographs, need_phrases, copy_for_phrases);
                    if !cells.is_empty() {
                        graph.entry(start_pos).or_default().insert(end_pos, cells);
                    }
                    if !phrases.is_empty() {
                        user_phrases.insert(consumed, phrases);
                    }
                }
            }

            // ③ 词表（prism 前缀命中，**倒序**处理：长的先连，格子先到先占）
            for m in self
                .dict
                .prism
                .common_prefix_search(active_input)
                .iter()
                .rev()
            {
                if m.length == 0 {
                    continue;
                }
                let Some(key) = active_input.get(..m.length) else {
                    continue;
                };
                let consumed = consume_trailing_delimiters(m.length, active_input, &delimiters);
                let end_pos = start_pos + consumed;
                if cell_len(&graph, start_pos, end_pos) >= max_homographs {
                    continue;
                }
                let mut iter = DictEntryIterator::new();
                self.dict
                    .lookup_words(&mut iter, key, false, 0, Some(&blacklist));
                if self.filter_by_charset() {
                    iter.add_filter(Box::new(charset_filter::filter_dict_entry));
                }
                if iter.exhausted() {
                    continue;
                }
                let entries = drain_table_entries(&mut iter);
                vertices.insert(end_pos);
                let (cells, phrases) =
                    split_entries(entries, max_homographs, need_phrases, copy_for_phrases);
                if !cells.is_empty() {
                    graph.entry(start_pos).or_default().insert(end_pos, cells);
                }
                if !phrases.is_empty() {
                    table_phrases.insert(consumed, phrases);
                }
            }
        }

        let mut out: Vec<Candidate> = Vec::new();

        // ④ 整句
        //
        // `preceding_text` 传空串：C++ 的 `GetPrecedingText` 在
        // `contextual_suggestions_ == false` 时直接返回空串，而出厂 124 个方案
        // 没有一个开这个开关。
        if let Some(sentence) = self.poet.make_sentence(&graph, n, "") {
            let mut preedit = sentence_preedit(input, &sentence, &delimiters);
            self.options.format_preedit(&mut preedit);
            let mut candidate = Candidate::new(
                "sentence",
                0,
                sentence.end_pos,
                sentence.text.clone(),
                SENTENCE_BASE + sentence.weight,
            )
            .with_comment(K_UNITY_SYMBOL)
            .with_preedit(preedit);
            // C++ 的 `Sentence` 自带一个累加出来的词条；有它，提交整句才能进用户词典。
            if let Some(entry) = sentence_entry(&graph, &sentence) {
                candidate = candidate.with_entry(entry);
            }
            out.push(candidate);
        }

        // ⑤ 手选首词：两条流按码长降序归并，同长度用户词优先
        let user_stream = flatten_desc(&user_phrases);
        let table_stream = flatten_desc(&table_phrases);
        let (mut user_index, mut table_index) = (0usize, 0usize);
        let mut rank = 0usize;
        while user_index < user_stream.len() || table_index < table_stream.len() {
            let user_len = user_stream.get(user_index).map_or(0, |item| item.0);
            let table_len = table_stream.get(table_index).map_or(0, |item| item.0);
            let prefer_user = user_len > 0 && user_len >= table_len;
            let (code_length, entry, is_user) = if prefer_user {
                let item = user_stream[user_index].clone();
                user_index += 1;
                (item.0, item.1, true)
            } else {
                let item = table_stream[table_index].clone();
                table_index += 1;
                (item.0, item.1, false)
            };
            rank += 1;
            let quality = SENTENCE_BASE - rank as f64 * PHRASE_STEP;
            let candidate = self.phrase_candidate(&entry, is_user, code_length, input, quality);
            out.push(candidate);
        }
        out
    }

    /// `SentenceTranslation::Peek` 的 `Phrase` 分支：候选覆盖首段 `[0, code_length)`。
    ///
    /// 注释**不过** `comment_formatter` —— C++ 这一路是 `Phrase` 构造器直接
    /// `set_comment(entry->comment)`，与词级候选（`TableTranslation::Peek` 会过格式化）
    /// 不同。照抄。
    fn phrase_candidate(
        &mut self,
        entry: &DictEntry,
        is_user: bool,
        code_length: usize,
        input: &str,
        quality: f64,
    ) -> Candidate {
        let mut preedit = input.get(..code_length).unwrap_or(input).to_string();
        self.options.format_preedit(&mut preedit);
        Candidate::new(
            if is_user { "user_table" } else { "table" },
            0,
            code_length,
            entry.text.clone(),
            quality,
        )
        .with_comment(entry.comment.clone())
        .with_preedit(preedit)
        .with_entry(entry.clone())
    }
}

/// `consume_trailing_delimiters`（`table_translator.cc:521-527`）：
/// 码后面紧跟的分隔符（`;` 之类）一起吃进这一段。
fn consume_trailing_delimiters(pos: usize, input: &str, delimiters: &str) -> usize {
    let mut pos = pos;
    while let Some(ch) = input.get(pos..).and_then(|rest| rest.chars().next()) {
        if !delimiters.contains(ch) {
            break;
        }
        pos += ch.len_utf8();
    }
    pos
}

/// 图里 `graph[start][end]` 已经收了几条。
fn cell_len(graph: &WordGraph, start: usize, end: usize) -> usize {
    graph
        .get(&start)
        .and_then(|edges| edges.get(&end))
        .map_or(0, Vec::len)
}

/// 把一个迭代器里剩下的词条全倒出来（急切化的代价，条数很少）。
fn drain_user_entries(iter: &mut UserDictEntryIterator) -> Vec<DictEntry> {
    let mut entries = Vec::new();
    if iter.exhausted() {
        return entries;
    }
    if let Some(entry) = iter.peek() {
        entries.push(entry.clone());
    }
    while iter.next() {
        if let Some(entry) = iter.peek() {
            entries.push(entry.clone());
        }
    }
    entries
}

/// 同 [`drain_user_entries`]，词表侧。
fn drain_table_entries(iter: &mut DictEntryIterator) -> Vec<DictEntry> {
    let mut entries = Vec::new();
    if iter.exhausted() {
        return entries;
    }
    if let Some(entry) = iter.peek() {
        entries.push(entry.clone());
    }
    while iter.next() {
        if let Some(entry) = iter.peek() {
            entries.push(entry.clone());
        }
    }
    entries
}

/// 按 `collect_entries` 的消费顺序切成"进图的"和"留给手选首词的"。
///
/// C++ 里 `max_homographs_ > 1` 时用副本建图，所以手选候选从第 1 条开始；
/// 否则建图已经把前 `max_homographs_` 条吃掉了，手选候选从剩下的开始。
fn split_entries(
    entries: Vec<DictEntry>,
    max_homographs: usize,
    need_phrases: bool,
    copy_for_phrases: bool,
) -> (Vec<DictEntry>, Vec<DictEntry>) {
    let take = max_homographs.min(entries.len());
    let cells = entries[..take].to_vec();
    if !need_phrases {
        return (cells, Vec::new());
    }
    let phrases = if copy_for_phrases {
        entries
    } else {
        entries[take..].to_vec()
    };
    (cells, phrases)
}

/// 码长降序展开成扁平列表（C++ 对 collector 反复 `rbegin()` 的效果）。
fn flatten_desc(collected: &BTreeMap<usize, Vec<DictEntry>>) -> Vec<(usize, DictEntry)> {
    let mut out = Vec::new();
    for (code_length, entries) in collected.iter().rev() {
        for entry in entries {
            out.push((*code_length, entry.clone()));
        }
    }
    out
}

/// `SentenceTranslation::PrepareSentence` 的 preedit：词的边界插空格，
/// 但前一个字符本身就是分隔符时不插（否则会出现 `ni ; hao`）。
fn sentence_preedit(input: &str, sentence: &Sentence, delimiters: &str) -> String {
    let mut preedit = input.to_string();
    let mut pos = 0usize;
    let mut previous_end = 0usize;
    for (end, _, _) in &sentence.segments {
        if pos > 0
            && !preedit
                .as_bytes()
                .get(pos - 1)
                .is_some_and(|byte| delimiters.contains(*byte as char))
        {
            preedit.insert(pos, ' ');
            pos += 1;
        }
        pos += end - previous_end;
        previous_end = *end;
    }
    preedit
}

/// C++ `Sentence::Extend` 累加出来的那个词条：文本拼起来、码接起来、权重是整句分。
///
/// 提交整句时会走 `Memorize`，靠它把整句写进用户词典（否则整句只能看不能学）。
/// 用"段的文本"在词图对应格子里反查词条 —— `Sentence` 只带 `(end_pos, text, weight)`，
/// 没带词条本体，同格同文本的多条（不同码）取第一条。
fn sentence_entry(graph: &WordGraph, sentence: &Sentence) -> Option<DictEntry> {
    if sentence.segments.is_empty() {
        return None;
    }
    let mut code = Code::new();
    let mut previous_end = 0usize;
    for (end, text, _) in &sentence.segments {
        let entry = graph
            .get(&previous_end)?
            .get(end)?
            .iter()
            .find(|entry| &entry.text == text)?
            .clone();
        for syllable in entry.code.iter() {
            code.push(*syllable);
        }
        previous_end = *end;
    }
    Some(DictEntry {
        text: sentence.text.clone(),
        code,
        weight: sentence.weight,
        ..Default::default()
    })
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

    /// 码后面紧跟的分隔符一起吃进这一段（`consume_trailing_delimiters`）。
    #[test]
    fn trailing_delimiters_are_consumed() {
        let delimiters = " ;'";
        assert_eq!(consume_trailing_delimiters(2, "wqvb", delimiters), 2, "没有分隔符");
        assert_eq!(consume_trailing_delimiters(2, "wq;vb", delimiters), 3, "吃一个 ;");
        assert_eq!(consume_trailing_delimiters(2, "wq;; vb", delimiters), 5, "连着吃");
        assert_eq!(consume_trailing_delimiters(3, "wq;", delimiters), 3, "越过末尾就停");
    }

    /// `max_homographs == 1` 时建图吃掉第 1 条，手选首词从第 2 条起
    /// （C++ 里 `max_homographs_ > 1` 才用副本，照抄这个分支差异）。
    #[test]
    fn first_entry_goes_to_graph_when_one_homograph() {
        let entries = vec![
            entry("甲", 3.0, 0, ""),
            entry("乙", 2.0, 0, ""),
            entry("丙", 1.0, 0, ""),
        ];
        let (cells, phrases) = split_entries(entries, 1, true, false);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].text, "甲", "图里每格只留最优");
        let texts: Vec<&str> = phrases.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["乙", "丙"], "手选候选从第 2 条起");
    }

    /// `max_homographs > 1` 时建图用副本，手选候选从第 1 条起。
    #[test]
    fn phrases_start_from_first_entry_when_many_homographs() {
        let entries = vec![entry("甲", 3.0, 0, ""), entry("乙", 2.0, 0, "")];
        let (cells, phrases) = split_entries(entries, 2, true, true);
        assert_eq!(cells.len(), 2, "两格都进图");
        assert_eq!(phrases.len(), 2, "手选候选不从第 2 条起");
        assert_eq!(phrases[0].text, "甲");
    }

    /// 不要手选候选时，剩下的词条直接丢掉。
    #[test]
    fn phrases_are_dropped_when_not_needed() {
        let entries = vec![entry("甲", 3.0, 0, ""), entry("乙", 2.0, 0, "")];
        let (cells, phrases) = split_entries(entries, 1, false, false);
        assert_eq!(cells.len(), 1);
        assert!(phrases.is_empty());
    }

    /// 整句 preedit 在词边界插空格，但边界前是分隔符时不插
    /// （`SentenceTranslation::PrepareSentence`）。
    #[test]
    fn sentence_preedit_inserts_spaces_at_word_boundaries() {
        let sentence = Sentence {
            text: "你好".to_string(),
            end_pos: 4,
            weight: 0.0,
            segments: vec![(2, "你".to_string(), 0.0), (4, "好".to_string(), 0.0)],
        };
        assert_eq!(sentence_preedit("wqvb", &sentence, " ;'"), "wq vb");

        // 第二段前面是 `;`（分隔符）⇒ 不再插空格
        let sentence = Sentence {
            text: "你好".to_string(),
            end_pos: 5,
            weight: 0.0,
            segments: vec![(3, "你".to_string(), 0.0), (5, "好".to_string(), 0.0)],
        };
        assert_eq!(sentence_preedit("wq;vb", &sentence, " ;'"), "wq;vb");
    }
}
