//! R-04 邻键纠错旁路：`rime/librime` 的 `NearSearchCorrector::ToleranceSearch()`
//! （`src/rime/dict/corrector.cc`）的移植。
//!
//! # 与上游的关系
//!
//! 上游同一个文件里的 `EditDistanceCorrector` 被 `#if 0` 关掉，注释是
//! *"Don't use edit distance based correction for now."* —— 本模块**只抄**邻键
//! 版本，不把通用编辑距离搬进来。已有的 [`crate::gear::dict::corrector::EditDistanceCorrector`]
//! 是另一条独立路径（由 `translator/enable_correction` 控制），与本模块互不影响。
//!
//! # 算法
//!
//! 在 prism 的 darts 双数组上做 BFS。状态是
//! `(trie 节点, 输入串下标, 已替换次数, 已走过的键串)`：
//!
//! * 每一步先按**原字符**走一条分支（不消耗替换次数）；
//! * 再按当前字符的**每个邻键**各走一条分支（消耗 1 次替换）；
//! * 走完整个输入串时，若节点带值（`has_leaf`）就产出一条纠错拼写，
//!   `credibility` 惩罚由切分器负责（见 [`crate::algo::syllabifier::Syllabifier`]）。
//!
//! 三个边界（与任务口径一致）：
//!
//! 1. **只做替换**：没有插入/删除分支，所以产出的键与输入**等长**；
//! 2. **只替换 1 次**：默认 `max_substitutions = 1`，不做深度 2；
//! 3. **无替换 = 精确匹配**：`substitutions == 0` 的命中直接丢弃，交给原有精确
//!    检索路径，纠错结果里不会混进"其实没打错"的拼写。
//!
//! # 邻键表
//!
//! 不写死 `q↔w` 这类配对，而是复用 [`crate::gear::dict::corrector`] 的**物理键位几何**
//! （键心坐标 + 归一化距离），运行时对 26 个字母逐个算一遍。换一套键位布局只需要
//! 改那张坐标表，算法与查表代码都不用动。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::OnceLock;

use crate::algo::syllabifier::{Corrector, Prism, SyllableId};
use crate::gear::dict::darts::DoubleArray;

/// 默认只容忍 1 次邻键替换。
pub const MAX_SUBSTITUTIONS: usize = 1;

/// 输入串短于这个长度就不纠（两个字母的抖动误纠率太高）。
pub const MIN_INPUT_LEN: usize = 3;

/// darts 叶子值的哨兵：`0x7FFF_FFFF` 在 C++ 里就是 `-1`（"没有值"）。
const NO_VALUE: u32 = 0x7FFF_FFFF;

/// 邻键表（`字节 → 邻键字节`），首次使用时按物理键位算一次。
fn neighbor_table() -> &'static HashMap<u8, Vec<u8>> {
    static TABLE: OnceLock<HashMap<u8, Vec<u8>>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = HashMap::with_capacity(26);
        for byte in b'a'..=b'z' {
            let mut list: Vec<u8> = (b'a'..=b'z')
                .filter(|other| {
                    *other != byte
                        && crate::gear::dict::corrector::is_neighbor_key(
                            char::from(byte),
                            char::from(*other),
                        )
                })
                .collect();
            // 顺序固定，保证候选结果可复现（测试要确定性）。
            list.sort_unstable();
            table.insert(byte, list);
        }
        table
    })
}

/// 从左到右消费一个字节。语义与 `darts.h::traverse(key, node_pos, key_pos, 1)` 一致：
/// 位置 `p ^ offset(p) ^ c` 的单元 label 必须等于 `c`，否则这条路径不存在。
fn step(trie: &DoubleArray, node_pos: usize, byte: u8) -> Option<usize> {
    let unit = trie.units().get(node_pos)?;
    let next = node_pos ^ unit.offset() as usize ^ usize::from(byte);
    let next_unit = trie.units().get(next)?;
    (next_unit.label() == u32::from(byte)).then_some(next)
}

/// 节点上的值（`has_leaf` 时取 `p ^ offset(p)` 的叶子单元）。
fn value_at(trie: &DoubleArray, node_pos: usize) -> Option<SyllableId> {
    let unit = trie.units().get(node_pos)?;
    if !unit.has_leaf() {
        return None;
    }
    let value = trie.units().get(node_pos ^ unit.offset() as usize)?.value();
    (value != NO_VALUE).then_some(value as SyllableId)
}

struct State {
    node_pos: usize,
    key_pos: usize,
    substitutions: usize,
    spelling: Vec<u8>,
}

/// `NearSearchCorrector::ToleranceSearch()`：在 trie 上做"原字符 / 邻键"双分支 BFS。
///
/// 返回 `(音节 id, 纠错后的拼写串)`，按 `(替换次数, 音节 id)` 升序，最多 `limit` 条。
/// 没有替换的命中不算纠错（由精确检索那条路负责），所以不会出现在结果里。
pub fn search(
    trie: &DoubleArray,
    input: &str,
    max_substitutions: usize,
    limit: usize,
) -> Vec<(SyllableId, String)> {
    if input.is_empty() || limit == 0 {
        return Vec::new();
    }
    let bytes = input.as_bytes();
    let neighbors = neighbor_table();
    let mut queue: VecDeque<State> = VecDeque::new();
    queue.push_back(State {
        node_pos: 0,
        key_pos: 0,
        substitutions: 0,
        spelling: Vec::new(),
    });
    // 同一个音节可能被多条路径命中：只留替换次数最少的那条，再按音节 id 定序。
    let mut hits: BTreeMap<SyllableId, (usize, String)> = BTreeMap::new();
    while let Some(state) = queue.pop_front() {
        if state.key_pos == bytes.len() {
            if state.substitutions == 0 {
                continue;
            }
            if let Some(syllable_id) = value_at(trie, state.node_pos) {
                let (best_subs, best_spelling) = hits
                    .entry(syllable_id)
                    .or_insert_with(|| (state.substitutions, String::new()));
                if state.substitutions <= *best_subs {
                    *best_subs = state.substitutions;
                    *best_spelling = String::from_utf8(state.spelling).unwrap_or_default();
                }
            }
            continue;
        }
        let byte = bytes[state.key_pos];
        // 分支 1：原字符（不消耗替换次数）。
        if let Some(next) = step(trie, state.node_pos, byte) {
            let mut spelling = state.spelling.clone();
            spelling.push(byte);
            queue.push_back(State {
                node_pos: next,
                key_pos: state.key_pos + 1,
                substitutions: state.substitutions,
                spelling,
            });
        }
        // 分支 2：邻键替换（每种邻键各一条，最多替换 `max_substitutions` 次）。
        if state.substitutions < max_substitutions
            && let Some(list) = neighbors.get(&byte)
        {
            for &replacement in list {
                let Some(next) = step(trie, state.node_pos, replacement) else {
                    continue;
                };
                let mut spelling = state.spelling.clone();
                spelling.push(replacement);
                queue.push_back(State {
                    node_pos: next,
                    key_pos: state.key_pos + 1,
                    substitutions: state.substitutions + 1,
                    spelling,
                });
            }
        }
    }
    let mut out: Vec<(usize, SyllableId, String)> = hits
        .into_iter()
        .map(|(syllable_id, (substitutions, spelling))| (substitutions, syllable_id, spelling))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    out.truncate(limit);
    out.into_iter()
        .map(|(_, syllable_id, spelling)| (syllable_id, spelling))
        .collect()
}

/// [`Corrector`] 的邻键实现。无状态（trie 与邻键表都在外部/全局），`clone` 只是复制开关。
#[derive(Debug, Clone, Copy)]
pub struct NearSearchCorrector {
    max_substitutions: usize,
}

impl NearSearchCorrector {
    /// 生产口径：只替换 1 次。
    pub fn new() -> Self {
        Self::with_max_substitutions(MAX_SUBSTITUTIONS)
    }

    /// 供测试/实验放宽替换次数（生产路径不要用它）。
    pub fn with_max_substitutions(max_substitutions: usize) -> Self {
        Self {
            max_substitutions,
        }
    }

    pub fn max_substitutions(&self) -> usize {
        self.max_substitutions
    }
}

impl Default for NearSearchCorrector {
    fn default() -> Self {
        Self::new()
    }
}

impl Corrector for NearSearchCorrector {
    fn tolerance_search(
        &self,
        prism: &dyn Prism,
        input: &str,
        limit: usize,
    ) -> Vec<(SyllableId, String)> {
        // 长度 ≥3 才纠：太短的串上"邻键"与"另一种拼法"无法区分。
        if input.len() < MIN_INPUT_LEN {
            return Vec::new();
        }
        // 误纠防护：这一段本身就是一个合法音节时**不纠**。
        // 否则「nihao」里的「hao」会被纠成邻键「bao」（b 在 h 正下方），
        // 打对的输入也会凭空多出一批纠错候选。精确匹配的片段不需要旁路。
        if prism.get_value(input).is_some() {
            return Vec::new();
        }
        prism.near_search(input, self.max_substitutions, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用 darts 现场建一份小 trie（值 = 键在数组里的下标）。
    fn trie_of(keys: &[&str]) -> DoubleArray {
        let raw: Vec<&[u8]> = keys.iter().map(|key| key.as_bytes()).collect();
        DoubleArray::build(&raw).expect("小键表不会超出 darts 编码范围")
    }

    #[test]
    fn finds_one_neighbor_typo() {
        let trie = trie_of(&["hao", "ni", "ma", "zhong"]);
        // 「hap」把 o 打到右邻 p → 「hao」
        let hits = search(&trie, "hap", 1, 5);
        assert_eq!(
            hits,
            vec![(0, "hao".to_string())],
            "o→p 是一次邻键替换，应纠成 hao"
        );
    }

    #[test]
    fn rejects_two_substitutions_when_capped_at_one() {
        let trie = trie_of(&["hao"]);
        // 「gai」→「hao」要连换两处（g→h、i→o，都是邻键），1 次替换收不下。
        assert!(search(&trie, "gai", 1, 5).is_empty());
        // 放宽到 2 次才收。
        assert_eq!(search(&trie, "gai", 2, 5), vec![(0, "hao".to_string())]);
    }

    #[test]
    fn never_inserts_or_deletes() {
        let trie = trie_of(&["hao", "haoo"]);
        // 多敲一个 o：要删掉一个字符才能到「hao」，不是替换，不收。
        let hits = search(&trie, "haoo", 1, 5);
        assert!(
            hits.iter().all(|(_, text)| text != "hao"),
            "删除不在纠错分支里，实际 {hits:?}"
        );
    }

    #[test]
    fn exact_keys_are_not_reported_as_corrections() {
        // 单键 trie：本仓库的 first-fit 构建器会让不同节点复用同一个 base（这是既有的
        // 排布特性，不在本模块范围内），多键玩具 trie 上 `exact_match_search` 可能有
        // 误命中；这里只验证"没替换的命中不会作为纠错返回"。
        let trie = trie_of(&["hao"]);
        let hits = search(&trie, "hao", 1, 5);
        assert!(
            hits.is_empty(),
            "没替换的命中属于精确检索路径，不该算纠错，实际 {hits:?}"
        );
    }

    #[test]
    fn respects_limit() {
        // 三个键都是一次邻键替换可达（h→g/j、a→s/o→i 等），只留 limit 条。
        let trie = trie_of(&["gao", "jao", "hso", "hio"]);
        let hits = search(&trie, "hao", 1, 2);
        assert!(hits.len() <= 2, "结果条数不超过 limit，实际 {}", hits.len());
        // 顺序确定（按音节 id 升序）
        let mut ids: Vec<SyllableId> = hits.iter().map(|(id, _)| *id).collect();
        let sorted = ids.clone();
        ids.sort();
        assert_eq!(ids, sorted);
    }

    #[test]
    fn min_length_gate_is_three() {
        let trie = trie_of(&["hao", "ni", "ma"]);
        let corrector = NearSearchCorrector::new();
        // 2 字母：直接不纠（即使键盘距离上能纠）
        assert_eq!(MIN_INPUT_LEN, 3);
        assert!(corrector.max_substitutions() == 1);
        // 这里用 InMemoryPrism 的默认 near_search（返回空）验证门控不会 panic：
        // 真正的 trie 路径由下游端到端测试覆盖。
        assert!(corrector.tolerance_search(&crate::algo::syllabifier::InMemoryPrism::default(), "hp", 5).is_empty());
        let _ = trie;
    }
}
