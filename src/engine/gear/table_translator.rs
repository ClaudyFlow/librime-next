//! `src/rime/gear/table_translator.cc`：形码翻译器（五笔 / 郑码 / 宇浩 / 仓颉…）。
//!
//! 与 `script_translator` 的分工：拼音靠**音节图切分组合**（一份输入有多种切法），
//! 形码靠**整码命中 + 前缀补全**（prism 里的拼写就是用户的按键码本身）。
//! 两者共享 [`TranslatorOptions`]（认领段标签 / 削分隔符 / 补全开关 / 质量基数 /
//! preedit 与 comment 格式化 / 黑名单）。
//!
//! C++ 的懒式 `LazyTableTranslation`（边翻页边查表）在急切模型下改为一次查全：
//! * 整码命中：`prism.get_value(code)` → `table.query_phrases`；
//! * 补全（`enable_completion`，默认开）：`prism.common_prefix_search` 里所有
//!   **短于整码**的拼写也出候选，质量随剩余码长递减，注释带 `~剩余码` 提示。
//!
//! 还没移植的 C++ 部分：`MakeSentence`（造句，依赖 poet + user dict）——
//! 形码方案的整句输入（多词连打）暂不出候选，逐词输入不受影响。

use crate::dict::dictionary::Dictionary;
use crate::dict::vocabulary::Code;
use crate::engine::candidate::Candidate;
use crate::engine::gear::translator_options::TranslatorOptions;

/// 整码命中的质量基数：保证压过一切补全候选（补全最多再拿到词频权重）。
const EXACT_MATCH_BONUS: f64 = 1_000_000.0;
/// 补全候选：每差一个字符扣的质量分（保证"打满的码"排在"半截码"前面）。
const COMPLETION_PENALTY_PER_CHAR: f64 = 100.0;

pub struct TableTranslator {
    pub dict: Dictionary,
    pub options: TranslatorOptions,
}

impl TableTranslator {
    pub fn new(dict: Dictionary, options: TranslatorOptions) -> Self {
        Self { dict, options }
    }

    /// 查一个拼写（整码或前缀码）对应的全部词条。
    fn lookup_spelling(&self, spelling: &str) -> Vec<(String, f64)> {
        let Some(spelling_id) = self.dict.prism.get_value(spelling) else {
            return Vec::new();
        };
        let syllable_id = self.dict.prism.spelling_syllable_id(spelling_id);
        let code = Code::from_slice(&[syllable_id]);
        let mut accessor = self.dict.table.query_phrases(&code);
        let mut out = Vec::new();
        while !accessor.exhausted() {
            if let Some(entry) = accessor.entry() {
                let text = self.dict.table.entry_text(&entry);
                if !text.is_empty() && !self.options.blacklist.contains(&text) {
                    out.push((text, entry.weight as f64));
                }
            }
            accessor.next();
        }
        out
    }
}

impl crate::engine::gear::translator::Translator for TableTranslator {
    fn query(
        &mut self,
        input: &str,
        _start: usize,
        seg_tags: &[String],
    ) -> Vec<Candidate> {
        if !self.options.has_any_tag(seg_tags) {
            return Vec::new();
        }
        // C++：`boost::trim_right_if(code, is_any_of(delimiters_))`
        let code = self.options.trim_delimiters(input);
        if code.is_empty() {
            return Vec::new();
        }
        let end = code.len();
        let mut out: Vec<Candidate> = Vec::new();

        // 1) 整码命中
        for (text, weight) in self.lookup_spelling(code) {
            out.push(
                Candidate::new(
                    "table",
                    0,
                    end,
                    text,
                    weight + EXACT_MATCH_BONUS + self.options.initial_quality,
                )
                .with_preedit(code.to_string()),
            );
        }

        // 2) 前缀补全（enable_completion，默认开）
        if self.options.enable_completion {
            let mut matches = self.dict.prism.common_prefix_search(code);
            // 长前缀优先：common_prefix_search 的顺序不确定，显式排序保证稳定
            matches.sort_by(|a, b| b.length.cmp(&a.length));
            for m in matches {
                if m.length >= end {
                    continue; // 整码已处理；>end 不可能（是 code 的前缀）
                }
                let spelling = &code[..m.length];
                let remaining = &code[m.length..];
                for (text, weight) in self.lookup_spelling(spelling) {
                    let quality = weight - remaining.len() as f64 * COMPLETION_PENALTY_PER_CHAR
                        + self.options.initial_quality;
                    out.push(
                        Candidate::new("table", 0, end, text, quality)
                            .with_comment(format!("~{remaining}"))
                            .with_preedit(code.to_string()),
                    );
                }
            }
        }

        // comment 格式化（C++ TableTranslation 的 comment 走 comment_format 投影）
        for c in &mut out {
            self.options.format_comment(&mut c.comment);
        }
        out
    }
}
