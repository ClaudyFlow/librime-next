//! 神经重排打字速度基准——6 层(keyboard)与 8 层(desktop)模型各跑一份。
//!
//! 关注的是**接入打字链路的三类耗时**:
//! 1. 模型加载(引擎启动一次性);
//! 2. `score_sentences`:一批候选(真实重排 batch = 4~12 条)打分耗时
//!    ——这就是后台线程的"一轮工作量",**不在按键路径上**;
//! 3. 上下文缓存命中后的第二批(用户连续打字时的常态)。
//!
//! ```text
//! cargo run --release --example neural_bench
//! ```

use librime_next::engine::gear::neural;
use std::time::Instant;

fn bench_model(name: &str, path: &str) {
    let path = std::path::PathBuf::from(path);
    if !path.is_file() {
        println!("[{name}] 模型不存在,跳过: {}", path.display());
        return;
    }
    let size_mb = std::fs::metadata(&path).map(|m| m.len() as f64 / 1048576.0).unwrap_or(0.0);

    let t = Instant::now();
    let Some(model) = neural::shared_sentence_model(&path) else {
        println!("[{name}] 加载失败");
        return;
    };
    let load_ms = t.elapsed().as_secs_f64() * 1000.0;
    let c = model.config();
    println!(
        "[{name}] {:.1}MB  {}层/{}头/{}维  词表{}  窗口{}  加载 {:.1}ms",
        size_mb, c.n_layer, c.n_head, c.n_embd, c.vocab, c.context, load_ms
    );

    // ---- 单步 next_log_probabilities(参考 API,不进打字路径) ----------
    let t = Instant::now();
    let _ = model.next_log_probabilities("今天天气很好,我们一起");
    let single_ms = t.elapsed().as_secs_f64() * 1000.0;
    println!("  单步 next_log_probabilities: {:.2}ms", single_ms);

    // ---- score_sentences:重排 batch 大小 × 句长 ----------------------
    // 真实场景:词格 n-best 最多 12 条,句子 4~12 字,上下文 64 字。
    let context = "我们昨天在公园里散步的时候看到很多孩子在放风筝,今天天气这么好,";
    let sentence_pool = [
        "一起出去散步",
        "一起出去散布",
        "一起去公园玩",
        "今天天气很好",
        "我想吃点东西",
        "明天早上出发",
        "这是我的书",
        "再见了朋友",
        "明天再看吧",
        "我们一起走",
        "今晚吃什么",
        "周末去爬山",
    ];

    for batch in [4usize, 8, 12] {
        let texts: Vec<String> = sentence_pool[..batch].iter().map(|s| s.to_string()).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        // 冷上下文(首次,建 prefix 缓存)
        let t = Instant::now();
        let _ = model.score_sentences(context, &refs);
        let cold = t.elapsed().as_secs_f64() * 1000.0;
        // 暖上下文(同上下文连续打字的常态,缓存命中)
        let mut warm_total = 0.0f64;
        let rounds = 5;
        for _ in 0..rounds {
            let t = Instant::now();
            let _ = model.score_sentences(context, &refs);
            warm_total += t.elapsed().as_secs_f64() * 1000.0;
        }
        let warm = warm_total / rounds as f64;
        println!(
            "  score_sentences(batch={batch:2}, 7字句): 冷 {cold:7.2}ms  暖 {warm:7.2}ms/轮"
        );
    }
    println!();
}

fn main() {
    println!("=== 神经重排打字速度基准(release,本轮机器)===");
    println!("设计约束:score_sentences 在后台线程跑,按键路径只有 order_for 的哈希查找(<1µs)。");
    println!();
    bench_model("keyboard(6层)", "test-data/neural/sentence-model.safetensors");
    bench_model("desktop(8层)", "test-data/neural/sentence-model-desktop.safetensors");

    // ---- 引擎按键基线(无神经重排时的每键耗时) ------------------------
    println!("=== 参照:引擎每键耗时(bench_keystroke 结论) ===");
    println!("  无神经重排: ~21µs/键(引擎本身不是瓶颈,卡顿在宿主 IPC 链路)");
    println!("  神经重排对按键路径的增量: order_for 一次哈希查找,≪1µs");
}
