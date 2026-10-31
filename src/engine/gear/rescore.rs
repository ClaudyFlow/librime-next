//! 神经整句重排——异步后台线程(移植自 msime `neural/rescore_worker.cpp`)。
//!
//! # 设计(与 C++ 逐条对齐)
//!
//! 给 N 条候选打一次神经分要几十毫秒——比一次按键的预算大一个数量级，
//! 所以它**不能长在按键路径上**。查询照旧同步返回 n-gram 静态顺序，
//! 同时把这一批候选交给后台线程；线程算完把顺序存进结果表；
//! 调用方下次查询时从结果表取到重排后的顺序。
//!
//! "每个模型最新的赢"：用户还在打字时，同一模型前几次请求的结果已经
//! 没人要，新请求直接盖掉旧待办。
//!
//! "正在算的那一批不再排第二份"：结果表按 (模型, 前文, 候选列表) 做键。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// 句子评分器 trait——不同后端(n-gram 插值 / 神经模型)实现这个接口。
pub trait SentenceScorer: Send + Sync {
    /// 给一组候选句子打分，返回重排后的下标序列(按分数降序)。
    ///
    /// * `context` —— 前文(最近上屏的文本，用于条件打分)
    /// * `sentences` —— 候选句子列表
    /// * `static_scores` —— 静态分数(n-gram，已算好)
    ///
    /// 返回 `indices`，其中 `indices[0]` 是最佳句子的下标。
    fn score(&self, context: &str, sentences: &[String], static_scores: &[f64]) -> Vec<usize>;
}

/// 一批待重排的任务。
struct Job {
    context: String,
    sentences: Vec<String>,
    static_scores: Vec<f64>,
}

/// 异步重排结果缓存。
struct RescoreState {
    /// 已完成的结果:`key → 重排后的下标序列`。
    done: Mutex<HashMap<String, Vec<usize>>>,
    /// 待办(每个模型只有一个槽，新的盖旧的)。
    pending: Mutex<Option<Job>>,
    /// 停工标志。
    stopping: AtomicBool,
}

/// 异步重排后台线程。
pub struct RescoreWorker {
    state: Arc<RescoreState>,
    scorer: Arc<dyn SentenceScorer>,
    monitor: Arc<Mutex<()>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RescoreWorker {
    /// 创建并启动后台线程。
    pub fn new(scorer: Arc<dyn SentenceScorer>) -> Self {
        let state = Arc::new(RescoreState {
            done: Mutex::new(HashMap::new()),
            pending: Mutex::new(None),
            stopping: AtomicBool::new(false),
        });
        let monitor = Arc::new(Mutex::new(()));

        let st = state.clone();
        let scorer_clone = scorer.clone();
        let handle = std::thread::Builder::new()
            .name("rescore-worker".into())
            .spawn(move || {
                loop {
                    if st.stopping.load(Ordering::Acquire) {
                        break;
                    }
                    // 取待办
                    let job = {
                        let mut pending = st.pending.lock().unwrap();
                        pending.take()
                    };
                    match job {
                        Some(job) => {
                            // 后台打分(不阻塞按键路径)
                            let key = format!("{}|{}", job.context, job.sentences.join("\x1f"));
                            let indices = scorer_clone.score(
                                &job.context,
                                &job.sentences,
                                &job.static_scores,
                            );
                            let mut done = st.done.lock().unwrap();
                            done.insert(key, indices);
                            drop(done);
                            // 结果已写入 done 表,下次查询时取
                        }
                        None => {
                            // 没活，等一会儿再查
                            std::thread::sleep(std::time::Duration::from_millis(10));
                            if st.stopping.load(Ordering::Acquire) {
                                break;
                            }
                        }
                    }
                }
            })
            .unwrap();

        Self {
            state,
            scorer: scorer.clone(),
            monitor,
            handle: Some(handle),
        }
    }

    /// 提交一批候选给后台重排。**不阻塞**——立即返回。
    ///
    /// 调用方下次查询时用 [`order_for`] 取重排结果。
    pub fn submit(
        &self,
        model_id: &str,
        context: &str,
        sentences: Vec<String>,
        static_scores: Vec<f64>,
    ) {
        let key = format!("{}|{}|{}", model_id, context, sentences.join("\x1f"));
        let mut pending = self.state.pending.lock().unwrap();
        // "每个模型最新的赢"：直接覆盖
        *pending = Some(Job {
            context: context.to_string(),
            sentences,
            static_scores,
        });
        drop(pending);

        let _ = key;
    }

    /// 有现成结果就返回重排后的下标序列；没有就返回 None(调用方按静态顺序走)。
    pub fn order_for(
        &self,
        model_id: &str,
        context: &str,
        sentences: &[String],
        static_scores: &[f64],
    ) -> Option<Vec<usize>> {
        let key = format!("{}|{}|{}", model_id, context, sentences.join("\x1f"));
        self.state.done.lock().unwrap().get(&key).cloned()
    }

    /// 上屏、换方案、换前文：存着的顺序对新语境没有意义了。
    pub fn clear(&self) {
        self.state.done.lock().unwrap().clear();
        *self.state.pending.lock().unwrap() = None;
    }

    /// 停工，等当前一批算完。
    pub fn shutdown(&mut self) {
        self.state.stopping.store(true, Ordering::Release);

        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for RescoreWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 简单的 n-gram 插值评分器(v1 占位，后续替换为神经模型)。
pub struct NgramScorer {
    /// 神经分权重 λ(0 = 纯 n-gram,1 = 纯神经)。
    pub lambda: f64,
}

impl SentenceScorer for NgramScorer {
    fn score(&self, _context: &str, sentences: &[String], static_scores: &[f64]) -> Vec<usize> {
        // v1: 按 static_scores 降序排列(恒等重排——真正的神经打分后加)
        let mut indices: Vec<usize> = (0..sentences.len()).collect();
        indices.sort_by(|&a, &b| {
            static_scores
                .get(b)
                .unwrap_or(&f64::NEG_INFINITY)
                .partial_cmp(static_scores.get(a).unwrap_or(&f64::NEG_INFINITY))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        indices
    }
}
