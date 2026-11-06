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

use super::neural::{predict_extensions, rerank_order, LazySentenceModel, PredictOptions, RerankOptions};

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

struct WorkerState {
    /// 已完成的重排结果:`key → 重排后的下标序列`。
    done: HashMap<String, Vec<usize>>,
    /// 已完成的预测结果:`key → (完整文本, 平均对数概率)`。
    done_predict: HashMap<String, Vec<(String, f64)>>,
    /// 待办(单槽,新的盖旧的)。
    pending: Option<Task>,
    /// 停工标志。
    stopping: bool,
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
}

/// 神经打分器:模型 + 重排选项。`SentenceModel` 自身线程安全
/// (内部 prefix 缓存有锁),所以一个打分器跨线程共享。
pub struct NeuralScorer {
    model: LazySentenceModel,
    pub options: RerankOptions,
    /// 续写预测的参数(与重排各管一套:一个决定"排哪条",一个决定"续什么")。
    pub predict_options: PredictOptions,
}

impl NeuralScorer {
    pub fn new(model_path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            model: LazySentenceModel::new(model_path),
            options: RerankOptions::default(),
            predict_options: PredictOptions::default(),
        }
    }

    /// 对每个前缀做短续写,返回 `(完整文本 = 前缀 + 续写, 平均对数概率)`。
    ///
    /// 模型缺失时返回空表 —— 调用方按"没有预测"处理,不影响原有候选。
    fn predict(&self, context: &str, prefixes: &[String]) -> Vec<(String, f64)> {
        let Some(model) = self.model.get() else {
            return Vec::new();
        };
        let mut out: Vec<(String, f64)> = Vec::new();
        for prefix in prefixes {
            for (extension, score) in
                predict_extensions(model, context, prefix, &self.predict_options)
            {
                let text = format!("{prefix}{extension}");
                if text == *prefix || out.iter().any(|(existing, _)| *existing == text) {
                    continue;
                }
                out.push((text, score));
            }
        }
        out.sort_by(|a, b| b.1.total_cmp(&a.1));
        out.truncate(self.predict_options.max_results);
        out
    }

    /// 模型可用时跑一次重排;模型缺失/句子不足两条时返回空表
    /// (调用方保持静态顺序——半份重排比不排更糟)。
    fn rerank(
        &self,
        context: &str,
        sentences: &[String],
        static_scores: &[f64],
    ) -> Vec<usize> {
        let Some(model) = self.model.get() else {
            return Vec::new();
        };
        rerank_order(model, context, sentences, static_scores, &self.options)
    }
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
/// **keyboard(6层) 优先**——2026-10-31 基准(`example/neural_bench`):
/// 重排一轮暖缓存 60~170ms;desktop(8层) 293~858ms,质量更好但一轮要吃掉
/// 半个打字停顿,只适合显式放置它的用户。都没有就保持 `None`
/// ——整个神经重排层静默关闭,输入法照常工作。
pub fn init_from_data_roots(roots: &[std::path::PathBuf]) {
    if global().is_some() {
        return;
    }
    const CANDIDATES: &[&str] = &[
        "model/sentence-model.safetensors",
        "model/sentence-model-desktop.safetensors",
    ];
    let mut slot = GLOBAL.write().expect("rescore 全局表毒化不可恢复");
    if slot.is_some() {
        return;
    }
    for root in roots {
        for name in CANDIDATES {
            let path = root.join(name);
            if path.is_file() {
                *slot = Some(RescoreWorker::new_singleton(NeuralScorer::new(path)));
                return;
            }
        }
    }
}

/// 同步给一段文本打分（`log P(text | context)`），模型不可用时 `None`。
///
/// **只给上屏路径用**：一次提交算一次，不在按键路径上。预测/重排另有异步通道，
/// 不要拿它俩去调这个函数——那是把几十毫秒塞进按键预算。
pub fn score_text(context: &str, text: &str) -> Option<f64> {
    let worker = global()?;
    let model = worker.scorer.model.get()?;
    let scores = model.score_sentences(context, &[text]);
    scores.first().copied()
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
            state: Mutex::new(WorkerState {
                done: HashMap::new(),
                done_predict: HashMap::new(),
                pending: None,
                stopping: false,
            }),
            wake: Condvar::new(),
            scorer: Arc::new(scorer),
            handle: Mutex::new(None),
            me: std::sync::OnceLock::new(),
            generation: AtomicU64::new(0),
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
        if state.stopping {
            return;
        }
        // 正在算的就是这一批,算完自会进表,不必再排一份。
        if state.done.contains_key(&key) {
            return;
        }
        state.pending = Some(Task::Rerank(Job {
            key,
            context: context.to_string(),
            sentences,
            static_scores,
            options,
        }));
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
        if state.stopping || state.done_predict.contains_key(&key) {
            return;
        }
        state.pending = Some(Task::Predict(PredictJob {
            key,
            context: context.to_string(),
            prefixes,
            options,
        }));
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
        state.done.get(&key).cloned()
    }

    /// 当前结果代数(引擎线程用来发现"有新结果落地了")。
    pub fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// 上屏、换方案、换前文:存着的顺序对新语境没有意义了。
    /// (结果表整体作废——键里有 context,留着也只是占内存。)
    pub fn clear(&self) {
        let mut state = self.state.lock().unwrap();
        state.done.clear();
        state.done_predict.clear();
        if state.pending.is_some() {
            state.pending = None;
            self.wake.notify_one();
        }
    }

    /// 停工,等当前一批算完。单例故意不析构(C++ 同款注释:退出时操作系统收走),
    /// 这个方法只给测试用。
    pub fn shutdown_for_test(&self) {
        let mut state = self.state.lock().unwrap();
        state.stopping = true;
        state.pending = None;
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
                    if let Some(job) = state.pending.take() {
                        break job;
                    }
                    state = owner.wake.wait(state).unwrap();
                }
            };
            let owner = me.as_ref().and_then(|w| w.upgrade());
            let Some(owner) = owner else { return };
            match job {
                Task::Rerank(job) => {
                    let order = owner.scorer.rerank(&job.context, &job.sentences, &job.static_scores);
                    let mut state = owner.state.lock().unwrap();
                    if state.done.len() >= MAX_RESULTS {
                        state.done.clear();
                    }
                    state.done.insert(job.key, order);
                }
                Task::Predict(job) => {
                    let predicted = owner.scorer.predict(&job.context, &job.prefixes);
                    let mut state = owner.state.lock().unwrap();
                    if state.done_predict.len() >= MAX_RESULTS {
                        state.done_predict.clear();
                    }
                    state.done_predict.insert(job.key, predicted);
                }
            }
            // 落地信号:引擎线程在 get_context 时轮询代数,变了就 re-compose。
            owner.generation.fetch_add(1, Ordering::Release);
            // 回到外圈前降级;worker 若已被丢弃(测试场景),下轮 upgrade 失败退出。
        }
    }
}
