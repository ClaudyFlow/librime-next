//! `dictionary.h:27-61` + `dictionary.cc:21-224`：`DictEntryIterator`（表侧词条迭代器）。
//!
//! # 它和 `UserDictEntryIterator` 不是一回事
//!
//! | | 表侧（本模块） | 用户侧（[`crate::dict::user_dictionary`]） |
//! |---|---|---|
//! | 数据来源 | `.table.bin` 里若干**页**（每页是一个音节/音节串的全部同码词） | `.userdb` 里按前缀扫出来的一批行 |
//! | 结构 | `Vec<Chunk>` + 游标，**按需换页** | 一次性装进 `DictEntryList` + 下标 |
//! | 顺序 | 每取一条都重排"下一页"（见下） | 装进来时已排好（`SortRange`） |
//!
//! # 为什么需要 chunk 和"每次取都重排"
//!
//! 一次 `LookupWords` 会命中**很多页**：整码 `abc` 命中的是"音节 abc 的全部同码词"一页，
//! 补全模式还会把 `ab`、`a` 这些前缀键的各页也带进来（`Prism::ExpandSearch`）。
//! 这些页必须**交错**出词条，否则"打满的码"和"半截码"会整块整块地排在一起。
//!
//! C++ 的做法是：每页一个 `Chunk`，`Sort()` 用 `partial_sort` 把"当前最优的那一页"
//! 换到 `chunk_index_` 位置，`Peek()` 只读那一页的 `cursor`。`Next()` 让该页游标 +1，
//! 页读完了就 `++chunk_index_`，然后重新挑一次最优页。
//!
//! 页的优先级（`compare_chunk_by_head_element`，`dictionary.cc:75-86`）依次是：
//!
//! 1. **精确匹配优先**：`matching_code_size == code.size()` 的页排在前面；
//! 2. **剩余码短的优先**：`remaining_code.length()` 小的先出（补全时 `~abc` 里的 `abc`）；
//! 3. **权重高的优先**：`credibility + entries[cursor].weight` 降序。
//!
//! # 与 C++ 的接口差异（都是"接缝"）
//!
//! * C++ 的 `Chunk::entries` 是 `const table::Entry*` 裸指针 + `size`；Rust 侧在
//!   [`Chunk::from_accessor`] 里**一次性把这一页读成 `Vec`**（连文本一起，因为
//!   `Table::GetEntryText` 要借 `Table`，而迭代器不该借它）。一页只有几条到几十条，
//!   开销可以忽略，换来的是迭代器**不借任何东西**（`'static`）。
//! * `Peek()` 在 C++ 里返回 `an<DictEntry>`（`shared_ptr`，调用方可能长期持有）；
//!   Rust 侧返回 `Option<&DictEntry>`（指向内部缓存），所以是 `&mut self`。
//! * `DictEntryFilterBinder` 在 C++ 是基类；Rust 侧是
//!   [`crate::dict::vocabulary::DictEntryFilterBinder`]（`Option<Box<dyn Fn>>`），
//!   见 [`Self::add_filter`]。

use crate::dict::table::{Table, TableAccessor};
use crate::dict::vocabulary::{
    Code, DictEntry, DictEntryFilter, DictEntryFilterBinder, SyllableId,
};

/// `log(1e8)`（`dictionary.cc:162`）：`.table.bin` 里的整数权重 → 对数概率的偏移量。
///
/// 必须照抄：grammar 打分、用户词权重都是同一对数尺度上的加法，漏掉就整体错位。
pub const K_WEIGHT_OFFSET: f64 = 18.420_680_743_952_367;

/// `dictionary::Chunk`（`dictionary.cc:25-72`）：一页词条 + 这一页的元信息。
#[derive(Debug, Clone, Default)]
pub struct Chunk {
    /// 这一页剩下的词条（文本已取好）。C++ 是 `const table::Entry* entries` + `size`。
    pub entries: Vec<(String, f32)>,
    /// 本页读到第几条。C++ 是 `cursor`。
    pub cursor: usize,
    /// 这一页的索引码（`.table.bin` 里那几级音节 id）。
    pub code: Code,
    /// 补全时"用户还没打的那截"（`"~abc"` 里的 `abc`，不含 `~`）。
    pub remaining_code: String,
    /// 命中的码长（`index_code.size()`）；判断精确/补全用。
    pub matching_code_size: usize,
    /// 这条路径上各音节 `credibility` 之和（表查询恒 0，纠错/模糊才有）。
    pub credibility: f64,
    /// 其中属于「全拼」部分的字节数（拼音方案排序用；形码恒 0）。
    pub quality_len: f64,
}

impl Chunk {
    /// `Chunk(Table*, const TableAccessor&, const string& remaining_code, cr, q)`
    /// （`dictionary.cc:57-67`）。
    ///
    /// 把 `accessor` **当前剩下的部分**整体读成快照（C++ 只是记下 `a.entry()` 指针 +
    /// `a.remaining()`，依赖 `Table` 一直活着）。
    pub fn from_accessor(
        table: &Table,
        mut accessor: TableAccessor<'_>,
        remaining_code: String,
        credibility: f64,
        quality_len: f64,
    ) -> Self {
        let code = accessor.index_code().clone();
        let matching_code_size = code.len();
        let mut entries = Vec::with_capacity(accessor.remaining());
        while !accessor.exhausted() {
            if let Some(entry) = accessor.entry() {
                entries.push((table.entry_text(&entry), entry.weight));
            }
            accessor.next();
        }
        Self {
            entries,
            cursor: 0,
            code,
            remaining_code,
            matching_code_size,
            credibility,
            quality_len,
        }
    }

    /// 本页还剩几条。
    pub fn remaining(&self) -> usize {
        self.entries.len().saturating_sub(self.cursor)
    }

    /// `Chunk::is_exact_match()`：整条索引码都被用户打完了。
    pub fn is_exact_match(&self) -> bool {
        self.matching_code_size == self.code.len()
    }

    /// `Chunk::is_predictive_match()`：只打了一部分（补全）。
    pub fn is_predictive_match(&self) -> bool {
        self.matching_code_size < self.code.len()
    }
}

/// `compare_chunk_by_head_element`（`dictionary.cc:75-86`）：**a 是否优于 b**。
///
/// C++ 拿它喂 `partial_sort`（把最优的换到队首），条件方向逐条照抄：
/// 精确匹配 > 补全；剩余码短 > 长；权重高 > 低。已耗尽的页永远"不优"。
pub fn compare_chunk_by_head_element(a: &Chunk, b: &Chunk) -> bool {
    if a.remaining() == 0 {
        return false;
    }
    if b.remaining() == 0 {
        return true;
    }
    if a.is_exact_match() != b.is_exact_match() {
        // C++：`a.is_exact_match() > b.is_exact_match()`
        return a.is_exact_match();
    }
    if a.remaining_code.len() != b.remaining_code.len() {
        return a.remaining_code.len() < b.remaining_code.len();
    }
    a.credibility + f64::from(a.entries[a.cursor].1)
        > b.credibility + f64::from(b.entries[b.cursor].1)
}

/// `class DictEntryIterator`（`dictionary.h:27-51`）。
#[derive(Default)]
pub struct DictEntryIterator {
    chunks: Vec<Chunk>,
    chunk_index: usize,
    /// `Peek()` 造出来的当前条目（C++ 的 `an<DictEntry> entry_`）。
    entry: Option<DictEntry>,
    entry_count: usize,
    filter: DictEntryFilterBinder,
}

impl DictEntryIterator {
    pub fn new() -> Self {
        Self::default()
    }

    /// `AddChunk`（`dictionary.cc:120-123`）：追加一页，并把这一页的条数记进总数。
    pub fn add_chunk(&mut self, chunk: Chunk) {
        self.entry_count += chunk.entries.len();
        self.chunks.push(chunk);
    }

    /// 已装进来的词条总数（**含已经读过的**，C++ `entry_count()` 同样如此）。
    pub fn entry_count(&self) -> usize {
        self.entry_count
    }

    /// `exhausted()`（`dictionary.cc:214-216`）：所有页都读完了。
    pub fn exhausted(&self) -> bool {
        self.chunk_index >= self.chunks.len()
    }

    /// `Sort()`（`dictionary.cc:125-131`）：把"当前最优的那一页"换到 `chunk_index`。
    ///
    /// C++ 是 `partial_sort(begin+idx, begin+idx+1, end, cmp)` —— 只需队首那一个，
    /// 所以这里直接线性找最优再 `swap`。两者对队首的结果一致；尾部顺序 C++
    /// 不保证、这里也不保证，调用方**不能**依赖它。
    fn sort(&mut self) {
        let index = self.chunk_index;
        if index >= self.chunks.len() {
            return;
        }
        let rest = &mut self.chunks[index..];
        let mut best = 0;
        for i in 1..rest.len() {
            if compare_chunk_by_head_element(&rest[i], &rest[best]) {
                best = i;
            }
        }
        rest.swap(0, best);
    }

    /// `AddFilter`（`dictionary.cc:133-141`）：追加过滤条件，并**立即**跳过当前已经不满足的条目。
    pub fn add_filter(&mut self, filter: DictEntryFilter) {
        self.filter = Some(match self.filter.take() {
            Some(existing) => Box::new(move |entry| existing(entry) && filter(entry)),
            None => filter,
        });
        while !self.exhausted() && !self.accepts() {
            self.entry = None;
            self.find_next_entry();
        }
    }

    fn accepts(&mut self) -> bool {
        // 先把手上的 filter 取出来：`peek()` 要 `&mut self`，不能和 filter 的借用重叠
        let Some(filter) = self.filter.take() else {
            return true;
        };
        let accepted = self.peek().map(|entry| filter(entry)).unwrap_or(true);
        self.filter = Some(filter);
        accepted
    }

    /// `Peek()`（`dictionary.cc:143-165`）：造出（并缓存）当前条目。
    ///
    /// 权重换算、`~剩余码` 注释、`matching_code_size` 三件事都在这里做，与 C++ 一致。
    pub fn peek(&mut self) -> Option<&DictEntry> {
        if self.entry.is_none() && !self.exhausted() {
            let chunk = &self.chunks[self.chunk_index];
            let (text, weight) = &chunk.entries[chunk.cursor];
            let mut entry = DictEntry {
                code: chunk.code.clone(),
                text: text.clone(),
                weight: f64::from(*weight) - K_WEIGHT_OFFSET + chunk.credibility,
                quality_len: chunk.quality_len,
                ..Default::default()
            };
            if !chunk.remaining_code.is_empty() {
                entry.comment = format!("~{}", chunk.remaining_code);
                entry.remaining_code_length = chunk.remaining_code.len() as i32;
            }
            if chunk.is_predictive_match() {
                entry.matching_code_size = chunk.matching_code_size as i32;
            }
            self.entry = Some(entry);
        }
        self.entry.as_ref()
    }

    /// `FindNextEntry()`（`dictionary.cc:167-188`）：当前页游标 +1，读完了就换页并重排。
    fn find_next_entry(&mut self) -> bool {
        if self.exhausted() {
            return false;
        }
        {
            let chunk = &mut self.chunks[self.chunk_index];
            chunk.cursor += 1;
            if chunk.cursor >= chunk.entries.len() {
                self.chunk_index += 1;
            }
        }
        if self.exhausted() {
            return false;
        }
        self.sort();
        true
    }

    /// `Next()`（`dictionary.cc:190-198`）：前进一条（跳过被 filter 拒掉的）。
    pub fn next(&mut self) -> bool {
        loop {
            self.entry = None;
            if !self.find_next_entry() {
                return false;
            }
            if self.filter.is_none() || self.accepts() {
                return true;
            }
        }
    }

    /// `Skip(num_entries)`（`dictionary.cc:201-212`）：**不套过滤器**，直接跳过 n 条。
    ///
    /// C++ 的注释写得很明确："Note: does not apply filters"。`LazyTableTranslation`
    /// 用它做"多取一页的增量"，所以别在这里偷偷加过滤。
    pub fn skip(&mut self, mut num_entries: usize) -> bool {
        while num_entries > 0 {
            if self.exhausted() {
                return false;
            }
            let chunk = &mut self.chunks[self.chunk_index];
            if chunk.cursor + num_entries < chunk.entries.len() {
                chunk.cursor += num_entries;
                self.entry = None;
                return true;
            }
            num_entries -= chunk.entries.len() - chunk.cursor;
            self.chunk_index += 1;
        }
        self.entry = None;
        true
    }

    /// 当前条目（等价于先 [`Self::peek`] 再取走），给"看完就推进"的调用方用。
    pub fn take_entry(&mut self) -> Option<DictEntry> {
        let entry = self.peek().cloned();
        entry
    }

    /// 本迭代器里所有条目（**按出词顺序**，内部会消耗掉游标）。
    ///
    /// C++ 没有这个方法：`TableTranslator` 那边是一边翻页一边出候选。LRN 的候选是
    /// 急切构造的，所以这里补一个"一口气取完"的收口写法。
    pub fn collect_entries(mut self) -> Vec<DictEntry> {
        let mut out = Vec::new();
        if let Some(entry) = self.peek() {
            out.push(entry.clone());
        }
        while self.next() {
            if let Some(entry) = self.peek() {
                out.push(entry.clone());
            }
        }
        out
    }
}

/// `Dictionary::LookupWords(result, str_code, predictive, limit, blacklist)`
/// （`dictionary.cc:299-348`）的实参装填部分，见
/// [`crate::dict::dictionary::Dictionary::lookup_words`]。
///
/// 单独抽出来是为了让"取键 → 逐拼写 → 逐音节查页"这段能脱离 `Dictionary` 测试。
pub fn add_chunks_for_keys(
    result: &mut DictEntryIterator,
    table: &Table,
    prism: &crate::dict::prism::Prism,
    keys: &[crate::dict::prism::Match],
    code_length: usize,
) {
    for key in keys {
        let mut accessor = prism.query_spelling(key.value);
        while !accessor.exhausted() {
            let syllable_id: SyllableId = accessor.syllable_id();
            let spelling_type = accessor.properties().r#type;
            accessor.next();
            // `dictionary.cc:324-325`：`if (type > kNormalSpelling) continue;`
            // 即只认"用户把这一截全打完了"的拼写；模糊/简拼/补全都不用。
            if spelling_type != crate::algo::SpellingType::Normal {
                continue;
            }
            // `dictionary.cc:326-331`：只有补全时才可能"音节比用户打的码还长"。
            let mut remaining_code = String::new();
            if key.length > code_length {
                let syllable = table.get_syllable_by_id(syllable_id);
                if syllable.len() > code_length {
                    remaining_code = syllable[code_length..].to_string();
                }
            }
            let words = table.query_words(syllable_id);
            if !words.exhausted() {
                result.add_chunk(Chunk::from_accessor(table, words, remaining_code, 0.0, 0.0));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(code: &[SyllableId], entries: &[(&str, f32)], remaining: &str) -> Chunk {
        Chunk {
            entries: entries
                .iter()
                .map(|(text, weight)| ((*text).to_string(), *weight))
                .collect(),
            cursor: 0,
            code: Code::from_slice(code),
            remaining_code: remaining.to_string(),
            matching_code_size: code.len(),
            credibility: 0.0,
            quality_len: 0.0,
        }
    }

    #[test]
    fn peek_converts_weight_and_marks_completion() {
        let mut iter = DictEntryIterator::new();
        iter.add_chunk(chunk(&[1, 2], &[("你好", 100.0f32)], ""));
        let entry = iter.peek().expect("应该有一条");
        // weight = 100 - log(1e8)（`dictionary.cc:162`）
        assert!((entry.weight - (100.0 - K_WEIGHT_OFFSET)).abs() < 1e-9);
        assert_eq!(entry.text, "你好");
        assert_eq!(entry.comment, "", "整码命中不该有 ~ 注释");
        assert_eq!(entry.remaining_code_length, 0);
        assert_eq!(
            entry.matching_code_size, 0,
            "精确匹配不填 matching_code_size"
        );
    }

    #[test]
    fn remaining_code_becomes_tilde_comment() {
        let mut iter = DictEntryIterator::new();
        iter.add_chunk(chunk(&[1, 2], &[("你好", 10.0f32)], "ao"));
        let entry = iter.peek().expect("应该有一条");
        assert_eq!(entry.comment, "~ao", "补全提示 = `~` + 用户还没打的码");
        assert_eq!(entry.remaining_code_length, 2);
        // ⚠️ C++ 的 `Chunk(t, a, r)` 把 `matching_code_size` 设成 `a.index_code().size()`，
        // 与 `chunk.code.size()` 恒等 —— 所以 `is_predictive_match()` 永远是 false，
        // `Peek()` 里那个 `if (is_predictive_match())` 分支对 LookupWords 产出的 chunk
        // 是死代码。这里钉住这个"反直觉但必须照抄"的事实。
        assert_eq!(entry.matching_code_size, 0);
        assert!(entry.is_exact_match());
    }

    #[test]
    fn first_peek_comes_from_the_first_chunk() {
        // ⚠️ C++ 的一个反直觉细节，必须照抄：`Sort()` 只在 `FindNextEntry` 里调，
        // 而 `Peek()` 直接读 `chunks[0]`。所以**第一条永远是先装进来的那一页**，
        // 从第二条起才按"最优页优先"重排。
        //
        // 实际效果正好是想要的：`ExpandSearch` 的第一个键就是整码命中的键。
        let mut iter = DictEntryIterator::new();
        iter.add_chunk(chunk(&[1, 2], &[("你", 99999.0f32)], "ao"));
        iter.add_chunk(chunk(&[1, 2], &[("你好", 1.0f32)], ""));

        assert_eq!(iter.peek().unwrap().text, "你", "第一条来自先装的页");
        assert!(iter.next());
        assert_eq!(iter.peek().unwrap().text, "你好", "之后无剩余码的页胜出");
        assert!(!iter.next(), "只有两条");
    }

    #[test]
    fn comparator_prefers_shorter_remaining_code() {
        let long = chunk(&[1, 2], &[("一", 9999.0f32)], "abc");
        let short = chunk(&[1, 2], &[("乙", 1.0f32)], "a");
        // 剩余码短的优先，哪怕权重低六千倍
        assert!(compare_chunk_by_head_element(&short, &long));
        assert!(!compare_chunk_by_head_element(&long, &short));
    }

    #[test]
    fn comparator_prefers_higher_weight_on_tie() {
        let heavy = chunk(&[1, 2], &[("甲", 9.0f32)], "");
        let light = chunk(&[1, 2], &[("乙", 1.0f32)], "");
        assert!(compare_chunk_by_head_element(&heavy, &light));
        assert!(!compare_chunk_by_head_element(&light, &heavy));
    }

    #[test]
    fn interleaves_chunks_by_head_weight() {
        // 两页各自有两条：出的顺序应该是 甲(9) 乙(5) 丙(8) 丁(1)？
        // 实际按"每取一条重排"来：先两页的队首比（甲 9 > 丙 8）→ 甲；
        // 然后 A 页队首变乙(5) < 丙(8) → 丙；再比乙(5) vs 丁(1) → 乙；最后丁。
        let mut iter = DictEntryIterator::new();
        iter.add_chunk(chunk(&[1], &[("甲", 9.0f32), ("乙", 5.0f32)], ""));
        iter.add_chunk(chunk(&[2], &[("丙", 8.0f32), ("丁", 1.0f32)], ""));

        let mut order = Vec::new();
        if let Some(entry) = iter.peek() {
            order.push(entry.text.clone());
        }
        while iter.next() {
            if let Some(entry) = iter.peek() {
                order.push(entry.text.clone());
            }
        }
        assert_eq!(order, vec!["甲", "丙", "乙", "丁"]);
    }

    #[test]
    fn skip_advances_across_chunks() {
        let mut iter = DictEntryIterator::new();
        iter.add_chunk(chunk(&[1], &[("甲", 9.0f32), ("乙", 5.0f32)], ""));
        iter.add_chunk(chunk(&[2], &[("丙", 8.0f32)], ""));

        assert!(iter.skip(2), "跳两条应该成功");
        assert_eq!(iter.peek().unwrap().text, "丙");
        assert!(iter.skip(1), "跳过最后一条");
        assert!(iter.exhausted());
    }

    #[test]
    fn filter_rejects_matching_entries() {
        let mut iter = DictEntryIterator::new();
        iter.add_chunk(chunk(&[1], &[("甲", 9.0f32), ("乙", 5.0f32)], ""));
        iter.add_filter(Box::new(|entry: &DictEntry| entry.text != "甲"));
        // AddFilter 会立刻跳过已不满足的当前条目
        assert_eq!(iter.peek().unwrap().text, "乙");
    }

    #[test]
    fn entry_count_counts_every_chunk() {
        let mut iter = DictEntryIterator::new();
        iter.add_chunk(chunk(&[1], &[("甲", 9.0f32), ("乙", 5.0f32)], ""));
        iter.add_chunk(chunk(&[2], &[("丙", 8.0f32)], ""));
        assert_eq!(iter.entry_count(), 3);
    }
}
