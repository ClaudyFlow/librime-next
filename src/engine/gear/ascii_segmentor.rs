//! `gear/ascii_segmentor.cc` 移植：西文模式下把整段输入当一段 raw。
//!
//! C++ 只有 30 行，逻辑很清楚：开着 `ascii_mode` 时，从当前起点到串尾**整段**
//! 切成一段并打 `raw` 标签，然后 `return false` 结束本轮（后面的 segmentor 不用看了）。
//! 关着 ascii_mode 时什么都不做（`return true`，把机会让给别人）。

use crate::engine::commit_history::CommitHistory;
use crate::engine::gear::segmentor::{Segmentor, SegmentorView};
use crate::engine::segmentation::{Segment, Segmentation};

pub struct AsciiSegmentor;

impl AsciiSegmentor {
    pub fn new() -> Self {
        AsciiSegmentor
    }
}

impl Segmentor for AsciiSegmentor {
    fn proceed(
        &self,
        segmentation: &mut Segmentation,
        view: SegmentorView,
        _history: &CommitHistory,
    ) -> bool {
        if !view.ascii_mode {
            return true;
        }
        let j = segmentation.current_start();
        if j < segmentation.input.len() {
            let mut segment = Segment::new(j, segmentation.input.len());
            segment.tags.push("raw".to_string());
            segmentation.add_segment(segment);
        }
        false // 本轮结束
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(input: &str) -> Segmentation {
        Segmentation {
            input: input.to_string(),
            segments: Vec::new(),
        }
    }

    #[test]
    fn takes_whole_input_as_one_raw_segment_in_ascii_mode() {
        let s = AsciiSegmentor::new();
        let mut seg = segs("hello world");
        assert!(!s.proceed(
            &mut seg,
            SegmentorView {
                ascii_mode: true,
                ..Default::default()
            },
            &CommitHistory::new()
        ));
        assert_eq!(seg.segments.len(), 1);
        assert_eq!((seg.segments[0].start, seg.segments[0].end), (0, 11));
        assert!(seg.segments[0].tags.contains(&"raw".to_string()));
    }

    #[test]
    fn does_nothing_when_not_in_ascii_mode() {
        let s = AsciiSegmentor::new();
        let mut seg = segs("hello");
        assert!(s.proceed(
            &mut seg,
            SegmentorView::default(),
            &CommitHistory::new()
        ));
        assert!(seg.segments.is_empty());
    }

    #[test]
    fn no_segment_at_input_end() {
        let s = AsciiSegmentor::new();
        let mut seg = segs("abc");
        seg.segments.push(Segment::new(0, 3));
        seg.forward();
        assert!(!s.proceed(
            &mut seg,
            SegmentorView {
                ascii_mode: true,
                ..Default::default()
            },
            &CommitHistory::new()
        ));
        assert_eq!(seg.segments.len(), 2, "串尾不该再切一段（只剩 Forward 造出的空段）");
        let last = seg.segments.last().unwrap();
        assert_eq!((last.start, last.end), (3, 3));
    }
}
