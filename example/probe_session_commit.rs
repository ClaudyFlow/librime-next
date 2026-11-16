//! 离线会话压测：在 temp 的 build/userdb 副本上连续「打词 + 空格上屏」，
//! 逐次记录提交键耗时，找周期性尖峰（userdb 写入 / n-gram 落盘 / 词频压缩）。
//!
//! 只在 `%TEMP%\rime_perf_copy` 上写，不碰 `%APPDATA%` 与 `C:\Program Files`。
use librime_next::engine::Session;
use std::path::PathBuf;
use std::time::Instant;

fn pct(v: &mut [u128], q: f64) -> u128 {
    v.sort_unstable();
    v[((v.len() - 1) as f64 * q) as usize]
}

fn main() {
    let copy = std::env::temp_dir().join("rime_perf_copy");
    let build = copy.join("build");
    let data = PathBuf::from(r"C:\Program Files\Rime-Next\Weasel-Next\data");
    // 和 live 一样把神经模型挂上（否则 predict_sync 直接返回空，测不到提交路径的大头）。
    librime_next::gear::filter::rescore::init_from_data_roots(&[copy.clone(), data.clone()]);
    let mut session = Session::create_with_roots(&build, "luna_pinyin_simp", &[copy.clone(), data]);
    println!("引擎加载: {}", session.engine.is_some());

    // 预热：首键 + 首次提交（含神经模型加载），单独打印不算入统计。
    let t = Instant::now();
    for ch in "nihao".chars() {
        session.process_key(ch as u32, 0);
    }
    let warm_keys = t.elapsed();
    let t = Instant::now();
    session.process_key(0x20, 0);
    println!(
        "warmup: keys={:.1}ms first_commit={:.1}ms",
        warm_keys.as_secs_f64() * 1000.0,
        t.elapsed().as_secs_f64() * 1000.0
    );

    const N: usize = 300;
    let word = "women";
    let mut commits: Vec<(usize, u128)> = Vec::with_capacity(N);
    let mut keys: Vec<(usize, u128)> = Vec::new();
    let mut seq = 0usize;
    for i in 0..N {
        for ch in word.chars() {
            let t = Instant::now();
            session.process_key(ch as u32, 0);
            keys.push((seq, t.elapsed().as_micros()));
            seq += 1;
        }
        let t = Instant::now();
        let handled = session.process_key(0x20, 0);
        if !handled {
            println!("commit {i} 未被消费");
        }
        let us = t.elapsed().as_micros();
        commits.push((i, us));
        if i < 3 {
            let text = session.take_commit();
            println!(
                "  #{i}: composing={} commit={:?} {:.2}ms",
                session.engine.as_ref().map(|e| e.context.is_composing()).unwrap_or(false),
                text,
                us as f64 / 1000.0
            );
        } else {
            let _ = session.take_commit();
        }
    }

    let mut v: Vec<u128> = commits.iter().map(|(_, us)| *us).collect();
    println!(
        "commit n={} p50={:.2} p95={:.2} p99={:.2} max={:.2}ms",
        v.len(),
        pct(&mut v, 0.5) as f64 / 1000.0,
        pct(&mut v, 0.95) as f64 / 1000.0,
        pct(&mut v, 0.99) as f64 / 1000.0,
        v[v.len() - 1] as f64 / 1000.0
    );
    let mut kv: Vec<u128> = keys.iter().map(|(_, us)| *us).collect();
    println!(
        "keys   n={} p50={:.2} p95={:.2} p99={:.2} max={:.2}ms",
        kv.len(),
        pct(&mut kv, 0.5) as f64 / 1000.0,
        pct(&mut kv, 0.95) as f64 / 1000.0,
        pct(&mut kv, 0.99) as f64 / 1000.0,
        kv[kv.len() - 1] as f64 / 1000.0
    );

    // 每 10 次会 `user_ngram::save` 落盘一次：对比这两组的代价。
    for (label, pick) in [
        ("i%10==9(落盘)", true),
        ("其它", false),
    ] {
        let mut v: Vec<u128> = commits
            .iter()
            .filter(|(i, _)| (i % 10 == 9) == pick)
            .map(|(_, us)| *us)
            .collect();
        println!(
            "  {label}: n={} p50={:.2} max={:.2}ms",
            v.len(),
            pct(&mut v, 0.5) as f64 / 1000.0,
            v[v.len() - 1] as f64 / 1000.0
        );
    }

    let spikes: Vec<(usize, u128)> = commits.iter().copied().filter(|(_, us)| *us > 50000).collect();
    println!("commit >50ms: {} 次", spikes.len());
    for (i, us) in &spikes {
        println!("  commit #{i}: {:.1}ms (i%10={})", *us as f64 / 1000.0, i % 10);
    }
    let any_key: Vec<(usize, u128)> = keys.iter().copied().filter(|(_, us)| *us > 50000).collect();
    println!("普通键 >50ms: {} 次", any_key.len());
    for (i, us) in &any_key {
        println!("  key #{i}: {:.1}ms", *us as f64 / 1000.0);
    }
}
