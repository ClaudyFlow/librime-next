//! 神经重排滤镜——把词格 n-best 整句候选交给后台线程重排(见 `rescore.rs`)。
//!
//! # 挂载点
//!
//! 作为滤镜链的**最外层**只套在**最后一段**上(C++ msime 的词格重排同样只对
//! 覆盖全部输入的 n-best 路径做):前面的段落候选是已确认的词,重排没有意义。
//!
//! # 重排对象
//!
//! 只取**全覆盖**候选——`quality` 的码长分量(×1e12)等于该段最大码长的那批,
//! 即"整句路径"。短词/单字不进 batch:神经模型打的是绝对概率,长句天然吃亏,
//! 拿它跟短词比排序是错误问题。用户词(层级分量 >0)不进 batch——结构序保证
//! 用户词优先,神经模型没见过用户的用词习惯,不该推翻它。
//!
//! # 异步语义
//!
//! `apply` 时先查结果表:命中(同批次打过了——重打、重 compose)当场重排;
//! 未命中就把 batch 提交给后台线程,本次按静态顺序发。结果落地后不再动
//! 已物化的顺序——菜单的物化前缀是增量的,流中间换序会产生"翻页回到
//! 上一页看过的一半"的错乱;跨次一致的重排在下一轮 compose 生效。

use std::collections::VecDeque;

use crate::engine::candidate::Candidate;
use crate::engine::context::Context;
use crate::engine::gear::filter::Filter;
use crate::engine::gear::rescore;
use crate::engine::gear::translation::Translation;
use crate::engine::segmentation::Segment;

/// 头部一次最多物化多少条(重排 batch + 同层的其余候选)。
const MAX_HEAD: usize = 24;

/// 神经重排滤镜(无状态;真正的工作在 `RescoredTranslation`)。
pub struct NeuralRescoreFilter;

impl Filter for NeuralRescoreFilter {
    fn applies_to_segment(&self, ctx: &Context, segment: &Segment) -> bool {
        // 只套最后一段:整句候选只在那里。
        segment.end == ctx.composition.segmentation.input.len()
    }

    fn apply(
        &mut self,
        ctx: &Context,
        mut translation: Box<dyn Translation>,
        _candidates: &mut Vec<Candidate>,
    ) -> Box<dyn Translation> {
        // 物化头部候选(重排只发生在有限前缀上;之后的流保持懒式)。
        let mut head: Vec<Candidate> = Vec::with_capacity(MAX_HEAD);
        for _ in 0..MAX_HEAD {
            match translation.peek() {
                Some(c) => {
                    head.push(c);
                    translation.next();
                }
                None => break,
            }
        }

        // 全覆盖候选组:码长分量(quality / 1e12)等于头部最大码长。
        let quality_stride = 1e12f64;
        let s_max = head
            .iter()
            .map(|c| (c.quality / quality_stride) as usize)
            .max()
            .unwrap_or(0);
        let group: Vec<usize> = head
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                (c.quality / quality_stride) as usize == s_max && c.text.chars().count() >= 2
            })
            .map(|(i, _)| i)
            .collect();
        let mut queue: VecDeque<Candidate> = head.into_iter().collect();

        let Some(worker) = rescore::global() else {
            return Box::new(RescoredTranslation { queue, inner: translation });
        };
        if group.len() < 2 {
            return Box::new(RescoredTranslation { queue, inner: translation });
        }

        // 静态分:词典权重的对数压缩(质量 = 码长×1e12 + 层级×1e9 + weight;
        // 取余数把结构分量剥掉,只剩"这词条本身多常见")。
        let texts: Vec<String> = group.iter().map(|&i| queue[i].text.clone()).collect();
        let static_scores: Vec<f64> = group
            .iter()
            .map(|&i| (queue[i].quality.rem_euclid(1e9).max(1.0)).ln())
            .collect();
        let context = ctx.commit_history.latest_text();

        // 结果表命中:当场重排(同批次打过了——重打同一句话、auto_select 重 compose)。
        if let Some(order) = worker.order_for(&context, &texts) {
            if order.len() == group.len() {
                let scored: Vec<Candidate> =
                    order.iter().map(|&o| queue[group[o]].clone()).collect();
                for (slot, candidate) in group.iter().zip(scored) {
                    queue[*slot] = candidate;
                }
                return Box::new(RescoredTranslation { queue, inner: translation });
            }
        }
        // 未命中:交给后台,下次同样的批次落地。
        worker.submit(&context, texts, static_scores, Default::default());
        Box::new(RescoredTranslation { queue, inner: translation })
    }
}

/// 重排后的翻译:队列按当前已知最优顺序发出,流尾直通。
struct RescoredTranslation {
    queue: VecDeque<Candidate>,
    inner: Box<dyn Translation>,
}

impl Translation for RescoredTranslation {
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
