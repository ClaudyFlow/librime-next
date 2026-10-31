//! `echo_translator`（对照 C++ `gear/echo_translator.{h,cc}`）。
//!
//! 把输入串原样当作一条候选吐出来，类型是 `raw`。它存在的意义只有一个：
//! **让"翻译不出东西"的段也有候选**，从而这个段能被确认、原始输入能被上屏。
//! 没有它，一个谁都认领不了的段（比如用户打了几个方案里没有的字母）会卡在
//! 组合状态里，只能靠 Escape 退出。
//!
//! `quality` 是 **-100**（`echo_translator.cc:38`，注释写着 "lowest priority"）。
//!
//! ## Compare 语义怎么落到了 runtime 上
//!
//! C++ 的 `EchoTranslation::Compare` 里有这么一段：
//!
//! ```cpp
//! if (!candidates.empty() || (other && !other->exhausted())) {
//!   set_exhausted(true);
//! }
//! ```
//!
//! 即"菜单里已经有候选，或者别的翻译器还有料 ⇒ 我立刻自尽"。这是**跨翻译器**的
//! 协商，而 LRN 的翻译模型是"每个翻译器各自返回 `Vec<Candidate>`，全塞进一个
//! `Menu` 按 `quality` 排序"，没有翻译器之间互相窥视的通道。
//!
//! 于是改用 [`Translator::is_fallback`] 声明"我的候选是兜底的"，runtime 把它单独
//! 收着，**只有在同一段的正经候选全空时**才补进去。这与上面那段 C++ 完全等价：
//!
//! - `!candidates.empty()` ⇔ 本段已有候选 → LRN 丢弃 fallback；
//! - `other && !other->exhausted()` ⇔ 别的翻译器还有料 → 同样已有候选。
//!
//! 单一 `quality` 排序做不到这件事：`-100` 只是"排最后"，候选仍然会出现，
//! 而 C++ 的行为是"根本不出现"。

use crate::engine::gear::translation::{Translation as TranslationTrait, VecTranslation};
use crate::engine::candidate::Candidate;
use crate::engine::commit_history::CommitHistory;
use crate::engine::gear::translator::Translator;
use crate::engine::segmentation::Segment;

/// 候选类型（`SimpleCandidate("raw", ...)`）。
const KIND_RAW: &str = "raw";

/// C++ 里的最低优先级（`echo_translator.cc:38`）。
const LOWEST_PRIORITY: f64 = -100.0;

/// 兜底翻译器：输入串原样成候选。
#[derive(Default)]
pub struct EchoTranslator;

impl EchoTranslator {
    pub fn new() -> Self {
        Self
    }
}

impl Translator for EchoTranslator {
    /// "我的候选只在别人都交白卷时才成立" —— 见模块注释。
    fn is_fallback(&self) -> bool {
        true
    }

    fn query(
        &mut self,
        input: &str,
        _segment: &mut Segment,
        _history: &CommitHistory,
    ) -> Option<Box<dyn TranslationTrait>> {
        if input.is_empty() {
            return None;
        }
        // C++ 传 `segment.start / segment.end`（绝对坐标）；LRN 的候选是**相对本段**，
        // 由 runtime 统一加 `segment.start`，所以这里 0..input.len()。
        Some(Box::new(VecTranslation::new(vec![Candidate::new(
            KIND_RAW,
            0,
            input.len(),
            input.to_string(),
            LOWEST_PRIORITY,
        )])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::gear::translation::drain_opt;

    fn segment() -> Segment {
        Segment::new(0, 3)
    }

    #[test]
    fn echoes_the_input_as_a_raw_candidate() {
        let mut t = EchoTranslator::new();
        let cands = drain_opt(t.query("abc", &mut segment(), &CommitHistory::new()));
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].text, "abc", "原样回显");
        assert_eq!(cands[0].kind, "raw");
        assert_eq!(cands[0].quality, -100.0, "最低优先级");
        assert_eq!((cands[0].start, cands[0].end), (0, 3), "坐标相对本段");
    }

    #[test]
    fn empty_input_yields_nothing() {
        let mut t = EchoTranslator::new();
        assert!(
            drain_opt(t.query("", &mut segment(), &CommitHistory::new())).is_empty(),
            "空输入没东西可回显（C++ 同样 `if (input.empty()) return nullptr;`）"
        );
    }

    #[test]
    fn is_marked_as_fallback() {
        assert!(
            EchoTranslator::new().is_fallback(),
            "必须标记为兜底，否则会与正常候选一起出现在菜单里"
        );
    }
}
