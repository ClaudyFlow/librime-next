//! `src/rime/composition.{h,cc}`：组合串（C++ 全量移植）。
//!
//! Composition = Segmentation + preedit/提交文本的计算。分段持有的 `input` 可能是
//! 截到光标的串，`GetPreedit` 额外接收完整 `input` 与光标位置以还原光标后的字符。

use crate::engine::segmentation::{SegStatus, Segmentation};

/// preedit 区文本（对照 C++ `Preedit`）。
#[derive(Clone, Debug, Default)]
pub struct Preedit {
    pub text: String,
    /// `usize::MAX` 对应 C++ 的 `string::npos`（未出现）。
    pub caret_pos: usize,
    pub sel_start: usize,
    pub sel_end: usize,
}

pub const NPOS: usize = usize::MAX;

/// 候选预览（对照 evo `Composition::CandidatePreview`，`composition.h:21-44`）。
///
/// 三段的划分以「当前活动段的选中候选」为界：候选之前已选定的文本、候选本身、
/// 候选之后尚未转换的输入。C API 的 `RimeCandidatePreview` 直接映射这三项。
#[derive(Clone, Debug, Default)]
pub struct CandidatePreview {
    pub text_before_selection: String,
    pub selected_text: String,
    pub text_after_selection: String,
}

/// 组合串（对照 C++ `Composition`）。
#[derive(Clone, Debug, Default)]
pub struct Composition {
    pub segmentation: Segmentation,
}

impl Composition {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self, input: &str) {
        self.segmentation.reset(input);
    }

    pub fn input(&self) -> &str {
        &self.segmentation.input
    }

    pub fn empty(&self) -> bool {
        self.segmentation.segments.is_empty()
    }

    pub fn back(&self) -> Option<&crate::engine::segmentation::Segment> {
        self.segmentation.back()
    }

    pub fn back_mut(&mut self) -> Option<&mut crate::engine::segmentation::Segment> {
        self.segmentation.back_mut()
    }

    /// `HasFinishedComposition`：最后一个非空段已选定（其后只允许跟一个空段）。
    pub fn has_finished(&self) -> bool {
        let segs = &self.segmentation.segments;
        if segs.is_empty() {
            return false;
        }
        let mut k = segs.len() - 1;
        if k > 0 && segs[k].start == segs[k].end {
            k -= 1;
        }
        segs[k].status >= SegStatus::Selected
    }

    /// `GetPrompt`：末段 prompt（供 preedit 尾部展示）。
    pub fn get_prompt(&self) -> &str {
        self.back().map_or("", |s| s.prompt.as_str())
    }

    /// `GetPreedit(full_input, caret_pos, caret_symbol)`（C++ 逐分支移植）。
    ///
    /// 非末段显示已选候选文本（否则显示原文）；末段高亮区显示高亮候选的 preedit
    /// （候选 preedit 里的 `\t` 是光标占位，只在光标位于输入末尾时展开）。
    pub fn get_preedit(
        &self,
        full_input: &str,
        caret_pos: usize,
        caret_symbol: &str,
    ) -> Preedit {
        let input = self.segmentation.input.as_str();
        let segs = &self.segmentation.segments;
        let mut preedit = Preedit {
            caret_pos: NPOS,
            ..Default::default()
        };
        let mut end = 0usize;
        for (i, seg) in segs.iter().enumerate() {
            let start = end;
            if caret_pos == start {
                preedit.caret_pos = preedit.text.len();
            }
            let cand = seg.get_selected_candidate();
            if i < segs.len() - 1 {
                // 已转换段
                if let Some(cand) = cand {
                    end = cand.end;
                    preedit.text.push_str(&cand.text);
                } else {
                    end = seg.end;
                    if !seg.has_tag("phony") {
                        preedit.text.push_str(&input[start.min(input.len())..end.min(input.len())]);
                    }
                }
            } else {
                // 高亮段
                preedit.sel_start = preedit.text.len();
                preedit.sel_end = NPOS;
                let cand_preedit = cand
                    .as_ref()
                    .filter(|c| !c.preedit.is_empty())
                    .map(|c| c.preedit.clone());
                if let Some(cp) = cand_preedit {
                    end = cand.as_ref().map_or(seg.end, |c| c.end);
                    if let Some(placeholder) = cp.find('\t') {
                        preedit.text.push_str(&cp[..placeholder]);
                        if caret_pos == end && end == full_input.len() {
                            preedit.sel_end = preedit.sel_start + placeholder;
                            preedit.caret_pos = preedit.sel_end;
                            preedit.text.push_str(&cp[placeholder + 1..]);
                        }
                    } else {
                        preedit.text.push_str(&cp);
                    }
                } else {
                    end = seg.end;
                    let lo = start.min(input.len());
                    let hi = end.min(input.len());
                    preedit.text.push_str(&input[lo..hi]);
                }
                if preedit.sel_end == NPOS {
                    preedit.sel_end = preedit.text.len();
                }
            }
        }
        if end < input.len() {
            preedit.text.push_str(&input[end..]);
            end = input.len();
        }
        if preedit.caret_pos == NPOS {
            preedit.caret_pos = preedit.text.len();
        }
        if end < full_input.len() {
            preedit.text.push_str(&full_input[end..]);
        }
        // 软光标 + prompt 插入光标处
        let mut prompt = String::new();
        prompt.push_str(caret_symbol);
        prompt.push_str(self.get_prompt());
        if !prompt.is_empty() {
            let at = preedit.caret_pos.min(preedit.text.len());
            preedit.text.insert_str(at, &prompt);
            if preedit.caret_pos < preedit.sel_start {
                preedit.sel_start += prompt.len();
            }
            if preedit.caret_pos < preedit.sel_end {
                preedit.sel_end += prompt.len();
            }
        }
        preedit
    }

    /// `GetCommitText`：各段已选候选文本（无则原文，`phony` 段跳过）拼接，
    /// 末尾补上分段没盖到的剩余输入。
    pub fn get_commit_text(&self) -> String {
        let input = self.segmentation.input.as_str();
        let mut result = String::new();
        let mut end = 0usize;
        for seg in &self.segmentation.segments {
            if let Some(cand) = seg.get_selected_candidate() {
                end = cand.end;
                result.push_str(&cand.text);
            } else {
                end = seg.end;
                if !seg.has_tag("phony") {
                    let lo = seg.start.min(input.len());
                    let hi = seg.end.min(input.len());
                    result.push_str(&input[lo..hi]);
                }
            }
        }
        if input.len() > end {
            result.push_str(&input[end..]);
        }
        result
    }

    /// `GetCandidatePreview(full_input)`（evo `composition.cc:194-241`）。
    ///
    /// `input_` 是截到光标的串；`full_input` 用来还原光标之后的字符。与
    /// [`Self::get_commit_text`] 的差别：
    ///
    /// * 只看「最后一个非空段」（rime 会在光标处补一个空段）；
    /// * 未选定段直接用 `input_` 的原文切片，**不做** `phony` 跳过；
    /// * 分成 before / selected / after 三段，候选跨段时 after 以候选的 `end` 为界。
    pub fn get_candidate_preview(&self, full_input: &str) -> CandidatePreview {
        let input = self.segmentation.input.as_str();
        let source = if full_input.is_empty() {
            input
        } else {
            full_input
        };
        let mut preview = CandidatePreview::default();
        if self.empty() {
            preview.selected_text = source.to_string();
            return preview;
        }

        let segs = &self.segmentation.segments;
        let mut active = segs.len() - 1;
        while active > 0 && segs[active].start >= segs[active].end {
            active -= 1;
        }

        // 活动段之前的：已选定的用候选文本，否则用原文切片
        for seg in &segs[..active] {
            if let Some(cand) = seg.get_selected_candidate() {
                preview.text_before_selection.push_str(&cand.text);
            } else {
                let lo = seg.start.min(input.len());
                let hi = seg.end.min(input.len()).max(lo);
                preview.text_before_selection.push_str(&input[lo..hi]);
            }
        }

        // 活动段：候选文本优先，且以候选的 end 作为「选中到哪里」
        let current = &segs[active];
        let mut candidate_end_pos = current.end;
        if let Some(cand) = current.get_selected_candidate() {
            if !cand.text.is_empty() {
                preview.selected_text.push_str(&cand.text);
                candidate_end_pos = cand.end;
            }
        }
        if preview.selected_text.is_empty() {
            let lo = current.start.min(input.len());
            let hi = current.end.min(input.len()).max(lo);
            preview.selected_text.push_str(&input[lo..hi]);
        }

        if source.len() > candidate_end_pos {
            preview.text_after_selection = source[candidate_end_pos..].to_string();
        }
        preview
    }

    /// `GetScriptText`：与 `GetCommitText` 类似，但未选定段用高亮候选的 preedit。
    pub fn get_script_text(&self, keep_selection: bool) -> String {
        let input = self.segmentation.input.as_str();
        let mut result = String::new();
        let mut end = 0usize;
        for seg in &self.segmentation.segments {
            let start = end;
            let cand = seg.get_selected_candidate();
            end = cand.as_ref().map_or(seg.end, |c| c.end);
            if keep_selection
                && cand.as_ref().map_or(false, |c| !c.text.is_empty())
                && seg.status >= SegStatus::Selected
            {
                result.push_str(cand.as_ref().map_or("", |c| c.text.as_str()));
            } else if cand.as_ref().map_or(false, |c| !c.preedit.is_empty()) {
                let cp = cand.as_ref().map_or(String::new(), |c| c.preedit.clone());
                if let Some(pos) = cp.find('\t') {
                    result.push_str(&cp[..pos]);
                    result.push_str(&cp[pos + 1..]);
                } else {
                    result.push_str(&cp);
                }
            } else if !seg.has_tag("phony") {
                let lo = start.min(input.len());
                let hi = end.min(input.len());
                result.push_str(&input[lo..hi]);
            }
        }
        if input.len() > end {
            result.push_str(&input[end..]);
        }
        result
    }
}
