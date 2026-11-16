//! 字符级 Transformer 整句模型——`sentence_model.cpp`(水杉,Apache-2.0)的忠实移植。
//!
//! decoder-only Pre-LN Transformer,tied input/output embeddings。给定已上屏上下文,
//! 产出下一个字符的 log 概率分布;IME 真正用的原语是 [`SentenceModel::score_sentences`]
//! —— 对若干已成形句子一次算 log P(sentence | context),交给重排器给词格 n-best 路径排序。
//!
//! # 性能关键(两条都比算术本身重要)
//!
//! 1. 权重 ~25MB 进不了 cache → forward 是内存带宽受限 → matmul 输出主序(output-major),
//!    一批候选过一层时每个权重行只从内存流一次;
//! 2. PrefixCache:上下文逐层 K/V 缓存,上下文不变就不重算;所有候选拼成一个 batch,
//!    位置从缓存长度起算,行与行互不可见(因果掩码)。
//!
//! # 数值红线(与参考实现不一致结果就是垃圾)
//!
//! * 精确 erf 版 GELU(不是 tanh 近似——模型不是用它训练的);
//! * LayerNorm eps = 1e-5;
//! * attention scale = 1/sqrt(n_embd / n_head);
//! * int8 per-output-row 对称反量化(scale 在 dot 之后乘一次,不折进权重);
//! * 位置 i 预测 token i+1。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use super::matrix::Matrix;
use super::safetensors::{DType, Tensor};

/// `<unk>` 的固定 token id(格式规范定义)。
const UNK: u32 = 1;
/// `<bos>` 的固定 token id。
const BOS: u32 = 2;
/// `target_log_probs` 里"该位置没有目标"的哨兵。
const NO_TARGET: u32 = u32::MAX;

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub vocab: usize,
    pub n_layer: usize,
    pub n_head: usize,
    pub n_embd: usize,
    pub context: usize,
}

struct Block {
    ln1_weight: Vec<f32>,
    ln1_bias: Vec<f32>,
    qkv_weight: Matrix,
    qkv_bias: Vec<f32>,
    proj_weight: Matrix,
    proj_bias: Vec<f32>,
    ln2_weight: Vec<f32>,
    ln2_bias: Vec<f32>,
    fc_weight: Matrix,
    fc_bias: Vec<f32>,
    out_weight: Matrix,
    out_bias: Vec<f32>,
}

/// 一段前导 token 的逐层 K/V 缓存,各 `[length, n_embd]` 行主序。
/// 后续 batch 的每一行都看同一份缓存,共享上下文只付一次代价。
#[derive(Default)]
pub struct PrefixCache {
    pub keys: Vec<Vec<f32>>,
    pub values: Vec<Vec<f32>>,
    /// 已覆盖的 token 数,即下一个 token 的位置。
    pub length: usize,
}

/// 受互斥锁保护的 prefix 缓存状态(模型是 `&self` 使用的,要线程安全)。
#[derive(Default)]
struct CacheState {
    tokens: Vec<u32>,
    prefix: PrefixCache,
}

pub struct SentenceModel {
    config: Config,
    /// 单码点 → token id(保留 token 如 `<pad>` 按常量寻址,不进索引)。
    index: HashMap<char, u32>,
    /// token id → 单码点(调试/展示用反查)。
    chars: Vec<Option<char>>,
    /// `tok.weight`——既当 embedding 表也当输出投影(tied)。
    token_embedding: Matrix,
    position: Vec<f32>,
    blocks: Vec<Block>,
    final_weight: Vec<f32>,
    final_bias: Vec<f32>,
    attribution: String,
    license: String,
    cache: Mutex<CacheState>,
}

/// out[m][o] = dot(x[m], w[o]) + bias[o],两侧都是行主序。
///
/// **输出主序是故意的**:权重远大于 cache,如果行在外层,每个输入行都把整个矩阵
/// 从内存重流一遍,forward 全程跑在 DRAM 带宽上。输出在外层时每个权重行只读一次、
/// 跨 batch 复用——这就是"一批候选不比一个贵多少"的原因。
fn linear_into(
    x: &[f32],
    w: &Matrix,
    bias: &[f32],
    inputs: usize,
    outputs: usize,
    out: &mut Vec<f32>,
    scratch: &mut Vec<f32>,
) {
    let rows = if inputs == 0 { 0 } else { x.len() / inputs };
    out.clear();
    out.resize(rows * outputs, 0.0);
    if rows == 0 || inputs == 0 {
        return;
    }
    if w.quantized && scratch.len() < inputs {
        scratch.resize(inputs, 0.0);
    }
    for o in 0..outputs {
        // 量化行的 int8 每个输出行只展宽一次(而不是每 (输出行, 输入行) 一次)。
        // scale 仍然乘在点积结果上:展宽是精确的,提前折进权重会引入舍入。
        let (weights, scale): (&[f32], f32) = if w.quantized {
            let source = &w.ints[o * inputs..o * inputs + inputs];
            for (i, &v) in source.iter().enumerate() {
                scratch[i] = v as f32;
            }
            (scratch.as_slice(), w.scales[o])
        } else {
            (&w.floats[o * inputs..o * inputs + inputs], 1.0)
        };
        let b = bias.first().map_or(0.0, |_| bias[o]);
        for row in 0..rows {
            let acc = super::matrix::dot_f32_f32(&x[row * inputs..row * inputs + inputs], weights);
            out[row * outputs + o] = acc * scale + b;
        }
    }
}

/// 每次 matmul 最多给几个线程。
///
/// * `WEASEL_NEURAL_THREADS=1` 强制串行（回归/调试/低配机）；
/// * 缺省取 `available_parallelism()`，封顶 6（见 [`thread_cap`]）；
/// * 小矩阵（MAC 量不到 [`MT_MIN_MAC`]）直接串行，线程开销比算的还贵。
fn matmul_threads(mac: usize) -> usize {
    matmul_threads_at(mac, MT_MIN_MAC)
}

/// 全词表投影的线程数：输出只有 1~3 行，单次矩阵很"扁"，门槛比 matmul 低一些。
fn projection_threads(mac: usize) -> usize {
    matmul_threads_at(mac, MT_MIN_PROJECTION_MAC)
}

fn matmul_threads_at(mac: usize, min_mac: usize) -> usize {
    let cap = thread_cap();
    if cap <= 1 || mac < min_mac {
        return 1;
    }
    cap
}

/// 线程数上限：`WEASEL_NEURAL_THREADS` 可强制（1 = 串行），缺省按核数、封顶 6。
///
/// 封顶 6 是实测结果（本机 16 逻辑核、宿主有别的负载）：desk 模型 64 字上文
/// 从 4 线程的 62.6ms 降到 6 线程的 55.0ms，8 线程不再涨（多的只是每线程缓冲）。
/// 上屏联想同时还有 key 模型占一条线程，再往上加只抢核。
fn thread_cap() -> usize {
    static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("WEASEL_NEURAL_THREADS")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|n| *n >= 1)
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
            })
            .clamp(1, 6)
    })
}

/// 固定的后台小线程池。
///
/// 为什么不直接 `thread::scope`：一次 forward 有 8 层 × 4 个矩阵乘 + 若干投影，
/// 每次新建/回收线程在 Windows 上实测要一两百微秒，32 次就把省下的时间吃掉一半
/// （key 模型那种小矩阵甚至净亏）。池子只在第一次用时建，之后只付提交任务的几微秒。
fn worker_pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: std::sync::OnceLock<Option<rayon::ThreadPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let cap = thread_cap();
        if cap <= 1 {
            return None;
        }
        rayon::ThreadPoolBuilder::new()
            .num_threads(cap)
            .thread_name(|i| format!("weasel-neural-{i}"))
            .build()
            .ok()
    })
    .as_ref()
}

/// 低于这个 MAC 量就别开线程：一次任务提交 + 每线程缓冲的固定开销要几十微秒，
/// 小矩阵算不回来。
///
/// 250k 是本机扫描出来的（6M / 3M / 2M / 1M / 500k / 250k / 100k，交错 A/B 各跑
/// 两轮，用同一次运行里的点积校准扣掉宿主负载漂移）：3 行批量解码的每个矩阵只有
/// 0.6M~2.4M MAC，6M 门槛把整段逼回串行——desk `step1` 8.5~9.0ms、降到 250k 后
/// 4.9~5.2ms，key 模型 `step1` 1.7~1.9ms→1.3~1.5ms。再往下降（100k）key 反而
/// 回涨（5.8ms），所以 250k 是这一档的底。
const MT_MIN_MAC: usize = 250_000;

/// 投影全是 `vocab × n_embd` 的扁矩阵（1~3 行），单次约 1~4ms，门槛比 matmul 低。
const MT_MIN_PROJECTION_MAC: usize = 3_000_000;

/// 多线程版 [`linear_into`]：**按输出列**切块，各任务只算自己那段列、写各自的
/// 小缓冲，主线程再按行拷回行主序输出。权重因此每行只流一次（不像按行切那样每个
/// 线程重流一遍整个矩阵，实测那样几乎没收益）。
///
/// 每个输出元素的点积与 [`linear_into`] 完全相同，结果逐位一致（见 `mt_linear` 测试）；
/// `threads <= 1` 或线程池不可用时退回 [`linear_into`]。
fn linear_into_mt(
    x: &[f32],
    w: &Matrix,
    bias: &[f32],
    inputs: usize,
    outputs: usize,
    out: &mut Vec<f32>,
    threads: usize,
) {
    let rows = if inputs == 0 { 0 } else { x.len() / inputs };
    if threads <= 1 || rows == 0 || inputs == 0 {
        let mut scratch = Vec::new();
        linear_into(x, w, bias, inputs, outputs, out, &mut scratch);
        return;
    }
    out.clear();
    out.resize(rows * outputs, 0.0);
    let Some(pool) = worker_pool() else {
        let mut scratch = Vec::new();
        linear_into(x, w, bias, inputs, outputs, out, &mut scratch);
        return;
    };
    let per = outputs.div_ceil(threads);
    // 每个任务算一段输出列、写自己的小缓冲；主线程按行拷回去。这样是纯安全代码，
    // 也不用把 `&mut` 切给多个线程（列方向切开在行主序里不是连续切片）。
    let parts: Vec<(usize, Vec<f32>)> = pool.install(|| {
        use rayon::prelude::*;
        (0..threads)
            .filter(|t| t * per < outputs)
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|t| {
                let first = t * per;
                let take = outputs.min(first + per) - first;
                let mut buf = vec![0.0f32; rows * take];
                let mut scratch = if w.quantized {
                    vec![0.0f32; inputs]
                } else {
                    Vec::new()
                };
                for o in first..first + take {
                    let (weights, scale): (&[f32], f32) = if w.quantized {
                        let source = &w.ints[o * inputs..o * inputs + inputs];
                        for (i, &v) in source.iter().enumerate() {
                            scratch[i] = v as f32;
                        }
                        (scratch.as_slice(), w.scales[o])
                    } else {
                        (&w.floats[o * inputs..o * inputs + inputs], 1.0)
                    };
                    let b = bias.get(o).copied().unwrap_or(0.0);
                    for row in 0..rows {
                        let acc = super::matrix::dot_f32_f32(
                            &x[row * inputs..row * inputs + inputs],
                            weights,
                        );
                        buf[row * take + (o - first)] = acc * scale + b;
                    }
                }
                (first, buf)
            })
            .collect()
    });
    for (first, buf) in parts {
        let take = buf.len() / rows;
        for row in 0..rows {
            out[row * outputs + first..row * outputs + first + take]
                .copy_from_slice(&buf[row * take..(row + 1) * take]);
        }
    }
}

fn layer_norm_into(
    x: &[f32],
    rows: usize,
    weight: &[f32],
    bias: &[f32],
    width: usize,
    out: &mut Vec<f32>,
) {
    out.clear();
    out.resize(rows * width, 0.0);
    for row in 0..rows {
        let source = &x[row * width..row * width + width];
        let target = &mut out[row * width..row * width + width];
        let mut mean = 0.0f32;
        for &v in source {
            mean += v;
        }
        mean /= width as f32;
        let mut variance = 0.0f32;
        for &v in source {
            let d = v - mean;
            variance += d * d;
        }
        variance /= width as f32;
        let inverse = 1.0 / (variance + 1e-5f32).sqrt();
        for i in 0..width {
            target[i] = (source[i] - mean) * inverse * weight[i] + bias[i];
        }
    }
}

fn split_qkv(qkv: &[f32], rows: usize, n_embd: usize) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    let mut q = vec![0.0f32; rows * n_embd];
    let mut k = vec![0.0f32; rows * n_embd];
    let mut v = vec![0.0f32; rows * n_embd];
    for row in 0..rows {
        let source = &qkv[row * 3 * n_embd..(row + 1) * 3 * n_embd];
        q[row * n_embd..(row + 1) * n_embd].copy_from_slice(&source[..n_embd]);
        k[row * n_embd..(row + 1) * n_embd].copy_from_slice(&source[n_embd..2 * n_embd]);
        v[row * n_embd..(row + 1) * n_embd].copy_from_slice(&source[2 * n_embd..]);
    }
    (q, k, v)
}

fn product(shape: &[usize]) -> usize {
    shape.iter().product()
}

/// 从(已解析的)safetensors 张量集搭建模型。任何畸形输入都返回 `None`
/// ——缺模型或模型坏了,输入法照常工作,只是没有神经重排。
fn build(meta: &HashMap<String, String>, tensors: &HashMap<String, Tensor>) -> Option<SentenceModel> {
    let config_text = meta.get("config")?;
    let vocab_text = meta.get("vocab")?;
    let config_json: serde_json::Value = serde_json::from_str(config_text).ok()?;
    let config_uint = |key: &str| -> Option<usize> {
        config_json.get(key)?.as_u64().map(|n| n as usize)
    };
    let config = Config {
        vocab: config_uint("vocab")?,
        n_layer: config_uint("n_layer")?,
        n_head: config_uint("n_head")?,
        n_embd: config_uint("n_embd")?,
        context: config_uint("context")?,
    };
    if config.n_head == 0
        || config.n_embd == 0
        || config.n_embd % config.n_head != 0
        || config.vocab == 0
        || config.n_layer == 0
        || config.context == 0
    {
        return None;
    }

    let vocab_json: serde_json::Value = serde_json::from_str(vocab_text).ok()?;
    let vocab_array = vocab_json.as_array()?;
    if vocab_array.len() != config.vocab {
        return None;
    }
    let mut index = HashMap::new();
    let mut chars = vec![None; config.vocab];
    for (id, item) in vocab_array.iter().enumerate() {
        let Some(text) = item.as_str() else { continue };
        // 保留 token(<pad> 等)按常量寻址;单码点才是字符词表。
        let mut it = text.chars();
        if let (Some(ch), None) = (it.next(), it.next()) {
            index.insert(ch, id as u32);
            chars[id] = Some(ch);
        }
    }

    // 张量实体化:按名字取,检查形状与数据长度一致。int8 还要求 `.scale` 伙伴张量。
    let materialize = |name: &str| -> Option<Matrix> {
        let entry = tensors.get(name)?;
        let expected = product(&entry.shape);
        let matrix = match entry.dtype {
            DType::F32 | DType::F16 => {
                let floats = entry.as_f32();
                if floats.len() != expected {
                    return None;
                }
                Matrix {
                    quantized: false,
                    floats,
                    ints: Vec::new(),
                    scales: Vec::new(),
                }
            }
            DType::I8 => {
                let scale_entry = tensors.get(&format!("{name}.scale"))?;
                let scales = scale_entry.as_f32();
                let rows = entry.shape.first().copied().unwrap_or(1);
                if rows == 0 || scales.len() != rows || entry.data.len() != expected {
                    return None;
                }
                Matrix {
                    quantized: true,
                    floats: Vec::new(),
                    ints: entry.data.iter().map(|&b| b as i8).collect(),
                    scales,
                }
            }
        };
        Some(matrix)
    };
    let materialize_floats = |name: &str| -> Option<Vec<f32>> {
        let m = materialize(name)?;
        if m.quantized {
            return None;
        }
        Some(m.floats)
    };

    let token_embedding = materialize("tok.weight")?;
    let position = materialize_floats("pos.weight")?;
    let mut blocks = Vec::with_capacity(config.n_layer);
    for layer in 0..config.n_layer {
        let p = format!("blocks.{layer}.");
        blocks.push(Block {
            ln1_weight: materialize_floats(&format!("{p}ln1.weight"))?,
            ln1_bias: materialize_floats(&format!("{p}ln1.bias"))?,
            qkv_weight: materialize(&format!("{p}qkv.weight"))?,
            qkv_bias: materialize_floats(&format!("{p}qkv.bias"))?,
            proj_weight: materialize(&format!("{p}proj.weight"))?,
            proj_bias: materialize_floats(&format!("{p}proj.bias"))?,
            ln2_weight: materialize_floats(&format!("{p}ln2.weight"))?,
            ln2_bias: materialize_floats(&format!("{p}ln2.bias"))?,
            fc_weight: materialize(&format!("{p}fc.weight"))?,
            fc_bias: materialize_floats(&format!("{p}fc.bias"))?,
            out_weight: materialize(&format!("{p}out.weight"))?,
            out_bias: materialize_floats(&format!("{p}out.bias"))?,
        });
    }
    let final_weight = materialize_floats("ln_f.weight")?;
    let final_bias = materialize_floats("ln_f.bias")?;

    // 几何校验:任何不匹配都说明文件畸形,拒绝。
    let n_embd = config.n_embd;
    if token_embedding.len() != config.vocab * n_embd
        || position.len() != config.context * n_embd
        || final_weight.len() != n_embd
        || final_bias.len() != n_embd
    {
        return None;
    }
    for block in &blocks {
        if block.qkv_weight.len() != 3 * n_embd * n_embd
            || block.proj_weight.len() != n_embd * n_embd
            || block.fc_weight.len() != 4 * n_embd * n_embd
            || block.out_weight.len() != 4 * n_embd * n_embd
        {
            return None;
        }
    }

    Some(SentenceModel {
        config,
        index,
        chars,
        token_embedding,
        position,
        blocks,
        final_weight,
        final_bias,
        attribution: meta.get("attribution").cloned().unwrap_or_default(),
        license: meta.get("license").cloned().unwrap_or_default(),
        cache: Mutex::new(CacheState::default()),
    })
}

impl SentenceModel {
    /// 从内存中的 safetensors 字节加载。
    pub fn load(bytes: &[u8]) -> Option<SentenceModel> {
        if bytes.len() < 8 {
            return None;
        }
        let header_len = u64::from_le_bytes(bytes[..8].try_into().ok()?) as usize;
        let header_end = 8usize.checked_add(header_len)?;
        if header_len == 0 || header_end > bytes.len() {
            return None;
        }
        let header_json: serde_json::Value =
            serde_json::from_slice(&bytes[8..header_end]).ok()?;
        let data_offset = header_end;
        let raw = &bytes[data_offset..];

        let mut meta: HashMap<String, String> = HashMap::new();
        let mut tensors: HashMap<String, Tensor> = HashMap::new();
        let map = header_json.as_object()?;
        for (key, value) in map {
            if key == "__metadata__" {
                let m = value.as_object()?;
                for (k, v) in m {
                    meta.insert(k.clone(), v.as_str()?.to_string());
                }
                continue;
            }
            let dtype_str = value.get("dtype")?.as_str()?;
            let shape: Vec<usize> = value
                .get("shape")
                .and_then(|s| s.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_u64().map(|n| n as usize)).collect())
                .unwrap_or_default();
            let offsets = value.get("data_offsets")?.as_array()?;
            let begin = offsets.first()?.as_u64()? as usize;
            let end = offsets.get(1)?.as_u64()? as usize;
            if data_offset + end > bytes.len() || begin > end {
                return None;
            }
            let dtype = match dtype_str {
                "F32" => DType::F32,
                "F16" => DType::F16,
                "I8" => DType::I8,
                _ => continue,
            };
            tensors.insert(
                key.clone(),
                Tensor {
                    dtype,
                    shape,
                    data: raw[begin..end].to_vec(),
                },
            );
        }
        build(&meta, &tensors)
    }

    /// 从磁盘加载。文件缺失或损坏返回 `None`。
    pub fn load_file(path: &Path) -> Option<SentenceModel> {
        let bytes = std::fs::read(path).ok()?;
        Self::load(&bytes)
    }

    pub fn context_length(&self) -> usize {
        self.config.context
    }

    pub fn attribution(&self) -> &str {
        &self.attribution
    }

    pub fn license(&self) -> &str {
        &self.license
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// 模型为某字符(Unicode 码点)使用的 token id。
    pub fn token(&self, character: char) -> Option<u32> {
        self.index.get(&character).copied()
    }

    /// token id → 字符(单码点词表项的反查;保留 token 返回 `None`)。
    pub fn char_for_token(&self, token_id: usize) -> Option<char> {
        self.chars.get(token_id).copied().flatten()
    }

    fn encode(&self, text: &str) -> Vec<u32> {
        text.chars()
            .map(|ch| self.index.get(&ch).copied().unwrap_or(UNK))
            .collect()
    }

    /// 嵌入:`token embedding + position embedding`。batch 展平后每行位置从
    /// `offset` 重新起算(每行都是同一份缓存前缀的独立续写,不是接在上一行后)。
    fn embed(&self, tokens: &[u32], width: usize, offset: usize) -> Vec<f32> {
        let n_embd = self.config.n_embd;
        let mut x = vec![0.0f32; tokens.len() * n_embd];
        let mut token_row = vec![0.0f32; n_embd];
        for (step, &tok) in tokens.iter().enumerate() {
            let token = (tok as usize).min(self.config.vocab - 1);
            let within = if width == 0 { step } else { step % width };
            let position = (offset + within).min(self.config.context - 1);
            self.token_embedding.row_into(token, n_embd, &mut token_row);
            let position_row = &self.position[position * n_embd..(position + 1) * n_embd];
            let row = &mut x[step * n_embd..(step + 1) * n_embd];
            for i in 0..n_embd {
                row[i] = token_row[i] + position_row[i];
            }
        }
        x
    }

    /// 因果注意力,分两段跑:整段共享前缀,然后本行自己的 token(到 `step` 为止,
    /// 含)。行与行互不可见。输出累加进 `out`(调用方保证已清零)。
    #[allow(clippy::too_many_arguments)]
    fn attention(
        &self,
        q: &[f32],
        k: &[f32],
        v: &[f32],
        batch: usize,
        width: usize,
        past_k: Option<&[f32]>,
        past_v: Option<&[f32]>,
        past_len: usize,
        out: &mut [f32],
        scores: &mut Vec<f32>,
    ) {
        let n_embd = self.config.n_embd;
        let heads = self.config.n_head;
        let head_width = n_embd / heads;
        let scale = 1.0 / (head_width as f32).sqrt();
        scores.clear();
        scores.resize(past_len + width, 0.0);
        for row in 0..batch {
            let base_row = row * width;
            for step in 0..width {
                let visible_self = step + 1;
                let visible = past_len + visible_self;
                for head in 0..heads {
                    let base = head * head_width;
                    let query = &q[(base_row + step) * n_embd + base..][..head_width];
                    let mut highest = f32::NEG_INFINITY;
                    if let (Some(pk), Some(_)) = (past_k, past_v) {
                        for j in 0..past_len {
                            let s = super::matrix::dot_f32_f32(query, &pk[j * n_embd + base..][..head_width]) * scale;
                            scores[j] = s;
                            highest = highest.max(s);
                        }
                    }
                    for j in 0..visible_self {
                        let s = super::matrix::dot_f32_f32(
                            query,
                            &k[(base_row + j) * n_embd + base..][..head_width],
                        ) * scale;
                        scores[past_len + j] = s;
                        highest = highest.max(s);
                    }
                    let mut sum = 0.0f32;
                    for score in &mut scores[..visible] {
                        *score = (*score - highest).exp();
                        sum += *score;
                    }
                    let target = &mut out[(base_row + step) * n_embd + base..][..head_width];
                    if let (Some(pk), Some(pv)) = (past_k, past_v) {
                        for j in 0..past_len {
                            let weight = scores[j] / sum;
                            let value = &pv[j * n_embd + base..][..head_width];
                            for (t, &val) in target.iter_mut().zip(value) {
                                *t += weight * val;
                            }
                        }
                    }
                    for j in 0..visible_self {
                        let weight = scores[past_len + j] / sum;
                        let value = &v[(base_row + j) * n_embd + base..][..head_width];
                        for (t, &val) in target.iter_mut().zip(value) {
                            *t += weight * val;
                        }
                    }
                }
            }
        }
    }

    /// FFN 子层:LN → fc → GELU → out,残差加回 `x`。
    fn feed_forward(&self, x: &mut [f32], block: &Block) {
        let n_embd = self.config.n_embd;
        let rows = x.len() / n_embd;
        let mut normed = Vec::new();
        layer_norm_into(x, rows, &block.ln2_weight, &block.ln2_bias, n_embd, &mut normed);
        let mut ff_hidden = Vec::new();
        linear_into_mt(
            &normed,
            &block.fc_weight,
            &block.fc_bias,
            n_embd,
            4 * n_embd,
            &mut ff_hidden,
            matmul_threads(normed.len() * 4 * n_embd),
        );
        for value in &mut ff_hidden {
            *value = super::matrix::gelu(*value);
        }
        let mut ff_out = Vec::new();
        linear_into_mt(
            &ff_hidden,
            &block.out_weight,
            &block.out_bias,
            4 * n_embd,
            n_embd,
            &mut ff_out,
            matmul_threads(ff_hidden.len() * n_embd),
        );
        for (xv, fv) in x.iter_mut().zip(ff_out) {
            *xv += fv;
        }
    }

    /// 跑 `batch` 行、每行 `width` 个 token(行主序、调用方已补齐等长),位置从
    /// `past_len` 开始,每行额外 attend 到 `past`。返回最终 LayerNorm 后的隐状态,
    /// `[batch * width, n_embd]`。
    ///
    /// `record` 非空时把本次产生的 K/V 存进去——这要求 batch == 1
    /// (缓存被后续行共享,只能描述一条序列)。
    fn forward(
        &self,
        tokens: &[u32],
        batch: usize,
        width: usize,
        past: Option<&PrefixCache>,
        mut record: Option<&mut PrefixCache>,
    ) -> Vec<f32> {
        let n_embd = self.config.n_embd;
        let rows = batch * width;
        let past_len = past.map_or(0, |p| p.length);
        let mut x = self.embed(tokens, width, past_len);
        if let Some(record) = record.as_deref_mut() {
            // 有 past 时把它的 K/V 拷进来再追加本段。这样 width=1 的"续接"
            // 得到的缓存与一次性 `build_prefix` 同一段文本的缓存完全等价，
            // 下一步批量解码可以直接复用，不必把整段上文重算一遍。
            // `build_prefix([])` 返回的是空缓存（keys 连一层都没有），按"无 past"处理。
            let seeded = past.filter(|p| p.keys.len() == self.blocks.len());
            record.keys = match seeded {
                Some(p) => p.keys.clone(),
                None => vec![Vec::new(); self.blocks.len()],
            };
            record.values = match seeded {
                Some(p) => p.values.clone(),
                None => vec![Vec::new(); self.blocks.len()],
            };
            record.length = past_len + width;
        }
        for (layer, block) in self.blocks.iter().enumerate() {
            let mut normed = Vec::new();
            layer_norm_into(&x, rows, &block.ln1_weight, &block.ln1_bias, n_embd, &mut normed);
            let mut qkv = Vec::new();
            linear_into_mt(
                &normed,
                &block.qkv_weight,
                &block.qkv_bias,
                n_embd,
                3 * n_embd,
                &mut qkv,
                matmul_threads(normed.len() * 3 * n_embd),
            );
            let (q, k, v) = split_qkv(&qkv, rows, n_embd);
            let (past_k, past_v) = match past {
                Some(p) if past_len != 0 => (
                    Some(p.keys[layer].as_slice()),
                    Some(p.values[layer].as_slice()),
                ),
                _ => (None, None),
            };
            let mut attended = vec![0.0f32; rows * n_embd];
            let mut scores = Vec::new();
            self.attention(
                &q,
                &k,
                &v,
                batch,
                width,
                past_k,
                past_v,
                past_len,
                &mut attended,
                &mut scores,
            );
            let mut projected = Vec::new();
            linear_into_mt(
                &attended,
                &block.proj_weight,
                &block.proj_bias,
                n_embd,
                n_embd,
                &mut projected,
                matmul_threads(attended.len() * n_embd),
            );
            for (xv, pv) in x.iter_mut().zip(projected) {
                *xv += pv;
            }
            self.feed_forward(&mut x, block);
            if let Some(record) = record.as_deref_mut() {
                record.keys[layer].extend_from_slice(&k);
                record.values[layer].extend_from_slice(&v);
            }
        }
        let mut normed = Vec::new();
        layer_norm_into(&x, rows, &self.final_weight, &self.final_bias, n_embd, &mut normed);
        normed
    }

    /// 为 `tokens` 建 K/V 缓存(视为从位置 0 开始)。batch == 1、位置 0——
    /// 唯一合法的建缓存入口。
    fn build_prefix(&self, tokens: &[u32]) -> PrefixCache {
        let mut cache = PrefixCache::default();
        if tokens.is_empty() {
            return cache;
        }
        self.forward(tokens, 1, tokens.len(), None, Some(&mut cache));
        cache
    }

    /// 全词表投影的逐 id 点积（输出主序 `logits[row * vocab + id]`）。
    ///
    /// 按 **id 段**切线程：每个 id 只算一次、权重只流一遍；各线程写自己的列，
    /// 下标互不重叠。每个元素仍走同一个 `dot_row`/`dot_f32_f32`，与串行循环
    /// 逐位相同（`mt_linear` 测试比对 `to_bits()`）。
    fn project_vocab_mt(&self, hidden: &[f32], rows: usize, logits: &mut [f32], threads: usize) {
        let n_embd = self.config.n_embd;
        let vocab = self.config.vocab;
        let pool = if threads > 1 && rows > 0 && vocab > 0 {
            worker_pool()
        } else {
            None
        };
        let Some(pool) = pool else {
            self.project_vocab_serial(hidden, rows, logits);
            return;
        };
        let per = vocab.div_ceil(threads);
        let embedding = &self.token_embedding;
        // 同 `linear_into_mt`：按 id 段切、各算各的小缓冲，主线程按行拷回。
        let parts: Vec<(usize, Vec<f32>)> = pool.install(|| {
            use rayon::prelude::*;
            (0..threads)
                .filter(|t| t * per < vocab)
                .collect::<Vec<_>>()
                .into_par_iter()
                .map(|t| {
                    let first = t * per;
                    let take = vocab.min(first + per) - first;
                    let mut buf = vec![0.0f32; rows * take];
                    let mut scratch = if embedding.quantized {
                        vec![0.0f32; n_embd]
                    } else {
                        Vec::new()
                    };
                    for id in first..first + take {
                        let (weights, scale): (&[f32], f32) = if embedding.quantized {
                            let source = &embedding.ints[id * n_embd..id * n_embd + n_embd];
                            for (i, &b) in source.iter().enumerate() {
                                scratch[i] = b as f32;
                            }
                            (scratch.as_slice(), embedding.scales[id])
                        } else {
                            (&embedding.floats[id * n_embd..id * n_embd + n_embd], 1.0)
                        };
                        for row in 0..rows {
                            let acc = super::matrix::dot_f32_f32(
                                &hidden[row * n_embd..row * n_embd + n_embd],
                                weights,
                            );
                            buf[row * take + (id - first)] = acc * scale;
                        }
                    }
                    (first, buf)
                })
                .collect()
        });
        for (first, buf) in parts {
            let take = buf.len() / rows;
            for row in 0..rows {
                logits[row * vocab + first..row * vocab + first + take]
                    .copy_from_slice(&buf[row * take..(row + 1) * take]);
            }
        }
    }

    /// 单线程的全词表投影（并行版的回退，也是数值基准）。
    fn project_vocab_serial(&self, hidden: &[f32], rows: usize, logits: &mut [f32]) {
        let n_embd = self.config.n_embd;
        let vocab = self.config.vocab;
        let embedding = &self.token_embedding;
        let mut scratch = if embedding.quantized {
            vec![0.0f32; n_embd]
        } else {
            Vec::new()
        };
        for id in 0..vocab {
            let (weights, scale): (&[f32], f32) = if embedding.quantized {
                let source = &embedding.ints[id * n_embd..id * n_embd + n_embd];
                for (i, &b) in source.iter().enumerate() {
                    scratch[i] = b as f32;
                }
                (scratch.as_slice(), embedding.scales[id])
            } else {
                (&embedding.floats[id * n_embd..id * n_embd + n_embd], 1.0)
            };
            for row in 0..rows {
                logits[row * vocab + id] = super::matrix::dot_f32_f32(
                    &hidden[row * n_embd..row * n_embd + n_embd],
                    weights,
                ) * scale;
            }
        }
    }

    /// log-softmax over 全词表(单个隐状态)。
    fn distribution(&self, hidden: &[f32]) -> Vec<f32> {
        let n_embd = self.config.n_embd;
        let vocab = self.config.vocab;
        let mut logits = vec![0.0f32; vocab];
        self.project_vocab_mt(hidden, 1, &mut logits, projection_threads(vocab * n_embd));
        let mut highest = f32::NEG_INFINITY;
        for &logit in &logits {
            highest = highest.max(logit);
        }
        let mut sum = 0.0f32;
        for &logit in &logits {
            sum += (logit - highest).exp();
        }
        let offset = highest + sum.ln();
        for logit in &mut logits {
            *logit -= offset;
        }
        logits
    }

    /// 对 `rows` 个隐状态,各算 log P(targets[row])。`NO_TARGET` 的行跳过、报 0。
    /// 输出主序:词表行展宽一次,跨所有活动行复用(和 `linear_into` 同一个道理)。
    fn target_log_probs(
        &self,
        hidden: &[f32],
        rows: usize,
        targets: &[u32],
    ) -> Vec<f32> {
        let n_embd = self.config.n_embd;
        let vocab = self.config.vocab;
        let active_rows: Vec<usize> = (0..rows).filter(|&row| targets[row] != NO_TARGET).collect();
        let mut out = vec![0.0f32; rows];
        if active_rows.is_empty() {
            return out;
        }

        let mut logits = vec![0.0f32; active_rows.len() * vocab];
        let mut scratch = if self.token_embedding.quantized {
            vec![0.0f32; n_embd]
        } else {
            Vec::new()
        };
        for id in 0..vocab {
            let (weights, scale): (&[f32], f32) = if self.token_embedding.quantized {
                let source = &self.token_embedding.ints[id * n_embd..id * n_embd + n_embd];
                for (i, &b) in source.iter().enumerate() {
                    scratch[i] = b as f32;
                }
                (scratch.as_slice(), self.token_embedding.scales[id])
            } else {
                (
                    &self.token_embedding.floats[id * n_embd..id * n_embd + n_embd],
                    1.0,
                )
            };
            for (slot, &row) in active_rows.iter().enumerate() {
                logits[slot * vocab + id] =
                    super::matrix::dot_f32_f32(&hidden[row * n_embd..row * n_embd + n_embd], weights) * scale;
            }
        }

        for (slot, &row) in active_rows.iter().enumerate() {
            let source = &logits[slot * vocab..(slot + 1) * vocab];
            let mut highest = f32::NEG_INFINITY;
            for &logit in source {
                highest = highest.max(logit);
            }
            let mut sum = 0.0f32;
            for &logit in source {
                sum += (logit - highest).exp();
            }
            out[row] = source[targets[row] as usize] - (highest + sum.ln());
        }
        out
    }

    /// log P(下一个字符 | text),全词表分布,按 token id 下标。
    ///
    /// **与 [`Self::score_sentences`] 走同一条实现**:最后一个 token 当"领队"、
    /// 它之前的内容进前缀缓存。早先这里直接对整串做一次无 past 的 forward,
    /// 实测两种路径给出的数值不一致(同一 `logP(们|我)` 差 5 个 nat),
    /// 且无 past 那条几乎不随上文变化 —— 生成会因此全是噪声。
    /// 统一到打分的路径后,`next_log_probabilities(ctx)[c]` 与
    /// `score_sentences(ctx, [c])` 逐位对齐。
    pub fn next_log_probabilities(&self, text: &str) -> Vec<f32> {
        self.next_log_probabilities_with_prefix(text).0
    }

    /// 同 [`Self::next_log_probabilities`]，同时返回把 `text` 全部压进 K/V 的
    /// 前缀缓存。同一段上文的下一步批量解码可直接拿它当 past（实测 desk 模型
    /// 重算一次整段上下文 ≈ 11ms），数值与 `build_prefix` 重建的缓存等价。
    ///
    /// **一次整段前向**完成两件事：末行的隐状态给出"下一个字符"的分布，同一遍
    /// 计算里把整段的 K/V 记进缓存。早先把它拆成 `build_prefix(head)` +
    /// `forward([last], past)`，一次调用要流两遍权重（desk ≈ 10.8ms，其中一半
    /// 是重复的），合并后逐位相同——等价性由本文件 `single_pass_ab` 测试逐元素
    /// 比对（分布、缓存 K/V、长度全量 `to_bits()` 相等）。
    pub fn next_log_probabilities_with_prefix(&self, text: &str) -> (Vec<f32>, PrefixCache) {
        let limit = self.config.context;
        let mut ids = vec![BOS];
        ids.extend(self.encode(text));
        // 超出窗口从左侧截断,给末 token 留一位。
        let room = limit.saturating_sub(1);
        let start = ids.len().saturating_sub(room + 1);
        ids.drain(..start);
        if ids.is_empty() {
            return (Vec::new(), PrefixCache::default());
        }
        let mut cache = PrefixCache::default();
        let hidden = self.forward(&ids, 1, ids.len(), None, Some(&mut cache));
        let n_embd = self.config.n_embd;
        let last_row = &hidden[hidden.len() - n_embd..];
        (self.distribution(last_row), cache)
    }

    /// 2026-11-08 合并前的两次前向实现（`build_prefix(head)` + `forward([last], past)`）。
    ///
    /// 仅供探针做同进程 A/B 与回归测试，线上代码不调用它。保留它是因为跨进程跑
    /// 两次拿到的"前后"数字会被机器噪声污染；同进程交替计时才有意义。
    #[doc(hidden)]
    pub fn next_log_probabilities_two_pass(&self, text: &str) -> (Vec<f32>, PrefixCache) {
        let limit = self.config.context;
        let mut ids = vec![BOS];
        ids.extend(self.encode(text));
        let room = limit.saturating_sub(1);
        let start = ids.len().saturating_sub(room + 1);
        ids.drain(..start);
        let Some(&last) = ids.last() else {
            return (Vec::new(), PrefixCache::default());
        };
        let head = &ids[..ids.len() - 1];
        let prefix = self.build_prefix(head);
        let mut grown = PrefixCache::default();
        let hidden = self.forward(&[last], 1, 1, Some(&prefix), Some(&mut grown));
        (self.distribution(&hidden[..self.config.n_embd]), grown)
    }

    /// 下一个字符概率最高的 `count` 个候选:`(字符, log P)`,按降序。
    ///
    /// 生成式预测的最小原语 —— 预测每走一步都调它一次。保留 token(`<bos>` 等)
    /// 与未登录 id 不会出现在结果里;非有限值(理论上的 -inf)也丢掉。
    ///
    /// 代价:一次完整 forward + 一次全词表投影,与 [`Self::score_sentences`] 的
    /// 单条候选相当(上下文长度相同时)。
    pub fn top_next_chars(&self, text: &str, count: usize) -> Vec<(char, f32)> {
        if count == 0 {
            return Vec::new();
        }
        let log_probs = self.next_log_probabilities(text);
        self.best_chars(&log_probs, count)
    }

    /// 同 [`Self::top_next_chars`]，同时返回覆盖 `text` 的前缀缓存（供下一步复用）。
    pub fn top_next_chars_with_prefix(
        &self,
        text: &str,
        count: usize,
    ) -> (Vec<(char, f32)>, PrefixCache) {
        if count == 0 {
            return (Vec::new(), PrefixCache::default());
        }
        let (log_probs, prefix) = self.next_log_probabilities_with_prefix(text);
        (self.best_chars(&log_probs, count), prefix)
    }

    /// 从全词表 log P 里取前 `count` 个可预测字符。束搜索的每一步、单条和批量
    /// 路径都走这一份实现,保证平票时的挑法逐位一致。
    fn best_chars(&self, log_probs: &[f32], count: usize) -> Vec<(char, f32)> {
        if count == 0 {
            return Vec::new();
        }
        let mut best: Vec<(char, f32)> = Vec::with_capacity(count + 1);
        for (id, &log_prob) in log_probs.iter().enumerate() {
            if !log_prob.is_finite() {
                continue;
            }
            let Some(character) = self.char_for_token(id) else {
                continue;
            };
            if best.len() < count {
                best.push((character, log_prob));
                if best.len() == count {
                    best.sort_by(|a, b| b.1.total_cmp(&a.1));
                }
            } else if log_prob > best[count - 1].1 {
                best[count - 1] = (character, log_prob);
                best.sort_by(|a, b| b.1.total_cmp(&a.1));
            }
        }
        if best.len() < count {
            best.sort_by(|a, b| b.1.total_cmp(&a.1));
        }
        best
    }

    /// 批量版 [`Self::top_next_chars`]:所有行共享 `base` 上下文,`tails` 是各自
    /// 已续写的尾巴(束搜索同一 step 的行等长)。
    ///
    /// 逐行等价于 `top_next_chars(base + tail)`:共享部分只 forward 一次,各行
    /// 的尾巴当作 batch 行、用 `base` 的 K/V 当 past;全词表投影也只展宽一遍、
    /// 跨行复用。束搜索每步最多 3 行,原来每行都把整段上下文重算一遍。
    pub fn top_next_chars_batch(
        &self,
        base: &str,
        tails: &[&str],
        count: usize,
    ) -> Vec<Vec<(char, f32)>> {
        if count == 0 || tails.is_empty() {
            return vec![Vec::new(); tails.len()];
        }
        let limit = self.config.context;
        if limit < 2 {
            return vec![Vec::new(); tails.len()];
        }
        let widths: Vec<usize> = tails.iter().map(|tail| tail.chars().count()).collect();
        let width = widths[0];
        if widths.iter().any(|&w| w != width) || width == 0 {
            // 不应发生(同一 step 的假设等长);退化成逐行调用,保底正确。
            return tails
                .iter()
                .map(|tail| self.top_next_chars(&format!("{base}{tail}"), count))
                .collect();
        }

        let mut shared = vec![BOS];
        shared.extend(self.encode(base));
        // 和 next_log_probabilities 相同:总长不超过窗口,超出从左侧截断。
        let room = limit.saturating_sub(width);
        let shared = if shared.len() > room {
            &shared[shared.len() - room..]
        } else {
            shared.as_slice()
        };
        let prefix = self.build_prefix(shared);
        self.decode_over_prefix(&prefix, tails, width, count)
    }

    /// 批量版 [`Self::top_next_chars`]，但共享段的 K/V 由调用方提供
    /// （通常就是 [`Self::top_next_chars_with_prefix`] 对同一 `base` 的返回）。
    ///
    /// 缓存不够长（下一步尾巴放不进窗口）或尾巴不等长时退回
    /// [`Self::top_next_chars_batch`] 用 `base` 重建，结果不变、只是多算一次。
    pub fn top_next_chars_after_prefix(
        &self,
        base: &str,
        prefix: &PrefixCache,
        tails: &[&str],
        count: usize,
    ) -> Vec<Vec<(char, f32)>> {
        if count == 0 || tails.is_empty() {
            return vec![Vec::new(); tails.len()];
        }
        let width = tails[0].chars().count();
        if width == 0
            || tails.iter().any(|tail| tail.chars().count() != width)
            || prefix.length == 0
            || prefix.length + width > self.config.context
        {
            return self.top_next_chars_batch(base, tails, count);
        }
        self.decode_over_prefix(prefix, tails, width, count)
    }

    /// 拿现成的共享 K/V 解码 `tails` 的下一个字符（`tails` 等长、非空）。
    fn decode_over_prefix(
        &self,
        prefix: &PrefixCache,
        tails: &[&str],
        width: usize,
        count: usize,
    ) -> Vec<Vec<(char, f32)>> {
        let batch = tails.len();
        let mut flat = vec![0u32; batch * width];
        for (row, tail) in tails.iter().enumerate() {
            for (i, id) in self.encode(tail).into_iter().enumerate() {
                flat[row * width + i] = id;
            }
        }
        let hidden = self.forward(&flat, batch, width, Some(prefix), None);

        let n_embd = self.config.n_embd;
        let mut last_hidden = Vec::with_capacity(batch * n_embd);
        for row in 0..batch {
            let start = (row * width + width - 1) * n_embd;
            last_hidden.extend_from_slice(&hidden[start..start + n_embd]);
        }
        let log_probs = self.distributions_batch(&last_hidden, batch);
        let vocab = self.config.vocab;
        (0..batch)
            .map(|row| self.best_chars(&log_probs[row * vocab..(row + 1) * vocab], count))
            .collect()
    }

    /// 对 `rows` 个隐状态各算一份全词表 log-softmax(输出主序,行×词表)。
    ///
    /// 和 [`Self::target_log_probs`] 一样把词表行只展宽一遍、跨行复用;逐行的
    /// 点积与 [`Self::distribution`] 相同,数值逐位一致。
    fn distributions_batch(&self, hidden: &[f32], rows: usize) -> Vec<f32> {
        let n_embd = self.config.n_embd;
        let vocab = self.config.vocab;
        let mut logits = vec![0.0f32; rows * vocab];
        self.project_vocab_mt(
            &hidden[..rows * n_embd],
            rows,
            &mut logits,
            projection_threads(rows * vocab * n_embd),
        );
        for row in 0..rows {
            let source = &mut logits[row * vocab..(row + 1) * vocab];
            let mut highest = f32::NEG_INFINITY;
            for &logit in source.iter() {
                highest = highest.max(logit);
            }
            let mut sum = 0.0f32;
            for &logit in source.iter() {
                sum += (logit - highest).exp();
            }
            let offset = highest + sum.ln();
            for logit in source.iter_mut() {
                *logit -= offset;
            }
        }
        logits
    }

    /// 一次算一批句子的 log P(sentence | context)。
    ///
    /// 每个候选的打分序列是 `<bos> + context + candidate`,所有候选共享除自身尾部外
    /// 的全部内容,所以在共享部分的最后一个 token 处切开:该 token 领进每个 batch 行
    /// (它产出的分布正好给候选首字符打分),只有它之前的内容进缓存(只有那部分不变)。
    ///
    /// 超出上下文窗口的文本从左侧截断;字符不在词表按 `<unk>` 计,不会丢句子。
    ///
    /// 线程安全:prefix 缓存有锁,不同上下文的并发调用结果正确但会互相逐出。
    pub fn score_sentences(&self, context: &str, texts: &[&str]) -> Vec<f64> {
        if texts.is_empty() {
            return Vec::new();
        }
        let limit = self.config.context;
        if limit < 2 {
            return vec![0.0; texts.len()];
        }

        let mut full = vec![BOS];
        full.extend(self.encode(context));

        let mut tails: Vec<Vec<u32>> = Vec::with_capacity(texts.len());
        let mut longest = 0usize;
        for text in texts {
            let mut ids = self.encode(text);
            // 候选比窗口减去领队 token 还长:保留尾部——剩余上下文还能照顾到的部分。
            if ids.len() > limit - 1 {
                let keep = limit - 1;
                ids.drain(..ids.len() - keep);
            }
            longest = longest.max(ids.len());
            tails.push(ids);
        }
        if longest == 0 {
            return vec![0.0; texts.len()];
        }
        // 末个候选 token 只当目标:它的隐状态不预测任何被打分的字符。
        let width = longest;

        let last = *full.last().expect("full 以 BOS 开头,非空");
        let head = &full[..full.len() - 1];
        // 保住旧上下文限额:去掉不用的领队 token 不能改变分数。
        let room = limit.saturating_sub(longest + 1);
        let head = if head.len() > room {
            &head[head.len() - room..]
        } else {
            head
        };

        let batch = tails.len();
        // 短行用 token 0 补齐;补齐位置的 target 是 NO_TARGET,永不读取。
        let mut flat = vec![0u32; batch * width];
        let mut targets = vec![NO_TARGET; batch * width];
        for (row, tail) in tails.iter().enumerate() {
            flat[row * width] = last;
            for (i, &id) in tail.iter().enumerate() {
                // 位置 i 预测 token i+1:领队 token 的槽位给候选首字符打分。
                targets[row * width + i] = id;
                if i + 1 < tail.len() {
                    flat[row * width + 1 + i] = id;
                }
            }
        }

        let mut scored;
        {
            let mut guard = self.cache.lock().expect("neural cache 毒化不可恢复");
            if guard.tokens != head {
                guard.prefix = self.build_prefix(head);
                guard.tokens = head.to_vec();
            }
            let hidden = self.forward(&flat, batch, width, Some(&guard.prefix), None);
            scored = self.target_log_probs(&hidden, batch * width, &targets);
        }

        let mut out = vec![0.0f64; batch];
        for (row, tail) in tails.iter().enumerate() {
            let mut total = 0.0f64;
            for (i, _) in tail.iter().enumerate() {
                total += scored[row * width + i] as f64;
            }
            out[row] = total;
        }
        out
    }
}

#[cfg(test)]
mod single_pass_ab {
    //! 一次性整段前向（新）与 `build_prefix(head)` + `forward([last], past)`（旧）
    //! 的逐位 A/B。两边的分布与缓存 K/V 必须全量 `to_bits()` 相同；
    //! 顺带把分布与打分路径 `score_sentences` 对齐，回答"差 5 个 nat"的旧疑点。

    use super::super::shared_sentence_model;
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    /// 2026-11-08 合并前的原样实现（线上入口 `next_log_probabilities_two_pass` 的别名）。
    fn old_two_pass(model: &SentenceModel, text: &str) -> (Vec<f32>, PrefixCache) {
        model.next_log_probabilities_two_pass(text)
    }

    fn same_bits(a: &[f32], b: &[f32]) -> (usize, f32) {
        let mut diff = 0usize;
        let mut worst = 0.0f32;
        for (x, y) in a.iter().zip(b.iter()) {
            if x.to_bits() != y.to_bits() {
                diff += 1;
                worst = worst.max((x - y).abs());
            }
        }
        (diff, worst)
    }

    fn models() -> Option<(Arc<SentenceModel>, Arc<SentenceModel>)> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-data/model");
        let key = shared_sentence_model(&root.join("sentence-model.safetensors"))?;
        let desk = shared_sentence_model(&root.join("sentence-model-desktop.safetensors"))?;
        Some((key, desk))
    }

    #[test]
    fn single_pass_matches_two_pass_and_scoring() {
        let Some((key, desk)) = models() else {
            eprintln!("跳过：test-data/model 下没有模型");
            return;
        };
        let long: String = std::iter::repeat(
            "春眠不觉晓处处闻啼鸟夜来风雨声花落知多少",
        )
        .take(4)
        .collect();
        let mut texts: Vec<String> = vec![
            String::new(),
            "你".into(),
            "你好".into(),
            "我们".into(),
            "我今天".into(),
            "他刚才说".into(),
            "他刚才说没问题".into(),
            format!("{long}故乡"),
            long.clone(),
        ];
        texts.shrink_to_fit();

        let mut total_lp_diff = 0usize;
        let mut total_cache_diff = 0usize;
        let mut worst_lp = 0.0f32;
        let mut worst_score = 0.0f64;
        for (model, name) in [(&key, "key"), (&desk, "desk")] {
            for text in &texts {
                let (lp_new, cache_new) = model.next_log_probabilities_with_prefix(text);
                let (lp_old, cache_old) = old_two_pass(model, text);
                assert_eq!(lp_new.len(), lp_old.len(), "{name}: 分布长度不一致 text={text:?}");
                let (lp_diff, lp_worst) = same_bits(&lp_new, &lp_old);
                total_lp_diff += lp_diff;
                worst_lp = worst_lp.max(lp_worst);
                if lp_diff != 0 {
                    println!(
                        "LP MISMATCH {name} text={text:?}: {lp_diff}/{} 元素不同，最大 |Δ|={lp_worst:e}",
                        lp_new.len()
                    );
                }
                // 缓存：长度与逐层 K/V 都要一致，下一步批量解码才等价。
                let mut cache_diff = usize::from(cache_new.length != cache_old.length);
                assert_eq!(
                    cache_new.keys.len(),
                    cache_old.keys.len(),
                    "{name}: 缓存层数不一致"
                );
                for (kn, ko) in cache_new.keys.iter().zip(&cache_old.keys) {
                    cache_diff += same_bits(kn, ko).0;
                }
                for (vn, vo) in cache_new.values.iter().zip(&cache_old.values) {
                    cache_diff += same_bits(vn, vo).0;
                }
                total_cache_diff += cache_diff;
                if cache_diff != 0 {
                    println!("CACHE MISMATCH {name} text={text:?}: {cache_diff} 个元素不同");
                }
                // 与打分路径对齐：同一字符的 log P 应逐位相等（旧疑点"差 5 个 nat"）。
                // 只为短上下文取 top1，避免长文本多次整段 forward 把测试拖到分钟级。
                if text.chars().count() <= 20 {
                    for (character, log_prob) in model.best_chars(&lp_new, 1) {
                        let candidate = character.to_string();
                        let scored = model.score_sentences(text, &[candidate.as_str()]);
                        let delta = (scored[0] - log_prob as f64).abs();
                        worst_score = worst_score.max(delta);
                        if delta > 1e-4 {
                            println!(
                                "SCORE MISMATCH {name} text={text:?} char={character:?}: dist={log_prob} score={} Δ={delta}",
                                scored[0]
                            );
                        }
                    }
                }
            }
        }
        println!(
            "single_pass_ab: 分布 {total_lp_diff} 个元素不同（最大 |Δ|={worst_lp:e}），缓存 {total_cache_diff} 个元素不同，分布 vs 打分最大 |Δ|={worst_score:e}"
        );
        assert_eq!(total_lp_diff, 0, "分布不是逐位相同");
        assert_eq!(total_cache_diff, 0, "缓存不是逐位相同");
    }
}

#[cfg(test)]
mod mt_linear {
    //! 多线程 matmul 与单线程必须**逐位**相同（只切输出列，不改累加顺序）。
    //! 上屏联想走的就是 `forward` → `linear_into_mt`，一旦不一致，联想结果
    //! 会随机器核数变化——那比慢几十毫秒严重得多。

    use super::super::matrix::Matrix;
    use super::{linear_into, linear_into_mt};

    fn synth(inputs: usize, outputs: usize) -> (Vec<f32>, Matrix, Vec<f32>) {
        let x: Vec<f32> = (0..7 * inputs)
            .map(|i| ((i % 29) as f32 - 14.0) * 0.037)
            .collect();
        let ints: Vec<i8> = (0..outputs * inputs)
            .map(|i| ((i % 97) as i32 - 48) as i8)
            .collect();
        let scales: Vec<f32> = (0..outputs).map(|o| 1e-3 + o as f32 * 1e-7).collect();
        let w = Matrix {
            quantized: true,
            floats: Vec::new(),
            ints,
            scales,
        };
        let bias: Vec<f32> = (0..outputs).map(|o| o as f32 * 0.001).collect();
        (x, w, bias)
    }

    #[test]
    fn threads_produce_identical_output() {
        // 覆盖：单线程回退、正好整除、不整除（尾部线程分到更少列）。
        for &(inputs, outputs) in &[(16usize, 12usize), (448, 1344), (448, 1792)] {
            let (x, w, bias) = synth(inputs, outputs);
            let mut serial = Vec::new();
            let mut scratch = Vec::new();
            linear_into(&x, &w, &bias, inputs, outputs, &mut serial, &mut scratch);
            for threads in [2usize, 3, 4] {
                let mut parallel = Vec::new();
                linear_into_mt(&x, &w, &bias, inputs, outputs, &mut parallel, threads);
                assert_eq!(serial.len(), parallel.len());
                for (i, (a, b)) in serial.iter().zip(&parallel).enumerate() {
                    assert_eq!(
                        a.to_bits(),
                        b.to_bits(),
                        "inputs={inputs} outputs={outputs} threads={threads} 第 {i} 个元素不一致"
                    );
                }
            }
        }
    }

    #[test]
    fn empty_input_stays_empty() {
        let (_, w, bias) = synth(8, 8);
        let mut out = vec![1.0f32; 3];
        linear_into_mt(&[], &w, &bias, 8, 8, &mut out, 4);
        assert!(out.is_empty());
    }

    /// 全词表投影按 id 段切线程后，logits 必须与原串行循环逐位相同。
    #[test]
    fn vocab_projection_matches_serial_bitwise() {
        use super::super::shared_sentence_model;
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-data/model");
        let Some(model) =
            shared_sentence_model(&root.join("sentence-model-desktop.safetensors"))
        else {
            eprintln!("跳过：test-data/model 下没有 desk 模型");
            return;
        };
        let n_embd = model.config().n_embd;
        let vocab = model.config().vocab;
        for rows in [1usize, 3] {
            let hidden: Vec<f32> = (0..rows * n_embd)
                .map(|i| ((i % 37) as f32 - 18.0) * 0.021)
                .collect();
            let mut serial = vec![0.0f32; rows * vocab];
            let mut parallel = vec![0.0f32; rows * vocab];
            model.project_vocab_mt(&hidden, rows, &mut serial, 1);
            for threads in [2usize, 3, 4] {
                model.project_vocab_mt(&hidden, rows, &mut parallel, threads);
                for (i, (a, b)) in serial.iter().zip(&parallel).enumerate() {
                    assert_eq!(
                        a.to_bits(),
                        b.to_bits(),
                        "rows={rows} threads={threads} 第 {i} 个 logit 不一致"
                    );
                }
            }
        }
    }
}
