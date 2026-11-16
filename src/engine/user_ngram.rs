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
    /// 单个词的提交次数（用户反复打同一个词时，靠它进首选——不依赖上文）。
    unigram: HashMap<String, PairCount>,
    /// 被用户**手动删掉**的词（`Ctrl+Delete`）：永不抬分，并压低排序。
    blocked: std::collections::HashSet<String>,
    /// 最近提交过的词（最多 4 个）：单次提交往往只有 1 个词，必须带上文才凑得出二元组。
    recent: Vec<String>,
    /// 自上次落盘以来提交的次数（每 N 次写一次盘，避免每键都写文件）。
    dirty: u32,
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
    /// **运行时增量入口**：把本次提交的词接到"最近窗口"后面再学。
    ///
    /// 为什么需要它：一次提交常常只有一个词（用户点了一条候选），
    /// `learn_words` 要求 ≥2 个词，直接喂就会静默不学（2026-11-01 用户报
    /// "打过十几遍还是不在首选"就是这个原因）。
    pub fn learn_commit(&mut self, words: &[String]) {
        self.learn_commit_incremental(words);
    }

    /// [`Self::learn_commit`] 的增量实现：只数本次新形成的转移。
    ///
    /// 旧实现把「最近 4 个词」整窗重学，同一个 bigram/trigram 会在后续几次
    /// 提交里被反复 +1（打 4 个词后计数约为真实转移次数的数倍），而线性头
    /// 与 `bonus` 都直接读这些计数。这里改成：窗口仍然带上下文，
    /// 但只统计**末词落在本次新增词上**的转移，每条转移每次提交只记一次。
    fn learn_commit_incremental(&mut self, words: &[String]) {
        let old_len = self.recent.len();
        let mut combined: Vec<String> =
            Vec::with_capacity(old_len.saturating_add(words.len()));
        combined.extend(self.recent.iter().cloned());
        for word in words {
            if word.is_empty() {
                continue;
            }
            // 词频：反复打同一个词 ⇒ 直接抬高它的分数
            self.clock += 1;
            let entry = self.unigram.entry(word.clone()).or_default();
            entry.count = entry.count.saturating_add(1);
            entry.last_used = self.clock;
            combined.push(word.clone());
        }
        self.dirty += 1;
        self.learn_transitions_since(&combined, old_len);
        let keep_from = combined.len().saturating_sub(4);
        self.recent = combined.split_off(keep_from);
    }

    /// 只统计「末词下标 ≥ `new_start`」的二元/三元转移。
    ///
    /// `words` = 旧窗口 + 本次新词；`new_start` = 旧窗口长度。
    /// 每条转移的末词若是新词，就在这次提交里记一次；下一轮它变成旧词后
    /// 不会再被重复计数。
    fn learn_transitions_since(&mut self, words: &[String], new_start: usize) {
        if words.len() < 2 {
            return;
        }
        for window in 2..=3 {
            if words.len() < window {
                continue;
            }
            for start in 0..=words.len() - window {
                if start + window - 1 < new_start {
                    continue;
                }
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

    /// 线性重排头用的三项计数：`(unigram, bigram(prev→candidate), trigram)`。
    pub fn counts_for(
        &self,
        before_last: Option<&str>,
        prev: Option<&str>,
        candidate: &str,
    ) -> (u32, u32, u32) {
        let unigram = self.unigram_count(candidate);
        let bigram = prev.map_or(0, |prev| self.bigram_count(prev, candidate));
        let trigram = match (before_last, prev) {
            (Some(first), Some(prev)) => self.trigram_count(first, prev, candidate),
            _ => 0,
        };
        (unigram, bigram, trigram)
    }

    /// 用户删词：从统计里抹掉，并拉黑（打错一次就永远别再冒头）。
    ///
    /// ⚠️ 2026-11-06 修：**单字不拉黑**。这份 `blocked` 是全局黑名单
    /// （`is_blocked_global` 在每个 translator 的 `peek()` 里都查一次），
    /// 「是」「的」这类字来自系统词典，一次误删就会让它们从**所有方案**里永久消失。
    /// 删词功能针对的是「造错的词」（≥2 字），不是系统词典里的基础单字。
    pub fn penalize(&mut self, word: &str) {
        if word.chars().count() < 2 {
            return;
        }
        self.blocked.insert(word.to_string());
        self.unigram.remove(word);
        self.bigram.retain(|(_, next), _| next != word);
        self.trigram.retain(|(_, _, next), _| next != word);
        self.dirty += 1;
    }

    /// 这个词是否被拉黑。
    pub fn is_blocked(&self, word: &str) -> bool {
        self.blocked.contains(word)
    }

    /// 落盘（TSV：`b<TAB>词1<TAB>词2<TAB>次数` / `t<TAB>词1<TAB>词2<TAB>词3<TAB>次数`）。
    ///
    /// 2026-11-09：按键路径不再调用它（见 [`request_flush`]）；只剩退出时同步落盘。
    pub fn save(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        let text = self.to_tsv();
        write_atomic(path, &text)?;
        self.dirty = 0;
        Ok(())
    }

    /// 把整表格式化成 TSV 文本（纯内存，不碰磁盘；后台落盘线程用）。
    fn to_tsv(&self) -> String {
        let mut out = String::new();
        for word in &self.blocked {
            out.push_str(&format!("x\t{word}\n"));
        }
        for (word, entry) in &self.unigram {
            out.push_str(&format!("u\t{word}\t{}\n", entry.count));
        }
        for ((first, second), entry) in &self.bigram {
            out.push_str(&format!("b\t{first}\t{second}\t{}\n", entry.count));
        }
        for ((a, b, c), entry) in &self.trigram {
            out.push_str(&format!("t\t{a}\t{b}\t{c}\t{}\n", entry.count));
        }
        out
    }

    /// 读盘；文件不存在时静默返回。
    pub fn load(&mut self, path: &std::path::Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        for line in text.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            match parts.first().copied() {
                Some("x") if parts.len() == 2 => {
                    // 历史脏数据自愈：老版本可能把误删的单字写成 `x\t是`。
                    // 不再加载它，下次 save() 会自然把这一行抹掉。
                    if parts[1].chars().count() >= 2 {
                        self.blocked.insert(parts[1].to_string());
                    }
                }
                Some("u") if parts.len() == 3 => {
                    if let Ok(count) = parts[2].parse::<u32>() {
                        self.clock += 1;
                        self.unigram.insert(parts[1].to_string(), PairCount { count, last_used: self.clock });
                    }
                }
                Some("b") if parts.len() == 4 => {
                    if let Ok(count) = parts[3].parse::<u32>() {
                        self.clock += 1;
                        self.bigram.insert(
                            (parts[1].to_string(), parts[2].to_string()),
                            PairCount { count, last_used: self.clock },
                        );
                    }
                }
                Some("t") if parts.len() == 5 => {
                    if let Ok(count) = parts[4].parse::<u32>() {
                        self.clock += 1;
                        self.trigram.insert(
                            (parts[1].to_string(), parts[2].to_string(), parts[3].to_string()),
                            PairCount { count, last_used: self.clock },
                        );
                    }
                }
                _ => {}
            }
        }
    }

    /// 该落盘了吗（每 10 次提交写一次）。
    pub fn should_flush(&self) -> bool {
        self.dirty >= 10
    }

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
        // ⚠️ 2026-11-01 修：以前这里先 `let Some(prev) = prev else { return 0.0 }`，
        // 于是**词频加成和黑名单在"没有上文"时完全不生效**——用户打第一个词
        // （如 yinamiao → 一纳秒）永远拿不到加成、删了也压不下去。
        // 现在把「拉黑」和「词频」提到最前，二元/三元才受上文约束。
        if self.is_blocked(candidate) {
            return -weight * 1000.0; // 黑名单：压到几乎最后（10 太小，压不过词典权重）
        }
        let unigram = self
            .unigram
            .get(candidate)
            .map(|entry| entry.count as f64)
            .unwrap_or(0.0);
        let unigram_bonus = weight * (1.0 + unigram).ln() * 4.0; // 词频权重 1.5→4.0（要让"常打的词"压过词典序）
        let Some(prev) = prev else {
            return unigram_bonus;
        };
        let bigram = self.bigram_count(prev, candidate) as f64;
        let trigram = before_last
            .map(|first| self.trigram_count(first, prev, candidate) as f64)
            .unwrap_or(0.0);
        // 三元给双倍权重：更具体，也更说明用户就是这个搭配。
        unigram_bonus + weight * ((1.0 + bigram).ln() + 2.0 * (1.0 + trigram).ln())
    }

    /// 预测(2026-11-01,用户拍板"预测做进去"):给定上文最后两个词,
    /// 返回最可能的下文词(三元优先,其次二元),按计数降序最多 `limit` 个。
    ///
    /// 与青简"上屏后不联想"边界不同——**用户 11-01 更新了边界**:预测做进
    /// n-gram 体系。空表/没学过返回空。
    pub fn predict(&self, before_last: Option<&str>, prev: Option<&str>, limit: usize) -> Vec<String> {
        let Some(prev) = prev else { return Vec::new() };
        // 三元先(更具体),二元补充;按 (key, count) 聚合取最高。
        let mut scores: HashMap<String, u32> = HashMap::new();
        for ((first, second, next), entry) in &self.trigram {
            if second == prev && before_last.map_or(true, |b| b == first) {
                let slot = scores.entry(next.clone()).or_default();
                *slot = slot.saturating_add(entry.count * 2);
            }
        }
        for ((second, next), entry) in &self.bigram {
            if second == prev {
                let slot = scores.entry(next.clone()).or_default();
                *slot = slot.saturating_add(entry.count);
            }
        }
        let mut ranked: Vec<(String, u32)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked.truncate(limit);
        ranked.into_iter().map(|(word, _)| word).collect()
    }

    /// 单个词被上屏的总次数（0 = 没学过）。
    pub fn unigram_count(&self, word: &str) -> u32 {
        self.unigram.get(word).map(|entry| entry.count).unwrap_or(0)
    }

    /// 预测候选的「用户口径」版：`predict` 按转移计数排序后，只留
    /// **上屏 ≥ `min_count` 次**的多字中文词，并跳过用户删过的词。
    ///
    /// 2026-11-07 用户口径：「可以预测一些上屏十次以上的用户词」。
    /// `predict` 会先截断，所以这里要给它一个够大的池子（64）再按门槛筛。
    pub fn predict_frequent(
        &self,
        before_last: Option<&str>,
        prev: Option<&str>,
        limit: usize,
        min_count: u32,
    ) -> Vec<String> {
        let mut out = Vec::with_capacity(limit);
        for word in self.predict(before_last, prev, 64) {
            if out.len() >= limit {
                break;
            }
            if word.chars().count() < 2 || !word.chars().any(is_cjk_char) {
                continue;
            }
            if self.is_blocked(&word) || self.unigram_count(&word) < min_count {
                continue;
            }
            out.push(word);
        }
        out
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

    /// 2026-11-06 回归（用户三问之一）：「为什么小鹤里面没有『是』这个字」。
    ///
    /// 根因是 `%APPDATA%/Rime-Next/user_ngram.tsv` 里有一条 `x\t是`：
    /// `x` 行是**全局**黑名单，`ScriptWordsTranslation::peek()` 对每个候选都查一次，
    /// 于是「是」在所有方案里被永久过滤。61 条 `x` 行里只有这一条是单字 ⇒ 误删。
    /// 删词语义针对「造错的词」（≥2 字），单字不该进黑名单。
    #[test]
    fn single_character_words_are_never_blocked() {
        let mut ngram = UserNgram::new();
        ngram.penalize("是");
        assert!(!ngram.is_blocked("是"), "单字不进黑名单");
        ngram.penalize("是后");
        assert!(ngram.is_blocked("是后"), "多字删词照旧拉黑");
        assert!(!is_blocked_global("是"), "全局查询也对单字兜底");
    }

    /// 选哪个词作下文「预测候选」：只放上屏 ≥10 次的词（插进 `unigram`/`bigram` 造统计）。
    ///
    /// 必须同时有正负样本：只测「9 次不出」会让门槛写坏（比如恒返回空）也全绿。
    #[test]
    fn predict_frequent_requires_ten_commits() {
        fn seed(ngram: &mut UserNgram, word: &str, count: u32, transition: u32) {
            ngram.unigram.insert(
                word.to_string(),
                PairCount { count, last_used: 0 },
            );
            ngram.bigram.insert(
                ("然后".to_string(), word.to_string()),
                PairCount { count: transition, last_used: 0 },
            );
        }
        let mut ngram = UserNgram::new();
        seed(&mut ngram, "现在", 9, 50); // 差一次：不推
        seed(&mut ngram, "这个", 10, 5); // 正好到门槛：推
        seed(&mut ngram, "单", 30, 60); // 单字：不推
        seed(&mut ngram, "hello", 30, 60); // 非中文：不推
        let got = ngram.predict_frequent(None, Some("然后"), PREDICT_LIMIT, PREDICT_MIN_COUNT);
        assert_eq!(
            got,
            vec!["这个".to_string()],
            "9 次不出、10 次出、单字与非中文不出；实际 {got:?}"
        );
        // 用户删过的词（黑名单）即使次数够也不推。
        ngram.penalize("这个");
        assert!(
            ngram
                .predict_frequent(None, Some("然后"), PREDICT_LIMIT, PREDICT_MIN_COUNT)
                .is_empty(),
            "黑名单词不进预测"
        );
    }

    /// 没有上文（历史不足）时宁可空着，也不做无约束瞎猜。
    #[test]
    fn predictions_without_context_are_empty() {
        let context = crate::engine::context::Context::new();
        assert!(predictions_for_context(&context).is_empty());
    }

    /// 提交历史的接线：`predictions_for_context` 读的就是 `last_two_words` 那套口径
    /// （raw/单字不进上下文）。这里只钉住取词，不依赖进程级全局表。
    #[test]
    fn prediction_context_uses_last_two_menu_words() {
        let mut history = CommitHistory::new();
        history.push(CommitRecord::new("raw", "hello"));
        history.push(CommitRecord::new("test", "的"));
        history.push(CommitRecord::new("test", "天气"));
        history.push(CommitRecord::new("test", "很好"));
        let (before, prev) = last_two_words(&history);
        assert_eq!(prev.as_deref(), Some("很好"));
        assert_eq!(before.as_deref(), Some("天气"));
    }

    #[test]
    fn load_self_heals_legacy_single_char_block_rows() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "lrn_ngram_block_{}_{}.tsv",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, "x\t是\nx\t海市\nu\t就是\t9\n").unwrap();
        let mut ngram = UserNgram::new();
        ngram.load(&path);
        assert!(!ngram.is_blocked("是"), "历史单字 x 行不再生效");
        assert!(ngram.is_blocked("海市"), "多字 x 行照旧生效");
        let _ = std::fs::remove_file(path);
    }
}

/// 进程级个人 n-gram 表（本进程学习、本进程使用——青简边界：不跨应用）。
pub fn global() -> &'static std::sync::Mutex<UserNgram> {
    static GLOBAL: std::sync::OnceLock<std::sync::Mutex<UserNgram>> = std::sync::OnceLock::new();
    GLOBAL.get_or_init(|| {
        let mut ngram = UserNgram::new();
        ngram.load(&store_path());
        std::sync::Mutex::new(ngram)
    })
}

/// 个人 n-gram 的落盘位置（%APPDATA%/Rime-Next/user_ngram.tsv）。
pub fn store_path() -> std::path::PathBuf {
    std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join("Rime-Next")
        .join("user_ngram.tsv")
}

/// 后台落盘信号：按键线程只 `notify_one`，磁盘 I/O 全在 worker 线程。
struct FlushSignal {
    pending: bool,
    seq: u64,
}

fn flush_signal() -> &'static (std::sync::Mutex<FlushSignal>, std::sync::Condvar) {
    static SIGNAL: std::sync::OnceLock<(std::sync::Mutex<FlushSignal>, std::sync::Condvar)> =
        std::sync::OnceLock::new();
    SIGNAL.get_or_init(|| {
        (
            std::sync::Mutex::new(FlushSignal { pending: false, seq: 0 }),
            std::sync::Condvar::new(),
        )
    })
}

/// 请求后台落盘：按键路径唯一该调用的落盘入口（只改标记 + `notify_one`）。
pub fn request_flush() {
    ensure_flush_worker();
    let (lock, cond) = flush_signal();
    if let Ok(mut signal) = lock.lock() {
        signal.pending = true;
        signal.seq = crate::engine::stage_trace::current();
        cond.notify_one();
    }
}

fn ensure_flush_worker() {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    STARTED.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("user-ngram-flush".into())
            .spawn(flush_loop);
    });
}

/// 后台落盘循环：请求立刻写；没有请求时最多等 [`IDLE_FLUSH`](5s) 也检查一次 dirty，
/// 保证"空闲/定时"都会最终落盘；失败则重新挂起并按 1s→30s 退避重试。
fn flush_loop() {
    const IDLE_FLUSH: std::time::Duration = std::time::Duration::from_secs(5);
    let mut retry = std::time::Duration::from_secs(1);
    loop {
        let seq = {
            let (lock, cond) = flush_signal();
            let Ok(mut signal) = lock.lock() else {
                std::thread::sleep(IDLE_FLUSH);
                continue;
            };
            if !signal.pending {
                let Ok((guard, _)) = cond.wait_timeout(signal, IDLE_FLUSH) else {
                    std::thread::sleep(IDLE_FLUSH);
                    continue;
                };
                signal = guard;
            }
            // 有请求：用请求携带的 seq（和触发它的那次提交对齐）；
            // 空闲超时：用当前 seq。取完立刻清掉，避免下一次定时落盘复用旧 id。
            let seq = if signal.pending {
                signal.seq
            } else {
                crate::engine::stage_trace::current()
            };
            signal.pending = false;
            signal.seq = 0;
            seq
        };
        match flush_once(seq) {
            Ok(true) => retry = std::time::Duration::from_secs(1),
            Ok(false) => {}
            Err(_) => {
                let (lock, cond) = flush_signal();
                if let Ok(mut signal) = lock.lock() {
                    signal.pending = true;
                    cond.notify_one();
                }
                std::thread::sleep(retry);
                retry = (retry * 2).min(std::time::Duration::from_secs(30));
            }
        }
    }
}

/// 落一次盘：锁内只做内存快照（`to_tsv`），写文件在锁外。
///
/// 返回 `Ok(false)` 表示当前没有脏数据、无需写盘；`Err` 由调用方重试。
fn flush_once(seq: u64) -> std::io::Result<bool> {
    let start = std::time::Instant::now();
    let (text, dirty) = {
        let Ok(ngram) = global().lock() else {
            return Ok(false);
        };
        if ngram.dirty == 0 {
            return Ok(false);
        }
        (ngram.to_tsv(), ngram.dirty)
    };
    write_atomic(&store_path(), &text)?;
    if let Ok(mut ngram) = global().lock() {
        // 只扣掉本次快照覆盖的批次数；期间新学的保持 dirty，等下一轮再写。
        ngram.dirty = ngram.dirty.saturating_sub(dirty);
    }
    crate::engine::stage_trace::emit(seq, "ngram_flush", start.elapsed().as_micros());
    Ok(true)
}

/// 先写临时文件再改名，避免"写一半"把正式文件截断（崩溃最多丢最近一批）。
///
/// 重排头的参数文件也走这一份（`pub(crate)` 是为了不复制第二套原子写）。
pub(crate) fn write_atomic(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "user_ngram.tsv".to_string());
    let tmp = path.with_file_name(format!("{name}.tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 引擎 finalize / 进程退出：同步落一次（失败不重试，最坏丢最近一批）。
pub fn flush_on_exit() {
    let _ = flush_once(crate::engine::stage_trace::current());
}

/// 候选生成期的快速判定：这个词被用户删过吗？
///
/// 独立的 lock scope（不能和 `bonus` 的锁重入），命中即跳过该候选。
/// 放在这里而不是让各处自己 `global().lock()`，是为了避免把锁的生命周期
/// 拖进候选构造（容易和 `bonus` 撞成死锁）。
pub fn is_blocked_global(word: &str) -> bool {
    // 单字候选来自系统词典，不归用户删词管（见 `penalize` / `load` 的说明）。
    // 这一层是兜底：即便有别处往 `blocked` 里塞了单字，也不该过滤它。
    if word.chars().count() < 2 {
        return false;
    }
    global()
        .lock()
        .map(|ngram| ngram.is_blocked(word))
        .unwrap_or(false)
}

/// 学一次提交并适时请求后台落盘（按键路径不碰磁盘）。
pub fn learn_and_maybe_flush(words: &[String]) {
    // 第一次学词就把后台线程拉起来：这样即便提交数还没到 10（未触发
    // `request_flush`），空闲 5s 的定时落盘也会生效，崩溃丢得更少。
    ensure_flush_worker();
    let flush = {
        let Ok(mut ngram) = global().lock() else {
            return;
        };
        ngram.learn_commit(words);
        ngram.should_flush()
    };
    // 锁已释放再通知，避免和后台线程的（signal → global）顺序交叉成死锁。
    if flush {
        request_flush();
    }
}

/// 预测候选的门槛：只推**用户已经上屏 ≥10 次**的多字词（2026-11-07 用户口径）。
pub const PREDICT_MIN_COUNT: u32 = 10;

/// 一次最多推几条（联想条一行放得下；2026-11-08 双神经模型对齐成 4 条）。
pub const PREDICT_LIMIT: usize = 4;

/// 判断一个字符是不是 CJK 统一汉字（预测候选只收中文词）。
fn is_cjk_char(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c)
}

/// `apply_commit` 用：按提交历史最后两个词预测下一个词，套用户门槛（上屏 ≥10 次）。
///
/// 2026-11-07 用户原话：「学习得还是很慢，可以预测一些上屏十次以上的用户词」。
/// 没上文（历史不足一个词）时不做无约束瞎猜，返回空——青简删掉「上屏后联想」
/// 的理由就是没有拼音约束的纯猜；加上上文约束后才是这条链路成立的边界。
pub fn predictions_for_context(context: &crate::engine::context::Context) -> Vec<String> {
    let (before_last, prev) = last_two_words(&context.commit_history);
    let Some(prev) = prev else {
        return Vec::new();
    };
    match global().lock() {
        Ok(ngram) => ngram.predict_frequent(
            before_last.as_deref(),
            Some(prev.as_str()),
            PREDICT_LIMIT,
            PREDICT_MIN_COUNT,
        ),
        Err(_) => Vec::new(),
    }
}

/// 线性重排头的特征读取入口：一次拿齐三项计数（锁只取一次）。
///
/// 供 `rerank_head` 在候选打分/学习时复用，语义与 [`UserNgram::counts_for`] 一致。
pub fn counts(
    before_last: Option<&str>,
    prev: Option<&str>,
    candidate: &str,
) -> (u32, u32, u32) {
    global()
        .lock()
        .map(|ngram| ngram.counts_for(before_last, prev, candidate))
        .unwrap_or((0, 0, 0))
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
    use crate::engine::commit_history::CommitRecord;

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

    /// `learn_commit` 带上下文窗口，但**只数新增转移**：同一个 bigram
    /// 不会在后续几次提交里被反复 +1（线性头的特征依赖这一点）。
    #[test]
    fn learn_commit_counts_each_transition_once() {
        let mut ngram = UserNgram::new();
        let feed = |ngram: &mut UserNgram, word: &str| ngram.learn_commit(&[word.to_string()]);
        feed(&mut ngram, "你");
        feed(&mut ngram, "好");
        assert_eq!(ngram.bigram_count("你", "好"), 1);
        assert_eq!(ngram.unigram_count("你"), 1);
        // 第三个词：只新增 (好→吗) 与 (你,好,吗)，(你→好) 不再重记。
        feed(&mut ngram, "吗");
        assert_eq!(ngram.bigram_count("你", "好"), 1, "旧 bigram 被整窗重学了");
        assert_eq!(ngram.bigram_count("好", "吗"), 1);
        assert_eq!(ngram.trigram_count("你", "好", "吗"), 1);
        // 第四个词：(好,吗) 与 (你,好,吗) 都不再重记。
        feed(&mut ngram, "啊");
        assert_eq!(ngram.bigram_count("好", "吗"), 1);
        assert_eq!(ngram.trigram_count("你", "好", "吗"), 1);
        assert_eq!(ngram.trigram_count("好", "吗", "啊"), 1);
        assert_eq!(ngram.unigram_count("啊"), 1);
    }
}
