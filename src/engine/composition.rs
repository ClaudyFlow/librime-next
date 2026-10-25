//! `src/rime/composition.{h,cc}`：组合串（Segmentation 的超集 + preedit 计算）。

use crate::engine::segmentation::Segmentation;

/// preedit 区文本（对照 C++ `Preedit`）。
#[derive(Clone, Debug, Default)]
pub struct Preedit {
    pub text: String,
    pub caret_pos: usize,
    pub sel_start: usize,
    pub sel_end: usize,
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

    pub fn is_empty(&self) -> bool {
        self.segmentation.segments.is_empty()
    }

    /// 是否正在组字（有未确认段 / 有候选菜单）。
    pub fn has_finished(&self) -> bool {
        self.segmentation.confirmed_position() >= self.segmentation.input.len()
    }

    pub fn has_menu(&self) -> bool {
        self.segmentation
            .segments
            .iter()
            .any(|s| s.menu.as_ref().map_or(false, |m| !m.is_empty()))
    }

    /// 计算 preedit 文本：已确认段用其 confirmed_text，未确认段用其原始输入。
    pub fn get_preedit(&self) -> Preedit {
        let input = &self.segmentation.input;
        let mut text = String::new();
        for seg in &self.segmentation.segments {
            if seg.status == crate::engine::segmentation::SegStatus::Confirmed {
                text.push_str(seg.confirmed_text.as_deref().unwrap_or(""));
            } else {
                let slice = &input[seg.start..seg.end];
                text.push_str(slice);
            }
        }
        Preedit {
            caret_pos: text.len(),
            text,
            ..Default::default()
        }
    }

    /// 已确认文本（提交前的组成文本）。
    pub fn get_commit_text(&self) -> String {
        let input = &self.segmentation.input;
        let mut text = String::new();
        for seg in &self.segmentation.segments {
            if seg.status == crate::engine::segmentation::SegStatus::Confirmed {
                text.push_str(seg.confirmed_text.as_deref().unwrap_or(""));
            } else {
                text.push_str(&input[seg.start..seg.end]);
            }
        }
        text
    }
}
