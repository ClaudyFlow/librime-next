//! 提交键上屏联想的耗时拆解（`rescore::predict_sync`）。
//!
//! ```text
//! cargo run --release --example bench_predict_breakdown
//! WEASEL_NEURAL_THREADS=1 cargo run --release --example bench_predict_breakdown   # 串行对照
//! ```
//!
//! 分段口径（desk / key 各自量）：
//! * `step0` —— `next_log_probabilities_with_prefix(ctx)`：整段上文一次前向 +
//!   全词表投影 + 写满前缀 K/V（= `predict_extensions` 的第一段）。
//! * `step1` —— 拿 step0 的 K/V 批量解码 3 条 1 字尾巴：小前向 + **3 份**全词表投影。
//! * `extensions` —— 完整 `predict_extensions`（两段 + 排序/字符串/去重）。
//! * `predict_sync` —— 真入口（key/desk 两条腿并行 + 双模型合并 + 调度）。
//!
//! 2026-11-10 实测（本机 16 逻辑核，宿主另有负载，用开头的点积校准换算）：
//! 拆分结果 ≈ 上文前向 65~75%、全词表投影 20~30%、合并/调度/分配 <5%（前向随上文
//! 长度线性涨、投影不涨，见下面 ctx 扫描）。按输出列切 + 6 线程固定池后，
//! desk 上文 21 字 `predict_extensions` 43.8 → 20.4ms、64 字 120.3 → 62.6ms，
//! `predict_sync` 45.1 → 24.3ms（同一次运行的校准 30.5 vs 31.0 GFLOPS）。
//! 缩短上文到 24 字虽然更快，但 top4 联想会整体变样（见文末探针），所以没走那条路。
//!
//! 末尾的「上文缩短探针」不是性能数字，是**质量代价的证据**：改窗口前先看它。

use std::path::PathBuf;
use std::time::Instant;

use librime_next::gear::neural::{predict_extensions, PredictOptions, SentenceModel};
use librime_next::gear::filter::rescore;

#[derive(Default)]
struct Samples {
    /// 微秒，保留原始精度。
    us: Vec<f64>,
}

impl Samples {
    fn report(&self, label: &str) {
        let mut sorted = self.us.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let n = sorted.len().max(1) as f64;
        let mean = sorted.iter().sum::<f64>() / n;
        let var = sorted.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / n;
        let pct = |q: f64| sorted[((sorted.len().saturating_sub(1)) as f64 * q) as usize];
        println!(
            "{label:<32} n={:<3} min={:>9.1}us p50={:>9.1}us p95={:>9.1}us mean={:>9.1}us sd={:>7.1}us max={:>9.1}us",
            self.us.len(),
            pct(0.0),
            pct(0.50),
            pct(0.95),
            mean,
            var.sqrt(),
            pct(1.0),
        );
    }
}

fn measure(rounds: usize, mut f: impl FnMut()) -> Samples {
    let mut out = Samples::default();
    for _ in 0..rounds {
        let started = Instant::now();
        f();
        out.us.push(started.elapsed().as_nanos() as f64 / 1000.0);
    }
    out
}

fn main() {
    let key_path = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| {
        r"C:\Code\Weasel-Next\Rime-Next\model\sentence-model.safetensors".into()
    }));
    let desk_path = PathBuf::from(std::env::args().nth(2).unwrap_or_else(|| {
        r"C:\Code\Weasel-Next\Rime-Next\model\sentence-model-desktop.safetensors".into()
    }));

    let key = SentenceModel::load_file(&key_path).expect("key 模型加载失败");
    let desk = SentenceModel::load_file(&desk_path).expect("desk 模型加载失败");
    for (name, model) in [("key", &key), ("desk", &desk)] {
        let c = model.config();
        println!(
            "{name}: vocab={} layers={} heads={} n_embd={} ctx={}",
            c.vocab, c.n_layer, c.n_head, c.n_embd, c.context
        );
    }

    // 同机干扰校准：固定 MAC 量的点积，换算实测 GFLOPS。机器上有别的负载时，
    // 用它能判断两次测量是不是同一"当量"，别把污染后的绝对值当结论。
    {
        let a: Vec<f32> = (0..448).map(|i| (i % 17) as f32 * 0.01).collect();
        let b: Vec<f32> = (0..448).map(|i| (i % 23) as f32 * 0.01).collect();
        const REPS: usize = 60_000;
        let mut sink = 0.0f32;
        let started = Instant::now();
        for _ in 0..REPS {
            sink += librime_next::gear::neural::matrix::dot_f32_f32(&a, &b);
        }
        let secs = started.elapsed().as_secs_f64();
        println!(
            "校准: dot(448) × {REPS} 用时 {:.1}ms → {:.1} GFLOPS（sink={sink:.1}）",
            secs * 1000.0,
            REPS as f64 * 448.0 * 2.0 / secs / 1e9
        );
    }

    let context = "今天天气不错我们一起去公园散步然后回家吃饭";
    let options = PredictOptions::default();
    let rounds = 30;

    for (name, model) in [("desk", &desk), ("key", &key)] {
        for _ in 0..3 {
            let _ = model.next_log_probabilities_with_prefix(context);
            let _ = predict_extensions(model, context, "", &options);
        }

        let step0 = measure(rounds, || {
            let _ = model.next_log_probabilities_with_prefix(context);
        });
        let (_, prefix) = model.next_log_probabilities_with_prefix(context);
        let tails = ["吗", "呢", "吧"];
        let step1 = measure(rounds, || {
            let _ = model.top_next_chars_after_prefix(context, &prefix, &tails, options.beam);
        });
        let full = measure(rounds, || {
            let _ = predict_extensions(model, context, "", &options);
        });

        println!("\n=== {name} 模型（ctx {} 字）===", context.chars().count());
        step0.report("step0 整段前向+投影+写K/V");
        step1.report("step1 3行批量解码（3份投影）");
        full.report("predict_extensions 全量");
        println!(
            "  ≈单份全词表投影 ≈ step1/3 = {:.1}us；step0 里的前向部分 ≈ {:.1}us",
            step1.us.iter().sum::<f64>() / 3.0 / step1.us.len() as f64,
            (step0.us.iter().sum::<f64>() - step1.us.iter().sum::<f64>() / 3.0)
                / step0.us.len() as f64
        );

        // 前向随上文长度线性涨，投影不涨 —— 这是"上文前向最贵"的直接证据。
        for ctx_len in [2usize, 8, 20, 40, 64] {
            let trimmed: String = context.chars().cycle().take(ctx_len).collect();
            let s = measure(10, || {
                let _ = model.next_log_probabilities_with_prefix(&trimmed);
            });
            let mut sorted = s.us.clone();
            sorted.sort_by(|a, b| a.total_cmp(b));
            println!("  ctx_len={ctx_len:<3} step0 p50={:.1}us", sorted[sorted.len() / 2]);
        }

        // 真实提交路径的上文封顶 64 字（`last_characters(.., 64)`），量最坏情形。
        let long_ctx: String = context.chars().cycle().take(64).collect();
        for _ in 0..3 {
            let _ = predict_extensions(model, &long_ctx, "", &options);
        }
        measure(rounds, || {
            let _ = predict_extensions(model, &long_ctx, "", &options);
        })
        .report("predict_extensions（ctx=64 最坏）");
    }

    // 真入口：key/desk 并行 + 合并。模型根只读，不写任何文件。
    rescore::init_from_data_roots(&[PathBuf::from(r"C:\Code\Weasel-Next\Rime-Next")]);
    println!(
        "\nrescore worker = {}",
        if rescore::global().is_some() {
            "Some"
        } else {
            "None（没找到模型）"
        }
    );
    for _ in 0..3 {
        let _ = rescore::predict_sync(context, "吃饭", 4);
    }
    println!("\n=== 真入口（4 条联想，key2+desk2）===");
    measure(rounds, || {
        let _ = rescore::predict_sync(context, "吃饭", 4);
    })
    .report("rescore::predict_sync（ctx 22）");

    // 真入口的最坏情形：上文吃满 64 字。
    let long_ctx: String = context.chars().cycle().take(64).collect();
    for _ in 0..3 {
        let _ = rescore::predict_sync(&long_ctx, "吃饭", 4);
    }
    measure(rounds, || {
        let _ = rescore::predict_sync(&long_ctx, "吃饭", 4);
    })
    .report("rescore::predict_sync（ctx 64）");

    // 质量代价探针：上文砍到最近 24 字，top4 联想是否还一样。
    let head = "他打开手机看了看时间发现已经快迟到了赶紧跑向地铁站这时候站台上的人已经排成了长队他只好站在最后面等下一班车";
    let tails = [
        "心里想着今天肯定要迟到了",
        "可是车迟迟没有来天气又很冷",
        "旁边有人正在打电话声音很大",
        "手机突然震动了一下是同事发的",
    ];
    let mut same = 0usize;
    println!("\n=== 上文缩短探针（64 字 vs 最近 24 字）===");
    for tail in tails {
        let ctx = format!("{head}{tail}");
        let long = predict_words(&key, &desk, &ctx);
        let short_ctx: String = ctx
            .chars()
            .rev()
            .take(24)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        let short = predict_words(&key, &desk, &short_ctx);
        if long == short {
            same += 1;
        }
        println!("  {} 字: 64={long:?} 24={short:?}", ctx.chars().count());
    }
    println!("  top4 完全一致: {same}/{}", tails.len());
}

/// 与 `rescore::predict_sync` 同口径的近似（key 2 条 + desk 2 条，去重取 4）。
fn predict_words(key: &SentenceModel, desk: &SentenceModel, context: &str) -> Vec<String> {
    let mut options = PredictOptions::default();
    options.max_results = 8;
    let mut key_out = predict_extensions(key, context, "", &options);
    let mut desk_out = predict_extensions(desk, context, "", &options);
    key_out.truncate(2);
    desk_out.truncate(2);
    key_out
        .into_iter()
        .chain(desk_out)
        .map(|(text, _)| text)
        .take(4)
        .collect()
}
