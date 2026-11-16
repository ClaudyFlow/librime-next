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
//! 同一个线程还接预测任务(见 [`crate::gear::neural::predict_filter`])。
//! 预测是**逐字自回归**,一步一次 forward,比整批打分更慢,更不能上按键路径 ——
//! 但两者抢的是同一个后台槽:预测只在候选稀薄时被提交,频率远低于重排,
//! 排在重排后面等一拍无所谓。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::gear::neural::{
    predict_extensions, rerank_scored, LazySentenceModel, PredictOptions, RerankOptions,
    SentenceModel,
};

/// 热路径查找表用 foldhash（抄 CloudIME 依赖选型）。结果表按 `make_key`
/// 生成的长字符串做 key，每次 `order_for` / `predict_for` 都要查；SipHash
/// 的防哈希攻击特性在这个纯内部 key 上没有价值，只带来每次查表的开销。
/// ⚠️ `BTreeMap` 不能换——字典序是双流合并游标与 RecognizerPatterns 优先级的依赖。
type FastHashMap<K, V> = std::collections::HashMap<K, V, foldhash::fast::RandomState>;

/// 结果表只是给「同一次输入被重查一遍」兜底的,不是长期缓存:装满就整张丢掉,
/// 比维护 LRU 简单,代价也只是重算一次。
const MAX_RESULTS: usize = 128;

/// 停键防抖（抄 CloudInputMethodEditor `dispatch/rescore/state.rs` 的调度思路；
/// 目标仓 GPL-3.0-or-later，本仓 BSD-3-Clause，只取思想不取代码）。
///
/// 连击时每次 `submit` 都重计时，停手 `DEBOUNCE` 才把这一批交给后台打分。
/// 不这么做的话，打字途中的每个中间批次都会被 worker 老老实实算一遍，
/// 而它们的 key 里带着旧 context，结果注定没人读 —— 纯烧 CPU + 抢预测槽。
const DEBOUNCE: Duration = Duration::from_millis(80);
/// worker 的统一 tick：有待办但未到期时，最多睡这么久就醒来看看。
/// 完全没活时仍用无限 `wake.wait()`，空转不烧 CPU。
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// 最长等待：用户一直连击也不能永远不打分（防抖窗口被无限后推）。
/// 与 `DEBOUNCE` 取较早者放行。
const MAX_WAIT: Duration = Duration::from_secs(2);

/// 抄 CloudInputMethodEditor `HYBRID_PREFETCH_SYLLABLES=6`：输入到 6 个音节
/// 就在**后台线程**提前装好 24MB 的 desk 大模型；`HYBRID_DESK_SYLLABLES=9`
/// 才让它参与重排打分。
///
/// 本仓原本 `LazySentenceModel` 只做「用到才装」——长句第一次异步打分会卡在
/// 模型解析上（虽然不在按键路径，但结果落地明显迟一拍）。预取把这次解析
/// 挪到用户还在打字的时间窗里；短句则完全不碰 desk，继续走 4.3MB key。
const HYBRID_PREFETCH_SYLLABLES: usize = 6;
const HYBRID_DESK_SYLLABLES: usize = 9;

/// 重排该用哪个模型槽。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RerankSlot {
    Key,
    Desk,
}

/// 用首条候选的字数近似音节数（中文一字≈一音节）。
fn estimate_syllables(sentences: &[String]) -> usize {
    sentences.first().map_or(0, |text| text.chars().count())
}

fn needs_desk_prefetch(syllables: usize) -> bool {
    syllables >= HYBRID_PREFETCH_SYLLABLES
}

/// 短句用 key（快），长句才让 desk（准）参与；缺哪个就退到另一个。
fn choose_rerank_slot(syllables: usize, has_key: bool, has_desk: bool) -> Option<RerankSlot> {
    if syllables >= HYBRID_DESK_SYLLABLES && has_desk {
        Some(RerankSlot::Desk)
    } else if has_key {
        Some(RerankSlot::Key)
    } else if has_desk {
        Some(RerankSlot::Desk)
    } else {
        None
    }
}

/// 一批待重排的任务。
struct Job {
    key: String,
    context: String,
    sentences: Vec<String>,
    static_scores: Vec<f64>,
    options: RerankOptions,
    /// 提交时的导航态：落地时用来判断用户是否已接管当前页。
    nav: NavSnapshot,
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

/// 提交一批重排任务时的导航态快照——抄 CloudInputMethodEditor
/// `dispatch/rescore/mod.rs::advance_rescoring` 的 `navigated` /
/// `highlight >= page_size` / `choices` 闸门（只取思路，目标仓 GPL-3.0）。
///
/// 异步结果落地时若用户已经翻页或动过高亮，当前页已经被用户接管：
/// 这时候把候选顺序换掉，用户正在看的那**一整页**内容会跳变。
/// 与其"重排了但打扰用户"，不如丢弃这一次落地，等下一轮打字再生效。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NavSnapshot {
    /// 提交时选中的全局候选下标（来自 `Context::highlight_index`）。
    pub highlight: usize,
    /// 每页候选数（来自 `Context::page_size`）。
    pub page_size: usize,
}

impl NavSnapshot {
    pub fn new(highlight: usize, page_size: usize) -> Self {
        Self {
            highlight,
            page_size,
        }
    }

    /// 这次异步落地是否该被导航态挡住。
    pub fn defers(&self, current: &NavSnapshot) -> bool {
        let page = current.page_size.max(1);
        // 用户已翻到第二页或更后：整页内容正在被接管，绝不重绘。
        if current.highlight >= page {
            return true;
        }
        // 提交后用户动过高亮（第一页内也算）：当前页已被接管，丢弃。
        current.highlight != self.highlight
    }
}

/// 一批待办的防抖闸门。纯数据 + 纯判定，便于用具体时间戳单测。
///
/// `first_queued` 只在第一次入队时记下，之后每次按键重计时只动 `last_touch`。
/// 放行时刻 = `min(last_touch + delay, first_queued + MAX_WAIT)`：
/// 前者实现「停键才打分」，后者保证连击不断时最迟 2 秒也要出一次结果。
#[derive(Debug, Clone, Copy, Default)]
struct DebounceGate {
    delay: Duration,
    first_queued: Option<Instant>,
    last_touch: Option<Instant>,
}

impl DebounceGate {
    fn new(delay: Duration) -> Self {
        Self {
            delay,
            ..Self::default()
        }
    }

    /// 新任务入槽：记起点（仅首次）+ 重计时（每次）。
    fn touch(&mut self, now: Instant) {
        self.first_queued.get_or_insert(now);
        self.last_touch = Some(now);
    }

    fn clear(&mut self) {
        self.first_queued = None;
        self.last_touch = None;
    }

    fn active(&self) -> bool {
        self.last_touch.is_some()
    }

    fn ready_at(&self) -> Option<Instant> {
        let first = self.first_queued?;
        let last = self.last_touch?;
        Some((last + self.delay).min(first + MAX_WAIT))
    }

    /// 没 touch 过（含 `clear()` 后）视为立即可取——兼容直接给 `pending_*`
    /// 塞数据的纯逻辑测试，也保证换语境后旧闸门不会卡住新一批。
    fn is_ready(&self, now: Instant) -> bool {
        match self.ready_at() {
            Some(at) => now >= at,
            None => true,
        }
    }
}

#[derive(Default)]
struct WorkerState {
    /// 已完成的重排结果:`key → 顺序 + 融合分`。
    done: FastHashMap<String, RerankResult>,
    /// 已完成的预测结果:`key → (完整文本, 平均对数概率)`。
    done_predict: FastHashMap<String, Vec<(String, f64)>>,
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
    /// 两槽各自的停键防抖闸门。与 `pending_*` 一一对应：入槽 touch、
    /// 取走 clear、上屏/换语境 clear。重排/预测互不牵连（预测在上屏后，
    /// 频率低且要跟住提交帧，不该被打字途中的重排防抖拖住）。
    rerank_gate: DebounceGate,
    predict_gate: DebounceGate,
    /// 停工标志。
    stopping: bool,
}

impl WorkerState {
    /// 生产用默认 80ms；测试可缩短，避免真等。
    fn with_debounce(delay: Duration) -> Self {
        Self {
            rerank_gate: DebounceGate::new(delay),
            predict_gate: DebounceGate::new(delay),
            ..Self::default()
        }
    }

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
    /// 保留无参版本供纯数据测试（gate 未 touch ⇒ 立即可取）。
    fn take_next(&mut self) -> Option<Task> {
        self.take_next_at(Instant::now())
    }

    /// worker 统一 tick 调用：只取**防抖已到期**的槽。没到期的槽留在原处，
    /// 下一个 tick 再看；两个都到期时重排优先。
    fn take_next_at(&mut self, now: Instant) -> Option<Task> {
        if self.pending_rerank.is_some() && self.rerank_gate.is_ready(now) {
            self.rerank_gate.clear();
            if let Some(job) = self.pending_rerank.take() {
                self.active_rerank = Some(job.key.clone());
                return Some(Task::Rerank(job));
            }
        }
        if self.pending_predict.is_some() && self.predict_gate.is_ready(now) {
            self.predict_gate.clear();
            if let Some(job) = self.pending_predict.take() {
                self.active_predict = Some(job.key.clone());
                return Some(Task::Predict(job));
            }
        }
        None
    }

    /// 统一 tick 的下一个检查点：只算**确实有待办**的槽，取最早到期时刻。
    /// `None` = 两槽都空 ⇒ 调用方无限睡，等 `submit` 唤醒（空转零 CPU）。
    fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let mut earliest: Option<Instant> = None;
        if self.pending_rerank.is_some() {
            earliest = Some(self.rerank_gate.ready_at().unwrap_or(now).max(now));
        }
        if self.pending_predict.is_some() {
            let at = self.predict_gate.ready_at().unwrap_or(now).max(now);
            earliest = Some(earliest.map_or(at, |old| old.min(at)));
        }
        earliest
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
    /// 最近一次**重排**落地时附带的导航态；预测落地写 `None`（不适用）。
    /// 与 `last_landed` 同锁序发布：线程先写这里、再写 signature、最后 bump
    /// generation，所以引擎侧看到代数变化后两处都已就位。
    last_landed_nav: Mutex<Option<NavSnapshot>>,
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

    /// 重排用的模型：短句走 key（快），≥9 音节才用 desk（质量高）；
    /// 缺哪个就退到另一个，两个都不可用 ⇒ `None`（神经层静默关闭）。
    fn rerank_model(&self, syllables: usize) -> Option<Arc<SentenceModel>> {
        let slot =
            choose_rerank_slot(syllables, self.key_model.is_some(), self.desk_model.is_some())?;
        match slot {
            RerankSlot::Key => self.key_model.as_ref().and_then(|m| m.get()).cloned(),
            RerankSlot::Desk => self.desk_model.as_ref().and_then(|m| m.get()).cloned(),
        }
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
        let syllables = estimate_syllables(sentences);
        // 预取必须在选模型之前：本方法由 worker 线程调用（见 `RescoreWorker::run`），
        // 24MB 的 desk 在这里解析不占按键预算；等用户打到 9 音节要它打分时已就绪。
        if needs_desk_prefetch(syllables)
            && let Some(desk) = &self.desk_model
        {
            let _ = desk.get();
        }
        let Some(model) = self.rerank_model(syllables) else {
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
/// 一刀切（[`crate::gear::neural::predictions_enabled`]）—— 关掉后直接返回
/// 空表，不碰模型、不付推理成本；调用点不需要各自判断。
pub fn predict_sync(context: &str, excluded: &str, total: usize) -> Vec<(String, f64)> {
    predict_sync_gated(
        crate::gear::neural::predictions_enabled(),
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
        Self::with_debounce(scorer, DEBOUNCE)
    }

    /// 测试用：缩短防抖窗口，不必真等 80ms；生产恒走 [`Self::new`]。
    pub fn with_debounce(scorer: NeuralScorer, delay: Duration) -> Self {
        Self {
            state: Mutex::new(WorkerState::with_debounce(delay)),
            wake: Condvar::new(),
            scorer: Arc::new(scorer),
            handle: Mutex::new(None),
            me: std::sync::OnceLock::new(),
            generation: AtomicU64::new(0),
            last_landed: Mutex::new(None),
            last_landed_nav: Mutex::new(None),
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
    ///
    /// 不带导航态的兼容入口（旧调用点 / 纯逻辑测试）：默认「第一页首位」，
    /// 不会触发导航闸门。生产路径走 [`Self::submit_with_nav`]。
    pub fn submit(
        &self,
        context: &str,
        sentences: Vec<String>,
        static_scores: Vec<f64>,
        options: RerankOptions,
    ) {
        self.submit_with_nav(context, sentences, static_scores, options, NavSnapshot::default());
    }

    /// 带导航快照的提交入口：落地时据此判断用户是否已翻页/移高亮。
    pub fn submit_with_nav(
        &self,
        context: &str,
        sentences: Vec<String>,
        static_scores: Vec<f64>,
        options: RerankOptions,
        nav: NavSnapshot,
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
            nav,
        });
        // 每敲一键都重计时：停手 DEBOUNCE 才真正开工，中间批次不算。
        state.rerank_gate.touch(Instant::now());
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
        state.predict_gate.touch(Instant::now());
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

    /// 最近一次重排落地时的导航快照；`None` = 最近落地的是预测（或还没落地）。
    pub fn last_landed_nav(&self) -> Option<NavSnapshot> {
        self.last_landed_nav.lock().unwrap().clone()
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
        *self.last_landed_nav.lock().unwrap() = None;
        // 换语境后正在算的旧批次也作废：清掉 active,免得它落地时又把结果
        // 写回新语境的同名 key。两槽一并清空。
        let had_pending = state.pending_rerank.take().is_some()
            || state.pending_predict.take().is_some();
        // 旧语境的防抖计时一并作废，否则新语境的首批会被旧 last_touch 拖住。
        state.rerank_gate.clear();
        state.predict_gate.clear();
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
        state.rerank_gate.clear();
        state.predict_gate.clear();
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
                    let now = Instant::now();
                    if let Some(job) = state.take_next_at(now) {
                        break job;
                    }
                    // 统一 tick：有待办但未过防抖窗口 ⇒ 睡到最近到期点（最多
                    // POLL_INTERVAL 醒一次）；两槽都空 ⇒ 无限睡，submit 时才醒。
                    // 这样空转零 CPU，又不靠各任务自己 sleep 拼凑节奏。
                    state = match state.next_deadline(now) {
                        Some(deadline) => {
                            let wait = deadline
                                .saturating_duration_since(now)
                                .min(POLL_INTERVAL);
                            owner.wake.wait_timeout(state, wait).unwrap().0
                        }
                        None => owner.wake.wait(state).unwrap(),
                    };
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
                    let nav = job.nav;
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
                    *owner.last_landed_nav.lock().unwrap() = Some(nav);
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
                    // 预测落地没有重排导航态可比对 ⇒ 交给引擎侧按内容签名处理。
                    *owner.last_landed_nav.lock().unwrap() = None;
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
/// 2026-11-10 抄 CloudIME 第七条：热路径 HashMap 换 foldhash。
#[cfg(test)]
mod foldhash_tests {
    use super::FastHashMap;

    #[test]
    fn fast_hashmap_keeps_lookup_insert_remove_semantics() {
        let mut table = FastHashMap::<String, usize>::default();
        table.insert("你好".to_owned(), 1);
        table.insert("中国".to_owned(), 2);
        table.insert("人工智能".to_owned(), 3);
        assert_eq!(table.len(), 3);
        assert_eq!(table.get("你好"), Some(&1), "短中文 key 命中");
        assert_eq!(table.get("中国"), Some(&2));
        assert_eq!(table.get("人工智能"), Some(&3), "长 key 命中");
        assert_eq!(table.get("不存在"), None);
        table.insert("你好".to_owned(), 9);
        assert_eq!(table.len(), 3, "同 key 覆盖不增行");
        assert_eq!(table.get("你好"), Some(&9));
        assert_eq!(table.remove("中国"), Some(2));
        assert_eq!(table.get("中国"), None);
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn fast_hashmap_does_not_promise_iteration_order() {
        // foldhash 会改变桶序：这里先排序再比，验证的是集合完整性。
        // 任何依赖字典序的地方必须继续用 BTreeMap（双流合并游标、RecognizerPatterns）。
        let mut table = FastHashMap::<String, usize>::default();
        for (i, key) in ["a", "b", "c", "d"].iter().enumerate() {
            table.insert((*key).to_owned(), i);
        }
        let mut keys: Vec<String> = table.keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["a", "b", "c", "d"],
            "遍历集合完整；顺序由调用方自己决定，不依赖 hasher"
        );
    }
}

/// 2026-11-10 抄 CloudIME 第三条：大模型 lazy 预取 + 参与门槛。
#[cfg(test)]
mod prefetch_tests {
    use super::*;

    #[test]
    fn thresholds_keep_prefetch_ahead_of_participation() {
        assert_eq!(HYBRID_PREFETCH_SYLLABLES, 6);
        assert_eq!(HYBRID_DESK_SYLLABLES, 9);
        assert!(
            HYBRID_PREFETCH_SYLLABLES < HYBRID_DESK_SYLLABLES,
            "先预装再启用，别等要用时才解析 24MB"
        );
    }

    #[test]
    fn syllables_are_counted_from_the_first_candidate() {
        assert_eq!(estimate_syllables(&[]), 0);
        assert_eq!(
            estimate_syllables(&["你好".to_owned(), "你们".to_owned()]),
            2,
            "只取首条候选（同组字数一致）"
        );
        assert_eq!(estimate_syllables(&["中华人民共和国".to_owned()]), 7);
        assert_eq!(estimate_syllables(&["人工智能".to_owned()]), 4);
    }

    #[test]
    fn prefetch_starts_at_six_syllables() {
        for n in 0..6 {
            assert!(!needs_desk_prefetch(n), "n={n} 不该碰 24MB desk");
        }
        for n in 6..=20 {
            assert!(needs_desk_prefetch(n), "n={n} 应在后台预装 desk");
        }
    }

    #[test]
    fn desk_joins_rerank_only_at_nine_syllables() {
        // 双模型齐备：8 音节仍走 4.3MB key，9 音节才切到 24MB desk。
        assert_eq!(choose_rerank_slot(0, true, true), Some(RerankSlot::Key));
        assert_eq!(choose_rerank_slot(5, true, true), Some(RerankSlot::Key));
        assert_eq!(choose_rerank_slot(8, true, true), Some(RerankSlot::Key));
        assert_eq!(choose_rerank_slot(9, true, true), Some(RerankSlot::Desk));
        assert_eq!(choose_rerank_slot(20, true, true), Some(RerankSlot::Desk));
    }

    #[test]
    fn missing_model_falls_back_regardless_of_length() {
        assert_eq!(
            choose_rerank_slot(0, false, true),
            Some(RerankSlot::Desk),
            "只有 desk：短句也用它（旧单模型退化行为不变）"
        );
        assert_eq!(
            choose_rerank_slot(20, true, false),
            Some(RerankSlot::Key),
            "只有 key：长句仍用它"
        );
        assert_eq!(
            choose_rerank_slot(20, false, false),
            None,
            "两个都没有 ⇒ 神经层静默关闭"
        );
    }
}

/// 2026-11-10 抄 CloudIME 第二条：navigated / `highlight >= page_size` 闸门。
#[cfg(test)]
mod nav_tests {
    use super::*;

    #[test]
    fn nav_gate_decides_with_explicit_indices() {
        let at_first = NavSnapshot::new(0, 5);
        assert!(
            !at_first.defers(&NavSnapshot::new(0, 5)),
            "停在第一页首位（高亮0/页5）⇒ 应用落地"
        );
        assert!(
            at_first.defers(&NavSnapshot::new(1, 5)),
            "第一页内移到第2条（高亮1/页5）⇒ 丢弃"
        );
        assert!(
            at_first.defers(&NavSnapshot::new(5, 5)),
            "翻到第二页（高亮5/页5）⇒ 丢弃"
        );
        assert!(
            at_first.defers(&NavSnapshot::new(9, 5)),
            "翻到更后（高亮9/页5）⇒ 丢弃"
        );
        let from_second = NavSnapshot::new(5, 5);
        assert!(
            from_second.defers(&NavSnapshot::new(5, 5)),
            "提交时就在第二页、用户没动 ⇒ 仍不打扰当前页"
        );
        assert!(
            !NavSnapshot::default().defers(&NavSnapshot::default()),
            "默认快照（旧 submit 调用点）不挡"
        );
        assert!(
            !NavSnapshot::new(0, 0).defers(&NavSnapshot::new(0, 0)),
            "page_size=0 按 1 算，高亮 0 也不挡"
        );
    }

    #[test]
    fn submit_with_nav_records_the_snapshot_in_pending_job() {
        // 10s 防抖把 job 钉在待办槽，避免后台线程提前取走。
        let worker =
            RescoreWorker::with_debounce(NeuralScorer::new(""), Duration::from_secs(10));
        let nav = NavSnapshot::new(7, 5);
        worker.submit_with_nav(
            "ctx",
            vec!["句".to_owned()],
            vec![0.0],
            Default::default(),
            nav,
        );
        {
            let state = worker.state.lock().unwrap();
            let job = state.pending_rerank.as_ref().expect("应留在待办槽");
            assert_eq!(
                job.nav, nav,
                "提交时的高亮=7/页大小=5 必须随 job 带到落地时"
            );
        }
        worker.shutdown_for_test();
    }

    #[test]
    fn clear_drops_the_landed_navigation_snapshot() {
        let worker =
            RescoreWorker::with_debounce(NeuralScorer::new(""), Duration::from_secs(10));
        *worker.last_landed_nav.lock().unwrap() = Some(NavSnapshot::new(3, 5));
        assert_eq!(worker.last_landed_nav(), Some(NavSnapshot::new(3, 5)));
        worker.clear();
        assert_eq!(
            worker.last_landed_nav(),
            None,
            "换语境后旧导航态必须失效，否则新页被旧高亮误挡"
        );
        worker.shutdown_for_test();
    }
}

/// 2026-11-10 抄 CloudIME 第一条：停键防抖 + 统一 tick。
/// 全部只构造纯数据（时间戳用固定偏移），不碰线程 / 模型 / 环境变量。
#[cfg(test)]
mod debounce_tests {
    use super::*;
    use crate::gear::neural::{PredictOptions, RerankOptions};

    const KEYS: Duration = Duration::from_millis(80);

    fn rerank_job(key: &str) -> Job {
        Job {
            key: key.to_owned(),
            context: "ctx".to_owned(),
            sentences: vec!["句".to_owned()],
            static_scores: vec![0.0],
            options: RerankOptions::default(),
            nav: super::NavSnapshot::default(),
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
    fn production_constants_match_cloudime_schedule() {
        assert_eq!(DEBOUNCE, Duration::from_millis(80));
        assert_eq!(POLL_INTERVAL, Duration::from_millis(20));
        assert_eq!(MAX_WAIT, Duration::from_secs(2));
        assert!(POLL_INTERVAL < DEBOUNCE, "tick 必须比防抖窗口短");
    }

    #[test]
    fn untouched_pending_releases_immediately() {
        let mut state = WorkerState::default();
        state.pending_rerank = Some(rerank_job("r"));
        let t = Instant::now();
        assert!(
            state.take_next_at(t).is_some(),
            "没 touch 过的待办 ⇒ 立即可取（兼容直接塞 pending 的纯逻辑测试）"
        );
    }

    #[test]
    fn each_keystroke_rearms_the_window() {
        let mut gate = DebounceGate::new(KEYS);
        let t0 = Instant::now();
        gate.touch(t0);
        assert_eq!(gate.ready_at(), Some(t0 + KEYS));
        gate.touch(t0 + Duration::from_millis(60));
        assert_eq!(
            gate.ready_at(),
            Some(t0 + Duration::from_millis(140)),
            "t=60ms 又敲一键 ⇒ 放行从 80ms 推到 140ms"
        );
        assert!(!gate.is_ready(t0 + Duration::from_millis(139)));
        assert!(gate.is_ready(t0 + Duration::from_millis(140)));
    }

    #[test]
    fn endless_typing_is_forced_out_by_max_wait() {
        let mut gate = DebounceGate::new(KEYS);
        let t0 = Instant::now();
        // 每 40ms 一键，连敲 50 次（到 1960ms）——纯重计时永远不会静默 80ms。
        for i in 0..=49 {
            gate.touch(t0 + Duration::from_millis(40 * i));
        }
        assert_eq!(
            gate.ready_at(),
            Some(t0 + MAX_WAIT),
            "1960+80=2040ms 晚于 0+2000ms ⇒ 硬上限先到"
        );
        assert!(!gate.is_ready(t0 + Duration::from_millis(1960)));
        assert!(gate.is_ready(t0 + MAX_WAIT), "2000ms 必须放行");
    }

    #[test]
    fn next_deadline_watches_only_pending_slots() {
        let mut state = WorkerState::with_debounce(KEYS);
        let t0 = Instant::now();
        assert_eq!(state.next_deadline(t0), None, "两槽空 ⇒ 无限睡");
        state.pending_rerank = Some(rerank_job("r"));
        state.rerank_gate.touch(t0);
        assert_eq!(state.next_deadline(t0), Some(t0 + KEYS));
        state.pending_predict = Some(predict_job("p"));
        state.predict_gate.touch(t0 + Duration::from_millis(10));
        assert_eq!(state.next_deadline(t0), Some(t0 + KEYS), "取更早的重排 80ms");
        state.pending_rerank = None;
        assert_eq!(
            state.next_deadline(t0),
            Some(t0 + Duration::from_millis(90)),
            "重排槽空后应看预测的 90ms，而不是继续等已清掉的重排"
        );
        state.pending_predict = None;
        assert_eq!(state.next_deadline(t0), None);
    }

    #[test]
    fn take_next_at_holds_both_slots_until_their_own_deadline() {
        let mut state = WorkerState::with_debounce(KEYS);
        let t0 = Instant::now();
        state.pending_rerank = Some(rerank_job("r"));
        state.rerank_gate.touch(t0);
        state.pending_predict = Some(predict_job("p"));
        state.predict_gate.touch(t0 + Duration::from_millis(30));
        assert!(
            state.take_next_at(t0 + Duration::from_millis(79)).is_none(),
            "79ms 两槽都没到期"
        );
        let at80 = t0 + KEYS;
        assert!(
            matches!(state.take_next_at(at80), Some(Task::Rerank(j)) if j.key == "r"),
            "80ms 重排先到期，且重排优先"
        );
        assert!(state.take_next_at(at80).is_none(), "预测要到 110ms");
        assert_eq!(state.active_rerank.as_deref(), Some("r"));
        assert_eq!(state.active_predict, None);
        let at110 = t0 + Duration::from_millis(110);
        assert!(
            matches!(state.take_next_at(at110), Some(Task::Predict(j)) if j.key == "p"),
            "110ms 预测到期后独立落地"
        );
        assert!(state.pending_rerank.is_none() && state.pending_predict.is_none());
    }

    #[test]
    fn clear_drops_old_timing_for_the_new_context() {
        let mut state = WorkerState::with_debounce(KEYS);
        let t0 = Instant::now();
        state.rerank_gate.touch(t0);
        assert!(state.rerank_gate.active());
        state.rerank_gate.clear();
        assert_eq!(state.rerank_gate.ready_at(), None);
        state.pending_rerank = Some(rerank_job("new"));
        let t1 = t0 + Duration::from_secs(1);
        state.rerank_gate.touch(t1);
        assert_eq!(
            state.next_deadline(t1),
            Some(t1 + KEYS),
            "换语境后的新一轮应在 t1+80ms 到期，不被 t0 的旧计时拖住"
        );
    }
}

#[cfg(test)]
mod pending_tests {
    use super::{
        Job, PredictJob, RerankResult, Task, WorkerState, make_key,
    };
    use crate::gear::neural::{PredictOptions, RerankOptions};

    fn rerank_job(key: &str) -> Job {
        Job {
            key: key.to_owned(),
            context: "ctx".to_owned(),
            sentences: vec!["句".to_owned()],
            static_scores: vec![0.0],
            options: RerankOptions::default(),
            nav: super::NavSnapshot::default(),
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
