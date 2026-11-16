//! 在线线性重排头（方案 A）。
//!
//! # 位置
//!
//! 神经重排（[`super::rescore_filter`]）拿到 λ 融合分之后，在**同一个候选组内**
//! 再加一层 `w · f`。`w` 初值全零 ⇒ 顺序与没有这一层完全一致；只有
//! `samples ≥ [`SHADOW_MIN_SAMPLES`]` 才真正参与排序（之前是影子模式，只记账）。
//!
//! # 样本怎么来
//!
//! 重排滤镜把「本批候选的特征 + 融合分」挂到 `Candidate` 上（[`RerankRef`]），
//! 用户选中哪条，`Context` 就把那条的下标记进 `pending_rerank_samples`；
//! `Engine::apply_commit` 取走并调 [`observe_samples`]。全程走既有上下文传递，
//! 不引入全局可变状态。
//!
//! # 边界
//!
//! * 只在**同字长、同层级**的连续段内调序（用户词层永远不动）；
//! * 单步参数增量 `‖Δw‖ ≤ [`MAX_STEP_NORM`]`；
//! * 每天 `w ← 0.99w`（权重衰减，防一次误上屏永久污染）；
//! * 参数文件 `%APPDATA%\Rime-Next\rerank_head.tsv`，写入复用
//!   [`crate::engine::user_ngram::write_atomic`]；加载失败/校验和不符整份丢弃回零。
//!
//! # 三层开关
//!
//! 1. 环境变量 `WEASEL_RERANK_HEAD=off|0|false` ⇒ 完全回退；
//! 2. 方案配置 `translator/enable_rerank_head: false`（运行时不再挂载头部）；
//! 3. [`set_enabled_override`]（设置页/宿主可调）。
//!
//! 任意一层关掉都只影响「调序与学习」，神经重排本身照旧。
//!
//! # 影子 → 生效：离线回放结论（2026-11-09，合成标签）
//!
//! [`effective()`] 目前只看 `samples >= `[`SHADOW_MIN_SAMPLES`] + 开关。
//! ⚠️ 影子期**权重一直在训练**（[`observe_samples`] 不区分影子/生效），
//! 所以 200 不是「开始学习」而是「开始影响顺序」：第 200 次提交会带着
//! 200 步训练好的权重立刻改序，没有质量兜底。
//!
//! `offline_replay_shadow_vs_effective`（release `--nocapture`）用固定 6 条候选、
//! 基线 top-1=0、真实偏好=3 的**非平凡**合成标签跑 600 次提交：
//!
//! | 场景 | 影子期改序 | 生效期 head top-1 | 平均选择序位 | 放弃率 | max\|w\| |
//! |---|---:|---:|---:|---:|---:|
//! | 可学标签（恒选 3） | 0 次 | 400/400 | 0.00 | 0/400 | 触及 2.0 钳位 |
//! | 轮转噪声标签 | 0 次 | 65/400 | 2.51（≈随机 2.5） | 199/400（≈随机 50%） | 0.46 |
//!
//! 结论：保护项（单步 ≤ 0.01、L2、每日 0.99 衰减、±2.0 钳位）在噪声标签下
//! 确实压住了参数，既没改善也没恶化；可学标签下能纠偏。**但这是合成数据，
//! 不能当上线收益依据。**
//!
//! ⚠️ `final_w` 还暴露一个退化现象：第 7/8/9/11 维
//! （`len8` / `user` / `edit8` / `userdb_w`）在本构造下**全组恒为常数**，
//! 对组内排序本该零贡献，权重却照样漂到 ±2.0 钳位。这是「6 条候选 + 12 维
//! + 每步同一标签」的过拟合，也提示真实生效前应对特征做**组内中心化**
//! （减去组内均值），否则常数维只白占参数容量。真实数据里这几维并非常数，
//! **不能直接删**，需真实样本复核。
//!
//! ## 2026-11-10 补上两道闸（回应上表暴露的缺口）
//!
//! 1. **质量闸**：[`effective()`] 不再只看 `samples >= 200`。旧语义下第 200 次
//!    提交会带着 200 步训练好的权重**立刻改序**，噪声标签也照放。现在要求
//!    `head top-1 >= baseline top-1 + `[`MIN_TOP1_GAIN`]（200 样本里多命中 10 次，
//!    ≈ 5 个百分点），且两个计数都要 ≥ 200 次可比较样本。离线回放里的轮转噪声
//!    标签（head 65/400）过不了这道闸。**注意这只保证历史上更准，不保证未来收益。**
//! 2. **统计落盘**：`shadow_baseline_top1` / `shadow_head_top1` / `shadow_loss` /
//!    `shadow_updates` 随 `samples` 一起写进参数文件（[`VERSION`] 推到 2），
//!    重启不再归零 —— 否则每次重启都要从 0 重新攒 200 样本，质量闸永远不生效。
//!    旧 v1 文件会被整份丢弃回零（真实 `%APPDATA%\Rime-Next\rerank_head.tsv`
//!    目前不存在，无兼容负担）。
//!
//! 真实 `%APPDATA%\Rime-Next\rerank_head.tsv` 目前仍不存在 ⇒ `samples = 0`，
//! 线上恒为影子模式，**现在开与不开对候选顺序完全等价**（只多一次 SGD 与
//! 每 10 次落盘）。建议保持默认开以收集数据，但**不要调低 200 门槛**。
//! 生效前用 [`shadow_stats()`] 看 `(baseline, head, updates)`，或用
//! [`quality_gate()`] 直接查闸是否放行。

use std::path::PathBuf;
use std::sync::atomic::{AtomicI8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::engine::candidate::Candidate;
use crate::engine::context::Context;
use crate::engine::personalization::{self, PreferenceView};
use crate::engine::user_ngram;

/// 特征维数。顺序必须与 [`FEATURE_NAMES`] 一致。
pub const FEATURE_DIM: usize = 12;

/// 参数文件里的特征名（改动会让旧文件校验失败 ⇒ 回零，符合"回滚"预期）。
///
/// 全部读**现有统计**，学习头不自己维护任何计数：
/// * `uni/bi/tri` ← `user_ngram`（唯一一份 n-gram 表）；
/// * `pin_hit/pin_ttl` ← `personalization` 的 pin 表；
/// * `user_commits/userdb_w` ← 用户词典词条的 `commit_count` / `weight`
///   （`weight = ln(衰减后词频) + credibility`，见 `create_dict_entry`）。
pub const FEATURE_NAMES: [&str; FEATURE_DIM] = [
    "uni_ln", "bi_ln", "tri_ln", "pin_hit", "pin_ttl", "static10", "neural10", "len8", "user",
    "edit8", "user_commits", "userdb_w",
];

/// 影子模式门槛：样本数不到这个值只记录，不改变候选顺序。
pub const SHADOW_MIN_SAMPLES: u64 = 200;

/// 影子期质量闸：head top-1 至少比 baseline top-1 多命中这么多次才放行。
/// 200 个样本里要求 +10（≈ 5 个百分点）；离线回放的轮转噪声标签达不到。
pub const MIN_TOP1_GAIN: u64 = 10;

/// 单次 SGD 的参数增量上限（欧氏范数）。
pub const MAX_STEP_NORM: f64 = 0.01;

/// 默认学习率。样本是逐次提交的弱标签，不能大。
const DEFAULT_LR: f64 = 0.05;

/// L2 权重（只压参数，不影响特征）。
const L2: f64 = 1e-4;

/// 参数钳位，防止个别特征把排序拉爆（也是"可回退"的一部分）。
const WEIGHT_CLAMP: f64 = 2.0;

/// 每写几次样本落一次盘（按键路径不碰磁盘）。
const FLUSH_EVERY: u32 = 10;

/// v2 = 影子统计（baseline/head/loss/updates）一起进 checksum；v1 文件整份丢弃。
const VERSION: u32 = 2;

/// 神经侧的融合分在特征里的下标（其余特征在滤镜里已算好）。
const NEURAL_SLOT: usize = 6;

/// 一条候选组的学习轨迹：特征、字长、层级、以及可选的融合分。
///
/// `features` 按**组内下标**排列；`combined` 为空表示后台还没算完
/// （提交时再查一次 worker，查不到就跳过这次学习）。
#[derive(Clone)]
pub struct RerankTrace {
    /// 组内每条候选的文本（同时是 worker 结果表的键的一部分）。
    pub texts: Vec<String>,
    /// 组内每条候选的字符数（只允许同字长互调）。
    pub char_lens: Vec<u8>,
    /// 组内每条候选的层级分量（0 = 词典，1 = 用户词，2 = 整句）。
    pub tiers: Vec<u8>,
    /// 组内每条候选的 12 维特征（第 6 维留 0，用时由融合分填）。
    pub features: Vec<[f64; FEATURE_DIM]>,
    /// 组内每条候选的 λ 融合分；空 = 后台未就绪。
    pub combined: Vec<f64>,
    /// 神经 batch 的候选文本（worker 结果表的键；与 `texts` 可能不同）。
    pub worker_texts: Vec<String>,
    /// 每条头候选在神经 batch 里的下标；`None` = 用户层候选，模型没算它。
    pub worker_indices: Vec<Option<u16>>,
    /// 模型没算的候选用的静态分（nats），保证同组可比。
    pub fallback: Vec<f64>,
    /// 重排批次的上文（查 worker 结果表的键）。
    pub context: String,
}

impl std::fmt::Debug for RerankTrace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RerankTrace")
            .field("texts", &self.texts)
            .field("combined", &self.combined.len())
            .finish()
    }
}

/// 挂在候选上的引用：同一组共享一份 [`RerankTrace`]，只额外记组内下标。
#[derive(Clone, Debug)]
pub struct RerankRef {
    pub trace: Arc<RerankTrace>,
    pub index: u32,
}

/// 一次提交里"用户选中了哪条"的学习样本。
#[derive(Clone, Debug)]
pub struct RerankSample {
    pub trace: Arc<RerankTrace>,
    pub selected: usize,
}

/// 从候选上取样本；没有重排轨迹（头部关闭/未命中）就返回 `None`。
pub fn sample_from_candidate(candidate: &Candidate) -> Option<RerankSample> {
    let reference = candidate.rerank.as_ref()?;
    let selected = reference.index as usize;
    if selected >= reference.trace.features.len() {
        return None;
    }
    Some(RerankSample {
        trace: Arc::clone(&reference.trace),
        selected,
    })
}

/// 一批候选共用的特征上下文（锁/快照各取一次）。
pub struct FeatureInputs {
    before_last: Option<String>,
    prev: Option<String>,
    input: String,
    view: Option<PreferenceView>,
}

impl FeatureInputs {
    /// `include_personalization = false`（方案里关掉
    /// `translator/enable_personalization`）时不读 pin 表：个性化整层关掉，
    /// 学习头也必须看不到它，否则开关不是"完全回退"。
    pub fn from_context(ctx: &Context, include_personalization: bool) -> Self {
        let (before_last, prev) = user_ngram::last_two_words(&ctx.commit_history);
        Self {
            before_last,
            prev,
            input: ctx.composition.input().to_string(),
            view: include_personalization
                .then(personalization::snapshot_global)
                .flatten(),
        }
    }

    /// 计算一条候选的 12 维特征。
    ///
    /// 第 6 维（融合分）留 0，由 [`with_neural`] 在拿到模型分后补。
    /// `edit` 只在候选注释是 ASCII（通常是拼音）时才有意义；没有注释的
    /// 候选该维恒为 0 —— 不为此新增数据源。
    pub fn features(
        &self,
        text: &str,
        comment: &str,
        quality: f64,
        user_commits: i32,
        user_weight: f64,
    ) -> [f64; FEATURE_DIM] {
        let (unigram, bigram, trigram) = user_ngram::counts(
            self.before_last.as_deref(),
            self.prev.as_deref(),
            text,
        );
        let static_ln = (quality.rem_euclid(1e9).max(1.0)).ln() / 10.0;
        let (pin_hit, pin_ttl) = self
            .view
            .as_ref()
            .map(|view| {
                let fraction = view.pin_ttl_fraction(&self.input, text);
                ((fraction > 0.0) as u8 as f64, fraction)
            })
            .unwrap_or((0.0, 0.0));
        [
            (1.0 + unigram as f64).ln(),
            (1.0 + bigram as f64).ln(),
            (1.0 + trigram as f64).ln(),
            pin_hit,
            pin_ttl,
            static_ln,
            0.0,
            (text.chars().count() as f64) / 8.0,
            tier_of(quality) as f64,
            edit_distance(&self.input, comment) / 8.0,
            // 复用 userdb：原始上屏次数与「ln(衰减词频)+credibility」。
            (1.0 + user_commits.max(0) as f64).ln(),
            user_weight / 10.0,
        ]
    }
}

/// 把融合分填进第 6 维（`combined` 是 nats，按 10 归一）。
fn with_neural(mut features: [f64; FEATURE_DIM], combined: f64) -> [f64; FEATURE_DIM] {
    features[NEURAL_SLOT] = combined / 10.0;
    features
}

/// 候选的层级分量（与 `rescore_filter` 同一口径）。
fn tier_of(quality: f64) -> u8 {
    ((quality.rem_euclid(1e12) / 1e9) as u64).min(255) as u8
}

/// 编辑距离（只对 ASCII 注释生效，上限 8 后归一）。
fn edit_distance(input: &str, comment: &str) -> f64 {
    let comment = comment.trim();
    if comment.is_empty() || !comment.is_ascii() || input.is_empty() {
        return 0.0;
    }
    let a: Vec<char> = input.chars().collect();
    let b: Vec<char> = comment.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            current[j + 1] = (prev[j] + cost)
                .min(prev[j + 1] + 1)
                .min(current[j] + 1);
        }
        std::mem::swap(&mut prev, &mut current);
    }
    (prev[b.len()] as f64).min(8.0)
}

/// 进程级参数表。
struct HeadState {
    weights: [f64; FEATURE_DIM],
    lr: f64,
    samples: u64,
    /// 上次权重衰减的 UTC 天序号（0 = 还没跑过）。
    last_decay_day: i64,
    dirty: u32,
    /// 影子期质量统计（随 `samples` 一起落盘，重启不清零）。
    shadow_baseline_top1: u64,
    shadow_head_top1: u64,
    shadow_loss: f64,
    /// 累计了多少条 loss（`shadow_loss` 的平均分母）。
    shadow_updates: u64,
}

impl Default for HeadState {
    fn default() -> Self {
        Self {
            weights: [0.0; FEATURE_DIM],
            lr: DEFAULT_LR,
            samples: 0,
            last_decay_day: 0,
            dirty: 0,
            shadow_baseline_top1: 0,
            shadow_head_top1: 0,
            shadow_loss: 0.0,
            shadow_updates: 0,
        }
    }
}

impl HeadState {
    /// 组内线性分（融合分 + w·f）。
    fn group_scores(
        &self,
        trace: &RerankTrace,
        combined: &[f64],
    ) -> Vec<f64> {
        trace
            .features
            .iter()
            .enumerate()
            .map(|(i, features)| {
                // `max_paths` 之外的融合分是 NaN 哨兵：整条当负无穷，
                // 保证它们留在原段尾部、不参与调序。
                //
                // ⚠️ 不能写 `combined_score + dot(...)` 再靠 NaN 自然传播：
                // 权重初值全零时 `0.0 * -inf = NaN`，反而把"未打分"变成
                // 无法比较的项。这里直接短路。
                let combined_score = combined
                    .get(i)
                    .copied()
                    .filter(|value| value.is_finite())
                    .unwrap_or(f64::NEG_INFINITY);
                if !combined_score.is_finite() {
                    return f64::NEG_INFINITY;
                }
                combined_score + dot(&self.weights, &with_neural(*features, combined_score))
            })
            .collect()
    }

    /// 只在**连续的同字长、同层级段**内按线性分调序。
    fn reorder(
        &self,
        base: &[usize],
        trace: &RerankTrace,
        combined: &[f64],
        effective: bool,
    ) -> Vec<usize> {
        if !effective
            || base.len() < 2
            || trace.features.len() != trace.texts.len()
            || trace.char_lens.len() != trace.texts.len()
            || trace.tiers.len() != trace.texts.len()
        {
            return base.to_vec();
        }
        let scores = self.group_scores(trace, combined);
        let mut out = base.to_vec();
        let mut start = 0;
        while start < out.len() {
            let key = (trace.char_lens[out[start]], trace.tiers[out[start]]);
            let mut end = start + 1;
            while end < out.len()
                && (trace.char_lens[out[end]], trace.tiers[out[end]]) == key
            {
                end += 1;
            }
            out[start..end].sort_by(|&a, &b| {
                scores[b]
                    .partial_cmp(&scores[a])
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            start = end;
        }
        out
    }

    /// 一次 pairwise logistic SGD（选中 vs 同组其余）。
    ///
    /// 返回 `(loss, baseline_top1, head_top1)`；`None` = 样本不可用。
    fn update(
        &mut self,
        trace: &RerankTrace,
        combined: &[f64],
        selected: usize,
        day: i64,
    ) -> Option<(f64, usize, usize)> {
        let n = trace.texts.len();
        if n < 2 || trace.features.len() != n || selected >= n {
            return None;
        }
        // 选中项没被模型打分（`max_paths` 之外）就没有基线可比 —— 跳过，
        // 否则会把"没有神经证据"当成强反例去推参数。
        if !combined
            .get(selected)
            .copied()
            .is_some_and(f64::is_finite)
        {
            return None;
        }
        self.decay(day);
        let scores = self.group_scores(trace, combined);
        let max = scores.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let exps: Vec<f64> = scores.iter().map(|score| (score - max).exp()).collect();
        let sum: f64 = exps.iter().sum();
        if !(sum.is_finite() && sum > 0.0) {
            return None;
        }
        let probabilities: Vec<f64> = exps.iter().map(|value| value / sum).collect();
        let loss = -(probabilities[selected].max(1e-12)).ln();

        let mut gradient = [0.0f64; FEATURE_DIM];
        for (i, probability) in probabilities.iter().enumerate() {
            let target = if i == selected { 1.0 } else { 0.0 };
            let coefficient = target - probability;
            let combined_score = combined
                .get(i)
                .copied()
                .filter(|value| value.is_finite())
                .unwrap_or(0.0);
            let features = with_neural(trace.features[i], combined_score);
            for (dim, value) in features.iter().enumerate() {
                gradient[dim] += coefficient * value;
            }
        }
        for (dim, weight) in self.weights.iter().enumerate() {
            gradient[dim] -= L2 * weight;
        }

        let mut delta = [0.0f64; FEATURE_DIM];
        for (dim, value) in gradient.iter().enumerate() {
            delta[dim] = value * self.lr;
        }
        let norm = delta.iter().map(|value| value * value).sum::<f64>().sqrt();
        if norm > MAX_STEP_NORM {
            let scale = MAX_STEP_NORM / norm;
            for value in &mut delta {
                *value *= scale;
            }
        }
        for (weight, step) in self.weights.iter_mut().zip(delta) {
            *weight = (*weight + step).clamp(-WEIGHT_CLAMP, WEIGHT_CLAMP);
        }

        self.samples = self.samples.saturating_add(1);
        self.dirty = self.dirty.saturating_add(1);
        self.shadow_loss += loss;
        self.shadow_updates = self.shadow_updates.saturating_add(1);
        let baseline_top1 = argmax(combined);
        let head_top1 = argmax(&scores);
        if head_top1 == selected {
            self.shadow_head_top1 += 1;
        }
        if baseline_top1 == selected {
            self.shadow_baseline_top1 += 1;
        }
        if self.samples % 50 == 0 {
            let average = self.shadow_loss / self.shadow_updates.max(1) as f64;
            tracing::debug!(
                samples = self.samples,
                baseline_top1 = self.shadow_baseline_top1,
                head_top1 = self.shadow_head_top1,
                avg_loss = average,
                "rerank_head: 影子统计"
            );
        }
        Some((loss, baseline_top1, head_top1))
    }

    /// 每天 `w ← 0.99w`；`last_decay_day == 0` 时只记起点。
    fn decay(&mut self, day: i64) {
        if self.last_decay_day == 0 {
            self.last_decay_day = day;
            return;
        }
        if day > self.last_decay_day {
            let days = (day - self.last_decay_day) as i32;
            let factor = 0.99f64.powi(days);
            for weight in &mut self.weights {
                *weight *= factor;
            }
            self.last_decay_day = day;
        }
    }

    /// 参数文件内容（`checksum` 覆盖前面全部行）。
    fn payload(&self) -> String {
        let weights = self
            .weights
            .iter()
            .map(|weight| format!("{weight:.6}"))
            .collect::<Vec<_>>()
            .join(",");
        let base = format!(
            "version\t{VERSION}\nfeature_names\t{}\nweights\t{weights}\nlr\t{:.6}\nsamples\t{}\ndecay_day\t{}\nshadow_baseline\t{}\nshadow_head\t{}\nshadow_loss\t{:.6}\nshadow_updates\t{}\n",
            FEATURE_NAMES.join(","),
            self.lr,
            self.samples,
            self.last_decay_day,
            self.shadow_baseline_top1,
            self.shadow_head_top1,
            self.shadow_loss,
            self.shadow_updates,
        );
        let checksum = fnv1a(base.as_bytes());
        format!("{base}checksum\t{checksum:016x}\n")
    }

    /// 读参数文件：任何一项不合法（含校验和）⇒ 整份丢弃回全零。
    fn load(path: &std::path::Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        Self::parse(&text).unwrap_or_default()
    }

    fn parse(text: &str) -> Option<Self> {
        let mut version = None;
        let mut names = None;
        let mut weights = None;
        let mut lr = None;
        let mut samples = None;
        let mut decay_day = None;
        let mut shadow_baseline = None;
        let mut shadow_head = None;
        let mut shadow_loss = None;
        let mut shadow_updates = None;
        let mut checksum = None;
        for line in text.lines() {
            let (key, value) = line.split_once('\t')?;
            match key {
                "version" => version = value.parse::<u32>().ok(),
                "feature_names" => names = Some(value.to_string()),
                "weights" => {
                    let parsed: Vec<f64> = value
                        .split(',')
                        .map(|item| item.parse::<f64>().ok())
                        .collect::<Option<Vec<_>>>()?;
                    weights = Some(parsed);
                }
                "lr" => lr = value.parse::<f64>().ok(),
                "samples" => samples = value.parse::<u64>().ok(),
                "decay_day" => decay_day = value.parse::<i64>().ok(),
                "shadow_baseline" => shadow_baseline = value.parse::<u64>().ok(),
                "shadow_head" => shadow_head = value.parse::<u64>().ok(),
                "shadow_loss" => shadow_loss = value.parse::<f64>().ok(),
                "shadow_updates" => shadow_updates = value.parse::<u64>().ok(),
                "checksum" => checksum = u64::from_str_radix(value, 16).ok(),
                _ => {}
            }
        }
        if version? != VERSION
            || names.as_deref() != Some(FEATURE_NAMES.join(",").as_str())
        {
            return None;
        }
        let weights = weights?;
        if weights.len() != FEATURE_DIM || !weights.iter().all(|value| value.is_finite()) {
            return None;
        }
        let lr = lr?;
        if !(lr.is_finite() && lr > 0.0) {
            return None;
        }
        let samples = samples?;
        let decay_day = decay_day?;
        // v2 新增四行：缺省按 0 处理，旧 v1 文件因 version 不符已在上面丢弃。
        let sb = shadow_baseline.unwrap_or(0);
        let sh = shadow_head.unwrap_or(0);
        let sl = shadow_loss.unwrap_or(0.0);
        let su = shadow_updates.unwrap_or(0);
        if !sl.is_finite() {
            return None;
        }
        let base = format!(
            "version\t{VERSION}\nfeature_names\t{}\nweights\t{}\nlr\t{lr:.6}\nsamples\t{samples}\ndecay_day\t{decay_day}\nshadow_baseline\t{sb}\nshadow_head\t{sh}\nshadow_loss\t{sl:.6}\nshadow_updates\t{su}\n",
            FEATURE_NAMES.join(","),
            weights
                .iter()
                .map(|weight| format!("{weight:.6}"))
                .collect::<Vec<_>>()
                .join(","),
        );
        if fnv1a(base.as_bytes()) != checksum? {
            return None;
        }
        Some(Self {
            weights: weights.try_into().ok()?,
            lr,
            samples,
            last_decay_day: decay_day,
            dirty: 0,
            shadow_baseline_top1: sb,
            shadow_head_top1: sh,
            shadow_loss: sl,
            shadow_updates: su,
        })
    }
}

fn dot(left: &[f64; FEATURE_DIM], right: &[f64; FEATURE_DIM]) -> f64 {
    left.iter()
        .zip(right.iter())
        .map(|(a, b)| a * b)
        .sum()
}

/// 最大值的下标；跳过非有限项（`max_paths` 之外的 NaN 哨兵不参与）。
fn argmax(values: &[f64]) -> usize {
    let mut best: Option<usize> = None;
    for (index, value) in values.iter().enumerate() {
        if !value.is_finite() {
            continue;
        }
        match best {
            Some(current) if *value <= values[current] => {}
            _ => best = Some(index),
        }
    }
    best.unwrap_or(0)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

// ------------------------------------------------------------------ 进程级入口

fn global() -> &'static Mutex<HeadState> {
    static GLOBAL: OnceLock<Mutex<HeadState>> = OnceLock::new();
    GLOBAL.get_or_init(|| Mutex::new(HeadState::load(&store_path())))
}

/// 参数文件位置（`%APPDATA%\Rime-Next\rerank_head.tsv`）。
///
/// 测试构建一律落临时目录，绝不碰真实用户数据。
pub fn store_path() -> PathBuf {
    #[cfg(test)]
    {
        return std::env::temp_dir()
            .join("Rime-Next-test")
            .join(format!("rerank_head-{}.tsv", std::process::id()));
    }
    #[cfg(not(test))]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("Rime-Next")
            .join("rerank_head.tsv")
    }
}

fn env_off() -> bool {
    static OFF: OnceLock<bool> = OnceLock::new();
    *OFF.get_or_init(|| {
        matches!(
            std::env::var("WEASEL_RERANK_HEAD").ok().as_deref(),
            Some("off") | Some("0") | Some("false")
        )
    })
}

/// 进程级覆盖：0 = 未设置，1 = 强制开，-1 = 强制关。
static OVERRIDE: AtomicI8 = AtomicI8::new(0);

/// 进程级覆盖（设置页/宿主可调）：`Some(false)` 完全回退。
pub fn set_enabled_override(enabled: Option<bool>) {
    OVERRIDE.store(
        match enabled {
            Some(true) => 1,
            Some(false) => -1,
            None => 0,
        },
        Ordering::Relaxed,
    );
}

fn override_value() -> Option<bool> {
    match OVERRIDE.load(Ordering::Relaxed) {
        1 => Some(true),
        -1 => Some(false),
        _ => None,
    }
}

/// 开关：环境变量关掉即关；否则看进程级 override；默认开（影子模式）。
pub fn enabled() -> bool {
    if env_off() {
        return false;
    }
    override_value().unwrap_or(true)
}

/// 是否真正影响候选顺序：开关打开 + 样本够 + 影子期证明「加了头更准」。
///
/// 2026-11-10：只看样本数会让第 200 次提交带着 200 步权重立刻改序，
/// 噪声标签也照放。现在多一道 [`quality_gate()`]。
pub fn effective() -> bool {
    enabled() && quality_gate()
}

/// 影子期质量闸（只读，不含开关，便于诊断）。
pub fn quality_gate() -> bool {
    global()
        .lock()
        .map(|state| quality_of(&state))
        .unwrap_or(false)
}

/// 纯函数，方便单测直接构造状态，不碰进程级单例。
fn quality_of(state: &HeadState) -> bool {
    if state.samples < SHADOW_MIN_SAMPLES || state.shadow_updates < SHADOW_MIN_SAMPLES {
        return false;
    }
    state.shadow_head_top1
        >= state
            .shadow_baseline_top1
            .saturating_add(MIN_TOP1_GAIN)
}

/// 影子统计 `(baseline top-1, head top-1, 可比较样本数)`，给设置页/诊断看。
pub fn shadow_stats() -> (u64, u64, u64) {
    global()
        .lock()
        .map(|state| {
            (
                state.shadow_baseline_top1,
                state.shadow_head_top1,
                state.shadow_updates,
            )
        })
        .unwrap_or((0, 0, 0))
}

/// 已积累的样本数（诊断用）。
pub fn samples() -> u64 {
    global()
        .lock()
        .map(|state| state.samples)
        .unwrap_or(0)
}

/// 方案配置：`translator/enable_rerank_head`，缺省 true（默认影子模式）。
pub fn scheme_enabled(config: &serde_json::Value) -> bool {
    config
        .pointer("/translator/enable_rerank_head")
        .and_then(|value| {
            value
                .as_bool()
                .or_else(|| value.as_i64().map(|number| number != 0))
        })
        .unwrap_or(true)
}

/// 影子模式也要能算分：只读权重快照。
fn weights_snapshot() -> [f64; FEATURE_DIM] {
    global()
        .lock()
        .map(|state| state.weights)
        .unwrap_or([0.0; FEATURE_DIM])
}

/// 在既有重排顺序上叠加线性头（不满足条件时原样返回）。
pub fn choose_order(
    base: &[usize],
    trace: &RerankTrace,
    combined: &[f64],
) -> Vec<usize> {
    if !effective() {
        return base.to_vec();
    }
    // 用一个只带权重的临时状态，避免持锁做排序。
    let state = HeadState {
        weights: weights_snapshot(),
        ..HeadState::default()
    };
    state.reorder(base, trace, combined, true)
}

/// 提交时喂样本：逐条更新（影子阶段也更新，这样样本数才会涨到门槛）。
pub fn observe_samples(samples: &[RerankSample]) {
    if !enabled() || samples.is_empty() {
        return;
    }
    let day = personalization::now_secs() / 86400;
    let mut pending_write: Option<String> = None;
    {
        let Ok(mut state) = global().lock() else {
            return;
        };
        for sample in samples {
            let trace = &sample.trace;
            let n = trace.texts.len();
            // 后台分还没落地：提交时再查一次结果表（例如首次 compose 立刻选词）。
            let combined = if trace.combined.len() == n {
                trace.combined.clone()
            } else {
                match crate::gear::filter::rescore::scores_for(
                    &trace.context,
                    &trace.worker_texts,
                ) {
                    Some(scores) if scores.len() == trace.worker_texts.len() => {
                        // 头组比神经 batch 多出用户层候选：模型没算的用静态分补，
                        // 保证同组比较时每一维都有值（用户词特征靠这里才有样本）。
                        trace
                            .worker_indices
                            .iter()
                            .enumerate()
                            .map(|(pos, worker_index)| {
                                worker_index
                                    .and_then(|index| scores.get(index as usize).copied())
                                    .filter(|value| value.is_finite())
                                    .unwrap_or_else(|| {
                                        trace.fallback.get(pos).copied().unwrap_or(0.0)
                                    })
                            })
                            .collect()
                    }
                    _ => continue,
                }
            };
            let _ = state.update(trace, &combined, sample.selected, day);
        }
        if state.dirty >= FLUSH_EVERY {
            state.dirty = 0;
            pending_write = Some(state.payload());
        }
    }
    // 锁外写盘。
    if let Some(payload) = pending_write {
        let _ = user_ngram::write_atomic(&store_path(), &payload);
    }
}

/// 退出/finalize：同步落一次（失败不重试，最坏丢最近一批）。
pub fn flush_on_exit() {
    let payload = {
        let Ok(mut state) = global().lock() else {
            return;
        };
        if state.dirty == 0 {
            return;
        }
        state.dirty = 0;
        state.payload()
    };
    let _ = user_ngram::write_atomic(&store_path(), &payload);
}

/// 设置页"重置学习"入口：回全零并落盘。
pub fn reset() {
    {
        let Ok(mut state) = global().lock() else {
            return;
        };
        *state = HeadState::default();
    }
    let _ = std::fs::remove_file(store_path());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace(texts: &[&str], lens: &[u8], tiers: &[u8]) -> RerankTrace {
        RerankTrace {
            texts: texts.iter().map(|text| text.to_string()).collect(),
            char_lens: lens.to_vec(),
            tiers: tiers.to_vec(),
            features: texts
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    let mut features = [0.0; FEATURE_DIM];
                    features[0] = index as f64;
                    features
                })
                .collect(),
            combined: vec![0.0; texts.len()],
            worker_texts: texts.iter().map(|text| text.to_string()).collect(),
            worker_indices: (0..texts.len()).map(|index| Some(index as u16)).collect(),
            fallback: vec![0.0; texts.len()],
            context: String::new(),
        }
    }

    #[test]
    fn zero_weights_keep_base_order() {
        let trace = trace(&["a", "b", "c"], &[2, 2, 2], &[0, 0, 0]);
        let state = HeadState::default();
        let base = vec![0, 1, 2];
        // 即便强行开 effective，零权重也只能给出原顺序。
        assert_eq!(state.reorder(&base, &trace, &[3.0, 2.0, 1.0], true), base);
    }

    #[test]
    fn shadow_mode_never_reorders() {
        let trace = trace(&["a", "b"], &[2, 2], &[0, 0]);
        let mut state = HeadState::default();
        state.weights[0] = 2.0;
        assert_eq!(state.reorder(&[0, 1], &trace, &[-1.0, -2.0], false), vec![0, 1]);
        // 更新只涨样本数，不改顺序。
        let _ = state.update(&trace, &[0.0, 0.0], 1, 100);
        assert_eq!(state.samples, 1);
        assert_eq!(state.reorder(&[0, 1], &trace, &[0.0, 0.0], false), vec![0, 1]);
    }

    #[test]
    fn step_is_clipped_to_norm() {
        let mut trace = trace(&["a", "b", "c"], &[2, 2, 2], &[0, 0, 0]);
        trace.features[0][0] = 1.0;
        let mut state = HeadState::default();
        state.lr = 10.0;
        let before = state.weights;
        let _ = state.update(&trace, &[0.0, 0.0, 0.0], 0, 100);
        let delta: Vec<f64> = state
            .weights
            .iter()
            .zip(before.iter())
            .map(|(after, before)| after - before)
            .collect();
        let norm = delta.iter().map(|value| value * value).sum::<f64>().sqrt();
        assert!(norm <= MAX_STEP_NORM + 1e-12, "单步范数超限: {norm}");
        assert!(norm > 0.0, "应当有参数更新");
    }

    #[test]
    fn weights_decay_by_day() {
        let mut state = HeadState::default();
        state.weights = [1.0; FEATURE_DIM];
        state.last_decay_day = 100;
        state.decay(102);
        let expected = 0.99f64.powi(2);
        assert!((state.weights[0] - expected).abs() < 1e-12);
    }

    #[test]
    fn reorder_never_crosses_length_or_tier() {
        let mut mixed_trace = trace(&["a", "b", "c", "d"], &[2, 2, 3, 3], &[0, 0, 0, 0]);
        // 让后两条的线性分更高：也不允许跨字长。
        mixed_trace.features[2][0] = 10.0;
        mixed_trace.features[3][0] = 10.0;
        let mut state = HeadState::default();
        state.weights[0] = 1.0;
        let order = state.reorder(&[0, 1, 2, 3], &mixed_trace, &[0.0; 4], true);
        let mut first_run = order[..2].to_vec();
        first_run.sort_unstable();
        assert_eq!(first_run, vec![0, 1], "同字长段内才允许调序");
        assert_eq!(&order[2..], &[2, 3], "高分的 3 字候选不能插进 2 字段");
        // 用户词层单独一段，永不与 0 层互调。
        let mut user_trace = trace(&["a", "b"], &[2, 2], &[0, 1]);
        user_trace.features[1][0] = 10.0;
        let order = state.reorder(&[0, 1], &user_trace, &[0.0, 0.0], true);
        assert_eq!(order, vec![0, 1]);
    }

    #[test]
    fn unscored_tail_is_never_compared() {
        let mut head_trace = trace(&["a", "b", "c"], &[2, 2, 2], &[0, 0, 0]);
        // 尾部（`max_paths` 之外）线性分很高，也不许越过有神经分的候选。
        head_trace.features[2][0] = 99.0;
        let mut state = HeadState::default();
        state.weights[0] = 2.0;
        let combined = [1.0, 0.0, f64::NAN];
        let order = state.reorder(&[0, 1, 2], &head_trace, &combined, true);
        assert_eq!(order.last(), Some(&2), "NaN 尾部必须留在最后");
        // 选中尾部（没有神经分）⇒ 这条样本不可用。
        assert!(state.update(&head_trace, &combined, 2, 100).is_none());
        assert_eq!(state.samples, 0);
    }

    #[test]
    fn user_tier_trains_but_never_reorders() {
        let mut user_trace = trace(&["system", "user"], &[2, 2], &[0, 1]);
        user_trace.features[1][0] = 5.0;
        let mut state = HeadState::default();
        state.weights[0] = 1.0;
        // 用户层不参与调序：结构序（用户词优先）不能被线性头推翻。
        assert_eq!(
            state.reorder(&[0, 1], &user_trace, &[0.0, 0.0], true),
            vec![0, 1]
        );
        // 但用户层必须能产生训练样本，否则第 9 维永远没有数据。
        assert!(state.update(&user_trace, &[0.0, 0.0], 1, 100).is_some());
        assert_eq!(state.samples, 1);
    }

    #[test]
    fn corrupt_file_falls_back_to_zero() {
        let path = std::env::temp_dir().join(format!(
            "rerank_head-corrupt-{}.tsv",
            std::process::id()
        ));
        std::fs::write(&path, "version\t1\nweights\tNaN\n").unwrap();
        let state = HeadState::load(&path);
        assert_eq!(state.weights, [0.0; FEATURE_DIM]);
        assert_eq!(state.samples, 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn roundtrip_keeps_weights_and_samples() {
        let path = std::env::temp_dir().join(format!(
            "rerank_head-roundtrip-{}.tsv",
            std::process::id()
        ));
        let mut state = HeadState::default();
        state.weights[3] = 0.123456;
        state.samples = 321;
        state.last_decay_day = 20_000;
        state.shadow_baseline_top1 = 88;
        state.shadow_head_top1 = 121;
        state.shadow_loss = 12.5;
        state.shadow_updates = 321;
        std::fs::write(&path, state.payload()).unwrap();
        let loaded = HeadState::load(&path);
        assert_eq!(loaded.samples, 321);
        assert!((loaded.weights[3] - 0.123456).abs() < 1e-6);
        assert_eq!(loaded.last_decay_day, 20_000);
        assert_eq!(loaded.shadow_baseline_top1, 88);
        assert_eq!(loaded.shadow_head_top1, 121);
        assert_eq!(loaded.shadow_updates, 321);
        assert!((loaded.shadow_loss - 12.5).abs() < 1e-6);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn shadow_stats_survive_roundtrip() {
        let mut state = HeadState::default();
        state.samples = 240;
        state.shadow_updates = 240;
        state.shadow_baseline_top1 = 90;
        state.shadow_head_top1 = 140;
        state.shadow_loss = 12.5;
        let loaded = HeadState::parse(&state.payload()).expect("v2 参数文件必须可解析");
        assert_eq!(
            (
                loaded.shadow_baseline_top1,
                loaded.shadow_head_top1,
                loaded.shadow_updates
            ),
            (90, 140, 240)
        );
        assert!((loaded.shadow_loss - 12.5).abs() < 1e-6);
        assert!(quality_of(&loaded), "落盘重启后质量闸仍应放行");
    }

    #[test]
    fn quality_gate_blocks_noisy_or_short_history() {
        let mut state = HeadState::default();
        state.samples = SHADOW_MIN_SAMPLES;
        state.shadow_updates = SHADOW_MIN_SAMPLES;
        state.shadow_baseline_top1 = 120;
        state.shadow_head_top1 = 125; // 只多 5 次，不到 +10
        assert!(!quality_of(&state), "弱提升不得放行");

        state.shadow_head_top1 = 120 + MIN_TOP1_GAIN - 1;
        assert!(!quality_of(&state), "差一个也不放行");

        state.samples = SHADOW_MIN_SAMPLES - 1;
        state.shadow_updates = SHADOW_MIN_SAMPLES - 1;
        state.shadow_head_top1 = 200;
        assert!(!quality_of(&state), "样本不够不得放行");
    }

    #[test]
    fn quality_gate_allows_clear_improvement() {
        let mut state = HeadState::default();
        state.samples = SHADOW_MIN_SAMPLES;
        state.shadow_updates = SHADOW_MIN_SAMPLES;
        // 离线回放里「可学标签」生效期 head 400/400，噪声标签只有 65/400。
        state.shadow_baseline_top1 = 90;
        state.shadow_head_top1 = 140;
        assert!(quality_of(&state));
    }

    #[test]
    fn checksum_mismatch_discards_file() {
        let path = std::env::temp_dir().join(format!(
            "rerank_head-tamper-{}.tsv",
            std::process::id()
        ));
        let mut state = HeadState::default();
        state.weights[0] = 0.5;
        let tampered = state.payload().replace("0.500000", "0.900000");
        std::fs::write(&path, tampered).unwrap();
        let loaded = HeadState::load(&path);
        assert_eq!(loaded.weights, [0.0; FEATURE_DIM]);
        let _ = std::fs::remove_file(path);
    }

    /// 离线回放（**合成标签**，不是实盘数据）：量化「影子 → 生效」的边界行为。
    ///
    /// ⚠️ 真实 `%APPDATA%\Rime-Next\rerank_head.tsv` 目前不存在 ⇒ `samples = 0`，
    /// 线上恒为影子模式。这里的数字只用来回答两件事：
    /// 1. 影子期（< 200 样本）是否真的零影响；
    /// 2. 生效期（≥ 200）在「可学 / 不可学」两种标签下，顺序会不会被搞坏。
    /// **不能**当作上线收益的依据。用 `--nocapture` 看表。
    #[test]
    fn offline_replay_shadow_vs_effective() {
        const N: usize = 6;
        const TOTAL: usize = 600;

        // 固定 xorshift，保证每次运行输出完全一致。
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut rand = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            // `seed >> 40` 是 24 位，必须除以 2^24 才是 [-0.5, 0.5)。
            ((seed >> 40) as f64) / 16_777_216.0 - 0.5
        };
        let features: Vec<[f64; FEATURE_DIM]> = (0..N)
            .map(|_| {
                let mut f = [0.0f64; FEATURE_DIM];
                for value in &mut f {
                    *value = rand();
                }
                f
            })
            .collect();
        // 基线：combined 严格递减 ⇒ 基线 top1 恒为 0。
        let combined: Vec<f64> = (0..N).map(|i| (N - i) as f64 * 0.3).collect();
        let trace = RerankTrace {
            texts: (0..N).map(|i| format!("c{i}")).collect(),
            char_lens: vec![2; N],
            tiers: vec![0; N],
            features: features.clone(),
            combined: combined.clone(),
            worker_texts: (0..N).map(|i| format!("c{i}")).collect(),
            worker_indices: (0..N).map(|i| Some(i as u16)).collect(),
            fallback: vec![0.0; N],
            context: String::new(),
        };
        // 造一个「基线 top1=0、真实偏好=3」的**非平凡**场景：只把第 3 条
        // 候选的第 0/10 维抬高。若标签恰好等于基线 top1，回放会得出
        // 「400/400 命中」这种平凡结论，测不出学习头是否真的能纠偏。
        let mut features = features;
        for (i, f) in features.iter_mut().enumerate() {
            if i == 3 {
                f[0] = 1.0;
                f[10] = 1.0;
            } else {
                f[0] = 0.0;
                f[10] = 0.0;
            }
        }
        let learnable = argmax(
            &features
                .iter()
                .map(|f| 1.5 * f[0] + 0.8 * f[10] - 0.2 * f[5])
                .collect::<Vec<_>>(),
        );
        assert_eq!(learnable, 3, "可学标签必须与基线 top1 不同，否则回放无意义");

        println!("replay: baseline_top1=0 learnable={learnable} min_samples={SHADOW_MIN_SAMPLES}");
        for label in ["learnable", "random"] {
            let base: Vec<usize> = (0..N).collect();
            let mut state = HeadState::default();
            let mut shadow_changed = 0usize;
            let mut hit = 0usize;
            let mut rank_sum = 0usize;
            let mut abandoned = 0usize;
            let mut effective_steps = 0usize;
            println!("  scenario={label}");
            for step in 0..TOTAL {
                // random：确定性轮转标签，等价于「没有稳定偏好」的噪声。
                let selector = if label == "learnable" {
                    learnable
                } else {
                    (step * 7 + 3) % N
                };
                let effective = state.samples >= SHADOW_MIN_SAMPLES;
                let order = state.reorder(&base, &trace, &combined, effective);
                if !effective {
                    if order != base {
                        shadow_changed += 1;
                    }
                } else {
                    effective_steps += 1;
                    let rank = order.iter().position(|&i| i == selector).unwrap_or(N);
                    rank_sum += rank;
                    if rank == 0 {
                        hit += 1;
                    }
                    // 放弃率：选中项被挤到后 3 位（6 条候选）就算放弃。
                    if rank >= 3 {
                        abandoned += 1;
                    }
                }
                let _ = state.update(&trace, &combined, selector, 20_000);
                if (step + 1) % 100 == 0 {
                    let phase = if effective { "effective" } else { "shadow   " };
                    println!(
                        "    step={:3} samples={:3} {phase} w0={:+.4} w10={:+.4} max|w|={:.4}",
                        step + 1,
                        state.samples,
                        state.weights[0],
                        state.weights[10],
                        state.weights.iter().map(|w| w.abs()).fold(0.0, f64::max),
                    );
                }
            }
            let avg_rank = rank_sum as f64 / effective_steps.max(1) as f64;
            println!(
                "    => shadow_changed={shadow_changed} effective_steps={effective_steps} \
                 head_top1={hit}/{effective_steps} avg_selected_rank={avg_rank:.2} \
                 abandoned={abandoned}/{effective_steps}"
            );
            let weights = state
                .weights
                .iter()
                .map(|w| format!("{w:+.3}"))
                .collect::<Vec<_>>()
                .join(" ");
            println!("    final_w=[{weights}]");
            assert_eq!(shadow_changed, 0, "影子期绝不允许改顺序");
            assert_eq!(effective_steps, TOTAL - SHADOW_MIN_SAMPLES as usize);
        }
    }
}
