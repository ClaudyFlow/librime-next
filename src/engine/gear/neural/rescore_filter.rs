//! 神经重排滤镜——把词格 n-best 整句候选交给后台线程重排(见 `rescore.rs`)。
//!
//! # 挂载点
//!
//! 作为滤镜链的**最外层**只套在**最后一段**上(C++ msime 的词格重排同样只对
//! 覆盖全部输入的 n-best 路径做):前面的段落候选是已确认的词,重排没有意义。
//!
//! # 重排对象：两种模式
//!
//! 1. **整句裁决**：`quality` 的码长分量（×1e12）等于该段最大码长、且 ≥2 字
//!    的那批，即"整句路径"。这是原本的用途 —— 在整句候选里挑读起来最顺的。
//! 2. **选字**：整句组不足两条时（打 `shi` 只有一堆单字），退化为"同字长组"
//!    —— 头部 24 条里字符数相同、成员最多的那一组。同字长的静态分同尺度、
//!    模型分也只多算同样多的字符，可以直接比。
//!
//! **永远不跨字长比**：模型打的是绝对概率，长词天然吃亏，混排等于系统性
//! 把长词压后。用户词（层级分量 >0）也不进 batch——结构序保证用户词优先，
//! 神经模型没见过用户的用词习惯，不该推翻它。
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

/// 诊断开关：`WEASEL_DEBUG_RESCORE=1` 时把「这一批到底提交了什么／命中没有」打到 stderr。
/// 只读一次环境变量（OnceLock），默认关；生产路径上零分配。
fn debug_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| matches!(std::env::var("WEASEL_DEBUG_RESCORE"), Ok(v) if v == "1"))
}

/// 神经重排滤镜(无状态;真正的工作在 `RescoredTranslation`)。
pub struct NeuralRescoreFilter;

impl NeuralRescoreFilter {
    /// `WEASEL_RESCORE=off` 关掉整个选字重排（进程内只读一次）。
    ///
    /// 重排原本"只要模型在就生效"，没有关闭通道 —— 想对比手感只能把权重文件
    /// 改名或移走，等于拿部署目录当开关。输入法是常驻进程，任何"聪明"功能
    /// 都得留一条能立刻关掉的退路，不必重新编译、不必动数据。
    fn disabled() -> bool {
        static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *OFF.get_or_init(|| std::env::var("WEASEL_RESCORE").ok().as_deref() == Some("off"))
    }
}

impl Filter for NeuralRescoreFilter {
    fn applies_to_segment(&self, ctx: &Context, segment: &Segment) -> bool {
        if Self::disabled() {
            return false;
        }
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

        // ① 整句组：码长分量（quality / 1e12）等于头部最大码长，且 ≥2 字。
        //    这是「组句裁决」——整句候选之间比分。
        let quality_stride = 1e12f64;
        // 层级分量（1e9 那一位）：0 = 词典/码表，1 = 用户词，2 = 整句路径（见 translator.rs）。
        //
        // ⚠️ 2026-11-03 实测事故：本文件开头写着"用户词不进 batch"，但**代码里没有这条过滤**。
        // 后果是刚学过的词又被神经模型压回去 —— `pinyin.userdb` 里「界面」已有 c=3，
        // 打 `jiemian` 第一次仍是「街面」在前；「圆角」c=2 时连前五都不进。
        // 用户看到的就是"打了十几遍还是不到首选"。结构序（QUALITY_TIER_USER +1e9）
        // 本来就把用户词放最前，神经模型没见过用户的用词习惯，不该推翻它 ⇒ 这里真的排除。
        const TIER_USER: usize = 1;
        let tier_of = |q: f64| (q.rem_euclid(quality_stride) / 1e9) as usize;
        let s_max = head
            .iter()
            .map(|c| (c.quality / quality_stride) as usize)
            .max()
            .unwrap_or(0);
        let mut group: Vec<usize> = head
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                (c.quality / quality_stride) as usize == s_max
                    && c.text.chars().count() >= 2
                    && tier_of(c.quality) != TIER_USER
            })
            .map(|(i, _)| i)
            .collect();

        // ② 整句组不足两条 ⇒ 退化为「同字长组」：这就是**选字**。
        //
        //    打 `shi` 时头部 24 条全是单字，整句组是空的；这时该比的是
        //    「给定上文，哪个字更顺」，而不是「谁在词典里权重高」。同字长的
        //    候选静态分在同一尺度、模型分也只多算同样多的字符，可以直接比。
        //
        //    不同字长绝不混排：模型打的是绝对概率，长词天然吃亏，混在一起
        //    等于系统性地把长词压到后面。
        if group.len() < 2 {
            let mut buckets: Vec<(usize, Vec<usize>)> = Vec::new();
            for (i, candidate) in head.iter().enumerate() {
                let chars = candidate.text.chars().count();
                // 用户词同样不进「选字」组（同上面 ① 的理由：结构序优先）。
                if chars == 0 || tier_of(candidate.quality) == TIER_USER {
                    continue;
                }
                match buckets.iter_mut().find(|(len, _)| *len == chars) {
                    Some((_, members)) => members.push(i),
                    None => buckets.push((chars, vec![i])),
                }
            }
            // 人数并列时取字长更长的那一组：长词信息量大，模型判得更稳。
            if let Some((_, members)) = buckets
                .into_iter()
                .max_by(|a, b| a.1.len().cmp(&b.1.len()).then(a.0.cmp(&b.0)))
            {
                if members.len() >= 2 {
                    group = members;
                }
            }
        }
        let mut queue: VecDeque<Candidate> = head.into_iter().collect();

        if debug_on() {
            static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let head_desc: Vec<String> = queue
                .iter()
                .map(|c| format!("{}@{:.0}", c.text, c.quality))
                .collect();
            let group_desc: Vec<String> = group.iter().map(|&i| queue[i].text.clone()).collect();
            eprintln!(
                "[rescore#{n}] input={:?} caret={} segs={} head=[{}] s_max={} group=[{}]",
                ctx.composition.input(),
                ctx.caret_pos,
                ctx.composition.segmentation.segments.len(),
                head_desc.join(","),
                s_max,
                group_desc.join(",")
            );
        }

        let Some(worker) = rescore::global() else {
            if debug_on() {
                eprintln!("[rescore] worker=None（模型没加载，整层关闭）");
            }
            return Box::new(RescoredTranslation { queue, inner: translation });
        };
        if group.len() < 2 {
            if debug_on() {
                eprintln!("[rescore] group<2，不提交");
            }
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
                if debug_on() {
                    let after: Vec<String> = scored.iter().map(|c| c.text.clone()).collect();
                    eprintln!("[rescore] HIT order={order:?} => [{}]", after.join(","));
                }
                for (slot, candidate) in group.iter().zip(scored) {
                    queue[*slot] = candidate;
                }
                return Box::new(RescoredTranslation { queue, inner: translation });
            }
        }
        if debug_on() {
            eprintln!(
                "[rescore] MISS 提交批次 texts={texts:?} static={static_scores:?} gen={}",
                worker.current_generation()
            );
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
