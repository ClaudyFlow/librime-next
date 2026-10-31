//! `src/rime/filter.h` 移植——**甲-1 懒化后**的滤镜语义。
//!
//! # 签名(与 C++ `Filter::Apply` 对齐)
//!
//! 滤镜**包装流**而不是全量加工:返回的新 Translation 在被 Peek 时才逐条
//! 处理。`candidates`(已物化前缀)传进来供"对前缀也有语义"的滤镜使用。
//!
//! # 与 C++ 的差异
//!
//! C++ `uniquifier` 把重复项挂到首个候选(`UniquifiedCandidate`);这里
//! 流式跳过后出现的重复项(语义等价:同文本只出现一次)。

use crate::engine::candidate::Candidate;
use crate::engine::context::Context;
use crate::engine::gear::translation::{
    ConvertTranslation, FilteredTranslation, Translation, UniquifiedTranslation,
};
use crate::engine::segmentation::Segment;

/// 候选过滤器(对照 C++ `Filter`)。
pub trait Filter {
    /// `Filter::AppliesToSegment`:默认对每段都生效。
    fn applies_to_segment(&self, _ctx: &Context, _segment: &Segment) -> bool {
        true
    }

    /// 包装流:返回经本滤镜加工后的 Translation。
    fn apply(
        &mut self,
        ctx: &Context,
        translation: Box<dyn Translation>,
        candidates: &mut Vec<Candidate>,
    ) -> Box<dyn Translation>;
}

/// `gear/uniquifier.cc`:同文本只留首条。
pub struct Uniquifier;

impl Uniquifier {
    pub fn new() -> Self {
        Uniquifier
    }
}

impl Filter for Uniquifier {
    fn apply(
        &mut self,
        _ctx: &Context,
        translation: Box<dyn Translation>,
        candidates: &mut Vec<Candidate>,
    ) -> Box<dyn Translation> {
        let seen: std::collections::HashSet<String> =
            candidates.iter().map(|c| c.text.clone()).collect();
        Box::new(UniquifiedTranslation::new(translation, seen))
    }
}

/// `gear/single_char_filter.cc`:单字候选提到多字词前面(组内保持原序)。
///
/// C++ 实现是全量收集后重排——急切缓冲保留该语义;输入从 Vec 换成流。
pub struct SingleCharFilter;

impl SingleCharFilter {
    pub fn new() -> Self {
        SingleCharFilter
    }
}

/// 单字前置的缓冲式实现。
struct SingleCharBuffered {
    inner: Box<dyn Translation>,
    buffer: Vec<Candidate>,
    pos: usize,
    filled: bool,
}

impl SingleCharBuffered {
    fn fill(&mut self) {
        if self.filled {
            return;
        }
        self.filled = true;
        let mut single = Vec::new();
        let mut multi = Vec::new();
        while let Some(c) = self.inner.peek() {
            if c.text.chars().count() == 1 {
                single.push(c);
            } else {
                multi.push(c);
            }
            self.inner.next();
        }
        single.append(&mut multi);
        self.buffer = single;
    }
}

impl Translation for SingleCharBuffered {
    fn peek(&mut self) -> Option<Candidate> {
        self.fill();
        self.buffer.get(self.pos).cloned()
    }
    fn next(&mut self) {
        self.fill();
        if self.pos < self.buffer.len() {
            self.pos += 1;
        }
    }
}

impl Filter for SingleCharFilter {
    fn apply(
        &mut self,
        _ctx: &Context,
        translation: Box<dyn Translation>,
        _candidates: &mut Vec<Candidate>,
    ) -> Box<dyn Translation> {
        Box::new(SingleCharBuffered {
            inner: translation,
            buffer: Vec::new(),
            pos: 0,
            filled: false,
        })
    }
}
