//! `src/rime/menu.{h,cc}`：候选菜单（一个段的候选集合）。

use crate::engine::candidate::Candidate;

/// 候选菜单（对照 C++ `Menu`）。Rust 版做「急切展开」：翻译器已经把所有候选铺平，
/// 排序去重后存在 `candidates` 里；分页只是切片。
#[derive(Clone, Debug, Default)]
pub struct Menu {
    candidates: Vec<Candidate>,
}

impl Menu {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, candidate: Candidate) {
        self.candidates.push(candidate);
    }

    /// 按 quality 降序排序并去重（同文本保留质量最高者）。
    pub fn prepare(&mut self) {
        self.candidates.sort_by(|a, b| {
            b.quality
                .partial_cmp(&a.quality)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut seen = std::collections::HashSet::new();
        self.candidates.retain(|c| seen.insert(c.text.clone()));
    }

    pub fn candidate_count(&self) -> usize {
        self.candidates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    pub fn candidate_at(&self, index: usize) -> Option<&Candidate> {
        self.candidates.get(index)
    }

    /// 全部候选（已排序去重后的视图）。
    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    /// 取出第 `page_no` 页（每页 `page_size` 条）。`is_last_page` 由调用方据总数判断。
    pub fn page(&self, page_size: usize, page_no: usize) -> Vec<Candidate> {
        if page_size == 0 {
            return Vec::new();
        }
        let start = page_no * page_size;
        self.candidates
            .iter()
            .skip(start)
            .take(page_size)
            .cloned()
            .collect()
    }
}
