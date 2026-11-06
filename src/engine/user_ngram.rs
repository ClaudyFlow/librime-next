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
        for word in words {
            if word.is_empty() {
                continue;
            }
            // 词频：反复打同一个词 ⇒ 直接抬高它的分数
            self.clock += 1;
            let entry = self.unigram.entry(word.clone()).or_default();
            entry.count = entry.count.saturating_add(1);
            entry.last_used = self.clock;
            self.recent.push(word.clone());
            if self.recent.len() > 4 {
                self.recent.remove(0);
            }
        }
        self.dirty += 1;
        let window = self.recent.clone();
        self.learn_words(&window);
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
    pub fn save(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        use std::io::Write as _;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut out = String::new();
        for word in &self.blocked {
            out.push_str(&format!("x	{word}
"));
        }
        for (word, entry) in &self.unigram {
            out.push_str(&format!("u	{word}	{}
", entry.count));
        }
        for ((first, second), entry) in &self.bigram {
            out.push_str(&format!("b	{first}	{second}	{}
", entry.count));
        }
        for ((a, b, c), entry) in &self.trigram {
            out.push_str(&format!("t	{a}	{b}	{c}	{}
", entry.count));
        }
        let mut file = std::fs::File::create(path)?;
        file.write_all(out.as_bytes())?;
        self.dirty = 0;
        Ok(())
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

    /// 不改用户数据文件：加载侧跳过单字 `x` 行，让历史脏数据自愈。
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

/// 学一次提交并适时落盘。
pub fn learn_and_maybe_flush(words: &[String]) {
    if let Ok(mut ngram) = global().lock() {
        ngram.learn_commit(words);
        if ngram.should_flush() {
            let _ = ngram.save(&store_path());
        }
    }
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
}
