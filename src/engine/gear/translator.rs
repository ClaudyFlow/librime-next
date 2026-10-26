//! `src/rime/translator.h` + `gear/script_translator.cc`：`Translator` trait + 拼音翻译器。

use crate::dict::dictionary::Dictionary;
use crate::engine::candidate::Candidate;
use crate::engine::gear::translator_options::TranslatorOptions;

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
}

impl ScriptTranslator {
    pub fn new(dict: Dictionary) -> Self {
        Self::with_options(dict, TranslatorOptions::default())
    }

    pub fn with_options(dict: Dictionary, options: TranslatorOptions) -> Self {
        Self { dict, options }
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
        let mut out: Vec<Candidate> = self
            .dict
            .translate(input)
            .into_iter()
            .filter(|e| !self.options.blacklist.contains(&e.text))
            .map(|e| {
                let mut c = Candidate::new("script", 0, e.end, e.text, e.quality + self.options.initial_quality);
                if !e.comment.is_empty() {
                    c = c.with_comment(e.comment);
                }
                // 构造 preedit 串并套 preedit_formatter。
                let preedit = self.build_preedit(input, e.end);
                if !preedit.is_empty() {
                    c = c.with_preedit(preedit);
                }
                c
            })
            .collect();
        for c in &mut out {
            self.options.format_comment(&mut c.comment);
            self.options.format_preedit(&mut c.preedit);
        }
        out
    }
}
