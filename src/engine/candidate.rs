//! `src/rime/candidate.{h,cc}`：候选词条 + 候选页。

/// 一条候选（对照 C++ `Candidate` / `SimpleCandidate`）。
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// 识别类型（如 `"pinyin"`, `"table"`），由 translator 填。
    pub kind: String,
    /// `[start, end)` 标记候选对应的输入区间（字节偏移）。
    pub start: usize,
    pub end: usize,
    /// 排序质量分（越大越靠前）。
    pub quality: f64,
    /// 提交文本。
    pub text: String,
    /// 注释（如拼音、编码），可选。
    pub comment: String,
    /// 在 preedit 区替换输入串显示的文本，可选。
    pub preedit: String,
}

impl Candidate {
    pub fn new(kind: &str, start: usize, end: usize, text: impl Into<String>, quality: f64) -> Self {
        Self {
            kind: kind.to_string(),
            start,
            end,
            quality,
            text: text.into(),
            comment: String::new(),
            preedit: String::new(),
        }
    }

    pub fn with_comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = comment.into();
        self
    }

    pub fn with_preedit(mut self, preedit: impl Into<String>) -> Self {
        self.preedit = preedit.into();
        self
    }
}

/// 一页候选（对照 C++ `Page`）。
#[derive(Clone, Debug, Default)]
pub struct Page {
    pub page_size: usize,
    pub page_no: usize,
    pub is_last_page: bool,
    pub candidates: Vec<Candidate>,
    pub select_keys: String,
}
