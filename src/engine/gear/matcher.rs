//! `gear/matcher.cc` 移植：按 `recognizer/patterns` 把识别出的整段标上 tag。
//!
//! 注意它与 processor 侧的 [`crate::engine::gear::recognizer::RecognizerPatterns`]
//! 读的是**同一个配置节点**，共用同一份 [`RecognizerPatterns`]。
//! 差别只在怎么用它：processor 用来"放行按键"，这里用来"给段打标签"。

use crate::engine::gear::recognizer::RecognizerPatterns;
use crate::engine::gear::segmentor::{Segmentor, SegmentorView};
use crate::engine::segmentation::{Segment, Segmentation};

pub struct Matcher {
    patterns: RecognizerPatterns,
}

impl Matcher {
    pub fn new(config: &serde_json::Value, name_space: &str) -> Self {
        let mut patterns = RecognizerPatterns::new();
        // C++：命名空间是 "segmentor" 时按 "recognizer" 读
        let ns = if name_space == "segmentor" {
            "recognizer"
        } else {
            name_space
        };
        patterns.load_config(config, ns);
        Matcher { patterns }
    }
}

impl Segmentor for Matcher {
    fn proceed(&self, segmentation: &mut Segmentation, _view: SegmentorView) -> bool {
        if self.patterns.is_empty() {
            return true;
        }
        let match_ = self
            .patterns
            .get_match(&segmentation.input, segmentation);
        if match_.found() {
            // C++ `while (GetCurrentStartPosition() > match.start) pop_back();`
            // —— 每轮重新取当前起点，不是快照
            while segmentation.current_start() > match_.start {
                segmentation.pop_back();
            }
            let mut segment = Segment::new(match_.start, match_.end);
            segment.tags.push(match_.tag.clone());
            segmentation.add_segment(segment);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tags_whole_input_with_email() {
        let config = json!({
            "recognizer": { "patterns": { "email": "^[a-z]+@[a-z.]+$" } }
        });
        let m = Matcher::new(&config, "recognizer");
        let mut seg = Segmentation {
            input: "abc@example.com".to_string(),
            segments: Vec::new(),
        };
        assert!(m.proceed(&mut seg, SegmentorView::default()));
        assert_eq!(seg.segments.len(), 1);
        assert_eq!((seg.segments[0].start, seg.segments[0].end), (0, 15));
        assert!(seg.segments[0].tags.contains(&"email".to_string()));
    }

    #[test]
    fn no_patterns_is_noop() {
        let m = Matcher::new(&json!({}), "recognizer");
        let mut seg = Segmentation {
            input: "abc".to_string(),
            segments: Vec::new(),
        };
        assert!(m.proceed(&mut seg, SegmentorView::default()));
        assert!(seg.segments.is_empty());
    }
}
