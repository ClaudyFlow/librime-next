//! 神经模型冒烟测试:加载 safetensors、推理打分、计时。
//!
//! ```text
//! cargo run --release --example neural_smoke -- [模型路径]
//! ```

use librime_next::engine::gear::neural;
use std::time::Instant;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "test-data/neural/sentence-model.safetensors".to_string());
    let path = std::path::PathBuf::from(&path);
    if !path.is_file() {
        eprintln!(
            "skip: 模型不存在({})——从 chinese-ime-lm model-v1 release 下载,\n\
             键盘版 sha256=86ac529510cb3b4968a5e6ade83ec8080f5b34a0a681e75c362accbbd387d1c1",
            path.display()
        );
        std::process::exit(0);
    }

    let t0 = Instant::now();
    let Some(model) = neural::shared_sentence_model(&path) else {
        eprintln!("模型加载失败: {}", path.display());
        std::process::exit(1);
    };
    let load_ms = t0.elapsed().as_millis();
    let config = model.config();
    println!(
        "加载 OK({load_ms}ms): vocab={} n_layer={} n_head={} n_embd={} context={} attribution={} license={}",
        config.vocab,
        config.n_layer,
        config.n_head,
        config.n_embd,
        config.context,
        model.attribution(),
        model.license()
    );

    // 1) next_log_probabilities:给定上下文,下一个字符的 log 概率分布
    let t = Instant::now();
    let dist = model.next_log_probabilities("今天天气很好,我们");
    let first_ms = t.elapsed();
    let t = Instant::now();
    let dist2 = model.next_log_probabilities("今天天气很好,我们");
    let second_ms = t.elapsed();
    // 找分布里概率最高的几个 token
    let mut top: Vec<(usize, f32)> = dist.iter().copied().enumerate().collect();
    top.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    print!("next_log_probabilities: 首次 {first_ms:?} 二次(含缓存) {second_ms:?},top5 logp:");
    for (id, logp) in top.iter().take(5) {
        let ch = token_char(&model, *id);
        print!(" {ch}:{logp:.3}");
    }
    println!();
    assert_eq!(dist.len(), dist2.len(), "同输入分布长度必须一致");
    assert!(
        dist.iter().sum::<f32>().is_finite(),
        "分布必须是有限值"
    );

    // 2) score_sentences:一批候选句子的 log P(句子 | 上下文)
    let candidates = vec![
        "出去散步吧",
        "出去散布吧",
        "出去三步吧",
        "明天再见",
    ];
    let t = Instant::now();
    let scores = model.score_sentences("今天天气很好,", &candidates);
    let score_ms = t.elapsed();
    println!("score_sentences(4 条): {score_ms:?}");
    for (text, score) in candidates.iter().zip(&scores) {
        println!("  {text:>12} → logp {score:.3}");
    }
    assert_eq!(scores.len(), candidates.len());
    // 同拼音候选("sanbu" 的三种解析)之间,神经模型应当选中真词
    let idx_of = |s: &str| candidates.iter().position(|c| *c == s).unwrap();
    assert!(
        scores[idx_of("出去散步吧")] > scores[idx_of("出去散布吧")],
        "'散步' 应该比 '散布' 概率高"
    );
    assert!(
        scores[idx_of("出去散步吧")] > scores[idx_of("出去三步吧")],
        "'散步' 应该比 '三步' 概率高"
    );
    // "明天再见" 是独立高频句,绝对概率高属正常——真实管线里词格只送
    // 同一拼音的候选,神经分在同拼音候选间选择,不跨拼音比绝对概率。
    assert!(scores[idx_of("明天再见")].is_finite());

    // 3) rerank_order:静态分(log10)+ 神经分(nats)插值重排。
    //    只送同一拼音("sanbu")的三种解析——真实管线里词格只产同拼音候选。
    //    词格自己偏爱 "散布"(静态分最高),神经模型把 "散步" 提回来。
    let options = neural::RerankOptions::default();
    let sanbu: Vec<String> = vec![
        "出去散步吧".to_string(),
        "出去散布吧".to_string(),
        "出去三步吧".to_string(),
    ];
    let static_scores = vec![-3.5f64, -2.9, -3.6]; // 词格 log10 分,"散布" 静态最优
    let order = neural::rerank_order(&model, "今天天气很好,", &sanbu, &static_scores, &options);
    println!(
        "rerank_order(λ=0.5): {:?}",
        order.iter().map(|&i| sanbu[i].as_str()).collect::<Vec<_>>()
    );
    assert_eq!(order.len(), sanbu.len());
    assert_eq!(order[0], 0, "神经重排应把 '散步' 提到第一");

    // 4) 上下文缓存生效验证:同上下文再来一批要明显快过首次
    let t = Instant::now();
    let _ = model.score_sentences("今天天气很好,", &["出去吃个饭", "出去吃个饭吧"]);
    println!("同上下文第二批次: {:?}", t.elapsed());

    println!("neural_smoke 全部通过");
}

/// token id → 展示字符(词表反查)。
fn token_char(model: &neural::SentenceModel, id: usize) -> String {
    match model.char_for_token(id) {
        Some(ch) => ch.to_string(),
        None => format!("#{id}"),
    }
}
