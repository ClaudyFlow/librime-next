//! `gear/abc_segmentor.cc` 移植：把"一段合法拼写"切成一段。
//!
//! 判定规则逐字照抄 C++：从当前轮起点 `j` 往后扫，字符必须在 `speller/alphabet`
//! 里，或者是非首位的分隔符；并且"期待声母"的位置必须真的是声母（在 `initials` 里）
//! 或分隔符。扫到第一个不合格字符为止，`[j, k)` 即一段，打 `abc` 标签 +
//! `abc_segmentor/extra_tags` 里配的额外标签。

use crate::engine::commit_history::CommitHistory;
use crate::engine::gear::segmentor::{Segmentor, SegmentorView};
use crate::engine::segmentation::{Segment, Segmentation};

const DEFAULT_ALPHABET: &str = "zyxwvutsrqponmlkjihgfedcba";

pub struct AbcSegmentor {
    alphabet: String,
    delimiter: String,
    initials: String,
    finals: String,
    extra_tags: Vec<String>,
}

impl AbcSegmentor {
    pub fn new(config: &serde_json::Value) -> Self {
        let alphabet = config
            .pointer("/speller/alphabet")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_ALPHABET)
            .to_string();
        let delimiter = config
            .pointer("/speller/delimiter")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let initials = config
            .pointer("/speller/initials")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map_or_else(|| alphabet.clone(), str::to_string);
        let finals = config
            .pointer("/speller/finals")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let extra_tags = config
            .pointer("/abc_segmentor/extra_tags")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        AbcSegmentor {
            alphabet,
            delimiter,
            initials,
            finals,
            extra_tags,
        }
    }
}

impl Segmentor for AbcSegmentor {
    fn proceed(
        &self,
        segmentation: &mut Segmentation,
        _view: SegmentorView,
        _history: &CommitHistory,
    ) -> bool {
        let input = segmentation.input.clone();
        let j = segmentation.current_start();
        let mut k = j;
        let mut expecting_an_initial = true;
        while k < input.len() {
            let ch = input.as_bytes()[k] as char;
            let is_letter = self.alphabet.contains(ch);
            let is_delimiter = k != 0 && self.delimiter.contains(ch);
            if !is_letter && !is_delimiter {
                break;
            }
            let is_initial = self.initials.contains(ch);
            let is_final = self.finals.contains(ch);
            if expecting_an_initial && !is_initial && !is_delimiter {
                break; // 不是合法拼写
            }
            // 只有"以韵母/分隔符收尾"才允许接下一个声母
            expecting_an_initial = is_final || is_delimiter;
            k += 1;
        }
        if j < k {
            let mut segment = Segment::new(j, k);
            segment.tags.push("abc".to_string());
            for tag in &self.extra_tags {
                if !segment.tags.contains(tag) {
                    segment.tags.push(tag.clone());
                }
            }
            segmentation.add_segment(segment);
        }
        // 继续这一轮（后面的 segmentor 还能给同一段加标签）
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn segs(input: &str) -> Segmentation {
        Segmentation {
            input: input.to_string(),
            segments: Vec::new(),
        }
    }

    #[test]
    fn whole_input_becomes_one_abc_segment() {
        let s = AbcSegmentor::new(&json!({}));
        let mut seg = segs("nihao");
        assert!(s.proceed(&mut seg, SegmentorView::default(), &CommitHistory::new()));
        assert_eq!(seg.segments.len(), 1);
        assert_eq!((seg.segments[0].start, seg.segments[0].end), (0, 5));
        assert!(seg.segments[0].tags.contains(&"abc".to_string()));
    }

    #[test]
    fn stops_at_character_outside_alphabet() {
        let s = AbcSegmentor::new(&json!({ "speller": { "alphabet": "abc" } }));
        let mut seg = segs("abc;");
        assert!(s.proceed(&mut seg, SegmentorView::default(), &CommitHistory::new()));
        assert_eq!(seg.segments.len(), 1);
        assert_eq!((seg.segments[0].start, seg.segments[0].end), (0, 3));
    }

    #[test]
    fn empty_segment_when_nothing_valid() {
        let s = AbcSegmentor::new(&json!({ "speller": { "alphabet": "abc" } }));
        let mut seg = segs(";");
        assert!(s.proceed(&mut seg, SegmentorView::default(), &CommitHistory::new()));
        assert!(seg.segments.is_empty());
    }
}
