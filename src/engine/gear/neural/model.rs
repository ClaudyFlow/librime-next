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
        let mut scratch = Vec::new();
        linear_into(
            &normed,
            &block.fc_weight,
            &block.fc_bias,
            n_embd,
            4 * n_embd,
            &mut ff_hidden,
            &mut scratch,
        );
        for value in &mut ff_hidden {
            *value = super::matrix::gelu(*value);
        }
        let mut ff_out = Vec::new();
        linear_into(
            &ff_hidden,
            &block.out_weight,
            &block.out_bias,
            4 * n_embd,
            n_embd,
            &mut ff_out,
            &mut scratch,
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
            record.keys = vec![Vec::new(); self.blocks.len()];
            record.values = vec![Vec::new(); self.blocks.len()];
            record.length = past_len + width;
        }
        for (layer, block) in self.blocks.iter().enumerate() {
            let mut normed = Vec::new();
            layer_norm_into(&x, rows, &block.ln1_weight, &block.ln1_bias, n_embd, &mut normed);
            let mut qkv = Vec::new();
            let mut scratch = Vec::new();
            linear_into(
                &normed,
                &block.qkv_weight,
                &block.qkv_bias,
                n_embd,
                3 * n_embd,
                &mut qkv,
                &mut scratch,
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
            linear_into(
                &attended,
                &block.proj_weight,
                &block.proj_bias,
                n_embd,
                n_embd,
                &mut projected,
                &mut scratch,
            );
            for (xv, pv) in x.iter_mut().zip(projected) {
                *xv += pv;
            }
            self.feed_forward(&mut x, block);
            if let Some(record) = record.as_deref_mut() {
                record.keys[layer] = k;
                record.values[layer] = v;
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

    /// log-softmax over 全词表(单个隐状态)。
    fn distribution(&self, hidden: &[f32]) -> Vec<f32> {
        let n_embd = self.config.n_embd;
        let vocab = self.config.vocab;
        let mut logits = vec![0.0f32; vocab];
        let mut highest = f32::NEG_INFINITY;
        for (row, logit) in logits.iter_mut().enumerate() {
            *logit = self.token_embedding.dot_row(hidden, row, n_embd);
            highest = highest.max(*logit);
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
        let limit = self.config.context;
        let mut ids = vec![BOS];
        ids.extend(self.encode(text));
        // 超出窗口从左侧截断,给末 token 留一位。
        let room = limit.saturating_sub(1);
        let start = ids.len().saturating_sub(room + 1);
        ids.drain(..start);
        let Some(&last) = ids.last() else {
            return Vec::new();
        };
        let head = &ids[..ids.len() - 1];
        let prefix = self.build_prefix(head);
        let hidden = self.forward(&[last], 1, 1, Some(&prefix), None);
        self.distribution(&hidden[..self.config.n_embd])
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
