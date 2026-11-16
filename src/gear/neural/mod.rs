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
pub mod rerank_head;
pub mod rescore_filter;
pub mod safetensors;

pub use decoder::{
    predict_extensions, rerank_order, rerank_scored, shared_sentence_model, LazySentenceModel,
    PredictOptions, RerankOptions,
};
pub use model::SentenceModel;
pub use predict_filter::NeuralPredictFilter;
pub use rescore_filter::NeuralRescoreFilter;
pub use rerank_head::{RerankRef, RerankSample, RerankTrace};

/// `WEASEL_PREDICT=off` 关掉**全部**神经预测：候选窗补位 + 上屏后联想条。
///
/// 2026-11-09 修：这个开关原先只长在 [`predict_filter`] 里，而 `apply_commit`
/// 那条联想链直接调 `rescore::predict_sync`，完全不受它管 —— 关了开关照样
/// 每提交付一次神经推理、联想条照弹。现在两条链共用这一个总开关。
/// 常驻进程只读一次环境变量，中途改 env 不生效（与旧行为一致）。
pub fn prediction_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        prediction_switch_from(std::env::var("WEASEL_PREDICT").ok().as_deref())
    })
}

/// 开关判定抽成纯函数，单测不必碰进程级环境变量。只认精确的 `off`。
pub fn prediction_switch_from(raw: Option<&str>) -> bool {
    raw != Some("off")
}

/// 预测的**唯一**闸门：环境开关 `WEASEL_PREDICT` + 隐私字段
/// `translator/enable_neural_inference`，两者都开才放行。
///
/// 候选窗补位、上屏联想（`rescore::predict_sync` 入口）都用它；不要在调用点
/// 各自拼条件 —— 2026-11-09 之前两边各判一套，关掉开关联想照样弹。
pub fn predictions_enabled() -> bool {
    predictions_gate(prediction_enabled(), crate::engine::privacy::inference_enabled())
}

/// 闸门合成抽成纯函数（单测不必碰进程级开关）。
pub fn predictions_gate(predict_on: bool, inference_on: bool) -> bool {
    predict_on && inference_on
}

#[cfg(test)]
mod tests {
    use super::{prediction_switch_from, predictions_gate};

    #[test]
    fn predict_switch_only_off_disables() {
        assert!(prediction_switch_from(None), "缺省开");
        assert!(prediction_switch_from(Some("on")));
        assert!(prediction_switch_from(Some("")));
        assert!(prediction_switch_from(Some("OFF")), "大小写不敏感是旧行为，别悄悄改");
        assert!(!prediction_switch_from(Some("off")));
    }

    #[test]
    fn predictions_gate_needs_both_layers() {
        assert!(predictions_gate(true, true));
        assert!(!predictions_gate(false, true), "WEASEL_PREDICT=off 必须一刀切");
        assert!(!predictions_gate(true, false), "隐私字段关掉同样不许预测");
        assert!(!predictions_gate(false, false));
    }
}
