//! `src/rime/translator.h` + `gear/script_translator.cc`：`Translator` trait + 拼音翻译器。

use crate::algo::syllabifier::{SyllableGraph, Syllabifier};
use crate::dict::dictionary::Dictionary;
use crate::engine::candidate::Candidate;
use crate::engine::gear::poet::{Poet, Sentence, WordGraph};
use crate::engine::gear::translator_options::TranslatorOptions;

/// 整句候选在 `quality` 上的基数，远大于词级的 `音节数 * 1000 + weight` 量级。
///
/// C++ 靠 `PrepareCandidate` 的分支顺序让整句排最前；LRN 的菜单统一按 `quality`
/// 降序排，所以把"整句优先"编码进分数里。见 [`ScriptTranslator::sentence_candidates`]。
const SENTENCE_QUALITY_BASE: f64 = 1.0e6;

/// 翻译器（对照 C++ `Translator::Query(input, segment)`）。
///
/// `seg_tags` 是本段命中的标签集：翻译器用它决定是否认领这段
/// （C++ 里是 `segment.HasAnyTagIn(tags_)`，tags 来自 [`TranslatorOptions`]）。
pub trait Translator {
    /// 给定一段输入串，产出候选列表（位置**相对本段**，候选各带覆盖到的 `end`）。
    fn query(&mut self, input: &str, start: usize, seg_tags: &[String]) -> Vec<Candidate>;
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
        }
    }

    /// 换上带语法模型的组句器（`Poet` 里 `grammar` 非空即走束搜索）。
    pub fn with_poet(mut self, poet: Poet) -> Self {
        self.poet = poet;
        self
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
    fn sentence_candidates(&mut self, input: &str) -> Vec<Candidate> {
        let Some(sentence) = self.best_sentence(input) else {
            return Vec::new();
        };
        let consumed = sentence.end_pos;
        let preedit = self.build_preedit(input, consumed);
        let mut candidate = Candidate::new(
            "script",
            0,
            consumed,
            sentence.text,
            SENTENCE_QUALITY_BASE + sentence.weight + self.options.initial_quality,
        );
        if !preedit.is_empty() {
            candidate = candidate.with_preedit(preedit);
        }
        self.options.format_preedit(&mut candidate.preedit);
        vec![candidate]
    }

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
        let consumed = syllabifier.build_syllable_graph(input, &self.dict.prism, &mut graph);
        if consumed <= 0 {
            return None;
        }
        Some((graph, consumed as usize))
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
}

impl Translator for ScriptTranslator {
    fn query(
        &mut self,
        input: &str,
        _start: usize,
        seg_tags: &[String],
    ) -> Vec<Candidate> {
        if !self.options.has_any_tag(seg_tags) {
            return Vec::new();
        }
        let mut out: Vec<Candidate> = self.sentence_candidates(input);
        out.extend(
            self.dict
                .translate(input)
                .into_iter()
                .filter(|e| !self.options.blacklist.contains(&e.text))
                .map(|e| {
                    let mut c = Candidate::new(
                        "script",
                        0,
                        e.end,
                        e.text,
                        e.quality + self.options.initial_quality,
                    );
                    if !e.comment.is_empty() {
                        c = c.with_comment(e.comment);
                    }
                    // 构造 preedit 串并套 preedit_formatter。
                    let preedit = self.build_preedit(input, e.end);
                    if !preedit.is_empty() {
                        c = c.with_preedit(preedit);
                    }
                    c
                }),
        );
        for c in &mut out {
            self.options.format_comment(&mut c.comment);
            self.options.format_preedit(&mut c.preedit);
        }
        out
    }
}
