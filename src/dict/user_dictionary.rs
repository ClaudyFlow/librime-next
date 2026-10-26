//! librime `dict/user_dictionary.h`(121) + `user_dictionary.cc`(622) 的移植：
//! 用户词典的**运行时视图**——学词、调频、以及把用户词掺进候选。
//!
//! # 它凭什么能"一遍扫完"
//!
//! userdb 的键是 `"<码串> <TAB> <词条>"`，而 LevelDB 按**字节序**排列键。
//! 于是"同一个码串下的所有词条"天然连续，前缀扫一遍就能全拿到。
//! `DfsLookup`（`user_dictionary.cc:216-303`）就是利用这一点：
//!
//! 1. 按音节图的 `indices`（`start` → 音节 id → 拼写属性）逐段拼出**码串前缀**；
//! 2. `ForwardScan(prefix)` 把游标跳到该前缀；
//! 3. 只有键等于 `"<前缀><TAB>..."` 才算命中（`IsExactMatch`）；
//! 4. 输入还没走完就递归到下一段；走到头了、且请求预测，就把
//!    `"<前缀>..."` 的所有键都当"预测词"收下（`IsPrefixMatch`）。
//!
//! 注解（`user_dictionary.cc:205-214`）里那两条 `shsh` / `aaab` 的例子说的是同一件事：
//! **前向扫不能回退**，所以同一位置上的多个拼写必须按字典序访问，
//! 靠的是 `SyllableGraph::indices` 已经按 `SyllableId` 排好序这个前提。
//!
//! # 与 C++ 的接口差异
//!
//! * **`Attach(table, prism)` → 参数 `&dyn UserDictSource`**。C++ 让 `UserDictionary`
//!   持有 table/prism 两个 `an<>`；Rust 侧改成"调用时传一个窄接口"
//!   （[`UserDictSource`]，由 `Dictionary` 实现），好处是不用把 `Dictionary`
//!   塞进 `Arc` 里到处拷，也让单测能塞一个假词典。
//! * **`Lookup(..., string* resume_key)` → 返回 `(计数, 下一个 resume_key)`**。
//! * **`Load()` 不自带恢复线程**。C++ 打开失败会往 `Deployer` 排一个
//!   `userdb_recovery_task`；LRN 没有工作线程池，所以只返回 `false`，
//!   恢复交给部署链（`lever::deployment_tasks` 的 `userdb_recovery_task`）去做。
//! * **`CreateDictEntry` 的 `weight`**：`log(max(weight, DBL_EPSILON)) + credibility`
//!   —— `DBL_EPSILON` 取 `f64::EPSILON`（2.220446049250313e-16）。

use std::collections::{BTreeMap, HashMap};

use super::db::Db;
use super::dictionary::Dictionary;
use super::user_db::UserDbValue;
use super::vocabulary::{
    Code, DictEntry, DictEntryFilterBinder, DictEntryList, Syllabary, SyllableId,
};
use crate::algo::dynamics::{formula_d, formula_p};
use crate::algo::SpellingType;
use crate::algo::syllabifier::SyllableGraph;

/// `UserDictionary` 需要从词典那边拿到的东西（C++ 靠 `Attach(table, prism)` 拿到两个指针）。
///
/// 只要这两条：把音节 id 翻回拼写串（拼 userdb 的码串前缀）、以及整张音节表
/// （"预测词"分支要把码串里没用到的音节也翻成 id）。
pub trait UserDictSource {
    /// `Table::GetSyllableById(id)`。未知 id 返回空串。
    fn syllable_by_id(&self, syllable_id: SyllableId) -> String;
    /// `Table::GetSyllabary(&out)`。
    fn syllabary(&self) -> Option<Syllabary>;
}

impl UserDictSource for Dictionary {
    fn syllable_by_id(&self, syllable_id: SyllableId) -> String {
        self.table.get_syllable_by_id(syllable_id)
    }

    fn syllabary(&self) -> Option<Syllabary> {
        self.table.syllabary()
    }
}

/// `using TickCount = uint64_t;`（`user_db.h:19`）。
pub type TickCount = u64;

/// `kDiscardThreshold = 1e-200`（`user_dictionary.cc:533`）：
/// 衰减到这个量级以下的用户词直接丢弃，不再出现在候选里。
const K_DISCARD_THRESHOLD: f64 = 1e-200;

/// `DfsState::depth()`
/// —— DFS 走到第几个音节。
///
/// `UserDictEntryIterator`：一次查询拿到的用户词列表 + 游标 + 过滤器。
///
/// 排序规则与 `DictEntryList` 一致（`weight` 降序）；`sort_range` 用于
/// `LookupWords` 的"精确匹配段单独排序"。
#[derive(Default)]
pub struct UserDictEntryIterator {
    cache: DictEntryList,
    index: usize,
    filter: DictEntryFilterBinder,
}

impl UserDictEntryIterator {
    pub fn new() -> Self {
        Self::default()
    }

    /// 由已排好序的列表直接构造（`SetEntries`）。
    pub fn from_entries(entries: DictEntryList) -> Self {
        Self {
            cache: entries,
            index: 0,
            filter: None,
        }
    }

    /// `UserDictEntryIterator::Add`。
    pub fn add(&mut self, entry: DictEntry) {
        self.cache.push(entry);
    }

    /// `UserDictEntryIterator::SetEntries`。
    pub fn set_entries(&mut self, entries: DictEntryList) {
        self.cache = entries;
    }

    /// `UserDictEntryIterator::SortRange(start, count)`。
    pub fn sort_range(&mut self, start: usize, count: usize) {
        self.cache.sort_range(start, count);
    }

    /// `UserDictEntryIterator::AddFilter`：追加一条过滤条件，并**立即**跳过
    /// 当前已经不满足的条目（`user_dictionary.cc:117-124`）。
    pub fn add_filter(&mut self, filter: super::vocabulary::DictEntryFilter) {
        self.filter = Some(match self.filter.take() {
            Some(existing) => Box::new(move |entry| existing(entry) && filter(entry)),
            None => filter,
        });
        while !self.exhausted() && !self.accepts() {
            self.find_next_entry();
        }
    }

    fn accepts(&self) -> bool {
        match (&self.filter, self.cache.get(self.index)) {
            (Some(filter), Some(entry)) => filter(entry),
            _ => true,
        }
    }

    /// `UserDictEntryIterator::Peek`。
    pub fn peek(&self) -> Option<&DictEntry> {
        self.cache.get(self.index)
    }

    /// `UserDictEntryIterator::FindNextEntry`。
    fn find_next_entry(&mut self) -> bool {
        if self.exhausted() {
            return false;
        }
        self.index += 1;
        !self.exhausted()
    }

    /// `UserDictEntryIterator::Next`：前进一条，跳过被 filter 拒掉的。
    pub fn next(&mut self) -> bool {
        if !self.find_next_entry() {
            return false;
        }
        while !self.accepts() {
            if !self.find_next_entry() {
                return false;
            }
        }
        true
    }

    /// `exhausted()`。
    pub fn exhausted(&self) -> bool {
        self.index >= self.cache.len()
    }

    /// `cache_size()`。
    pub fn cache_size(&self) -> usize {
        self.cache.len()
    }

    /// 取走剩下的全部条目（调用方通常只需要文本）。
    pub fn into_entries(self) -> DictEntryList {
        self.cache
    }

    /// 拷一份条目（调用方只有 `&self` 时用，比如从 collector 里取出来看）。
    pub fn clone_entries(&self) -> DictEntryList {
        self.cache.clone()
    }
}

/// `using UserDictEntryCollector = map<size_t, UserDictEntryIterator>`
/// （`user_dictionary.h:39`）：`起始位置 → 该位置上的用户词`。
pub type UserDictEntryCollector = BTreeMap<usize, UserDictEntryIterator>;

/// `struct DfsState`：一次 `Lookup` 的可变状态。
struct DfsState {
    depth_limit: usize,
    predict_word_from_depth: usize,
    present_tick: TickCount,
    code: Vec<SyllableId>,
    credibility: Vec<f64>,
    quality_len: Vec<f64>,
    query_result: BTreeMap<usize, DictEntryList>,
    accessor: Box<dyn super::db::DbAccessor>,
    key: String,
    value: String,
}

impl DfsState {
    fn depth(&self) -> usize {
        self.code.len()
    }

    /// `IsExactMatch(prefix)`：键以 `"<prefix><TAB>"` 开头 = 码串正好用完。
    fn is_exact_match(&self, prefix: &str) -> bool {
        self.key.starts_with(prefix) && self.key[prefix.len()..].starts_with('\t')
    }

    /// `IsPrefixMatch(prefix)`：键以 `prefix` 开头（输入还没走完，或要预测）。
    fn is_prefix_match(&self, prefix: &str) -> bool {
        self.key.starts_with(prefix)
    }

    fn next_entry(&mut self) -> bool {
        match self.accessor.get_next_record() {
            Some((key, value)) => {
                self.key = key;
                self.value = value;
                true
            }
            None => {
                self.key.clear();
                self.value.clear();
                false
            }
        }
    }

    fn forward_scan(&mut self, prefix: &str) -> bool {
        if !self.accessor.jump(prefix) {
            return false;
        }
        self.next_entry()
    }

    /// `DfsState::RecruitEntry`。
    ///
    /// `syllabary` 有值 = "预测词"分支：键里可能还有没用到的音节，
    /// 要把整条码串翻成 `Vec<SyllableId>` 存进 `entry.code`，
    /// 而 `matching_code_size` 记"用户真正输入了几段"。
    fn recruit_entry(&mut self, pos: usize, syllabary: Option<&HashMap<String, SyllableId>>) {
        let mut full_code = String::new();
        let Some(mut entry) = UserDictionary::create_dict_entry(
            &self.key,
            &self.value,
            self.present_tick,
            *self.credibility.last().unwrap_or(&0.0),
            *self.quality_len.last().unwrap_or(&0.0),
            syllabary.map(|_| &mut full_code),
        ) else {
            return;
        };
        match syllabary {
            Some(syllabary) => {
                let mut numeric_code = Vec::new();
                for syllable in
                    crate::algo::strings::split(&full_code, " ", crate::algo::strings::SplitBehavior::SkipToken)
                {
                    let Some(id) = syllabary.get(syllable.as_str()) else {
                        eprintln!(
                            "[user_dictionary] failed to recruit entry {:?}: unrecognized syllable {syllable:?}",
                            entry.text
                        );
                        return;
                    };
                    numeric_code.push(*id);
                }
                entry.code = Code::from(numeric_code);
                entry.matching_code_size = self.code.len() as i32;
            }
            None => entry.code = Code::from_slice(&self.code),
        }
        self.query_result.entry(pos).or_default().push(entry);
    }
}

/// `class UserDictionary`：挂在某个词典名上的用户词典。
pub struct UserDictionary {
    name: String,
    db: std::sync::Arc<dyn Db>,
    /// `hash_map<string, SyllableId> syllabary_`：只在"预测词"分支用到，
    /// 第一次需要时从 table 建，之后缓存。
    syllabary: HashMap<String, SyllableId>,
    tick: TickCount,
}

impl UserDictionary {
    /// `UserDictionary::UserDictionary(name, db)`。
    pub fn new(name: &str, db: std::sync::Arc<dyn Db>) -> Self {
        Self {
            name: name.to_string(),
            db,
            syllabary: HashMap::new(),
            tick: 0,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn db(&self) -> &std::sync::Arc<dyn Db> {
        &self.db
    }

    /// `UserDictionary::Load()`：打开 db 并读出 tick。
    ///
    /// 打开失败时不排恢复任务（LRN 没有工作线程池），只返回 `false`，
    /// 由部署链的 `userdb_recovery_task` 负责修。
    pub fn load(&mut self) -> bool {
        if self.db.disabled() {
            return false;
        }
        if !self.db.loaded() && !self.db.open() {
            eprintln!("[user_dictionary] failed to open db '{}'.", self.name);
            return false;
        }
        self.fetch_tick_count() || self.initialize()
    }

    pub fn loaded(&self) -> bool {
        !self.db.disabled() && self.db.loaded()
    }

    pub fn readonly(&self) -> bool {
        self.db.readonly()
    }

    pub fn tick(&self) -> TickCount {
        self.tick
    }

    /// `Initialize()`：新库把 tick 写成 0。
    fn initialize(&mut self) -> bool {
        self.tick = 0;
        self.db.meta_update("/tick", "0")
    }

    /// `FetchTickCount()`：老版本曾把 tick 写在空键里，两条路都试。
    fn fetch_tick_count(&mut self) -> bool {
        let value = self
            .db
            .meta_fetch("/tick")
            .or_else(|| self.db.fetch(""));
        let Some(value) = value else {
            return false;
        };
        match value.parse::<TickCount>() {
            Ok(tick) => {
                self.tick = tick;
                true
            }
            Err(_) => false,
        }
    }

    /// `UpdateTickCount(increment)`：tick 是"现在几点"，学词时 +1。
    pub fn update_tick_count(&mut self, increment: TickCount) -> bool {
        self.tick += increment;
        self.db.meta_update("/tick", &self.tick.to_string())
    }

    /// `TranslateCodeToString(code)`：`Vec<SyllableId>` → `"a b c "`（尾随空格）。
    fn translate_code_to_string(
        &mut self,
        source: &dyn UserDictSource,
        code: &[SyllableId],
    ) -> Option<String> {
        // C++ 有 `rev_syllabary_` 缓存；这里 table 查询已经是 O(1)，不另建缓存。
        let mut result = String::new();
        for &syllable_id in code {
            let spelling = source.syllable_by_id(syllable_id);
            if spelling.is_empty() {
                eprintln!("[user_dictionary] error translating syllable_id '{syllable_id}'.");
                return None;
            }
            result.push_str(&spelling);
            result.push(' ');
        }
        Some(result)
    }

    /// `UserDictionary::Lookup(...)`：把音节图里**以 `start_pos` 起**的用户词全捞出来。
    ///
    /// * `depth_limit != 0` 时最多递归这么多段音节；
    /// * `predict_word_from_depth != 0` 时，输入走完且已走过这么多段，就开始收"预测词"；
    /// * `initial_credibility` 是起点可信度（通常是 0，模糊音会给负值）。
    pub fn lookup(
        &mut self,
        source: &dyn UserDictSource,
        syll_graph: &SyllableGraph,
        start_pos: usize,
        depth_limit: usize,
        predict_word_from_depth: usize,
        initial_credibility: f64,
    ) -> Option<UserDictEntryCollector> {
        if !self.loaded() || start_pos >= syll_graph.interpreted_length {
            return None;
        }
        self.fetch_tick_count();
        let accessor = self.db.query("")?;
        let mut state = DfsState {
            depth_limit,
            predict_word_from_depth,
            present_tick: self.tick + 1,
            code: Vec::new(),
            credibility: vec![initial_credibility],
            quality_len: vec![0.0],
            query_result: BTreeMap::new(),
            accessor,
            key: String::new(),
            value: String::new(),
        };
        // `Lookup` 里游标先跳到 " " 跳过元数据
        state.accessor.jump(" ");
        self.dfs_lookup(source, syll_graph, start_pos, "", &mut state);
        if state.query_result.is_empty() {
            return None;
        }
        // 每组同音词按 weight 排序；要预测时，若打头的是预测词而组里存在精确匹配，
        // 把精确匹配转到最前面（`user_dictionary.cc:336-354`）。
        for entries in state.query_result.values_mut() {
            entries.sort();
            if predict_word_from_depth != 0 {
                if entries.0.first().is_some_and(DictEntry::is_predictive_match) {
                    if let Some(found) = entries.0.iter().position(DictEntry::is_exact_match) {
                        entries.0[..=found].rotate_right(1);
                    }
                }
            }
        }
        let mut collector = UserDictEntryCollector::new();
        for (pos, entries) in state.query_result {
            collector.insert(pos, UserDictEntryIterator::from_entries(entries));
        }
        Some(collector)
    }

    /// `DfsLookup`（`user_dictionary.cc:216-303`）。
    fn dfs_lookup(
        &mut self,
        source: &dyn UserDictSource,
        syll_graph: &SyllableGraph,
        current_pos: usize,
        current_prefix: &str,
        state: &mut DfsState,
    ) {
        let Some(index) = syll_graph.indices().get(&current_pos) else {
            return;
        };
        // 把 spellings 借出来，避免与 &mut self 冲突
        let spellings: Vec<(SyllableId, Vec<(SpellingType, f64, usize)>)> = index
            .iter()
            .map(|(id, props)| {
                (
                    *id,
                    props
                        .iter()
                        .map(|p| (p.r#type, p.credibility, p.end_pos()))
                        .collect(),
                )
            })
            .collect();

        for (syllable_id, props_list) in spellings {
            state.code.push(syllable_id);
            let translate_result = self.translate_code_to_string(source, &state.code);
            if translate_result.is_none() {
                state.code.pop();
                continue;
            }
            let prefix = translate_result.unwrap();

            for (i, (spelling_type, credibility, end_pos)) in props_list.into_iter().enumerate() {
                // 简拼及更弱的拼写只在第 0 个位置算（`i > 0 && type >= kAbbreviation`）
                if i > 0 && spelling_type as i32 >= SpellingType::Abbreviation as i32 {
                    continue;
                }
                let base_credibility = *state.credibility.last().unwrap_or(&0.0);
                let base_quality_len = *state.quality_len.last().unwrap_or(&0.0);
                state.credibility.push(base_credibility + credibility);
                // 全码匹配长度积分：只有完整拼写才计长度
                let delta_quality_len = if spelling_type == SpellingType::Normal {
                    (end_pos.saturating_sub(current_pos)) as f64
                } else {
                    0.0
                };
                state.quality_len.push(base_quality_len + delta_quality_len);

                let mut descend = true;
                if prefix != state.key && !state.forward_scan(&prefix) {
                    // 扫到 db 末尾，没有这个前缀
                    descend = false;
                }
                if descend {
                    while state.is_exact_match(&prefix) {
                        state.recruit_entry(end_pos, None);
                        if !state.next_entry() {
                            break;
                        }
                    }
                    match syll_graph.indices().get(&end_pos) {
                        None => {
                            // 输入走完，按需预测
                            if state.predict_word_from_depth != 0
                                && state.depth() >= state.predict_word_from_depth
                            {
                                self.ensure_syllabary(source);
                                while state.is_prefix_match(&prefix) {
                                    let syllabary = self.syllabary.clone();
                                    state.recruit_entry(end_pos, Some(&syllabary));
                                    if !state.next_entry() {
                                        break;
                                    }
                                }
                            }
                        }
                        Some(_) => {
                            let within_depth = state.depth_limit == 0
                                || state.depth() < state.depth_limit;
                            if within_depth && state.is_prefix_match(&prefix) {
                                self.dfs_lookup(source, syll_graph, end_pos, &prefix, state);
                            }
                        }
                    }
                }

                state.credibility.pop();
                state.quality_len.pop();
            }

            state.code.pop();
            if !state.is_prefix_match(current_prefix) {
                return;
            }
        }
    }

    /// 懒建 `syllabary_`（C++ 在预测分支里现建一次）。
    fn ensure_syllabary(&mut self, source: &dyn UserDictSource) {
        if !self.syllabary.is_empty() {
            return;
        }
        let Some(syllabary) = source.syllabary() else {
            eprintln!("[user_dictionary] failed to get syllabary for user dict: {}", self.name);
            return;
        };
        self.syllabary = build_syllabary_map(&syllabary);
    }

    /// `UserDictionary::LookupWords(...)`：按**输入串前缀**直接扫 db。
    ///
    /// 与 [`Self::lookup`] 的区别：不经过音节图，纯字符串前缀匹配。
    /// `predictive = false` 时只收码串正好等于输入的条目；`true` 时也收更长的。
    ///
    /// 返回 `(收下的条数, 下一个 resume_key)`。`resume_key` 用于分页：
    /// 传上一轮拿到的键，就接着往下扫。
    pub fn lookup_words(
        &self,
        result: &mut UserDictEntryIterator,
        input: &str,
        predictive: bool,
        limit: usize,
        resume_key: Option<&str>,
    ) -> (usize, Option<String>) {
        /// C++ 的哨兵值（`user_dictionary.cc:368`）：不可能出现的键，表示"没有下一页"。
        const K_END: &str = "\u{ff}";
        let present_tick = self.tick + 1;
        let len = input.len();
        let start = result.cache_size();
        let mut count = 0usize;
        let mut exact_match_count = 0usize;

        let Some(mut accessor) = self.db.query(input) else {
            return (0, Some(K_END.to_string()));
        };
        if accessor.exhausted() {
            return (0, Some(K_END.to_string()));
        }
        if let Some(resume) = resume_key.filter(|key| !key.is_empty()) {
            if !accessor.jump(resume) {
                return (0, Some(K_END.to_string()));
            }
            // C++ 在 Jump 之后还要 GetNextRecord 一次：resume_key 指向的行已经读过了
            if accessor.get_next_record().is_none() {
                return (0, Some(K_END.to_string()));
            }
        }

        let mut key;
        let mut last_key = String::new();
        let mut resume_out = None;
        while let Some((current_key, value)) = accessor.get_next_record() {
            key = current_key;
            // 精确匹配 = 键的下一字节正好是空格（码串在 len 处结束）
            let is_exact_match = key.len() > len && key.as_bytes()[len] == b' ';
            if !is_exact_match && !predictive {
                key = last_key;
                break;
            }
            last_key = key.clone();
            let mut full_code = String::new();
            let Some(mut entry) = Self::create_dict_entry(
                &key,
                &value,
                present_tick,
                1.0,
                len as f64,
                Some(&mut full_code),
            ) else {
                continue;
            };
            entry.custom_code = full_code.clone();
            let trimmed = full_code.trim_end_matches(' ');
            if trimmed.len() > len {
                entry.comment = format!("~{}", &trimmed[len..]);
                entry.remaining_code_length = (trimmed.len() - len) as i32;
            }
            result.add(entry);
            count += 1;
            if is_exact_match {
                exact_match_count += 1;
            } else if limit != 0 && count >= limit {
                break;
            }
            resume_out = Some(last_key.clone());
            if !is_exact_match {
                // 预测段只读一条就够本轮的 limit；继续由 caller 用 resume_key 续
            }
        }
        if exact_match_count > 0 {
            result.sort_range(start, exact_match_count);
        }
        (count, resume_out.or(Some(K_END.to_string())))
    }

    /// `UpdateEntry(entry, commits)`（`user_dictionary.cc:421-457`）。
    pub fn update_entry(
        &mut self,
        source: &dyn UserDictSource,
        entry: &DictEntry,
        commits: i32,
    ) -> bool {
        self.update_entry_with_prefix(source, entry, commits, "")
    }

    /// 带 `new_entry_prefix` 的版本：新词可以插到别的键下面（如 `affix_segmentor` 的 `~`）。
    pub fn update_entry_with_prefix(
        &mut self,
        source: &dyn UserDictSource,
        entry: &DictEntry,
        commits: i32,
        new_entry_prefix: &str,
    ) -> bool {
        let code_str = if entry.custom_code.is_empty() {
            match self.translate_code_to_string(source, &entry.code) {
                Some(code) => code,
                None => return false,
            }
        } else {
            entry.custom_code.clone()
        };
        let mut key = format!("{code_str}\t{}", entry.text);
        let mut value = UserDbValue::default();
        if let Some(raw) = self.db.fetch(&key) {
            if let Some(parsed) = UserDbValue::unpack(&raw) {
                value = parsed;
            }
            if value.tick > self.tick {
                value.tick = self.tick; // 修掉异常时间戳
            }
        } else if !new_entry_prefix.is_empty() {
            key.insert_str(0, new_entry_prefix);
        }
        if commits > 0 {
            if value.commits < 0 {
                value.commits = -value.commits; // 复活被删的词
            }
            value.commits += commits;
            self.update_tick_count(1);
            value.dee = formula_d(commits as f64, self.tick as f64, value.dee, value.tick as f64);
        } else if commits == 0 {
            const K: f64 = 0.1;
            value.dee = formula_d(K, self.tick as f64, value.dee, value.tick as f64);
        } else {
            // 标记删除
            value.commits = (-1).min(-value.commits);
            value.dee = formula_d(0.0, self.tick as f64, value.dee, value.tick as f64);
        }
        value.tick = self.tick;
        self.db.update(&key, &value.pack())
    }

    /// `NewTransaction()`：提交上一个事务，再开新的。
    pub fn new_transaction(&mut self) -> bool {
        let Some(tx) = self.db.as_transactional() else {
            return false;
        };
        self.commit_pending_transaction();
        tx.begin_transaction()
    }

    /// `CommitPendingTransaction()`。
    pub fn commit_pending_transaction(&self) -> bool {
        match self.db.as_transactional() {
            Some(tx) if tx.in_transaction() => tx.commit_transaction(),
            _ => false,
        }
    }

    /// `RevertRecentTransaction()`：只有 3 秒内的事务能撤（防误删学过的词）。
    pub fn revert_recent_transaction(&self, elapsed_secs: u64) -> bool {
        let Some(tx) = self.db.as_transactional() else {
            return false;
        };
        if !tx.in_transaction() || elapsed_secs > 3 {
            return false;
        }
        tx.abort_transaction()
    }

    /// `UserDictionary::CreateDictEntry`（`user_dictionary.cc:535-574`）。
    ///
    /// 三道门槛，任一不过就 `None`：
    /// 1. 键里没有 TAB → 不是 userdb 的键；
    /// 2. 值解析失败，或 `commits < 0`（被用户删了）；
    /// 3. `tick != 0` 且衰减后 `dee <= 1e-200`（老到没意义了）。
    ///
    /// `tick == 0` 的是"从没进过 userdb 的词"（`custom_phrase.txt` 之类），
    /// 不参与衰减，也不该被丢弃。
    pub fn create_dict_entry(
        key: &str,
        value: &str,
        present_tick: TickCount,
        credibility: f64,
        quality_len: f64,
        full_code: Option<&mut String>,
    ) -> Option<DictEntry> {
        let separator_pos = key.find('\t')?;
        let mut v = UserDbValue::unpack(value)?;
        if v.commits < 0 {
            return None;
        }
        if v.tick < present_tick {
            v.dee = formula_d(0.0, present_tick as f64, v.dee, v.tick as f64);
        }
        if v.tick != 0 && v.dee <= K_DISCARD_THRESHOLD {
            return None;
        }
        let mut entry = DictEntry::default();
        entry.text = key[separator_pos + 1..].to_string();
        entry.commit_count = v.commits;
        // weight = log(max(weight, DBL_EPSILON)) + credibility
        let weight = formula_p(
            0.0,
            v.commits as f64 / present_tick as f64,
            present_tick as f64,
            v.dee,
        );
        entry.weight = (if weight > 0.0 { weight } else { f64::EPSILON }).ln() + credibility;
        entry.quality_len = quality_len;
        if let Some(full_code) = full_code {
            *full_code = key[..separator_pos].to_string();
        }
        Some(entry)
    }
}

/// `Syllabary` → `hash_map<string, SyllableId>`（C++ 在预测分支里现建的映射）。
fn build_syllabary_map(syllabary: &Syllabary) -> HashMap<String, SyllableId> {
    syllabary
        .syllable_to_id()
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::level_db::LevelDb;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "lrn_userdict_{tag}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 建一个已经打开的空用户词典。
    fn new_user_dict(tag: &str) -> (TempDir, UserDictionary) {
        let dir = TempDir::new(tag);
        let db = Arc::new(LevelDb::new_user_db(dir.path(), "test"));
        let mut user_db = UserDictionary::new("test", db);
        assert!(user_db.load(), "新库应能创建并初始化");
        (dir, user_db)
    }

    #[test]
    fn load_initializes_tick_to_zero() {
        let (_dir, user_db) = new_user_dict("init");
        assert_eq!(user_db.tick(), 0);
        assert!(user_db.loaded());
        assert_eq!(
            user_db.db().meta_fetch("/tick").as_deref(),
            Some("0")
        );
    }

    #[test]
    fn create_dict_entry_rejects_deleted_and_decayed() {
        // 被删（commits 负数）
        assert!(UserDictionary::create_dict_entry("a \t阿", "c=-1 d=1 t=1", 10, 0.0, 0.0, None).is_none());
        // 没有 TAB 的键
        assert!(UserDictionary::create_dict_entry("abc", "c=1 d=1 t=1", 10, 0.0, 0.0, None).is_none());
        // 值坏掉
        assert!(UserDictionary::create_dict_entry("a \t阿", "c=x", 10, 0.0, 0.0, None).is_none());
        // tick==0（非用户历史词）不参与丢弃判定
        assert!(UserDictionary::create_dict_entry("a \t阿", "c=1 d=0 t=0", 10, 0.0, 0.0, None).is_some());
        // 正常词
        let entry = UserDictionary::create_dict_entry(
            "nihao \t你好",
            "c=3 d=5 t=10",
            11,
            1.0,
            5.0,
            None,
        )
        .unwrap();
        assert_eq!(entry.text, "你好");
        assert_eq!(entry.commit_count, 3);
        assert_eq!(entry.quality_len, 5.0);
        assert!(entry.weight.is_finite());
    }

    #[test]
    fn create_dict_entry_exposes_full_code() {
        let mut full_code = String::new();
        let entry =
            UserDictionary::create_dict_entry("ni hao \t你好", "c=1 d=1 t=1", 2, 0.0, 0.0, Some(&mut full_code))
                .unwrap();
        assert_eq!(full_code, "ni hao ");
        assert_eq!(entry.text, "你好");
    }

    /// `LookupWords` 的"精确匹配"判定比字面意思松：
    /// 只要求**码串在 `len` 处落在音节边界上**（`key[len] == ' '`），
    /// 所以 `"ni hao"` 会同时命中 `"ni hao "` 和 `"ni hao ma "`，
    /// 后者靠 `comment = "~ ma"` / `remaining_code_length` 表示"还差多少码"。
    #[test]
    fn lookup_words_matches_on_syllable_boundary() {
        let (_dir, user_db) = new_user_dict("words");
        user_db.db().update("ni hao \t你好", "c=5 d=5 t=1");
        user_db.db().update("ni hao ma \t你好吗", "c=1 d=1 t=1");
        user_db.db().update("ni \t你", "c=9 d=9 t=1");

        // 边界对齐 → 两条都算
        let mut it = UserDictEntryIterator::new();
        let (count, _) = user_db.lookup_words(&mut it, "ni hao", false, 0, None);
        assert_eq!(count, 2, "「ni hao」应同时命中「ni hao 」与「ni hao ma 」");
        let entries = it.into_entries();
        let exact = entries.iter().find(|e| e.text == "你好").unwrap();
        assert_eq!(exact.comment, "");
        assert_eq!(exact.remaining_code_length, 0);
        let longer = entries.iter().find(|e| e.text == "你好吗").unwrap();
        assert_eq!(longer.comment, "~ ma");
        assert_eq!(longer.remaining_code_length, 3);
        assert_eq!(longer.custom_code, "ni hao ma ");

        // 边界不对齐 → 非预测模式一条都不要
        let mut it = UserDictEntryIterator::new();
        let (count, _) = user_db.lookup_words(&mut it, "ni h", false, 0, None);
        assert_eq!(count, 0, "「ni h」不在音节边界上，非预测模式应零命中");

        // 预测模式 → 收下，comment 记缺的码
        let mut it = UserDictEntryIterator::new();
        let (count, _) = user_db.lookup_words(&mut it, "ni h", true, 0, None);
        assert_eq!(count, 2);
        let entries = it.into_entries();
        let shorter = entries.iter().find(|e| e.text == "你好").unwrap();
        assert_eq!(shorter.comment, "~ao");
        assert_eq!(shorter.remaining_code_length, 2);
    }

    /// `limit` 只约束**非精确**（预测）条目：C++ 的 `else if (limit && count >= limit) break;`
    /// 挂在 `is_exact_match` 的 else 分支上，边界对齐的条目永远全收。
    #[test]
    fn lookup_words_limit_applies_only_to_predictions() {
        let (_dir, user_db) = new_user_dict("limit");
        for key in ["ni hao \t你好", "ni hao a \t你好啊", "ni hao ma \t你好吗"] {
            user_db.db().update(key, "c=1 d=1 t=1");
        }
        let mut it = UserDictEntryIterator::new();
        let (count, _) = user_db.lookup_words(&mut it, "ni hao", true, 1, None);
        assert_eq!(count, 3, "三条都在音节边界上，limit 不该砍它们");
    }

    /// `resume_key` 续扫：第一轮拿到的键再传回去，会跳过它继续往下。
    #[test]
    fn lookup_words_resume_key_continues_scan() {
        let (_dir, user_db) = new_user_dict("resume");
        for key in ["ni h \t你哈", "ni ha \t你哈啊", "ni hao \t你好"] {
            user_db.db().update(key, "c=1 d=1 t=1");
        }
        // 输入 "ni"：三条的 key[2] 都是 ' '，都算边界对齐
        let mut first = UserDictEntryIterator::new();
        let (count1, resume) = user_db.lookup_words(&mut first, "ni", true, 0, None);
        assert_eq!(count1, 3);
        let resume = resume.expect("应给出 resume_key");
        assert_eq!(resume, "ni hao \t你好");

        // 从末尾续扫：已经到底了，拿到 0 条 + 哨兵
        let mut second = UserDictEntryIterator::new();
        let (count2, next) = user_db.lookup_words(&mut second, "ni", true, 0, Some(&resume));
        assert_eq!(count2, 0);
        assert_eq!(next.as_deref(), Some("\u{ff}"));
    }

    #[test]
    fn update_entry_learns_and_increments_tick() {
        let (_dir0, mut user_db) = new_user_dict("learn");
        let dict = stub_dict();
        let mut entry = DictEntry::default();
        entry.text = "你好".into();
        entry.custom_code = "ni hao ".into();

        assert!(user_db.update_entry(&dict, &entry, 1));
        assert_eq!(user_db.tick(), 1);
        let raw = user_db.db().fetch("ni hao \t你好").expect("应写进 db");
        let value = UserDbValue::unpack(&raw).unwrap();
        assert_eq!(value.commits, 1);
        assert_eq!(value.tick, 1);
        assert!(value.dee > 0.0, "第一次学词 dee 应为正，实际 {}", value.dee);

        // 再选一次：commits 累加、tick 再 +1
        assert!(user_db.update_entry(&dict, &entry, 1));
        assert_eq!(user_db.tick(), 2);
        let value = UserDbValue::unpack(&user_db.db().fetch("ni hao \t你好").unwrap()).unwrap();
        assert_eq!(value.commits, 2);
        assert_eq!(value.tick, 2);

        // commits = 0：只"衰减"，不加次数。
        // 注意 C++ 的 `formula_d(0.1, tick, dee, tick_)` = `0.1 + dee * exp((tick_ - tick)/200)`，
        // 而此刻 `tick_ == tick`（同一轮刚写过），所以结果是 dee + 0.1 —— 逐字照抄，不改。
        let before = value.dee;
        assert!(user_db.update_entry(&dict, &entry, 0));
        let value = UserDbValue::unpack(&user_db.db().fetch("ni hao \t你好").unwrap()).unwrap();
        assert_eq!(value.commits, 2);
        assert!(
            (value.dee - (before + 0.1)).abs() < 1e-9,
            "commits=0 分支应为 dee + 0.1：{before} -> {}",
            value.dee
        );
    }

    #[test]
    fn negative_commits_marks_entry_deleted() {
        let (_dir0, mut user_db) = new_user_dict("delete");
        let dict = stub_dict();
        let mut entry = DictEntry::default();
        entry.text = "你好".into();
        entry.custom_code = "ni hao ".into();
        assert!(user_db.update_entry(&dict, &entry, 1));
        let value = UserDbValue::unpack(&user_db.db().fetch("ni hao \t你好").unwrap()).unwrap();
        assert_eq!(value.commits, 1);

        // 删掉：commits 变 -1，此后 CreateDictEntry 不再产候选
        assert!(user_db.update_entry(&dict, &entry, -1));
        let value = UserDbValue::unpack(&user_db.db().fetch("ni hao \t你好").unwrap()).unwrap();
        assert_eq!(value.commits, -1);
        assert!(UserDictionary::create_dict_entry(
            "ni hao \t你好",
            &value.pack(),
            100,
            0.0,
            0.0,
            None
        )
        .is_none());

        // 再选一次 = 复活
        assert!(user_db.update_entry(&dict, &entry, 2));
        let value = UserDbValue::unpack(&user_db.db().fetch("ni hao \t你好").unwrap()).unwrap();
        assert_eq!(value.commits, 3);
    }

    #[test]
    fn transaction_rolls_back_learning() {
        let (_dir0, mut user_db) = new_user_dict("tx");
        user_db.db().update("ni hao \t你好", "c=1 d=1 t=1");
        assert!(user_db.new_transaction());
        user_db.db().update("ni hao \t你好", "c=99 d=99 t=9");
        assert_eq!(
            user_db.db().fetch("ni hao \t你好").as_deref(),
            Some("c=1 d=1 t=1"),
            "事务未提交前应还是旧值"
        );
        assert!(user_db.commit_pending_transaction());
        assert_eq!(
            user_db.db().fetch("ni hao \t你好").as_deref(),
            Some("c=99 d=99 t=9")
        );

        // 3 秒内可撤
        assert!(user_db.new_transaction());
        user_db.db().update("ni hao \t你好", "c=7 d=7 t=7");
        assert!(user_db.revert_recent_transaction(1));
        assert_eq!(
            user_db.db().fetch("ni hao \t你好").as_deref(),
            Some("c=99 d=99 t=9")
        );
        // 超过 3 秒不许撤
        assert!(user_db.new_transaction());
        user_db.db().update("ni hao \t你好", "c=8 d=8 t=8");
        assert!(!user_db.revert_recent_transaction(5));
        assert!(user_db.commit_pending_transaction());
    }

    // ---- 下面这些测试用假词典（只要音节表，不需要真 .bin） ----

    /// 一个只有音节表、没有真实词典数据的 [`UserDictSource`]。
    struct StubDict {
        syllables: Vec<String>,
    }

    impl StubDict {
        /// 按**字典序**给 id —— 与真实 `Table` 的 id 规则一致。
        fn from_syllables(mut syllables: Vec<&str>) -> Self {
            syllables.sort_unstable();
            Self {
                syllables: syllables.into_iter().map(str::to_string).collect(),
            }
        }
    }

    impl UserDictSource for StubDict {
        fn syllable_by_id(&self, syllable_id: SyllableId) -> String {
            usize::try_from(syllable_id)
                .ok()
                .and_then(|index| self.syllables.get(index).cloned())
                .unwrap_or_default()
        }

        fn syllabary(&self) -> Option<Syllabary> {
            let mut syllabary = Syllabary::new();
            for syllable in &self.syllables {
                syllabary.insert(syllable.clone());
            }
            Some(syllabary)
        }
    }

    fn stub_dict() -> StubDict {
        StubDict::from_syllables(vec!["ni", "hao", "ma", "zhong", "guo"])
    }

    /// 真实词典夹具（`luna_pinyin.table.bin` + `luna_pinyin_simp.prism.bin`）。
    /// 缺夹具时让测试静默跳过 —— 与 e2e 其它真实数据测试同风格。
    fn real_dict() -> Option<Dictionary> {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/e2e/build");
        Dictionary::open(&dir, "luna_pinyin", "luna_pinyin_simp")
    }

    /// 用真实 prism/table 建音节图。
    fn build_graph(dict: &Dictionary, input: &str) -> Option<SyllableGraph> {
        let mut graph = SyllableGraph::default();
        let mut syllabifier =
            crate::algo::syllabifier::Syllabifier::with_options(" ", true, false, None);
        let consumed = syllabifier.build_syllable_graph(input, &dict.prism, &mut graph);
        (consumed > 0).then_some(graph)
    }

    /// **DFS 查询的主验证**：真实音节图 + 真实 userdb，输入 `nihao` 应捞出「你好」。
    #[test]
    fn lookup_dfs_finds_user_phrase_against_real_dict() {
        let Some(dict) = real_dict() else {
            return;
        };
        let Some(graph) = build_graph(&dict, "nihao") else {
            return;
        };
        let dir = TempDir::new("dfs");
        let db = Arc::new(LevelDb::new_user_db(dir.path(), "luna_pinyin"));
        let mut user_db = UserDictionary::new("luna_pinyin", db);
        assert!(user_db.load());
        user_db.db().update("ni hao \t你好", "c=5 d=5 t=1");
        user_db.update_tick_count(10);

        let collector = user_db
            .lookup(&dict, &graph, 0, 0, 0, 0.0)
            .expect("「ni hao」应能查到用户词");

        // 词条挂在 "nihao" 走完的位置（5）
        let it = collector.get(&5).expect("应有一条挂在位置 5 的组");
        let entries = it.clone_entries();
        let texts: Vec<&str> = entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["你好"], "实际候选：{texts:?}");

        let entry = &entries[0];
        // 精确匹配分支：code 是走过的音节 id 串，matching_code_size 不设
        assert_eq!(entry.code.0.len(), 2, "「ni hao」应是两个音节");
        assert_eq!(entry.commit_count, 5);
        assert!(entry.weight.is_finite() && entry.weight > -100.0, "weight={}", entry.weight);
        // 用户词不该被当成预测词
        assert_eq!(entry.comment, "");
        assert!(entry.is_exact_match());
    }

    /// 预测分支：输入 `nihao`、db 里存着「ni hao ma 你好吗」，
    /// 在 `predict_word_from_depth` 打开时应把它当预测词收下，
    /// 并且 `code` 是**整条**码串翻成的音节 id、`matching_code_size` 记"输入了几段"。
    #[test]
    fn lookup_dfs_predicts_longer_user_phrase() {
        let Some(dict) = real_dict() else {
            return;
        };
        let Some(graph) = build_graph(&dict, "nihao") else {
            return;
        };
        let dir = TempDir::new("predict");
        let db = Arc::new(LevelDb::new_user_db(dir.path(), "luna_pinyin"));
        let mut user_db = UserDictionary::new("luna_pinyin", db);
        assert!(user_db.load());
        user_db.db().update("ni hao ma \t你好吗", "c=3 d=3 t=1");
        user_db.update_tick_count(10);

        // 不开预测：捞不到（输入没走完就没有下一条边）
        assert!(
            user_db.lookup(&dict, &graph, 0, 0, 0, 0.0).is_none(),
            "不预测时「你好吗」不该出现"
        );

        let collector = user_db
            .lookup(&dict, &graph, 0, 0, 1, 0.0)
            .expect("开预测应能捞到「你好吗」");
        let it = collector.get(&5).expect("应有一条挂在位置 5 的组");
        let entries = it.clone_entries();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.text, "你好吗");
        assert_eq!(entry.code.0.len(), 3, "整条码串 = ni + hao + ma");
        assert_eq!(entry.matching_code_size, 2, "用户只输入了 ni hao 两段");
    }

    /// 被删掉的词（`c=-1`）不该出现在 DFS 结果里。
    #[test]
    fn lookup_dfs_skips_deleted_entries() {
        let Some(dict) = real_dict() else {
            return;
        };
        let Some(graph) = build_graph(&dict, "nihao") else {
            return;
        };
        let dir = TempDir::new("deleted");
        let db = Arc::new(LevelDb::new_user_db(dir.path(), "luna_pinyin"));
        let mut user_db = UserDictionary::new("luna_pinyin", db);
        assert!(user_db.load());
        user_db.db().update("ni hao \t你好", "c=-1 d=5 t=1");
        user_db.update_tick_count(10);

        assert!(
            user_db.lookup(&dict, &graph, 0, 0, 0, 0.0).is_none(),
            "commits < 0 的词条应被 CreateDictEntry 拒掉"
        );
    }
}
