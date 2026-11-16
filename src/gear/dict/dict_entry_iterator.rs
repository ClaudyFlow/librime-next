//! `dictionary.h:27-61` + `dictionary.cc:21-224`：`DictEntryIterator`（表侧词条迭代器）。
//!
//! # 它和 `UserDictEntryIterator` 不是一回事
//!
//! | | 表侧（本模块） | 用户侧（[`crate::gear::dict::user_dictionary`]） |
//! |---|---|---|
//! | 数据来源 | `.table.bin` 里若干**页**（每页是一个音节/音节串的全部同码词） | `.userdb` 里按前缀扫出来的一批行 |
//! | 结构 | `Vec<Chunk>` + 游标，**按需换页** | 一次性装进 `DictEntryList` + 下标 |
//! | 顺序 | 每取一条维护"下一页"的堆（librime#1268 前是重排） | 装进来时已排好（`SortRange`） |
//!
//! # 为什么需要 chunk
//!
//! 一次 `LookupWords` 会命中**很多页**：整码 `abc` 命中的是"音节 abc 的全部同码词"一页，
//! 补全模式还会把 `ab`、`a` 这些前缀键的各页也带进来（`Prism::ExpandSearch`）。
//! 这些页必须**交错**出词条，否则"打满的码"和"半截码"会整块整块地排在一起。
//!
//! 每页一个 `Chunk`，`Peek()` 只读"当前最优那一页"的 `cursor`。`Next()` 让该页游标 +1，
//! 页读完了就摘掉，然后重新挑一次最优页：librime#1268 之后这一步是堆的
//! `pop_heap` / `push_heap`（见下节），不再是"线性扫一遍剩余页"。
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
//!   [`crate::gear::dict::vocabulary::DictEntryFilterBinder`]（`Option<Box<dyn Fn>>`），
//!   见 [`Self::add_filter`]。
//!
//! # librime#1268：堆合并（2026-11-10 已接入）
//!
//! 改造前 `sort()` 是"线性扫剩余页 + swap"，`find_next_entry()` 每取一条都调一次，
//! 全遍历复杂度 O(N·k)。真词库上一旦 `k` 大就是灾难级：
//! `example/bench_dict_iterator.rs`（release，同一台机交替跑新旧二进制，µs）：
//!
//! | 词库 / 形状 | N | k | 旧 walk p50 | 新 walk p50 | 旧 advance p50 | 新 advance p50 |
//! |---|---:|---:|---:|---:|---:|---:|
//! | wubi86 单字母 `a` 补全 | 18438 | 8329 | 502–639 ms | **22.5–42.8 ms** | 457–631 ms | **9.8–15.4 ms** |
//! | luna_pinyin 空码全量 | 76802 | 687 | 325–640 ms | **96–181 ms** | 231–426 ms | **22–39 ms** |
//! | luna_pinyin 双字母 `ni` 补全 | 760 | 13 | 812–822 µs | 655–918 µs | 44–53 µs | 78–125 µs |
//! | luna_pinyin 单字母 `a` 补全 | 506 | 6 | 232–415 µs | 499–626 µs | 12–18 µs | 53–57 µs |
//! | 整码（k=1，13–182 条） | ≤182 | 1 | 3.4–6.1 µs | 5.8–7.5 µs | 0.2–3.7 µs | 0.7–9.3 µs |
//!
//! `walk` = `collect_entries`（含构造 `DictEntry` 的分配），`advance` = 只推进游标。
//! **交叉点在 k≈200**：k 大时收益 10–46 倍；k≤13 的全遍历反而慢 1.5–4 倍——堆的常数
//! 更大（每步 swap 112 字节的 `Chunk`），而 k=6 时线性扫只要 5 次比较。
//!
//! 生产热路径是**前 10/100 条**（`K_INITIAL_SEARCH_LIMIT`），每条多几十纳秒，可忽略；
//! 受影响的只有 `reverse_lookup_translator` 的 `collect_entries` 这类小 k 全遍历。
//! 若将来这条路径变成瓶颈，可给它加"`k` 小于阈值就走线性"的开关（本次按上游保持单一形态）。
//!
//! 实现与上游逐点对齐：`Chunk::order`（插入序，比较完全相等时稳定出词）、
//! `is_heap`（堆缓存标记，`add_chunk` / `skip` 置假）、`sort()` 只在失效时 `make_heap`。
//! 首次 `Peek()` 仍然不排序（直接读 `chunks[chunk_index]`），C++ 的反直觉行为保留。

use crate::gear::dict::table::{Table, TableAccessor};
use crate::gear::dict::vocabulary::{
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
    /// 插入序（librime#1268）：比较完全相等时按它稳定出词。
    pub order: usize,
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
            order: 0,
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
/// 排序条件逐条照抄 C++：精确匹配 > 补全；剩余码短 > 长；权重高 > 低。已耗尽的页永远"不优"。
pub fn compare_chunk_by_head_element(a: &Chunk, b: &Chunk) -> bool {
    compare_head_entries(a, b) < 0
}

/// 三态比较（librime#1268 的 `compare_head_entries`）：`<0` 表示 a 先出，`>0` 表示 b 先出。
fn compare_head_entries(a: &Chunk, b: &Chunk) -> i32 {
    let a_exhausted = a.remaining() == 0;
    let b_exhausted = b.remaining() == 0;
    if a_exhausted || b_exhausted {
        // 耗尽的页排最后（bool → 0/1）
        return i32::from(a_exhausted) - i32::from(b_exhausted);
    }
    if a.is_exact_match() != b.is_exact_match() {
        // C++：`a.is_exact_match() > b.is_exact_match()`
        return if a.is_exact_match() { -1 } else { 1 };
    }
    if a.remaining_code.len() != b.remaining_code.len() {
        return if a.remaining_code.len() < b.remaining_code.len() {
            -1
        } else {
            1
        };
    }
    let a_weight = a.credibility + f64::from(a.entries[a.cursor].1);
    let b_weight = b.credibility + f64::from(b.entries[b.cursor].1);
    if a_weight > b_weight {
        -1
    } else if a_weight < b_weight {
        1
    } else {
        0
    }
}

/// 堆序（librime#1268 的 `chunk_visited_later`）：a 是否该排在 b **之后**。
///
/// 比较完全相等时按 `order`（插入序）决出先后，保证同一份输入每次出词顺序一致。
fn chunk_visited_later(a: &Chunk, b: &Chunk) -> bool {
    match compare_head_entries(a, b) {
        0 => a.order > b.order,
        cmp => cmp > 0,
    }
}

// `std::make_heap` / `pop_heap` / `push_heap`（`<algorithm>` 的那三个）在 `[Chunk]` 上的等价物。
// std 只在 `BinaryHeap`（要求 `Ord` + 独占所有权）上暴露堆操作，而这里要原地维护
// `chunks[chunk_index..]` 这一段、且比较键随 cursor 变化，所以手写 sift。

/// 下沉：`chunks[pos]` 与更优的孩子交换，直到堆序恢复。
fn sift_down(chunks: &mut [Chunk], mut pos: usize) {
    let len = chunks.len();
    loop {
        let left = pos * 2 + 1;
        if left >= len {
            return;
        }
        // 右孩子若比左孩子更靠前，就选右孩子
        let child = if left + 1 < len && chunk_visited_later(&chunks[left], &chunks[left + 1]) {
            left + 1
        } else {
            left
        };
        if !chunk_visited_later(&chunks[pos], &chunks[child]) {
            return;
        }
        chunks.swap(pos, child);
        pos = child;
    }
}

/// 上浮：`chunks[pos]` 比父亲更靠前就往上换。
fn sift_up(chunks: &mut [Chunk], mut pos: usize) {
    while pos > 0 {
        let parent = (pos - 1) / 2;
        if !chunk_visited_later(&chunks[parent], &chunks[pos]) {
            return;
        }
        chunks.swap(parent, pos);
        pos = parent;
    }
}

/// `make_heap`：队首是"当前最优页"。
fn make_heap(chunks: &mut [Chunk]) {
    let len = chunks.len();
    for pos in (0..len / 2).rev() {
        sift_down(chunks, pos);
    }
}

/// `pop_heap`：把队首（最优页）换到尾部，剩下的仍然是一个堆。
fn pop_heap(chunks: &mut [Chunk]) {
    let len = chunks.len();
    if len < 2 {
        return;
    }
    chunks.swap(0, len - 1);
    sift_down(&mut chunks[..len - 1], 0);
}

/// `push_heap`：`chunks[..len-1]` 已是堆，把新的队尾上浮到位。
fn push_heap(chunks: &mut [Chunk]) {
    let len = chunks.len();
    if len < 2 {
        return;
    }
    sift_up(chunks, len - 1);
}

/// `class DictEntryIterator`（`dictionary.h:27-51`）。
#[derive(Default)]
pub struct DictEntryIterator {
    chunks: Vec<Chunk>,
    chunk_index: usize,
    /// `chunks[chunk_index..]` 是否已经是堆（队首 = 最优页）。
    ///
    /// `add_chunk` / `skip` 会把它置假：两者都用线性方式改动过页表/游标，
    /// 堆序不再成立。置假后下一条会退回"线性 + 重建堆"，语义与旧实现一致。
    is_heap: bool,
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
    pub fn add_chunk(&mut self, mut chunk: Chunk) {
        self.entry_count += chunk.entries.len();
        chunk.order = self.chunks.len();
        self.chunks.push(chunk);
        self.is_heap = false;
    }

    /// 已装进来的词条总数（**含已经读过的**，C++ `entry_count()` 同样如此）。
    pub fn entry_count(&self) -> usize {
        self.entry_count
    }

    /// 已装进来的页数（C++ `chunks_.size()`）。量化"每取一条要扫多少页"用。
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// `exhausted()`（`dictionary.cc:214-216`）：所有页都读完了。
    pub fn exhausted(&self) -> bool {
        self.chunk_index >= self.chunks.len()
    }

    /// `Sort()`（`dictionary.cc:125-131`）：把剩余页整理成堆，队首即"当前最优的那一页"。
    ///
    /// 堆已经有效时直接返回（`is_heap`）；之后每取一条只做一次 `pop_heap` / `push_heap`，
    /// 不再线性扫剩余页。这是 librime#1268 的核心：旧实现 O(N·k)，现在 O(N·log k)。
    fn sort(&mut self) {
        if self.is_heap {
            return;
        }
        let index = self.chunk_index;
        if index < self.chunks.len() {
            make_heap(&mut self.chunks[index..]);
        }
        self.is_heap = true;
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

    /// `FindNextEntry()`（`dictionary.cc:167-188`）：当前页游标 +1，读完了就换页。
    fn find_next_entry(&mut self) -> bool {
        if self.exhausted() {
            return false;
        }
        if !self.is_heap {
            // 堆还没建（首条、或 `skip` 刚把缓存置假）：照旧线性推进，然后重建堆。
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
            return true;
        }
        // 堆有效：把队首（最优页）换到尾部、推进它的游标，还有剩余就放回堆。
        // 每条 O(log k)，不再重扫剩余的页。
        let index = self.chunk_index;
        let mut drop_last = false;
        {
            let rest = &mut self.chunks[index..];
            pop_heap(rest);
            let last = rest.len() - 1;
            let chunk = &mut rest[last];
            chunk.cursor += 1;
            if chunk.cursor < chunk.entries.len() {
                push_heap(rest);
            } else {
                drop_last = true;
            }
        }
        if drop_last {
            // 这一页读完了：它是 pop_heap 换到队尾的那一个，直接摘掉。
            self.chunks.pop();
        }
        !self.exhausted()
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
    ///
    /// `skip` 按"当前数组顺序"整页整页地跳，会破坏堆序，所以先把 `is_heap` 置假
    /// （同时丢掉 `Peek()` 缓存的条目），下一条再线性推进 + 重建堆。
    pub fn skip(&mut self, mut num_entries: usize) -> bool {
        if num_entries == 0 {
            return true;
        }
        self.entry = None;
        self.is_heap = false;
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

    /// 急切收口的条数硬上限（防「全遍历 × 巨大词表」的长尾）。
    ///
    /// release 量化：堆合并后单条 O(log k)，但 `k=64 / N=16384` 的全遍历仍要
    /// 6ms 量级；`reverse_lookup_translator` 的 `K_INITIAL_SEARCH_LIMIT=100` 只限制
    /// prism 展开的键数，**不限制 `collect_entries()` 倒出的条数**，异常词表下会一口
    /// 气倒完。热路径 `table_translator` limit=10、反查 limit=100 都远低于 4096，
    /// 所以这个上限不改变任何正常候选，只辟断异常分支。
    pub const MAX_DRAIN_ENTRIES: usize = 4096;

    /// 本迭代器里剩余条目（**按出词顺序**，内部会消耗掉游标，最多
    /// [`Self::MAX_DRAIN_ENTRIES`] 条）。
    ///
    /// C++ 没有这个方法：`TableTranslator` 那边是一边翻页一边出候选。LRN 的候选是
    /// 急切构造的，所以这里补一个"一口气取完"的收口写法。
    pub fn collect_entries(mut self) -> Vec<DictEntry> {
        let mut out = Vec::new();
        if let Some(entry) = self.peek() {
            out.push(entry.clone());
        }
        while out.len() < Self::MAX_DRAIN_ENTRIES && self.next() {
            if let Some(entry) = self.peek() {
                out.push(entry.clone());
            }
        }
        out
    }
}

/// `Dictionary::LookupWords(result, str_code, predictive, limit, blacklist)`
/// （`dictionary.cc:299-348`）的实参装填部分，见
/// [`crate::gear::dict::dictionary::Dictionary::lookup_words`]。
///
/// 单独抽出来是为了让"取键 → 逐拼写 → 逐音节查页"这段能脱离 `Dictionary` 测试。
pub fn add_chunks_for_keys(
    result: &mut DictEntryIterator,
    tables: &[(&Table, Option<&crate::gear::dict::table::SyllableIdMap>)],
    prism: &crate::gear::dict::prism::Prism,
    keys: &[crate::gear::dict::prism::Match],
    code_length: usize,
) {
    // 音节串（补全提示那截）只在主表/prism 里查：v6 的音节 id 是 prism 的**全局** id，
    // 所以优先用 prism 的音节表；v5 没有音节表，回退主表（两版 id 都一致）。
    let Some(primary) = tables.first().map(|(table, _)| *table) else {
        return;
    };
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
                let syllable = prism
                    .syllable_by_id(syllable_id)
                    .unwrap_or_else(|| primary.get_syllable_by_id(syllable_id));
                if syllable.len() > code_length {
                    remaining_code = syllable[code_length..].to_string();
                }
            }
            // 主表 + 分包（`dictionary.cc:332` 的 `for (const auto& table : tables_)`）：
            // 同一个音节 id 在每张表里可能各有一份词条，各自成一页。
            // v6：全局 id 经各表自己的映射换成本表局部 id。
            for (table, map) in tables {
                let words = table.query_words_with_map(syllable_id, *map);
                if !words.exhausted() {
                    result.add_chunk(Chunk::from_accessor(
                        table,
                        words,
                        remaining_code.clone(),
                        0.0,
                        0.0,
                    ));
                }
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
            order: 0,
        }
    }

    /// 急切收口必须有上限：反查补全传进去的 limit=100 只管 prism 键数，
    /// 不设上限时异常词表会一口气倒完（查询侧 p95 长尾）。
    #[test]
    fn collect_entries_caps_abnormal_table_at_max_drain() {
        let mut iter = DictEntryIterator::new();
        let total = DictEntryIterator::MAX_DRAIN_ENTRIES + 137;
        let mut c = chunk(&[1], &[], "");
        c.entries = (0..total).map(|i| (format!("词{i}"), i as f32)).collect();
        iter.add_chunk(c);
        let got = iter.collect_entries();
        assert_eq!(got.len(), DictEntryIterator::MAX_DRAIN_ENTRIES);
        assert_eq!(got[0].text, "词0", "上限不能打乱出词顺序");
        assert_eq!(
            got.last().unwrap().text,
            format!("词{}", DictEntryIterator::MAX_DRAIN_ENTRIES - 1)
        );
    }

    #[test]
    fn normal_iterators_are_not_truncated_by_the_cap() {
        let mut iter = DictEntryIterator::new();
        for k in 0..8u32 {
            iter.add_chunk(chunk(&[k as SyllableId], &[("甲", 1.0), ("乙", 2.0)], ""));
        }
        assert_eq!(iter.collect_entries().len(), 16, "正常词表不受上限影响");
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

    /// 量化 librime#1268：`sort()` 是线性扫 k 个 chunk，**每取一条都跑一次**，
    /// 全遍历成本 O(N·k)。用 `--nocapture` 打印不同页数/条数下的实测 µs。
    /// 只锁正确性（遍历条数），不锁时间——CI 机器快慢不同。
    #[test]
    fn iterator_walk_cost_scales_with_chunk_count() {
        use std::time::Instant;
        for (k, n) in [
            (4usize, 64usize),
            (16, 64),
            (64, 64),
            (4, 256),
            (16, 256),
            (64, 256),
        ] {
            let entries: Vec<(&str, f32)> = vec![("词", 1.0); n];
            let mut iter = DictEntryIterator::new();
            for j in 0..k {
                iter.add_chunk(chunk(&[j as SyllableId], &entries, ""));
            }
            let t0 = Instant::now();
            let mut seen = 0usize;
            while iter.peek().is_some() {
                seen += 1;
                if !iter.next() {
                    break;
                }
            }
            let us = t0.elapsed().as_micros();
            println!("1268 k={k:2} n={n:3} N={:5} walk={us:6}µs", k * n);
            assert_eq!(seen, k * n, "全遍历不能漏条目");
        }
    }

    /// 量化 librime#1268 的**真实热路径**：`LookupWords` 带 `limit`，
    /// `TableTranslator` / `ReverseLookupTranslator` 通常只取前几十条，不会全遍历。
    ///
    /// 上一条测的是最坏情况（全遍历）；这一条测的是"按键一次实际会走多少"，
    /// 用来判断把线性 `sort()` 换成 `BinaryHeap` 到底值不值。用 `--nocapture`
    /// 打印不同页数 / 取条数下的实测 µs。只锁正确性，不锁时间。
    #[test]
    fn iterator_head_only_cost_is_bounded_by_limit() {
        use std::time::Instant;
        for (k, n) in [
            (4usize, 64usize),
            (16, 64),
            (64, 64),
            (4, 256),
            (16, 256),
            (64, 256),
        ] {
            for take in [10usize, 50, 200] {
                let entries: Vec<(&str, f32)> = vec![("词", 1.0); n];
                let mut iter = DictEntryIterator::new();
                for j in 0..k {
                    iter.add_chunk(chunk(&[j as SyllableId], &entries, ""));
                }
                let t0 = Instant::now();
                let mut seen = 0usize;
                while seen < take && iter.peek().is_some() {
                    seen += 1;
                    if !iter.next() {
                        break;
                    }
                }
                let us = t0.elapsed().as_micros();
                println!("1268-head k={k:2} n={n:3} take={take:3} walk={us:6}µs");
                assert_eq!(seen, take.min(k * n), "只取前 M 条不能少给");
            }
        }
    }

    // ---------------------------------------------------------- librime#1268 回归

    /// 改造前的出词算法（线性扫剩余页 + swap）参考版，只用于对照。
    ///
    /// 换堆之后"谁先出"必须与它逐条一致（并列项除外），所以这里原样保留旧逻辑。
    struct LegacyLinearIterator {
        chunks: Vec<Chunk>,
        index: usize,
    }

    impl LegacyLinearIterator {
        fn new(chunks: Vec<Chunk>) -> Self {
            Self { chunks, index: 0 }
        }

        fn exhausted(&self) -> bool {
            self.index >= self.chunks.len()
        }

        fn sort(&mut self) {
            let index = self.index;
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

        fn advance(&mut self) -> bool {
            if self.exhausted() {
                return false;
            }
            let chunk = &mut self.chunks[self.index];
            chunk.cursor += 1;
            if chunk.cursor >= chunk.entries.len() {
                self.index += 1;
            }
            if self.exhausted() {
                return false;
            }
            self.sort();
            true
        }

        fn drain(mut self) -> Vec<String> {
            let mut out = Vec::new();
            while !self.exhausted() {
                let chunk = &self.chunks[self.index];
                out.push(chunk.entries[chunk.cursor].0.clone());
                if !self.advance() {
                    break;
                }
            }
            out
        }
    }

    /// 权重全不相同时（没有并列），堆合并的出词顺序必须与旧实现逐条一致。
    #[test]
    fn heap_order_matches_legacy_for_distinct_weights() {
        let mut iter = DictEntryIterator::new();
        let mut legacy_chunks = Vec::new();
        for k in 0..8u32 {
            let entries: Vec<(String, f32)> = (0..6)
                .map(|i| (format!("词{k}-{i}"), (k * 10 + i) as f32))
                .collect();
            let mut c = chunk(&[k as SyllableId], &[], "");
            c.entries = entries;
            legacy_chunks.push(c.clone());
            iter.add_chunk(c);
        }
        // 再叠两页补全（剩余码更长、恒排后面），把三级比较都走到
        for k in 0..2u32 {
            let entries: Vec<(String, f32)> = (0..4)
                .map(|i| (format!("补{k}-{i}"), (k * 10 + i) as f32))
                .collect();
            let mut c = chunk(&[k as SyllableId], &[], "ao");
            c.entries = entries;
            legacy_chunks.push(c.clone());
            iter.add_chunk(c);
        }

        let got: Vec<String> = iter
            .collect_entries()
            .into_iter()
            .map(|e| e.text)
            .collect();
        let want = LegacyLinearIterator::new(legacy_chunks).drain();
        assert_eq!(got, want, "换堆不能改变无并列时的出词顺序");
    }

    /// 比较完全相等时按插入序稳定出词（旧实现是"当前数组里排最前的那个"，换堆后要可复现）。
    #[test]
    fn equal_heads_break_ties_by_insertion_order() {
        let mut iter = DictEntryIterator::new();
        for k in 0..4 {
            iter.add_chunk(chunk(&[k as SyllableId], &[("甲", 1.0f32), ("乙", 1.0f32)], ""));
        }
        let order: Vec<String> = iter
            .collect_entries()
            .into_iter()
            .map(|e| e.text)
            .collect();
        assert_eq!(
            order,
            vec!["甲", "乙", "甲", "乙", "甲", "乙", "甲", "乙"],
            "全部并列时先装进来的页先出完，且结果每次一致"
        );
    }

    /// `skip()` 必须让 `Peek()` 的缓存与堆缓存一起失效：跳过 n 条后的落点，
    /// 与"没 peek 过、直接 skip n 条"的迭代器一致。
    #[test]
    fn skip_invalidates_cached_entry_and_heap() {
        let build = || {
            let mut iter = DictEntryIterator::new();
            for k in 0..16u32 {
                let entries: Vec<(String, f32)> = (0..5)
                    .map(|i| (format!("词{k}-{i}"), (k * 10 + i) as f32))
                    .collect();
                let mut c = chunk(&[k as SyllableId], &[], "");
                c.entries = entries;
                iter.add_chunk(c);
            }
            iter
        };
        let mut peeked = build();
        let mut fresh = build();
        // 先各走 6 条，让两边都建好堆且排列一致
        for _ in 0..6 {
            assert!(peeked.peek().is_some());
            assert!(peeked.next());
            assert!(fresh.peek().is_some());
            assert!(fresh.next());
        }
        assert!(peeked.peek().is_some());
        assert!(peeked.skip(3));
        assert!(fresh.skip(3));
        assert_eq!(
            peeked.peek().map(|e| e.text.clone()),
            fresh.peek().map(|e| e.text.clone()),
            "skip 之后不能还拿着 skip 之前的缓存条目"
        );
    }

    /// 堆已经建好、走过若干条之后 `skip`，剩余条数必须仍然精确。
    #[test]
    fn skip_after_heap_walk_keeps_remaining_total() {
        let mut iter = DictEntryIterator::new();
        let (k, n) = (32usize, 8usize);
        for c in 0..k {
            let entries: Vec<(String, f32)> = (0..n)
                .map(|i| (format!("词{c}-{i}"), (c * 100 + i) as f32))
                .collect();
            let mut chunk = chunk(&[c as SyllableId], &[], "");
            chunk.entries = entries;
            iter.add_chunk(chunk);
        }
        for _ in 0..10 {
            assert!(iter.peek().is_some());
            assert!(iter.next());
        }
        assert!(iter.skip(5));
        let rest = iter.collect_entries();
        assert_eq!(rest.len(), k * n - 15, "10 条走过 + 5 条跳过，其余一条不少");
    }

    /// 堆模式下全遍历：每条只出一次，耗尽后不再出条目。
    #[test]
    fn draining_heap_yields_each_entry_once() {
        use std::collections::HashSet;
        let mut iter = DictEntryIterator::new();
        let (k, n) = (64usize, 16usize);
        for c in 0..k {
            let entries: Vec<(String, f32)> = (0..n)
                .map(|i| (format!("词{c}-{i}"), ((c * n + i) % 37) as f32))
                .collect();
            let mut chunk = chunk(&[c as SyllableId], &[], "");
            chunk.entries = entries;
            iter.add_chunk(chunk);
        }
        let total = iter.entry_count();
        let mut seen: HashSet<String> = HashSet::new();
        let mut count = 0usize;
        while let Some(entry) = iter.peek() {
            assert!(seen.insert(entry.text.clone()), "同一条不能出两次");
            count += 1;
            if !iter.next() {
                break;
            }
        }
        assert_eq!(count, total, "全遍历条数必须等于 entry_count()");
        assert!(iter.peek().is_none(), "耗尽后 peek 不能再给条目");
        assert!(!iter.next(), "耗尽后 next 必须返回 false");
        assert!(iter.collect_entries().is_empty(), "耗尽后再收一遍必须为空");
    }
}
