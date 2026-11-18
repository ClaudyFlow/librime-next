//! 只读量化 bigram 语言模型（R-09「n-gram 重排」的可行性验证 + 最小实现）。
//!
//! # 为什么单独一个模块
//!
//! octagram 已按 2026-11-03 的决议移除，现役重排是神经 transformer
//! （`engine/gear/neural/`）。本模块只回答一个问题：**再叠一层 bigram 统计
//! LM 值不值**。它不链接 KenLM（LGPL），不引入任何新依赖。
//!
//! 存储是**两层只读 trie**（根 → 首字 → 次字）+ **u8 量化 log 概率** +
//! **一元回退**。构建期用 `HashMap` 统计（可慢），查询期只读、不分配。
//!
//! # 单位
//!
//! [`BigramModel::score`] 返回**平均每字符 log10 概率**（≤ 0，越接近 0 越好）；
//! [`BigramModel::score_total`] 返回同一模型下的累计值。神经模型
//! （`SentenceModel::score_sentences`）报的是**自然对数**，两者单位不同，
//! 融合前必须乘 `ln(10) ≈ 2.3026` 换算（见离线评测里的 `LN10`）。
//!
//! # 量化与回退
//!
//! 概率按 log10 线性量化到 u8：`q = round(-log10 p / STEP)`，`STEP = 4.0 / 254`，
//! 覆盖 4 个数量级（1 → 1e-4）。`q = 255` 保留给「词表里没有这个字」，固定取
//! floor `-5.0`。回退链：**二元 → 一元 → floor**；未登录字不会丢句子。
//!
//! # 已知局限
//!
//! 只建模「前一个字 → 当前字」，不看跨句上下文（`score` 的入参只有句内文本）；
//! 这是刻意的最小实现，够用来量收益，不够用来做主排序。

use std::collections::HashMap;

/// 量化覆盖的 log10 数量级（概率 1 → 1e-4）。
const QUANT_RANGE_LOG10: f64 = 4.0;
/// 有效量化级数（0..=254）；255 专用于「未收录」。
const QUANT_LEVELS: u8 = 255;
/// 未收录字符的固定 floor（log10）。
pub const UNSEEN_LOG10: f64 = -5.0;
/// 一元 / 二元各保留多少个候选字（按计数截断，控制内存）。
const MAX_VOCAB: usize = 4096;

/// 每级量化对应的 log10 步长。
fn quant_step() -> f64 {
    QUANT_RANGE_LOG10 / (QUANT_LEVELS - 1) as f64
}

/// log10 概率 → u8。
fn quantize(log10_p: f64) -> u8 {
    let q = (-log10_p / quant_step()).round();
    q.clamp(0.0, (QUANT_LEVELS - 1) as f64) as u8
}

/// u8 → log10 概率（255 = [`UNSEEN_LOG10`]）。
fn dequantize(q: u8) -> f64 {
    if q == QUANT_LEVELS {
        UNSEEN_LOG10
    } else {
        -(q as f64) * quant_step()
    }
}

/// trie 节点。子节点区间 `[first, first + count)` 按字符升序排列，查询用二分。
#[derive(Clone, Copy)]
struct Node {
    ch: char,
    prob_q: u8,
    first: u32,
    count: u32,
}

/// 只读量化 bigram 模型。
///
/// * `nodes[0]` 是根；它的子节点是「首字层」（存一元概率）；
/// * 首字层节点的子节点是「次字层」（存 bigram 条件概率），没有孙节点。
pub struct BigramModel {
    nodes: Vec<Node>,
    /// 训练语料的字符总数（含重复）。
    tokens: u64,
    /// 训练语料的句子数。
    sentences: u64,
}

impl BigramModel {
    /// 从纯文本语料构建：按句末标点 / 换行切句，句内逐字统计。
    pub fn from_corpus(text: &str) -> Self {
        let sentences: Vec<&str> = text
            .split(|c: char| {
                matches!(
                    c,
                    '\n' | '\r'
                        | '。'
                        | '！'
                        | '？'
                        | '；'
                        | '…'
                )
            })
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        Self::from_sentences(sentences)
    }

    /// 从已切好的句子构建（句间不产生跨句 bigram）。
    pub fn from_sentences<'a, I>(sentences: I) -> Self
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut unigram: HashMap<char, u64> = HashMap::new();
        let mut bigram: HashMap<(char, char), u64> = HashMap::new();
        let mut sentences_count = 0u64;
        let mut tokens = 0u64;
        for sentence in sentences {
            let chars: Vec<char> = sentence.chars().collect();
            if chars.is_empty() {
                continue;
            }
            sentences_count += 1;
            tokens += chars.len() as u64;
            for &ch in &chars {
                *unigram.entry(ch).or_insert(0) += 1;
            }
            for pair in chars.windows(2) {
                *bigram.entry((pair[0], pair[1])).or_insert(0) += 1;
            }
        }

        // 词表按计数截断到 MAX_VOCAB，再按字符升序固定布局（查询靠二分）。
        let mut vocab: Vec<(char, u64)> = unigram.into_iter().collect();
        vocab.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        vocab.truncate(MAX_VOCAB);
        vocab.sort_by(|a, b| a.0.cmp(&b.0));

        let mut nodes: Vec<Node> = Vec::with_capacity(vocab.len() + 1);
        // 根（占位节点，`ch` 不参与比较）。
        nodes.push(Node {
            ch: '\0',
            prob_q: 0,
            first: 1,
            count: vocab.len() as u32,
        });
        for &(ch, count) in &vocab {
            let p = count as f64 / tokens.max(1) as f64;
            nodes.push(Node {
                ch,
                prob_q: quantize(p.log10()),
                first: 0,
                count: 0,
            });
        }
        // 一元计数表（用于把 bigram 的条件概率分母取对）。
        let counts: HashMap<char, u64> = vocab.iter().copied().collect();

        // 二元按 (首字, 次字) 排序后一次性铺进 flat 数组。
        let mut pairs: Vec<((char, char), u64)> = bigram.into_iter().collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));

        let mut cursor = 0usize;
        for (index, &(ch, _)) in vocab.iter().enumerate() {
            let node = 1 + index; // 根占 0
            let start = cursor;
            while cursor < pairs.len() && pairs[cursor].0.0 == ch {
                let ((_, second), count) = pairs[cursor];
                let total = counts.get(&ch).copied().unwrap_or(0).max(1) as f64;
                let p = count as f64 / total;
                nodes.push(Node {
                    ch: second,
                    prob_q: quantize(p.log10()),
                    first: 0,
                    count: 0,
                });
                cursor += 1;
            }
            let len = (cursor - start) as u32;
            if len > 0 {
                nodes[node].first = (nodes.len() - len as usize) as u32;
                nodes[node].count = len;
            }
        }

        Self {
            nodes,
            tokens,
            sentences: sentences_count,
        }
    }

    /// 词表大小（一元字数）。
    pub fn vocab_size(&self) -> usize {
        self.nodes[0].count as usize
    }

    /// trie 节点总数（含根）。
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// 训练语料的字符总数 / 句子数。
    pub fn corpus_size(&self) -> (u64, u64) {
        (self.sentences, self.tokens)
    }

    /// 平均每字符 log10 概率（≤ 0）。空串返回 `0.0`。
    pub fn score(&self, text: &str) -> f64 {
        let (sum, count) = self.accumulate(text);
        if count == 0 {
            0.0
        } else {
            sum / count as f64
        }
    }

    /// 累计 log10 概率（空串 `0.0`）。
    pub fn score_total(&self, text: &str) -> f64 {
        self.accumulate(text).0
    }

    /// 逐字走 trie：二元命中取 bigram，否则回退一元，再否则 floor。
    fn accumulate(&self, text: &str) -> (f64, usize) {
        let mut context: u32 = 0;
        let mut sum = 0.0f64;
        let mut count = 0usize;
        for ch in text.chars() {
            let node = self.child(context, ch).or_else(|| self.child(0, ch));
            let (q, next) = match node {
                Some(index) => (self.nodes[index as usize].prob_q, index),
                None => (QUANT_LEVELS, 0),
            };
            sum += dequantize(q);
            context = next;
            count += 1;
        }
        (sum, count)
    }

    /// 在 `node` 的子节点里按字符二分查找。
    fn child(&self, node: u32, ch: char) -> Option<u32> {
        let parent = &self.nodes[node as usize];
        if parent.count == 0 {
            return None;
        }
        let start = parent.first as usize;
        let end = start + parent.count as usize;
        self.nodes[start..end]
            .binary_search_by(|probe| probe.ch.cmp(&ch))
            .ok()
            .map(|offset| (start + offset) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::{BigramModel, UNSEEN_LOG10};

    #[test]
    fn empty_text_is_zero() {
        let model = BigramModel::from_sentences(["我们", "你们"]);
        assert_eq!(model.score(""), 0.0);
        assert_eq!(model.score_total(""), 0.0);
    }

    #[test]
    fn unseen_character_falls_to_floor() {
        let model = BigramModel::from_sentences(["我们", "你们"]);
        // 「龘」没在语料里：首字就取 floor，整句只剩 floor。
        assert!((model.score("龘") - UNSEEN_LOG10).abs() < 1e-9);
    }

    #[test]
    fn unseen_bigram_backs_off_to_unigram() {
        let model = BigramModel::from_sentences(["我们", "我们", "他们", "人们", "人们"]);
        // 「我人」：bigram (我,人) 未见 → 回退一元；不能跌到 floor。
        let score = model.score("我人");
        assert!(score > UNSEEN_LOG10, "回退应高于 floor，实际 {score}");
        // 回退值与一元的量化值一致（误差 ≤ 一个量化步长）。
        let first = model.score("我");
        let second = model.score("人");
        let expected = (first + second) / 2.0;
        let step = super::quant_step();
        assert!(
            (score - expected).abs() <= step + 1e-9,
            "回退应为两字一元的均值：{score} vs {expected}"
        );
    }

    #[test]
    fn seen_bigram_beats_backoff() {
        let model = BigramModel::from_sentences(["我们", "我们", "我们", "他们"]);
        // (我,们) 见 3 次；「我你」的 (我,你) 未见，只能回退。
        assert!(
            model.score("我们") > model.score("我你"),
            "已见二元 {} 应高于回退 {}",
            model.score("我们"),
            model.score("我你")
        );
    }

    #[test]
    fn longer_cliché_sentence_scores_higher() {
        let model = BigramModel::from_sentences([
            "今天天气很好",
            "今天天气很好",
            "今天天气很好",
            "明天开会讨论",
        ]);
        // 训练句自身的平均分应高于同长度乱序句。
        assert!(model.score("今天天气很好") > model.score("天好气很今"));
    }

    #[test]
    fn corpus_build_splits_sentences() {
        let model = BigramModel::from_corpus("你好。世界！\n再会");
        assert_eq!(model.corpus_size().0, 3, "三个句子");
        assert_eq!(model.vocab_size(), 6);
    }
}

// ----------------------------------------------------------------------------
// 离线评测（第一步：证明有没有用）
//
// 这是**构造集 / 方向性验证**，不是真实用户分布：语料是同仓库的中文技术随笔
// （doc/progress-summary.md + doc/大厂输入法功能对照.md 训练 / doc/memo.md 测试，句级去重防泄漏），同音干扰按拼音词典
// 人工替换 1–2 个字生成，5 选 1。结论只能说明"方向对不对"，不能当作上线后的
// 准确率。
//
// 三项打分：
// * `static` —— eassy 单字一元次数（log10，按字平均）。词级 eassy 分只作为
//   诊断列打印：单字次数（「的」7.8M）与多字词次数（「就是」66 万）量级不可比，
//   会退化成"把词拆成单字"，见输出里的 0% top-1。
// * `static + w*lm` —— 加本模块的 bigram（同样是 log10/字，w 扫 0.1/0.3/0.5/1.0）。
// * 神经 —— `SentenceModel::score_sentences`（自然对数/字，无上下文），
//   与 `神经 + w*lm` 比较 top-1 冲突。
//
// 神经对照默认关闭（debug 下近 10 分钟）；`WEASEL_LM_EVAL_NEURAL=1` 打开。
// ----------------------------------------------------------------------------

#[cfg(test)]
mod offline_eval {
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};

    use super::BigramModel;

    /// 神经模型报自然对数，bigram 报 log10；融合前换算。
    const LN10: f64 = std::f64::consts::LN_10;

    /// 静态分里未收录字符的惩罚（log10）。
    const UNKNOWN_STATIC_LOG10: f64 = -3.0;
    /// 测试样本上限（要求至少 100 句）。
    const TEST_LIMIT: usize = 100;
    /// 权重扫描点。
    const WEIGHTS: [f64; 4] = [0.1, 0.3, 0.5, 1.0];

    fn manifest_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn repo_root() -> PathBuf {
        manifest_dir().parent().expect("librime-next 应有父目录").to_path_buf()
    }

    fn is_cjk(ch: char) -> bool {
        ('\u{4E00}'..='\u{9FFF}').contains(&ch) || ('\u{3400}'..='\u{4DBF}').contains(&ch)
    }

    // ---------------------------------------------------------------- 语料抽取

    /// 去掉 markdown 行首的 `#` / `>` / 列表符号 / 表格竖线。
    fn strip_prefix(line: &str) -> &str {
        let mut body = line.trim_start();
        body = body.trim_start_matches(['#', '>', '-', '*', '+', '|', ' ', '\t']);
        // 列表编号「1. 」/「2）」
        let digits: String = body.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            let rest = &body[digits.len()..];
            if rest.starts_with('.') || rest.starts_with(')') || rest.starts_with('、') {
                body = &rest[1..];
            }
        }
        body
    }

    /// 去掉行内反引号代码（`xxx`）：整句按 CJK 占比过滤会把带代码标识符的
    /// 正常句子一起丢掉，从而把训练集缩到几百句。
    fn strip_inline_code(line: &str) -> String {
        let mut out = String::with_capacity(line.len());
        let mut in_code = false;
        for ch in line.chars() {
            if ch == '`' {
                in_code = !in_code;
                continue;
            }
            if !in_code {
                out.push(ch);
            }
        }
        out
    }

    /// 从 markdown 抽「CJK 占比 ≥ 85%、6–24 字」的句子（去重、去代码块）。
    fn extract_sentences(markdown: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut in_code = false;
        for line in markdown.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("```") {
                in_code = !in_code;
                continue;
            }
            if in_code || trimmed.is_empty() {
                continue;
            }
            let stripped = strip_inline_code(trimmed);
            let body = strip_prefix(stripped.trim());
            for piece in body.split(|c: char| {
                matches!(
                    c,
                    '。' | '！'
                        | '？'
                        | '；'
                        | '，'
                        | '、'
                        | '：'
                        | '…'
                        | '—'
                        | '\u{201c}'
                        | '\u{201d}'
                        | '\u{2018}'
                        | '\u{2019}'
                        | '「'
                        | '」'
                        | '（'
                        | '）'
                        | '('
                        | ')'
                )
            }) {
                let total = piece.chars().count();
                if total == 0 {
                    continue;
                }
                let cjk: String = piece.chars().filter(|&c| is_cjk(c)).collect();
                let length = cjk.chars().count();
                if !(6..=24).contains(&length) {
                    continue;
                }
                if length as f64 / (total as f64) < 0.85 {
                    continue;
                }
                if seen.insert(cjk.clone()) {
                    out.push(cjk);
                }
            }
        }
        out
    }

    // ------------------------------------------------------------ eassy 一元分

    /// eassy 词频表的两份视图。
    ///
    /// * `words` —— 词级入口（最长 4 字，供贪心匹配的诊断分用）；
    /// * `chars` —— 单字入口。
    ///
    /// **只用单字做静态基线**是刻意的：eassy 的单字次数是语料里的出现次数
    /// （「的」7.8M），而多字词的次数是共现次数（「就是」66 万）。两者量级不可比，
    /// 按词做「log 次数求和」会把任何词都拆成单字（拆开更高），静态分因此退化
    /// （见评测里 `static(词级, 仅诊断)` 的 0% top-1）。输入法里同编码单字候选的
    /// 静态权重本来也是字一级的，所以字一元才是可用的消融基线。
    struct Eassy {
        words: HashMap<String, f64>,
        chars: HashMap<char, f64>,
    }

    fn load_eassy(path: &Path) -> Eassy {
        let text = std::fs::read_to_string(path).expect("eassy 词频表读不出");
        let mut words = HashMap::with_capacity(256 * 1024);
        let mut chars = HashMap::new();
        for line in text.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split('\t');
            let (Some(word), Some(count)) = (parts.next(), parts.next()) else {
                continue;
            };
            if word.is_empty() || word.chars().count() > 4 {
                continue;
            }
            let Ok(count) = count.trim().parse::<f64>() else {
                continue;
            };
            if count <= 0.0 {
                continue;
            }
            if let [ch] = word.chars().collect::<Vec<_>>()[..] {
                chars.insert(ch, count);
            }
            words.insert(word.to_string(), count);
        }
        Eassy { words, chars }
    }

    /// 静态分：eassy 单字一元次数取 log10，按字数平均；词表里没有的字退到
    /// 拼音词典权重，再没有就吃惩罚。
    fn static_score(text: &str, eassy: &Eassy, pinyin: &PinyinIndex) -> f64 {
        let chars: Vec<char> = text.chars().collect();
        if chars.is_empty() {
            return 0.0;
        }
        let mut total = 0.0f64;
        for &ch in &chars {
            let count = eassy
                .chars
                .get(&ch)
                .copied()
                .or_else(|| pinyin.char_weight.get(&ch).copied())
                .unwrap_or(0.0);
            total += if count > 0.0 {
                count.log10()
            } else {
                UNKNOWN_STATIC_LOG10
            };
        }
        total / chars.len() as f64
    }

    /// 词级贪心最长匹配（≤4 字）的 log10 词频，按字数平均。**仅作诊断**：
    /// 因上文的量级不可比问题，它天然偏好把词拆成单字。
    fn word_level_static_score(text: &str, vocab: &HashMap<String, f64>) -> f64 {
        let chars: Vec<char> = text.chars().collect();
        if chars.is_empty() {
            return 0.0;
        }
        let mut total = 0.0f64;
        let mut i = 0usize;
        while i < chars.len() {
            let mut matched = 1usize;
            let mut hit = false;
            for len in (1..=4.min(chars.len() - i)).rev() {
                let word: String = chars[i..i + len].iter().collect();
                if let Some(&count) = vocab.get(&word) {
                    total += count.log10();
                    matched = len;
                    hit = true;
                    break;
                }
            }
            if !hit {
                total += UNKNOWN_STATIC_LOG10;
            }
            i += matched;
        }
        total / chars.len() as f64
    }

    // ------------------------------------------------------------ 同音干扰生成

    /// 拼音词典反查：编码 → 常用同音字；字 → 它的主编码（取权重最高的那条）。
    struct PinyinIndex {
        by_code: HashMap<String, Vec<char>>,
        code_of: HashMap<char, String>,
        /// 字的最高词典权重（eassy 缺字时的静态分兜底）。
        char_weight: HashMap<char, f64>,
    }

    fn load_pinyin(path: &Path) -> PinyinIndex {
        let text = std::fs::read_to_string(path).expect("拼音词典读不出");
        // 同一（编码, 字）可能重复出现，保留最大权重。
        let mut weights: HashMap<(String, char), f64> = HashMap::new();
        for line in text.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split('\t');
            let (Some(word), Some(code)) = (parts.next(), parts.next()) else {
                continue;
            };
            let weight = parts
                .next()
                .and_then(|w| w.trim().parse::<f64>().ok())
                .unwrap_or(1.0);
            let mut chars = word.chars();
            let (Some(ch), None) = (chars.next(), chars.next()) else {
                continue; // 只看单字
            };
            if !is_cjk(ch) {
                continue;
            }
            // 多音节编码（含空格 / 逗号）与单字替换无关，跳过。
            if code.is_empty() || code.contains(' ') || code.contains(',') {
                continue;
            }
            let slot = weights.entry((code.to_string(), ch)).or_insert(0.0);
            if weight > *slot {
                *slot = weight;
            }
        }

        let mut by_code: HashMap<String, Vec<(char, f64)>> = HashMap::new();
        let mut code_of_raw: HashMap<char, (String, f64)> = HashMap::new();
        for ((code, ch), weight) in weights {
            by_code.entry(code.clone()).or_default().push((ch, weight));
            let better = match code_of_raw.get(&ch) {
                Some((_, best)) => weight > *best,
                None => true,
            };
            if better {
                code_of_raw.insert(ch, (code, weight));
            }
        }

        // 每个编码只留权重最高的 32 个同音字（越靠前越常用）。
        let mut pools: HashMap<String, Vec<char>> = HashMap::new();
        for (code, mut list) in by_code {
            list.sort_by(|a, b| b.1.total_cmp(&a.1));
            list.truncate(32);
            pools.insert(code, list.into_iter().map(|(ch, _)| ch).collect());
        }
        let char_weight = code_of_raw
            .iter()
            .map(|(ch, (_, weight))| (*ch, *weight))
            .collect();
        PinyinIndex {
            by_code: pools,
            code_of: code_of_raw.into_iter().map(|(ch, (code, _))| (ch, code)).collect(),
            char_weight,
        }
    }

    /// 确定性 xorshift：评测要可复现，不引 rand。
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed | 1)
        }

        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn pick(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// 把 1–2 个字换成**编码相同**的其它常用字，造 4 个干扰句。
    fn distractors(correct: &str, pinyin: &PinyinIndex, rng: &mut Rng) -> Vec<String> {
        let chars: Vec<char> = correct.chars().collect();
        let mut out: Vec<String> = Vec::new();
        let mut tries = 0usize;
        while out.len() < 4 && tries < 64 {
            tries += 1;
            let edits = if rng.pick(2) == 0 { 1 } else { 2 };
            let mut variant = chars.clone();
            let mut changed = 0usize;
            for _ in 0..edits {
                let pos = rng.pick(chars.len());
                let original = variant[pos];
                let Some(code) = pinyin.code_of.get(&original) else {
                    continue;
                };
                let Some(pool) = pinyin.by_code.get(code.as_str()) else {
                    continue;
                };
                if pool.len() < 2 {
                    continue;
                }
                let replacement = pool[rng.pick(pool.len())];
                if replacement == original || variant.contains(&replacement) {
                    continue;
                }
                variant[pos] = replacement;
                changed += 1;
            }
            if changed == 0 {
                continue;
            }
            let text: String = variant.into_iter().collect();
            if text != correct && !out.iter().any(|existing| existing == &text) {
                out.push(text);
            }
        }
        out
    }

    // ---------------------------------------------------------------- 指标累计

    #[derive(Clone, Copy, Default)]
    struct Acc {
        hits: usize,
        strict: usize,
        rank_sum: f64,
        n: usize,
    }

    impl Acc {
        /// 候选 0 是正确句；返回（并列算中是否 top-1，严格是否 top-1，名次）。
        fn add(&mut self, scores: &[f64]) {
            let correct = scores[0];
            let better = scores[1..].iter().filter(|&&s| s > correct).count();
            let tied = scores[1..].iter().filter(|&&s| s == correct).count();
            self.rank_sum += (1 + better) as f64;
            if better == 0 {
                self.hits += 1;
            }
            if better == 0 && tied == 0 {
                self.strict += 1;
            }
            self.n += 1;
        }

        fn top1(&self) -> f64 {
            if self.n == 0 { 0.0 } else { self.hits as f64 / self.n as f64 }
        }

        fn top1_strict(&self) -> f64 {
            if self.n == 0 { 0.0 } else { self.strict as f64 / self.n as f64 }
        }

        fn mean_rank(&self) -> f64 {
            if self.n == 0 { 0.0 } else { self.rank_sum / self.n as f64 }
        }
    }

    fn argmax(scores: &[f64]) -> usize {
        let mut best = 0usize;
        for (i, &score) in scores.iter().enumerate().skip(1) {
            if score > scores[best] {
                best = i;
            }
        }
        best
    }

    // ---------------------------------------------------------------- 主评测

    #[test]
    fn offline_bigram_eval() {
        let root = repo_root();
        let train_doc = root.join("doc/progress-summary.md");
        let extra_doc = root.join("doc/大厂输入法功能对照.md");
        let test_doc = root.join("doc/memo.md");
        let eassy_path = root.join("Rime-Next/eassy/default/default.eassy.yaml");
        let pinyin_path = root.join("Rime-Next/dict/pinyin/luna/pinyin.luna.dict.yaml");
        let model_path = manifest_dir().join("test-data/model/sentence-model.safetensors");

        for path in [&train_doc, &extra_doc, &test_doc, &eassy_path, &pinyin_path] {
            assert!(path.is_file(), "缺评测语料：{}", path.display());
        }

        let train_text = std::fs::read_to_string(&train_doc).expect("读 doc/progress-summary.md 失败");
        let extra_text =
            std::fs::read_to_string(&extra_doc).expect("读 doc/大厂输入法功能对照.md 失败");
        let test_text = std::fs::read_to_string(&test_doc).expect("读 memo.md 失败");
        // 训练 = 两份文档的句子并集（跨文档也去重）。
        let mut train = extract_sentences(&train_text);
        train.extend(extract_sentences(&extra_text));
        let mut seen = HashSet::new();
        train.retain(|sentence| seen.insert(sentence.clone()));
        let train_set: HashSet<&String> = train.iter().collect();
        let test: Vec<String> = extract_sentences(&test_text)
            .into_iter()
            .filter(|sentence| !train_set.contains(sentence))
            .take(TEST_LIMIT)
            .collect();
        assert!(!train.is_empty(), "训练集为空");
        assert!(test.len() >= 50, "测试集太小：{}", test.len());

        let lm = BigramModel::from_sentences(train.iter().map(|s| s.as_str()));
        let eassy = load_eassy(&eassy_path);
        let pinyin = load_pinyin(&pinyin_path);

        let mut rng = Rng::new(0x9E37_79B9_7F4A_7C15);
        let mut acc_static = Acc::default();
        let mut acc_word_static = Acc::default();
        let mut acc_lm_only = Acc::default();
        let mut acc_static_lm = [Acc::default(); WEIGHTS.len()];
        let mut acc_neural = Acc::default();
        let mut acc_neural_lm = [Acc::default(); WEIGHTS.len()];
        // 神经 top-1 与「神经 + w·lm」top-1 不一致的样本数。
        let mut conflicts = [0usize; WEIGHTS.len()];
        let mut lm_overrode_correct = [0usize; WEIGHTS.len()];
        let mut lm_rescued = [0usize; WEIGHTS.len()];
        let mut examples: Vec<String> = Vec::new();
        let mut example_sentences: HashSet<String> = HashSet::new();

        // 神经对照要 500 次 forward（debug 构建下近 10 分钟），默认不跑；
        // 设 `WEASEL_LM_EVAL_NEURAL=1` 再开启。静态/lm 两项始终跑（约十秒）。
        let neural = if std::env::var("WEASEL_LM_EVAL_NEURAL").ok().as_deref() == Some("1") {
            crate::gear::neural::shared_sentence_model(&model_path)
        } else {
            None
        };
        let mut used = 0usize;
        let mut skipped = 0usize;
        let mut static_sum = 0.0f64;
        let mut lm_sum = 0.0f64;
        let mut neural_sum = 0.0f64;

        for sentence in &test {
            let variants = distractors(sentence, &pinyin, &mut rng);
            if variants.len() < 4 {
                skipped += 1;
                continue;
            }
            let candidates: Vec<String> = std::iter::once(sentence.clone())
                .chain(variants.into_iter())
                .collect();
            let static_scores: Vec<f64> = candidates
                .iter()
                .map(|c| static_score(c, &eassy, &pinyin))
                .collect();
            let word_scores: Vec<f64> = candidates
                .iter()
                .map(|c| word_level_static_score(c, &eassy.words))
                .collect();
            let lm_scores: Vec<f64> = candidates.iter().map(|c| lm.score(c)).collect();

            acc_static.add(&static_scores);
            acc_word_static.add(&word_scores);
            acc_lm_only.add(&lm_scores);
            static_sum += static_scores[0];
            lm_sum += lm_scores[0];

            for (i, &w) in WEIGHTS.iter().enumerate() {
                let fused: Vec<f64> = static_scores
                    .iter()
                    .zip(&lm_scores)
                    .map(|(s, l)| s + w * l)
                    .collect();
                acc_static_lm[i].add(&fused);
            }

            if let Some(model) = neural.as_ref() {
                let refs: Vec<&str> = candidates.iter().map(|c| c.as_str()).collect();
                let raw = model.score_sentences("", &refs);
                if raw.len() == candidates.len() {
                    let chars = sentence.chars().count().max(1) as f64;
                    let neural_scores: Vec<f64> = raw.iter().map(|s| s / chars).collect();
                    neural_sum += neural_scores[0];
                    acc_neural.add(&neural_scores);
                    let neural_top = argmax(&neural_scores);
                    for (i, &w) in WEIGHTS.iter().enumerate() {
                        let fused: Vec<f64> = neural_scores
                            .iter()
                            .zip(&lm_scores)
                            .map(|(n, l)| n + w * (l * LN10))
                            .collect();
                        acc_neural_lm[i].add(&fused);
                        let fused_top = argmax(&fused);
                        if fused_top != neural_top {
                            conflicts[i] += 1;
                            let direction = if neural_top == 0 && fused_top != 0 {
                                lm_overrode_correct[i] += 1;
                                "压掉神经首选"
                            } else if neural_top != 0 && fused_top == 0 {
                                lm_rescued[i] += 1;
                                "修正神经错误"
                            } else {
                                "改了别的候选"
                            };
                            // 只留 w=0.5 / w=1.0 的具体例子，同一句只记一次。
                            if (w == 0.5 || w == 1.0)
                                && examples.len() < 4
                                && example_sentences.insert(sentence.clone())
                            {
                                examples.push(format!(
                                    "[{direction}] 正确「{}」：神经 {:.3} nat / lm {:.3} log10；神经首选「{}」：神经 {:.3} / lm {:.3}；神经+lm 首选「{}」：融合 {:.3}",
                                    sentence,
                                    neural_scores[0],
                                    lm_scores[0],
                                    candidates[neural_top],
                                    neural_scores[neural_top],
                                    lm_scores[neural_top],
                                    candidates[fused_top],
                                    fused[fused_top],
                                ));
                            }
                        }
                    }
                }
            }
            used += 1;
            if used % 20 == 0 {
                eprintln!("  已评测 {used} 句");
            }
        }

        assert!(used >= 50, "有效样本不足：{}", used);
        let (sentences, tokens) = lm.corpus_size();
        println!("\n=== bigram LM 离线评测（构造集，方向性验证） ===");
        println!(
            "训练：doc/progress-summary.md + doc/大厂输入法功能对照.md {} 句 / {} 字符；bigram 词表 {} 字 / trie 节点 {}",
            sentences,
            tokens,
            lm.vocab_size(),
            lm.node_count()
        );
        println!(
            "测试：doc/memo.md {} 句（6–24 字，与训练去重；跳过 {} 句无法造满 4 个干扰）",
            used, skipped
        );
        println!(
            "正确句均分：static {:.3} / lm {:.3}（log10/字）{}",
            static_sum / used as f64,
            lm_sum / used as f64,
            if neural.is_some() {
                format!(" / neural {:.3}（nat/字）", neural_sum / used as f64)
            } else {
                String::from(" / neural 未加载")
            }
        );

        println!("\n| 打分 | top-1(并列算中) | top-1(严格) | 正确句平均名次 |");
        println!("|---|---|---|---|");
        let row = |name: String, acc: &Acc| {
            println!(
                "| {} | {:.1}% | {:.1}% | {:.2} |",
                name,
                acc.top1() * 100.0,
                acc.top1_strict() * 100.0,
                acc.mean_rank()
            );
        };
        row(String::from("static（eassy 字一元）"), &acc_static);
        row(String::from("lm 单独（诊断用）"), &acc_lm_only);
        for (i, &w) in WEIGHTS.iter().enumerate() {
            row(format!("static + {w}*lm"), &acc_static_lm[i]);
        }
        row(String::from("static（词级贪心，仅诊断）"), &acc_word_static);
        if neural.is_some() {
            row(String::from("神经（无上下文）"), &acc_neural);
            for (i, &w) in WEIGHTS.iter().enumerate() {
                row(format!("神经 + {w}*lm"), &acc_neural_lm[i]);
            }
            println!(
                "\n| w | 神经↔神经+lm top-1 不一致 | 其中 lm 压掉正确的神经首选 | 其中 lm 修正神经错误 |"
            );
            println!("|---|---|---|---|");
            for (i, &w) in WEIGHTS.iter().enumerate() {
                println!(
                    "| {w} | {} | {} | {} |",
                    conflicts[i], lm_overrode_correct[i], lm_rescued[i]
                );
            }
            if !examples.is_empty() {
                println!("\n冲突样例：");
                for example in &examples {
                    println!("* {example}");
                }
            }
        } else {
            println!("\n（未设 WEASEL_LM_EVAL_NEURAL=1，跳过神经对照）");
        }
        println!();
    }
}
