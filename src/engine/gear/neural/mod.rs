//! 神经整句重排——字符级 Transformer 推理(移植自 msime `neural/`)。
//!
//! # 模型
//!
//! Decoder-only Pre-LN Transformer, tied input/output embeddings。
//! 权重 ~25MB safetensors 来自 chinese-ime-lm 项目(不需要训练,加载即用)。
//!
//! # 性能关键设计(水杉原文注释)
//!
//! 1. 权重 ~25MB 不进 cache → forward 是内存带宽受限 → 每次 matmul 必须流整矩阵
//! 2. PrefixCache: 上下文的逐层 K/V 缓存,上下文不变就不重算
//! 3. 批量推理: 所有候选拼成一个 batch 过每层,权重矩阵只流一次
//!
//! # 数值要求(必须与参考实现一致,否则结果是垃圾)
//!
//! * GELU: 精确 erf 版(不是 tanh 近似——模型不是用 tanh 训练的)
//! * LayerNorm eps = 1e-5
//! * attention scale = 1/sqrt(n_embd / n_head)
//! * int8 per-output-row symmetric dequant(scale 在 dot 后乘一次)
//! * position i 预测 token i+1

pub mod decoder;
pub mod matrix;
pub mod model;
pub mod predict_filter;
pub mod rescore_filter;
pub mod safetensors;

pub use decoder::{
    predict_extensions, rerank_order, shared_sentence_model, LazySentenceModel, PredictOptions,
    RerankOptions,
};
pub use model::SentenceModel;
pub use predict_filter::NeuralPredictFilter;
pub use rescore_filter::NeuralRescoreFilter;
