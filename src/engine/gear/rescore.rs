//! 神经整句重排——异步后台线程(移植自 msime `neural/rescore_worker.cpp`)。
//!
//! # 设计(与 C++ 逐条对齐)
//!
//! 给 N 条候选打一次神经分要几十毫秒——比一次按键的预算大一个数量级,
//! 所以它**不能长在按键路径上**。查询照旧同步返回静态顺序,同时把这一批
//! 候选交给后台线程;线程算完把顺序存进结果表;调用方下次查询时取。
//!
//! "最新的赢":用户还在打字时,前几次请求的结果已经没人要,新请求直接
//! 盖掉旧待办(待办只有一个槽)。
//!
//! "正在算的那一批不再排第二份":结果表按 (模型, 前文, 候选列表) 做键。
//!
//! # 第二种活:续写预测
//!
//! 同一个线程还接预测任务(见 [`crate::engine::gear::neural::predict_filter`])。
//! 预测是**逐字自回归**,一步一次 forward,比整批打分更慢,更不能上按键路径 ——
//! 但两者抢的是同一个后台槽:预测只在候选稀薄时被提交,频率远低于重排,
//! 排在重排后面等一拍无所谓。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use super::neural::{
    predict_extensions, rerank_scored, LazySentenceModel, PredictOptions, RerankOptions,
    SentenceModel,
};

/// 结果表只是给「同一次输入被重查一遍」兜底的,不是长期缓存:装满就整张丢掉,
/// 比维护 LRU 简单,代价也只是重算一次。
const MAX_RESULTS: usize = 128;

/// 一批待重排的任务。
struct Job {
    key: String,
    context: String,
    sentences: Vec<String>,
    static_scores: Vec<f64>,
    options: RerankOptions,
}

/// 一批待续写的任务。
struct PredictJob {
    key: String,
    context: String,
    /// 要往后接的候选文本(通常是当前首候选)。
    prefixes: Vec<String>,
    options: PredictOptions,
}

/// 后台线程的一件活。
enum Task {
    Rerank(Job),
    Predict(PredictJob),
}

/// 一批重排结果：顺序 + 每条的 λ 融合分（线性重排头的特征/基线）。
struct RerankResult {
    order: Vec<usize>,
    combined: Vec<f64>,
}

/// 一次后台落地的「内容签名」——抄 msime `ApplyCandidateTranslations` 的
/// `glosses_changed` / `IsTranslationCurrent`：异步结果回来后先逐条比现值，
/// **全都没变就不推帧**。
///
/// 本仓的对应物是：worker 每落地一批就把「这次到底改了什么」记在
/// [`RescoreWorker::last_landed`]；引擎侧 `poll_rescore_landing` 在 re-compose
/// 前比对，内容一致就跳过一次整段重译。重排看 `order`(下标序列)，预测看
/// 文本序列——只要落到屏幕上的东西和现在一样，这次 compose 就是纯浪费。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LandingSignature {
    /// 重排结果落地：按这个下标序列调序（`identity` = 与静态顺序一致）。
    Rerank { order: Vec<usize> },
    /// 预测结果落地：插入这些文本（空 = 没有新候选）。
    Predict { texts: Vec<String> },
}

impl LandingSignature {
    /// 与屏幕现值逐条比对：一致 ⇒ 不值得重绘。
    ///
    /// * 重排：`order` 与已应用顺序相同 ⇒ 调序后候选列表一模一样；
    ///   特别地，恒等 order 对任何已应用顺序都代表「没动」（`matches_any` 处理）。
    /// * 预测：`texts` 与已插入的预测条相同 ⇒ 再 compose 也插不出新东西。
    pub fn changes_from(&self, applied: Option<&LandingSignature>) -> bool {
        match (self, applied) {
            (LandingSignature::Rerank { order }, Some(LandingSignature::Rerank { order: old })) => {
                order != old
            }
            // 恒等 order 与「还没应用过任何重排」在视觉上等价：静态顺序本来就在屏幕上。
            (LandingSignature::Rerank { order }, None) => !Self::is_identity(order),
            (LandingSignature::Predict { texts }, Some(LandingSignature::Predict { texts: old })) => {
                texts != old
            }
            (LandingSignature::Predict { texts }, None) => !texts.is_empty(),
            // 类别不同（重排 ↔ 预测）⇒ 一定要重新 compose，让对应滤镜消费这一批。
            _ => true,
        }
    }

    /// `order == [0,1,2,…]`：神经认为静态顺序已最优，落地后候选不变。
    pub fn is_identity(order: &[usize]) -> bool {
        order.iter().enumerate().all(|(i, &v)| i == v)
    }
}

#[derive(Default)]
struct WorkerState {
    /// 已完成的重排结果:`key → 顺序 + 融合分`。
    done: HashMap<String, RerankResult>,
    /// 已完成的预测结果:`key → (完整文本, 平均对数概率)`。
    done_predict: HashMap<String, Vec<(String, f64)>>,
    /// 待办**按任务类型分槽**(抄 msime `RescoreWorker::pending_` 的「每任务类型各
    /// 留一格」)：重排 / 预测各一格,同类新的盖旧的,跨类不互挤。旧的单 `pending`
    /// 会让后提交的一批把先提交的那批直接挤掉——预测任务频率低,常被重排挤没。
    pending_rerank: Option<Job>,
    pending_predict: Option<PredictJob>,
    /// **正在算**的 key(从待办取出后到落地前)。抄 msime `active_keys_`：
    /// 这段时间结果还没进 `done`,若同 key 再来一批会重复排队、白算一遍。
    /// 单后台线程 ⇒ 同时最多一个,用 Option 足够。
    active_rerank: Option<String>,
    active_predict: Option<String>,
    /// 停工标志。
    stopping: bool,
}

impl WorkerState {
    /// 重排任务是否值得排队。纯判定,便于单测覆盖「已完成 / 正在算 / 已在待办」。
    fn should_queue_rerank(&self, key: &str) -> bool {
        !self.stopping
            && !self.done.contains_key(key)
            && self.active_rerank.as_deref() != Some(key)
            && self.pending_rerank.as_ref().is_none_or(|job| job.key != key)
    }

    /// 预测任务是否值得排队。与重排同构,只查自己的表 / 槽 / active。
    fn should_queue_predict(&self, key: &str) -> bool {
        !self.stopping
            && !self.done_predict.contains_key(key)
            && self.active_predict.as_deref() != Some(key)
            && self
                .pending_predict
                .as_ref()
                .is_none_or(|job| job.key != key)
    }

    /// 从两槽取下一件活:重排优先(当前候选顺序更贴按键反馈),其次预测。
    /// 取走时立刻记进 active,落地时清除。两槽都空返回 `None` ⇒ 调用方睡等待。
    fn take_next(&mut self) -> Option<Task> {
        if let Some(job) = self.pending_rerank.take() {
            self.active_rerank = Some(job.key.clone());
            return Some(Task::Rerank(job));
        }
        if let Some(job) = self.pending_predict.take() {
            self.active_predict = Some(job.key.clone());
            return Some(Task::Predict(job));
        }
        None
    }
}

/// 异步重排后台线程。进程级单例——见 [`global`] / [`init_from_data_roots`]。
///
/// 通过 [`RescoreWorker::new_singleton`] 构造成 `Arc`:线程持有 `Weak`
/// 句柄,每轮升级——单例常驻时升级恒成功;测试里 worker 被丢弃后线程自行退出。
pub struct RescoreWorker {
    state: Mutex<WorkerState>,
    /// 无活时睡在这里;`submit`/`shutdown` 唤醒。
    wake: Condvar,
    scorer: Arc<NeuralScorer>,
    /// 惰性起线程:没开神经重排的用户不会平白多线程。
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// 线程的存活句柄(构造后立即填入)。
    me: std::sync::OnceLock<std::sync::Weak<RescoreWorker>>,
    /// 结果落地代数:后台线程每写一条结果自增一次。调用方(引擎线程)记着
    /// 上次见过的代数,代数变了就 re-compose 一次——这就是"后台分算完,
    /// 候选列表当场跳到神经顺序"的信号通道(线程不能直接摸引擎)。
    generation: std::sync::atomic::AtomicU64,
    /// 最近一次落地的内容签名(与 `generation` 同锁序发布)。引擎侧据此判断
    /// 「代数虽然变了,但落到屏幕上的东西没变」⇒ 跳过一次无谓的 re-compose。
    last_landed: Mutex<Option<LandingSignature>>,
}

/// 神经打分器：双模型 + 两套选项。`SentenceModel` 自身线程安全
/// （内部 prefix 缓存有锁），所以一个打分器跨线程共享。
///
/// # 双模型（2026-11-08 用户口径）
///
/// 同时启用两个大小不同的模型：
///
/// * key —— 小号键盘模型（`model/sentence-model.safetensors`，6 层 4.5MB）。
///   提交路径要快，所以 `score_text` 用它；打字预测它是主力。
/// * desk —— 大号桌面模型（`model/sentence-model-desktop.safetensors`，8 层 25MB）。
///   质量更好但一轮几十到几百毫秒，只放在**异步**重排上（不挡按键）；预测也掺一半。
///
/// 只有 desk、没有 key 时退化成单模型（与旧版一致）：desk 装进 key 槽，
/// `desk_model` 留空，重排/打分/预测全走它。
pub struct NeuralScorer {
    /// 小号键盘模型（主模型）。单模型退化时这里装的可能是 desktop。
    key_model: Option<LazySentenceModel>,
    /// 大号桌面模型；`None` = 数据根里没有，重排回退 key。
    desk_model: Option<LazySentenceModel>,
    pub options: RerankOptions,
    /// 续写预测的参数(与重排各管一套:一个决定"排哪条",一个决定"续什么")。
    pub predict_options: PredictOptions,
}

impl NeuralScorer {
    /// 单模型构造（旧行为）：一个模型管打分/重排/预测，`desk` 槽留空。
    pub fn new(model_path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            key_model: Some(LazySentenceModel::new(model_path)),
            desk_model: None,
            options: RerankOptions::default(),
            predict_options: PredictOptions::default(),
        }
    }

    /// 双模型构造：key/desk 各自可以为 `None`（缺哪个就少哪条腿）。
    fn from_paths(
        key: Option<std::path::PathBuf>,
        desk: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            key_model: key.map(LazySentenceModel::new),
            desk_model: desk.map(LazySentenceModel::new),
            options: RerankOptions::default(),
            predict_options: PredictOptions::default(),
        }
    }

    /// 打分用的模型：key 优先；key 缺失时退到 desk（只有 desk 的单模型退化）。
    fn score_model(&self) -> Option<Arc<SentenceModel>> {
        self.key_model
            .as_ref()
            .and_then(|model| model.get())
            .or_else(|| self.desk_model.as_ref().and_then(|model| model.get()))
            .cloned()
    }

    /// 重排用的模型：desk 优先（质量高），desk 缺失/加载失败回退 key。
    fn rerank_model(&self) -> Option<Arc<SentenceModel>> {
        self.desk_model
            .as_ref()
            .and_then(|model| model.get())
            .or_else(|| self.key_model.as_ref().and_then(|model| model.get()))
            .cloned()
    }

    /// 对每个前缀做短续写，返回 `(完整文本 = 前缀 + 续写, 平均对数概率)`。
    ///
    /// 模型缺失时返回空表 —— 调用方按"没有预测"处理,不影响原有候选。
    /// 双模型各跑一遍，再按 [`merge_predictions`] 的配额合并。
    fn predict(&self, context: &str, prefixes: &[String]) -> Vec<(String, f64)> {
        self.predict_merged(
            context,
            prefixes,
            self.predict_options.max_results,
            "",
            false,
        )
    }

    /// 上屏联想条专用同步预测（对外入口见 [`predict_sync`]）。
    ///
    /// `context` 是**已上屏文本**（提交前上文 + 刚上屏的词），`excluded` 是
    /// 刚上屏的词本身。prefix 留空让模型从 context 末尾直接生成「下一个词」，
    /// 再由合并层把 `excluded` 从结果里剔掉 —— 联想条不再重复刚上屏的文字
    /// （2026-11-08 用户反馈）。打字时候选窗的预测仍走 `predict` + 非空 anchors。
    fn predict_sync(&self, context: &str, excluded: &str, total: usize) -> Vec<(String, f64)> {
        if context.is_empty() || total == 0 {
            return Vec::new();
        }
        // 上屏路径是"必须同帧出联想条"的同步等待，key/desk 两条腿并行算。
        self.predict_merged(context, &[String::new()], total, excluded, true)
    }

    /// 双模型合并预测的公共路径。
    ///
    /// 两个模型各跑 `predict_extensions`（**多要一点**：去重与"配额不足换另一个
    /// 模型补齐"都需要余量），再把完整词条交给 [`merge_predictions`] 按配额排。
    /// `excluded` 非空时走 [`merge_predictions_excluding`]（上屏联想条用）。
    fn predict_merged(
        &self,
        context: &str,
        prefixes: &[String],
        total: usize,
        excluded: &str,
        parallel: bool,
    ) -> Vec<(String, f64)> {
        if prefixes.is_empty() || total == 0 {
            return Vec::new();
        }
        // 每个模型先按 2*total 收：够"各模型配额 + 去重损耗 + 补齐"用。
        let want = total.saturating_mul(2);
        // 两个模型互不依赖、只读共享权重；同步上屏路径把 key 丢到一条临时线程，
        // desk 留本线程，join 后再按固定顺序合并 —— 结果与串行逐位相同。
        let (key, desk) = if parallel {
            let key_model = self
                .key_model
                .as_ref()
                .and_then(|m| m.get())
                .map(|m| m.as_ref());
            let desk_model = self
                .desk_model
                .as_ref()
                .and_then(|m| m.get())
                .map(|m| m.as_ref());
            match (key_model, desk_model) {
                (Some(key), Some(desk)) => {
                    let mut options = self.predict_options.clone();
                    options.max_results = want;
                    let trace = crate::engine::stage_trace::enabled();
                    std::thread::scope(|s| {
                        let key_task = s.spawn(|| {
                            let t = trace.then(std::time::Instant::now);
                            let out = predict_model(key, context, prefixes, &options);
                            (out, t.map(|t| t.elapsed()))
                        });
                        let t_desk = trace.then(std::time::Instant::now);
                        let desk = predict_model(desk, context, prefixes, &options);
                        let desk_elapsed = t_desk.map(|t| t.elapsed());
                        let (key_out, key_elapsed) = key_task.join().unwrap_or_default();
                        // 只在上屏同步路径（`parallel`）打这两行：候选窗每键预测不打。
                        if let (Some(k), Some(d)) = (key_elapsed, desk_elapsed) {
                            let seq = crate::engine::stage_trace::current();
                            crate::engine::stage_trace::emit(seq, "predict_key", k.as_micros());
                            crate::engine::stage_trace::emit(seq, "predict_desk", d.as_micros());
                        }
                        (key_out, desk)
                    })
                }
                // 缺模型/未配置时照旧走串行，省掉无意义的线程。
                _ => (
                    self.predict_with(self.key_model.as_ref(), context, prefixes, want),
                    self.predict_with(self.desk_model.as_ref(), context, prefixes, want),
                ),
            }
        } else {
            (
                self.predict_with(self.key_model.as_ref(), context, prefixes, want),
                self.predict_with(self.desk_model.as_ref(), context, prefixes, want),
            )
        };
        merge_predictions_excluding(&key, &desk, total, excluded)
    }

    /// 一个模型对所有前缀做续写，拼成完整词条、去重、按平均分排序后截断。
    ///
    /// 返回的是**完整文本**（前缀 + 续出的那段）：`predict_extensions` 只给续出的
    /// 部分，联想条/候选窗要的是整词。
    fn predict_with(
        &self,
        model: Option<&LazySentenceModel>,
        context: &str,
        prefixes: &[String],
        max_results: usize,
    ) -> Vec<(String, f64)> {
        let Some(model) = model.and_then(|model| model.get()).cloned() else {
            return Vec::new();
        };
        let mut options = self.predict_options.clone();
        options.max_results = max_results;
        predict_model(&model, context, prefixes, &options)
    }

    /// 模型可用时跑一次重排;模型缺失/句子不足两条时返回空表
    /// (调用方保持静态顺序——半份重排比不排更糟)。
    fn rerank_scored(
        &self,
        context: &str,
        sentences: &[String],
        static_scores: &[f64],
    ) -> (Vec<usize>, Vec<f64>) {
        let Some(model) = self.rerank_model() else {
            return (Vec::new(), Vec::new());
        };
        rerank_scored(&model, context, sentences, static_scores, &self.options)
    }
}

/// 一个模型对所有前缀做续写、拼成完整词条、去重、按平均分排序后截断。
///
/// 不依赖 `&self`：同步上屏路径要在临时线程里跑 key 模型，串行路径也复用同一份实现。
fn predict_model(
    model: &SentenceModel,
    context: &str,
    prefixes: &[String],
    options: &PredictOptions,
) -> Vec<(String, f64)> {
    let mut out: Vec<(String, f64)> = Vec::new();
    for prefix in prefixes {
        for (extension, score) in predict_extensions(model, context, prefix, options) {
            let text = format!("{prefix}{extension}");
            if text == *prefix || out.iter().any(|(existing, _)| *existing == text) {
                continue;
            }
            out.push((text, score));
        }
    }
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out.truncate(options.max_results);
    out
}

/// 双模型预测结果的配额合并（纯函数，便于单测）。
///
/// 用户 2026-11-08 拍板（"kkdd"）：**先 key 后 desk 分块**，不再交替。
/// * 打字时的预测条（总数 3）= key 2 + desk 1，顺序 `k0,k1,d0`；
/// * 上屏后联想条（总数 4）= key 2 + desk 2，顺序 `k0,k1,d0,d1`；
/// * 一般规则：key 拿 `ceil(total/2)`（**奇数余数给 key**），desk 拿 `floor(total/2)`。
///
/// 补充规则：
/// * 同文本去重，保留**第一次出现**（key 段先到先得，desk 段遇重复顺延）；
/// * 配额内某个模型凑不够时，用另一个模型的剩余结果补齐，最多 `total` 条；
/// * 只有 key / 只有 desk 时等价于单模型截断；两个都空就返回空表。
pub(crate) fn merge_predictions(
    key: &[(String, f64)],
    desk: &[(String, f64)],
    total: usize,
) -> Vec<(String, f64)> {
    if total == 0 {
        return Vec::new();
    }
    let key_quota = total.div_ceil(2);
    let desk_quota = total / 2;
    let mut out: Vec<(String, f64)> = Vec::with_capacity(total);
    let mut key_cursor = 0usize;
    let mut desk_cursor = 0usize;

    // 第一段：key 先占满自己的配额（奇数余数也归它）。
    // out 这一段全来自 key，故 out.len() 就等于已取的 key 条数。
    while out.len() < key_quota {
        match take_next(key, &mut key_cursor, &out) {
            Some(item) => out.push(item),
            // key 料用完：剩下的位子交给 desk 段与补齐阶段。
            None => break,
        }
    }
    // 第二段：desk 占自己的配额（key 段已定序，desk 只往后排）。
    let mut desk_taken = 0usize;
    while desk_taken < desk_quota && out.len() < total {
        match take_next(desk, &mut desk_cursor, &out) {
            Some(item) => {
                out.push(item);
                desk_taken += 1;
            }
            None => break,
        }
    }
    // 配额内不足：用剩余结果补齐（key 优先，与"奇数余数给 key"的口径一致）。
    while out.len() < total {
        if let Some(item) = take_next(key, &mut key_cursor, &out) {
            out.push(item);
            continue;
        }
        match take_next(desk, &mut desk_cursor, &out) {
            Some(item) => out.push(item),
            None => break,
        }
    }
    out
}

/// 上屏联想条专用合并：额外把「刚上屏的词」本身排除掉。
///
/// 2026-11-08 用户反馈「预测怎么还会重复刚上屏的文字」。根因是旧实现把
/// 刚上屏词当 prefix 续写，结果每条都是「上屏词 + 续写」（「你好」→「你好像是」）。
/// 现在 prefix 留空、上屏词并入 context，模型直接生成下一词；这里再兜一道底，
/// 把模型偶尔又生成的上屏词本身剔掉，**且不占 `total` 配额**（kkdd 仍严格 2+2）。
pub(crate) fn merge_predictions_excluding(
    key: &[(String, f64)],
    desk: &[(String, f64)],
    total: usize,
    excluded: &str,
) -> Vec<(String, f64)> {
    if excluded.is_empty() {
        return merge_predictions(key, desk, total);
    }
    let strip = |list: &[(String, f64)]| -> Vec<(String, f64)> {
        list.iter()
            .filter(|(text, _)| text != excluded)
            .cloned()
            .collect()
    };
    merge_predictions(&strip(key), &strip(desk), total)
}

/// 从 `list[*cursor..]` 里取下一个不与 `out` 重复的预测条目
/// （[`merge_predictions`] 的核心小工具）。
fn take_next(
    list: &[(String, f64)],
    cursor: &mut usize,
    out: &[(String, f64)],
) -> Option<(String, f64)> {
    while *cursor < list.len() {
        let item = &list[*cursor];
        *cursor += 1;
        if !out.iter().any(|(text, _)| *text == item.0) {
            return Some(item.clone());
        }
    }
    None
}

/// 结果键:`\x01` 不会出现在 UTF-8 文本里,拿它当分隔符不会把两批不同的
/// 候选拼成同一个键。
pub fn make_key(model_tag: &str, context: &str, sentences: &[String]) -> String {
    let mut key = String::with_capacity(64 + context.len() + sentences.iter().map(|s| s.len() + 1).sum::<usize>());
    key.push_str(model_tag);
    key.push('\x01');
    key.push_str(context);
    for sentence in sentences {
        key.push('\x01');
        key.push_str(sentence);
    }
    key
}

/// 进程级单例。`None` = 尚未找到模型。
///
/// 用 RwLock 而不是 OnceLock:OnceLock 会把"第一次没找到"永久缓存,
/// 后续会话换了数据根(或测试给了正确根)也没机会补上。运行期实际
/// 只写一次(有 worker 后直接短路),无争用。
static GLOBAL: std::sync::RwLock<Option<Arc<RescoreWorker>>> = std::sync::RwLock::new(None);

/// 在数据根里找模型文件并初始化全局 worker(已有 worker 时直接短路)。
///
/// # 双模型(2026-11-08 用户口径)
///
/// * `model/sentence-model.safetensors` —— 小号键盘模型(key,6 层 4.5MB);
/// * `model/sentence-model-desktop.safetensors` —— 大号桌面模型(desk,8 层 25MB)。
///
/// 两个可以来自**不同**数据根(用户目录覆盖共享目录),所以每个 root 都分别找、
/// 各自取第一个命中。**key 优先**:找到 key 才启用双模型;只有 desk 时把 desk
/// 当单模型启用(与旧版一致——旧版 desk 就是 fallback 单模型);两个都没有就保持
/// `None` —— 整个神经层静默关闭,输入法照常工作。
pub fn init_from_data_roots(roots: &[std::path::PathBuf]) {
    if global().is_some() {
        return;
    }
    const KEY_MODEL: &str = "model/sentence-model.safetensors";
    const DESK_MODEL: &str = "model/sentence-model-desktop.safetensors";
    let mut slot = GLOBAL.write().expect("rescore 全局表毒化不可恢复");
    if slot.is_some() {
        return;
    }
    let (mut key_path, mut desk_path) = (None, None);
    for root in roots {
        if key_path.is_none() {
            let path = root.join(KEY_MODEL);
            if path.is_file() {
                key_path = Some(path);
            }
        }
        if desk_path.is_none() {
            let path = root.join(DESK_MODEL);
            if path.is_file() {
                desk_path = Some(path);
            }
        }
    }
    match (key_path, desk_path) {
        (Some(key), desk) => {
            // 部署核验用:日志里能直接看到 desk 路径,证明双模型都找到了。
            tracing::info!(
                key = %key.display(),
                desk = ?desk.as_ref().map(|path| path.display().to_string()),
                "rescore: 神经模型已加载（key + desk）"
            );
            *slot = Some(RescoreWorker::new_singleton(NeuralScorer::from_paths(
                Some(key),
                desk,
            )));
        }
        (None, Some(desk)) => {
            tracing::info!(
                model = %desk.display(),
                "rescore: 没有小号键盘模型,desktop 模型按单模型启用"
            );
            *slot = Some(RescoreWorker::new_singleton(NeuralScorer::new(desk)));
        }
        (None, None) => {}
    }
}

/// 同步给一段文本打分（`log P(text | context)`），模型不可用时 `None`。
///
/// **只给上屏路径用**：一次提交算一次，不在按键路径上。预测/重排另有异步通道，
/// 不要拿它俩去调这个函数——那是把几十毫秒塞进按键预算。
/// 双模型下用 **key**（小号模型）：提交路径要快。
pub fn score_text(context: &str, text: &str) -> Option<f64> {
    let worker = global()?;
    let model = worker.scorer.score_model()?;
    let scores = model.score_sentences(context, &[text]);
    scores.first().copied()
}

/// 同步跑一次双模型合并预测——**只给上屏后的联想条用**（2026-11-08 用户口径：
/// 联想条 4 条 = key2 + desk2，kkdd 顺序）。
///
/// `context` = 已上屏文本（提交前上文 + 刚上屏的词），`excluded` = 刚上屏的词。
/// prefix 留空让模型从 context 末尾直接生成「下一个词」，并把 `excluded` 从
/// 结果里剔掉，避免联想条重复刚上屏的文字。
///
/// 逐字自回归要几十毫秒（上一棒实测默认参数 key 17ms + desk 71ms ≈ 77ms/次提交），
/// 用户认可放在提交路径上同步算 —— 联想条必须和上屏同一帧出现，异步重绘方案已被否。
/// **别拿它上按键路径。** worker/模型不可用 → 空表，调用方回退个人 n-gram。
///
/// 2026-11-09：`WEASEL_PREDICT=off` / `enable_neural_inference=false` 在**这个入口**
/// 一刀切（[`crate::engine::gear::neural::predictions_enabled`]）—— 关掉后直接返回
/// 空表，不碰模型、不付推理成本；调用点不需要各自判断。
pub fn predict_sync(context: &str, excluded: &str, total: usize) -> Vec<(String, f64)> {
    predict_sync_gated(
        crate::engine::gear::neural::predictions_enabled(),
        context,
        excluded,
        total,
    )
}

/// [`predict_sync`] 的实现：闸门作为参数，回归用例不必碰进程级环境变量。
fn predict_sync_gated(
    gate: bool,
    context: &str,
    excluded: &str,
    total: usize,
) -> Vec<(String, f64)> {
    if !gate {
        return Vec::new();
    }
    let Some(worker) = global() else {
        return Vec::new();
    };
    worker.scorer.predict_sync(context, excluded, total)
}

/// 全局 worker 里某批候选的 λ 融合分；没算完/没模型返回 `None`。
///
/// 给线性重排头在提交时补查用（首次 compose 直接选词时，结果可能刚好落地）。
pub fn scores_for(context: &str, sentences: &[String]) -> Option<Vec<f64>> {
    global()?.scores_for(context, sentences)
}

/// 全局 worker 的共享句柄;未初始化(没有模型)时返回 `None`。
pub fn global() -> Option<Arc<RescoreWorker>> {
    GLOBAL
        .read()
        .expect("rescore 全局表毒化不可恢复")
        .clone()
}

/// 上屏/换方案后调用:待办里的旧请求作废,结果表留着(同输入重打还能命中)。
pub fn clear_pending() {
    if let Some(worker) = global() {
        worker.clear();
    }
}

impl RescoreWorker {
    pub fn new(scorer: NeuralScorer) -> Self {
        Self {
            state: Mutex::new(WorkerState::default()),
            wake: Condvar::new(),
            scorer: Arc::new(scorer),
            handle: Mutex::new(None),
            me: std::sync::OnceLock::new(),
            generation: AtomicU64::new(0),
            last_landed: Mutex::new(None),
        }
    }

    /// 单例构造:`Arc` + 线程存活句柄一次配好。
    pub fn new_singleton(scorer: NeuralScorer) -> Arc<Self> {
        let me = Arc::new(Self::new(scorer));
        let _ = me.me.set(Arc::downgrade(&me));
        me
    }

    fn model_tag(&self) -> &'static str {
        "neural"
    }

    /// 提交一批候选给后台重排。**不阻塞**——立即返回 `None`;
    /// 结果落在 [`RescoreWorker::order_for`]。
    pub fn submit(
        &self,
        context: &str,
        sentences: Vec<String>,
        static_scores: Vec<f64>,
        options: RerankOptions,
    ) {
        let key = make_key(self.model_tag(), context, &sentences);
        let mut state = self.state.lock().unwrap();
        // 已完成 / 正在算 / 已在待办槽的同一批都不必再排一份。
        if !state.should_queue_rerank(&key) {
            return;
        }
        state.pending_rerank = Some(Job {
            key,
            context: context.to_string(),
            sentences,
            static_scores,
            options,
        });
        self.ensure_thread(&mut state);
        drop(state);
        self.wake.notify_one();
    }

    /// 提交一批续写任务。与 [`Self::submit`] 同构:立即返回,结果落在
    /// [`Self::predict_for`]。
    pub fn submit_predict(
        &self,
        context: &str,
        prefixes: Vec<String>,
        options: PredictOptions,
    ) {
        if prefixes.is_empty() {
            return;
        }
        let key = make_key(self.model_tag(), context, &prefixes);
        let mut state = self.state.lock().unwrap();
        // 与重排同构：同 key 已完成 / 正在算 / 已在待办都跳过。
        if !state.should_queue_predict(&key) {
            return;
        }
        state.pending_predict = Some(PredictJob {
            key,
            context: context.to_string(),
            prefixes,
            options,
        });
        self.ensure_thread(&mut state);
        drop(state);
        self.wake.notify_one();
    }

    /// 预测结果就绪就返回;没算完就 `None`(调用方先不给预测)。
    pub fn predict_for(&self, context: &str, prefixes: &[String]) -> Option<Vec<(String, f64)>> {
        let key = make_key(self.model_tag(), context, prefixes);
        let state = self.state.lock().unwrap();
        state.done_predict.get(&key).cloned()
    }

    /// 有现成结果就返回重排后的下标序列;没有就 `None`(调用方按静态顺序走)。
    pub fn order_for(
        &self,
        context: &str,
        sentences: &[String],
    ) -> Option<Vec<usize>> {
        let key = make_key(self.model_tag(), context, sentences);
        let state = self.state.lock().unwrap();
        state.done.get(&key).map(|result| result.order.clone())
    }

    /// 与 [`Self::order_for`] 同一批结果里的 λ 融合分；给线性重排头用。
    pub fn scores_for(
        &self,
        context: &str,
        sentences: &[String],
    ) -> Option<Vec<f64>> {
        let key = make_key(self.model_tag(), context, sentences);
        let state = self.state.lock().unwrap();
        state.done.get(&key).map(|result| result.combined.clone())
    }

    /// 当前结果代数(引擎线程用来发现"有新结果落地了")。
    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// 最近一次落地的内容签名。引擎侧在代数变化时取回，与「已应用签名」比对；
    /// 内容一致就跳过 re-compose（对应 msime 的 `glosses_changed` 守卫）。
    pub fn last_landed(&self) -> Option<LandingSignature> {
        self.last_landed.lock().unwrap().clone()
    }

    /// 上屏、换方案、换前文:存着的顺序对新语境没有意义了。
    /// (结果表整体作废——键里有 context,留着也只是占内存。)
    pub fn clear(&self) {
        let mut state = self.state.lock().unwrap();
        state.done.clear();
        state.done_predict.clear();
        // 结果表作废了，「上次落地的内容」对新语境也不再有效，否则下一次
        // 同文本落地会被误判成「没变」而不推帧。
        *self.last_landed.lock().unwrap() = None;
        // 换语境后正在算的旧批次也作废：清掉 active,免得它落地时又把结果
        // 写回新语境的同名 key。两槽一并清空。
        let had_pending = state.pending_rerank.take().is_some()
            || state.pending_predict.take().is_some();
        state.active_rerank = None;
        state.active_predict = None;
        if had_pending {
            self.wake.notify_one();
        }
    }

    /// 停工,等当前一批算完。单例故意不析构(C++ 同款注释:退出时操作系统收走),
    /// 这个方法只给测试用。
    pub fn shutdown_for_test(&self) {
        let mut state = self.state.lock().unwrap();
        state.stopping = true;
        state.pending_rerank = None;
        state.pending_predict = None;
        state.active_rerank = None;
        state.active_predict = None;
        self.wake.notify_all();
        let handle = self.handle.lock().unwrap().take();
        drop(state);
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }

    /// 线程只在第一次真有活干时才起(调用方持有 state 锁)。
    fn ensure_thread(&self, state: &mut WorkerState) {
        let mut slot = self.handle.lock().unwrap();
        if slot.is_some() || state.stopping {
            return;
        }
        let weak = self.me.get().cloned();
        *slot = Some(
            std::thread::Builder::new()
                .name("rescore-worker".into())
                .spawn(move || Self::run(weak))
                .expect("rescore worker 线程创建失败"),
        );
    }

    fn run(me: Option<std::sync::Weak<RescoreWorker>>) {
        loop {
            // 取活:锁内只拿数据,打分在锁外做——它是最慢的一步,
            // 占着锁会让按键路径上的 order_for 跟着等。
            let job = {
                let Some(weak) = me.as_ref() else { return };
                let Some(owner) = weak.upgrade() else { return };
                let mut state = owner.state.lock().unwrap();
                loop {
                    if state.stopping {
                        return;
                    }
                    if let Some(job) = state.take_next() {
                        break job;
                    }
                    state = owner.wake.wait(state).unwrap();
                }
            };
            let owner = me.as_ref().and_then(|w| w.upgrade());
            let Some(owner) = owner else { return };
            // 本次落地的内容签名 + 「相对上次落地有没有真变化」。抄 msime
            // `glosses_changed`：结果全没变就不 bump 代数 ⇒ 引擎侧连轮询都
            // 不会触发 re-compose，省掉一次整段重译 + 一帧重绘。
            // 两个分支都会赋值，不需要占位初值（也免掉 unused assignment 警告）。
            let changed;
            let signature = match job {
                Task::Rerank(job) => {
                    let (order, combined) =
                        owner
                            .scorer
                            .rerank_scored(&job.context, &job.sentences, &job.static_scores);
                    let mut state = owner.state.lock().unwrap();
                    if state.done.len() >= MAX_RESULTS {
                        state.done.clear();
                    }
                    let sig = LandingSignature::Rerank { order: order.clone() };
                    changed = owner
                        .last_landed
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map_or(true, |old| sig.changes_from(Some(old)));
                    state.done.insert(job.key.clone(), RerankResult { order, combined });
                    // 落地完成,active 放行：同 key 再来才允许排新一轮。
                    state.active_rerank = None;
                    sig
                }
                Task::Predict(job) => {
                    let predicted = owner.scorer.predict(&job.context, &job.prefixes);
                    let texts: Vec<String> =
                        predicted.iter().map(|(text, _)| text.clone()).collect();
                    let mut state = owner.state.lock().unwrap();
                    if state.done_predict.len() >= MAX_RESULTS {
                        state.done_predict.clear();
                    }
                    let sig = LandingSignature::Predict { texts };
                    changed = owner
                        .last_landed
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map_or(true, |old| sig.changes_from(Some(old)));
                    state.done_predict.insert(job.key.clone(), predicted);
                    state.active_predict = None;
                    sig
                }
            };
            *owner.last_landed.lock().unwrap() = Some(signature);
            // 落地信号:引擎线程在 get_context 时轮询代数,变了就 re-compose。
            // 内容没变(恒等 order / 同文本预测)时不发信号,这就是「结果没变不重绘」。
            if changed {
                owner.generation.fetch_add(1, Ordering::Release);
            }
            // 回到外圈前降级;worker 若已被丢弃(测试场景),下轮 upgrade 失败退出。
        }
    }
}

#[cfg(test)]
mod merge_tests {
    use super::{merge_predictions, merge_predictions_excluding};

    /// 造一组 `(完整词条, 分数)`：文本按 `prefix` 编号、分数递减，便于断言顺序。
    fn pairs(prefix: &str, n: usize) -> Vec<(String, f64)> {
        (0..n)
            .map(|i| (format!("{prefix}{i}"), -(i as f64)))
            .collect()
    }

    fn texts(items: &[(String, f64)]) -> Vec<&str> {
        items.iter().map(|(text, _)| text.as_str()).collect()
    }

    /// 打字预测口径：总数 3 ⇒ key 2 + desk 1，kkdd 分块 `k0,k1,d0`。
    #[test]
    fn two_models_total_three_is_key_key_desk() {
        let key = pairs("k", 3);
        let desk = pairs("d", 3);
        let merged = merge_predictions(&key, &desk, 3);
        assert_eq!(texts(&merged), vec!["k0", "k1", "d0"]);
    }

    /// 联想条口径：总数 4 ⇒ key 2 + desk 2，用户 2026-11-08 "kkdd" 拍板。
    #[test]
    fn two_models_total_four_is_kkdd() {
        let key = pairs("k", 3);
        let desk = pairs("d", 3);
        let merged = merge_predictions(&key, &desk, 4);
        assert_eq!(texts(&merged), vec!["k0", "k1", "d0", "d1"]);
        // 总数 2：key 1 + desk 1。
        assert_eq!(texts(&merge_predictions(&key, &desk, 2)), vec!["k0", "d0"]);
    }

    /// 一般规则：奇数余数给 key（key 拿 ceil、desk 拿 floor），先 key 后 desk。
    #[test]
    fn odd_remainder_goes_to_key() {
        let key = pairs("k", 5);
        let desk = pairs("d", 5);
        let merged = merge_predictions(&key, &desk, 5);
        assert_eq!(texts(&merged), vec!["k0", "k1", "k2", "d0", "d1"]);
        assert_eq!(merged.len(), 5);
    }

    /// 只有 key：等价单模型截断（打字 3 条 / 联想 4 条）。
    #[test]
    fn only_key_truncates_to_total() {
        let key = pairs("k", 6);
        assert_eq!(texts(&merge_predictions(&key, &[], 3)), vec!["k0", "k1", "k2"]);
        assert_eq!(
            texts(&merge_predictions(&key, &[], 4)),
            vec!["k0", "k1", "k2", "k3"]
        );
    }

    /// 只有 desk：同样按单模型截断（desk-only 退化路径）。
    #[test]
    fn only_desk_truncates_to_total() {
        let desk = pairs("d", 6);
        assert_eq!(texts(&merge_predictions(&[], &desk, 3)), vec!["d0", "d1", "d2"]);
        assert_eq!(
            texts(&merge_predictions(&[], &desk, 4)),
            vec!["d0", "d1", "d2", "d3"]
        );
    }

    /// 两个模型都没预测（或 total=0）⇒ 空表，调用方回退个人 n-gram。
    #[test]
    fn empty_inputs_yield_empty() {
        assert!(merge_predictions(&[], &[], 4).is_empty());
        assert!(merge_predictions(&pairs("k", 3), &pairs("d", 3), 0).is_empty());
        assert!(merge_predictions(&[], &[], 0).is_empty());
    }

    /// 同文本去重：保留第一次出现（key 段先到先得），desk 的重复项顺延。
    #[test]
    fn duplicate_texts_keep_first_occurrence() {
        let key = vec![("同".to_string(), -0.1), ("甲".to_string(), -0.2)];
        let desk = vec![("同".to_string(), -0.3), ("乙".to_string(), -0.4)];
        let merged = merge_predictions(&key, &desk, 4);
        assert_eq!(
            texts(&merged),
            vec!["同", "甲", "乙"],
            "desk 的「同」是重复项，被跳过并顺延到下一条"
        );
        // 分数跟着各自的模型走，没有被合并/重算。
        assert_eq!(merged[0].1, -0.1);
        assert_eq!(merged[2].1, -0.4);
    }

    /// 配额不足补齐：key 只有 1 条时，联想条 4 个位子由 desk 的剩余结果填满。
    #[test]
    fn short_key_quota_is_filled_from_desk() {
        let key = pairs("k", 1);
        let desk = pairs("d", 4);
        let merged = merge_predictions(&key, &desk, 4);
        assert_eq!(merged.len(), 4);
        assert_eq!(texts(&merged), vec!["k0", "d0", "d1", "d2"]);
    }

    /// 反向补齐：desk 只有 1 条时由 key 的剩余结果填满（key 段已在最前）。
    #[test]
    fn short_desk_quota_is_filled_from_key() {
        let key = pairs("k", 4);
        let desk = pairs("d", 1);
        let merged = merge_predictions(&key, &desk, 4);
        assert_eq!(merged.len(), 4);
        assert_eq!(texts(&merged), vec!["k0", "k1", "d0", "k2"]);
    }

    /// 两个模型给得比总数多：只取配额，不把另一模型的剩余塞进来（除非配额不足）。
    #[test]
    fn surplus_is_truncated_to_quota() {
        let key = pairs("k", 8);
        let desk = pairs("d", 8);
        let merged = merge_predictions(&key, &desk, 3);
        assert_eq!(merged.len(), 3);
        assert_eq!(texts(&merged), vec!["k0", "k1", "d0"]);
    }

    /// 上屏联想条：刚上屏的词不进结果、也不占配额（kkdd 仍严格 2+2）。
    ///
    /// 守 2026-11-08 用户反馈「预测怎么还会重复刚上屏的文字」。
    #[test]
    fn excluded_word_does_not_consume_quota() {
        let key = vec![
            ("你好".to_string(), -0.1), // 刚上屏的词，必须剔掉
            ("k0".to_string(), -0.2),
            ("k1".to_string(), -0.3),
        ];
        let desk = vec![
            ("你好".to_string(), -0.4),
            ("d0".to_string(), -0.5),
            ("d1".to_string(), -0.6),
        ];
        assert_eq!(
            texts(&merge_predictions_excluding(&key, &desk, 4, "你好")),
            vec!["k0", "k1", "d0", "d1"]
        );
        // 空排除词 = 原行为（打字时候选窗的预测走这条）。
        assert_eq!(
            texts(&merge_predictions_excluding(&key, &desk, 4, "")),
            vec!["你好", "k0", "d0", "d1"]
        );
    }

    /// 排除词只在一侧出现时，另一侧配额不受影响。
    #[test]
    fn excluded_word_only_on_one_side() {
        let key = vec![("你好".to_string(), -0.1), ("k0".to_string(), -0.2)];
        let desk = pairs("d", 2);
        assert_eq!(
            texts(&merge_predictions_excluding(&key, &desk, 3, "你好")),
            vec!["k0", "d0", "d1"]
        );
    }
}

#[cfg(test)]
mod landing_tests {
    use super::LandingSignature as S;

    fn rerank(order: &[usize]) -> S {
        S::Rerank {
            order: order.to_vec(),
        }
    }
    fn predict(texts: &[&str]) -> S {
        S::Predict {
            texts: texts.iter().map(|t| t.to_string()).collect(),
        }
    }
    fn text(sig: &S) -> String {
        match sig {
            S::Rerank { order } => format!("rerank {order:?}"),
            S::Predict { texts } => format!("predict {texts:?}"),
        }
    }

    /// 抄 msime `glosses_changed`：恒等 order（静态顺序已最优）落地 ⇒ 屏幕不变，
    /// 即使引擎还没应用过任何重排签名也不值得 re-compose 一帧。
    #[test]
    fn identity_order_never_needs_recompose() {
        let sig = rerank(&[0, 1, 2, 3]);
        assert!(S::is_identity(&sig.clone().into_order().unwrap()));
        println!(
            "落地={} 已应用=None → 需要重绘={}（期望 false）",
            text(&sig),
            sig.changes_from(None)
        );
        assert!(!sig.changes_from(None), "恒等 order 不该触发重绘");
    }

    /// 非恒等 order 首次落地必须推一帧，否则神经顺序上不了屏。
    #[test]
    fn first_non_identity_order_needs_recompose() {
        let sig = rerank(&[2, 0, 1]);
        println!(
            "落地={} 已应用=None → 需要重绘={}（期望 true）",
            text(&sig),
            sig.changes_from(None)
        );
        assert!(sig.changes_from(None));
    }

    /// 同一 order 重复落地（用户重打同一句）⇒ 逐条比现值全一样，跳过。
    #[test]
    fn same_order_twice_is_noop() {
        let sig = rerank(&[2, 0, 1]);
        let applied = rerank(&[2, 0, 1]);
        println!(
            "落地={} 已应用={} → 需要重绘={}（期望 false）",
            text(&sig),
            text(&applied),
            sig.changes_from(Some(&applied))
        );
        assert!(!sig.changes_from(Some(&applied)));
    }

    /// order 变了（哪怕只换两条）必须推帧。
    #[test]
    fn different_order_needs_recompose() {
        let sig = rerank(&[2, 0, 1]);
        let applied = rerank(&[1, 0, 2]);
        println!(
            "落地={} 已应用={} → 需要重绘={}（期望 true）",
            text(&sig),
            text(&applied),
            sig.changes_from(Some(&applied))
        );
        assert!(sig.changes_from(Some(&applied)));
    }

    /// 预测文本逐条相同 ⇒ 不重绘；多一条/少一条都算变化。
    #[test]
    fn prediction_changes_only_when_texts_change() {
        let same = predict(&["你好", "你好呀"]);
        let applied = predict(&["你好", "你好呀"]);
        let added = predict(&["你好", "你好呀", "你们好"]);
        let reordered = predict(&["你好呀", "你好"]);
        println!(
            "落地={} 已应用={} → {}（期望 false）",
            text(&same),
            text(&applied),
            same.changes_from(Some(&applied))
        );
        println!(
            "落地={} 已应用={} → {}（期望 true）",
            text(&added),
            text(&applied),
            added.changes_from(Some(&applied))
        );
        println!(
            "落地={} 已应用={} → {}（期望 true）",
            text(&reordered),
            text(&applied),
            reordered.changes_from(Some(&applied))
        );
        assert!(!same.changes_from(Some(&applied)));
        assert!(added.changes_from(Some(&applied)));
        assert!(reordered.changes_from(Some(&applied)));
    }

    /// 空预测落地（模型没给出新东西）不推帧。
    #[test]
    fn empty_prediction_is_noop() {
        let sig = predict(&[]);
        println!(
            "落地={} 已应用=None → 需要重绘={}（期望 false）",
            text(&sig),
            sig.changes_from(None)
        );
        assert!(!sig.changes_from(None));
    }

    /// 重排 ↔ 预测 是两类不同的落地，类别变了必须重新 compose 让对应滤镜消费。
    #[test]
    fn cross_kind_always_needs_recompose() {
        let r = rerank(&[2, 0, 1]);
        let p = predict(&["你好"]);
        println!(
            "落地={} 已应用={} → {}（期望 true）",
            text(&r),
            text(&p),
            r.changes_from(Some(&p))
        );
        assert!(r.changes_from(Some(&p)));
        assert!(p.changes_from(Some(&r)));
    }
}

impl LandingSignature {
    #[cfg(test)]
    fn into_order(self) -> Option<Vec<usize>> {
        match self {
            LandingSignature::Rerank { order } => Some(order),
            _ => None,
        }
    }
}

#[cfg(test)]
mod gate_tests {
    /// 2026-11-09：总开关关掉时，上屏联想入口必须直接给空表
    /// （不碰模型、不付推理成本）。闸门参数化，单测不必碰进程级环境变量。
    #[test]
    fn predict_sync_gate_off_is_empty() {
        assert!(super::predict_sync_gated(false, "今天你好", "你好", 4).is_empty());
    }
}

/// 2026-11-10 第四条：待办分槽 + active key 去重（抄 msime `RescoreWorker`）。
/// 全部只构造 `WorkerState` 纯数据，不碰模型 / 线程 / 环境变量。
#[cfg(test)]
mod pending_tests {
    use super::{
        Job, PredictJob, RerankResult, Task, WorkerState, make_key,
    };
    use super::super::neural::{PredictOptions, RerankOptions};

    fn rerank_job(key: &str) -> Job {
        Job {
            key: key.to_owned(),
            context: "ctx".to_owned(),
            sentences: vec!["句".to_owned()],
            static_scores: vec![0.0],
            options: RerankOptions::default(),
        }
    }

    fn predict_job(key: &str) -> PredictJob {
        PredictJob {
            key: key.to_owned(),
            context: "ctx".to_owned(),
            prefixes: vec!["你".to_owned()],
            options: PredictOptions::default(),
        }
    }

    #[test]
    fn rerank_and_predict_take_separate_slots() {
        // 两槽各一：重排先取（贴按键反馈），预测仍在自己槽里、不会被挤掉。
        let mut state = WorkerState::default();
        state.pending_rerank = Some(rerank_job("rk"));
        state.pending_predict = Some(predict_job("pk"));
        match state.take_next() {
            Some(Task::Rerank(j)) => assert_eq!(j.key, "rk"),
            _ => panic!("第一件应是重排 rk"),
        }
        assert_eq!(state.active_rerank.as_deref(), Some("rk"));
        assert_eq!(state.active_predict, None, "还没取预测就不该标 active");
        match state.take_next() {
            Some(Task::Predict(j)) => assert_eq!(j.key, "pk"),
            _ => panic!("第二件应是预测 pk（分槽后没被重排挤掉）"),
        }
        assert_eq!(state.active_rerank.as_deref(), Some("rk"));
        assert_eq!(state.active_predict.as_deref(), Some("pk"));
        assert!(state.take_next().is_none(), "两槽都取空后应无活");
    }

    #[test]
    fn predict_submitted_first_is_not_overwritten_by_rerank() {
        // 旧单槽的真实 bug：先提交预测 pk，再提交重排 rk ⇒ 预测被覆盖丢弃。
        // 分槽后两次入队都成功，且重排优先被取走、预测仍在槽里。
        let mut state = WorkerState::default();
        assert!(state.should_queue_predict("pk"));
        state.pending_predict = Some(predict_job("pk"));
        assert!(state.should_queue_rerank("rk"));
        state.pending_rerank = Some(rerank_job("rk"));
        assert!(matches!(state.take_next(), Some(Task::Rerank(_))));
        assert!(
            matches!(state.take_next(), Some(Task::Predict(_))),
            "重排入队不能把先提交的预测挤掉"
        );
    }

    #[test]
    fn active_key_blocks_duplicate_queue_until_landed() {
        let mut state = WorkerState::default();
        let key = make_key("neural", "前文", &["句".to_owned()]);
        assert!(state.should_queue_rerank(&key));
        state.pending_rerank = Some(rerank_job(&key));
        // 取出后 pending 空，但 active 仍标着 ⇒ 同 key 不得再排第二份。
        assert!(matches!(state.take_next(), Some(Task::Rerank(_))));
        assert_eq!(state.active_rerank.as_deref(), Some(key.as_str()));
        assert!(
            !state.should_queue_rerank(&key),
            "正在算的同 key 必须被 active 挡住"
        );
        // 落地清 active 后，同 key 才允许重新排。
        state.active_rerank = None;
        state.done.insert(
            key.clone(),
            RerankResult {
                order: vec![0],
                combined: vec![0.0],
            },
        );
        assert!(!state.should_queue_rerank(&key), "已完成也不该重排");
    }

    #[test]
    fn predict_active_blocks_duplicate_queue_until_landed() {
        let mut state = WorkerState::default();
        let key = make_key("neural", "前文", &["你".to_owned()]);
        state.pending_predict = Some(predict_job(&key));
        assert!(matches!(state.take_next(), Some(Task::Predict(_))));
        assert!(
            !state.should_queue_predict(&key),
            "预测正在算的同 key 也必须被 active 挡住"
        );
        state.active_predict = None;
        state.done_predict.insert(key.clone(), vec![("你好".to_owned(), -1.0)]);
        assert!(!state.should_queue_predict(&key));
    }

    #[test]
    fn duplicate_key_in_same_slot_is_overwritten_not_stacked() {
        // 同类待办仍保持「新的盖旧的」：已在 pending 的同 key 不该再入队。
        let mut state = WorkerState::default();
        let key = "same";
        state.pending_rerank = Some(rerank_job(key));
        assert!(!state.should_queue_rerank(key));
        assert!(state.should_queue_rerank("other"));
    }

    #[test]
    fn stopping_blocks_all_new_work() {
        let mut state = WorkerState::default();
        state.stopping = true;
        assert!(!state.should_queue_rerank("r"));
        assert!(!state.should_queue_predict("p"));
        assert!(state.take_next().is_none());
    }
}
