//! `src/rime/dict/corrector.{h,cc}`（134 + 322 行）的移植 —— **纯算法部分**。
//!
//! ## 纠错在 Rime 里怎么分工
//!
//! | C++ 类型 | 作用 | 这里的状态 |
//! |---|---|---|
//! | `corrector::Correction` | 一条纠错结果（距离 / 目标音节 / 消耗长度） | ✅ |
//! | `corrector::Corrections` | `HashMap<SyllableId, Correction>`，`Alter` 只保留更优的 | ✅ |
//! | `EditDistanceCorrector::LevenshteinDistance` | 标准编辑距离 | ✅ |
//! | `EditDistanceCorrector::RestrictedDistance` | 带阈值的编辑距离（提前放弃） | ✅ |
//! | `SymDeleteCollector::Collect` | 由音节表生成"对称删除"规则串（喂 algebra 的模糊音） | ⛔ 未移植（见下） |
//! | `NearSearchCorrector::ToleranceSearch` | 在 prism trie 上做近似搜索（键盘邻键替换） | ⛔ 未移植（缺 trie 级 traverse 暴露） |
//! | `EditDistanceCorrector`（本身继承 `Prism`） | 编译期生成"纠错用 prism" | ⛔ 属写路径，未做 |
//!
//! ## 两个未移植项卡在哪（都是接口缺口，不是算法不懂）
//!
//! 1. `NearSearchCorrector::ToleranceSearch` 需要 `prism.trie().traverse(key, node_pos, idx, ...)`
//!    —— 也就是 darts 的**逐字符遍历**（`darts.rs` 里有 `DoubleArray::traverse`，
//!    但 `Prism` 没有把底层 `DoubleArray` 暴露出来）。补一个访问器即可，属下一轮。
//! 2. `SymDeleteCollector::Collect` 生成的是 `Script`（algebra 的规则串），
//!    依赖 `algo::calculus::Script` 的具体构造方式；等 `calculus` 的写侧接口稳定后再补。
//!
//! ## 为什么先把纯函数抄完
//!
//! 这两条距离函数是 `dictionary.cc` / `table_translator.cc` 真正用到的判据（模糊音匹配），
//! 而且**不需要任何 IO**，可以直接用 C++ 的语义钉住单测。等 trie 访问器补上，
//! `NearSearchCorrector` 只是把这些距离函数串到 BFS 上。

use std::collections::BTreeMap;

use crate::dict::vocabulary::SyllableId;

/// 编辑距离（C++ 里是 `size_t`）。
pub type Distance = usize;

/// 一条纠错结果。对应 `corrector::Correction`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Correction {
    /// 与原始输入的距离（越小越好）。
    pub distance: Distance,
    /// 纠错后命中的音节 id。
    pub syllable: SyllableId,
    /// 这次匹配消耗掉的输入长度。
    pub length: usize,
}

/// `corrector::Corrections`：同一个音节只保留**距离更小**的那条。
///
/// C++ 用 `hash_map`，迭代顺序无关；Rust 用 `BTreeMap` 让结果可复现（测试要确定性）。
#[derive(Debug, Default, Clone)]
pub struct Corrections {
    entries: BTreeMap<SyllableId, Correction>,
}

impl Corrections {
    pub fn new() -> Self {
        Self::default()
    }

    /// `Corrections::Alter`：已有更优（距离 ≤ 新的）就保持不动。
    pub fn alter(&mut self, correction: Correction) {
        match self.entries.get(&correction.syllable) {
            Some(existing) if existing.distance <= correction.distance => {}
            _ => {
                self.entries.insert(correction.syllable, correction);
            }
        }
    }

    pub fn get(&self, syllable: SyllableId) -> Option<&Correction> {
        self.entries.get(&syllable)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 按音节 id 升序迭代（确定性）。
    pub fn iter(&self) -> impl Iterator<Item = (&SyllableId, &Correction)> {
        self.entries.iter()
    }
}

/// `EditDistanceCorrector::LevenshteinDistance`（`corrector.cc:163`）。
///
/// 标准编辑距离：插入 / 删除 / 替换各计 1。**按字节**比较（C++ 传的是 `std::string`，
/// 对 UTF-8 多字节字符会逐字节算 —— 这里保持同样行为，注释里说明）。
pub fn levenshtein_distance(left: &str, right: &str) -> Distance {
    let a = left.as_bytes();
    let b = right.as_bytes();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    // 滚动一维数组，空间 O(min(a,b))
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let mut previous: Vec<Distance> = (0..=short.len()).collect();
    let mut current = vec![0usize; short.len() + 1];
    for (index, long_byte) in long.iter().enumerate() {
        current[0] = index + 1;
        for (position, short_byte) in short.iter().enumerate() {
            let substitution = previous[position] + usize::from(short_byte != long_byte);
            current[position + 1] = (previous[position + 1] + 1)
                .min(current[position] + 1)
                .min(substitution);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[short.len()]
}

/// `EditDistanceCorrector::RestrictedDistance`（`corrector.cc:193`）。
///
/// 与 `levenshtein_distance` 同义，但当距离一旦**超过 `threshold`** 就返回一个大于阈值的值
/// （C++ 里靠带状 DP 提前放弃，省算力）。返回值语义：`≤ threshold` 说明在容差内。
pub fn restricted_distance(left: &str, right: &str, threshold: Distance) -> Distance {
    let a = left.as_bytes();
    let b = right.as_bytes();
    // 长度差本身就已经超过阈值 → 不可能在容差内
    if a.len().abs_diff(b.len()) > threshold {
        return threshold + 1;
    }
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }

    let width = a.len();
    let mut previous: Vec<Distance> = (0..=width).collect();
    let mut current = vec![0usize; width + 1];
    for (row, b_byte) in b.iter().enumerate() {
        current[0] = row + 1;
        // 带状：只算 |col - row| <= threshold 的格子，其余置为 threshold+1
        let start = (row + 1).saturating_sub(threshold);
        let end = (row + 1 + threshold).min(width);
        if start > 0 {
            current[start] = threshold + 1;
        }
        for column in (start.max(1))..=end {
            let substitution = previous[column - 1] + usize::from(a[column - 1] != *b_byte);
            current[column] = (previous[column] + 1)
                .min(current[column - 1] + 1)
                .min(substitution);
        }
        for column in (end + 1)..=width {
            current[column] = threshold + 1;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[width]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein_distance("", ""), 0);
        assert_eq!(levenshtein_distance("abc", ""), 3);
        assert_eq!(levenshtein_distance("", "abc"), 3);
        assert_eq!(levenshtein_distance("abc", "abc"), 0);
        assert_eq!(levenshtein_distance("abc", "ab"), 1, "删一个");
        assert_eq!(levenshtein_distance("abc", "abcd"), 1, "插一个");
        assert_eq!(levenshtein_distance("abc", "abd"), 1, "换一个");
        assert_eq!(levenshtein_distance("kitten", "sitting"), 3, "教科书例子");
    }

    #[test]
    fn levenshtein_is_symmetric() {
        for (left, right) in [("abc", "xyz"), ("ni", "hao"), ("a", "abcde")] {
            assert_eq!(
                levenshtein_distance(left, right),
                levenshtein_distance(right, left),
                "{left} / {right} 应满足对称性"
            );
        }
    }

    #[test]
    fn restricted_matches_plain_within_threshold() {
        for (left, right) in [("abc", "abd"), ("abc", "abc"), ("hello", "hallo")] {
            let plain = levenshtein_distance(left, right);
            assert_eq!(
                restricted_distance(left, right, 3),
                plain,
                "{left}/{right} 在容差内应与 plain 一致"
            );
        }
    }

    #[test]
    fn restricted_reports_beyond_threshold() {
        // 距离 3，阈值 1 → 必须返回 >1
        let value = restricted_distance("abc", "xyz", 1);
        assert!(value > 1, "超过阈值应返回 > threshold，实际 {value}");
        // 长度差就超阈值
        assert!(restricted_distance("a", "abcdef", 2) > 2);
    }

    #[test]
    fn corrections_keep_the_better_distance() {
        let mut corrections = Corrections::new();
        corrections.alter(Correction {
            distance: 2,
            syllable: 10,
            length: 3,
        });
        // 更差 → 不覆盖
        corrections.alter(Correction {
            distance: 3,
            syllable: 10,
            length: 3,
        });
        assert_eq!(corrections.get(10).unwrap().distance, 2);
        // 更好 → 覆盖
        corrections.alter(Correction {
            distance: 1,
            syllable: 10,
            length: 2,
        });
        assert_eq!(corrections.get(10).unwrap().distance, 1);
        assert_eq!(corrections.get(10).unwrap().length, 2);
        assert_eq!(corrections.len(), 1, "同一个音节只留一条");
    }

    #[test]
    fn corrections_are_ordered_by_syllable() {
        let mut corrections = Corrections::new();
        for syllable in [30i32, 10, 20] {
            corrections.alter(Correction {
                distance: 1,
                syllable,
                length: 1,
            });
        }
        let order: Vec<i32> = corrections.iter().map(|(id, _)| *id).collect();
        assert_eq!(order, vec![10, 20, 30], "迭代必须确定（BTreeMap 升序）");
    }

    #[test]
    fn empty_key_has_no_corrections() {
        // C++：`if (key.empty()) return;`
        let corrections = Corrections::new();
        assert!(corrections.is_empty());
    }
}
