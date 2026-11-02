//! `src/rime/translator.h` + `gear/script_translator.cc`：`Translator` trait + 拼音翻译器。

use crate::algo::syllabifier::{SyllableGraph, Syllabifier};
use crate::dict::corrector::EditDistanceCorrector;
use crate::dict::dictionary::Dictionary;
use crate::dict::user_dictionary::UserDictionary;
use crate::dict::vocabulary::{Code, DictEntry, SyllableId};
use crate::engine::candidate::Candidate;
use crate::engine::commit_history::CommitHistory;
use crate::engine::gear::poet::{Poet, Sentence, WordGraph};
use crate::engine::gear::translation::Translation as TranslationTrait;
use crate::engine::gear::translator_options::TranslatorOptions;
use crate::engine::segmentation::Segment;

/// 候选质量的统一尺度，对齐 C++ `ScriptTranslation::Evaluate` 的结构序：
///
/// ```text
/// quality = 码长(音节数) × 1e12 + 层级 × 1e9 + weight(对数尺度)
/// 层级:整句 2 > 用户词典 1 > 系统词表 0(C++ PrepareCandidate 的分支顺序)
/// ```
///
/// C++ 里"整句/用户词优先"靠 `PrepareCandidate` 的分支顺序、"长码优先"靠
/// `DictEntryCollector` 按 code_length 分桶 + `rbegin()` 最长先消费
/// （`script_translator.cc:479`）——都是**结构序**，与数值无关。LRN 的菜单是
/// 单一 `quality` 降序，所以把两级结构序都编码进分数：码长是主台阶
/// （1e12 远超任何 weight 摆动），同码长内层级定"学过的最优先"。
const QUALITY_SYLLABLE_STRIDE: f64 = 1.0e12;
const QUALITY_TIER_SENTENCE: f64 = 2.0e9;
const QUALITY_TIER_USER: f64 = 1.0e9;

/// 翻译器（对照 C++ `Translator::Query(input, segment)`）。
///
/// 传整段 `Segment` 而不是只传 `start` + `tags`：C++ 的 `Query` 拿的是
/// `const Segment*`，`reverse_lookup_translator` 会 `const_cast` 往里写
/// `prompt`（"usually translators do not modify the segment directly; prompt text
/// is best set by a processor or a segmentor"，但它自己就是例外：打了反查前缀
/// `` ` `` 之后要在输入区显示〔笔画〕）。Rust 侧直接把 `&mut Segment` 交出去，
/// 比留个 `prompt` 旁路干净。
pub trait Translator {
    /// 每次翻译前同步一次运行期状态（C++ 里 translator 持 `Engine*`，随时能读 option）。
    ///
    /// lrn 的 `query` 只拿得到 `input` + `segment` + `history`，拿不到 `Context`，
    /// 所以需要读 option 的实现（目前只有 `punct_translator` 要 `full_shape`）
    /// 在这里刷新。默认实现什么都不做。
    fn sync_context(&mut self, _ctx: &crate::engine::context::Context) {}

    /// 本翻译器的候选是**兜底**性质：只有同一段的正经候选全空时才补上。
    ///
    /// C++ 靠 `Translation::Compare` 让翻译器之间互相窥视（`echo_translator` 的
    /// `EchoTranslation::Compare` 会检查 `other->exhausted()`），LRN 的翻译器各自
    /// 返回 `Vec` 后统一塞进 `Menu` 排序，没有这种协商通道。用一个显式标记让
    /// runtime 延后合并，语义与 C++ 等价。默认 `false`。
    fn is_fallback(&self) -> bool {
        false
    }

    /// 给定一段输入串，产出候选列表（位置**相对本段**，候选各带覆盖到的 `end`）。
    ///
    /// `history` 是本会话的提交历史（C++ 里通过 `engine_->context()` 拿到）。
    /// 只有 `history_translator` 会真的读它；其余实现忽略。
    fn query(
        &mut self,
        input: &str,
        segment: &mut Segment,
        history: &CommitHistory,
    ) -> Option<Box<dyn TranslationTrait>>;

    /// `Translator::Memorize(commit_entry)`：一次提交里选中的词条交回来学。
    ///
    /// C++ 在 `Context::commit()` 里把 `CommitEntry::elements` 播给每个 translator；
    /// 只有带用户词典的实现会真的写库，默认实现什么都不做。
    ///
    /// `history` 是本会话的提交历史：`table_translator` 的 `encode_commit_history`
    /// 要拿它把"连续几个词"拼成短语去编码（C++ 里同样是查 `engine_->context()`）。
    fn memorize(&mut self, _elements: &[DictEntry], _history: &CommitHistory) -> bool {
        false
    }
}

/// 拼音翻译器：把输入按音节图切分，逐切分查 `.table.bin` 得词条。
///
/// 词级：枚举整段输入的所有音节切分，对每个切分 `query_phrases(Code)`，
/// 词条 `end` = 该切分覆盖到的字节位置。这样「你」(单音节切分, end=2) 与「你好」
/// (双音节切分, end=4) 同台出现，逐音节确认行为自然成立。
pub struct ScriptTranslator {
    pub dict: Dictionary,
    pub options: TranslatorOptions,
    /// 组句器（`poet.cc`）。没有 grammar 时内部走动态规划，等价 C++ 的
    /// `Poet::MakeSentenceWithStrategy<DynamicProgramming>`。
    pub poet: Poet,
    /// 用户词典（`Translator::user_dict_`）。`None` = 方案没开 `enable_user_dict`
    /// 或者拿不到词典名。
    pub user_dict: Option<UserDictionary>,
    /// 纠错器（`translator/enable_correction: true` 时懒建）。
    ///
    /// 音节表来自 [`Dictionary::table`]，一次建好；每次 `build_graph` 只需要
    /// [`EditDistanceCorrector::clone`]（共享同一份音节表与查询缓存）。
    pub correction: Option<EditDistanceCorrector>,
}

impl ScriptTranslator {
    pub fn new(dict: Dictionary) -> Self {
        Self::with_options(dict, TranslatorOptions::default())
    }

    pub fn with_options(dict: Dictionary, options: TranslatorOptions) -> Self {
        Self {
            dict,
            options,
            poet: Poet::new(None),
            user_dict: None,
            correction: None,
        }
    }

    /// 挂上用户词典（`Translator::user_dict_`）。
    pub fn with_user_dict(mut self, user_dict: UserDictionary) -> Self {
        self.user_dict = Some(user_dict);
        self
    }

    /// 换上带语法模型的组句器（`Poet` 里 `grammar` 非空即走束搜索）。
    pub fn with_poet(mut self, poet: Poet) -> Self {
        self.poet = poet;
        self
    }

    /// 建纠错器（`translator/enable_correction: true` 时）。
    ///
    /// 幂等：没开开关、或已经建过就直接返回。音节表从 `Table` 拿 —— prism 那边
    /// 不暴露 syllabary（`dict/prism.h` 只有查词方法），而 `Table` 有。
    /// 拿不到音节表（词典没打开）时保持 `None`，纠错静默失效。
    pub fn ensure_correction(&mut self) {
        if !self.options.enable_correction || self.correction.is_some() {
            return;
        }
        let Some(syllabary) = self.dict.table.syllabary() else {
            return;
        };
        // ⚠️ id 必须是 **prism 的拼写 id**，不能是 table 的音节 id：
        // `syllabifier` 拿到 `tolerance_search` 的结果后直接拿它去
        // `prism.query_spelling(id)` 查属性（`syllabifier.cc:97-104`），
        // 所以这里逐条用 `prism.get_value` 把音节串翻成 prism 自己的 id。
        // 串从 `Table` 拿（prism 不暴露 syllabary，见 `EditDistanceCorrector` 的文档）。
        let mut pairs: Vec<(String, SyllableId)> = Vec::new();
        for (spelling, _syllable_id) in syllabary.syllable_to_id() {
            if let Some(spelling_id) = self.dict.prism.get_value(&spelling) {
                pairs.push((spelling.to_string(), spelling_id));
            }
        }
        if pairs.is_empty() {
            return;
        }
        self.correction = Some(EditDistanceCorrector::with_syllabary(
            self.options.correction_tolerance,
            pairs,
        ));
    }

    /// 构造候选的 preedit 串（对照 C++ `ScriptSyllabifier::GetPreeditString`）。
    ///
    /// 用 prism 把候选覆盖的输入重新切分成音节，用分隔符连接，
    /// 再套 `preedit_formatter`。切分失败时返回空串（候选 preedit 留空）。
    fn build_preedit(&self, input: &str, end: usize) -> String {
        let sub = &input[..end.min(input.len())];
        let n = sub.len();
        if n == 0 {
            return String::new();
        }
        // 每个起始位置能起的合法音节（只关心跨度，不关心音节 id）。
        let mut match_at: Vec<Vec<usize>> = vec![Vec::new(); n + 1];
        for start in 0..n {
            let suffix = &sub[start..];
            for m in self.dict.prism.common_prefix_search(suffix) {
                match_at[start].push(start + m.length);
            }
        }
        // DFS 找一条能精确覆盖到 `end` 的最短切分。
        let mut best: Vec<usize> = Vec::new();
        fn dfs(
            start: usize,
            end: usize,
            match_at: &[Vec<usize>],
            current: &mut Vec<usize>,
            best: &mut Vec<usize>,
        ) {
            if start == end {
                if best.is_empty() || current.len() < best.len() {
                    *best = current.clone();
                }
                return;
            }
            if start > end || current.len() > 12 {
                return;
            }
            for &next_pos in &match_at[start] {
                current.push(next_pos);
                dfs(next_pos, end, match_at, current, best);
                current.pop();
            }
        }
        dfs(0, n, &match_at, &mut Vec::new(), &mut best);
        if best.is_empty() {
            return String::new();
        }
        // 用分隔符连接各音节。
        let delimiter = self.options.delimiters.chars().next().unwrap_or(' ');
        let mut out = String::new();
        let mut prev = 0;
        for &pos in &best {
            if !out.is_empty() && !out.ends_with(delimiter) {
                out.push(delimiter);
            }
            out.push_str(&sub[prev..pos]);
            prev = pos;
        }
        out
    }

    /// 整句候选（`ScriptTranslation::Evaluate` 的 sentence 分支，
    /// `script_translator.cc:444-514`）。
    ///
    /// C++ 的三个前置条件全部照抄：
    ///
    /// 1. `has_at_least_two_syllables` —— `syllable_graph.edges.size() >= 2`
    ///    （只有一个起点的输入谈不上"组句"）；
    /// 2. 没有可靠词级候选 —— 若整段输入能命中一个 `IsExactMatch` 的词条
    ///    （比如 `nihao` → 「你好」），就不出整句；
    /// 3. 组句成功。
    ///
    /// # quality 换算（与 C++ 的差异）
    ///
    /// C++ 的候选顺序是**结构性**的：`PrepareCandidate` 先走 `kSentence` 分支，
    /// 整句天然排在词级前面，与 `quality` 无关。LRN 的
    /// [`crate::engine::menu`] 统一按 `quality` 降序排序，所以这里给整句叠一个
    /// 远大于词级量级（词级是 `音节数 * 1000 + weight`）的基数：
    /// 既保证整句排最前，又保留多条整句之间的相对顺序。
    ///
    /// 2026-10-31:去掉无人调用的 `sentence_candidates` 包装(query 内联构造),
    /// 它的候选坐标还是段内相对系(起点写死 0),affix 段会错位 —— 留着是坑。
    /// [`Self::best_sentence`] 保留(条件判定仍然用它)。

    /// 按 C++ 的前置条件算出一条最优整句；不满足条件时 `None`。
    fn best_sentence(&self, input: &str) -> Option<Sentence> {
        let (graph, consumed) = self.build_graph(input)?;
        // 条件 1：至少两个起点（`script_translator.cc:497`）
        if graph.edges.len() < 2 {
            return None;
        }
        let word_graph = self.prepare_word_graph(&graph, consumed);
        // 条件 2：有可靠词级候选就不组句（`script_translator.cc:502-508`）
        if self.has_reliable_phrase(&word_graph, consumed) || word_graph.is_empty() {
            return None;
        }
        // 条件 3：组句（`max_sentences_` 默认 1，出厂方案没配过）
        self.poet.make_sentence(&word_graph, consumed, "")
    }

    /// 构建音节图；`consumed` 是音节图能解释到的位置（`interpreted_length`）。
    fn build_graph(&self, input: &str) -> Option<(SyllableGraph, usize)> {
        let mut graph = SyllableGraph::default();
        let mut syllabifier = Syllabifier::with_options(
            &self.options.delimiters,
            self.options.enable_completion,
            false,
            None,
        );
        // 纠错：`enable_correction` 打开 + 纠错器已建（音节表来自 Table）。
        // 挂上后纠出来的拼写带 `is_correction` + `credibility = log(0.01)`，
        // 因此只会在真候选**后面**出现（Xime 说的"只追加、绝不替换"）。
        if let Some(corrector) = self.correction.as_ref() {
            syllabifier.enable_correction(Box::new(corrector.clone()));
        }
        let consumed = syllabifier.build_syllable_graph(input, &self.dict.prism, &mut graph);
        if consumed <= 0 {
            return None;
        }
        Some((graph, consumed as usize))
    }

    /// 覆盖 `[0, end]` 的一条音节路径 → `Code`（用户词典的学词键）。
    ///
    /// 整句候选的 `entry` 原本**没有 code**，于是 `Memorize` 写不进用户词典
    /// （`update_entry` 见空 code 就跳过）——用户上屏的整句永远学不会，下次打同样
    /// 一串还是整句候选压在前面，刚打过的词"又回到后面"（2026-11-01 实机报告）。
    ///
    /// 走法：每个起点取**跨度最大**的边（长音节优先，与 `Transpose` 的序一致），
    /// 直到走满 `end`。切分点优先用 `stops`（整句各成分的 end_pos，来自 poet），
    /// 这样学进去的切法与屏幕上看到的分词一致。
    fn code_for(&self, input: &str, end: usize, stops: &[usize]) -> Option<Code> {
        if end == 0 {
            return None;
        }
        let (graph, _consumed) = self.build_graph(input)?;
        let mut ids: Vec<crate::algo::syllabifier::SyllableId> = Vec::new();
        let mut pos = 0usize;
        while pos < end {
            // 优先落在成分边界上（poet 的分词点），否则取最长音节
            let target = stops
                .iter()
                .copied()
                .find(|s| *s > pos && *s <= end && graph.edges.get(&pos).map_or(false, |m| m.contains_key(s)))
                .or_else(|| {
                    graph.edges.get(&pos).and_then(|m| {
                        m.keys()
                            .filter(|e| **e <= end)
                            .max()
                            .copied()
                    })
                })?;
            let syllable_id = graph
                .edges
                .get(&pos)
                .and_then(|m| m.get(&target))
                .and_then(|sp| sp.keys().next().copied())?;
            ids.push(syllable_id);
            pos = target;
        }
        Some(Code::from_slice(&ids))
    }

    /// `PrepareForMakingSentence`（`script_translator.cc:705-722`）：
    /// 图里**每个起点**都查一次词表，合成 `WordGraph`。
    fn prepare_word_graph(
        &self,
        graph: &SyllableGraph,
        _consumed: usize,
    ) -> WordGraph {
        let starts: Vec<usize> = graph.edges.keys().copied().collect();
        let mut word_graph = WordGraph::new();
        for start in starts {
            let collector = self.dict.lookup_from_graph(
                graph,
                start,
                Some(&self.options.blacklist),
                self.options.enable_completion,
            );
            if !collector.is_empty() {
                word_graph.insert(start, collector);
            }
        }
        word_graph
    }

    /// `has_exact_match_phrase`（`script_translator.cc:478-482`）：
    /// `end_pos == consumed` 且首条词条是精确匹配。
    fn has_reliable_phrase(&self, word_graph: &WordGraph, consumed: usize) -> bool {
        word_graph
            .get(&0)
            .and_then(|edges| edges.get(&consumed))
            .and_then(|bucket| bucket.first())
            .is_some_and(|entry| entry.is_exact_match())
    }

    /// 取组句结果（多条），给 e2e 断言与调试用。
    pub fn make_sentences(&self, input: &str) -> Vec<Sentence> {
        let Some((graph, consumed)) = self.build_graph(input) else {
            return Vec::new();
        };
        let word_graph = self.prepare_word_graph(&graph, consumed);
        self.poet.make_sentences(&word_graph, consumed, "", 5, 0.1)
    }

    /// 用户词典候选（`script_translator.cc` 里 `user_dict_->Lookup(...)` 那段）。
    ///
    /// C++ 把用户词典词条塞进同一个 `ScriptTranslation`，靠 `credibility` 加成
    /// 顶到词表词条前面；LRN 用 [`USER_DICT_QUALITY_BASE`] 表达同一件事。
    ///
    /// `predict_word_from_depth` 传 0（`script_translator` 出厂方案不开预测），
    /// 与 C++ 的 `user_dict_->Lookup(syllable_graph, start, 0, 0)` 一致。
        fn user_dict_candidates(
        &mut self,
        input: &str,
        seg_start: usize,
        ngram_bonus: &dyn Fn(&str) -> f64,
    ) -> Vec<Candidate> {
        if self.user_dict.is_none() {
            return Vec::new();
        }
        // 先把音节图算出来：`build_graph` 借 `&self`，不能和 `user_dict` 的可变借用共存
        let Some((graph, _consumed)) = self.build_graph(input) else {
            return Vec::new();
        };
        let Some(user_dict) = self.user_dict.as_mut() else {
            return Vec::new();
        };
        if !user_dict.loaded() {
            return Vec::new();
        }
        let Some(collector) = user_dict.lookup(&self.dict, &graph, 0, 0, 0, 0.0) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (end, iterator) in collector {
            for entry in iterator.into_entries().0 {
                if self.options.blacklist.contains(&entry.text) {
                    continue;
                }
                // 同上（translation.rs 的 peek）：删过的词直接消失，不靠压分。
                if crate::engine::user_ngram::is_blocked_global(&entry.text) {
                    continue;
                }
                let quality =
                    entry.code.len() as f64 * QUALITY_SYLLABLE_STRIDE + QUALITY_TIER_USER + entry.weight;
                // 收集器键是段内相对 end;候选跨度补成绝对坐标。
                let mut candidate = Candidate::new(
                    "script",
                    seg_start,
                    seg_start + end,
                    entry.text.clone(),
                    quality + ngram_bonus(&entry.text),
                )
                .with_entry(entry);
                let preedit = self.build_preedit(input, end);
                if !preedit.is_empty() {
                    candidate = candidate.with_preedit(preedit);
                }
                out.push(candidate);
            }
        }
        // 用户词典候选按质量降序(2026-11-01):lookup 顺序≠质量序,
        // 不排序会把高频单字顶在多字词组前面(实机:打过十几遍的词组
        // 被两个 1e12 的单字压住)。整句让位判据也依赖这里的顺序。
        out.sort_by(|a, b| {
            b.quality
                .partial_cmp(&a.quality)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out
    }
}

impl Translator for ScriptTranslator {
    fn query(
        &mut self,
        input: &str,
        segment: &mut Segment,
        history: &CommitHistory,
    ) -> Option<Box<dyn TranslationTrait>> {
        if !self.options.has_any_tag(&segment.tags) {
            return None;
        }
        // 纠错器懒建一次（`enable_correction` 没开时什么都不做）
        self.ensure_correction();

        // ---- 层 1(先算):用户词典——命中整段就不出整句 ------------------
        // C++ `IsExactMatch` 的判据含用户词典词条:打过十几遍的词组再打,
        // 整句必须让位,否则学的永远上不了首选(用户实机报告的根因)。
        let full_len = input.len();
        let user = self.user_dict_candidates(input, segment.start, &|_| 0.0);
        let user_covers_all = user.iter().any(|c| c.end - c.start == full_len);

        // ---- 层 0:整句(poet 组句,最多一条) ------------------------------
        let mut tiers: Vec<Box<dyn TranslationTrait>> = Vec::new();
        if !user_covers_all
            && let Some(sentence) = self.best_sentence(input)
        {
            let consumed = sentence.end_pos;
            let preedit = self.build_preedit(input, consumed);
            // 学词键：整句覆盖 [0, consumed] 的音节路径；切分点用 poet 的成分边界。
            let stops: Vec<usize> = sentence
                .components()
                .iter()
                .map(|(end_pos, _, _)| *end_pos)
                .collect();
            let sentence_code = self.code_for(input, consumed, &stops);
            let mut candidate = Candidate::new(
                "script",
                segment.start,
                segment.start + consumed,
                sentence.text.clone(),
                sentence.segments.len() as f64 * QUALITY_SYLLABLE_STRIDE
                    + QUALITY_TIER_SENTENCE
                    + sentence.weight
                    + self.options.initial_quality,
            )
            .with_entry(crate::dict::vocabulary::DictEntry {
                text: sentence.text.clone(),
                code: sentence_code.unwrap_or_default(),
                ..Default::default()
            })
            .with_components(
                sentence
                    .components()
                    .iter()
                    .map(|(_, text, _)| text.clone())
                    .collect(),
            );
            if !preedit.is_empty() {
                candidate = candidate.with_preedit(preedit);
            }
            tiers.push(Box::new(crate::engine::gear::translation::VecTranslation::new(
                vec![candidate],
            )));
        }

        // 个人 n-gram 加成(青简):上文最后两个"词样"记录 → 词级候选加分。
        // 只加不减、没学过=0,不改变候选集合;整句候选(层 0)不吃这个。
        let (ngram_before, ngram_prev) =
            crate::engine::user_ngram::last_two_words(history);
        let ngram_bonus = |text: &str| -> f64 {
            crate::engine::user_ngram::global().lock().ok().map_or(0.0, |ngram| {
                ngram.bonus(ngram_before.as_deref(), ngram_prev.as_deref(), text, crate::engine::user_ngram::DEFAULT_WEIGHT)
            })
        };

        // ---- 层 1:用户词典 ------------------------------------------------
        let user = self.user_dict_candidates(input, segment.start, &ngram_bonus);
        if !user.is_empty() {
            tiers.push(Box::new(crate::engine::gear::translation::VecTranslation::new(
                user,
            )));
        }

        // ---- 层 2:词表懒迭代(C++ ScriptTranslation 的 kSysPhrase 分支) --
        let full_len = input.len();
        // 词图/收集器都是**段内相对坐标**(0 基);候选的绝对跨度由
        // `segment.start` 在流内补上。之前这里传绝对 start 给
        // `lookup_from_graph`,段起点非 0(affix/内联段)时查空。
        let graph = self.build_graph(input).map(|(g, _)| g).unwrap_or_default();
        let collector = self.dict.lookup_from_graph(&graph, 0, None, false);
        // preedit 预计算:懒流拿不到 Projection(非 Clone),对**去重后的 end**
        // 各算一次格式化串(一屏候选 end 至多几个),流内查表。键 = 绝对 end。
        let ends: Vec<usize> = collector.keys().copied().collect();
        let mut preedit_by_end = std::collections::HashMap::with_capacity(ends.len());
        for end in ends {
            let mut preedit = self.build_preedit(input, end);
            if !preedit.is_empty() {
                self.options.format_preedit(&mut preedit);
                preedit_by_end.insert(segment.start + end, preedit);
            }
        }
        tiers.push(Box::new(crate::engine::gear::translation::ScriptWordsTranslation::new(
            collector,
            segment.start,
            full_len,
            QUALITY_SYLLABLE_STRIDE,
            self.options.initial_quality,
            None,
            preedit_by_end,
            Some((ngram_before.clone(), ngram_prev.clone())),
        )));

        Some(Box::new(crate::engine::gear::translation::TieredTranslation::new(tiers)))
    }

    /// `ScriptTranslator::Memorize`：把这次提交里选中的词条逐条
    /// `UpdateEntry(entry, 1)` 写进用户词典(懒化重构时曾随 stash 丢失,2026-10-31 恢复)。
    fn memorize(&mut self, elements: &[DictEntry], _history: &CommitHistory) -> bool {
        let Some(user_dict) = self.user_dict.as_mut() else {
            return false;
        };
        if !user_dict.loaded() || user_dict.readonly() {
            return false;
        }
        let mut written = false;
        for entry in elements {
            if user_dict.update_entry(&self.dict, entry, 1) {
                written = true;
            }
        }
        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实词典夹具（`luna_pinyin.table.bin` + `luna_pinyin_simp.prism.bin`），
    /// 缺夹具时静默跳过 —— 与其它真实数据测试同风格。
    fn real_dict() -> Option<Dictionary> {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/e2e/build");
        Dictionary::open(&dir, "luna_pinyin", "luna_pinyin_simp")
    }

    #[test]
    fn correction_config_defaults_off_and_reads_from_schema() {
        assert!(
            !TranslatorOptions::default().enable_correction,
            "纠错默认关（与上游一致：要方案作者主动开）"
        );
        let config: serde_json::Value = serde_json::from_str(
            r#"{"translator": {"enable_correction": true, "correction_tolerance": 2}}"#,
        )
        .unwrap();
        let options = TranslatorOptions::from_config(&config, "translator");
        assert!(options.enable_correction);
        assert_eq!(options.correction_tolerance, 2);
        // 容差有个上限，配得再大也不让噪声无限膨胀
        let config: serde_json::Value =
            serde_json::from_str(r#"{"translator": {"correction_tolerance": 99}}"#).unwrap();
        assert!(TranslatorOptions::from_config(&config, "translator").correction_tolerance <= 3);
    }

    /// 端到端：把「nihao」打成「nihap」（`o` 敲到邻键 `p`）时，开了纠错整串能被
    /// 音节图解释完；关着就只能解释到一半。
    #[test]
    fn correction_saves_a_mistyped_syllable() {
        let Some(dict) = real_dict() else {
            return;
        };
        let input = "nihap";
        // `Dictionary` 不 `Clone`，两遍各自打开一次
        let Some(dict) = real_dict() else {
            return;
        };
        let hao = syllable_id(&dict, "hao");
        assert!(
            !edges_have(
                &ScriptTranslator::new(dict).build_graph(input).unwrap().0,
                2,
                5,
                hao
            ),
            "关着纠错时「hap」不该被解释成「hao」"
        );

        let Some(dict) = real_dict() else {
            return;
        };
        let mut options = TranslatorOptions::default();
        options.enable_correction = true;
        let mut translator = ScriptTranslator::with_options(dict, options);
        translator.ensure_correction();
        let Some(corrector) = translator.correction.as_ref() else {
            panic!("开了 enable_correction 就该建出纠错器（音节表来自 Table）");
        };
        assert!(corrector.syllabary_len() > 0, "音节表不该为空");
        // id 用 **table 的音节 id**：图里 `spellings` 的键是 `query_spelling` 出来的
        // syllable_id，而不是 prism 的 spelling_id（`tolerance_search` 返回的那个）。
        let hao = syllable_id(&translator.dict, "hao");
        let (g, _) = translator.build_graph(input).unwrap();
        assert!(
            edges_have(&g, 2, 5, hao),
            "「nihap」的 `hap` 应被纠成 `hao`（p 是 o 的邻键）"
        );
        // 反过来：不是邻键的替换不该被当成误触
        let mao = syllable_id(&translator.dict, "mao");
        if let Some(mao) = mao {
            let (g, _) = translator.build_graph("nihaz").unwrap();
            assert!(!edges_have(&g, 2, 5, Some(mao)), "「z」不是「o」的邻键，不该纠");
        }
    }

    /// `Table` 里某个拼写对应的**音节 id**。
    fn syllable_id(dict: &Dictionary, spelling: &str) -> Option<SyllableId> {
        dict.table
            .syllabary()?
            .syllable_to_id()
            .into_iter()
            .find(|(text, _)| *text == spelling)
            .map(|(_, id)| id)
    }

    /// 音节图里 `start → end` 这条边上有没有 `syllable` 这个音节。
    fn edges_have(
        graph: &SyllableGraph,
        start: usize,
        end: usize,
        syllable: Option<SyllableId>,
    ) -> bool {
        let Some(syllable) = syllable else {
            return false;
        };
        graph
            .edges
            .get(&start)
            .and_then(|ends| ends.get(&end))
            .is_some_and(|spellings| spellings.contains_key(&syllable))
    }

    /// 纠错不能抢位：`syllabifier` 给纠出来的拼写打 `is_correction` +
    /// `credibility = log(0.01)`，所以「打错才有的候选」一定比「打对就有的」低。
    #[test]
    fn corrected_spellings_are_penalized() {
        let Some(dict) = real_dict() else {
            return;
        };
        let mut options = TranslatorOptions::default();
        options.enable_correction = true;
        let mut translator = ScriptTranslator::with_options(dict, options);
        translator.ensure_correction();
        // 打对的：整串可解释，且没有一条边是 correction
        let (good, _) = translator.build_graph("nihao").expect("「nihao」应能建图");
        assert!(
            !has_correction_edge(&good),
            "打对时不该出现纠错边"
        );
        // 打错的：纠错边存在，且 credibility 是惩罚值
        let (bad, _) = translator.build_graph("nihap").expect("「nihap」应能建图");
        assert!(has_correction_edge(&bad), "打错时应出现纠错边");
    }

    /// 判据用 `credibility`：纠错的拼写被罚 `log(0.01)`（`is_correction` 是
    /// `algo` 模块内可见的字段，这里只能看分数）。
    fn has_correction_edge(graph: &SyllableGraph) -> bool {
        graph
            .edges
            .values()
            .flat_map(|ends| ends.values())
            .flat_map(|spellings| spellings.values())
            .any(|props| {
                props.credibility <= crate::algo::syllabifier::K_CORRECTION_CREDIBILITY
            })
    }
}
