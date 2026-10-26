//! `src/rime/filter.h` + 两个最常用的过滤器。
//!
//! # 与 C++ 的模型差异（必须知道）
//!
//! C++ 的 `Filter::Apply(translation, candidates)` 接的是**懒式流**，一边 peek 一边吐；
//! lrn 的菜单是**急切模型**（`Menu` 里就是排好序的 `Vec<Candidate>`，见 `menu.rs` 的注释）。
//! 所以这里的 `apply` 直接改写候选列表 —— 最终效果对齐，中间形态不同。
//!
//! 由此产生的一处表面差异：C++ 的 `uniquifier` 会把重复项**挂到首个候选身上**做成
//! `UniquifiedCandidate`（列表长度不变，重复项变"这个候选的另一种来源"）；lrn 直接
//! **删掉后出现的重复项**，长度变短。对前端而言呈现的都是"同一词条只出现一次"。

use crate::engine::candidate::Candidate;
use crate::engine::context::Context;
use crate::engine::segmentation::Segment;

/// 候选过滤器（对照 C++ `Filter`）。
pub trait Filter {
    /// `Filter::AppliesToSegment`：默认对每段都生效。
    fn applies_to_segment(&self, _ctx: &Context, _segment: &Segment) -> bool {
        true
    }

    /// 就地改写候选列表。`ctx` 用于按开关（option）决定是否转换。
    ///
    /// 取 `&mut self` 是为了 `Projection`（`comment_format` / `preedit_format`）——
    /// C++ 的 `Projection::Apply` 是 const 方法，Rust 侧的 `apply_str` 要
    /// `&mut self`（内部 `Vec<Box<dyn Calculation>>` 逐个 `apply`）。
    fn apply(&mut self, _ctx: &Context, candidates: &mut Vec<Candidate>);
}

/// `gear/uniquifier.cc`：同一词条只留一条（保留质量最高的那条）。
pub struct Uniquifier;

impl Uniquifier {
    pub fn new() -> Self {
        Uniquifier
    }
}

impl Filter for Uniquifier {
    fn apply(&mut self, _ctx: &Context, candidates: &mut Vec<Candidate>) {
        let mut seen: Vec<String> = Vec::new();
        let mut out: Vec<Candidate> = Vec::with_capacity(candidates.len());
        for cand in candidates.drain(..) {
            if seen.contains(&cand.text) {
                continue;
            }
            seen.push(cand.text.clone());
            out.push(cand);
        }
        *candidates = out;
    }
}

/// `gear/single_char_filter.cc`：单字候选提到多字词前面（组内保持原序）。
///
/// C++ 只对 `Phrase` 且 `type` 为 `table`/`user_table` 的候选生效；
/// lrn 现在只有 `script` 一种翻译器，这里放宽成"所有候选都参与"，否则这个过滤器
/// 会恒等于空操作。等 `table_translator` 落地再收紧条件。
pub struct SingleCharFilter;

impl SingleCharFilter {
    pub fn new() -> Self {
        SingleCharFilter
    }
}

impl Filter for SingleCharFilter {
    fn apply(&mut self, _ctx: &Context, candidates: &mut Vec<Candidate>) {
        let mut single: Vec<Candidate> = Vec::new();
        let mut multi: Vec<Candidate> = Vec::new();
        for cand in candidates.drain(..) {
            if cand.text.chars().count() == 1 {
                single.push(cand);
            } else {
                multi.push(cand);
            }
        }
        single.append(&mut multi);
        *candidates = single;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(text: &str, quality: f64) -> Candidate {
        Candidate::new("script", 0, text.len(), text.to_string(), quality)
    }

    #[test]
    fn uniquifier_keeps_highest_quality_occurrence() {
        let mut list = vec![cand("你好", 1.0), cand("你好", 5.0), cand("你", 0.5)];
        Uniquifier::new().apply(&Context::new(), &mut list);
        let texts: Vec<&str> = list.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["你好", "你"]);
    }

    #[test]
    fn single_char_comes_first_within_each_group() {
        let mut list = vec![cand("你好", 1.0), cand("你", 0.5), cand("好", 0.4)];
        SingleCharFilter::new().apply(&Context::new(), &mut list);
        let texts: Vec<&str> = list.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["你", "好", "你好"]);
    }
}
