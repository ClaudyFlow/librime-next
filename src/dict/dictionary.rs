//! `src/rime/dictionary.{h,cc}`：`Dictionary` 门面（装载 `Table + Prism + ReverseDb`，提供翻译）。
//!
//! MVP 做**词级拼音/形码翻译**：把输入按音节图切分（借助 `Prism::common_prefix_search`），
//! 对每个前缀切分 `Table::query_phrases(Code)` 取词条，候选 `end` = 该切分覆盖到的字节位置。
//! 这正好让「你」(单音节切分) 与「你好」(双音节切分) 同台出现，逐音节确认行为自然成立。

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use crate::algo::syllabifier::SyllableGraph;
use crate::dict::prism::Prism;
use crate::dict::reverse_lookup_dictionary::ReverseDb;
use crate::dict::table::Table;
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
pub struct Dictionary {
    pub name: String,
    pub prism: Prism,
    pub table: Table,
    pub reverse: Option<ReverseDb>,
}

impl Dictionary {
    /// 在 rime 的扁平 `build/` 下打开词典：
    /// `<dict_name>.table.bin` / `<dict_name>.reverse.bin` + `<prism_name>.prism.bin`
    /// （拼音方案的 table 与 prism 往往不同名，如 `luna_pinyin` vs `luna_pinyin_simp`）。
    pub fn open(build_dir: &Path, dict_name: &str, prism_name: &str) -> Option<Dictionary> {
        let table = Table::open(build_dir.join(format!("{dict_name}.table.bin"))).ok()?;
        let prism = Prism::open(build_dir.join(format!("{prism_name}.prism.bin"))).ok()?;
        let reverse = ReverseDb::open(build_dir.join(format!("{dict_name}.reverse.bin"))).ok();
        Some(Dictionary {
            name: dict_name.to_string(),
            prism,
            table,
            reverse,
        })
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
        let mut match_at: Vec<Vec<(SyllableId, usize)>> = vec![Vec::new(); n + 1];
        for start in 0..n {
            let suffix = &input[start..];
            for m in self.prism.common_prefix_search(suffix) {
                let syllable_id = self.prism.spelling_syllable_id(m.value);
                match_at[start].push((syllable_id, start + m.length));
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
            table: &Table,
            out: &mut Vec<DictEntry>,
        ) {
            if acc.len() > 12 {
                return; // 防极端爆搜
            }
            if start > 0 && !acc.is_empty() {
                let code = Code::from_slice(&acc[..]);
                let mut accessor = table.query_phrases(&code);
                let syllables = acc.len() as f64;
                while !accessor.exhausted() {
                    if let Some(entry) = accessor.entry() {
                        let text = table.entry_text(&entry);
                        if !text.is_empty() {
                            let weight = entry.weight as f64;
                            out.push(DictEntry {
                                text,
                                weight,
                                quality: syllables * 1000.0 + weight,
                                end: start,
                                comment: String::new(),
                                code: code.clone(),
                            });
                        }
                    }
                    accessor.next();
                }
            }
            if start >= n {
                return;
            }
            for &(id, end) in &match_at[start] {
                acc.push(id);
                dfs(end, n, acc, match_at, table, out);
                acc.pop();
            }
        }
        dfs(0, n, &mut acc, &match_at, &self.table, &mut entries);
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
    pub fn lookup_from_graph(
        &self,
        graph: &SyllableGraph,
        start_pos: usize,
        blacklist: Option<&HashSet<String>>,
        predict_word: bool,
    ) -> DictEntryCollector {
        /// `log(1e8)`（`dictionary.cc:166`）：table weight → 对数概率的偏移量。
        const K_WEIGHT_OFFSET: f64 = 18.420_680_743_952_367;

        let mut collector = DictEntryCollector::new();
        for (end_pos, pages) in self.table.query(graph, start_pos) {
            let bucket = collector.entry(end_pos).or_default();
            for page in pages {
                let mut accessor = page;
                let index_code = accessor.index_code().clone();
                let credibility = accessor.credibility();
                let quality_len = accessor.quality_len();
                while !accessor.exhausted() {
                    let Some(entry) = accessor.entry() else {
                        break;
                    };
                    let text = self.table.entry_text(&entry);
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
                        collector.entry(target_end).or_default().push(VocabularyEntry {
                            text,
                            weight: entry.weight as f64 - K_WEIGHT_OFFSET + credibility,
                            code: index_code.clone(),
                            quality_len,
                            ..Default::default()
                        });
                    }
                    accessor.next();
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

