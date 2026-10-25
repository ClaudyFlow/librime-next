//! `src/rime/context.{h,cc}`：会话级状态 —— input / caret / composition / commit / options。

use std::collections::HashMap;

use crate::engine::candidate::Candidate;
use crate::engine::composition::Composition;
use crate::engine::segmentation::{SegStatus, Segmentation};

/// `select()` 的返回：整段已提交，还是只确认了一部分、继续组字。
#[derive(Clone, Debug)]
pub enum SelectResult {
    /// 候选覆盖了整段输入 → 提交这段文本。
    Committed(String),
    /// 候选只覆盖前缀 → 前缀已确认为 `committed_prefix`，剩余输入继续组字。
    Continued,
}

/// 会话上下文（对照 C++ `Context`）。
///
/// 简化模型：组合串是**单段**覆盖整个 `input`；翻译器把候选铺进这段的菜单，
/// 每个候选带自己的 `end`（覆盖到第几个字节）。选择候选时按 `end` 决定是「整段提交」
/// 还是「只确认前缀、剩余输入继续组字」（真实拼音的逐音节确认行为）。
#[derive(Clone, Debug)]
pub struct Context {
    /// 尚未确认的输入串（已确认的会从这里截掉）。
    pub input: String,
    /// 本轮组字中已经确认的前缀文本。
    pub committed_prefix: String,
    pub caret_pos: usize,
    pub composition: Composition,
    /// 待提交文本（整段输入被消费完时出现）。
    pub commit_text: Option<String>,
    /// 当前高亮候选下标（箭头导航用，不直接提交）。
    pub highlight: usize,
    pub page_size: usize,
    pub options: HashMap<String, bool>,
    pub properties: HashMap<String, String>,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            input: String::new(),
            committed_prefix: String::new(),
            caret_pos: 0,
            composition: Composition::new(),
            commit_text: None,
            highlight: 0,
            page_size: 5,
            options: HashMap::new(),
            properties: HashMap::new(),
        }
    }
}

impl Context {
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否正在组字（有未确认输入，或已有提交文本待取）。
    pub fn is_composing(&self) -> bool {
        !self.input.is_empty() || !self.committed_prefix.is_empty() || self.commit_text.is_some()
    }

    pub fn has_menu(&self) -> bool {
        self.composition
            .segmentation
            .segments
            .first()
            .and_then(|s| s.menu.as_ref())
            .map(|m| !m.is_empty())
            .unwrap_or(false)
    }

    pub fn push_input(&mut self, ch: char) {
        self.input.push(ch);
        self.caret_pos = self.input.len();
    }

    pub fn pop_input(&mut self, len: usize) {
        if len >= self.input.len() {
            self.input.clear();
        } else {
            let new_len = self.input.len() - len;
            self.input.truncate(new_len);
        }
        self.caret_pos = self.input.len();
    }

    pub fn set_input(&mut self, value: &str) {
        self.input.clear();
        self.input.push_str(value);
        self.caret_pos = self.input.len();
    }

    pub fn clear(&mut self) {
        self.input.clear();
        self.committed_prefix.clear();
        self.caret_pos = 0;
        self.composition.reset("");
        self.commit_text = None;
    }

    /// 取待提交文本并清空（c_api `get_commit` 用）。
    pub fn take_commit(&mut self) -> Option<String> {
        self.commit_text.take().map(|t| {
            self.input.clear();
            self.committed_prefix.clear();
            self.composition.reset("");
            t
        })
    }

    /// 整段组合的提交文本（未选择候选时按片段拼接原始输入）。
    pub fn get_commit_text(&self) -> String {
        let mut s = self.committed_prefix.clone();
        s.push_str(&self.composition.get_commit_text());
        s
    }

    /// 选择当前段菜单里的第 `index` 个候选。
    pub fn select(&mut self, index: usize) -> Option<SelectResult> {
        let input_len = self.input.len();
        let cand: Candidate = {
            let seg = self.composition.segmentation.segments.first()?;
            let menu = seg.menu.as_ref()?;
            menu.candidate_at(index)?.clone()
        };
        if cand.end >= input_len {
            let text = format!("{}{}", self.committed_prefix, cand.text);
            self.committed_prefix.clear();
            self.input.clear();
            self.composition.reset("");
            self.commit_text = Some(text.clone());
            Some(SelectResult::Committed(text))
        } else {
            self.committed_prefix.push_str(&cand.text);
            self.input.drain(..cand.end);
            self.caret_pos = self.input.len();
            Some(SelectResult::Continued)
        }
    }

    /// 提交当前组合（c_api `commit_composition`）。无待提交时按高亮项选择。
    pub fn commit_composition(&mut self) -> bool {
        if self.commit_text.is_some() {
            return true;
        }
        if !self.has_menu() {
            return false;
        }
        let idx = self.highlight.min(self.menu_candidate_count().saturating_sub(1));
        match self.select(idx) {
            Some(SelectResult::Committed(_)) => true,
            Some(SelectResult::Continued) => false,
            None => false,
        }
    }

    /// 当前段菜单里的候选数。
    pub fn menu_candidate_count(&self) -> usize {
        self.composition
            .segmentation
            .segments
            .first()
            .and_then(|s| s.menu.as_ref())
            .map(|m| m.candidate_count())
            .unwrap_or(0)
    }

    /// 移动高亮候选（箭头/翻页导航，不直接提交）。
    pub fn move_highlight(&mut self, delta: isize) {
        let n = self.menu_candidate_count();
        if n == 0 {
            self.highlight = 0;
            return;
        }
        let mut h = self.highlight as isize + delta;
        if h < 0 {
            h = 0;
        }
        if h >= n as isize {
            h = n as isize - 1;
        }
        self.highlight = h as usize;
    }

    pub fn set_option(&mut self, name: &str, value: bool) {
        self.options.insert(name.to_string(), value);
    }

    pub fn get_option(&self, name: &str) -> bool {
        *self.options.get(name).unwrap_or(&false)
    }

    pub fn set_property(&mut self, name: &str, value: &str) {
        self.properties.insert(name.to_string(), value.to_string());
    }
}

impl Segmentation {
    /// 构造覆盖整段输入的单个段（我们的简化模型）。
    pub fn segment_zero(len: usize) -> crate::engine::segmentation::Segment {
        crate::engine::segmentation::Segment::new(0, len)
    }
}
