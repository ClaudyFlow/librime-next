//! `src/rime/candidate.{h,cc}`：候选词条 + 候选页。

use crate::gear::dict::vocabulary::DictEntry;

/// 一条候选（对照 C++ `Candidate` / `SimpleCandidate`）。
#[derive(Clone, Debug)]
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
    /// 整句候选的**分词成分**(poet 的 components,按序)。
    /// 提交时逐词喂 n-gram(整句本身是一个词条,学习需要词级粒度)。
    pub components: Vec<String>,
    /// 产出这条候选的词典词条（`DictEntry`）。
    ///
    /// C++ 的 `CommitEntry::elements` 是 `vector<const DictEntry*>`，
    /// 由 `Segment::GetSelectedCandidate()` 一路带出来，最终交给
    /// `Translator::Memorize` 学词。Rust 侧直接把词条挂在候选上 ——
    /// 没有它就无法在提交时知道"用户选的到底是哪条词条"。
    /// 组句候选 / 标点候选这类没有词条的，保持 `None`。
    pub entry: Option<DictEntry>,
    /// 音节边界顶点（C++ `Phrase::spans().vertices()`）。
    ///
    /// **绝对字节偏移**、升序、首元素 = `start`、末元素 = `end`。
    /// `partial_commit` 用它把「前 N 个音节」翻译成输入串上的切点：
    /// `Ctrl+3` 提交通道就靠 `spans[3]` 找到该从哪里断开剩余输入。
    ///
    /// 没有音节结构的候选（标点、单独字符兜底）留空 —— 空表示
    /// 「不知道音节边界」，`partial_commit` 会因此放行给其它处理器。
    pub spans: Vec<usize>,
    /// 线性重排头的学习轨迹（同一候选组共享一份）。
    ///
    /// 只有神经重排命中/提交过 batch 的候选才有；提交时由
    /// [`crate::engine::context::Context::collect_commit_elements`] 取走，
    /// 用来构造「选中下标」样本。`None` = 这一层没参与（完全回退）。
    pub rerank: Option<crate::gear::neural::rerank_head::RerankRef>,
}

impl PartialEq for Candidate {
    /// 比较除 `rerank` 外的全部字段：轨迹是旁路元数据，不该影响
    /// 「两条候选是否相同」的既有语义（测试/去重都在用）。
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.start == other.start
            && self.end == other.end
            && self.quality == other.quality
            && self.text == other.text
            && self.comment == other.comment
            && self.preedit == other.preedit
            && self.components == other.components
            && self.entry == other.entry
            && self.spans == other.spans
    }
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
            components: Vec::new(),
            entry: None,
            spans: Vec::new(),
            rerank: None,
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

    /// 挂上产出这条候选的词典词条（学词要用）。
    /// 挂整句分词成分(链式)。
    pub fn with_components(mut self, components: Vec<String>) -> Self {
        self.components = components;
        self
    }

    pub fn with_entry(mut self, entry: DictEntry) -> Self {
        self.entry = Some(entry);
        self
    }

    /// 挂音节边界顶点（绝对字节偏移，升序，首 = start、末 = end）。
    pub fn with_spans(mut self, spans: Vec<usize>) -> Self {
        self.spans = spans;
        self
    }

    /// 候选覆盖的音节数（`spans.len() - 1`；无 spans 时为 0）。
    pub fn syllable_count(&self) -> usize {
        self.spans.len().saturating_sub(1)
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
