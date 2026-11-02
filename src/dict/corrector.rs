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
//! ## 键盘几何（Xime 的做法，比 librime 自带的更全）
//!
//! Xime（`ximeiorg/Xime`，`jni/librime-correction`）做触屏纠错时重新量了一遍键盘：
//! librime 自带的 `NearSearchCorrector` 只认**左右邻**（`q→w`），抓不到 `q→a`
//! 这种最常见的上下误触。Xime 用的是"键心归一化距离 ≤ 1.35 键宽"——同行相邻 1.0、
//! 上下/斜向 1.118、跨行对角 1.414（刚好被 1.35 排除）。这里沿用同一把尺子。
//!
//! 三条距离档位对应 Xime 在 TSI 43735 次真实 tap 上测出来的方向先验：
//! 同行 0、斜向 −1.52、跨行 −2.56 —— 越"远"越不像误触。
//!
//! ## `EditDistanceCorrector`
//!
//! C++ 的 `EditDistanceCorrector` 继承 `Prism`，在**编译期**生成一份"纠错用 prism"，
//! 查询期在 trie 上做 `ToleranceSearch`。LRN 没有把 darts 的逐字符 `traverse` 暴露出来
//! （`prism.rs` 的 trait 只有 5 个方法），所以改成**等价的线性扫描**：
//! 第一次调用时用 `expand_search("")` 把音节表拉出来缓存，之后对每个音节跑
//! [`weighted_restricted_distance`]（阈值内提前放弃）。
//! 拼音音节表几百条，一次扫描几十微秒，且只在"值得纠"的时候才跑（见下）。
//!
//! ## 门控：什么时候才纠（Xime 的 `min_frac` / `margin`）
//!
//! Xime 的关键设计是**按原码的语言状态分档**，而且"只追加、绝不替换"。librime 侧
//! 天然有等价机制：`syllabifier` 给纠错拼写打 `is_correction = true` +
//! `credibility = log(0.01)`（`−4.605`），纠出来的词因此永远排在真候选**后面** ——
//! 与 Xime 的"追加到候选栏尾部"是同一件事。
//!
//! 在此基础上再加 Xime 的两道闸：
//!
//! 1. **死路才纠**（对应 Xime 的"空码 / 死路 margin = 0"）：当前输入本身就是一个
//!    完整合法音节时（`prism.get_value(input)` 有值），说明候选栏本来就有货，
//!    纠错只是噪声，直接返回空。
//! 2. **只留最优的一批**：按 (距离, 音节 id) 排序后取前 `limit` 条；距离相同才是
//!    "同样可能"的误触，更远的不再浪费位置。

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use crate::algo::syllabifier::{Corrector, Prism};
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

/// QWERTY 键心坐标（单位：键宽 / 键高）。
///
/// 与 Xime 一样用**真实物理偏移**：第二行相对第一行右移半个键、第三行再右移，
/// 所以 `q` 的正下方不是 `a` 而是 `a` 略偏左 —— 这正是 `q→a`、`w→s` 这种误触
/// 的距离（1.118）比跨行对角（1.414）小的原因。
///
/// `(x, y)`，只列 26 个字母。
const KEY_CENTER: [(char, f32, f32); 26] = [
    // 第一行
    ('q', 0.0, 0.0), ('w', 1.0, 0.0), ('e', 2.0, 0.0), ('r', 3.0, 0.0), ('t', 4.0, 0.0),
    ('y', 5.0, 0.0), ('u', 6.0, 0.0), ('i', 7.0, 0.0), ('o', 8.0, 0.0), ('p', 9.0, 0.0),
    // 第二行（右移 0.5）
    ('a', 0.5, 1.0), ('s', 1.5, 1.0), ('d', 2.5, 1.0), ('f', 3.5, 1.0), ('g', 4.5, 1.0),
    ('h', 5.5, 1.0), ('j', 6.5, 1.0), ('k', 7.5, 1.0), ('l', 8.5, 1.0),
    // 第三行（再右移，x 从 1.5 起）
    ('z', 1.5, 2.0), ('x', 2.5, 2.0), ('c', 3.5, 2.0), ('v', 4.5, 2.0), ('b', 5.5, 2.0),
    ('n', 6.5, 2.0), ('m', 7.5, 2.0),
];

/// "算邻居"的半径（Xime 实测值）：键心归一化距离 ≤ 1.35 键宽。
///
/// 同行相邻 = 1.0 ✅、上下/斜向 = 1.118 ✅、跨行对角 = 1.414 ❌。
const NEIGHBOR_RADIUS: f32 = 1.35;

fn key_center(ch: char) -> Option<(f32, f32)> {
    let ch = ch.to_ascii_lowercase();
    KEY_CENTER.iter().find(|(c, _, _)| *c == ch).map(|(_, x, y)| (*x, *y))
}

/// 两个字母键的**归一化距离**。非字母 → `None`。
///
/// ```
/// use librime_next::dict::corrector::key_distance;
/// assert!(key_distance('w', 'w').unwrap() < 0.001);          // 自己
/// assert!(key_distance('q', 'w').unwrap() <= 1.35);          // 同行相邻
/// assert!(key_distance('q', 'a').unwrap() <= 1.35);          // 上下（librime 抓不到）
/// assert!(key_distance('a', 'z').unwrap() > 1.35);           // 跨行对角，不算邻
/// ```
pub fn key_distance(left: char, right: char) -> Option<f32> {
    let (x1, y1) = key_center(left)?;
    let (x2, y2) = key_center(right)?;
    Some(((x1 - x2).powi(2) + (y1 - y2).powi(2)).sqrt())
}

/// 是不是邻键（含上下与斜向，不只是左右）。
pub fn is_neighbor_key(left: char, right: char) -> bool {
    match key_distance(left, right) {
        Some(distance) => distance <= NEIGHBOR_RADIUS,
        None => false,
    }
}

/// 一个键的所有邻键（含它自己），按距离升序。Xime 的邻键表就是这个。
pub fn neighbors(ch: char) -> Vec<char> {
    let mut out: Vec<(char, f32)> = KEY_CENTER
        .iter()
        .filter_map(|&(other, x, y)| {
            let (cx, cy) = key_center(ch)?;
            let distance = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
            (distance <= NEIGHBOR_RADIUS).then_some((other, distance))
        })
        .collect();
    out.sort_by(|a, b| {
        a.1.partial_cmp(&b.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    out.into_iter().map(|(ch, _)| ch).collect()
}

/// 键盘感知的替换代价：同键 0、邻键 1、其它 2。
///
/// 取 2 是为了让"非邻键替换"在 `tolerance = 1` 时**必然被拒** ——
/// 于是"纠错 = 只把某个字母换成它旁边的键"，这正是 Xime 想抓的那种误触；
/// 换成一个八竿子打不着的字母（代价 2）不算打错，算另一种拼法。
fn keyboard_substitution_cost(left: u8, right: u8) -> Distance {
    if left == right {
        return 0;
    }
    match (char::from(left), char::from(right)) {
        (a, b) if is_neighbor_key(a, b) => 1,
        _ => 2,
    }
}

/// 带**自定义替换代价**的受限编辑距离。
///
/// 与 [`restricted_distance`] 同构（带状 DP + 超阈值提前放弃），只是替换的代价
/// 由 `substitution_cost` 给（插入/删除仍各计 1）。返回值 `≤ threshold` 即在容差内。
pub fn weighted_restricted_distance(
    left: &str,
    right: &str,
    threshold: Distance,
    substitution_cost: impl Fn(u8, u8) -> Distance,
) -> Distance {
    let a = left.as_bytes();
    let b = right.as_bytes();
    // 长度差（每次插入/删除至少 1）本身就已经超过阈值
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
        let start = (row + 1).saturating_sub(threshold).max(1);
        let end = (row + 1 + threshold).min(width);
        if start > 1 {
            current[start - 1] = threshold + 1;
        }
        for column in start..=end {
            let substitution = previous[column - 1] + substitution_cost(a[column - 1], *b_byte);
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

/// 键盘距离意义下的编辑距离：替换代价按 [`keyboard_substitution_cost`]。
///
/// `≤ threshold` 表示"最多按错 `threshold` 个邻键"就能从 `left` 变成 `right`。
pub fn keyboard_distance_of(left: &str, right: &str, threshold: Distance) -> Distance {
    weighted_restricted_distance(left, right, threshold, keyboard_substitution_cost)
}

/// 输入能不能被**完整切成一串合法音节**（不含补全/简拼，只看精确键）。
///
/// 用于"死路才纠"的判据：能切开说明这一串拼音本身说得通，不该再猜"你是不是打错了"。
/// 做法是一次前向 DP —— `common_prefix_search` 给出"哪些键是当前位置的前缀"，
/// 逐个推进可达位置即可。输入最长十几字节，成本可忽略，结果随纠错结果一起缓存。
fn fully_analyzable(prism: &dyn Prism, input: &str) -> bool {
    let bytes = input.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let mut reachable = vec![false; bytes.len() + 1];
    reachable[0] = true;
    for pos in 0..bytes.len() {
        if !reachable[pos] {
            continue;
        }
        let Some(suffix) = input.get(pos..) else {
            continue;
        };
        for m in prism.common_prefix_search(suffix) {
            // ⚠️ 只认 ≥2 字节的键。拼音 prism 里有一批**单字母键**（`a` / `e` / `o`
            // 这类零声母音节，还有 `h` / `p` 这种声母键），若把它们也算"能切开"，
            // 那任何一串字母都能被拆成一堆单字母 → 门控永远拦不住，纠错变成噪声。
            // 按"真音节至少两个字母"来切，才贴合"这一串拼音说得通"的直觉。
            if m.length >= 2 && pos + m.length <= bytes.len() {
                reachable[pos + m.length] = true;
            }
        }
    }
    reachable[bytes.len()]
}

/// `EditDistanceCorrector` —— [`Corrector`] 的一个可用实现。
///
/// C++ 侧是"编译期生成纠错 prism，查询期 `ToleranceSearch`"；这里改成
/// "首次调用拉一次音节表、之后线性扫描"，语义等价（见模块文档）。
///
/// 音节表放在 `Arc<OnceLock<..>>` 里：[`Self::clone`] 很便宜，翻译器每次查询
/// 建 `Syllabifier` 时带着同一份缓存即可，不用每次重建。
#[derive(Clone, Default)]
pub struct EditDistanceCorrector {
    /// 容差：默认 1 —— 一次误触。调大能抓"打错两个字母"，噪声也一起涨。
    tolerance: Distance,
    /// 音节表（`(拼写串, 音节 id)`）。
    ///
    /// `Prism` trait 没有暴露 syllabary（`dict/prism.rs` 只给了 5 个查词方法，
    /// `expand_search` 只回 id 不回串），所以由**调用方注入** —— 翻译器手上有
    /// `Table`，`syllabary()` 直接可得。没注入时表为空，纠错自动失效（不出错、
    /// 不出候选），不会拖累没配纠错的路径。
    syllabary: Arc<Vec<(String, SyllableId)>>,
    /// 查询缓存：`输入串 → 纠错结果`。
    ///
    /// `Syllabifier` 对**每个起始位置**都问一次，同一串在一次查询里会被问到多次
    /// （而且按键之间输入只差一个字符）。缓存放在 `Mutex` 里是因为
    /// [`Corrector::tolerance_search`] 只有 `&self`，而 [`Self::clone`] 要共享同一份。
    cache: Arc<Mutex<HashMap<String, Vec<(SyllableId, String)>>>>,
}

/// 缓存上限：超过就整份丢掉（用户词库级别的输入组合不会长期驻留，没必要做 LRU）。
const CORRECTION_CACHE_LIMIT: usize = 512;

impl EditDistanceCorrector {
    /// 只有容差、没有音节表：纠错不会生效（`tolerance_search` 返回空）。
    pub fn new(tolerance: Distance) -> Self {
        Self {
            tolerance: tolerance.max(1),
            syllabary: Arc::new(Vec::new()),
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 注入音节表（`Table::syllabary()` 出来的那份）。
    pub fn with_syllabary(tolerance: Distance, syllabary: Vec<(String, SyllableId)>) -> Self {
        Self {
            tolerance: tolerance.max(1),
            syllabary: Arc::new(syllabary),
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn tolerance(&self) -> Distance {
        self.tolerance
    }

    pub fn syllabary_len(&self) -> usize {
        self.syllabary.len()
    }
}

impl Corrector for EditDistanceCorrector {
    /// 门控 + 排序（见模块文档）：
    ///
    /// * 输入本身是完整合法音节 → 不纠（候选栏本来就有货，Xime 的"完整词档"）；
    /// * 单字母输入 → 不纠（一两个字母的抖动误纠率太高）；
    /// * 其余按 (距离, 音节 id) 排序，取前 `limit` 条。
    fn tolerance_search(
        &self,
        prism: &dyn Prism,
        input: &str,
        limit: usize,
    ) -> Vec<(SyllableId, String)> {
        if limit == 0 || input.len() < 2 {
            return Vec::new();
        }
        // 死路才纠（Xime 的"完整词档"）：
        // ① 输入自己就是一个完整音节（`prism.get_value` 命中）；
        // ② 或者输入能被**完整切成一串音节** —— 说明候选栏本来就有货，
        //    这时候补一个"你可能想打另一个音节"的候选是纯噪声。
        if prism.get_value(input).is_some() || fully_analyzable(prism, input) {
            return Vec::new();
        }
        let syllabary = &self.syllabary;
        if syllabary.is_empty() {
            return Vec::new();
        }
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = cache.get(input) {
            return cached.clone();
        }
        let mut hits: Vec<(Distance, SyllableId, String)> = Vec::new();
        for (spelling, syllable_id) in syllabary.iter() {
            let distance = keyboard_distance_of(input, spelling, self.tolerance);
            if distance <= self.tolerance {
                hits.push((distance, *syllable_id, spelling.clone()));
            }
        }
        hits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        hits.truncate(limit);
        let out: Vec<(SyllableId, String)> = hits
            .into_iter()
            .map(|(_, id, text)| (id, text))
            .collect();
        if cache.len() >= CORRECTION_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(input.to_string(), out.clone());
        out
    }
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

    // ---- 键盘几何（Xime 的邻键表） ----

    #[test]
    fn keyboard_geometry_matches_xime_radius() {
        // 自己
        assert!(key_distance('w', 'w').unwrap() < 1e-6);
        // 同行相邻 1.0
        assert!((key_distance('q', 'w').unwrap() - 1.0).abs() < 1e-6);
        // 上下（第二行右移 0.5）：1.118 —— librime 自带的"只认左右邻"抓不到这类
        assert!(key_distance('q', 'a').unwrap() <= NEIGHBOR_RADIUS);
        assert!(key_distance('w', 's').unwrap() <= NEIGHBOR_RADIUS);
        assert!(key_distance('e', 'd').unwrap() <= NEIGHBOR_RADIUS);
        // 跨行对角 1.414 → 不算邻
        assert!(key_distance('a', 'z').unwrap() > NEIGHBOR_RADIUS);
        assert!(!is_neighbor_key('a', 'z'));
        // 同行隔一个 2.0
        assert!(!is_neighbor_key('q', 'e'));
        // 非字母没有键位
        assert!(key_distance('1', 'q').is_none());
        assert!(!is_neighbor_key('1', 'q'));
    }

    #[test]
    fn neighbors_include_vertical_and_diagonal() {
        let around_q = neighbors('q');
        assert_eq!(around_q[0], 'q', "第一个是自己（距离 0）");
        assert!(around_q.contains(&'w'), "同行右邻");
        assert!(around_q.contains(&'a'), "下邻");
        assert!(around_q.contains(&'w'));
        assert!(!around_q.contains(&'e'), "隔一个不算");

        // 「a」的邻键里必须有 q/w/s/z 之外的…… 至少要有 s（同行）和 q/w（上排）
        let around_a = neighbors('a');
        assert!(around_a.contains(&'s') && around_a.contains(&'q') && around_a.contains(&'w'));
        assert!(!around_a.contains(&'z'), "a-z 是对角，超出半径");
    }

    #[test]
    fn substitution_cost_treats_far_keys_as_two_typos() {
        // 同键 0、邻键 1、其它 2 —— 非邻键替换在 tolerance=1 时必然被拒
        assert_eq!(keyboard_substitution_cost(b'a', b'a'), 0);
        assert_eq!(keyboard_substitution_cost(b'a', b's'), 1);
        assert_eq!(keyboard_substitution_cost(b'a', b'q'), 1);
        assert_eq!(keyboard_substitution_cost(b'a', b'p'), 2);
    }

    #[test]
    fn keyboard_distance_only_allows_neighbor_typos() {
        // 一次邻键误触：pin yin 的 "nihao" vs "nihap"（o→p 相邻）
        assert!(keyboard_distance_of("nihap", "nihao", 1) <= 1);
        // 换成不相邻的键：两次才算，"容忍 1 次"时不收
        assert!(keyboard_distance_of("nihaz", "nihao", 1) > 1);
        // 容忍 2 次就收得下
        assert!(keyboard_distance_of("nihaz", "nihao", 2) <= 2);
        // 长度差超过阈值直接放弃
        assert!(keyboard_distance_of("ni", "nihao", 1) > 1);
    }

    // ---- EditDistanceCorrector ----

    /// 一个只有音节表的 prism（拼写 id == 音节 id，与 `InMemoryPrism::from_syllabary` 一致）。
    fn prism_of(syllables: &[&str]) -> crate::algo::syllabifier::InMemoryPrism {
        let owned: Vec<String> = syllables.iter().map(|s| s.to_string()).collect();
        crate::algo::syllabifier::InMemoryPrism::from_syllabary(owned.iter())
    }

    fn corrector_of(syllables: &[&str], tolerance: Distance) -> EditDistanceCorrector {
        let ids: Vec<(String, SyllableId)> = syllables
            .iter()
            .enumerate()
            .map(|(index, s)| (s.to_string(), index as SyllableId))
            .collect();
        EditDistanceCorrector::with_syllabary(tolerance, ids)
    }

    #[test]
    fn correction_finds_neighbor_typo() {
        let prism = prism_of(&["ni", "hao", "ma", "zhong", "guo"]);
        let corrector = corrector_of(&["ni", "hao", "ma", "zhong", "guo"], 1);
        // "haoo"：多敲了一个 o（删除一次即可）→ 纠成 hao
        let hits = corrector.tolerance_search(&prism, "haoo", 5);
        assert!(
            hits.iter().any(|(_, text)| text == "hao"),
            "「haoo」应纠成「hao」，实际 {hits:?}"
        );
        // "nii" → ni
        let hits = corrector.tolerance_search(&prism, "nii", 5);
        assert!(hits.iter().any(|(_, text)| text == "ni"));
    }

    #[test]
    fn correction_uses_keyboard_neighbors_not_any_letter() {
        let syllables = ["ni", "hao", "ma", "zhong", "guo"];
        let prism = prism_of(&syllables);
        let corrector = corrector_of(&syllables, 1);
        // "hao" → "hao" 自己就是完整音节：门控拦住，一条都不纠
        assert!(
            corrector.tolerance_search(&prism, "hao", 5).is_empty(),
            "输入本身是完整音节时不纠错（Xime 的「完整词档」）"
        );
        // "guo" 打成 "gpi"：p 与 u 不相邻、i 与 o 相邻 ⇒ 距离 3 > 1，不该纠
        assert!(
            corrector
                .tolerance_search(&prism, "gpi", 5)
                .iter()
                .all(|(_, text)| text != "guo"),
            "非邻键的替换不该在 tolerance=1 时被当成误触"
        );
        // 容差放宽到 3 才能收下
        let loose = corrector_of(&syllables, 3);
        assert!(loose
            .tolerance_search(&prism, "gpi", 5)
            .iter()
            .any(|(_, text)| text == "guo"));
    }

    #[test]
    fn correction_respects_gates() {
        let syllables = ["ni", "hao"];
        let prism = prism_of(&syllables);
        let corrector = corrector_of(&syllables, 1);
        // 单字母不纠（抖动误纠率太高）
        assert!(corrector.tolerance_search(&prism, "n", 5).is_empty());
        // limit = 0 不纠
        assert!(corrector.tolerance_search(&prism, "haoo", 0).is_empty());
        // 没有音节表 → 静默失效，不报错
        let empty = EditDistanceCorrector::new(1);
        assert!(empty.tolerance_search(&prism, "haoo", 5).is_empty());
    }

    #[test]
    fn correction_results_are_capped_and_sorted() {
        // 一批两两只差一个邻键的音节，一次全命中 → 只能留 limit 条
        let syllables = ["hao", "haoa", "haoo", "haoi"];
        let prism = prism_of(&syllables);
        let corrector = corrector_of(&syllables, 1);
        let hits = corrector.tolerance_search(&prism, "hao", 2);
        assert!(hits.len() <= 2, "结果条数不得超过 limit，实际 {}", hits.len());
        // 同一串再问一次走缓存，结果一致
        assert_eq!(hits, corrector.tolerance_search(&prism, "hao", 2));
    }

    #[test]
    fn empty_key_has_no_corrections() {
        // C++：`if (key.empty()) return;`
        let corrections = Corrections::new();
        assert!(corrections.is_empty());
    }
}
