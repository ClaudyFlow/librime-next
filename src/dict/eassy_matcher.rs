//! eassy（细胞词库）运行时索引：**汉字前缀树**（键 = 汉字，不生成任何编码）。
//!
//! # 为什么是汉字树
//!
//! eassy 只有「词 + 词频」，没有编码。之前那套「字→拼音派生编码」的做法已经删掉
//! （2026-11-06 用户定：**按汉字建树，不要编码**）：
//! * 派生编码依赖字音表，多音字会静默错；
//! * 而候选本来就是从词典里按音查出来的字，直接用这些字拼词、查 eassy 更准。
//!
//! 查询有两种：
//! * [`EassyIndex::contains`]：给一串汉字，判断它是不是 eassy 里的词（组合方案用）；
//! * [`EassyIndex::prefix`]：给一个前缀（如「你」），列出以它开头的词（补全用）。
//!
//! 内存代价随节点数增长；节点 = 所有词的所有前缀，实测见 `example/eassy_match_bench`。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// 前缀查询一次最多返回多少条（按词频降序）。
pub const PREFIX_LIMIT: usize = 32;

#[derive(Default)]
struct Node {
    /// 汉字 → 子节点下标。
    children: HashMap<char, u32>,
    /// 该节点是一个词的结尾时，存它的词频；0 = 不是词。
    freq: f64,
}

/// 汉字前缀树。构建完只读，可多线程共享。
pub struct EassyIndex {
    nodes: Vec<Node>,
    /// 收进去的词数。
    words: usize,
    /// 建索引时跳过的行数。
    skipped: usize,
}

impl EassyIndex {
    pub fn new() -> Self {
        Self {
            nodes: vec![Node::default()],
            words: 0,
            skipped: 0,
        }
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn word_count(&self) -> usize {
        self.words
    }

    pub fn skipped(&self) -> usize {
        self.skipped
    }

    /// 插入一个词（任意长度；空串忽略）。
    pub fn insert(&mut self, word: &str, freq: f64) {
        if word.is_empty() {
            return;
        }
        let mut cur = 0usize;
        for ch in word.chars() {
            let next = match self.nodes[cur].children.get(&ch) {
                Some(&index) => index as usize,
                None => {
                    self.nodes.push(Node::default());
                    let index = self.nodes.len() - 1;
                    self.nodes[cur].children.insert(ch, index as u32);
                    index
                }
            };
            cur = next;
        }
        if self.nodes[cur].freq == 0.0 {
            self.words += 1;
        }
        self.nodes[cur].freq = freq;
    }

    /// 这串汉字是不是 eassy 里的词？是就返回词频。
    pub fn contains(&self, word: &str) -> Option<f64> {
        let mut cur = 0usize;
        for ch in word.chars() {
            cur = *self.nodes[cur].children.get(&ch)? as usize;
        }
        let freq = self.nodes[cur].freq;
        if freq > 0.0 { Some(freq) } else { None }
    }

    /// 以 `prefix` 开头的词（按词频降序，最多 `limit` 条）。
    pub fn prefix(&self, prefix: &str, limit: usize) -> Vec<(String, f64)> {
        let mut cur = 0usize;
        for ch in prefix.chars() {
            match self.nodes[cur].children.get(&ch) {
                Some(&next) => cur = next as usize,
                None => return Vec::new(),
            }
        }
        let mut out = Vec::new();
        let mut stack = vec![(cur, prefix.to_string())];
        while let Some((index, word)) = stack.pop() {
            let node = &self.nodes[index];
            if node.freq > 0.0 {
                out.push((word.clone(), node.freq));
            }
            for (ch, child) in &node.children {
                let mut next = word.clone();
                next.push(*ch);
                stack.push((*child as usize, next));
            }
        }
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        out.truncate(limit);
        out
    }

    /// 从「词 + 词频」流构建。
    pub fn from_entries<I>(entries: I) -> Self
    where
        I: IntoIterator<Item = (String, f64)>,
    {
        let mut index = Self::new();
        for (word, freq) in entries {
            if word.chars().count() < 2 || word.chars().count() > 8 {
                index.skipped += 1;
                continue;
            }
            index.insert(&word, freq);
        }
        index
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

/// 后台建索引（只建一次）：读 `<root>/eassy/**/*.txt`，全部并进一棵树。
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
        if path.extension().and_then(|e| e.to_str()) != Some("txt") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
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
}
