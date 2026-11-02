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
    /// 已确认前缀对应的**音节编码**。逐音节确认时 `committed_prefix` 只留文本，
    /// 编码不留就永远拼不出整词（整词学不成 ⇒ 打几遍也不进首选）。
    pub committed_code: Vec<crate::dict::vocabulary::SyllableId>,
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
    /// 删词等"输入没变但候选要重算"的操作置它（`Engine` 下一轮消费后清掉）。
    ///
    /// 背景：`Engine::process_key` 只在 `input` 变化时 `compose()`，而删词不改变
    /// 输入 —— 用户按 Ctrl+Delete 后菜单纹丝不动，看着像没删掉（2026-11-01）。
    pub needs_recompose: bool,
}

impl Context {
    pub fn new() -> Self {
        Self {
            page_size: 5,
            ..Default::default()
        }
    }

    pub fn is_composing(&self) -> bool {
        // C++ `Context::IsComposing()`:`!input_.empty() || !composition_.empty()`。
        // switcher 展开菜单时输入是空的、但挂着一个 (0,0) 空段 —— 只看 input
        // 会把菜单判成"没在组合",宿主就不画候选窗了(2026-10-31 实测)。
        !self.input.is_empty() || !self.composition.segmentation.segments.is_empty()
    }

    pub fn has_menu(&self) -> bool {
        self.composition
            .back()
            .and_then(|s| s.menu.as_ref())
            .map_or(false, |m| !m.is_empty())
    }

    pub fn get_selected_candidate(&self) -> Option<crate::engine::candidate::Candidate> {
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
        self.committed_code.clear();
        self.composition.segmentation.segments.clear();
        self.composition.segmentation.input.clear();
    }

    /// C++ `Context::AbortComposition()`：`Clear()` + 发 `abort` 通知。
    ///
    /// lrn 里没有 `update_notifier_` / `abort_notifier_` 这条链（通知一律由调用方
    /// 显式补 `Engine::compose`），所以这里只做"清空"这一半。
    /// 使用者是 `Punctuator` 处理数字串（`3.14`）时的"拼成整串重来"。
    pub fn abort_composition(&mut self) {
        self.clear();
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
            self.push_whole_commit_phrase(&text);
            self.collect_commit_history();
            self.clear();
            self.commit_text = Some(text.clone());
            Some(SelectResult::Committed(text))
        } else {
            // 部分覆盖：前缀入账，截掉已确认的输入，续组字。
            // 编码必须一起记：否则整段提交时拼不出整词编码，学不出词组。
            self.committed_prefix.push_str(&cand.text);
            if let Some(entry) = &cand.entry {
                self.committed_code.extend_from_slice(&entry.code);
            }
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

    /// 把末段菜单物化到至少 `n` 条,返回物化后的条数(C++ `Menu::Prepare` 直通)。
    /// 懒菜单下"当前有多少候选"必须先问这个——空物化前缀会让翻页/导航以为没候选。
    pub fn menu_prepare(&self, n: usize) -> usize {
        self.composition
            .back()
            .and_then(|s| s.menu.as_ref())
            .map_or(0, |m| m.prepare(n))
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
        // 2026-11-01：**删词**。把该候选记进用户 n-gram 的黑名单（永不抬分 + 压后），
        // 这样"打错一次"的词不会因为后来误选几次又冒到首选。
        let text = self
            .composition
            .back()
            .and_then(|seg| seg.menu.as_ref())
            .and_then(|menu| menu.candidate_at(index))
            .map(|candidate| candidate.text);
        if let Some(text) = text {
            if let Ok(mut ngram) = crate::engine::user_ngram::global().lock() {
                ngram.penalize(&text);
                let _ = ngram.save(&crate::engine::user_ngram::store_path());
            }
        }
        let Some(seg) = self.composition.back_mut() else {
            return false;
        };
        seg.selected_index = index;
        // 把这段退回"待翻译"并丢掉旧菜单：下一轮 `compose` 会重新查一次，
        // 被拉黑的词这次就不再出现（否则用户看不到任何变化）。
        self.needs_recompose = true;
        seg.reopen(self.caret_pos);
        seg.menu = None;
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
        // 2026-11-01：**上屏就学**（原来 learn_commit 从没被调用过 ⇒ 用户打十几遍也不进首选）。
        // 一次提交常常只有一个词，交给 learn_commit 带上最近窗口凑二元组。
        if !text.is_empty() {
            crate::engine::user_ngram::learn_and_maybe_flush(&[text.clone()]);
        }
        self.collect_commit_elements();
        // 逐段拼出来的整段 → 额外把**整段**按整段音节学成一个词组。
        //
        // 场景（2026-11-02 用户报「只能一个个单字选，打几遍还是不进首选」）：
        // 词典里没有这个词，候选里只有单字，用户只能 一个字一个字 选完上屏。
        // 此时 `elements` 是 4 个单字词条（`yi 伊` / `bu 布` / …），各学各的，
        // **整词 伊布替尼 永远不会出现在候选里** ⇒ 打多少遍都学不会。
        // 这里把各段音标拼起来、文本拼起来，再学一条整段词条，下次就能直接出词。
        //
        // 只有 ≥2 段才补：单段提交本来就学的是那个词条，不必重复。
        self.push_whole_commit_phrase(&text);
        self.collect_commit_history();
        self.clear();
        self.commit_text = Some(text);
        true
    }

    /// 把这次提交的**整段文本**按整段音节再学成一个词条（见 [`Self::commit`]）。
    ///
    /// 两种来源都要覆盖：
    /// - 逐音节确认（`你` → 续组字 → `好`）：前缀编码在 `committed_code`，
    ///   末段词条在 `pending_commit_elements`；
    /// - 整段一次选（`以不踢你`）：`pending_commit_elements` 里已是完整词条，
    ///   这种情况 `elements.len() == 1` 且没有前缀，**不重复学**。
    fn push_whole_commit_phrase(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let has_prefix = !self.committed_code.is_empty();
        if !has_prefix && self.pending_commit_elements.len() < 2 {
            return;
        }
        let mut syllables = self.committed_code.clone();
        for element in &self.pending_commit_elements {
            if element.code.is_empty() {
                // 有一段没音标（整句成分等），拼不出完整编码，放弃整段学习。
                return;
            }
            syllables.extend_from_slice(&element.code);
        }
        if syllables.is_empty() {
            return;
        }
        self.pending_commit_elements
            .push(crate::dict::vocabulary::DictEntry {
                text: text.to_string(),
                code: crate::dict::vocabulary::Code::from_slice(&syllables),
                ..Default::default()
            });
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
                // 整句候选:分词成分逐词进学习管道(n-gram 需要词粒度;
                // 整句作为一个词条只会学出垃圾长词)。成分没有 code,
                // 用户词典的 update_entry 会跳过它,不受影响。
                if !candidate.components.is_empty() {
                    // 整句：先学**整句本身**（`ScriptTranslator` 已给它填上音节 code，
                    // 见 `code_for`），否则重复打同一串永远学不会、整句候选一直压在
                    // 刚打过的词前面（2026-11-01 用户报"又回到后面"）。
                    if let Some(entry) = &candidate.entry {
                        if !entry.code.is_empty() {
                            elements.push(entry.clone());
                        }
                    }
                    for text in &candidate.components {
                        elements.push(crate::dict::vocabulary::DictEntry {
                            text: text.clone(),
                            ..Default::default()
                        });
                    }
                    continue;
                }
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
