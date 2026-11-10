//! `src/rime/dictionary.{h,cc}`：`Dictionary` 门面（装载 `Table + Prism + ReverseDb`，提供翻译）。
//!
//! MVP 做**词级拼音/形码翻译**：把输入按音节图切分（借助 `Prism::common_prefix_search`），
//! 对每个前缀切分 `Table::query_phrases(Code)` 取词条，候选 `end` = 该切分覆盖到的字节位置。
//! 这正好让「你」(单音节切分) 与「你好」(双音节切分) 同台出现，逐音节确认行为自然成立。

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::algo::syllabifier::SyllableGraph;
use crate::dict::dict_entry_iterator::{DictEntryIterator, add_chunks_for_keys};
use crate::dict::prism::Prism;
use crate::dict::reverse_lookup_dictionary::ReverseDb;
use crate::dict::table::{SyllableIdMap, Table};
use crate::dict::vocabulary::DictEntry as VocabularyEntry;
use crate::dict::vocabulary::{Code, SyllableId};

/// 图查询结果（C++ `an<DictEntryCollector>` = `map<int, DictEntryList>`）：
/// `end_pos` → 词条列表，桶内按 `weight` 降序（C++ `DictEntryList::Sort`）。
pub type DictEntryCollector = BTreeMap<usize, Vec<VocabularyEntry>>;

/// 一条查到的词条（翻译器再包成 `engine::Candidate`）。
#[derive(Clone, Debug)]
pub struct DictEntry {
    pub text: String,
    /// 原始权重（来自 `.table.bin`）。
    pub weight: f64,
    /// 排序质量分（音节数 × 1000 + 权重）。
    pub quality: f64,
    /// 覆盖到的输入字节位置（选择此候选将消费到此处）。
    pub end: usize,
    pub comment: String,
    /// 该词条对应的音节码（`Code` = `Vec<SyllableId>`）。
    ///
    /// 学词要用：`UserDictionary::UpdateEntry` 在 `custom_code` 为空时靠它
    /// 反查出 `"ni hao "` 这样的码串，拼成 userdb 的键。
    pub code: Code,
}

/// 词典（对照 C++ `Dictionary`）：持有 Prism（拼写→音节）+ Table（音节码→词条）。
///
/// `packs` 对应 C++ 的 `tables_[1..]`（`dictionary.cc:395-399` `loaded pack:`）：
/// `translator/packs` 列的附加表，**与主表同码不同权**。查询必须遍历所有表
/// （`dictionary.cc:279` / `:332` 的两处 `for (const auto& table : tables_)`）。
pub struct Dictionary {
    pub name: String,
    /// 当前生效的 prism（evo 的 `prism_`）。所有查询都只走它。
    pub prism: Prism,
    /// 当前 prism 的名字（evo 没存，`SetPrism` 要靠它把换下来的 prism 放回 `prisms`）。
    pub active_prism_name: String,
    /// 备用 prism（evo `prisms_`）：input type → prism。默认空 —— 单 prism 方案
    /// 与升级前逐位一致，`set_prism` 只有在 `add_prism` 过之后才可能切走。
    pub prisms: BTreeMap<String, Prism>,
    pub table: Table,
    /// 附加表。典型用法是「主表给码、分包给权」：巧码的 `qiaoma.dict.yaml`
    /// 只有 `columns: [text, code]`，权重全在 `qiaoma.base` / `.ext` / `.user` 里。
    pub packs: Vec<Table>,
    /// v6：每张表一份"全局音节 id → 本表局部 id"映射，顺序与 [`Self::tables`] 对齐
    /// （主表在前、pack 表在后）。`None` = 恒等（v5，或本表音节表恰与 union 同序同内容）。
    pub code_maps: Vec<Option<SyllableIdMap>>,
    pub reverse: Option<ReverseDb>,
}

/// 真实 build 用**系列文件夹**布局（见 `crate::lever::paths::series_subdir` 的恒等式）：
/// `.bin` 落在 `build/<系列(id)>/<id><ext>.bin`（如 `build/luna_pinyin/luna_pinyin.table.bin`）。
/// 测试 fixture 用**扁平** `build/<id><ext>.bin`。两者都试 —— 系列优先、扁平回退，
/// 保证两种布局都能加载（这正是真机部署下 translator 之前全部加载失败的根因）。
///
/// `ext` 含前导点，例如 `".table"` → `<id>.table.bin`（不带点会拼成 `<id>table.bin`）。
pub(crate) fn bin_resource_path(build_dir: &Path, id: &str, ext: &str) -> PathBuf {
    let file = format!("{id}{ext}.bin");
    let series = build_dir.join(id.replace('.', "/")).join(&file);
    if series.is_file() {
        return series;
    }
    build_dir.join(&file)
}

/// 词图里每个 `[start, end)` 桶最多留几条同码词（[`Dictionary::lookup_from_graph`]）。
///
/// 只给**束搜索**用：词级候选流要按页展示全部同码词，不设上限。
///
/// # 为什么束搜索需要这个上限
///
/// 2026-10-30 实测（自然码双拼，`tianqibucuo` 11 键，release，语法模型开着）：
///
/// | 上限 | 最坏键耗时 | 平均 | 与"不限"的输出差异 |
/// |---|---|---|---|
/// | 不限 | 43.75 ms | 1.58 ms | —（每桶 130~190 条，全图 1928~5312 条） |
/// | 4    |  4.74 ms | 0.27 ms | 87 项里 1 项不同 |
/// | 8    |  6.80 ms | 0.32 ms | 87 项里 1 项不同 |
/// | **32** | **10.90 ms** | **0.50 ms** | **完全一致** |
///
/// 不限时 [`Poet`] 的束搜索每键要 `evaluate` 九千到三万三千次（`run_beam`
/// 对每个 `start_pos` 取 top-7，再 × 每个 `end_pos` × 桶里**每一条**）。
/// 而束搜索每步最终只留 top-7，桶里第 33 条以后根本不会被看到 ——
/// 取 32 是"输出与不限时逐字一致 + 快 4 倍"的稳妥点。
///
/// 该结论只在**单声调组**成立：全拼一个无调拼写会展开成多个带调音节 id
/// （`guo` → `guo`+`guo1..guo5`），它们共用一个 `end_pos` 桶，顶部 32 条
/// 会把 terra 独有、权重仅 1（对数权重 0）的生僻字整体挤掉。所以词级路径
/// 必须传 `None`。
///
/// [`Poet`]: crate::engine::gear::poet::Poet
pub const K_BUCKET_CAP: usize = 32;

/// `WEASEL_BUCKET_CAP` 的进程级覆盖（调参/排查用）；只读一次环境。
///
/// * `N > 0`：每桶最多留 N 条（对所有调用方生效）。
/// * `0` / `off` / `none`：**关掉截断**（回到补丁前的行为，用来做 A/B 对照）。
/// * 没设 / 解析失败：不覆盖，用调用方自己传的 `cap`。
fn env_bucket_cap() -> Option<Option<usize>> {
    static CAP: std::sync::OnceLock<Option<Option<usize>>> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| match std::env::var("WEASEL_BUCKET_CAP").ok().as_deref() {
        Some("0") | Some("off") | Some("none") => Some(None),
        Some(v) => v.parse::<usize>().ok().filter(|n| *n > 0).map(Some),
        None => None,
    })
}

/// v6：由 prism 的 union 音节表（全局 id）为**每张表**建一份局部映射。
///
/// v5 的 prism 没有音节表（`syllabary()` 为 `None`）→ 全部恒等（`None`），
/// 查询路径与升级前完全一致。表与 prism 音节表同序同内容（恒等映射）时也存 `None`，
/// 热路径只多一次 `Option::filter` + 长度判断。
fn build_code_maps(prism: &Prism, table: &Table, packs: &[Table]) -> Vec<Option<SyllableIdMap>> {
    let Some(global) = prism.syllabary() else {
        return vec![None; 1 + packs.len()];
    };
    std::iter::once(table)
        .chain(packs.iter())
        .map(|table| {
            table.syllabary_ordered().and_then(|local| {
                let map = SyllableIdMap::new(global, &local);
                if map.is_empty() {
                    None
                } else {
                    Some(map)
                }
            })
        })
        .collect()
}

/// v6 陈旧校验（`dictionary.cc:711-746`）：prism 的 union 音节表是 canonical，
/// 每张表自己的局部音节必须都能在 canonical 里找到，**且**各表局部音节表的并集
/// 必须与 canonical 完全相等。
///
/// 任一条件失败都说明 prism 与 `.table.bin` 不是同一次编译的产物（部署中断、
/// 手工替换、或 `translator/packs` 改过但 prism 没重编）。此时音节 id 已经对不上，
/// 继续查询会静默返回错词，所以宁可让词典加载失败。
fn code_sets_are_consistent(prism: &Prism, tables: &[&Table]) -> bool {
    let Some(canonical) = prism.syllabary() else {
        // v5 没有外置音节表：表与 prism 共享同一套 id，无需校验。
        return true;
    };
    let canonical: HashSet<&str> = canonical.iter().map(String::as_str).collect();
    let mut union: HashSet<String> = HashSet::new();
    for table in tables {
        let Some(local) = table.syllabary_ordered() else {
            return false;
        };
        for syllable in &local {
            if !canonical.contains(syllable.as_str()) {
                return false;
            }
            union.insert(syllable.clone());
        }
    }
    union.len() == canonical.len()
}

impl Dictionary {
    /// 在 rime 的 `build/` 下打开词典：
    /// `<dict_name>.table.bin` / `<dict_name>.reverse.bin` + `<prism_name>.prism.bin`
    /// （拼音方案的 table 与 prism 往往不同名，如 `luna_pinyin` vs `luna_pinyin_simp`）。
    pub fn open(build_dir: &Path, dict_name: &str, prism_name: &str) -> Option<Dictionary> {
        Self::open_with_packs(build_dir, dict_name, prism_name, &[])
    }

    /// 带 `translator/packs` 的打开方式（C++ `Dictionary::Load` 的 `tables_[1..]`）。
    ///
    /// 阶段 1：packs 在部署期合成一张 `<dict>.packs.table.bin`，加载侧**优先读它**；
    /// 合并表不在（升级前留下的一包一表）才回退逐个 `<pack>.table.bin`。
    ///
    /// v5：分包**缺了不算失败**（C++ `tables_[1..]` 缺了静默跳过），不影响主表可用。
    /// v6（prism `has_packs`）：合并表是必需产物，缺失或码集与 prism 不一致都返回
    /// `None` —— 此时 id 已对不上，继续用会静默出错词（`dictionary.cc:698-746`）。
    pub fn open_with_packs(
        build_dir: &Path,
        dict_name: &str,
        prism_name: &str,
        packs: &[String],
    ) -> Option<Dictionary> {
        let t_table = std::time::Instant::now();
        let table = Table::open(bin_resource_path(build_dir, dict_name, ".table")).ok()?;
        crate::engine::switch_trace::done(&format!("dict:{dict_name}.table.bin"), Some(t_table));
        let t_prism = std::time::Instant::now();
        let prism = Prism::open(bin_resource_path(build_dir, prism_name, ".prism")).ok()?;
        crate::engine::switch_trace::done(&format!("dict:{prism_name}.prism.bin"), Some(t_prism));
        let reverse = ReverseDb::open(bin_resource_path(build_dir, dict_name, ".reverse")).ok();
        // v6：prism 自己声明带 packs 时，哪怕配置里没列 `translator/packs`，也必须读
        // `<dict>.packs.table.bin`（evo `dictionary.cc:691-702` 的同语义）。
        let has_packs = prism.has_packs();
        let mut pack_tables = Vec::with_capacity(packs.len());
        if has_packs || !packs.is_empty() {
            let merged_path = bin_resource_path(build_dir, dict_name, ".packs.table");
            if merged_path.is_file() {
                match Table::open(&merged_path) {
                    Ok(merged) => pack_tables.push(merged),
                    Err(e) => eprintln!(
                        "[dictionary] failed to load merged pack table '{}': {e}",
                        merged_path.display()
                    ),
                }
            }
            // 合并表缺失/损坏 → 旧布局回退（一包一表）。只适用于 v5：
            // v6 的 prism 与合并表是同一次编译的产物，回退到旧包表只会读到错 id。
            if pack_tables.is_empty() && !has_packs {
                for name in packs {
                    let path = bin_resource_path(build_dir, name, ".table");
                    if !path.is_file() {
                        continue;
                    }
                    match Table::open(&path) {
                        Ok(pack) => pack_tables.push(pack),
                        Err(e) => eprintln!("[dictionary] failed to load pack '{name}': {e}"),
                    }
                }
            }
            if pack_tables.is_empty() && has_packs {
                eprintln!(
                    "[dictionary] missing compiled supplement for dictionary '{dict_name}'"
                );
                return None;
            }
            // 防陈旧：码集必须与 prism 的 union 音节表完全一致。
            if has_packs {
                let mut tables: Vec<&Table> = Vec::with_capacity(1 + pack_tables.len());
                tables.push(&table);
                tables.extend(pack_tables.iter());
                if !code_sets_are_consistent(&prism, &tables) {
                    eprintln!(
                        "[dictionary] table and prism code sets differ for dictionary '{dict_name}'"
                    );
                    return None;
                }
            }
        }
        let t_bind = std::time::Instant::now();
        let mut code_maps = build_code_maps(&prism, &table, &pack_tables);
        // 映射条数必须与 `tables()` 对齐（构建失败的表按恒等处理）。
        code_maps.resize(1 + pack_tables.len(), None);
        crate::engine::switch_trace::done(
            &format!("dict:{dict_name} 重绑码表({} 张表)", 1 + pack_tables.len()),
            Some(t_bind),
        );
        Some(Dictionary {
            name: dict_name.to_string(),
            prism,
            active_prism_name: prism_name.to_string(),
            prisms: BTreeMap::new(),
            table,
            packs: pack_tables,
            code_maps,
            reverse,
        })
    }

    /// 当前生效 prism 的名字。
    pub fn prism_name(&self) -> &str {
        &self.active_prism_name
    }

    /// 是否已注册（或正在使用）某个 prism 名字。
    pub fn has_prism(&self, name: &str) -> bool {
        name == self.active_prism_name || self.prisms.contains_key(name)
    }

    /// `Dictionary::AddPrism(name, prism)`（`dictionary.cc:462-467`）：
    /// 注册一个可切换的 prism。**不改变当前生效 prism**（evo 同语义）。
    ///
    /// 同名已注册会替换；同名恰好是当前生效 prism 时直接替换生效对象并重绑。
    /// 返回 `false` 只表示名字为空或新 prism 的码集与表对不上。
    pub fn add_prism(&mut self, name: impl Into<String>, prism: Prism) -> bool {
        let name = name.into();
        if name.is_empty() {
            return false;
        }
        if name == self.active_prism_name {
            self.prism = prism;
            return self.rebind_prism_bindings();
        }
        self.prisms.insert(name, prism);
        true
    }

    /// `Dictionary::SetPrism(name)`（`dictionary.cc:469-477`）：切换生效 prism。
    ///
    /// 切换后按新 prism 的音节表重算 `code_maps`（`bindings_valid_ = false` 的等价物），
    /// 并做与加载期相同的码集一致性校验。找不到名字或校验失败都返回 `false`，
    /// 且**保持原来的 prism 与映射不变**。
    pub fn set_prism(&mut self, name: &str) -> bool {
        if name == self.active_prism_name {
            return true;
        }
        let Some(next) = self.prisms.remove(name) else {
            return false;
        };
        let previous_name = std::mem::replace(&mut self.active_prism_name, name.to_string());
        let previous = std::mem::replace(&mut self.prism, next);
        if !self.rebind_prism_bindings() {
            // 回滚：把试切上去的 prism 放回 map，恢复原 prism。
            let attempted = std::mem::replace(&mut self.prism, previous);
            self.prisms.insert(name.to_string(), attempted);
            self.active_prism_name = previous_name;
            self.rebind_prism_bindings();
            return false;
        }
        self.prisms.insert(previous_name, previous);
        true
    }

    /// 按当前 `prism` 重算各表的音节 id 映射（v6 的 `code_maps`）。
    ///
    /// prism 变了，全局 id 的含义就变了 —— 即使两张表的内容没动，映射也必须重算。
    /// 码集不一致时返回 `false` 且不落新映射（调用方负责回滚）。
    fn rebind_prism_bindings(&mut self) -> bool {
        if self.prism.has_packs() {
            let mut tables: Vec<&Table> = Vec::with_capacity(1 + self.packs.len());
            tables.push(&self.table);
            tables.extend(self.packs.iter());
            if !code_sets_are_consistent(&self.prism, &tables) {
                return false;
            }
        }
        let mut maps = build_code_maps(&self.prism, &self.table, &self.packs);
        maps.resize(1 + self.packs.len(), None);
        self.code_maps = maps;
        true
    }

    /// 主表 + 全部分包，查询要遍历它们（C++ `for (const auto& table : tables_)`）。
    pub fn tables(&self) -> impl Iterator<Item = &Table> {
        std::iter::once(&self.table).chain(self.packs.iter())
    }

    /// 像 [`Self::tables`] 一样遍历，但每张表带上自己的 v6 id 映射（查表时用）。
    pub fn tables_with_maps(&self) -> impl Iterator<Item = (&Table, Option<&SyllableIdMap>)> {
        std::iter::once((&self.table, self.code_maps.first().and_then(Option::as_ref))).chain(
            self.packs
                .iter()
                .enumerate()
                .map(|(index, table)| {
                    (
                        table,
                        self.code_maps.get(index + 1).and_then(Option::as_ref),
                    )
                }),
        )
    }

    /// `Table::GetSyllableById(id)` 的 v6 版本：音节 id 是 prism 的**全局** id，
    /// 所以优先读 prism 的音节表（v5 没有，回退主表）。
    pub fn syllable_by_id(&self, syllable_id: SyllableId) -> String {
        self.prism
            .syllable_by_id(syllable_id)
            .unwrap_or_else(|| self.table.get_syllable_by_id(syllable_id))
    }

    /// 翻译整段输入，返回所有候选词条（含各自覆盖的 `end`）。
    pub fn translate(&self, input: &str) -> Vec<DictEntry> {
        let n = input.len();
        if n == 0 {
            return Vec::new();
        }
        // 每个起始位置能起的合法音节（spelling → syllable id + 跨度）。
        // 注意：拼音类拼写 prism 的 `common_prefix_search` 返回的是**拼写 id**，
        // 必须用拼写表转成音节 id（`Table` 词条编码用的是音节 id）。
        // 一个拼写可以对应**多个**音节 id（无调 `guo` → `guo`+`guo1..guo5`），
        // 必须全部展开：只取第 0 个会让本入口与真实查询路径（`Syllabifier`）
        // 结论不一致 —— 诊断/测试里会误报「查不到带调条目」。
        let mut match_at: Vec<Vec<(SyllableId, usize)>> = vec![Vec::new(); n + 1];
        for start in 0..n {
            let suffix = &input[start..];
            for m in self.prism.common_prefix_search(suffix) {
                // 没有拼写表（老格式 prism）时 accessor 取不到描述符，退回
                // 「拼写 id 即音节 id」的退化语义（与 C++ `spelling_syllable_id` 一致）。
                let mut found = 0usize;
                let mut spelling = self.prism.query_spelling(m.value);
                while !spelling.exhausted() {
                    if let Some(descriptor) = spelling.descriptor() {
                        match_at[start].push((descriptor.syllable_id, start + m.length));
                        found += 1;
                    }
                    spelling.next();
                }
                if found == 0 {
                    let syllable_id = self.prism.spelling_syllable_id(m.value);
                    match_at[start].push((syllable_id, start + m.length));
                }
            }
        }

        let mut entries: Vec<DictEntry> = Vec::new();
        let mut acc: Vec<SyllableId> = Vec::new();
        // 枚举从 0 出发的所有前缀切分；每段前缀都查一次表，得到覆盖到 `start` 的词条
        fn dfs(
            start: usize,
            n: usize,
            acc: &mut Vec<SyllableId>,
            match_at: &[Vec<(SyllableId, usize)>],
            tables: &[(&Table, Option<&SyllableIdMap>)],
            out: &mut Vec<DictEntry>,
        ) {
            if acc.len() > 12 {
                return; // 防极端爆搜
            }
            if start > 0 && !acc.is_empty() {
                let code = Code::from_slice(&acc[..]);
                let syllables = acc.len() as f64;
                // 主表 + 分包：同码不同权，都要查（C++ `tables_` 全遍历）。
                // v6：`code` 里是 prism 的全局 id，各表用自己的映射换成本表局部 id。
                for (table, map) in tables {
                    let mut accessor = table.query_phrases_with_map(&code, *map);
                    while !accessor.exhausted() {
                        if let Some(entry) = accessor.entry() {
                            let text = table.entry_text(&entry);
                            if !text.is_empty() {
                                let weight = entry.weight as f64;
                                // 质量尺度:码长(音节数)是主台阶——C++ 的
                                // DictEntryCollector 按 code_length 分桶、rbegin()
                                // 最长码先消费(`script_translator.cc:479`);同码长内
                                // 才比 weight(对数尺度)。1e12 台阶远超任何 weight 摆动。
                                out.push(DictEntry {
                                    text,
                                    weight,
                                    quality: syllables * 1.0e12 + weight,
                                    end: start,
                                    comment: String::new(),
                                    code: code.clone(),
                                });
                            }
                        }
                        accessor.next();
                    }
                }
            }
            if start >= n {
                return;
            }
            for &(id, end) in &match_at[start] {
                acc.push(id);
                dfs(end, n, acc, match_at, tables, out);
                acc.pop();
            }
        }
        let tables: Vec<(&Table, Option<&SyllableIdMap>)> = self.tables_with_maps().collect();
        dfs(0, n, &mut acc, &match_at, &tables, &mut entries);
        entries
    }

    /// `Dictionary::Lookup(syllable_graph, start_pos, blacklist, predict_word)`
    /// （`dictionary.cc:271-297` + `lookup_table` `:238-268`）：**按音节图**查表。
    ///
    /// 与 [`Self::translate`] 的区别：`translate` 是自己枚举前缀切分、只给词级候选；
    /// 本方法走的是 C++ 的正式路径 —— 音节图（[`SyllableGraph`]）由 `Syllabifier`
    /// 构建，`Table::query` 在图上游走，返回值按 `end_pos` 分桶，供组句器
    /// （[`crate::engine::gear::poet`]）拼整句。
    ///
    /// # 权重换算（`dictionary.cc:167`）
    ///
    /// C++ `DictEntryIterator::Peek()` 把 table 里的整数 weight 换算成对数概率：
    ///
    /// ```text
    /// weight = e.weight - log(1e8) + credibility
    ///        = e.weight - 18.420680743952367 + credibility
    /// ```
    ///
    /// 这个偏移**必须照抄**：grammar 的打分是同一对数尺度上的加法，漏掉就整体错位。
    ///
    /// # 已知简化
    ///
    /// `predict_word` 仅用于 `match_extra_code` 在输入耗尽时截断（`dictionary.cc:104`），
    /// LRN 的 `enable_completion` 尾补全已由 `Table::query` 的 tail 层承担，
    /// 这里保留同名参数以求语义对齐，但默认传 `false`。
    ///
    /// 每个 `end_pos` 桶的**分类硬预算**（C++ `script_translator.cc:695` 的
    /// `max_homophones` 同义）：按权重降序只留前 `cap` 条同音词，超出即截断。
    /// `cap = None` 表示不限（词级候选流要按页翻到全部同码词 / 生僻字）。
    ///
    /// 单独抽成纯函数有两个用处：①采集阶段每桶满了就地截断（见
    /// [`Self::lookup_from_graph`]），②单测无需真实词库就能钉死「截断保留
    /// 权重最高的 N 条、且 `None` 不截断」的行为。`sort_by` 是稳定排序，
    /// 权重并列时保持采集顺序（对应 C++ `DictEntryList::Sort` 的语义）。
    /// **只在超限时排序**：未超 cap 的热路径零排序开销，由函数末尾的
    /// 统一 `Sort()` 收口（与改造前逐字节等价）。
    fn cap_bucket_by_weight(bucket: &mut Vec<VocabularyEntry>, cap: Option<usize>) {
        if let Some(cap) = cap
            && bucket.len() > cap
        {
            bucket.sort_by(|a, b| a.cmp_weight_desc(b));
            bucket.truncate(cap);
        }
    }

    /// `cap` 是每个 `end_pos` 桶的条数上限：**束搜索**传 `Some(K_BUCKET_CAP)`，
    /// **词级候选流**传 `None`（要按页展示全部同码词）。`WEASEL_BUCKET_CAP`
    /// 仍可整体覆盖（`0` = 不限）；未设置时用调用方传入的值。
    pub fn lookup_from_graph(
        &self,
        graph: &SyllableGraph,
        start_pos: usize,
        blacklist: Option<&HashSet<String>>,
        predict_word: bool,
        cap: Option<usize>,
    ) -> DictEntryCollector {
        /// `log(1e8)`（`dictionary.cc:166`）：table weight → 对数概率的偏移量。
        const K_WEIGHT_OFFSET: f64 = 18.420_680_743_952_367;

        let cap_opt = env_bucket_cap().unwrap_or(cap);
        let mut collector = DictEntryCollector::new();
        // 主表 + 分包都要走（C++ `for (const auto& table : tables_)`）。
        // v6：音节图里是 prism 的全局 id，各表用自己的映射换成本表局部 id。
        for (table, map) in self.tables_with_maps() {
            for (end_pos, pages) in table.query_with_map(graph, start_pos, map) {
                for page in pages {
                    let mut accessor = page;
                    let index_code = accessor.index_code().clone();
                    let credibility = accessor.credibility();
                    let quality_len = accessor.quality_len();
                    while !accessor.exhausted() {
                        let Some(entry) = accessor.entry() else {
                            break;
                        };
                        // 桶已满且这条进不了前 N：直接跳过，**别去取文本** ——
                        // `entry_text` 要解字符串再分配一份 String，是采集阶段的大头。
                        // 权重是取文本前就能拿到的，所以这个判断是安全的。
                        if let Some(cap) = cap_opt {
                            if collector.get(&end_pos).is_some_and(|b| b.len() >= cap) {
                                let w = entry.weight as f64 - K_WEIGHT_OFFSET + credibility;
                                let worst = collector[&end_pos]
                                    .iter()
                                    .map(|e| e.weight)
                                    .fold(f64::INFINITY, f64::min);
                                if w <= worst {
                                    accessor.next();
                                    continue;
                                }
                            }
                        }
                        let text = table.entry_text(&entry);
                        let excluded = blacklist.is_some_and(|b| b.contains(&text));
                        if text.is_empty() || excluded {
                            accessor.next();
                            continue;
                        }
                        // `dictionary.cc:255-262`：长词（tail 层）带着 `extra_code`，
                        // 要在图上再走一遍才能知道它真正覆盖到哪里。
                        let target_end = match accessor.extra_code() {
                            None => Some(end_pos),
                            Some(extra) => {
                                let m = match_extra_code(&extra, 0, graph, end_pos, predict_word);
                                m.success.then_some(m.end_pos)
                            }
                        };
                        if let Some(target_end) = target_end {
                            let bucket = collector.entry(target_end).or_default();
                            bucket.push(VocabularyEntry {
                                text,
                                weight: entry.weight as f64 - K_WEIGHT_OFFSET + credibility,
                                code: index_code.clone(),
                                quality_len,
                                ..Default::default()
                            });
                            Self::cap_bucket_by_weight(bucket, cap_opt);
                        }
                        accessor.next();
                    }
                }
            }
        }
        // C++ `Dictionary::Lookup` 末尾对每个桶 `Sort()`（`dictionary.cc:288-296`）。
        for bucket in collector.values_mut() {
            bucket.sort_by(|a, b| a.cmp_weight_desc(b));
        }
        collector.retain(|_, bucket| !bucket.is_empty());
        collector
    }

    /// `Dictionary::LookupWords(result, str_code, predictive, limit, blacklist)`
    /// （`dictionary.cc:299-348`）：按**码串**取词条（不走音节图）。
    ///
    /// 这是形码方案的主查询路径 —— 用户的按键码就是 prism 的键，不存在"一种输入多种切分"，
    /// 所以要的是"整码命中 + 前缀补全"，不是音节图上的路径枚举。
    ///
    /// # 三路来源
    ///
    /// | `predictive` | 键怎么来 | 页怎么来 |
    /// |---|---|---|
    /// | `false` | `prism.get_value(str_code)`（整码，一个键） | `table.query_words(音节 id)` |
    /// | `true` | `prism.expand_search(str_code, limit)`（BFS，前缀族） | 同上 |
    ///
    /// 每个键经 `query_spelling` 展开成若干 (音节 id, 拼写类型)，**只留
    /// `kNormalSpelling`**（`dictionary.cc:324`：`if (type > kNormalSpelling) continue;`
    /// —— 模糊音/简拼/补全在这一层就被丢掉），每个音节取一页同码词。
    ///
    /// # `remaining_code`（`dictionary.cc:326-331`）
    ///
    /// 只有补全时"键比用户打的码还长"才发生：`expand_search` 会往下 BFS，
    /// 打出 `nih` 时键 `nihao` 也算命中，此时提示 `~ao`。判据是
    /// `match.length > code_length` 且 `音节.len() > code_length`。
    ///
    /// # 返回值
    ///
    /// C++ 返回**命中键的个数**（不是词条数），`LazyTableTranslation` 用它判断
    /// "是不是还有更多页"。词条本身装在 `result` 里。
    pub fn lookup_words(
        &self,
        result: &mut DictEntryIterator,
        str_code: &str,
        predictive: bool,
        limit: usize,
        blacklist: Option<&HashSet<String>>,
    ) -> usize {
        let keys = if predictive {
            self.prism.expand_search(str_code, limit)
        } else {
            // C++：`Prism::Match match{0, 0}` —— **length 留 0**，不是键长。
            // 留下 0 才能让下面 `match.length > code_length` 恒假（整码不该有 ~ 提示）。
            match self.prism.get_value(str_code) {
                Some(value) => vec![crate::dict::prism::Match { value, length: 0 }],
                None => Vec::new(),
            }
        };
        let tables: Vec<(&Table, Option<&SyllableIdMap>)> = self.tables_with_maps().collect();
        add_chunks_for_keys(result, &tables, &self.prism, &keys, str_code.len());
        if let Some(blacklist) = blacklist.filter(|blacklist| !blacklist.is_empty()) {
            // C++ 的 filter 闭包捕的是 `const hash_set*`（指针活到迭代器析构）；
            // Rust 侧 Box<dyn Fn> 要求 'static，只能把集合搬进去。黑名单通常只有几条，
            // 而且一个迭代器只挂一次，代价可以忽略。
            let blacklist = blacklist.clone();
            result.add_filter(Box::new(move |entry: &VocabularyEntry| {
                !blacklist.contains(&entry.text)
            }));
        }
        keys.len()
    }

    /// `Dictionary::Decode(const Code&, vector<string>* result)`（`dictionary.cc:351-363`）：
    /// 音节 id 序列 → 音节串序列。任一个 id 查不到就整条失败（`result` 仍被清空）。
    pub fn decode(&self, code: &Code) -> Option<Vec<String>> {
        let mut result = Vec::with_capacity(code.len());
        for &syllable_id in code.iter() {
            let syllable = self.syllable_by_id(syllable_id);
            if syllable.is_empty() {
                return None;
            }
            result.push(syllable);
        }
        Some(result)
    }
}

/// `dictionary::CodeMatch`（`dictionary.cc:78-82`）+ `match_extra_code`（`:84-112`）。
///
/// 长词词条只存了前 3 个音节的索引码，剩下的在 `extra_code` 里；要在音节图上
/// 逐个别对齐，才能算出这条词条覆盖到输入的哪个字节位置。
fn match_extra_code(
    extra_code: &[SyllableId],
    depth: usize,
    graph: &SyllableGraph,
    current_pos: usize,
    predict_word: bool,
) -> CodeMatch {
    if depth >= extra_code.len() {
        return CodeMatch {
            success: true,
            depth,
            end_pos: current_pos,
        };
    }
    if current_pos >= graph.interpreted_length {
        return if predict_word {
            CodeMatch {
                success: true,
                depth,
                end_pos: graph.interpreted_length,
            }
        } else {
            CodeMatch::FAILED
        };
    }
    let Some(index) = graph.indices().get(&current_pos) else {
        return CodeMatch::FAILED;
    };
    let Some(spellings) = index.get(&extra_code[depth]) else {
        return CodeMatch::FAILED;
    };
    let mut best = CodeMatch::FAILED;
    for props in spellings {
        let m = match_extra_code(extra_code, depth + 1, graph, props.end_pos(), predict_word);
        if !m.success {
            continue;
        }
        if m.end_pos > best.end_pos {
            best = m;
        }
    }
    best
}

/// `dictionary::CodeMatch`（`dictionary.cc:78-82`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CodeMatch {
    success: bool,
    #[allow(dead_code)]
    depth: usize,
    end_pos: usize,
}

impl CodeMatch {
    const FAILED: CodeMatch = CodeMatch {
        success: false,
        depth: 0,
        end_pos: 0,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::dict_compiler::DictCompiler;

    /// 造一条只带文本和权重的词条（分类硬预算单测用，不碰词库文件）。
    fn weighted(text: &str, weight: f64) -> VocabularyEntry {
        VocabularyEntry {
            text: text.to_string(),
            weight,
            ..Default::default()
        }
    }

    /// 2026-11-10 第五条（抄 librime `max_homophones` 分类硬预算）：每个
    /// `end_pos` 桶只留权重最高的 N 条同音词——本仓的 `K_BUCKET_CAP=32`
    /// 采集期截断已实现它（A/B 实测 43.75ms→10.90ms、输出逐字一致）。
    /// 这里钉死截断语义，防止将来重构把「留最优 N 条」写反成「留最差」。
    #[test]
    fn bucket_cap_keeps_the_highest_weighted_homophones() {
        let mut bucket: Vec<VocabularyEntry> = [1.0, 9.0, 3.0, 7.0, 5.0]
            .into_iter()
            .enumerate()
            .map(|(i, w)| weighted(&format!("词{i}"), w))
            .collect();
        Dictionary::cap_bucket_by_weight(&mut bucket, Some(3));
        assert_eq!(
            bucket.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            vec!["词1", "词3", "词4"],
            "9/7/5 是权重最高的三条，且按降序"
        );
    }

    /// 词级候选流 `cap=None`：必须一条不截（保留生僻字 / 全翻页），
    /// 对应 `lookup_from_graph` 的 `None` 路径。
    #[test]
    fn bucket_cap_none_keeps_every_entry() {
        let mut bucket: Vec<VocabularyEntry> = (0..K_BUCKET_CAP * 3)
            .map(|i| weighted(&format!("词{i}"), (i % 7) as f64))
            .collect();
        let n = bucket.len();
        Dictionary::cap_bucket_by_weight(&mut bucket, None);
        assert_eq!(bucket.len(), n, "cap=None 时不能丢任何词条");
        assert_eq!(bucket[0].text, "词0", "未超限时不重排（稳定）");
    }

    /// 未超 cap 时零排序：输入顺序原样保留（热路径不付排序成本）。
    #[test]
    fn bucket_cap_below_limit_does_not_reorder() {
        let mut bucket: Vec<VocabularyEntry> =
            vec![weighted("低", 1.0), weighted("高", 9.0), weighted("中", 5.0)];
        Dictionary::cap_bucket_by_weight(&mut bucket, Some(10));
        assert_eq!(
            bucket.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            vec!["低", "高", "中"],
            "未超限必须保持采集顺序，排序交给函数末尾统一收口"
        );
    }

    /// 权重并列时按插入序稳定出词（`sort_by` 的稳定性，对应 C++）。
    #[test]
    fn bucket_cap_break_ties_by_insertion_order() {
        let mut bucket: Vec<VocabularyEntry> = vec![
            weighted("甲", 5.0),
            weighted("乙", 5.0),
            weighted("丙", 9.0),
            weighted("丁", 5.0),
        ];
        Dictionary::cap_bucket_by_weight(&mut bucket, Some(2));
        assert_eq!(
            bucket.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            vec!["丙", "甲"],
            "9 分先出；三个 5 分并列时取最先采集的「甲」"
        );
    }

    /// 造一个「主表给码、分包给词」的两级目录，编译出主表 + 一张分包。
    ///
    /// 布局按系列恒等式：`schema/demo/demo.dict.yaml`、`schema/demo/pack/demo.pack.dict.yaml`，
    /// 产物落在 `staging/demo/demo.table.bin` 与 `staging/demo/pack/demo.pack.table.bin`。
    ///
    /// `merged_packs = true` 走阶段 1 的合并编译（`translator/packs` 一次进合并表，
    /// 旧的一包一表会被清掉）；`false` 保持旧布局（用于验证加载侧回退）。
    fn build_two_level(root: &Path, merged_packs: bool) -> PathBuf {
        let user = root.join("user");
        let shared = root.join("shared");
        let staging = user.join("build");
        for dir in [&user, &shared, &staging] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // 词库源在 `dict/<id 分段…>/`（`classified_dict_source`），
        // 方案源在 `schema/<id 分段…>/` —— 两者根目录不同，别搞混。
        let dict_dir = shared.join("dict/demo");
        std::fs::create_dir_all(dict_dir.join("pack")).unwrap();
        let schema = shared.join("schema/demo");
        std::fs::create_dir_all(&schema).unwrap();

        // 主表：只有码，没有权（巧码就是这样）
        std::fs::write(
            dict_dir.join("demo.dict.yaml"),
            "\
---
name: demo
version: \"1.0\"
sort: by_weight
columns:
  - text
  - code
...
甲\tab
乙\tcd
",
        )
        .unwrap();
        // 分包：同码表的另一批词，带权重
        std::fs::write(
            dict_dir.join("pack/demo.pack.dict.yaml"),
            "\
---
name: demo.pack
version: \"1.0\"
sort: by_weight
columns:
  - text
  - code
  - weight
...
丙\tab\t1000
",
        )
        .unwrap();
        let schema_src = schema.join("demo.schema.yaml");
        std::fs::write(
            &schema_src,
            "\
schema:
  schema_id: demo
translator:
  dictionary: demo
",
        )
        .unwrap();

        for (name, packs) in [("demo", Vec::new()), ("demo.pack", Vec::new())] {
            if merged_packs {
                break;
            }
            let mut compiler = DictCompiler::new(
                name,
                "",
                packs,
                &user,
                &shared,
                &staging,
                shared.join("prebuilt"),
            );
            compiler
                .compile(Some(&schema_src))
                .unwrap_or_else(|e| panic!("compile {name}: {e}"));
        }
        if merged_packs {
            let mut compiler = DictCompiler::new(
                "demo",
                "",
                vec!["demo.pack".to_string()],
                &user,
                &shared,
                &staging,
                shared.join("prebuilt"),
            );
            compiler
                .compile(Some(&schema_src))
                .unwrap_or_else(|e| panic!("compile merged packs: {e}"));
        }
        staging
    }

    fn texts(dict: &Dictionary, code: &str) -> Vec<String> {
        let mut iter = DictEntryIterator::new();
        dict.lookup_words(&mut iter, code, false, 0, None);
        iter.collect_entries().into_iter().map(|e| e.text).collect()
    }

    /// `translator/packs` 必须真的参与查询：主表只给出「甲」，分包补出「丙」。
    /// 以前 `Dictionary` 只开主表，分包编出来了却从不读 —— 巧码这类
    /// 「主表给码、分包给权」的方案会整份权重丢失（2026-11-03 实测）。
    #[test]
    fn packs_are_queried_alongside_primary_table() {
        let root = std::env::temp_dir().join("librime-dictionary-packs");
        let _ = std::fs::remove_dir_all(&root);
        let staging = build_two_level(&root, false);

        let without = Dictionary::open(&staging, "demo", "demo").expect("open 主表");
        assert_eq!(texts(&without, "ab"), vec!["甲".to_string()], "没分包时只有主表词条");

        let packs = vec!["demo.pack".to_string()];
        let with = Dictionary::open_with_packs(&staging, "demo", "demo", &packs).expect("open");
        assert_eq!(with.packs.len(), 1, "分包应当被打开");
        let mut got = texts(&with, "ab");
        got.sort();
        assert_eq!(got, vec!["丙".to_string(), "甲".to_string()], "主表 + 分包都要出词");

        // 分包缺失只跳过，不影响主表可用（C++ `table->Exists()` 同语义）
        let missing = Dictionary::open_with_packs(
            &staging,
            "demo",
            "demo",
            &["demo.nothere".to_string()],
        )
        .expect("缺分包不该让字典打不开");
        assert!(missing.packs.is_empty());
        assert_eq!(texts(&missing, "ab"), vec!["甲".to_string()]);
    }

    /// 阶段 1：部署期把所有 pack 合成一张 `<dict>.packs.table.bin`，加载侧优先读它；
    /// 多个 `translator/packs` 只对应一张附加表。
    #[test]
    fn merged_pack_table_is_loaded_as_single_pack() {
        let root = std::env::temp_dir().join("librime-dictionary-merged-packs");
        let _ = std::fs::remove_dir_all(&root);
        let staging = build_two_level(&root, true);

        let merged = staging.join("demo/demo.packs.table.bin");
        assert!(merged.is_file(), "合并表应存在: {}", merged.display());
        assert!(
            !staging.join("demo/pack/demo.pack.table.bin").is_file(),
            "旧的一包一表应被清掉"
        );

        let packs = vec!["demo.pack".to_string()];
        let dict = Dictionary::open_with_packs(&staging, "demo", "demo", &packs).expect("open");
        assert_eq!(dict.packs.len(), 1, "合并表作为单张附加表加载");
        let mut got = texts(&dict, "ab");
        got.sort();
        assert_eq!(got, vec!["丙".to_string(), "甲".to_string()], "主表 + 合并表都要出词");

        std::fs::remove_dir_all(&root).ok();
    }

    /// 阶段 2：pack 独有新音节（主表音节表没有）必须真的能打出来。
    ///
    /// 编译期：pack 表按自己的局部音节表编码；prism v6 存"主表 ∪ pack"的 union 音节表。
    /// 加载期：`SyllableIdMap` 把 prism 的全局 id 换成各表局部 id 再查索引。
    #[test]
    fn pack_only_syllable_is_typable() {
        let root = std::env::temp_dir().join("librime-dictionary-pack-syllable");
        let _ = std::fs::remove_dir_all(&root);
        let user = root.join("user");
        let shared = root.join("shared");
        let staging = user.join("build");
        for dir in [&user, &shared, &staging] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let dict_dir = shared.join("dict/demo");
        std::fs::create_dir_all(dict_dir.join("pack")).unwrap();
        std::fs::write(
            dict_dir.join("demo.dict.yaml"),
            "---\nname: demo\nversion: \"1.0\"\nsort: by_weight\ncolumns:\n  - text\n  - code\n...\n甲\tab\n乙\tcd\n",
        )
        .unwrap();
        // `zz` 只出现在 pack 里，主表音节表没有它。
        std::fs::write(
            dict_dir.join("pack/demo.pack.dict.yaml"),
            "---\nname: demo.pack\nversion: \"1.0\"\nsort: by_weight\ncolumns:\n  - text\n  - code\n  - weight\n...\n丙\tcd\t1000\n丁\tzz\t500\n",
        )
        .unwrap();
        let schema_dir = shared.join("schema/demo");
        std::fs::create_dir_all(&schema_dir).unwrap();
        let schema_src = schema_dir.join("demo.schema.yaml");
        std::fs::write(
            &schema_src,
            "schema:\n  schema_id: demo\ntranslator:\n  dictionary: demo\n",
        )
        .unwrap();

        let mut compiler = DictCompiler::new(
            "demo",
            "",
            vec!["demo.pack".to_string()],
            &user,
            &shared,
            &staging,
            shared.join("prebuilt"),
        );
        compiler.compile(Some(&schema_src)).expect("compile");

        let packs = vec!["demo.pack".to_string()];
        let dict = Dictionary::open_with_packs(&staging, "demo", "demo", &packs).expect("open");
        assert!(dict.prism.has_packs(), "含 packs 的方案应写 v6 prism");
        assert!(dict.prism.has_key("zz"), "union 音节表应包含 pack 独有新音节");

        // 主表音节表 = [ab, cd]，pack 表音节表 = [cd, zz]，union = [ab, cd, zz]。
        let global = dict
            .prism
            .syllabary()
            .and_then(|syllabary| syllabary.iter().position(|s| s == "zz"))
            .expect("zz 应在 prism 音节表里") as SyllableId;
        assert_eq!(dict.syllable_by_id(global), "zz", "id → 音节串走 prism");

        // 热路径核心：全局 id → pack 表局部 id，命中 pack 表里的「丁」。
        assert_eq!(dict.code_maps.len(), 2, "主表 + 一张 pack 表各一份映射");
        let (pack_table, map) = dict.tables_with_maps().nth(1).expect("pack 表");
        let map = map.expect("pack 表局部 id 与 union 不同，必须有映射");
        assert_ne!(global, map.to_local(global).expect("全局 id 应能映射到局部 id"));
        assert_eq!(map.to_global(map.to_local(global).unwrap()), Some(global));
        let mut accessor = pack_table.query_words_with_map(global, Some(map));
        assert!(!accessor.exhausted(), "映射后应命中 pack 表索引");
        assert_eq!(accessor.entry_text().as_deref(), Some("丁"));

        // 走完整查询路径（prism 拼写 → 全局 id → 各表映射）也能出词。
        assert_eq!(texts(&dict, "zz"), vec!["丁".to_string()], "pack 新音节应能打出「丁」");

        std::fs::remove_dir_all(&root).ok();
    }

    /// 阶段 2 防陈旧：v6 prism 声明带 packs 后，
    /// 1) 合并表缺失 → 拒绝加载（不能只当"没有分包"继续跑）；
    /// 2) 合并表的码集与 union 音节表不一致 → 拒绝加载（id 已错位）。
    #[test]
    fn stale_pack_table_is_rejected() {
        let root = std::env::temp_dir().join("librime-dictionary-stale-packs");
        let _ = std::fs::remove_dir_all(&root);
        let user = root.join("user");
        let shared = root.join("shared");
        let staging = user.join("build");
        for dir in [&user, &shared, &staging] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let dict_dir = shared.join("dict/demo");
        std::fs::create_dir_all(dict_dir.join("pack")).unwrap();
        std::fs::write(
            dict_dir.join("demo.dict.yaml"),
            "---\nname: demo\nversion: \"1.0\"\nsort: by_weight\ncolumns:\n  - text\n  - code\n...\n甲\tab\n",
        )
        .unwrap();
        std::fs::write(
            dict_dir.join("pack/demo.pack.dict.yaml"),
            "---\nname: demo.pack\nversion: \"1.0\"\nsort: by_weight\ncolumns:\n  - text\n  - code\n  - weight\n...\n丙\tab\t1000\n",
        )
        .unwrap();
        // 另一份独立词典：码 `zz` 不在 demo 的 union 音节表里（模拟陈旧/错位产物）。
        std::fs::write(
            dict_dir.join("demo.stale.dict.yaml"),
            "---\nname: demo.stale\nversion: \"1.0\"\nsort: by_weight\ncolumns:\n  - text\n  - code\n  - weight\n...\n戊\tzz\t100\n",
        )
        .unwrap();
        let schema_src = shared.join("schema/demo/demo.schema.yaml");
        std::fs::create_dir_all(schema_src.parent().unwrap()).unwrap();
        std::fs::write(
            &schema_src,
            "schema:\n  schema_id: demo\ntranslator:\n  dictionary: demo\n",
        )
        .unwrap();

        let mut compiler = DictCompiler::new(
            "demo",
            "",
            vec!["demo.pack".to_string()],
            &user,
            &shared,
            &staging,
            shared.join("prebuilt"),
        );
        compiler.compile(Some(&schema_src)).expect("compile");
        let packs = vec!["demo.pack".to_string()];
        assert!(
            Dictionary::open_with_packs(&staging, "demo", "demo", &packs).is_some(),
            "刚编出来的 v6 产物应能加载"
        );

        // 1) 合并表缺失：v6 必须拒绝（不是静默降级成"没有分包"）。
        let merged = staging.join("demo/demo.packs.table.bin");
        let backup = staging.join("demo/demo.packs.table.bin.bak");
        std::fs::rename(&merged, &backup).unwrap();
        assert!(
            Dictionary::open_with_packs(&staging, "demo", "demo", &packs).is_none(),
            "prism 声明 has_packs 时缺合并表应拒绝加载"
        );
        std::fs::rename(&backup, &merged).unwrap();

        // 2) 码集不一致：用 `zz` 的表覆盖合并表，union 里没有 `zz` → 拒绝。
        let mut stale = DictCompiler::new(
            "demo.stale",
            "",
            Vec::new(),
            &user,
            &shared,
            &staging,
            shared.join("prebuilt"),
        );
        stale.compile(Some(&schema_src)).expect("compile stale");
        let stale_table = staging.join("demo/stale/demo.stale.table.bin");
        assert!(stale_table.is_file(), "陈旧样本表应存在");
        std::fs::copy(&stale_table, &merged).unwrap();
        assert!(
            Dictionary::open_with_packs(&staging, "demo", "demo", &packs).is_none(),
            "码集与 prism 不一致应拒绝加载"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// 多 prism：同一份表 + 两个 prism（全拼 `zhongguo` / 双拼 `vsgo`）都能出同一个词；
    /// 默认单 prism 路径在整个用例前半段照常工作，切走/切回都不改变查询结果。
    #[test]
    fn dictionary_switches_prism_for_full_and_double_pinyin() {
        use crate::algo::algebra::{Projection, Script};

        let root = std::env::temp_dir().join("librime-dictionary-multi-prism");
        let _ = std::fs::remove_dir_all(&root);
        let user = root.join("user");
        let shared = root.join("shared");
        let staging = user.join("build");
        for dir in [&user, &shared, &staging] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let dict_dir = shared.join("dict/demo");
        std::fs::create_dir_all(&dict_dir).unwrap();
        std::fs::write(
            dict_dir.join("demo.dict.yaml"),
            "---\nname: demo\nversion: \"1.0\"\nsort: by_weight\ncolumns:\n  - text\n  - code\n...\n中国\tzhong guo\n",
        )
        .unwrap();
        let schema_src = shared.join("schema/demo/demo.schema.yaml");
        std::fs::create_dir_all(schema_src.parent().unwrap()).unwrap();
        std::fs::write(
            &schema_src,
            "schema:\n  schema_id: demo\ntranslator:\n  dictionary: demo\n",
        )
        .unwrap();
        let mut compiler = DictCompiler::new(
            "demo",
            "",
            Vec::new(),
            &user,
            &shared,
            &staging,
            shared.join("prebuilt"),
        );
        compiler.compile(Some(&schema_src)).expect("compile");

        let mut dict = Dictionary::open(&staging, "demo", "demo").expect("open demo");
        assert_eq!(dict.prism_name(), "demo", "默认只有一个 prism");
        assert!(!dict.has_prism("demo.double"));
        // 单 prism（全拼）：编译产出的 prism 键就是音节表。
        let full = dict.translate("zhongguo");
        let full_entry = full
            .iter()
            .find(|entry| entry.text == "中国" && entry.end == "zhongguo".len())
            .unwrap_or_else(|| panic!("全拼应能打出「中国」: {full:?}"));
        assert_eq!(
            dict.decode(&full_entry.code),
            Some(vec!["zhong".to_string(), "guo".to_string()])
        );

        // 双拼 prism：同一张音节表，只换拼写键（zhong→vs、guo→go）。
        let syllabary = dict.table.syllabary().expect("table syllabary");
        let mut script = Script::new();
        for syllable in syllabary.iter() {
            script.add_syllable(syllable);
        }
        let mut projection = Projection::from_rules(&[
            "xform/^zhong$/vs/".to_string(),
            "xform/^guo$/go/".to_string(),
        ])
        .expect("double pinyin rules");
        assert!(projection.apply_script(&mut script));
        let double_path = staging.join("demo/demo.double.prism.bin");
        Prism::compile(&double_path, &syllabary, Some(&script), 0, 0)
            .expect("build double prism");
        let double = Prism::open(&double_path).expect("open double prism");
        assert!(double.has_key("vs") && double.has_key("go"));

        assert!(dict.add_prism("demo.double", double));
        assert_eq!(dict.prism_name(), "demo", "AddPrism 不改变生效 prism");
        assert!(dict.set_prism("demo.double"));
        let got = dict.translate("vsgo");
        let entry = got
            .iter()
            .find(|entry| entry.text == "中国")
            .unwrap_or_else(|| panic!("双拼应能打出「中国」: {got:?}"));
        // 学词用的码串由音节 id → 规范音节串得到，与全拼一致 ——
        // 这是"多 prism 共用一份 userdb"成立的前提。
        assert_eq!(
            dict.decode(&entry.code),
            Some(vec!["zhong".to_string(), "guo".to_string()])
        );

        // 切回全拼：查询恢复，未知 prism 失败且不改状态。
        assert!(dict.set_prism("demo"));
        assert!(dict.translate("zhongguo").iter().any(|e| e.text == "中国"));
        assert!(!dict.set_prism("demo.nope"));
        assert_eq!(dict.prism_name(), "demo");
        assert!(!dict.has_prism("demo.nope"));

        std::fs::remove_dir_all(&root).ok();
    }
}
