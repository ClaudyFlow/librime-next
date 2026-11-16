//! eassy（细胞词库）运行时索引：**双数组字典树（DAT）**（键 = 汉字的 UTF-8 字节，不生成任何编码）。
//!
//! # 为什么是汉字树
//!
//! eassy 只有「词 + 词频」，没有编码。之前那套「字→拼音派生编码」的做法已经删掉
//! （2026-11-06 用户定：**按汉字建树，不要编码**）：
//! * 派生编码依赖字音表，多音字会静默错；
//! * 而候选本来就是从词典里按音查出来的字，直接用这些字拼词、查 eassy 更准。
//!
//! 2026-11-15 用户定：**全套切 DAT**——与 `.prism.bin` / `.table.bin` 用同一套
//! [`crate::gear::dict::darts::DoubleArray`] 后端，替掉旧的 `HashMap<char, u32>` 逐节点树。
//!
//! 查询有两种：
//! * [`EassyIndex::contains`]：给一串汉字，判断它是不是 eassy 里的词（组合方案用）——
//!   直接走 `DoubleArray::exact_match_search`，命中后按下标取词频；
//! * [`EassyIndex::prefix`]：给一个前缀（如「你」），列出以它开头的词（补全用）——
//!   在按字节排序的词下标表上二分出前缀区间，再按词频降序截断。
//!
//! 内存代价：双数组单元 ≈ 词表总字节数的常数倍 + 词串本体；构建一次、只读共享。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::gear::dict::darts::DoubleArray;

/// 前缀查询一次最多返回多少条（按词频降序）。
pub const PREFIX_LIMIT: usize = 32;

/// 汉字双数组字典树。构建完只读，可多线程共享。
pub struct EassyIndex {
    /// 词（UTF-8 字节串）→ 下标 id 的双数组；`exact_match_search` 的值 = `words` 下标。
    da: DoubleArray,
    /// 按 id 索引的词表。
    words: Vec<String>,
    /// 按 id 索引的词频（与 `words` 对齐）。
    freqs: Vec<f64>,
    /// `words` 下标按字节序排好的表（前缀区间二分用）。
    sorted: Vec<usize>,
    /// 建索引时跳过的行数。
    skipped: usize,
}

impl EassyIndex {
    pub fn new() -> Self {
        // 空索引：只有根单元的双数组，任何查询都落空。
        let da = DoubleArray::build(&[]).unwrap_or_default();
        Self {
            da,
            words: Vec::new(),
            freqs: Vec::new(),
            sorted: Vec::new(),
            skipped: 0,
        }
    }

    /// 双数组单元数（旧 `HashMap` 版返回的是节点数，这里是最接近的对应物，
    /// 只给基准/统计用，不参与查询语义）。
    pub fn node_count(&self) -> usize {
        self.da.size()
    }

    pub fn word_count(&self) -> usize {
        self.words.len()
    }

    pub fn skipped(&self) -> usize {
        self.skipped
    }

    /// 这串汉字是不是 eassy 里的词？是就返回词频。
    pub fn contains(&self, word: &str) -> Option<f64> {
        let id = self.da.exact_match_search(word.as_bytes())? as usize;
        let freq = *self.freqs.get(id)?;
        if freq > 0.0 { Some(freq) } else { None }
    }

    /// 以 `prefix` 开头的词（按词频降序，最多 `limit` 条）。
    pub fn prefix(&self, prefix: &str, limit: usize) -> Vec<(String, f64)> {
        if self.sorted.is_empty() {
            return Vec::new();
        }
        let prefix_bytes = prefix.as_bytes();
        // 词表按字节序排好后，以 `prefix` 开头的词是一段连续区间。
        let lo = self
            .sorted
            .partition_point(|&i| self.words[i].as_bytes() < prefix_bytes);
        let hi = lo
            + self.sorted[lo..]
                .partition_point(|&i| self.words[i].as_bytes().starts_with(prefix_bytes));
        let mut out: Vec<(String, f64)> = self.sorted[lo..hi]
            .iter()
            .map(|&i| (self.words[i].clone(), self.freqs[i]))
            .collect();
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        out.truncate(limit);
        out
    }

    /// 从「词 + 词频」流构建。重复词取**最后一次**的词频（与旧版 `insert` 语义一致），
    /// 词只计一次。
    pub fn from_entries<I>(entries: I) -> Self
    where
        I: IntoIterator<Item = (String, f64)>,
    {
        let mut words: Vec<String> = Vec::new();
        let mut freqs: Vec<f64> = Vec::new();
        let mut order: HashMap<String, usize> = HashMap::new();
        let mut skipped = 0usize;
        for (word, freq) in entries {
            let length = word.chars().count();
            if !(2..=8).contains(&length) {
                skipped += 1;
                continue;
            }
            match order.get(&word) {
                Some(&id) => freqs[id] = freq,
                None => {
                    order.insert(word.clone(), words.len());
                    words.push(word);
                    freqs.push(freq);
                }
            }
        }
        Self::from_word_table(words, freqs, skipped)
    }

    /// 从 eassy 文本（`词\t词频`）构建。
    pub fn from_file(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        let entries = text.lines().filter_map(|line| {
            let mut parts = line.split('\t');
            let word = parts.next()?.to_string();
            if word.is_empty() || word.starts_with('#') {
                return None;
            }
            let freq = parts.next().and_then(|f| f.trim().parse::<f64>().ok()).unwrap_or(1.0);
            Some((word, freq))
        });
        Some(Self::from_entries(entries))
    }

    /// 已有词表 + 词频（`words[i]` ↔ `freqs[i]`，无重复）时收口：建双数组 + 排序下标表。
    fn from_word_table(words: Vec<String>, freqs: Vec<f64>, skipped: usize) -> Self {
        debug_assert_eq!(words.len(), freqs.len());
        let mut sorted: Vec<usize> = (0..words.len()).collect();
        sorted.sort_unstable_by(|&a, &b| words[a].as_bytes().cmp(words[b].as_bytes()));
        let key_bytes: Vec<&[u8]> = words.iter().map(|s| s.as_bytes()).collect();
        // 建树失败（offset 溢出，真实词库不会）就退化为空索引：宁缺勿崩，
        // 调用方 `init_from_data_roots` 本来就承诺"绝不拖累输入法"。
        let da = DoubleArray::build(&key_bytes).unwrap_or_default();
        Self {
            da,
            words,
            freqs,
            sorted,
            skipped,
        }
    }
}

impl Default for EassyIndex {
    fn default() -> Self {
        Self::new()
    }
}

// ------------------------------------------------------------------ 全局单例

static INDEX: OnceLock<Arc<EassyIndex>> = OnceLock::new();
static LOADING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `WEASEL_EASSY=off` 关闭整个 eassy 匹配（进程内只读一次）。
pub fn disabled() -> bool {
    static OFF: OnceLock<bool> = OnceLock::new();
    *OFF.get_or_init(|| std::env::var("WEASEL_EASSY").ok().as_deref() == Some("off"))
}

/// 后台建索引（只建一次）：读 `<root>/eassy/**/*.yaml`，全部并进一棵树。
///
/// 没有 eassy 文件、或构建 panic，都静默保持"没有 eassy"，绝不拖累输入法。
pub fn init_from_data_roots(roots: &[PathBuf]) {
    if disabled() || INDEX.get().is_some() {
        return;
    }
    if LOADING.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    let roots = roots.to_vec();
    std::thread::spawn(move || {
        let built = std::panic::catch_unwind(|| build_from_roots(&roots)).ok().flatten();
        match built {
            Some(index) => {
                let _ = INDEX.set(Arc::new(index));
            }
            None => LOADING.store(false, std::sync::atomic::Ordering::Release),
        }
    });
}

pub fn ready() -> bool {
    INDEX.get().is_some()
}

/// 精确查（组合方案用）；索引未就绪返回 `None`。
pub fn contains(word: &str) -> Option<f64> {
    INDEX.get().and_then(|index| index.contains(word))
}

/// 前缀补全；索引未就绪返回空。
pub fn prefix(prefix: &str, limit: usize) -> Vec<(String, f64)> {
    INDEX.get().map(|index| index.prefix(prefix, limit)).unwrap_or_default()
}

fn build_from_roots(roots: &[PathBuf]) -> Option<EassyIndex> {
    let eassy = roots.iter().map(|root| root.join("eassy")).find(|path| path.is_dir())?;
    let mut entries: Vec<(String, f64)> = Vec::new();
    collect(&eassy, &mut entries, 0);
    if entries.is_empty() {
        return None;
    }
    Some(EassyIndex::from_entries(entries))
}

fn collect(dir: &Path, out: &mut Vec<(String, f64)>, depth: usize) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out, depth + 1);
            continue;
        }
        // 词源只认 YAML(用户 2026-11-08 定版):三行头 + 词	频 行。
        // 基础语料 essay.txt 已迁移为 essay.yaml。
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if ext.as_deref() != Some("yaml") {
            continue;
        }
        let is_yaml = true;
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut cleaned = text.to_string();
        if is_yaml {
            // Rime YAML 头:从 `---` 行之后开始取词条;没有 `...` 收尾也要稳。
            // (实机文件头 = "# Rime Eassy / # encode: utf-8 / # newline: lf / ---")
            if let Some(i) = cleaned.find("\n---") {
                cleaned = cleaned[i + 4..].to_string();
            } else if cleaned.starts_with("---") {
                cleaned = cleaned[3..].to_string();
            }
            cleaned = cleaned
                .lines()
                .skip_while(|line| line.trim() == "..." || line.starts_with('#'))
                .collect::<Vec<_>>()
                .join("\n");
        }
        for line in cleaned.lines() {
            let mut parts = line.split('\t');
            let Some(word) = parts.next() else { continue };
            if word.is_empty() || word.starts_with('#') {
                continue;
            }
            let freq = parts.next().and_then(|f| f.trim().parse::<f64>().ok()).unwrap_or(1.0);
            out.push((word.to_string(), freq));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> EassyIndex {
        let entries = vec![
            ("你好".to_string(), 4869.0),
            ("你们".to_string(), 3000.0),
            ("你".to_string(), 9999.0),      // 单字：跳过
            ("中国".to_string(), 337254.0),
            ("世界".to_string(), 121499.0),
        ];
        EassyIndex::from_entries(entries)
    }

    #[test]
    fn contains_is_exact() {
        let index = index();
        assert_eq!(index.contains("你好"), Some(4869.0));
        assert_eq!(index.contains("中国"), Some(337254.0));
        assert_eq!(index.contains("拟好"), None);
        assert_eq!(index.contains("你"), None, "单字不入库");
    }

    #[test]
    fn prefix_lists_words_by_frequency() {
        let index = index();
        let hits = index.prefix("你", 10);
        assert_eq!(hits[0].0, "你好");
        assert_eq!(hits[1].0, "你们");
        assert!(index.prefix("他", 10).is_empty());
    }

    #[test]
    fn empty_and_unknown_return_none() {
        let index = index();
        assert_eq!(index.contains(""), None);
        assert_eq!(index.contains("世界和平"), None);
    }

    /// DAT 版专属行为钉子：重复词取最后词频、词只计一次；
    /// 词数 / 跳过数与旧 HashMap 版一致。
    #[test]
    fn dat_stats_match_word_table() {
        let index = index();
        assert_eq!(index.word_count(), 4, "你好/你们/中国/世界（单字被跳过）");
        assert_eq!(index.skipped(), 1, "单字「你」被跳过");
        assert!(index.node_count() > 0, "双数组非空");
        // 重复词：后者覆盖词频，词数不重复累计
        let dedup = EassyIndex::from_entries(vec![
            ("你好".to_string(), 1.0),
            ("你好".to_string(), 7.0),
        ]);
        assert_eq!(dedup.word_count(), 1);
        assert_eq!(dedup.contains("你好"), Some(7.0), "重复词取最后一次词频");
        assert_eq!(dedup.prefix("你", 10), vec![("你好".to_string(), 7.0)]);
    }

    /// 空索引：查询全部落空、不 panic。
    #[test]
    fn empty_index_queries_fall_through() {
        let index = EassyIndex::new();
        assert_eq!(index.contains("你好"), None);
        assert!(index.prefix("你", 10).is_empty());
        assert_eq!(index.word_count(), 0);
        assert_eq!(index.node_count(), 1, "只剩根单元");
    }

    /// `from_file` 端到端：真实「词\t词频」文本进 DAT。
    #[test]
    fn from_file_round_trips_through_dat() {
        let dir = std::env::temp_dir().join(format!("eassy_dat_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let path = dir.join("mini.yaml");
        std::fs::write(
            &path,
            "# Rime Eassy\n---\n你好\t4869\n你们\t3000\n再见\t12\n...\n",
        )
        .expect("写临时文件");
        let index = EassyIndex::from_file(&path).expect("读文件");
        assert_eq!(index.contains("你好"), Some(4869.0));
        assert_eq!(index.contains("再见"), Some(12.0));
        let hits = index.prefix("你", 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, "你好", "词频高者在前");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
