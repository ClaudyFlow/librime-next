//! `src/rime/segmentation.{h,cc}`：输入串分段。每段带一个 `Menu`。

/// 段状态（对照 `Segment::Status`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegStatus {
    Void,
    Guess,
    Selected,
    Confirmed,
}

/// 一段输入（对照 C++ `Segment`）。`[start, end)` 是输入串中的字节区间。
#[derive(Clone, Debug)]
pub struct Segment {
    pub status: SegStatus,
    pub start: usize,
    pub end: usize,
    pub tags: Vec<String>,
    pub menu: Option<crate::engine::menu::Menu>,
    pub selected_index: usize,
    pub prompt: String,
    /// 该段被确认后产出的文本（选择候选后才填）。
    pub confirmed_text: Option<String>,
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
            confirmed_text: None,
        }
    }

    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t == tag)
    }

    pub fn length(&self) -> usize {
        self.end.saturating_sub(self.start)
    }
}

/// 分段（对照 C++ `Segmentation`，是 `vector<Segment>` 的超集）。
#[derive(Clone, Debug, Default)]
pub struct Segmentation {
    pub input: String,
    pub segments: Vec<Segment>,
}

impl Segmentation {
    pub fn reset(&mut self, input: &str) {
        self.input.clear();
        self.input.push_str(input);
        self.segments.clear();
    }

    pub fn empty(&self) -> bool {
        self.segments.is_empty()
    }

    pub fn back(&self) -> Option<&Segment> {
        self.segments.last()
    }

    /// 已确认位置：最后一个 Confirmed 段的 end。
    pub fn confirmed_position(&self) -> usize {
        let mut pos = 0;
        for seg in &self.segments {
            if seg.status == SegStatus::Confirmed {
                pos = seg.end;
            } else {
                break;
            }
        }
        pos
    }

    /// 当前段起点（最后一个未确认段的起点）：没有段时为输入长度。
    pub fn current_start(&self) -> usize {
        self.segments
            .iter()
            .find(|s| s.status != SegStatus::Confirmed)
            .map(|s| s.start)
            .unwrap_or_else(|| self.input.len())
    }
}
