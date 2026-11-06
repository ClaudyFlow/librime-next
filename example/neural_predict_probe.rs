//! 预测探针：用真模型看一眼 top-k 下一字符与续写结果，给阈值定标用。
//!
//! ```text
//! cargo run -p librime-next --example neural_predict_probe
//! ```

use std::time::Instant;

use librime_next::engine::gear::neural::{predict_extensions, PredictOptions, SentenceModel};

fn main() {
    let candidates = [
        "test-data/model/sentence-model.safetensors",
        "test-data/model/sentence-model-desktop.safetensors",
        "../Rime-Next/model/sentence-model.safetensors",
        "../Rime-Next/model/sentence-model-desktop.safetensors",
    ];
    let mut path = None;
    for candidate in candidates {
        if std::path::Path::new(candidate).is_file() {
            path = Some(candidate.to_string());
            break;
        }
    }
    let Some(path) = path else {
        eprintln!("没找到模型文件（从 librime-next/ 下运行）");
        return;
    };
    println!("模型: {path}");
    let model = SentenceModel::load_file(std::path::Path::new(&path)).expect("模型加载失败");
    println!(
        "config: vocab={} layers={} heads={} n_embd={} ctx={}",
        model.config().vocab,
        model.config().n_layer,
        model.config().n_head,
        model.config().n_embd,
        model.config().context
    );

    println!("\n=== 单字条件概率（给定上文，看下一个字）===");
    for context in ["", "我", "今天", "我们一起", "他说这个"] {
        let started = Instant::now();
        let top = model.top_next_chars(context, 8);
        let elapsed = started.elapsed();
        let rendered: Vec<String> = top
            .iter()
            .map(|(character, log_prob)| format!("{character}({log_prob:.2})"))
            .collect();
        println!(
            "  [{context}] -> {}   ({:.1} ms)",
            rendered.join(" "),
            elapsed.as_secs_f64() * 1000.0
        );
    }

    println!("\n=== 续写预测（prefix 之后接什么）===");
    let options = PredictOptions::default();
    println!(
        "  参数: max_new={} beam={} min_avg={} max_results={}",
        options.max_new, options.beam, options.min_avg_score, options.max_results
    );
    let probes = [
        ("", "我"),
        ("", "我们"),
        ("今天", "我"),
        ("我觉得", "这个"),
        ("", "的"),
        ("", "是"),
        ("他", "说"),
    ];
    for (context, prefix) in probes {
        let started = Instant::now();
        let predicted = predict_extensions(&model, context, prefix, &options);
        let elapsed = started.elapsed();
        let rendered: Vec<String> = predicted
            .iter()
            .map(|(text, score)| format!("{text}({score:.2})"))
            .collect();
        println!(
            "  ctx=[{context}] {prefix} -> [{}]   ({:.1} ms)",
            rendered.join(" "),
            elapsed.as_secs_f64() * 1000.0
        );
    }

    println!("\n=== 判别力对照：同一位置几个候选，谁的分高 ===");
    let pairs: &[(&str, &[&str])] = &[
        ("我", &["们", "想", "是", "要", "一"]),
        ("今天", &["天气", "填空", "上午", "下雨"]),
        ("我觉得", &["这个", "这样", "真的", "错的"]),
        ("", &["我们", "他们", "你们", "咱们"]),
        ("他说", &["这个", "那个", "什么", "的话"]),
    ];
    for (context, texts) in pairs {
        let scores = model.score_sentences(context, texts);
        let mut rendered: Vec<(&&str, f64)> = texts.iter().zip(scores.iter()).map(|(t, s)| (t, *s)).collect();
        rendered.sort_by(|a, b| b.1.total_cmp(&a.1));
        let line: Vec<String> = rendered
            .iter()
            .map(|(text, score)| format!("{text}({:.2})", score / text.chars().count() as f64))
            .collect();
        println!("  ctx=[{context}] 按每字平均分: {}", line.join(" > "));
    }

    println!("\n=== 指定字符的真实条件概率（验 top-k 有没有漏掉常识续写）===");
    let checks: &[(&str, &str)] = &[
        ("我", "们"),
        ("我", "一"),
        ("我", "想"),
        ("今天", "天"),
        ("今天", "上"),
    ];
    for (context, target) in checks {
        let score = model.score_sentences(context, &[*target]);
        println!("  logP({target} | {context}) = {:.3}", score.first().copied().unwrap_or(f64::NAN));
    }

    println!("\n=== token 映射自检（top-k 与 target 打分不一致时看这里）===");
    for target in ["们", "一", "想", "天"] {
        let id = target.chars().next().and_then(|c| model.token(c));
        let back = id.and_then(|id| model.char_for_token(id as usize));
        println!("  '{target}' -> token {id:?} -> 反查 {back:?}");
    }
    let log_probs = model.next_log_probabilities("我");
    let id_of = |c: &str| model.token(c.chars().next().unwrap()).map(|id| id as usize);
    println!("  next_log_probabilities(\"我\") 长度 = {}", log_probs.len());
    if let Some(i) = id_of("们") {
        println!("  分布里 logP(们|我) = {:.3}", log_probs[i]);
    }
    if let Some(i) = id_of("一") {
        println!("  分布里 logP(一|我) = {:.3}", log_probs[i]);
    }
    let mut ranked: Vec<(usize, f32)> = log_probs
        .iter()
        .enumerate()
        .map(|(i, &p)| (i, p))
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    print!("  分布 top-8 (id, logp, char): ");
    for (id, p) in ranked.iter().take(8) {
        print!("({id}, {p:.2}, {:?}) ", model.char_for_token(*id));
    }
    println!();

    println!("\n=== 上下文敏感性：同一目标字换上文，分数该变 ===");
    for context in ["", "我", "他", "你"] {
        let scored = model.score_sentences(context, &["们"]);
        let dist = model.next_log_probabilities(context);
        let from_dist = model
            .token('们')
            .map(|id| dist[id as usize])
            .unwrap_or(f32::NAN);
        println!(
            "  ctx=[{context}] score_sentences={:.3}  next_log_probabilities={:.3}",
            scored[0], from_dist
        );
    }
}
