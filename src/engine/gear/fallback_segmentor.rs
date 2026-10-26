//! `gear/fallback_segmentor.cc` 移植：兜底分段器。
//!
//! 前面的分段器都没接住的字符，这里一个字切一段、打 `raw` 标签，
//! 让它在界面上原样显示（"吃掉"不了的输入不会消失）。
//! 相邻的单字符 raw 段会**合并成长 raw 段**（打字时一次次追加）。

use crate::engine::commit_history::CommitHistory;
use crate::engine::gear::segmentor::{Segmentor, SegmentorView};
use crate::engine::segmentation::{Segment, Segmentation};

pub struct FallbackSegmentor;

impl FallbackSegmentor {
    pub fn new() -> Self {
        FallbackSegmentor
    }
}

impl Segmentor for FallbackSegmentor {
    fn proceed(
        &self,
        segmentation: &mut Segmentation,
        _view: SegmentorView,
        _history: &CommitHistory,
    ) -> bool {
        if segmentation.current_segment_length() > 0 {
            return false;
        }
        let k = segmentation.current_start();
        if k == segmentation.input.len() {
            return false;
        }
        // 上一段是尚未起步的空段 → 丢掉，它会被新段取代
        if !segmentation.segments.is_empty() {
            let last_empty = {
                let last = &segmentation.segments[segmentation.segments.len() - 1];
                last.start == last.end
            };
            if last_empty {
                segmentation.pop_back();
            }
        }
        if let Some(last) = segmentation.segments.last_mut() {
            if last.tags.iter().any(|t| t == "raw") {
                // 追加一个字符并标记"需要重译"
                last.end = k + 1;
                last.clear();
                last.tags.push("raw".to_string());
                return false;
            }
        }
        let mut segment = Segment::new(k, k + 1);
        segment.tags.push("raw".to_string());
        segmentation.forward();
        segmentation.add_segment(segment);
        // 兜底分段器应当是最后一个被调用的
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_segment_is_one_character() {
        let s = FallbackSegmentor::new();
        let mut seg = Segmentation {
            input: ";".to_string(),
            segments: Vec::new(),
        };
        assert!(!s.proceed(&mut seg, SegmentorView::default(), &CommitHistory::new()));
        assert_eq!(seg.segments.len(), 1);
        assert_eq!((seg.segments[0].start, seg.segments[0].end), (0, 1));
        assert!(seg.segments[0].tags.contains(&"raw".to_string()));
    }

    #[test]
    fn consecutive_raw_characters_merge() {
        let s = FallbackSegmentor::new();
        let mut seg = Segmentation {
            input: ";;".to_string(),
            segments: Vec::new(),
        };
        assert!(!s.proceed(&mut seg, SegmentorView::default(), &CommitHistory::new())); // 第一个 ';'
        seg.forward();
        assert!(!s.proceed(&mut seg, SegmentorView::default(), &CommitHistory::new())); // 第二个 ';' 并入上一段
        assert_eq!(seg.segments.len(), 1);
        assert_eq!((seg.segments[0].start, seg.segments[0].end), (0, 2));
    }

    #[test]
    fn noop_at_input_end() {
        let s = FallbackSegmentor::new();
        let mut seg = Segmentation {
            input: "abc".to_string(),
            segments: Vec::new(),
        };
        seg.forward();
        // current_start = 3 = input.len()
        let mut seg2 = Segmentation {
            input: "abc".to_string(),
            segments: vec![Segment::new(0, 3), Segment::new(3, 3)],
        };
        assert!(!s.proceed(&mut seg2, SegmentorView::default(), &CommitHistory::new()));
        assert_eq!(seg2.segments.len(), 2);
        let _ = &seg;
    }
}
