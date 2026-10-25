//! `src/rime/segmentation.{h,cc}`：输入串分段（C++ 全量移植）。
//!
//! 多段模型：输入串被切成若干 `[start, end)` 段，每段带标签集与候选菜单。
//! 三条合并规则（`AddSegment`）：同轮只看左对齐段；长段覆盖短段；等长段合并标签。
//! `Forward()` 结束一轮并开启下一段的空段；`Trim()` 去掉尾部空段。

use crate::engine::candidate::Candidate;
use crate::engine::menu::Menu;

/// `Segment::Close()` 部分选中时打的标签。
pub const PARTIAL_SELECTION_TAG: &str = "partial";

/// 段状态（对照 `Segment::Status`）。序关系即 C++ 的 `>=` 比较。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SegStatus {
    Void,
    Guess,
    Selected,
    Confirmed,
}

/// 一段输入（对照 C++ `Segment`）。`[start, end)` 是组合串（composition 持有的
/// 那份 input）中的字节区间。
#[derive(Clone, Debug)]
pub struct Segment {
    pub status: SegStatus,
    pub start: usize,
    pub end: usize,
    pub tags: Vec<String>,
    pub menu: Option<Menu>,
    pub selected_index: usize,
    pub prompt: String,
}

impl Segment {
    pub fn new(start: usize, end: usize) -> Self {
        Self {
            status: SegStatus::Void,
            start,
            end,
            tags: Vec::new(),
            menu: None,
            selected_index: 0,
            prompt: String::new(),
        }
    }

    /// `Segment::Clear`：标记需要重算（保留 start/end）。
    pub fn clear(&mut self) {
        self.status = SegStatus::Void;
        self.tags.clear();
        self.menu = None;
        self.selected_index = 0;
        self.prompt.clear();
    }

    /// `Segment::Close`：选中只覆盖部分输入的候选时，把段截短到候选末端并打
    /// `partial` 标签（剩余输入会作为下一段重新分段）。
    pub fn close(&mut self) {
        if let Some(cand) = self.get_selected_candidate() {
            if cand.end < self.end {
                self.end = cand.end;
                if !self.has_tag(PARTIAL_SELECTION_TAG) {
                    self.tags.push(PARTIAL_SELECTION_TAG.to_string());
                }
            }
        }
    }

    /// `Segment::Reopen`：重开已选段。光标在原段末 ⇒ 复用候选与选择（退回 partial），
    /// 否则整段回 Void。
    pub fn reopen(&mut self, caret_pos: usize) {
        if self.status < SegStatus::Selected {
            return;
        }
        let original_end = self.start + self.length();
        if original_end == caret_pos {
            if self.end < original_end {
                self.end = original_end;
                self.tags.retain(|t| t != PARTIAL_SELECTION_TAG);
            }
            self.status = SegStatus::Guess;
        } else {
            self.status = SegStatus::Void;
        }
    }

    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t == tag)
    }

    pub fn has_any_tag(&self, tags: &[&str]) -> bool {
        tags.iter().any(|t| self.has_tag(t))
    }

    pub fn length(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn get_candidate_at(&self, index: usize) -> Option<&Candidate> {
        self.menu.as_ref()?.candidate_at(index)
    }

    pub fn get_selected_candidate(&self) -> Option<&Candidate> {
        self.get_candidate_at(self.selected_index)
    }
}

/// 分段（对照 C++ `Segmentation`，是 `vector<Segment>` 的超集）。
#[derive(Clone, Debug, Default)]
pub struct Segmentation {
    pub input: String,
    pub segments: Vec<Segment>,
}

impl Segmentation {
    /// `Reset(input)`：换输入串时**差量保留**已确认段 —— 弹出 `end` 越过公共前缀
    /// 的段；弹过则 `Forward()` 收尾。这是「已确认段不被后续编辑推翻」的关键。
    pub fn reset(&mut self, new_input: &str) {
        let old = self.input.as_bytes();
        let new = new_input.as_bytes();
        let mut diff_pos = 0;
        while diff_pos < old.len()
            && diff_pos < new.len()
            && old[diff_pos] == new[diff_pos]
        {
            diff_pos += 1;
        }
        let mut disposed = 0;
        while !self.segments.is_empty() && self.segments.last().unwrap().end > diff_pos {
            self.segments.pop();
            disposed += 1;
        }
        if disposed > 0 {
            self.forward();
        }
        self.input.clear();
        self.input.push_str(new_input);
    }

    /// 截断到前 `n` 段。
    pub fn reset_n(&mut self, n: usize) {
        if n < self.segments.len() {
            self.segments.truncate(n);
        }
    }

    pub fn empty(&self) -> bool {
        self.segments.is_empty()
    }

    pub fn back(&self) -> Option<&Segment> {
        self.segments.last()
    }

    pub fn back_mut(&mut self) -> Option<&mut Segment> {
        self.segments.last_mut()
    }

    pub fn pop_back(&mut self) {
        self.segments.pop();
    }

    pub fn push_back(&mut self, segment: Segment) {
        self.segments.push(segment);
    }

    /// `AddSegment`：三条规则。
    /// 1. 只接受左对齐到当前轮起点的段；
    /// 2. 长段胜（覆盖等长短段）；
    /// 3. 等长段合并标签。
    pub fn add_segment(&mut self, mut segment: Segment) -> bool {
        let start = self.current_start();
        if segment.start != start {
            return false;
        }
        if self.segments.is_empty() {
            self.segments.push(segment);
            return true;
        }
        let last = self.segments.last_mut().unwrap();
        if last.end > segment.end {
            // 规则二（前半）：保留更长的
        } else if last.end < segment.end {
            // 规则二（后半）：覆盖短的
            *last = segment;
        } else {
            // 规则三：合并标签（去重并集）
            for tag in segment.tags.drain(..) {
                if !last.tags.contains(&tag) {
                    last.tags.push(tag);
                }
            }
        }
        true
    }

    /// `Forward`：结束一轮，开启下一段的空段。
    pub fn forward(&mut self) -> bool {
        match self.segments.last() {
            Some(last) if last.start != last.end => {
                let start = last.end;
                self.segments.push(Segment::new(start, start));
                true
            }
            _ => false,
        }
    }

    /// `Trim`：去掉尾部空段。
    pub fn trim(&mut self) -> bool {
        if self
            .back()
            .map_or(false, |s| s.start == s.end)
        {
            self.segments.pop();
            true
        } else {
            false
        }
    }

    pub fn has_finished(&self) -> bool {
        self.back().map_or(0, |s| s.end) >= self.input.len()
    }

    pub fn current_start(&self) -> usize {
        self.back().map_or(0, |s| s.start)
    }

    pub fn current_end(&self) -> usize {
        self.back().map_or(0, |s| s.end)
    }

    pub fn current_segment_length(&self) -> usize {
        self.back().map_or(0, |s| s.end.saturating_sub(s.start))
    }

    /// `GetConfirmedPosition`：**最后一个** `status >= Selected` 的段的 end。
    pub fn confirmed_position(&self) -> usize {
        let mut pos = 0;
        for seg in &self.segments {
            if seg.status >= SegStatus::Selected {
                pos = seg.end;
            }
        }
        pos
    }
}
