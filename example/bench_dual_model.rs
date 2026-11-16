//! 双模型延迟基准（临时诊断，2026-11-08 交接棒用）。
//!
//! 目的：决定「上屏后联想条 4 条 = key 2 + desk 2」到底能不能在提交路径上同步算出，
//! 还是必须走异步预计算 + 缓存。分别量：
//! 1. predict_extensions（max_new=2, beam=3，即候选窗预测的现参数）；
//! 2. predict_extensions（max_new=1, beam=1，单步）；
//! 3. score_sentences：用词池给「下一个词」打分（联想条候选来源方案 B）；
//! 4. rerank_order（12 条整句，现重排参数）。
//!
//! 用法：cargo run --release --example bench_dual_model -- <key模型> <desk模型>

use std::path::PathBuf;
use std::time::Instant;

use librime_next::gear::neural::{
    predict_extensions, rerank_order, PredictOptions, RerankOptions, SentenceModel,
};

fn report(label: &str, times: &[f64]) {
    let mut sorted = times.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "{label}: first={:.1} median={:.1} max={:.1} ms",
        times[0],
        sorted[sorted.len() / 2],
        sorted[sorted.len() - 1]
    );
}

fn timed(times: &mut Vec<f64>, f: impl Fn()) {
    let t0 = Instant::now();
    f();
    times.push(t0.elapsed().as_secs_f64() * 1000.0);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let key_path = PathBuf::from(args.get(1).cloned().unwrap_or_else(|| {
        r"C:\Code\Weasel-Next\Rime-Next\model\sentence-model.safetensors".into()
    }));
    let desk_path = PathBuf::from(args.get(2).cloned().unwrap_or_else(|| {
        r"C:\Code\Weasel-Next\Rime-Next\model\sentence-model-desktop.safetensors".into()
    }));
    let key = SentenceModel::load_file(&key_path).expect("key 模型加载失败");
    let desk = SentenceModel::load_file(&desk_path).expect("desk 模型加载失败");
    for (name, m) in [("key", &key), ("desk", &desk)] {
        let c = m.config();
        println!(
            "{name}: vocab={} layer={} head={} embd={} context={}",
            c.vocab, c.n_layer, c.n_head, c.n_embd, c.context
        );
    }

    // 真实一点的上下文：最后上屏了「你好」，再往前是「今天」。
    let context = "今天你好";
    let prefix = "你好";
    let words = ["吗", "啊", "的", "呀", "了", "再见", "世界", "朋友"];

    for (name, model) in [("key", &key), ("desk", &desk)] {
        let mut times = Vec::new();
        for _ in 0..5 {
            timed(&mut times, || {
                let _ = predict_extensions(model, context, prefix, &PredictOptions::default());
            });
        }
        report(&format!("[{name}] predict max_new=2 beam=3"), &times);

        let fast = PredictOptions {
            max_new: 1,
            beam: 1,
            ..PredictOptions::default()
        };
        let mut times = Vec::new();
        for _ in 0..5 {
            timed(&mut times, || {
                let _ = predict_extensions(model, context, prefix, &fast);
            });
        }
        report(&format!("[{name}] predict max_new=1 beam=1"), &times);

        let refs: Vec<&str> = words.iter().copied().collect();
        let mut times = Vec::new();
        for _ in 0..5 {
            timed(&mut times, || {
                let _ = model.score_sentences(context, &refs);
            });
        }
        report(&format!("[{name}] score 8 words"), &times);

        let sentences: Vec<String> = (0..12).map(|i| format!("候选{i}")).collect();
        let static_scores: Vec<f64> = (0..12).map(|i| (12 - i) as f64).collect();
        let mut times = Vec::new();
        for _ in 0..3 {
            timed(&mut times, || {
                let _ = rerank_order(
                    model,
                    context,
                    &sentences,
                    &static_scores,
                    &RerankOptions::default(),
                );
            });
        }
        report(&format!("[{name}] rerank 12"), &times);
        println!();
    }

    let mut both = Vec::new();
    for _ in 0..3 {
        timed(&mut both, || {
            let a = predict_extensions(&key, context, prefix, &PredictOptions::default());
            let b = predict_extensions(&desk, context, prefix, &PredictOptions::default());
            let _ = (a, b);
        });
    }
    report("两模型串行 predict", &both);

    let mut warm = Vec::new();
    for _ in 0..3 {
        timed(&mut warm, || {
            let _ = predict_extensions(&desk, "今天你好世界再见", "世界", &PredictOptions::default());
        });
    }
    report("desk predict 长一点上下文", &warm);
}
