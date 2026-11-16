//! 临时探针：拆解「上屏提交路径」的耗时，并做新旧算法的 A/B 与逐位一致性校验。
//!
//! ```text
//! cargo run -p librime-next --release --example probe_commit_stages
//! ```
//!
//! * `old.*`：旧实现（每个假设各自 `top_next_chars(context+text)`，上下文重算）
//! * `new.*`：新实现（同 step 假设共享 base K/V，批量解码）
//! * `same`：两边输出逐项比较（字符、顺序、分数）
//! * `score`：提交时 key 模型对新词的一次打分
//! * `sync`：真实 `predict_sync` 入口（现在走新实现）

use std::path::PathBuf;
use std::time::Instant;

use librime_next::gear::neural::{
    predict_extensions, shared_sentence_model, PredictOptions, SentenceModel,
};

fn pct(sorted: &[f64], q: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * q) as usize]
}

fn report(label: &str, mut values: Vec<f64>) {
    values.sort_by(|a, b| a.total_cmp(b));
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    println!(
        "{label:8} n={:3} mean={mean:7.2}ms p50={:7.2} p95={:7.2} max={:7.2}",
        values.len(),
        pct(&values, 0.50),
        pct(&values, 0.95),
        pct(&values, 1.0),
    );
}

fn ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// 旧实现原样（2026-11-08 优化前）——只用于 A/B 和一致性校验。
fn reference_predict(
    model: &SentenceModel,
    context: &str,
    prefix: &str,
    options: &PredictOptions,
) -> Vec<(String, f64)> {
    let context = last_characters(context, options.context_chars);
    let mut hypotheses: Vec<(String, f64, usize)> = vec![(prefix.to_string(), 0.0, 0)];
    for _ in 0..options.max_new {
        let mut next = Vec::new();
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
        next.sort_by(|a, b| {
            (b.1 / b.2 as f64)
                .partial_cmp(&(a.1 / a.2 as f64))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        next.truncate(options.beam);
        hypotheses = next;
    }
    // 与 `predict_extensions` 相同的收尾：去掉 prefix、去重、按平均分排序裁剪。
    let prefix_chars = prefix.chars().count();
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

// 与 decoder.rs 内 `last_characters` / `predictable` 相同的公开/私有拷贝。
fn last_characters(text: &str, count: usize) -> &str {
    if count == 0 {
        return "";
    }
    let mut seen = 0usize;
    let mut start = text.len();
    for (i, _ch) in text.char_indices().rev() {
        start = i;
        seen += 1;
        if seen == count {
            break;
        }
    }
    &text[start..]
}

fn predictable(character: char) -> bool {
    !character.is_whitespace()
        && !character.is_control()
        && !(character.is_ascii_punctuation())
}

fn main() {
    let data_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-data");
    librime_next::gear::filter::rescore::init_from_data_roots(&[data_root.clone()]);
    let root = data_root.join("model");
    let key = shared_sentence_model(&root.join("sentence-model.safetensors")).expect("key 模型加载失败");
    let desk =
        shared_sentence_model(&root.join("sentence-model-desktop.safetensors")).expect("desk 模型加载失败");

    let cases: &[(&str, &str)] = &[
        ("", "你好"),
        ("", "中国"),
        ("我今天", "可以"),
        ("我们", "谢谢"),
        ("他刚才说", "没问题"),
        ("这个", "东西"),
        ("明天的", "天气"),
        ("", "好的"),
        ("请", "注意"),
        ("刚才", "看了一下"),
    ];
    let options = PredictOptions {
        max_results: 8,
        ..PredictOptions::default()
    };

    // 预热:懒加载 + 首次 forward 分配。
    for (context, word) in cases {
        let full = format!("{context}{word}");
        let _ = key.score_sentences(context, &[word]);
        let _ = predict_extensions(&key, &full, "", &options);
        let _ = predict_extensions(&desk, &full, "", &options);
        let _ = reference_predict(&key, &full, "", &options);
        let _ = reference_predict(&desk, &full, "", &options);
    }

    let mut score = Vec::new();
    let mut old_key = Vec::new();
    let mut new_key = Vec::new();
    let mut old_desk = Vec::new();
    let mut new_desk = Vec::new();
    let mut old_seq = Vec::new();
    let mut new_seq = Vec::new();
    let mut sync = Vec::new();
    // 新实现内部分解：b1 = base forward + 1 行投影；b3 = base forward + 3 行投影；
    // single = next_log_probabilities 单行路径（前缀少一个 token，量级可比）。
    let mut b1 = Vec::new();
    let mut b3 = Vec::new();
    // 一次整段前向（新）vs `build_prefix(head)` + `forward([last], past)`（旧）。
    // 同进程交替计时，避免跨进程两次运行的机器噪声。
    let mut single_new = Vec::new();
    let mut single_old = Vec::new();
    // 同一 case 的旧/新配对样本，落到 `%TEMP%\s1-ab.csv`，供显著性检验（Welch/配对差）。
    let mut s1_pairs: Vec<(usize, usize, f64, f64)> = Vec::new();
    // 并行腿：key + desk 同时算（两者独立，合并顺序不变）。
    let mut par = Vec::new();
    let mut mismatches = 0usize;
    let mut compared = 0usize;

    for round in 0..8 {
        for (case_index, (context, word)) in cases.iter().enumerate() {
            let full = format!("{context}{word}");

            let t = Instant::now();
            let _ = key.score_sentences(context, &[word]);
            score.push(ms(t));

            let t = Instant::now();
            let old_k = reference_predict(&key, &full, "", &options);
            old_key.push(ms(t));

            let t = Instant::now();
            let new_k = predict_extensions(&key, &full, "", &options);
            new_key.push(ms(t));

            let t = Instant::now();
            let old_d = reference_predict(&desk, &full, "", &options);
            old_desk.push(ms(t));

            let t = Instant::now();
            let new_d = predict_extensions(&desk, &full, "", &options);
            new_desk.push(ms(t));

            let t = Instant::now();
            let _ = reference_predict(&key, &full, "", &options);
            let _ = reference_predict(&desk, &full, "", &options);
            old_seq.push(ms(t));

            let t = Instant::now();
            let _ = predict_extensions(&key, &full, "", &options);
            let _ = predict_extensions(&desk, &full, "", &options);
            new_seq.push(ms(t));

            let t = Instant::now();
            let _ = librime_next::gear::filter::rescore::predict_sync(&full, word, 4);
            sync.push(ms(t));

            // 内部分解（desk，最贵那条腿）：同一 base、尾巴 1 字，比较 1 行 vs 3 行。
            let t = Instant::now();
            let _ = desk.top_next_chars_batch(&full, &["是"], options.beam);
            b1.push(ms(t));
            let t = Instant::now();
            let _ = desk.top_next_chars_batch(&full, &["是", "也", "还"], options.beam);
            b3.push(ms(t));
            let t = Instant::now();
            let _ = desk.next_log_probabilities(&full);
            let new_ms = ms(t);
            single_new.push(new_ms);
            let t = Instant::now();
            let _ = desk.next_log_probabilities_two_pass(&full);
            let old_ms = ms(t);
            single_old.push(old_ms);
            s1_pairs.push((round, case_index, old_ms, new_ms));

            let t = Instant::now();
            std::thread::scope(|s| {
                let k = s.spawn(|| predict_extensions(&key, &full, "", &options));
                let d = predict_extensions(&desk, &full, "", &options);
                let _ = k.join();
                let _ = d;
            });
            par.push(ms(t));

            // 一致性：两边输出必须逐项相同（字符串、顺序、分数）。
            for (old, new) in [(&old_k, &new_k), (&old_d, &new_d)] {
                compared += 1;
                let same_text: Vec<&str> = old.iter().map(|(t, _)| t.as_str()).collect();
                let new_text: Vec<&str> = new.iter().map(|(t, _)| t.as_str()).collect();
                let text_ok = same_text == new_text;
                let score_ok = old
                    .iter()
                    .zip(new.iter())
                    .all(|(a, b)| (a.1 - b.1).abs() < 1e-4);
                if !text_ok || !score_ok {
                    mismatches += 1;
                    println!("MISMATCH full={full:?} old={old:?} new={new:?}");
                }
            }

            // 非空 prefix（打字时的锚点续写）也校验一遍。
            let typed_context = "我们";
            let typed_prefix = "今天";
            let old_t = reference_predict(&desk, typed_context, typed_prefix, &options);
            let new_t = predict_extensions(&desk, typed_context, typed_prefix, &options);
            compared += 1;
            let ot: Vec<&str> = old_t.iter().map(|(t, _)| t.as_str()).collect();
            let nt: Vec<&str> = new_t.iter().map(|(t, _)| t.as_str()).collect();
            if ot != nt || old_t.iter().zip(new_t.iter()).any(|(a, b)| (a.1 - b.1).abs() > 1e-4) {
                mismatches += 1;
                println!("MISMATCH typed old={old_t:?} new={new_t:?}");
            }
        }
    }

    // 空上下文：`build_prefix([])` 返回空缓存，验证复用路径不越界且结果一致。
    for (model, name) in [(&key, "key"), (&desk, "desk")] {
        let old = reference_predict(model, "", "", &options);
        let new = predict_extensions(model, "", "", &options);
        compared += 1;
        let ot: Vec<&str> = old.iter().map(|(t, _)| t.as_str()).collect();
        let nt: Vec<&str> = new.iter().map(|(t, _)| t.as_str()).collect();
        if ot != nt || old.iter().zip(new.iter()).any(|(a, b)| (a.1 - b.1).abs() > 1e-4) {
            mismatches += 1;
            println!("MISMATCH empty/{name} old={old:?} new={new:?}");
        }
    }

    // 长上下文（80 字 > 64 字窗口）：逼出左侧截断 + 前缀复用退化重建，单独校验。
    let long_ctx = "春眠不觉晓处处闻啼鸟夜来风雨声花落知多少春眠不觉晓处处闻啼鸟夜来风雨声花落知多少春眠不觉晓处处闻啼鸟夜来风雨声花落知多少春眠不觉晓处处闻啼鸟夜来风雨声花落知多少";
    let long_full = format!("{long_ctx}故乡");
    for (model, name) in [(&key, "key"), (&desk, "desk")] {
        let old = reference_predict(model, &long_full, "", &options);
        let new = predict_extensions(model, &long_full, "", &options);
        compared += 1;
        let ot: Vec<&str> = old.iter().map(|(t, _)| t.as_str()).collect();
        let nt: Vec<&str> = new.iter().map(|(t, _)| t.as_str()).collect();
        if ot != nt || old.iter().zip(new.iter()).any(|(a, b)| (a.1 - b.1).abs() > 1e-4) {
            mismatches += 1;
            println!("MISMATCH long/{name} old={old:?} new={new:?}");
        }
        println!(
            "long/{name}: old={:.2}ms new={:.2}ms",
            {
                let t = Instant::now();
                let _ = reference_predict(model, &long_full, "", &options);
                ms(t)
            },
            {
                let t = Instant::now();
                let _ = predict_extensions(model, &long_full, "", &options);
                ms(t)
            }
        );
    }

    report("score", score);
    report("old.key", old_key);
    report("new.key", new_key);
    report("old.desk", old_desk);
    report("new.desk", new_desk);
    report("old.seq", old_seq);
    report("new.seq", new_seq);
    report("sync", sync);
    report("par(k+d)", par);
    report("d.b1", b1.clone());
    report("d.b3", b3.clone());
    report("d.s1.new", single_new.clone());
    report("d.s1.old", single_old.clone());
    let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
    let row = (mean(&b3) - mean(&b1)) / 2.0;
    println!(
        "分解: base forward ≈ {:.2}ms, 每行投影 ≈ {:.2}ms (b3-b1)/2",
        mean(&b1) - row,
        row
    );
    println!(
        "单遍前向: 旧 {:.2}ms → 新 {:.2}ms，省 {:.2}ms（p50 {:.2} → {:.2}，p95 {:.2} → {:.2}）",
        mean(&single_old),
        mean(&single_new),
        mean(&single_old) - mean(&single_new),
        {
            let mut v = single_old.clone();
            v.sort_by(|a, b| a.total_cmp(b));
            pct(&v, 0.50)
        },
        {
            let mut v = single_new.clone();
            v.sort_by(|a, b| a.total_cmp(b));
            pct(&v, 0.50)
        },
        {
            let mut v = single_old.clone();
            v.sort_by(|a, b| a.total_cmp(b));
            pct(&v, 0.95)
        },
        {
            let mut v = single_new.clone();
            v.sort_by(|a, b| a.total_cmp(b));
            pct(&v, 0.95)
        },
    );
    println!("consistency: {compared} 组比较，{mismatches} 组不一致");
    println!("（max_new=2 × beam=3：旧实现两模型各 4 次全上下文 forward；新实现 step2 只 1 次共享 prefix）");

    // 原始配对样本（毫秒，3 位小数），供统计脚本做 Welch / Mann-Whitney / 配对差 CI。
    let ab = std::env::temp_dir().join("s1-ab.csv");
    let mut text = String::from("round,case,old_ms,new_ms\n");
    for (round, case, old, new) in &s1_pairs {
        text.push_str(&format!("{round},{case},{old:.4},{new:.4}\n"));
    }
    std::fs::write(&ab, text).expect("写 s1-ab.csv");
    println!("原始配对样本 {} 行：{}", s1_pairs.len(), ab.display());
}
