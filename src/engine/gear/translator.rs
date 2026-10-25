//! `src/rime/translator.h` + `gear/script_translator.cc`：`Translator` trait + 拼音翻译器。

use crate::dict::dictionary::Dictionary;
use crate::engine::candidate::Candidate;

/// 翻译器（对照 C++ `Translator::Query(input, segment)`）。
pub trait Translator {
    /// 给定一段输入串，产出候选列表（含各候选覆盖到的 `end` 位置）。
    fn query(&self, input: &str, start: usize) -> Vec<Candidate>;
}

/// 拼音 / 形码翻译器：把输入按音节图切分，逐切分查 `.table.bin` 得词条。
///
/// MVP 做**词级**：枚举整段输入的所有音节切分，对每个切分 `query_phrases(Code)`，
/// 词条 `end` = 该切分覆盖到的字节位置。这样「你」(单音节切分, end=2) 与「你好」
/// (双音节切分, end=4) 同台出现，逐音节确认行为自然成立。
pub struct ScriptTranslator {
    pub dict: Dictionary,
}

impl ScriptTranslator {
    pub fn new(dict: Dictionary) -> Self {
        Self { dict }
    }
}

impl Translator for ScriptTranslator {
    fn query(&self, input: &str, _start: usize) -> Vec<Candidate> {
        self.dict
            .translate(input)
            .into_iter()
            .map(|e| {
                let mut c =
                    Candidate::new("script", 0, e.end, e.text, e.quality);
                if !e.comment.is_empty() {
                    c = c.with_comment(e.comment);
                }
                c
            })
            .collect()
    }
}
