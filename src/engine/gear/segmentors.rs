//! `src/rime/segmentor.h` + `gear/{abc,ascii,fallback}_segmentor.cc`。
//!
//! `Segmentor::Proceed` 按链序调用：返回 `false` 结束本轮分段。
//! C++ 里 segmentor 经 `engine_->context()` 读选项（ascii_mode），Rust 版把
//! `&Context` 作为参数显式传入（引擎侧 take 出 segmentation 规避借用冲突）。

use crate::engine::context::Context;
use crate::engine::segmentation::{Segment, Segmentation};

/// 分段器（对照 C++ `Segmentor`）。
pub trait Segmentor {
    /// 尝试推进分段；返回 `false` 表示结束本轮（后面的分段器不再调用）。
    fn proceed(&mut self, segmentation: &mut Segmentation, ctx: &Context) -> bool;
}

const DEFAULT_ALPHABET: &str = "zyxwvutsrqponmlkjihgfedcba";

/// `abc_segmentor`：从当前起点吞一段合法拼写（字母 + 分隔符），打 `abc` 标签。
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
        let initials = config
            .pointer("/speller/initials")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map_or_else(|| alphabet.clone(), str::to_string);
        Self {
            initials,
            alphabet,
            delimiter: config
                .pointer("/speller/delimiter")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            finals: config
                .pointer("/speller/finals")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            extra_tags: config
                .pointer("/abc_segmentor/extra_tags")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

impl Segmentor for AbcSegmentor {
    fn proceed(&mut self, segmentation: &mut Segmentation, _ctx: &Context) -> bool {
        let input = segmentation.input.as_bytes();
        let j = segmentation.current_start();
        let mut k = j;
        let mut expecting_an_initial = true;
        while k < input.len() {
            let ch = input[k] as char;
            let is_letter = self.alphabet.contains(ch);
            let is_delimiter = k != 0 && self.delimiter.contains(ch);
            if !is_letter && !is_delimiter {
                break;
            }
            let is_initial = self.initials.contains(ch);
            let is_final = self.finals.contains(ch);
            if expecting_an_initial && !is_initial && !is_delimiter {
                break; // 非法拼写起点
            }
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
        true // 本轮继续（交后续分段器）
    }
}

/// `ascii_segmentor`：西文模式下整段输入打 `raw` 标签并结束分段。
pub struct AsciiSegmentor;

impl Segmentor for AsciiSegmentor {
    fn proceed(&mut self, segmentation: &mut Segmentation, ctx: &Context) -> bool {
        if !ctx.get_option("ascii_mode") {
            return true;
        }
        let j = segmentation.current_start();
        if j < segmentation.input.len() {
            let mut segment = Segment::new(j, segmentation.input.len());
            segment.tags.push("raw".to_string());
            segmentation.add_segment(segment);
        }
        false // 分段结束
    }
}

/// `fallback_segmentor`：兜底 —— 没有分段器认领时，吞一个字符打 `raw` 标签
/// （与前一段 raw 合并），并结束本轮。必须在链尾。
pub struct FallbackSegmentor;

impl Segmentor for FallbackSegmentor {
    fn proceed(&mut self, segmentation: &mut Segmentation, _ctx: &Context) -> bool {
        if segmentation.current_segment_length() > 0 {
            return false;
        }
        let k = segmentation.current_start();
        if k == segmentation.input.len() {
            return false;
        }
        // 去掉尾部空段
        if segmentation
            .back()
            .map_or(false, |s| s.start == s.end)
        {
            segmentation.pop_back();
        }
        if let Some(last) = segmentation.back_mut() {
            if last.has_tag("raw") {
                // 扩展前一个 raw 段一个字符，标记需要重算
                last.end = k + 1;
                last.clear();
                last.tags.push("raw".to_string());
                return false;
            }
        }
        {
            let mut segment = Segment::new(k, k + 1);
            segment.tags.push("raw".to_string());
            segmentation.forward();
            segmentation.add_segment(segment);
        }
        false
    }
}
