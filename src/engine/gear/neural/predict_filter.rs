//! 预测候选滤镜——把「模型觉得接下来该接什么」补进菜单。
//!
//! # 只在候选稀薄时出手
//!
//! 词典给得足的时候插预测是纯打扰（会顶掉用户背下来的位置）。所以门槛定在
//! 「整段候选不足 [`THIN_CANDIDATES`] 条」—— 菜单本来就空，补位只有好处。
//! 这也正是新词打不出来的场景：`yibutini` 只有四个单字，才需要模型帮着拼。
//!
//! # 异步
//!
//! 续写是逐字自回归（一步一次 forward），比整批打分更慢，**不能长在按键路径上**。
//! 第一次经过只提交任务、原样放行；结果落表后 `poll_rescore_landing` 触发重新
//! compose，这一次命中结果表才真把预测插进去。
//!
//! # 与重排的区别
//!
//! 重排只换顺序、不造候选；预测会**新增**候选。所以它放在滤镜链最外层
//! （后挂 = 包在外面），看到的是重排后的顺序 —— 拿重排后的首候选当续写锚点，
//! 等于让两个功能接力：先选对当前词，再猜下一个词。

use std::collections::VecDeque;

use crate::engine::candidate::Candidate;
use crate::engine::context::Context;
use crate::engine::gear::filter::Filter;
use crate::engine::gear::rescore;
use crate::engine::gear::translation::Translation;
use crate::engine::segmentation::Segment;

/// 候选少于此数才补预测。
const THIN_CANDIDATES: usize = 8;
/// 拿前几条候选当续写锚点。
const ANCHORS: usize = 2;

/// 预测滤镜（无状态；工作全在后台线程与结果表里）。
pub struct NeuralPredictFilter;

impl NeuralPredictFilter {
    /// `WEASEL_PREDICT=off` 关掉整个预测（进程内只读一次）。
    ///
    /// 输入法是常驻进程、候选窗又贴着人的手，任何"聪明"功能都得留一条能立刻
    /// 关掉的退路 —— 不必重新编译。
    fn disabled() -> bool {
        static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *OFF.get_or_init(|| {
            std::env::var("WEASEL_PREDICT").ok().as_deref() == Some("off")
        })
    }

    /// 物化到 [`THIN_CANDIDATES`] 条，同时判定这一段是不是「候选稀薄」。
    ///
    /// **必须把吃掉的那几条一起交回去** —— 判定为「不稀薄」时它们已经离开
    /// 原流了，丢了就是候选凭空消失。
    fn probe(
        translation: &mut Box<dyn Translation>,
    ) -> (VecDeque<Candidate>, bool, Vec<String>) {
        let mut head: VecDeque<Candidate> = VecDeque::with_capacity(THIN_CANDIDATES);
        let mut thin = false;
        while head.len() < THIN_CANDIDATES {
            match translation.peek() {
                Some(candidate) => {
                    head.push_back(candidate);
                    translation.next();
                }
                None => {
                    thin = true;
                    break;
                }
            }
        }
        let anchors: Vec<String> = if thin {
            head.iter()
                .take(ANCHORS)
                .map(|candidate| candidate.text.clone())
                .filter(|text| !text.is_empty())
                .collect()
        } else {
            Vec::new()
        };
        (head, thin, anchors)
    }
}

impl Filter for NeuralPredictFilter {
    fn applies_to_segment(&self, ctx: &Context, segment: &Segment) -> bool {
        if Self::disabled() {
            return false;
        }
        // 只对最后一段：预测是"往后接"，前面各段后面还有字，接不上。
        segment.end == ctx.composition.segmentation.input.len()
    }

    fn apply(
        &mut self,
        ctx: &Context,
        mut translation: Box<dyn Translation>,
        _candidates: &mut Vec<Candidate>,
    ) -> Box<dyn Translation> {
        let (queue, thin, anchors) = Self::probe(&mut translation);
        let mut queue = queue;
        // 候选充足：把探到的这几条按原序交回去，不做任何预测。
        if !thin || anchors.is_empty() {
            return Box::new(QueuedTranslation {
                queue,
                inner: translation,
            });
        }
        let context = ctx.commit_history.latest_text();

        let Some(worker) = rescore::global() else {
            return Box::new(QueuedTranslation {
                queue,
                inner: translation,
            });
        };
        match worker.predict_for(&context, &anchors) {
            Some(predicted) => {
                // (start, end) 取最后一段的区间：预测候选覆盖整段输入。
                let (start, end) = ctx
                    .composition
                    .segmentation
                    .segments
                    .last()
                    .map_or((0, 0), |segment| (segment.start, segment.end));
                for (text, _score) in predicted {
                    if queue.iter().any(|existing| existing.text == text) {
                        continue;
                    }
                    queue.push_back(
                        Candidate::new("predict", start, end, text, 0.0).with_comment("预测"),
                    );
                }
            }
            None => {
                worker.submit_predict(&context, anchors, Default::default());
            }
        }
        Box::new(QueuedTranslation {
            queue,
            inner: translation,
        })
    }
}

/// 先发队列、再直通内层流。
struct QueuedTranslation {
    queue: VecDeque<Candidate>,
    inner: Box<dyn Translation>,
}

impl Translation for QueuedTranslation {
    fn peek(&mut self) -> Option<Candidate> {
        if let Some(front) = self.queue.front() {
            return Some(front.clone());
        }
        self.inner.peek()
    }
    fn next(&mut self) {
        if self.queue.pop_front().is_none() {
            self.inner.next();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 小流：给定一批候选，按顺序发完就结束。
    struct Fixed(VecDeque<Candidate>);

    impl Translation for Fixed {
        fn peek(&mut self) -> Option<Candidate> {
            self.0.front().cloned()
        }
        fn next(&mut self) {
            self.0.pop_front();
        }
    }

    fn stream(words: &[&str]) -> Box<dyn Translation> {
        Box::new(Fixed(
            words
                .iter()
                .enumerate()
                .map(|(i, word)| Candidate::new("test", 0, 2, *word, 100.0 - i as f64))
                .collect(),
        ))
    }

    #[test]
    fn thin_stream_yields_anchors() {
        let mut translation = stream(&["一", "二", "三"]);
        let (head, thin, anchors) = NeuralPredictFilter::probe(&mut translation);
        assert!(thin);
        assert_eq!(head.len(), 3, "稀薄时把整段都吃进来当头部");
        assert_eq!(anchors, vec!["一".to_string(), "二".to_string()]);
    }

    #[test]
    fn rich_stream_has_no_anchors() {
        let words: Vec<String> = (0..THIN_CANDIDATES).map(|i| i.to_string()).collect();
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        let mut translation = stream(&refs);
        let (head, thin, anchors) = NeuralPredictFilter::probe(&mut translation);
        assert!(!thin, "凑满 THIN_CANDIDATES 条就不算稀薄");
        assert!(anchors.is_empty());
        assert_eq!(head.len(), THIN_CANDIDATES, "探到的候选必须还回去");
    }

    /// 稀薄流经滤镜后：原候选在前，顺序不变。
    #[test]
    fn thin_stream_keeps_original_order() {
        use crate::engine::context::Context;

        let ctx = Context::new();
        let mut filter = NeuralPredictFilter;
        let mut translation = filter.apply(&ctx, stream(&["甲", "乙"]), &mut Vec::new());
        let mut seen = Vec::new();
        while let Some(candidate) = translation.peek() {
            seen.push(candidate.text);
            translation.next();
        }
        assert_eq!(seen, vec!["甲".to_string(), "乙".to_string()]);
    }
}
