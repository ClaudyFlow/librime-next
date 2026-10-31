//! `src/rime/menu.{h,cc}` 全量移植:**懒式**候选菜单(甲-1 懒化管线,2026-10-28)。
//!
//! # 语义(与 C++ 逐条对齐)
//!
//! * `candidates` 只存**已物化前缀**;
//! * `result` 是流式翻译(经滤镜链包装),`prepare(k)` 只取到第 k 条;
//! * `candidate_at(index)` 内部先 `prepare(index+1)`;
//! * `add_filter` 把 `result` 换成滤镜包装(`filter->Apply(result_, &candidates_)`)。
//!
//! # 内部可变性
//!
//! `result`/`candidates` 用 `RefCell` 包——所有方法 `&self` 即可。
//! 单线程场景(Global Mutex 保证),RefCell 安全。
//! 这样 Context 的不可变引用也能调 prepare/page/candidate_at,不再需要
//! `&mut` 贯穿整个调用链。

use crate::engine::candidate::Candidate;
use crate::engine::context::Context;
use crate::engine::gear::filter::Filter;
use crate::engine::gear::translation::{MergedTranslation, Translation};
use std::cell::RefCell;

/// 懒式候选菜单(对照 C++ `Menu`)。
pub struct Menu {
    /// 滤镜链包装后的流式翻译(`result_`)。
    result: RefCell<Option<Box<dyn Translation>>>,
    /// 已物化前缀(`candidates_`)。
    candidates: RefCell<Vec<Candidate>>,
}

impl Default for Menu {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for Menu {
    fn clone(&self) -> Self {
        // 懒流不可复制——备份场景(如 speller 的 previous_segment)只需要
        // 已物化前缀,后续 growth 丢失可接受(那是 auto_select 的短暂备份)。
        Self {
            result: RefCell::new(None),
            candidates: RefCell::new(self.candidates.borrow().clone()),
        }
    }
}

impl std::fmt::Debug for Menu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Menu")
            .field("prepared", &self.candidates.borrow().len())
            .finish()
    }
}

impl Menu {
    pub fn new() -> Self {
        Self {
            result: RefCell::new(None),
            candidates: RefCell::new(Vec::new()),
        }
    }

    /// `Menu::AddTranslation`:并入一个翻译器输出(多个时 MergedTranslation
    /// 归并,按候选质量降序)。
    pub fn add_translation(&self, translation: Box<dyn Translation>) {
        let mut result = self.result.borrow_mut();
        match result.take() {
            None => *result = Some(translation),
            Some(existing) => {
                *result = Some(Box::new(MergedTranslation::new(vec![existing, translation])));
            }
        }
    }

    /// `Menu::AddFilter`:把 `result` 换成滤镜包装(C++ 同名)。
    pub fn add_filter(&self, ctx: &Context, filter: &mut dyn Filter) {
        // 先把 result 取出来、**结束这次借用**，再调 filter。
        // 原来的 `if let Some(result) = self.result.borrow_mut().take()` 里，那个
        // `RefMut` 临时值会活到整个 if-let 结束（Rust 临时作用域规则），于是下面
        // 第 3 行再 borrow_mut 就 panic "RefCell already borrowed"。
        // 2026-10-31 实测：double_pinyin.flypy 一 compose 就崩；live 的 dll 是旧代码
        // 所以没暴露，重新编就会复现。
        let taken = { self.result.borrow_mut().take() };
        if let Some(result) = taken {
            let mut candidates = self.candidates.borrow_mut();
            let wrapped = filter.apply(ctx, result, &mut candidates);
            *self.result.borrow_mut() = Some(wrapped);
        }
    }

    /// `Menu::Prepare(requested)`:把已物化前缀补到至少 `requested` 条
    /// (或流穷尽)。返回物化后的条数。
    pub fn prepare(&self, requested: usize) -> usize {
        let mut candidates = self.candidates.borrow_mut();
        while candidates.len() < requested {
            // `peek` 不消费、`next` 才前进：只 peek 会把同一条候选反复压进来
            // （2026-10-31 实测：打 nihao 出 8 条一模一样的「你好」）。
            let peeked = {
                let mut result = self.result.borrow_mut();
                result.as_mut().and_then(|r| r.peek())
            };
            match peeked {
                Some(c) => {
                    candidates.push(c);
                    if let Some(result) = self.result.borrow_mut().as_mut() {
                        result.next();
                    }
                }
                None => break,
            }
        }
        candidates.len()
    }

    /// `Menu::GetCandidateAt(index)`:内部 `Prepare(index+1)`,返回克隆。
    pub fn candidate_at(&self, index: usize) -> Option<Candidate> {
        self.prepare(index + 1);
        self.candidates.borrow().get(index).cloned()
    }

    /// 已物化条数(懒式下这不是全量——需要全量先 `prepare(usize::MAX)`)。
    pub fn candidate_count(&self) -> usize {
        self.candidates.borrow().len()
    }

    /// `Menu::empty()`:物化为空且流已穷尽。
    pub fn is_empty(&self) -> bool {
        if !self.candidates.borrow().is_empty() {
            return false;
        }
        self.result
            .borrow_mut()
            .as_mut()
            .map_or(true, |r| r.peek().is_none())
    }

    /// `Menu::CreatePage(page_size, page_no)` 内联版:按需 Prepare 到页尾,
    /// 切片返回;`is_last_page` = 流已穷尽且取到最后一条。
    pub fn page(&self, page_size: usize, page_no: usize) -> (Vec<Candidate>, bool) {
        if page_size == 0 {
            return (Vec::new(), true);
        }
        let end = page_size * (page_no + 1);
        let count = self.prepare(end);
        let start = page_size * page_no;
        if start >= count {
            return (Vec::new(), true);
        }
        let end = end.min(count);
        let is_last = self.prepare(count + 1) == count;
        (self.candidates.borrow()[start..end].to_vec(), is_last)
    }

    /// 已物化前缀的只读快照。
    pub fn materialized(&self) -> Vec<Candidate> {
        self.candidates.borrow().clone()
    }
}
