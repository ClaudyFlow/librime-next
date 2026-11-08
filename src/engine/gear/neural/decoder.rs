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

/// 续写预测的选项。
#[derive(Clone, Debug)]
pub struct PredictOptions {
    /// 最多续写几个字符。
    pub max_new: usize,
    /// 每步保留几条假设。
    pub beam: usize,
    /// 平均每字符对数概率的下限(低于它视为"模型也没把握")。
    pub min_avg_score: f64,
    /// 最多返回几条预测。
    pub max_results: usize,
    /// 送去打分的上下文长度。
    pub context_chars: usize,
}

impl Default for PredictOptions {
    fn default() -> Self {
        Self {
            // 只补 1~2 字:输入法的"预测"是补全,不是自由写作。
            max_new: 2,
            beam: 3,
            // 实测（6 层键盘版，2026-11-03 探针）：像样的续写平均分落在
            // -1.2 ~ -3.5；-5.0 之前基本还是人话，再往后就开始出"师主"这类
            // 纯噪声。注意判据是**候选稀薄**（见 predict_filter），这一条只是
            // 兜底的第二道闸。
            min_avg_score: -5.0,
            max_results: 3,
            context_chars: 64,
        }
    }
}

/// 续写时不该出现的字符:空白、控制符、ASCII 标点。
///
/// 中文标点没被排掉 —— 模型偶尔会正确预言"的话"这类带虚词的续写,
/// 逗号句号则几乎总是噪声,所以只滤 ASCII。
fn predictable(character: char) -> bool {
    !character.is_whitespace()
        && !character.is_control()
        && !(character.is_ascii_punctuation())
}

/// 从 `prefix` 往后做短续写,返回 `(续出来的文本, 平均每字符 log P)`。
///
/// 小束搜索:`max_new` 步、每步留 `beam` 条,按**平均**对数概率裁剪 ——
/// 平均是为了让不同长度的假设可比。只用 `prefix` 当锚点,不负责从零造整句。
///
/// **2026-11-08 起 `prefix` 允许为空**：此时直接拿 `context` 当上文、生成
/// 的就是「下一个词」，专供上屏联想条。旧实现把刚上屏的词当 prefix 续写，
/// 结果每条都是「上屏词 + 续写」（「你好」→「你好像是」），用户反馈
/// 「预测怎么还会重复刚上屏的文字」。改成空 prefix 后，刚上屏的词并入
/// `context`，模型从已上屏文本末尾直接生成下一词，不再重复。
/// 打字时候选窗的预测仍传非空 anchors，行为不变。
pub fn predict_extensions(
    model: &SentenceModel,
    context: &str,
    prefix: &str,
    options: &PredictOptions,
) -> Vec<(String, f64)> {
    if options.max_new == 0 || options.beam == 0 {
        return Vec::new();
    }
    let context = last_characters(context, options.context_chars);
    let prefix_chars = prefix.chars().count();

    // (续写中的完整文本, 累计 log P, 已续写的字符数)
    let mut hypotheses: Vec<(String, f64, usize)> = vec![(prefix.to_string(), 0.0, 0)];

    for _ in 0..options.max_new {
        let mut next: Vec<(String, f64, usize)> = Vec::with_capacity(hypotheses.len() * options.beam);
        for (text, sum, generated) in &hypotheses {
            let probe = format!("{context}{text}");
            for (character, log_prob) in model.top_next_chars(&probe, options.beam) {
                if !predictable(character) {
                    continue;
                }
                let mut extended = String::with_capacity(text.len() + 3);
                extended.push_str(text);
                extended.push(character);
                next.push((extended, sum + log_prob as f64, generated + 1));
            }
        }
        if next.is_empty() {
            break;
        }
        // 平均分排序:同一批假设长度相同,但裁剪标准要统一 —— 之后若
        // 允许变长续写,这里不必再改。
        next.sort_by(|a, b| {
            (b.1 / b.2 as f64)
                .partial_cmp(&(a.1 / a.2 as f64))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        next.truncate(options.beam);
        hypotheses = next;
    }

    let mut out: Vec<(String, f64)> = Vec::new();
    for (text, sum, generated) in hypotheses {
        if generated == 0 {
            continue;
        }
        let avg = sum / generated as f64;
        if avg < options.min_avg_score {
            continue;
        }
        let extension: String = text.chars().skip(prefix_chars).collect();
        if extension.is_empty() || out.iter().any(|(existing, _)| *existing == extension) {
            continue;
        }
        out.push((extension, avg));
    }
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    out.truncate(options.max_results);
    out
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
