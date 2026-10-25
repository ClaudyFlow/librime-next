//! `src/rime/dictionary.{h,cc}`：`Dictionary` 门面（装载 `Table + Prism + ReverseDb`，提供翻译）。
//!
//! MVP 做**词级拼音/形码翻译**：把输入按音节图切分（借助 `Prism::common_prefix_search`），
//! 对每个前缀切分 `Table::query_phrases(Code)` 取词条，候选 `end` = 该切分覆盖到的字节位置。
//! 这正好让「你」(单音节切分) 与「你好」(双音节切分) 同台出现，逐音节确认行为自然成立。

use std::path::{Path, PathBuf};

use crate::dict::prism::Prism;
use crate::dict::reverse_lookup_dictionary::ReverseDb;
use crate::dict::table::Table;
use crate::dict::vocabulary::{Code, SyllableId};

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
}
