//! 词格 n-best 的神经重排——`neural_decoder.cpp`(水杉,Apache-2.0)的移植。
//!
//! 模型不造句子。词格(引擎的整句候选)已经用静态分把键入的拼音解成 n-best 句子;
//! 这一层问字符级 Transformer:每个句子在**用户已上屏上下文**条件下有多像,
//! 按 `score = static + λ·(neural − static)` 重排(两项都是 nats)。
//!
//! 这是两个语言模型的 log 线性插值,λ 控制神经模型能推翻词格多少。它不会发明
//! 词格没提出的句子——词格擅长覆盖、不擅长长程选择,Transformer 正相反。
//!
//! 异步:神经打分一轮几十毫秒,不能长在按键路径上。调用方把候选交给
//! [`super::rescore::RescoreWorker`],下次查询时取回重排结果。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use super::model::SentenceModel;

/// 词格报 log10(kenlm 的单位);模型报自然对数。不换算的话 λ 会被悄悄放大 2.3 倍。
const LOG10_TO_NATS: f64 = 2.302_585_092_994_046;

/// 重排选项(水杉默认值)。
#[derive(Debug, Clone, Copy)]
pub struct RerankOptions {
    /// 神经模型的意见占多少。0 = 词格顺序不动;1 = 直接换成神经分。
    /// 0.5 保留词格自己的证据(用户词典权重、读音先验)——神经模型没见过这些。
    pub lambda: f64,
    /// 重排多少条路径。词格自己排到前几名之后已经是噪声,多一条就多一行 batch。
    pub max_paths: usize,
    /// 已上屏文本给模型看多少字符。受模型窗口限制;64 是模型训练时用的。
    pub context_chars: usize,
}

impl Default for RerankOptions {
    fn default() -> Self {
        Self {
            lambda: 0.5,
            max_paths: 12,
            context_chars: 64,
        }
    }
}

/// `text` 的最后 `count` 个 Unicode 字符(不是字节):上下文上限按字符算,
/// 因为模型窗口按字符算。按未截断文本做缓存键会在每次上屏都 miss——
/// 尾部相同的两段历史就是同一上下文。
pub fn last_characters(text: &str, count: usize) -> &str {
    if count == 0 {
        return "";
    }
    let mut seen = 0usize;
    let mut start = text.len();
    for (i, ch) in text.char_indices().rev() {
        start = i;
        seen += 1;
        if seen == count {
            break;
        }
        let _ = ch;
    }
    &text[start..]
}

/// 进程级模型缓存。值为 `None` 表示"加载 `path` 失败已试过"——缺文件
/// 不会在每次按键时重读。
fn model_cache() -> &'static Mutex<HashMap<PathBuf, Option<Arc<SentenceModel>>>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<Arc<SentenceModel>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 加载(一次,进程内共享)`path` 处的模型;缺失或无效返回 `None`。
pub fn shared_sentence_model(path: &Path) -> Option<Arc<SentenceModel>> {
    let mut cache = model_cache().lock().expect("neural model cache 毒化");
    cache
        .entry(path.to_path_buf())
        .or_insert_with(|| SentenceModel::load_file(path).map(Arc::new))
        .clone()
}

/// `sentences` 应有的展示顺序,以对它的下标返回。
///
/// * `static_scores` —— 词格自己的路径分(log10);
/// * `context` —— 组字前已上屏的文本,截到最后 `options.context_chars` 个字符。
///
/// 模型根本没被问上(句子不足两条、λ 为零、输入不齐)就返回空表:调用方必须
/// 保持自己的顺序——重排一半比不排更糟,那是把两套不可比的排序混在一起。
///
/// 只有前 `options.max_paths` 条被重排;之后的保持原相对顺序垫底。
pub fn rerank_order(
    model: &SentenceModel,
    context: &str,
    sentences: &[String],
    static_scores: &[f64],
    options: &RerankOptions,
) -> Vec<usize> {
    if sentences.len() < 2
        || sentences.len() != static_scores.len()
        || options.lambda == 0.0
    {
        return Vec::new();
    }
    let scored_count = sentences.len().min(options.max_paths);
    if scored_count < 2 {
        return Vec::new();
    }

    let texts: Vec<&str> = sentences[..scored_count].iter().map(|s| s.as_str()).collect();
    let neural = model.score_sentences(last_characters(context, options.context_chars), &texts);
    if neural.len() != scored_count {
        // 模型失败或不一致。半份重排不如没有,调用方保持自己的顺序。
        return Vec::new();
    }

    let mut combined = vec![0.0f64; scored_count];
    for i in 0..scored_count {
        let statik = static_scores[i] * LOG10_TO_NATS;
        combined[i] = statik + options.lambda * (neural[i] - statik);
    }

    let mut order: Vec<usize> = (0..sentences.len()).collect();
    // stable:两个模型打分相同的句子保持词格顺序;max_paths 之外的尾部原地不动。
    order[..scored_count].sort_by(|&a, &b| {
        combined[b]
            .partial_cmp(&combined[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    order
}

/// [`SentenceModel`] 打分器的惰性句柄——第一次被问才解析模型文件。
/// 两个模型加起来 ~28MB 堆内存;关着神经重排的用户不该白付这笔钱。
pub struct LazySentenceModel {
    path: PathBuf,
    resolved: OnceLock<Option<Arc<SentenceModel>>>,
}

impl LazySentenceModel {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            resolved: OnceLock::new(),
        }
    }

    pub fn get(&self) -> Option<&Arc<SentenceModel>> {
        self.resolved
            .get_or_init(|| {
                if self.path.as_os_str().is_empty() {
                    None
                } else {
                    shared_sentence_model(&self.path)
                }
            })
            .as_ref()
    }
}
