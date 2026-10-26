//! `src/rime/context.{h,cc}`：会话级状态（C++ 全量移植）。
//!
//! input / caret / composition / options / properties + 段选择与编辑操作。
//! C++ 通过 notifier 把 select/update 事件回传引擎；Rust 版由 `Engine` 在
//! 调用点（`process_key` / `select_candidate`）显式做对应处理（`on_select` 等）。

use std::collections::HashMap;

use crate::engine::composition::{Composition, Preedit};
use crate::engine::segmentation::SegStatus;

/// `select()` 的返回：选择导致了整段提交，还是继续组字（给 `Session::choose` 用）。
#[derive(Clone, Debug)]
pub enum SelectResult {
    Committed(String),
    Continued,
}

/// `BeginEditing` 打的标签：编辑发生后不再重开上一个选择。
const SELECTED_BEFORE_EDITING: &str = "selected_before_editing";

/// 软光标符号（U+2038 ‸），`soft_cursor` 选项开启时插进 preedit。
const CARET_SYMBOL: &str = "\u{2038}";

/// 会话上下文（对照 C++ `Context`）。
#[derive(Clone, Debug, Default)]
pub struct Context {
    /// 完整输入串（composition 持有的可能只是截到光标的前缀）。
    pub input: String,
    pub caret_pos: usize,
    pub composition: Composition,
    /// 已确认的前缀文本（逐音节确认时累积，如「你」；整段提交时拼接）。
    pub committed_prefix: String,
    /// 待提交文本（`commit()` 产生，`Session` 取走）。
    pub commit_text: Option<String>,
    /// 每页候选数（来自 schema `menu/page_size`，供 c_api 读）。
    pub page_size: usize,
    pub options: HashMap<String, bool>,
    pub properties: HashMap<String, String>,
    /// 本次提交里选中的词条（C++ `CommitEntry::elements`）。
    ///
    /// `commit()` / 整段 `select()` 会把它填上，[`crate::engine::runtime::Engine::apply_commit`]
    /// 取走并分发给各 translator 学词。放在 `Context` 里而不是直接回调 Engine，
    /// 是因为 processor（editor）只拿得到 `&mut Context`。
    pub pending_commit_elements: Vec<crate::dict::vocabulary::DictEntry>,
    /// 最近 20 条上屏记录（C++ `Context::commit_history_`）。
    ///
    /// `history_translator` 出"最近输入"、`punctuator` 判数字串、
    /// `table_translator` 拼短语去编码，都读这一份。
    pub commit_history: crate::engine::commit_history::CommitHistory,
}

impl Context {
    pub fn new() -> Self {
        Self {
            page_size: 5,
            ..Default::default()
        }
    }

    pub fn is_composing(&self) -> bool {
        !self.input.is_empty() || !self.composition.empty()
    }

    pub fn has_menu(&self) -> bool {
        self.composition
            .back()
            .and_then(|s| s.menu.as_ref())
            .map_or(false, |m| !m.is_empty())
    }

    pub fn get_selected_candidate(&self) -> Option<&crate::engine::candidate::Candidate> {
        self.composition.back().and_then(|s| s.get_selected_candidate())
    }

    /// 当前高亮下标（末段 `selected_index`）。
    pub fn highlight_index(&self) -> usize {
        self.composition.back().map_or(0, |s| s.selected_index)
    }

    // ---- 输入编辑 ----

    pub fn push_input_ch(&mut self, ch: char) -> bool {
        if self.caret_pos >= self.input.len() {
            self.input.push(ch);
            self.caret_pos = self.input.len();
        } else {
            let mut buf = [0u8; 4];
            let s = ch.encode_utf8(&mut buf);
            self.input.insert_str(self.caret_pos, s);
            self.caret_pos += s.len();
        }
        true
    }

    pub fn push_input_str(&mut self, s: &str) -> bool {
        if self.caret_pos >= self.input.len() {
            self.input.push_str(s);
            self.caret_pos = self.input.len();
        } else {
            self.input.insert_str(self.caret_pos, s);
            self.caret_pos += s.len();
        }
        true
    }

    pub fn pop_input(&mut self, len: usize) -> bool {
        if self.caret_pos < len {
            return false;
        }
        self.caret_pos -= len;
        self.input.truncate(self.caret_pos);
        true
    }

    pub fn delete_input(&mut self, len: usize) -> bool {
        if self.caret_pos + len > self.input.len() {
            return false;
        }
        self.input.drain(self.caret_pos..self.caret_pos + len);
        true
    }

    pub fn set_input(&mut self, value: &str) {
        self.input.clear();
        self.input.push_str(value);
        self.caret_pos = self.input.len();
    }

    pub fn set_caret_pos(&mut self, caret_pos: usize) {
        self.caret_pos = caret_pos.min(self.input.len());
    }

    pub fn clear(&mut self) {
        self.input.clear();
        self.caret_pos = 0;
        self.committed_prefix.clear();
        self.composition.segmentation.segments.clear();
        self.composition.segmentation.input.clear();
    }

    // ---- 段选择 ----

    /// `Select(index)`：选中末段第 index 个候选（置 `selected_index` + `Selected`）。
    ///
    /// 返回选择结果：候选覆盖整段输入 ⇒ [`SelectResult::Committed`]（整段提交）；
    /// 只覆盖前缀 ⇒ [`SelectResult::Continued`]（截掉已确认前缀，剩余输入重新组字）。
    pub fn select(&mut self, index: usize) -> Option<SelectResult> {
        let input_len = self.input.len();
        let cand: crate::engine::candidate::Candidate = {
            let seg = self.composition.back_mut()?;
            let menu = seg.menu.as_ref()?;
            let c = menu.candidate_at(index)?.clone();
            seg.selected_index = index;
            seg.status = SegStatus::Selected;
            c
        };
        if cand.end >= input_len {
            // 整段覆盖：拼接已确认前缀，提交
            let mut text = std::mem::take(&mut self.committed_prefix);
            text.push_str(&cand.text);
            // 提交前收词条 / 记历史（`clear()` 会把 segmentation 清掉）
            self.collect_commit_elements();
            self.collect_commit_history();
            self.clear();
            self.commit_text = Some(text.clone());
            Some(SelectResult::Committed(text))
        } else {
            // 部分覆盖：前缀入账，截掉已确认的输入，续组字
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
        let idx = self
            .highlight_index()
            .min(self.menu_candidate_count().saturating_sub(1));
        matches!(self.select(idx), Some(SelectResult::Committed(_)))
    }

    /// 当前段菜单里的候选数。
    pub fn menu_candidate_count(&self) -> usize {
        self.composition
            .back()
            .and_then(|s| s.menu.as_ref())
            .map_or(0, |m| m.candidate_count())
    }

    /// 移动高亮候选（箭头/翻页导航，不直接提交）。返回高亮是否有变化。
    pub fn move_highlight(&mut self, delta: isize) -> bool {
        let n = self.menu_candidate_count();
        if n == 0 {
            return false;
        }
        let cur = self.highlight_index() as isize;
        let h = (cur + delta).clamp(0, n as isize - 1) as usize;
        self.highlight(h)
    }

    /// `Highlight(index)`：移动高亮（末段 `selected_index`），返回是否有变化。
    pub fn highlight(&mut self, index: usize) -> bool {
        let Some(seg) = self.composition.back_mut() else {
            return false;
        };
        if seg.menu.is_none() {
            return false;
        }
        let count = seg.menu.as_ref().map_or(0, |m| m.candidate_count());
        let new_index = if count > 0 { index.min(count - 1) } else { 0 };
        if seg.selected_index == new_index {
            return false;
        }
        seg.selected_index = new_index;
        true
    }

    /// `ConfirmCurrentSelection`：确认当前高亮（不落候选也允许 —— 空段交给
    /// fluid_editor 提交整句的路径由调用方处理）。
    pub fn confirm_current_selection(&mut self) -> bool {
        let Some(seg) = self.composition.back_mut() else {
            return false;
        };
        if seg.get_selected_candidate().is_none() && seg.end == seg.start {
            return false;
        }
        seg.status = SegStatus::Selected;
        true
    }

    pub fn delete_candidate(&mut self, index: usize) -> bool {
        let Some(seg) = self.composition.back_mut() else {
            return false;
        };
        seg.selected_index = index;
        true
    }

    pub fn delete_current_selection(&mut self) -> bool {
        let idx = self.highlight_index();
        self.delete_candidate(idx)
    }

    /// `BeginEditing`：从后往前找第一个 `<= Selected` 的段；若恰为 `Selected`
    /// 打上 `selected_before_editing`（此后 `ReopenPreviousSelection` 拒绝重开）。
    pub fn begin_editing(&mut self) {
        for seg in self.composition.segmentation.segments.iter_mut().rev() {
            if seg.status > SegStatus::Selected {
                return;
            }
            if seg.status == SegStatus::Selected {
                if !seg.has_tag(SELECTED_BEFORE_EDITING) {
                    seg.tags.push(SELECTED_BEFORE_EDITING.to_string());
                }
                return;
            }
        }
    }

    pub fn reopen_previous_segment(&mut self) -> bool {
        if self.composition.segmentation.trim() {
            if let Some(back) = self.composition.back_mut() {
                if back.status >= SegStatus::Selected {
                    back.reopen(self.caret_pos);
                }
            }
            return true;
        }
        false
    }

    pub fn clear_previous_segment(&mut self) -> bool {
        let Some(back) = self.composition.back() else {
            return false;
        };
        let where_ = back.start;
        if where_ >= self.input.len() {
            return false;
        }
        let prefix = self.input[..where_].to_string();
        self.set_input(&prefix);
        true
    }

    pub fn reopen_previous_selection(&mut self) -> bool {
        let segs = &mut self.composition.segmentation.segments;
        for i in (0..segs.len()).rev() {
            if segs[i].status > SegStatus::Selected {
                return false;
            }
            if segs[i].status == SegStatus::Selected {
                if segs[i].has_tag(SELECTED_BEFORE_EDITING) {
                    return false;
                }
                segs.truncate(i + 1);
                let caret = self.caret_pos;
                segs[i].reopen(caret);
                return true;
            }
        }
        false
    }

    /// `ClearNonConfirmedComposition`：弹出末尾所有未选定段，再 `Forward()`。
    pub fn clear_non_confirmed_composition(&mut self) -> bool {
        let mut reverted = false;
        while self
            .composition
            .back()
            .map_or(false, |s| s.status < SegStatus::Selected)
        {
            self.composition.segmentation.pop_back();
            reverted = true;
        }
        if reverted {
            self.composition.segmentation.forward();
        }
        reverted
    }

    pub fn refresh_non_confirmed_composition(&mut self) -> bool {
        self.clear_non_confirmed_composition()
    }

    // ---- 提交 ----

    /// `Commit`：取组合文本待提交并清场。
    pub fn commit(&mut self) -> bool {
        if !self.is_composing() {
            return false;
        }
        let text = self.get_commit_text();
        self.collect_commit_elements();
        self.collect_commit_history();
        self.clear();
        self.commit_text = Some(text);
        true
    }

    /// C++ `ConcreteEngine::OnCommit(ctx)` 的第一件事
    /// （`engine.cc:333`）：把这段组合记进提交历史。
    ///
    /// 必须在 `clear()` **之前**调用 —— 记录来自各段的选中候选和 `input`，
    /// 清场后两者都没了。三个字段互不重叠，所以可以直接分开借。
    pub fn collect_commit_history(&mut self) {
        self.commit_history
            .push_composition(&self.composition, &self.input);
    }

    /// 把各段**已选中**候选挂着的词条收进 `pending_commit_elements`。
    ///
    /// 对应 C++ `Context::commit()` 里 `CommitEntry` 的构建：`elements` 是
    /// `segment.GetSelectedCandidate()->entry` 的集合，之后由
    /// `Engine::ApplyCommit` 播给每个 translator 的 `Memorize`。
    pub fn collect_commit_elements(&mut self) {
        let mut elements = Vec::new();
        for segment in &self.composition.segmentation.segments {
            if let Some(candidate) = segment
                .menu
                .as_ref()
                .and_then(|menu| menu.candidate_at(segment.selected_index))
            {
                if let Some(entry) = &candidate.entry {
                    elements.push(entry.clone());
                }
            }
        }
        self.pending_commit_elements = elements;
    }

    /// 取走待学词的词条（`Engine::apply_commit` 用）。
    pub fn take_commit_elements(&mut self) -> Vec<crate::dict::vocabulary::DictEntry> {
        std::mem::take(&mut self.pending_commit_elements)
    }

    /// C++ `engine_->sink()(text)`：不经组合**直接**把一段文本送进提交管道。
    ///
    /// 与 [`Self::commit`] 的区别：不走 `Composition::GetCommitText()`，也不要求当前
    /// 正在组字 —— `commit_script_text` / `commit_comment` 以及 AsciiComposer 的
    /// 临时西文结算（`CommitAndReset`）走的就是这条路。
    pub fn commit_direct(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        match self.commit_text {
            Some(ref mut s) => s.push_str(&text),
            None => self.commit_text = Some(text),
        }
    }

    /// C++ `ConcreteEngine::CommitText(text)`（`engine.cc:326-331`）：直接提交一段
    /// 文本，**并记一条 `raw`**。
    ///
    /// 与 [`Self::commit_direct`] 的区别正是这条历史记录：`commit_direct` 对应
    /// 裸的 `sink()(text)`（`editor` 的 `commit_script_text`、`commit_comment` 走它，
    /// C++ 里这些**不进**历史）；而 `ascii_composer` 的临时西文结算走 `CommitText`，
    /// 要进 —— 否则打完 `3` 再打 `.` 时「上一条是不是数字」就断了。
    pub fn commit_raw_text(&mut self, text: String) {
        self.commit_history.push(
            crate::engine::commit_history::CommitRecord::new(
                crate::engine::commit_history::KIND_RAW,
                text.clone(),
            ),
        );
        self.commit_direct(text);
    }

    pub fn get_commit_text(&self) -> String {
        if self.get_option("dumb") {
            return String::new();
        }
        let mut s = self.committed_prefix.clone();
        s.push_str(&self.composition.get_commit_text());
        s
    }

    pub fn get_script_text(&self) -> String {
        self.composition.get_script_text(false)
    }

    pub fn get_preedit(&self) -> Preedit {
        let soft = if self.get_option("soft_cursor") {
            CARET_SYMBOL
        } else {
            ""
        };
        self.composition.get_preedit(&self.input, self.caret_pos, soft)
    }

    // ---- options / properties ----

    pub fn set_option(&mut self, name: &str, value: bool) {
        self.options.insert(name.to_string(), value);
    }

    pub fn get_option(&self, name: &str) -> bool {
        *self.options.get(name).unwrap_or(&false)
    }

    pub fn set_property(&mut self, name: &str, value: &str) {
        self.properties.insert(name.to_string(), value.to_string());
    }

    pub fn get_property(&self, name: &str) -> String {
        self.properties.get(name).cloned().unwrap_or_default()
    }

    /// `ClearTransientOptions`：清掉 `_` 开头的 options/properties（方案局部）。
    pub fn clear_transient_options(&mut self) {
        self.options.retain(|k, _| !k.starts_with('_'));
        self.properties.retain(|k, _| !k.starts_with('_'));
    }
}

impl crate::engine::segmentation::Segmentation {
    /// 构造覆盖整段输入的单个段（`Engine::compose` 的简化单段模型用）。
    pub fn segment_zero(len: usize) -> crate::engine::segmentation::Segment {
        crate::engine::segmentation::Segment::new(0, len)
    }
}
