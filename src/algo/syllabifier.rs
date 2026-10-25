//! librime `src/rime/algo/syllabifier.h`(78 行) + `syllabifier.cc`(327 行) 的 Rust 移植。
//!
//! 音节切分：输入串 × 拼写表（prism）→ [`SyllableGraph`]。图上有三类东西：
//!
//! * `vertices`：位置 → 到达该位置时"最模糊的拼写类型"（越小越好）；
//! * `edges[start][end]`：一条边 = 一段输入，值是该段能代表的**音节集合**
//!   （因为拼写演算会让一个拼写对应多个音节）；
//! * `indices`：`edges` 的转置（`start` → 音节 id → 该音节的属性列表），
//!   `gear/script_translator.cc` 直接吃这个结构。
//!
//! ## 依赖：`Prism` 接口
//!
//! C++ 这里吃 `dict/Prism`（darts 双数组 trie + `prism::SpellingMap`），而 dict 层
//! **还没移植**。所以本模块把用到的那 5 个方法抽成 [`Prism`] trait
//! （`syllabifier.cc:73,82,87,97,126,228,241` 的调用点逐一对齐），
//! 并给出一个纯内存实现 [`InMemoryPrism`]：**行为与 `Prism::Build` 一致**
//! （含"没建 script 时拼写 id 映射到自己、属性全默认"这个关键退化，
//! 见 `dict/prism.cc:40-48`、`:69-80`）。
//!
//! `Corrector`（`dict/corrector.cc`，模糊纠错）同样只移植**接口与调用点**
//! （`syllabifier.cc:89-106`、`137-141`）：`EnableCorrection` 之后的行为已就位，
//! 但纠错算法本身属于 dict 层，不在本次范围。
//!
//! ## 照抄的古怪点
//!
//! 1. 顶点表是"**先到先得**"（`syllabifier.cc:47-53`）：优先队列按 `(位置, 类型)`
//!    升序弹，`vertices` 里已有的位置直接 `continue` 丢弃更差的类型。注释掉的
//!    `std::min` 说明作者曾想取最小，最终选择了"先到先得"。
//! 2. 剪枝时 `last_type = max(vertices[farthest], kFuzzySpelling)`
//!    （`syllabifier.cc:178-179`）—— **下限是 Fuzzy(1)**，所以最远点类型是 Normal 时，
//!    类型 > Fuzzy 的边（简拼/补全/歧义）在这一步会被全部剪掉。
//!    而 `last_type` 在整个循环里**不再更新**。
//! 3. `CheckOverlappedSpellings` 会在遍历 `edges[i]` 的过程中**反过来改**
//!    `vertices[joint]`（`syllabifier.cc:303`），而 `i` 是**从大到小**遍历的 ——
//!    被改的 `joint > i` 早就处理完了，所以这处修改对本次剪枝没有影响。
//! 4. `Transpose` 按 end **逆序**填 `indices`（`syllabifier.cc:314` 的
//!    `boost::adaptors::reverse`），所以同一 start 下长音节排在前面。
#![allow(dead_code)]

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use super::algebra::{Projection, Script};
use super::spelling::{SpellingProperties, SpellingType};

/// `syllabifier.h:22`
pub type SyllableId = i32;

/// `syllabifier.cc:28-29`：两条罚分（自然对数值）。
pub const K_COMPLETION_PENALTY: f64 = -2.995732273553991; // log(0.05)
pub const K_CORRECTION_CREDIBILITY: f64 = -4.605170185988091; // log(0.01)

/// `syllabifier.h:24-29`：`SpellingProperties` + 歧义切分来源集合。
///
/// 用 `Deref` 指向内部的 [`SpellingProperties`]，这样 `props.r#type` / `props.end_pos`
/// 的写法和 C++ 的继承一致。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EdgeProperties {
    base: SpellingProperties,
    /// 切分歧義編碼段的起始位置（`syllabifier.h:28`）
    ambiguous_source_positions: BTreeSet<usize>,
}

impl EdgeProperties {
    /// 对应 `EdgeProperties(SpellingProperties sup)`（`syllabifier.h:25`）。
    fn from_properties(base: SpellingProperties) -> Self {
        Self {
            base,
            ambiguous_source_positions: BTreeSet::new(),
        }
    }
}

impl std::ops::Deref for EdgeProperties {
    type Target = SpellingProperties;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl std::ops::DerefMut for EdgeProperties {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.base
    }
}

/// `syllabifier.h:31`：`SyllableId -> EdgeProperties`
pub type SpellingMap = BTreeMap<SyllableId, EdgeProperties>;
/// `syllabifier.h:32`：`位置 -> 拼写类型`
pub type VertexMap = BTreeMap<usize, SpellingType>;
/// `syllabifier.h:33`：`end -> SpellingMap`
pub type EndVertexMap = BTreeMap<usize, SpellingMap>;
/// `syllabifier.h:34`：`start -> EndVertexMap`
pub type EdgeMap = BTreeMap<usize, EndVertexMap>;
/// `syllabifier.h:36-38`：转置后的索引
type SpellingPropertiesList = Vec<EdgeProperties>;
type SpellingIndex = BTreeMap<SyllableId, SpellingPropertiesList>;
pub type SpellingIndices = BTreeMap<usize, SpellingIndex>;

/// `syllabifier.h:40-46`
#[derive(Debug, Clone, Default)]
pub struct SyllableGraph {
    input_length: usize,
    pub interpreted_length: usize,
    vertices: VertexMap,
    pub edges: EdgeMap,
    indices: SpellingIndices,
}

impl SyllableGraph {
    /// `syllabifier.h:45` 的 `SpellingIndices indices;` —— **只读**访问器。
    ///
    /// 它是 `edges` 的转置（`start` → 音节 id → 该音节的一串属性），由
    /// [`Syllabifier::build_syllable_graph`] 末尾的 `Transpose`（`syllabifier.cc:311-321`）
    /// 填好；算法语义没动，这里只是让 algo 层之外（`dict::table::Table::query`）看得见。
    ///
    /// 结构：`BTreeMap<start_pos, BTreeMap<SyllableId, Vec<EdgeProperties>>>`，
    /// 同一个 `start` 下**长音节排在前面**（`Transpose` 逆序遍历 end），与 C++ 同序。
    pub fn indices(&self) -> &SpellingIndices {
        &self.indices
    }

    /// `syllabifier.h:41` 的 `size_t input_length;`（C++ 是公开成员，这里补只读访问器）。
    pub fn input_length(&self) -> usize {
        self.input_length
    }

    /// `syllabifier.h:43` 的 `VertexMap vertices;`（同上，C++ 是公开成员）。
    pub fn vertices(&self) -> &VertexMap {
        &self.vertices
    }

    /// 造一张只有长度的空图 —— 对应 C++ 测试里直接写
    /// `g.input_length = input.length(); g.interpreted_length = g.input_length;`
    /// （`test/table_test.cc:160-161`）。**算法不用它**，只给"手工造图"的调用方。
    pub fn with_input_length(input_length: usize) -> Self {
        Self {
            input_length,
            interpreted_length: input_length,
            ..Self::default()
        }
    }

    /// 手工登记一条边 —— **算法不用它**，只给"照 C++ 公开字段造图"的调用方
    /// （典型是 `dict::table` 的对拍测试）。
    ///
    /// C++ 里这三步是分开写的（`test/table_test.cc:167-178`）：
    ///
    /// ```text
    /// g.edges[0][2][1].type = kNormalSpelling;      // 边 [0,2) 能表示音节 1
    /// g.edges[0][2][1].end_pos = 2;
    /// g.indices[0][1].push_back(&g.edges[0][2][1]); // 转置表里存的是指针
    /// ```
    ///
    /// Rust 侧 `indices` 存的是**克隆值**（不是指向 `edges` 的指针），两步必须一起做；
    /// 而 `end_pos` 只有 algo 层能写（`spelling.rs` 里是 `pub(super)`），
    /// 所以收成一个方法：写 `edges[start][end][syllable_id]`，并把同一份属性
    /// （`end_pos` 强制置为 `end`）追加到 `indices[start][syllable_id]` 末尾。
    ///
    /// 返回 `false` = 同一个 `(start, end, syllable_id)` 已存在（C++ 里是覆盖）。
    pub fn add_edge(
        &mut self,
        start: usize,
        end: usize,
        syllable_id: SyllableId,
        mut properties: SpellingProperties,
    ) -> bool {
        properties.end_pos = end;
        let spellings = self
            .edges
            .entry(start)
            .or_default()
            .entry(end)
            .or_default();
        if spellings.contains_key(&syllable_id) {
            return false;
        }
        spellings.insert(
            syllable_id,
            EdgeProperties::from_properties(properties.clone()),
        );
        self.indices
            .entry(start)
            .or_default()
            .entry(syllable_id)
            .or_default()
            .push(EdgeProperties::from_properties(properties));
        true
    }
}

/// `Prism::Match`（`dict/prism.h:74`）：`value` = 拼写 id，`length` = 键长（**字节**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrismMatch {
    value: SyllableId,
    length: usize,
}

/// 一次性拼写描述：对应 `prism::SpellingDescriptor` 解包后的东西
/// （`dict/prism.cc:69-80` 的 `SpellingAccessor::syllable_id/properties`）。
type SpellingDescriptor = (SyllableId, SpellingProperties);

/// `dict/Prism` 里音节切分用到的 5 个方法。见 `dict/prism.h:74-100`。
pub trait Prism {
    /// `prism.cc:339`：最长键的字节数（拼写演算开启时用于限制步进长度）。
    fn max_key_length(&self) -> usize;

    /// `prism.cc:268-275`：精确查键。
    fn get_value(&self, key: &str) -> Option<SyllableId>;

    /// `prism.cc:279-286`：所有"是 `key` 前缀"的键，按长度升序。
    fn common_prefix_search(&self, key: &str) -> Vec<PrismMatch>;

    /// `prism.cc:288-329`：以 `key` 为前缀的键（补全用）。
    fn expand_search(&self, key: &str, limit: usize) -> Vec<PrismMatch>;

    /// `prism.cc:331-333` + `dict/prism.cc:40-80`：拼写 id → 一组（音节 id, 属性）。
    ///
    /// ⚠️ 没有拼写表时 C++ 的 accessor **不空**：它把拼写 id 自己当音节 id 返回，
    /// 属性全是默认值。这里必须一样。
    fn query_spelling(&self, spelling_id: SyllableId) -> Vec<SpellingDescriptor>;
}

/// 纯内存 `Prism`：行为对齐 `Prism::Build(syllabary[, script])`（`dict/prism.cc:138-260`）。
#[derive(Debug, Clone, Default)]
pub struct InMemoryPrism {
    /// 键 → 拼写 id（id = 建树时按键序遍历的下标，与 darts 的行为一致）
    keys: BTreeMap<String, SyllableId>,
    /// 键序表（前缀搜索用）
    ordered_keys: Vec<String>,
    /// 拼写 id → 描述符；`None` = 建树时没给 script（C++ 的 `spelling_map_ == NULL`）
    spelling_map: Option<Vec<Vec<SpellingDescriptor>>>,
    max_key_length: usize,
}

impl InMemoryPrism {
    /// 对应 `Prism::Build(keyset)`：只建键，没有拼写表。
    fn from_syllabary<'a, I>(syllabary: I) -> Self
    where
        I: IntoIterator<Item = &'a String>,
    {
        // C++ 侧 `Syllabary` 是 `set<string>`，键序 = 字典序
        let mut ordered: Vec<String> = syllabary.into_iter().cloned().collect();
        ordered.sort();
        Self::build(ordered, None)
    }

    /// 对应 `Prism::Build(syllabary, &script)`：既有键表也有拼写表。
    ///
    /// `syllabary` 必须**有序**（C++ 的 `Syllabary` 是 `set`），`script` 的键就是 prism 的键。
    pub fn from_script(syllabary: &BTreeSet<String>, script: &Script) -> Self {
        // `dict/prism.cc:284-288`：音节 id 按 syllabary 的顺序分配
        let syllable_to_id: BTreeMap<&String, SyllableId> = syllabary
            .iter()
            .enumerate()
            .map(|(i, s)| (s, i as SyllableId))
            .collect();
        let keys: Vec<String> = script.keys().cloned().collect();
        // `dict/prism.cc:216-234`：按 script 的顺序（= 键序）填描述符
        let mut spelling_map: Vec<Vec<SpellingDescriptor>> = Vec::with_capacity(keys.len());
        for spellings in script.iter().map(|(_, v)| v) {
            let mut list = Vec::with_capacity(spellings.len());
            for spelling in spellings {
                // script 里的拼写可能不在 syllabary 里（演算生成了新串）——C++ 会
                // `syllable_to_id[j->str]` 静默插入 id 0；这里跳过并保持同样"能跑"的效果。
                let Some(&id) = syllable_to_id.get(&spelling.str) else {
                    continue;
                };
                list.push((id, spelling.properties.clone()));
            }
            spelling_map.push(list);
        }
        Self::build(keys, Some(spelling_map))
    }

    fn build(ordered_keys: Vec<String>, spelling_map: Option<Vec<Vec<SpellingDescriptor>>>) -> Self {
        let mut keys = BTreeMap::new();
        let mut max_key_length = 0usize;
        for (i, key) in ordered_keys.iter().enumerate() {
            keys.insert(key.clone(), i as SyllableId);
            max_key_length = max_key_length.max(key.len());
        }
        Self {
            keys,
            ordered_keys,
            spelling_map,
            max_key_length,
        }
    }
}

impl Prism for InMemoryPrism {
    fn max_key_length(&self) -> usize {
        self.max_key_length
    }

    fn get_value(&self, key: &str) -> Option<SyllableId> {
        self.keys.get(key).copied()
    }

    fn common_prefix_search(&self, key: &str) -> Vec<PrismMatch> {
        let mut result: Vec<PrismMatch> = Vec::new();
        for k in &self.ordered_keys {
            if k.len() > key.len() {
                // 键序是字典序，不是长度序 —— 不能 break，只能 continue
                continue;
            }
            if key.starts_with(k.as_str()) {
                result.push(PrismMatch {
                    value: self.keys[k],
                    length: k.len(),
                });
            }
        }
        // darts 的 commonPrefixSearch 按长度升序返回
        result.sort_by_key(|m| m.length);
        result
    }

    fn expand_search(&self, key: &str, limit: usize) -> Vec<PrismMatch> {
        let mut result = Vec::new();
        for k in &self.ordered_keys {
            if !k.starts_with(key) {
                continue;
            }
            result.push(PrismMatch {
                value: self.keys[k],
                length: k.len(),
            });
            if limit > 0 && result.len() >= limit {
                break;
            }
        }
        result
    }

    fn query_spelling(&self, spelling_id: SyllableId) -> Vec<SpellingDescriptor> {
        match &self.spelling_map {
            Some(map) => match map.get(spelling_id as usize) {
                Some(list) => list.clone(),
                // C++：`spelling_id >= spelling_map->size` → accessor 不空、属性默认
                None => vec![(spelling_id, SpellingProperties::default())],
            },
            // C++：`spelling_map_ == NULL` → 拼写 id 自己当音节 id，属性默认
            None => vec![(spelling_id, SpellingProperties::default())],
        }
    }
}

/// 模糊纠错接口。对应 `dict/corrector.h` 的 `Corrector::ToleranceSearch`
/// 在 `syllabifier.cc:89-106` 的**调用点**。
pub trait Corrector {
    /// 返回 (音节 id, 匹配到的拼写串)。C++ 侧 `corrections` 是
    /// `map<SyllableId, Correction>`，只用到 `m.first` 与 `m.second.length`。
    fn tolerance_search(&self, prism: &dyn Prism, input: &str, limit: usize) -> Vec<(SyllableId, String)>;
}

/// `syllabifier.h:48-74`
pub struct Syllabifier {
    /// 分隔符集合（注意是**字符串**，按字符查，`syllabifier.cc:61`）
    delimiters: String,
    enable_completion: bool,
    strict_spelling: bool,
    /// C++ 是裸指针 `Projection*`；这里持有所有权
    canonicalizer: Option<Projection>,
    corrector: Option<Box<dyn Corrector>>,
}

impl Default for Syllabifier {
    fn default() -> Self {
        Self::new()
    }
}

impl Syllabifier {
    /// `syllabifier.h:50-58`
    fn new() -> Self {
        Self {
            delimiters: String::new(),
            enable_completion: false,
            strict_spelling: false,
            canonicalizer: None,
            corrector: None,
        }
    }

    pub fn with_options(
        delimiters: &str,
        enable_completion: bool,
        strict_spelling: bool,
        canonicalizer: Option<Projection>,
    ) -> Self {
        Self {
            delimiters: delimiters.to_string(),
            enable_completion,
            strict_spelling,
            canonicalizer,
            corrector: None,
        }
    }

    fn set_canonicalizer(&mut self, canonicalizer: Projection) {
        self.canonicalizer = Some(canonicalizer);
    }

    /// `syllabifier.cc:323-325`
    fn enable_correction(&mut self, corrector: Box<dyn Corrector>) {
        self.corrector = Some(corrector);
    }

    fn is_delimiter(&self, byte: u8) -> bool {
        self.delimiters.as_bytes().contains(&byte)
    }

    /// 前向搜索：拼写演算开启时逐个长度精确查（`syllabifier.cc:71-88`），
    /// 否则用前缀搜索（`syllabifier.cc:86-87`）。
    fn search(&mut self, current_input: &str, prism: &dyn Prism) -> Vec<PrismMatch> {
        match self.canonicalizer.as_mut() {
            Some(canonicalizer) => {
                let limit = current_input.len().min(prism.max_key_length());
                let mut matches = Vec::new();
                for len in 1..=limit {
                    // C++ 按字节取子串；非字符边界不可能命中任何合法键，跳过即可
                    let Some(sub) = current_input.get(..len) else {
                        continue;
                    };
                    let mut syllable = sub.to_string();
                    canonicalizer.apply_str(&mut syllable);
                    // `syllabifier.cc:80-84`：重排打破前缀单调性，所以每次精确查
                    if let Some(value) = prism.get_value(&syllable) {
                        matches.push(PrismMatch { value, length: len });
                    }
                }
                matches
            }
            None => prism.common_prefix_search(current_input),
        }
    }

    /// `syllabifier.cc:31-275`：建音节图，返回"最远到达的位置"（字节）。
    pub fn build_syllable_graph(
        &mut self,
        input: &str,
        prism: &dyn Prism,
        graph: &mut SyllableGraph,
    ) -> i32 {
        if input.is_empty() {
            return 0;
        }
        let bytes = input.as_bytes();
        let mut farthest = 0usize;
        // `syllabifier.cc:20-22`：`std::greater<Vertex>` → 最小堆，按 (位置, 类型) 字典序
        let mut queue: BinaryHeap<Reverse<(usize, SpellingType)>> = BinaryHeap::new();
        queue.push(Reverse((0, SpellingType::Normal))); // start

        while let Some(Reverse((current_pos, vertex_type))) = queue.pop() {
            // `syllabifier.cc:46-53`：先到先得，后来的（更差的类型）直接丢
            if graph.vertices.contains_key(&current_pos) {
                continue;
            }
            graph.vertices.insert(current_pos, vertex_type);

            if current_pos > farthest {
                farthest = current_pos;
            }

            // `syllabifier.cc:58-62`：吃掉前导分隔符
            let mut begin_pos = current_pos;
            while begin_pos < bytes.len() && self.is_delimiter(bytes[begin_pos]) {
                begin_pos += 1;
            }

            // `syllabifier.cc:66-106`：看往前走一个音节能到哪
            let mut matches: Vec<PrismMatch> = Vec::new();
            let mut exact_match_syllables: BTreeSet<SyllableId> = BTreeSet::new();
            if let Some(current_input) = input.get(begin_pos..) {
                matches = self.search(current_input, prism);
                if let Some(corrector) = self.corrector.as_ref() {
                    for m in &matches {
                        exact_match_syllables.insert(m.value);
                    }
                    // `syllabifier.cc:93-95`：固定 limit = 5
                    let corrections = corrector.tolerance_search(prism, current_input, 5);
                    for (syllable_id, matched) in corrections {
                        // `syllabifier.cc:97-104`：只收"普通且非纠错"的拼写
                        for (_, props) in prism.query_spelling(syllable_id) {
                            if props.r#type == SpellingType::Normal && !props.is_correction {
                                matches.push(PrismMatch {
                                    value: syllable_id,
                                    length: matched.len(),
                                });
                                break;
                            }
                        }
                    }
                }
            }

            let leading_gap = begin_pos - current_pos;
            if matches.is_empty() {
                continue;
            }
            let end_vertices = graph.edges.entry(current_pos).or_default();
            for m in &matches {
                if m.length == 0 {
                    continue;
                }
                let mut end_pos = current_pos + leading_gap + m.length;
                // `syllabifier.cc:115-118`：吃掉尾随分隔符
                while end_pos < bytes.len() && self.is_delimiter(bytes[end_pos]) {
                    end_pos += 1;
                }
                let matches_input = current_pos == 0 && end_pos == bytes.len();
                let spellings = end_vertices.entry(end_pos).or_default();
                let mut end_vertex_type = SpellingType::Invalid;
                // `syllabifier.cc:126-155`
                for (syllable_id, props) in prism.query_spelling(m.value) {
                    if self.strict_spelling
                        && matches_input
                        && props.r#type != SpellingType::Normal
                    {
                        // 取消"模糊音节/简拼"作为整词单音节的资格（`syllabifier.cc:130-132`）
                        continue;
                    }
                    let mut props = props;
                    props.end_pos = end_pos;
                    if self.corrector.is_some()
                        && !exact_match_syllables.contains(&m.value)
                    {
                        // `syllabifier.cc:137-141`
                        props.is_correction = true;
                        props.credibility = K_CORRECTION_CREDIBILITY;
                    }
                    match spellings.get_mut(&syllable_id) {
                        None => {
                            spellings.insert(
                                syllable_id,
                                EdgeProperties::from_properties(props.clone()),
                            );
                        }
                        Some(existing) => {
                            // `syllabifier.cc:146`：只更新类型
                            existing.base.r#type = existing.base.r#type.min(props.r#type);
                        }
                    }
                    // `syllabifier.cc:150-152`：end_vertex_type 取"最优（最小）"类型
                    if end_vertex_type > props.r#type && !props.is_correction {
                        end_vertex_type = props.r#type;
                    }
                }
                if spellings.is_empty() {
                    // `syllabifier.cc:156-160`
                    end_vertices.remove(&end_pos);
                    continue;
                }
                // `syllabifier.cc:164-166`：路径上的类型取更差的那个
                if end_vertex_type < vertex_type {
                    end_vertex_type = vertex_type;
                }
                queue.push(Reverse((end_pos, end_vertex_type)));
            }
        }

        // `syllabifier.cc:174-222`：剪掉过时的顶点与边
        let mut good: BTreeSet<usize> = BTreeSet::new();
        good.insert(farthest);
        // fuzzy spellings are immune to invalidation by normal spellings
        let farthest_type = *graph
            .vertices
            .entry(farthest)
            .or_insert(SpellingType::Normal);
        let last_type = farthest_type.max(SpellingType::Fuzzy);
        for i in (0..farthest).rev() {
            if !graph.vertices.contains_key(&i) {
                continue;
            }
            // 先收集一份 end 列表：处理过程中会改 edges[i]，也可能改 vertices
            let ends: Vec<usize> = match graph.edges.get(&i) {
                Some(e) => e.keys().copied().collect(),
                None => Vec::new(),
            };
            for end in ends {
                if !good.contains(&end) {
                    // not connected（`syllabifier.cc:185-189`）
                    if let Some(e) = graph.edges.get_mut(&i) {
                        e.remove(&end);
                    }
                    continue;
                }
                // 取出这条边的音节表来过滤（`syllabifier.cc:192-205`）
                let Some(spellings) = graph.edges.get_mut(&i).and_then(|e| e.remove(&end)) else {
                    continue;
                };
                let mut edge_type = SpellingType::Invalid;
                let mut kept: SpellingMap = SpellingMap::new();
                for (syllable_id, props) in spellings {
                    if props.is_correction {
                        // Don't care correction edges（`syllabifier.cc:194-197`）
                        kept.insert(syllable_id, props);
                        continue;
                    }
                    if props.r#type > last_type {
                        // `syllabifier.cc:198-200`
                        continue;
                    }
                    if props.r#type < edge_type {
                        edge_type = props.r#type;
                    }
                    kept.insert(syllable_id, props);
                }
                if kept.is_empty() {
                    // `syllabifier.cc:206-208`：整条边都废了
                    continue;
                }
                if let Some(e) = graph.edges.get_mut(&i) {
                    e.insert(end, kept);
                }
                if edge_type < SpellingType::Abbreviation {
                    // `syllabifier.cc:209-211`
                    self.check_overlapped_spellings(graph, i, end);
                }
            }
            // `syllabifier.cc:214-222`
            let vertex_type = graph
                .vertices
                .get(&i)
                .copied()
                .unwrap_or(SpellingType::Normal);
            let edges_empty = graph.edges.get(&i).map_or(true, |e| e.is_empty());
            if vertex_type > last_type || edges_empty {
                graph.vertices.remove(&i);
                graph.edges.remove(&i);
                continue;
            }
            good.insert(i);
        }

        // `syllabifier.cc:224-265`：补全
        if self.enable_completion && farthest < bytes.len() {
            const K_EXPAND_SEARCH_LIMIT: usize = 512;
            if let Some(rest) = input.get(farthest..) {
                let keys = prism.expand_search(rest, K_EXPAND_SEARCH_LIMIT);
                if !keys.is_empty() {
                    let current_pos = farthest;
                    let end_pos = bytes.len();
                    let code_length = end_pos - current_pos;
                    let end_vertices = graph.edges.entry(current_pos).or_default();
                    let spellings = end_vertices.entry(end_pos).or_default();
                    for m in &keys {
                        if m.length < code_length {
                            continue;
                        }
                        for (syllable_id, mut props) in prism.query_spelling(m.value) {
                            if props.r#type < SpellingType::Abbreviation {
                                props.r#type = SpellingType::Completion;
                                props.credibility += K_COMPLETION_PENALTY;
                                props.end_pos = end_pos;
                                // C++ 是 map::insert（不覆盖已有）
                                spellings.entry(syllable_id).or_insert_with(|| {
                                    EdgeProperties::from_properties(props)
                                });
                            }
                        }
                    }
                    if spellings.is_empty() {
                        end_vertices.remove(&end_pos);
                    } else {
                        farthest = end_pos;
                    }
                }
            }
        }

        graph.input_length = input.len();
        graph.interpreted_length = farthest;

        self.transpose(graph);

        farthest as i32
    }

    /// `syllabifier.cc:277-309`
    fn check_overlapped_spellings(&self, graph: &mut SyllableGraph, start: usize, end: usize) {
        if !graph.edges.contains_key(&start) {
            return;
        }
        // 若 "Z" = "YX"，则把 Y 与 X 之间的顶点标记为歧义音节接缝
        let ys: Vec<usize> = graph.edges[&start].keys().copied().collect();
        for joint in ys {
            if joint >= end {
                break;
            }
            if !graph.edges.contains_key(&joint) {
                continue;
            }
            let xs: Vec<usize> = graph.edges[&joint].keys().copied().collect();
            for x in xs {
                if x < end {
                    continue;
                }
                if x == end {
                    // `syllabifier.cc:296-305`：给这条边（X）打上"相对起点构成歧义"
                    if let Some(spellings) = graph.edges.get_mut(&joint).and_then(|e| e.get_mut(&end))
                    {
                        for props in spellings.values_mut() {
                            props.ambiguous_source_positions.insert(start);
                        }
                    }
                    graph.vertices.insert(joint, SpellingType::Ambiguous);
                }
                break;
            }
        }
    }

    /// `syllabifier.cc:311-321`：把 `edges` 转置成 `indices`。
    fn transpose(&self, graph: &mut SyllableGraph) {
        let SyllableGraph { edges, indices, .. } = graph;
        for (start, end_vertices) in edges.iter() {
            let index = indices.entry(*start).or_default();
            // `syllabifier.cc:314`：**逆序**（长的音节排在前面）
            for spellings in end_vertices.values().rev() {
                for (syllable_id, props) in spellings.iter() {
                    index.entry(*syllable_id).or_default().push(props.clone());
                }
            }
        }
    }
}

/// 音节 id → 拼写串（调试/报告用；C++ 侧从 `Prism` 反向映射，这里从 [`Script`] 反查）。
fn spelling_of(script: &Script, syllable_id: SyllableId, syllabary: &[String]) -> Option<String> {
    let syllable = syllabary.get(syllable_id as usize)?;
    let mut found = None;
    for (spelling, list) in script.iter() {
        if list.iter().any(|s| s.str == *syllable) {
            found = Some(spelling.clone());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::calculus::Calculus;
    use crate::algo::spelling::Spelling;

    /// 与 `test/syllabifier_test.cc:19-34` 同一份音節表（**排序后**分配 id）。
    fn fixture_syllables() -> Vec<String> {
        let mut syllables: Vec<String> = [
            "a", "an", "cha", "chan", "chang", "gan", "han", "hang", "na", "tu", "tuan",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        syllables.sort();
        syllables
    }

    fn fixture() -> (InMemoryPrism, BTreeMap<String, SyllableId>) {
        let syllables = fixture_syllables();
        let ids: BTreeMap<String, SyllableId> = syllables
            .iter()
            .enumerate()
            .map(|(i, s)| (s.clone(), i as SyllableId))
            .collect();
        let set: BTreeSet<String> = syllables.iter().cloned().collect();
        (InMemoryPrism::from_syllabary(set.iter()), ids)
    }

    fn build(delimiters: &str, input: &str) -> (SyllableGraph, InMemoryPrism, BTreeMap<String, SyllableId>) {
        let (prism, ids) = fixture();
        let mut s = Syllabifier::with_options(delimiters, false, false, None);
        let mut g = SyllableGraph::default();
        s.build_syllable_graph(input, &prism, &mut g);
        (g, prism, ids)
    }

    fn edge<'a>(g: &'a SyllableGraph, start: usize, end: usize) -> &'a SpellingMap {
        g.edges
            .get(&start)
            .and_then(|e| e.get(&end))
            .unwrap_or_else(|| panic!("边 [{start}, {end}) 不存在"))
    }

    // ---- 以下对应 librime `test/syllabifier_test.cc` 的全部断言 ----

    #[test]
    fn case_alpha() {
        // syllabifier_test.cc:51-66
        let (g, _, ids) = build("", "a");
        assert_eq!("a".len(), g.input_length);
        assert_eq!("a".len(), g.interpreted_length);
        assert_eq!(2, g.vertices.len());
        assert!(g.vertices.contains_key(&1));
        assert_eq!(SpellingType::Normal, g.vertices[&1]);
        let sp = edge(&g, 0, 1);
        assert_eq!(1, sp.len());
        let props = sp.get(&ids["a"]).expect("a 应该在边上");
        assert_eq!(SpellingType::Normal, props.r#type);
        assert_eq!(0.0, props.credibility);
    }

    #[test]
    fn case_failure() {
        // syllabifier_test.cc:68-82
        let (g, _, ids) = build("", "ang");
        assert_eq!("ang".len(), g.input_length);
        assert_eq!("ang".len() - 1, g.interpreted_length);
        assert_eq!(2, g.vertices.len());
        assert!(!g.vertices.contains_key(&1));
        assert!(g.vertices.contains_key(&2));
        assert_eq!(SpellingType::Normal, g.vertices[&2]);
        let sp = edge(&g, 0, 2);
        assert_eq!(1, sp.len());
        assert!(sp.contains_key(&ids["an"]));
    }

    #[test]
    fn case_changan() {
        // syllabifier_test.cc:84-115
        let (g, _, ids) = build("", "changan");
        assert_eq!("changan".len(), g.input_length);
        assert_eq!("changan".len(), g.interpreted_length);
        assert_eq!(4, g.vertices.len());
        // not c'han'gan or c'hang'an
        assert!(!g.vertices.contains_key(&1));
        assert!(g.vertices.contains_key(&4));
        assert!(g.vertices.contains_key(&5));
        assert_eq!(SpellingType::Normal, g.vertices[&4]);
        assert_eq!(SpellingType::Normal, g.vertices[&5]);
        // chan, chang but not cha
        let e0 = g.edges.get(&0).expect("edges[0]");
        assert_eq!(2, e0.len());
        assert!(e0.contains_key(&4));
        assert!(e0.contains_key(&5));
        assert!(e0[&4].contains_key(&ids["chan"]));
        assert!(e0[&5].contains_key(&ids["chang"]));
        // gan$
        let e4 = g.edges.get(&4).expect("edges[4]");
        assert_eq!(1, e4.len());
        assert!(e4.contains_key(&7));
        assert!(e4[&7].contains_key(&ids["gan"]));
        // an$
        let e5 = g.edges.get(&5).expect("edges[5]");
        assert_eq!(1, e5.len());
        assert!(e5.contains_key(&7));
        assert!(e5[&7].contains_key(&ids["an"]));
    }

    #[test]
    fn case_tuan() {
        // syllabifier_test.cc:117-141
        let (g, _, ids) = build("", "tuan");
        assert_eq!("tuan".len(), g.input_length);
        assert_eq!("tuan".len(), g.interpreted_length);
        assert_eq!(3, g.vertices.len());
        // both tu'an and tuan
        assert!(g.vertices.contains_key(&2));
        assert!(g.vertices.contains_key(&4));
        assert_eq!(SpellingType::Ambiguous, g.vertices[&2]);
        assert_eq!(SpellingType::Normal, g.vertices[&4]);
        let e0 = g.edges.get(&0).expect("edges[0]");
        assert_eq!(2, e0.len());
        assert!(e0.contains_key(&2));
        assert!(e0.contains_key(&4));
        assert!(e0[&2].contains_key(&ids["tu"]));
        assert!(e0[&4].contains_key(&ids["tuan"]));
        // an$
        let e2 = g.edges.get(&2).expect("edges[2]");
        assert_eq!(1, e2.len());
        assert!(e2.contains_key(&4));
        assert!(e2[&4].contains_key(&ids["an"]));
    }

    #[test]
    fn case_chaining_ambiguity() {
        // syllabifier_test.cc:143-151
        let (g, _, _) = build("", "anana");
        assert_eq!("anana".len(), g.input_length);
        assert_eq!("anana".len(), g.interpreted_length);
        assert_eq!("anana".len() + 1, g.vertices.len());
    }

    #[test]
    fn transposed_syllable_graph() {
        // syllabifier_test.cc:153-165
        let (g, _, ids) = build("", "changan");
        let i0 = g.indices.get(&0).expect("indices[0]");
        assert_eq!(2, i0.len());
        assert!(i0.contains_key(&ids["chan"]));
        assert!(i0.contains_key(&ids["chang"]));
        let chan = &i0[&ids["chan"]];
        assert_eq!(1, chan.len());
        assert_eq!(4, chan[0].end_pos);
    }

    fn trim_case(input: &str, expected_end: usize) {
        let (mut g, _, ids) = build(" '", input);
        assert_eq!(input.len(), g.input_length);
        assert_eq!(input.len(), g.interpreted_length);
        assert_eq!(2, g.vertices.len());
        assert!(g.vertices.contains_key(&expected_end));
        // ⚠️ C++ 那句 `EXPECT_EQ(kNormalSpelling, g.vertices[1])` 其实是
        // `std::map::operator[]` **插入了默认值 0**（= kNormalSpelling），断言恒真。
        // 这里照样用 `or_insert` 复刻（含插入副作用），并注明它测不出任何东西。
        assert_eq!(
            SpellingType::Normal,
            *g.vertices.entry(1).or_insert(SpellingType::Normal)
        );
        let sp = edge(&g, 0, expected_end);
        assert_eq!(1, sp.len());
        let props = sp.get(&ids["a"]).expect("a");
        assert_eq!(SpellingType::Normal, props.r#type);
        assert_eq!(0.0, props.credibility);
    }

    #[test]
    fn trim_leading_delimiters() {
        // syllabifier_test.cc:167-182
        trim_case("''a", 3);
    }

    #[test]
    fn trim_trailing_delimiters() {
        // syllabifier_test.cc:184-199
        trim_case("a''", 3);
    }

    #[test]
    fn trim_both_leading_and_trailing_delimiters() {
        // syllabifier_test.cc:201-216
        trim_case("''a''", 5);
    }

    // ---- 宫保拼音并击：`CanonicalizeSyllabifierTest`（syllabifier_test.cc:218-360） ----

    fn canonicalize_fixture() -> (InMemoryPrism, BTreeSet<String>) {
        let syllabary: BTreeSet<String> = ["ZF", "ZFUR", "ZFURO", "SA", "HE", "GE", "HGE"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // 音節表本身也就是这些（C++ 的 `Prism::Build(syllabary)`，不带 script）
        let prism = InMemoryPrism::from_syllabary(syllabary.iter());
        (prism, syllabary)
    }

    fn canonicalizer() -> Projection {
        Projection::from_rules(&["reorder SCZHLFGDBKTPIUÜANREO".to_string()]).expect("reorder 规则")
    }

    #[test]
    fn canonicalize_basic_inversion() {
        // syllabifier_test.cc:262-282
        let (prism, _) = canonicalize_fixture();
        let mut s = Syllabifier::with_options("'", false, false, Some(canonicalizer()));
        let mut g = SyllableGraph::default();
        let farthest = s.build_syllable_graph("FZURO", &prism, &mut g);
        assert_eq!(5, farthest);
        assert_eq!(5, g.interpreted_length);
        let expected = prism.get_value("ZFURO").expect("ZFURO");
        let ends = g.edges.get(&0).expect("edges[0]");
        assert!(ends.contains_key(&5));
        assert!(ends[&5].contains_key(&expected));
    }

    #[test]
    fn canonicalize_cross_hand_interleaving() {
        // syllabifier_test.cc:285-298
        let (prism, _) = canonicalize_fixture();
        let mut s = Syllabifier::with_options("'", false, false, Some(canonicalizer()));
        let mut g = SyllableGraph::default();
        let farthest = s.build_syllable_graph("UZRF", &prism, &mut g);
        assert_eq!(4, farthest);
        let expected = prism.get_value("ZFUR").expect("ZFUR");
        assert!(g.edges[&0].contains_key(&4));
        assert!(g.edges[&0][&4].contains_key(&expected));
    }

    #[test]
    fn canonicalize_multi_syllable_streaming() {
        // syllabifier_test.cc:301-342
        let (prism, _) = canonicalize_fixture();
        let mut s = Syllabifier::with_options("'", false, false, Some(canonicalizer()));
        let id_zfuro = prism.get_value("ZFURO").expect("ZFURO");
        let id_sa = prism.get_value("SA").expect("SA");

        // 情況 A: 帶隔音符號
        let mut g = SyllableGraph::default();
        let farthest = s.build_syllable_graph("FZURO'SA", &prism, &mut g);
        assert_eq!(8, farthest);
        assert!(g.edges[&0].contains_key(&6));
        assert!(g.edges[&0][&6].contains_key(&id_zfuro));
        assert!(g.edges.contains_key(&6));
        assert!(g.edges[&6].contains_key(&8));
        assert!(g.edges[&6][&8].contains_key(&id_sa));

        // 情況 B: 無隔音符號
        let mut g = SyllableGraph::default();
        let farthest = s.build_syllable_graph("FZUROSA", &prism, &mut g);
        assert_eq!(7, farthest);
        assert!(g.edges[&0].contains_key(&5));
        assert!(g.edges[&0][&5].contains_key(&id_zfuro));
        assert!(g.edges.contains_key(&5));
        assert!(g.edges[&5].contains_key(&7));
        assert!(g.edges[&5][&7].contains_key(&id_sa));
    }

    #[test]
    fn canonicalize_null_canonicalizer_fallback() {
        // syllabifier_test.cc:345-359
        let (prism, _) = canonicalize_fixture();
        let mut s = Syllabifier::with_options("'", false, false, None);
        let mut g = SyllableGraph::default();
        assert_eq!(5, s.build_syllable_graph("ZFURO", &prism, &mut g));
        let mut g2 = SyllableGraph::default();
        assert_ne!(5, s.build_syllable_graph("FZURO", &prism, &mut g2));
    }

    // ---- 额外的语义钉子 ----

    #[test]
    fn empty_input_yields_empty_graph() {
        let (prism, _) = fixture();
        let mut s = Syllabifier::new();
        let mut g = SyllableGraph::default();
        assert_eq!(0, s.build_syllable_graph("", &prism, &mut g));
        assert!(g.vertices.is_empty());
        assert!(g.edges.is_empty());
    }

    #[test]
    fn completion_adds_edge_to_input_length() {
        // `enable_completion` 分支（syllabifier.cc:224-265）
        let (prism, _) = fixture();
        let mut s = Syllabifier::with_options("", true, false, None);
        let mut g = SyllableGraph::default();
        // "changanch"：前 7 字节切完（chan+gan），剩 "ch" 是 cha/chan/chang 的前缀
        let farthest = s.build_syllable_graph("changanch", &prism, &mut g);
        assert_eq!("changanch".len() as i32, farthest);
        let spellings = edge(&g, 7, "changanch".len());
        assert!(spellings.values().all(|p| p.r#type == SpellingType::Completion));
        assert!(spellings.values().all(|p| (p.credibility - K_COMPLETION_PENALTY).abs() < 1e-12));
    }

    #[test]
    fn strict_spelling_rejects_abbrev_as_whole_input() {
        // `strict_spelling`：整串匹配时不允许简拼（syllabifier.cc:130-132）
        let syllabary: BTreeSet<String> = ["chang", "sha"].iter().map(|s| s.to_string()).collect();
        let prism = InMemoryPrism::from_syllabary(syllabary.iter());
        // 造一条 "ch" -> chang 的简拼拼写
        let mut script = Script::new();
        script.add_syllable("chang");
        script.add_syllable("sha");
        let mut p = Projection::from_rules(&["abbrev/^([zcs]h).+$/$1/".to_string()]).unwrap();
        assert!(p.apply_script(&mut script));
        let prism_script = InMemoryPrism::from_script(&syllabary, &script);
        let _ = prism; // 上面那个只用来看 max_key_length 的类型，这里用带 script 的
        let mut strict = Syllabifier::with_options("", false, true, None);
        let mut g = SyllableGraph::default();
        strict.build_syllable_graph("ch", &prism_script, &mut g);
        assert!(
            g.edges.values().all(|e| e.is_empty()),
            "strict_spelling 下 'ch' 不该切出简拼音节: {:?}",
            g.edges
        );
        // 关掉 strict_spelling 就能切出来
        let mut loose = Syllabifier::with_options("", false, false, None);
        let mut g2 = SyllableGraph::default();
        loose.build_syllable_graph("ch", &prism_script, &mut g2);
        assert!(!g2.edges.is_empty());
    }

    #[test]
    fn correction_marks_credibility() {
        // `EnableCorrection` 的调用点（syllabifier.cc:137-141）：
        // 命中纠错（不在 exact_match_syllables 里）的拼写被标 is_correction + 罚分
        struct StubCorrector;
        impl Corrector for StubCorrector {
            fn tolerance_search(&self, prism: &dyn Prism, _input: &str, _limit: usize) -> Vec<(SyllableId, String)> {
                // 假装纠错到 "an"
                prism.get_value("an").map(|id| vec![(id, "an".to_string())]).unwrap_or_default()
            }
        }
        let (prism, _) = fixture();
        let mut s = Syllabifier::new();
        s.enable_correction(Box::new(StubCorrector));
        let mut g = SyllableGraph::default();
        // "tu" 是精确命中；补进来的 "an" 是纠错
        s.build_syllable_graph("tu", &prism, &mut g);
        let spellings = &g.edges[&0][&2];
        let an = spellings.values().find(|p| p.is_correction);
        assert!(an.is_some(), "纠错进来的音节要标 is_correction: {spellings:?}");
        let an = an.unwrap();
        assert_eq!(an.credibility, K_CORRECTION_CREDIBILITY);
    }

    #[test]
    fn last_type_floor_prunes_abbreviation_edges() {
        // ⚠️ `last_type = max(vertices[farthest], kFuzzySpelling)`（syllabifier.cc:178-179）
        // 的下限是 Fuzzy(1)，而 `last_type` 在循环里**不再更新**。所以当最远点是 Normal(0) 时，
        // 类型 > Fuzzy(1) 的（简拼/补全/歧义）顶点与边在这一步被全部剪掉。
        // "changan" 走 chan+gan 到达 7（Normal），而顶点 2 是简拼 "ch" → Abbreviation(2)，
        // 正好落在这条规则里被剪。
        let syllabary: BTreeSet<String> = fixture_syllables().into_iter().collect();
        let mut script = Script::new();
        for syllable in &syllabary {
            script.add_syllable(syllable);
        }
        let mut p = Projection::from_rules(&["abbrev/^([zcs]h).+$/$1/".to_string()]).unwrap();
        assert!(p.apply_script(&mut script));
        // 简拼 "ch" 确实存在于拼写表里（否则这个测试什么都没测）
        assert!(script.contains_key("ch"));
        let prism = InMemoryPrism::from_script(&syllabary, &script);
        let mut s = Syllabifier::new();
        let mut g = SyllableGraph::default();
        let farthest = s.build_syllable_graph("changan", &prism, &mut g);
        assert_eq!(7, farthest);
        assert_eq!(SpellingType::Normal, g.vertices[&7]);
        assert!(!g.vertices.contains_key(&2), "Abbreviation 顶点会被剪掉");
        assert!(
            g.edges.get(&0).map_or(true, |e| !e.contains_key(&2)),
            "指向简拼顶点的边也要一并消失"
        );
        // 长音节边（chan+gan 那条路）仍然在
        assert!(g.edges[&0].contains_key(&4));
    }

    #[test]
    fn transpose_prefers_longer_syllables() {
        // `Transpose` 逆序填 indices（syllabifier.cc:314）：同一个音节出现在多个终点时，
        // **长的先入列**。造一个 "a" 与 "aa" 都表示音节 "a" 的拼写表来验证。
        let syllabary: BTreeSet<String> = ["a"].iter().map(|s| s.to_string()).collect();
        let mut script = Script::new();
        script.add_syllable("a");
        let mut p = Projection::from_rules(&["derive/^a$/aa/".to_string()]).unwrap();
        assert!(p.apply_script(&mut script));
        assert!(script.contains_key("aa"));
        let prism = InMemoryPrism::from_script(&syllabary, &script);
        let mut s = Syllabifier::new();
        let mut g = SyllableGraph::default();
        assert_eq!(2, s.build_syllable_graph("aa", &prism, &mut g));
        let index = &g.indices[&0];
        assert_eq!(1, index.len(), "两种拼法都指向音节 a");
        let props = &index[&0];
        assert_eq!(2, props.len());
        assert_eq!(props[0].end_pos, 2, "长的先入列");
        assert_eq!(props[1].end_pos, 1);
    }

    #[test]
    fn spelling_of_helpers() {
        let mut script = Script::new();
        script.add_syllable("aa");
        let syllables = vec!["aa".to_string()];
        assert_eq!(spelling_of(&script, 0, &syllables).as_deref(), Some("aa"));
        assert_eq!(spelling_of(&script, 9, &syllables), None);
    }

    #[test]
    fn unused_calculus_import_is_used() {
        // 保证 `Calculus` 的导入有实际用途（同时验证大小写不敏感的重排规则）
        let calc = Calculus::new();
        let mut c = calc.parse("reorder ZFURO").unwrap();
        let mut sp = Spelling::new("zzz");
        assert!(!c.apply(&mut sp));
        let mut sp = Spelling::new("OFRUZ");
        assert!(c.apply(&mut sp));
        assert_eq!(sp.str, "ZFURO");
    }
}
