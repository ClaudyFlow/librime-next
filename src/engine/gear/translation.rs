//! 翻译抽象层(C++ `translation.h` 的移植)——**甲-1 懒化管线**的核心。
//!
//! # 为什么要这一层(2026-10-28 性能判决)
//!
//! 旧实现每次按键**全量物化**全部候选(实测 551~1062 条)+ 全量排序 + 滤镜
//! 全量转换,Engine 内存 488MB。C++ 是懒式:`Menu` 只在需要第 k 条时
//! `Prepare(k)`,翻译器是流式迭代器(`Peek/Next`),滤镜是**包装**而非全量
//! 加工。本模块把这条链按 C++ 语义移植。
//!
//! # 三层结构
//!
//! ```text
//! Translation(trait, Peek/Next)
//!   ├─ VecTranslation            已物化列表的桥(整句/用户词等一次成型的)
//!   ├─ TieredTranslation         层级链:整句 → 用户词 → 词表(C++ 分支序)
//!   ├─ ScriptWordsTranslation    词表懒迭代(码长分桶,长码组先出)
//!   ├─ MergedTranslation         多翻译器归并(quality 降序)
//!   └─ 滤镜包装(Uniquified/Simplified/…,由 Filter::apply 生成)
//! ```

use crate::dict::dict_entry_iterator::DictEntryIterator;
use crate::dict::opencc::Opencc;
use crate::engine::candidate::Candidate;
use std::collections::HashSet;

/// 流式候选迭代(对照 C++ `Translation`)。
pub trait Translation: Send {
    /// 当前候选(不消费)。`None` = 已穷尽。
    fn peek(&mut self) -> Option<Candidate>;
    /// 前进一条。
    fn next(&mut self);
}

/// 已物化列表的桥。
pub struct VecTranslation {
    items: Vec<Candidate>,
    pos: usize,
}

impl VecTranslation {
    pub fn new(items: Vec<Candidate>) -> Self {
        Self { items, pos: 0 }
    }
}

impl Translation for VecTranslation {
    fn peek(&mut self) -> Option<Candidate> {
        self.items.get(self.pos).cloned()
    }
    fn next(&mut self) {
        if self.pos < self.items.len() {
            self.pos += 1;
        }
    }
}

/// 把一条流一次取完(测试/探针用)。
///
/// 懒化后翻译器返回的是流而不是列表;要"看到全部候选"的调用方(单测、探针)
/// 用这个物化,语义等价于 `Menu::prepare(usize::MAX)`。
#[cfg(test)]
pub(crate) fn drain(mut translation: Box<dyn Translation>) -> Vec<Candidate> {
    let mut out = Vec::new();
    while let Some(candidate) = translation.peek() {
        out.push(candidate);
        translation.next();
    }
    out
}

/// `drain` 的 `Option` 版:翻译器没认领该段时是 `None`(等价空列表)。
#[cfg(test)]
pub(crate) fn drain_opt(translation: Option<Box<dyn Translation>>) -> Vec<Candidate> {
    translation.map(drain).unwrap_or_default()
}

/// 层级链:第 0 层全部出完才轮到第 1 层……
///
/// 对照 C++ `ScriptTranslation::Evaluate` 的分支序(kSentence → kUserPhrase →
/// kSysPhrase):靠**结构序**而非分数保证"整句最优先 / 学过的最优先"。
pub struct TieredTranslation {
    tiers: Vec<Box<dyn Translation>>,
    current: usize,
}

impl TieredTranslation {
    pub fn new(tiers: Vec<Box<dyn Translation>>) -> Self {
        Self { tiers, current: 0 }
    }
}

impl Translation for TieredTranslation {
    fn peek(&mut self) -> Option<Candidate> {
        loop {
            let Some(tier) = self.tiers.get_mut(self.current) else {
                return None;
            };
            match tier.peek() {
                Some(c) => return Some(c),
                None => self.current += 1,
            }
        }
    }
    fn next(&mut self) {
        if let Some(tier) = self.tiers.get_mut(self.current) {
            tier.next();
        }
    }
}

/// 词表懒翻译(C++ `ScriptTranslation` 的 kSysPhrase 分支):包装
/// `DictEntryIterator`(桶内已按 weight 降序),按**码长分桶、长码组先出**
/// (C++ `DictEntryCollector` 的 map<code_length> + `rbegin()`)。
pub struct ScriptWordsTranslation {
    /// 桶组:`(码长, 词条列表)`,按码长**降序**。
    groups: Vec<(usize, Vec<crate::dict::vocabulary::DictEntry>)>,
    gi: usize,
    ii: usize,
    /// 段起点(候选 end = start + 码长)。
    start: usize,
    /// 整段输入长度(质量公式的覆盖比分母)。
    full_len: usize,
    /// 质量台阶:码长 × [`QUALITY_SYLLABLE_STRIDE`]。
    quality_stride: f64,
    /// `initial_quality`。
    initial_quality: f64,
    /// 变体词库缺失时的 opencc 兜底(候选文本转换)。
    convert: Option<Opencc>,
}

impl ScriptWordsTranslation {
    pub fn new(
        collector: crate::dict::dictionary::DictEntryCollector,
        start: usize,
        full_len: usize,
        quality_stride: f64,
        initial_quality: f64,
        convert: Option<Opencc>,
    ) -> Self {
        // collector 键 = end_pos(字节);码长 = end - start。降序排。
        let mut groups: Vec<(usize, Vec<crate::dict::vocabulary::DictEntry>)> = collector
            .into_iter()
            .map(|(end, bucket)| (end.saturating_sub(start), bucket))
            .collect();
        groups.sort_by(|a, b| b.0.cmp(&a.0));
        Self {
            groups,
            gi: 0,
            ii: 0,
            start,
            full_len,
            quality_stride,
            initial_quality,
            convert,
        }
    }
}

impl Translation for ScriptWordsTranslation {
    fn peek(&mut self) -> Option<Candidate> {
        loop {
            let Some((code_len, bucket)) = self.groups.get(self.gi) else {
                return None;
            };
            let Some(entry) = bucket.get(self.ii) else {
                self.gi += 1;
                self.ii = 0;
                continue;
            };
            // C++ `set_quality(exp(entry->weight) + initial +
            //                  entry->quality_len / full_code_length)`
            let quality = self.quality_stride * (*code_len) as f64
                + entry.weight.exp()
                + entry.quality_len / self.full_len.max(1) as f64
                + self.initial_quality;
            let mut c = Candidate::new(
                "script",
                self.start,
                self.start + code_len,
                entry.text.clone(),
                quality,
            );
            if !entry.comment.is_empty() {
                c = c.with_comment(entry.comment.clone());
            }
            if let Some(opencc) = self.convert.as_ref() {
                c.text = opencc.convert(&c.text);
            }
            return Some(c);
        }
    }
    fn next(&mut self) {
        self.ii += 1;
    }
}

/// 多翻译器归并(对照 C++ `MergedTranslation`):每次取 peek 质量最高者。
pub struct MergedTranslation {
    sources: Vec<Peeked>,
}

struct Peeked {
    inner: Box<dyn Translation>,
    cached: Option<Candidate>,
}

impl MergedTranslation {
    pub fn new(sources: Vec<Box<dyn Translation>>) -> Self {
        Self {
            sources: sources.into_iter().map(|inner| Peeked { inner, cached: None }).collect(),
        }
    }
}

impl Translation for MergedTranslation {
    fn peek(&mut self) -> Option<Candidate> {
        let mut best: Option<(usize, Candidate)> = None;
        for (i, source) in self.sources.iter_mut().enumerate() {
            if source.cached.is_none() {
                source.cached = source.inner.peek();
            }
            if let Some(c) = &source.cached {
                match &best {
                    Some((_, bc)) if bc.quality >= c.quality => {}
                    _ => best = Some((i, c.clone())),
                }
            }
        }
        best.map(|(_, c)| c)
    }
    fn next(&mut self) {
        // 推进当前最优者(与 peek 同一判据)。
        let mut best: Option<(usize, f64)> = None;
        for (i, source) in self.sources.iter_mut().enumerate() {
            if source.cached.is_none() {
                source.cached = source.inner.peek();
            }
            if let Some(c) = &source.cached {
                match &best {
                    Some((_, bq)) if *bq >= c.quality => {}
                    _ => best = Some((i, c.quality)),
                }
            }
        }
        if let Some((i, _)) = best {
            self.sources[i].inner.next();
            self.sources[i].cached = None;
        }
    }
}

/// 去重包装(C++ `UniquifiedTranslation`):同文本只留首条,
/// 已物化前缀(`candidates`)里的也算数。
pub struct UniquifiedTranslation {
    inner: Box<dyn Translation>,
    seen: HashSet<String>,
}

impl UniquifiedTranslation {
    pub fn new(inner: Box<dyn Translation>, seen: HashSet<String>) -> Self {
        Self { inner, seen }
    }
}

impl Translation for UniquifiedTranslation {
    fn peek(&mut self) -> Option<Candidate> {
        loop {
            let Some(c) = self.inner.peek() else {
                return None;
            };
            if self.seen.contains(&c.text) {
                self.inner.next();
                continue;
            }
            return Some(c);
        }
    }
    fn next(&mut self) {
        if let Some(c) = self.inner.peek() {
            self.seen.insert(c.text.clone());
        }
        self.inner.next();
    }
}

/// opencc 转换包装(C++ `SimplifiedTranslation` / `PrefetchTranslation` 模式)。
pub struct ConvertTranslation {
    inner: Box<dyn Translation>,
    opencc: Opencc,
}

impl ConvertTranslation {
    pub fn new(inner: Box<dyn Translation>, opencc: Opencc) -> Self {
        Self { inner, opencc }
    }
}

impl Translation for ConvertTranslation {
    fn peek(&mut self) -> Option<Candidate> {
        self.inner.peek().map(|mut c| {
            c.text = self.opencc.convert(&c.text);
            c
        })
    }
    fn next(&mut self) {
        self.inner.next();
    }
}

/// 字符集包装的通用形:按谓词过滤。
pub struct FilteredTranslation<F: FnMut(&mut Candidate) -> bool + Send> {
    inner: Box<dyn Translation>,
    predicate: F,
}

impl<F: FnMut(&mut Candidate) -> bool + Send> FilteredTranslation<F> {
    pub fn new(inner: Box<dyn Translation>, predicate: F) -> Self {
        Self { inner, predicate }
    }
}

impl<F: FnMut(&mut Candidate) -> bool + Send> Translation for FilteredTranslation<F> {
    fn peek(&mut self) -> Option<Candidate> {
        loop {
            let Some(mut c) = self.inner.peek() else {
                return None;
            };
            if (self.predicate)(&mut c) {
                return Some(c);
            }
            self.inner.next();
        }
    }
    fn next(&mut self) {
        self.inner.next();
    }
}
