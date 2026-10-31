//! `src/rime/gear/poet.{h,cc}`：组句器（句级搜索）。
//!
//! # 它解决什么
//!
//! 翻译器在音节图上查出来的都是**词级**候选（「你」「你好」「世界」…），
//! 但用户敲 `nihaoshijie` 想要的是「你好世界」这个**整句**。组句器把
//! 「词条 + 覆盖区间」组成的图（[`WordGraph`]）走一遍，选出一条总分最高的路径。
//!
//! # 两种策略（`poet.cc:246-253`）
//!
//! * **无 grammar** → 动态规划（[`DynamicProgramming`]）：每个 `end_pos` 只留一条最优路径。
//! * **有 grammar** → 束搜索（[`BeamSearch`]）：每个 `end_pos` 按「最后一个词」分槽，
//!   保留至多 `kMaxLineCandidates = 7` 条 —— 因为最后一词不同，下一个词配上去的
//!   语法分也不同，只留一条会把好句子提前剪掉。
//!
//! # 与 C++ 的实现差异
//!
//! C++ 用 `const Line*` 串起前驱链（靠 `std::map` 节点地址稳定）；Rust 这里用
//! **arena + 索引**（[`Arena::lines`] 只增不改），等价且没有生命周期纠缠。
//! 词条本身不持有指针，只把 `text` / `weight` 复制进 `Line` —— C++ 最后
//! `Sentence::Extend(*c->entry, ...)` 需要的也只是这两样。

use std::collections::{BTreeMap, HashMap};

use crate::dict::vocabulary::DictEntry;

/// `using WordGraph = map<int, map<int, DictEntryList>>`（`poet.h:20`）。
///
/// `graph[start_pos][end_pos]` = 覆盖 `[start_pos, end_pos)` 的所有词条。
pub type WordGraph = BTreeMap<usize, BTreeMap<usize, Vec<DictEntry>>>;

/// `Grammar::Evaluate` 里"没有语法模型"时的常量（`gear/grammar.h:27`）：`log(1e-6)`。
pub const K_PENALTY: f64 = -13.815_510_557_964_274;

/// `class Grammar`（`gear/grammar.h:11-38`）—— 语法模型只暴露这一个方法。
pub trait Grammar: Send + Sync {
    /// 给定前文与当前词，返回该词在此处的对数概率（越大越通顺）。
    fn query(&self, context: &str, word: &str, is_rear: bool) -> f64;
}

/// 语法分系数（`WEASEL_GRAMMAR_SCALE`，默认 1.0）。进程内只读一次环境。
fn grammar_scale() -> f64 {
    static SCALE: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *SCALE.get_or_init(|| {
        std::env::var("WEASEL_GRAMMAR_SCALE")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(1.0)
    })
}

/// `Grammar::Evaluate`（`gear/grammar.h:23-36`）：词条权重 + 语法分。
pub fn evaluate(
    context: &str,
    word: &str,
    entry_weight: f64,
    is_rear: bool,
    grammar: Option<&dyn Grammar>,
) -> f64 {
    let grammar_score = match grammar {
        Some(g) => g.query(context, word, is_rear),
        None => K_PENALTY,
    };
    // 临时实验：语法分的权重系数（`WEASEL_GRAMMAR_SCALE`，默认 1.0）。
    // 用来验证「模型语料偏古/偏专业时，语法分压过词频」这个假设。
    let grammar_score = grammar_score * grammar_scale();
    // 排障开关：打印每一次词格打分的三段（词频 / 语法 / 合计），排查「明明该选 A 却选 B」。
    if std::env::var_os("WEASEL_POET_TRACE").is_some() {
        eprintln!(
            "[poet] ctx={context:?} word={word:?} rear={is_rear} entry={entry_weight:.4} grammar={grammar_score:.4} total={:.4}",
            entry_weight + grammar_score
        );
    }
    entry_weight + grammar_score
}

/// 一条路径（`struct Line`，`poet.cc:21-70`）。
///
/// `predecessor` 是 arena 索引而非指针：`None` 只在 `k_empty` 上出现。
#[derive(Clone, Debug, Default)]
struct Line {
    predecessor: Option<usize>,
    /// 本节点的词（`Line::last_word()`）。
    text: String,
    /// 走到本节点时覆盖到的输入位置。
    end_pos: usize,
    /// 到本节点为止的累计得分。
    weight: f64,
    /// 整句文本哈希，`MakeSentences` 用来去重（`poet.cc:289-292`）。
    text_hash: u64,
}

/// `kEmpty`（`poet.cc:72`）：前驱与词都为空的那条线。
const K_EMPTY: usize = 0;

/// 路径竞技场：`lines` 只增不改，索引即稳定引用。
#[derive(Debug)]
struct Arena {
    lines: Vec<Line>,
}

impl Arena {
    fn new() -> Self {
        Self {
            lines: vec![Line::default()],
        }
    }

    fn get(&self, index: usize) -> &Line {
        &self.lines[index]
    }

    fn push(&mut self, line: Line) -> usize {
        self.lines.push(line);
        self.lines.len() - 1
    }

    /// `Line::empty()`（`poet.cc:33`）：`!predecessor && !entry`。
    fn is_empty(&self, index: usize) -> bool {
        self.lines[index].predecessor.is_none() && self.lines[index].text.is_empty()
    }

    /// `Line::context()`（`poet.cc:53-59`）：回看两个词拼成的字符串。
    ///
    /// C++ 里 `predecessor->last_word() + last_word()` 是**直接相邻**拼接
    /// （中间没有空格），这里保持一致。
    fn context(&self, index: usize) -> String {
        if self.is_empty(index) {
            return String::new();
        }
        let line = &self.lines[index];
        match line.predecessor {
            Some(p) if !self.is_empty(p) => {
                let mut s = self.lines[p].text.clone();
                s.push_str(&line.text);
                s
            }
            _ => line.text.clone(),
        }
    }

    /// `Line::components()`（`poet.cc:37-51`）：从首词到末词的 arena 索引（正序）。
    fn components(&self, index: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut cursor = index;
        while !self.is_empty(cursor) {
            out.push(cursor);
            match self.lines[cursor].predecessor {
                Some(p) => cursor = p,
                None => break,
            }
        }
        out.reverse();
        out
    }

    /// `Line::word_lengths()`（`poet.cc:61-69`）：每个词各占多少字节。
    fn word_lengths(&self, index: usize) -> Vec<usize> {
        let mut lengths = Vec::new();
        let mut last_end = 0usize;
        for c in self.components(index) {
            let end = self.lines[c].end_pos;
            lengths.push(end.saturating_sub(last_end));
            last_end = end;
        }
        lengths
    }

    /// 把一条路径拼成整句文本 —— 等价 C++ 里 `Sentence::Extend` 的累加结果。
    fn sentence_text(&self, index: usize) -> String {
        let mut out = String::new();
        for c in self.components(index) {
            out.push_str(&self.lines[c].text);
        }
        out
    }
}

/// `Poet::Compare`（`poet.h:31`）："one 比 other 差"。
type Compare = fn(&Arena, usize, usize) -> bool;

/// `Poet::CompareWeight`（`poet.cc:88-90`）：只比总分。
fn compare_weight(arena: &Arena, one: usize, other: usize) -> bool {
    arena.get(one).weight < arena.get(other).weight
}

/// `Poet::LeftAssociateCompare`（`poet.cc:93-109`）：
/// 同分时**词少者优**，词数相同则词长序列字典序小者优（左结合倾向）。
fn left_associate_compare(arena: &Arena, one: usize, other: usize) -> bool {
    let w1 = arena.get(one).weight;
    let w2 = arena.get(other).weight;
    if w1 < w2 {
        return true;
    }
    if w1 == w2 {
        let l1 = arena.word_lengths(one);
        let l2 = arena.word_lengths(other);
        if l1.len() > l2.len() {
            return true;
        }
        if l1.len() == l2.len() {
            return l1 < l2;
        }
    }
    false
}

/// `find_top_candidates<N>`（`poet.cc:114-130`）：按 `compare` 取前 N 条（好的在前）。
fn find_top_candidates<const N: usize>(
    state: &HashMap<String, usize>,
    arena: &Arena,
    compare: Compare,
) -> Vec<usize> {
    let mut all: Vec<usize> = state.values().copied().collect();
    // 降序：compare(a, b) 为真表示 a 更差，所以 a 应排在 b 后面。
    // 两个方向都比一遍，保证是合法全序（否则 sort_by 可能 panic）。
    all.sort_by(|&a, &b| {
        if compare(arena, a, b) {
            std::cmp::Ordering::Greater
        } else if compare(arena, b, a) {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    });
    all.truncate(N);
    all
}

/// `BeamSearch`（`poet.cc:134-168`）：按「最后一个词」分槽，每槽留最优一条，
/// 每轮只从最好的 `kMaxLineCandidates` 条往下扩展。
struct BeamSearch;

impl BeamSearch {
    /// `kMaxLineCandidates`（`poet.cc:137`）
    const MAX_LINE_CANDIDATES: usize = 7;

    /// `BestLineToUpdate` + "best.empty() || compare_(best, new_line)" 的判断。
    ///
    /// 同一个 `last_word` 在同一个 `end_pos` 上只有一条线能活下来。
    fn update(
        state: &mut HashMap<String, usize>,
        key: &str,
        new_line: usize,
        arena: &Arena,
        compare: Compare,
    ) {
        match state.get(key).copied() {
            Some(current) if !arena.is_empty(current) && !compare(arena, current, new_line) => {}
            _ => {
                state.insert(key.to_string(), new_line);
            }
        }
    }

    /// `BestLineInState`（`poet.cc:158-167`）。
    fn best(state: &HashMap<String, usize>, arena: &Arena, compare: Compare) -> Option<usize> {
        let mut best: Option<usize> = None;
        for &candidate in state.values() {
            match best {
                None => best = Some(candidate),
                Some(b) if compare(arena, b, candidate) => best = Some(candidate),
                _ => {}
            }
        }
        best
    }
}

/// `DynamicProgramming`（`poet.cc:170-189`）：每个位置只保留一条最优路径。
struct DynamicProgramming;

impl DynamicProgramming {
    fn update(
        state: &mut Option<usize>,
        new_line: usize,
        arena: &Arena,
        compare: Compare,
    ) {
        match *state {
            None => *state = Some(new_line),
            Some(current) if arena.is_empty(current) || compare(arena, current, new_line) => {
                *state = Some(new_line);
            }
            _ => {}
        }
    }

    fn best(state: &Option<usize>, _arena: &Arena, _compare: Compare) -> Option<usize> {
        *state
    }
}

/// 组句结果：一条整句路径。
#[derive(Clone, Debug, PartialEq)]
pub struct Sentence {
    /// 拼好的整句文本。
    pub text: String,
    /// 覆盖到的输入位置（= 音节图的 `interpreted_length`）。
    pub end_pos: usize,
    /// 整句累计得分（对数尺度，越大越好）。
    pub weight: f64,
    /// 逐段的 `(end_pos, text, 累计得分)`，正序。
    pub segments: Vec<(usize, String, f64)>,
}

impl Sentence {
    /// 整句里每段的 `(end_pos, text, 累计得分)`，正序 —— C++ `Sentence::components()`。
    pub fn components(&self) -> &[(usize, String, f64)] {
        &self.segments
    }
}

/// 组句器（`class Poet`，`poet.h:26-58`）。
pub struct Poet {
    grammar: Option<Box<dyn Grammar>>,
    compare: Compare,
}

impl Poet {
    /// C++ `Poet(const Language*, Config*, Compare)`：`grammar` 为 `None` = 走动态规划。
    pub fn new(grammar: Option<Box<dyn Grammar>>) -> Self {
        Self {
            grammar,
            compare: compare_weight,
        }
    }

    /// C++ 里 `LeftAssociateCompare` 给"左结合优先"的翻译器用（如注音）。
    pub fn with_left_associative(grammar: Option<Box<dyn Grammar>>) -> Self {
        Self {
            grammar,
            compare: left_associate_compare,
        }
    }

    pub fn has_grammar(&self) -> bool {
        self.grammar.is_some()
    }

    /// `Poet::MakeSentence`（`poet.cc:246-253`）：选**一条**最优整句。
    ///
    /// 返回 `None` 的两种情况与 C++ 一致：`total_length` 不可达，或到达了但状态为空。
    /// 另外 C++ 会跳过 `start == 0 && end == total_length` 的边（`poet.cc:207-208`），
    /// 即整段就是一个词的情况不算"整句" —— 那种情况由词级候选承担。
    pub fn make_sentence(
        &self,
        graph: &WordGraph,
        total_length: usize,
        preceding_text: &str,
    ) -> Option<Sentence> {
        let mut arena = Arena::new();
        if self.grammar.is_some() {
            let mut states: BTreeMap<usize, HashMap<String, usize>> = BTreeMap::new();
            states.entry(0).or_default().insert(String::new(), K_EMPTY);
            self.run_beam(graph, total_length, preceding_text, &mut arena, &mut states);
            let found = states.get(&total_length)?;
            let best = BeamSearch::best(found, &arena, self.compare)?;
            Some(self.to_sentence(&arena, best))
        } else {
            let mut states: BTreeMap<usize, Option<usize>> = BTreeMap::new();
            states.insert(0, Some(K_EMPTY));
            self.run_dp(graph, total_length, preceding_text, &mut arena, &mut states);
            let found = states.get(&total_length).copied().flatten()?;
            // `DynamicProgramming::BestLineInState` 就是 `final_state` 本身；
            // 若它仍是 `kEmpty`（一步都没扩展），C++ 会返回空 Sentence，这里返回 None。
            if arena.is_empty(found) {
                return None;
            }
            Some(self.to_sentence(&arena, found))
        }
    }

    /// `Poet::MakeSentences`（`poet.cc:258-353`）：束搜索产出多条整句。
    ///
    /// C++ 的束宽是 `max_sentences * 3`，`cutoff_threshold` 用来在分数掉太多时提前收手。
    pub fn make_sentences(
        &self,
        graph: &WordGraph,
        total_length: usize,
        preceding_text: &str,
        max_sentences: usize,
        cutoff_threshold: f64,
    ) -> Vec<Sentence> {
        if max_sentences == 0 {
            return Vec::new();
        }
        let beam_width = max_sentences * 3;
        let mut arena = Arena::new();
        let mut states: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        states.entry(0).or_default().push(K_EMPTY);

        for (&start_pos, edges) in graph {
            let Some(source_state) = states.get(&start_pos) else {
                continue;
            };
            let sources = source_state.clone();
            for &source_line in &sources {
                for (&end_pos, entries) in edges {
                    if start_pos == 0 && end_pos == total_length {
                        continue;
                    }
                    let is_rear = end_pos == total_length;
                    for entry in entries {
                        let context = if arena.is_empty(source_line) {
                            preceding_text.to_string()
                        } else {
                            arena.context(source_line)
                        };
                        let weight = arena.get(source_line).weight
                            + evaluate(
                                &context,
                                &entry.text,
                                entry.weight,
                                is_rear,
                                self.grammar.as_deref(),
                            );
                        // `poet.cc:289-292`：在**前驱的** hash 上续算，不是从 0 起。
                        let new_hash = entry.text.bytes().fold(
                            arena.get(source_line).text_hash,
                            |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64),
                        );
                        let new_line = arena.push(Line {
                            predecessor: Some(source_line),
                            text: entry.text.clone(),
                            end_pos,
                            weight,
                            text_hash: new_hash,
                        });
                        let target = states.entry(end_pos).or_default();
                        // 同文本去重（`poet.cc:295-305`）：保留分高的。
                        if let Some(pos) = target
                            .iter()
                            .position(|&l| arena.get(l).text_hash == new_hash)
                        {
                            if weight > arena.get(target[pos]).weight {
                                target.remove(pos);
                            } else {
                                continue;
                            }
                        }
                        // 按 weight 降序插入（`poet.cc:307-313`）
                        let insert_at = target
                            .iter()
                            .position(|&l| arena.get(l).weight < weight)
                            .unwrap_or(target.len());
                        target.insert(insert_at, new_line);
                        if target.len() > beam_width {
                            target.pop();
                        }
                    }
                }
            }
        }

        let Some(found) = states.get(&total_length) else {
            return Vec::new();
        };
        let mut results = Vec::new();
        let acceleration = 1.0 - 1.0 / max_sentences as f64;
        let mut last_weight = 0.0f64;
        let mut threshold = cutoff_threshold;
        for (i, &line_index) in found.iter().enumerate() {
            if i >= max_sentences {
                break;
            }
            let cur_weight = arena.get(line_index).weight;
            if i > 0 {
                if last_weight != 0.0
                    && ((cur_weight - last_weight).abs() / last_weight.abs()) > threshold
                {
                    break;
                }
                threshold *= acceleration;
            }
            last_weight = cur_weight;
            results.push(self.to_sentence(&arena, line_index));
        }
        results
    }

    fn to_sentence(&self, arena: &Arena, index: usize) -> Sentence {
        let detail: Vec<(usize, String, f64)> = arena
            .components(index)
            .into_iter()
            .map(|c| {
                let line = arena.get(c);
                (line.end_pos, line.text.clone(), line.weight)
            })
            .collect();
        Sentence {
            text: arena.sentence_text(index),
            end_pos: arena.get(index).end_pos,
            weight: arena.get(index).weight,
            segments: detail,
        }
    }

    fn run_beam(
        &self,
        graph: &WordGraph,
        total_length: usize,
        preceding_text: &str,
        arena: &mut Arena,
        states: &mut BTreeMap<usize, HashMap<String, usize>>,
    ) {
        for (&start_pos, edges) in graph {
            let Some(source_state) = states.get(&start_pos) else {
                continue;
            };
            let top = find_top_candidates::<7>(source_state, arena, self.compare);
            for candidate in top {
                for (&end_pos, entries) in edges {
                    if start_pos == 0 && end_pos == total_length {
                        continue;
                    }
                    let is_rear = end_pos == total_length;
                    for entry in entries {
                        let context = if arena.is_empty(candidate) {
                            preceding_text.to_string()
                        } else {
                            arena.context(candidate)
                        };
                        let weight = arena.get(candidate).weight
                            + evaluate(
                                &context,
                                &entry.text,
                                entry.weight,
                                is_rear,
                                self.grammar.as_deref(),
                            );
                        let new_line = arena.push(Line {
                            predecessor: Some(candidate),
                            text: entry.text.clone(),
                            end_pos,
                            weight,
                            text_hash: 0,
                        });
                        let key = entry.text.clone();
                        let target = states.entry(end_pos).or_default();
                        BeamSearch::update(target, &key, new_line, arena, self.compare);
                    }
                }
            }
        }
    }

    fn run_dp(
        &self,
        graph: &WordGraph,
        total_length: usize,
        preceding_text: &str,
        arena: &mut Arena,
        states: &mut BTreeMap<usize, Option<usize>>,
    ) {
        for (&start_pos, edges) in graph {
            let Some(Some(source_line)) = states.get(&start_pos).copied() else {
                continue;
            };
            for (&end_pos, entries) in edges {
                if start_pos == 0 && end_pos == total_length {
                    continue;
                }
                let is_rear = end_pos == total_length;
                for entry in entries {
                    let context = if arena.is_empty(source_line) {
                        preceding_text.to_string()
                    } else {
                        arena.context(source_line)
                    };
                    let weight = arena.get(source_line).weight
                        + evaluate(
                            &context,
                            &entry.text,
                            entry.weight,
                            is_rear,
                            self.grammar.as_deref(),
                        );
                    let new_line = arena.push(Line {
                        predecessor: Some(source_line),
                        text: entry.text.clone(),
                        end_pos,
                        weight,
                        text_hash: 0,
                    });
                    let target = states.entry(end_pos).or_default();
                    DynamicProgramming::update(target, new_line, arena, self.compare);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(text: &str, weight: f64) -> DictEntry {
        DictEntry {
            text: text.to_string(),
            weight,
            ..Default::default()
        }
    }

    /// 两词拼接：「你好」+「世界」。
    fn two_word_graph() -> WordGraph {
        let mut graph = WordGraph::new();
        graph
            .entry(0)
            .or_default()
            .entry(4)
            .or_default()
            .push(entry("你好", -5.0));
        graph
            .entry(4)
            .or_default()
            .entry(8)
            .or_default()
            .push(entry("世界", -4.0));
        graph
    }

    #[test]
    fn make_sentence_concatenates_two_words() {
        let poet = Poet::new(None);
        let sentence = poet.make_sentence(&two_word_graph(), 8, "").unwrap();
        assert_eq!(sentence.text, "你好世界");
        assert_eq!(sentence.end_pos, 8);
        // 无 grammar：两段各加 K_PENALTY
        assert!((sentence.weight - (-5.0 - 4.0 + 2.0 * K_PENALTY)).abs() < 1e-9);
    }

    #[test]
    fn make_sentence_rejects_single_word_covering_all_input() {
        // 整段一个词（0→8）不算整句，返回 None
        let mut graph = WordGraph::new();
        graph
            .entry(0)
            .or_default()
            .entry(8)
            .or_default()
            .push(entry("你好世界", -3.0));
        let poet = Poet::new(None);
        assert!(poet.make_sentence(&graph, 8, "").is_none());
    }

    #[test]
    fn make_sentence_picks_higher_total_weight() {
        // 两条路径：A 权重高但零件多；B 权重更高
        let mut graph = WordGraph::new();
        graph
            .entry(0)
            .or_default()
            .entry(2)
            .or_default()
            .push(entry("你", -9.0));
        graph
            .entry(2)
            .or_default()
            .entry(8)
            .or_default()
            .push(entry("好世界", -1.0));
        graph
            .entry(0)
            .or_default()
            .entry(4)
            .or_default()
            .push(entry("你好", -2.0));
        graph
            .entry(4)
            .or_default()
            .entry(8)
            .or_default()
            .push(entry("世界", -2.0));
        let poet = Poet::new(None);
        let sentence = poet.make_sentence(&graph, 8, "").unwrap();
        assert_eq!(sentence.text, "你好世界");
    }

    #[test]
    fn make_sentence_uses_preceding_text_for_first_word() {
        // 首词的 context 用 preceding_text；无 grammar 时不影响分数，
        // 但保证不会 panic 且路径正确。
        let poet = Poet::new(None);
        let sentence = poet.make_sentence(&two_word_graph(), 8, "前面的话").unwrap();
        assert_eq!(sentence.text, "你好世界");
    }

    #[test]
    fn make_sentences_returns_multiple_ordered_by_weight() {
        let mut graph = WordGraph::new();
        graph
            .entry(0)
            .or_default()
            .entry(4)
            .or_default()
            .push(entry("你好", -2.0));
        graph
            .entry(4)
            .or_default()
            .entry(8)
            .or_default()
            .push(entry("世界", -2.0));
        graph
            .entry(0)
            .or_default()
            .entry(2)
            .or_default()
            .push(entry("你", -9.0));
        graph
            .entry(2)
            .or_default()
            .entry(8)
            .or_default()
            .push(entry("好世界", -1.0));
        let poet = Poet::new(None);
        let sentences = poet.make_sentences(&graph, 8, "", 5, 0.1);
        assert!(!sentences.is_empty());
        assert_eq!(sentences[0].text, "你好世界");
        for pair in sentences.windows(2) {
            assert!(pair[0].weight >= pair[1].weight);
        }
    }
}
