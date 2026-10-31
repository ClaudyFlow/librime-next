//! 在线 n-gram 个人化（学自青简 Qingjian 的 `UserNgram`）。
//!
//! 青简的结论（`C:\Code\qingjian\docs\design\architecture.md` §整句/§个人化）：
//! **个人化优先用在线 n-gram，神经模型只做重排与离线联想**；而且
//! 「本地输入历史不当上下文（跨应用碎片=噪音）」「上屏之后不联想（纯靠猜，已删除）」。
//!
//! 所以这里只做**最小可用**的一件事：从上屏历史里学**二元/三元词频**，
//! 给"上文最后一个词 → 候选词"这个转移打一个加成。它不生成候选、不改候选集合，
//! 只在排序时加一个可调的 `bonus`（`None` = 没学到，等于不存在）。
//!
//! 与青简一致的边界：
//! * 只吃**本进程的上屏记录**（[`CommitHistory`]），不读跨应用碎片；
//! * 不做"上屏后联想"；
//! * 学到的表**有上限**（[`MAX_PAIRS`]），超了按最近使用淘汰，绝不无限涨。

use std::collections::HashMap;

use crate::engine::commit_history::CommitHistory;

/// 最多记多少条转移（二元 + 三元共用这个预算）。
///
/// 青简的产品词库才 8.7 万条、内存占用按 arena 算；这里只是"个人习惯"，
/// 上限给 20000 条足够，超出按最近使用淘汰。
pub const MAX_PAIRS: usize = 20_000;

/// 一次转移的统计。
#[derive(Clone, Copy, Debug, Default)]
struct PairCount {
    count: u32,
    /// 最近一次被用到的时间戳（淘汰用，单调递增即可）。
    last_used: u64,
}

/// 在线学出来的二元/三元表。
#[derive(Default)]
pub struct UserNgram {
    /// `(上文词, 下一个词) → 次数`。
    bigram: HashMap<(String, String), PairCount>,
    /// `(上上个词, 上文词, 下一个词) → 次数`。
    trigram: HashMap<(String, String, String), PairCount>,
    /// 学过的总次数（单调递增，作为 `last_used`）。
    clock: u64,
    /// 因为超上限被丢掉的条数（诊断用）。
    evicted: u64,
}

impl UserNgram {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.bigram.is_empty() && self.trigram.is_empty()
    }

    pub fn bigram_count(&self, prev: &str, next: &str) -> u32 {
        self.bigram
            .get(&(prev.to_string(), next.to_string()))
            .map(|entry| entry.count)
            .unwrap_or(0)
    }

    pub fn trigram_count(&self, first: &str, prev: &str, next: &str) -> u32 {
        self.trigram
            .get(&(first.to_string(), prev.to_string(), next.to_string()))
            .map(|entry| entry.count)
            .unwrap_or(0)
    }

    /// 从**上屏历史**里学一轮（整段重学——测试用；运行时走 [`Self::learn_words`] 增量）。
    pub fn learn(&mut self, history: &CommitHistory) {
        // 历史是逆序迭代（最新在前），这里翻成正序，窗口才是"上文 → 下一个词"。
        let words: Vec<String> = history
            .iter_rev()
            .map(|record| record.text.clone())
            .filter(|text| !text.is_empty())
            .collect();
        let words: Vec<String> = words.into_iter().rev().collect();
        self.learn_words(&words);
    }

    /// 学一批**已按序切好的词**（增量入口——引擎在每次提交时喂本轮的词）。
    ///
    /// 2026-10-31 接线定口径：整段重学会让旧记录反复计数（学习即递增），
    /// 运行时必须**每次提交只喂本次的词**，由调用方负责切词
    /// （多段组合 = 逐段候选；整句提交的回切词待词典贪心，见 1031日志）。
    pub fn learn_words(&mut self, words: &[String]) {
        let words: Vec<String> = words
            .iter()
            .filter(|text| !text.is_empty())
            .cloned()
            .collect();
        if words.len() < 2 {
            return;
        }
        for window in 2..=3 {
            if words.len() < window {
                continue;
            }
            for start in 0..=words.len() - window {
                self.clock += 1;
                match window {
                    2 => {
                        let key = (words[start].clone(), words[start + 1].clone());
                        let entry = self.bigram.entry(key).or_default();
                        entry.count = entry.count.saturating_add(1);
                        entry.last_used = self.clock;
                    }
                    _ => {
                        let key = (
                            words[start].clone(),
                            words[start + 1].clone(),
                            words[start + 2].clone(),
                        );
                        let entry = self.trigram.entry(key).or_default();
                        entry.count = entry.count.saturating_add(1);
                        entry.last_used = self.clock;
                    }
                }
            }
        }
        self.evict_if_needed();
    }

    /// 给候选取一个加成（0.0 = 没学到）。
    ///
    /// 口径与青简/水杉的"线性插值"一致：`bonus = weight * log(1 + count)`，
    /// 由调用方决定 `weight`（建议 0.5 ~ 2.0，按 nats 量级），并且**只加不减**。
    pub fn bonus(
        &self,
        before_last: Option<&str>,
        prev: Option<&str>,
        candidate: &str,
        weight: f64,
    ) -> f64 {
        let Some(prev) = prev else {
            return 0.0;
        };
        let bigram = self.bigram_count(prev, candidate) as f64;
        let trigram = before_last
            .map(|first| self.trigram_count(first, prev, candidate) as f64)
            .unwrap_or(0.0);
        // 三元给双倍权重：更具体，也更说明用户就是这个搭配。
        weight * ((1.0 + bigram).ln() + 2.0 * (1.0 + trigram).ln())
    }

    /// 超上限时按最久未用淘汰，直到回到上限以内。
    fn evict_if_needed(&mut self) {
        while self.bigram.len() > MAX_PAIRS {
            let Some(oldest) = self
                .bigram
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.bigram.remove(&oldest);
            self.evicted += 1;
        }
        while self.trigram.len() > MAX_PAIRS {
            let Some(oldest) = self
                .trigram
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.trigram.remove(&oldest);
            self.evicted += 1;
        }
    }

    /// 被淘汰掉的条数（诊断/日志用）。
    pub fn evicted(&self) -> u64 {
        self.evicted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::commit_history::{CommitHistory, CommitRecord};

    fn history(words: &[&str]) -> CommitHistory {
        let mut history = CommitHistory::new();
        for word in words {
            history.push(CommitRecord::new("test", *word));
        }
        history
    }

    #[test]
    fn learns_bigram_from_commit_history() {
        let mut ngram = UserNgram::new();
        assert!(ngram.is_empty());
        ngram.learn(&history(&["今天", "天气", "很好"]));
        assert_eq!(ngram.bigram_count("今天", "天气"), 1);
        assert_eq!(ngram.bigram_count("天气", "很好"), 1);
        assert_eq!(ngram.bigram_count("很好", "今天"), 0);
        // 三元也对齐了
        assert_eq!(ngram.trigram_count("今天", "天气", "很好"), 1);
    }

    #[test]
    fn bonus_prefers_learned_transition_and_degrades_without_context() {
        let mut ngram = UserNgram::new();
        ngram.learn(&history(&["今天", "天气", "很好"]));
        // 学过：>0
        let learned = ngram.bonus(None, Some("天气"), "很好", 1.0);
        assert!(learned > 0.0, "{learned}");
        // 没学过：0
        assert_eq!(ngram.bonus(None, Some("天气"), "不错", 1.0), 0.0);
        // 没有上文：0（不做无约束的瞎猜）
        assert_eq!(ngram.bonus(None, None, "很好", 1.0), 0.0);
        // 三元比只看二元更重
        let with_before = ngram.bonus(Some("今天"), Some("天气"), "很好", 1.0);
        assert!(with_before > learned, "{with_before} vs {learned}");
    }

    #[test]
    fn repeated_learning_increases_bonus() {
        let mut ngram = UserNgram::new();
        ngram.learn(&history(&["你", "好"]));
        let once = ngram.bonus(None, Some("你"), "好", 1.0);
        ngram.learn(&history(&["你", "好"]));
        let twice = ngram.bonus(None, Some("你"), "好", 1.0);
        assert!(twice > once, "{twice} vs {once}");
        assert_eq!(ngram.bigram_count("你", "好"), 2);
    }
}

/// 进程级个人 n-gram 表（本进程学习、本进程使用——青简边界：不跨应用）。
pub fn global() -> &'static std::sync::Mutex<UserNgram> {
    static GLOBAL: std::sync::OnceLock<std::sync::Mutex<UserNgram>> = std::sync::OnceLock::new();
    GLOBAL.get_or_init(|| std::sync::Mutex::new(UserNgram::new()))
}

/// 默认加成权重（`translator/ngram_weight` 配置化之前先取 1.0）。
pub const DEFAULT_WEIGHT: f64 = 1.0;

/// 从提交历史里取"作上下文的最后两个词"。
///
/// 口径（2026-10-31 定）：只吃**选自菜单**的记录（`raw` = 英文/标点/直接注入，丢弃）、
/// **>1 字**（单字丢弃，防"的→是"噪声）。返回 `(上上个, 上一个)`。
pub fn last_two_words(history: &CommitHistory) -> (Option<String>, Option<String>) {
    let word_like = |record: &crate::engine::commit_history::CommitRecord| {
        record.kind != crate::engine::commit_history::KIND_RAW
            && record.text.chars().count() > 1
            && !record.text.is_empty()
    };
    let mut iter = history.iter_rev().filter(|record| word_like(record));
    let prev = iter.next().map(|record| record.text.clone());
    let before_last = iter.next().map(|record| record.text.clone());
    (before_last, prev)
}

#[cfg(test)]
mod wiring_tests {
    use super::*;

    /// 口径过滤:raw 丢弃、单字丢弃(2026-10-31 接线定稿)。
    #[test]
    fn last_two_words_filters_raw_and_single_chars() {
        let mut history = CommitHistory::new();
        history.push(CommitRecord::new("raw", "hello"));
        history.push(CommitRecord::new("test", "的"));
        history.push(CommitRecord::new("test", "天气"));
        history.push(CommitRecord::new("test", "很好"));
        let (before, prev) = last_two_words(&history);
        assert_eq!(prev.as_deref(), Some("很好"));
        assert_eq!(before.as_deref(), Some("天气"), "raw 与单字不吃进上下文");
    }

    /// 提交即学习:learn_words 增量喂词,同一批只计一次。
    #[test]
    fn incremental_learn_words_counts_once_per_feed() {
        let mut ngram = UserNgram::new();
        ngram.learn_words(&["今天".to_string(), "天气".to_string(), "很好".to_string()]);
        assert_eq!(ngram.bigram_count("今天", "天气"), 1);
        // 引擎在每次提交时只喂本轮的词——同批重喂才递增(与青简语义一致)。
        ngram.learn_words(&["天气".to_string(), "很好".to_string()]);
        assert_eq!(ngram.bigram_count("天气", "很好"), 2);
    }
}
