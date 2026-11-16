//! 诊断探针：**按数字选候选（提交）这一键的引擎侧耗时**。
//!
//! 背景（2026-11-08 用户报「打字按数字选候选时有概率上数字而不是候选」）：
//! TIP 宿主线程对每一键只等 `COMMAND_TIMEOUT(80ms) + HOST_WAIT_MARGIN(60ms)`
//! = 140ms（见 `Tip/src/rpc_worker.rs`）。超时就会 `push.invalidate()` 并把该键
//! 当「输入法没吃」退回应用 ⇒ 数字被应用收到。
//! 0.18.7 起 `apply_commit` 在提交路径**同步**跑双神经预测（key+desk），
//! 必测这一键到底要多少毫秒。
//!
//! 用法（离线）：
//! ```text
//! cargo run --release --example probe_commit_latency -- <用户目录副本> <共享目录> [方案id] [轮数]
//! ```
//! 用户目录必须是**副本**（`RIME_PROBE_USER` 语义同 Engine 的 live 探针），别指向现网。

use std::path::PathBuf;
use std::time::Instant;

use librime_next::gear::filter::rescore;
use librime_next::engine::key_event::KeyEvent;
use librime_next::engine::runtime::Engine;

fn press(engine: &mut Engine, text: &str) {
    for ch in text.chars() {
        let _ = engine.process_key(&KeyEvent::new(ch as u32, 0));
    }
}

fn candidates(engine: &mut Engine) -> usize {
    engine
        .context
        .composition
        .segmentation
        .segments
        .last()
        .and_then(|segment| segment.menu.as_ref())
        .map(|menu| {
            menu.prepare(8);
            menu.candidate_count()
        })
        .unwrap_or(0)
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let user_dir = PathBuf::from(raw.first().unwrap_or_else(|| {
        panic!("用法: probe_commit_latency <用户目录副本> <共享目录> [方案id] [轮数]")
    }));
    let shared_dir = PathBuf::from(raw.get(1).unwrap_or_else(|| panic!("缺共享目录")));
    let schema = raw
        .get(2)
        .cloned()
        .unwrap_or_else(|| "double_pinyin.ziranma".into());
    let rounds: usize = raw.get(3).and_then(|v| v.parse().ok()).unwrap_or(12);
    let anchor = raw
        .get(4)
        .cloned()
        .unwrap_or_else(|| "nihao".to_string());

    let roots = vec![user_dir.clone(), shared_dir.clone()];
    // 现网 c_api 在初始化时做同样的事（无模型则退回 n-gram，不会崩）。
    rescore::init_from_data_roots(&roots);
    println!(
        "rescore worker = {}",
        if rescore::global().is_some() { "Some（双神经已挂）" } else { "None（无模型，走 n-gram 回退）" }
    );
    let build_dir = user_dir.join("build");
    let mut engine = Engine::create_with_roots(&build_dir, &schema, &roots).expect("方案装配失败");
    println!("schema={schema} build={}", build_dir.display());

    // 预热：第一次提交会触发模型惰性解析 / n-gram 落盘。默认 2 轮；
    // `PROBE_WARMUP=0` 可量「冷启动首次提交」的真实耗时。
    let warmup: usize = std::env::var("PROBE_WARMUP")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2);
    for _ in 0..warmup {
        press(&mut engine, &anchor);
        let _ = engine.process_key(&KeyEvent::new(b'2' as u32, 0));
        let _ = engine.process_key(&KeyEvent::new(0xff1b, 0));
        let _ = engine.process_key(&KeyEvent::new(0xff1b, 0));
    }

    let mut digit_times = Vec::new();
    let mut space_times = Vec::new();
    for round in 0..rounds {
        // ── 数字选候选（提交）──
        press(&mut engine, &anchor);
        let count = candidates(&mut engine);
        let start = Instant::now();
        let eaten = engine.process_key(&KeyEvent::new(b'2' as u32, 0));
        let digit_ms = start.elapsed().as_secs_f64() * 1000.0;
        digit_times.push(digit_ms);
        let commit = engine.context.commit_text.clone().unwrap_or_default();
        println!(
            "ROUND {round:02} digit: 候选={count} eaten={eaten} 提交={commit:?} 耗时={digit_ms:.1} ms"
        );
        let _ = engine.process_key(&KeyEvent::new(0xff1b, 0));
        let _ = engine.process_key(&KeyEvent::new(0xff1b, 0));

        // ── 空格提交（对照：同样走 apply_commit）──
        press(&mut engine, &anchor);
        let start = Instant::now();
        let eaten = engine.process_key(&KeyEvent::new(b' ' as u32, 0));
        let space_ms = start.elapsed().as_secs_f64() * 1000.0;
        space_times.push(space_ms);
        let commit = engine.context.commit_text.clone().unwrap_or_default();
        println!(
            "ROUND {round:02} space: eaten={eaten} 提交={commit:?} 耗时={space_ms:.1} ms"
        );
        let _ = engine.process_key(&KeyEvent::new(0xff1b, 0));
        let _ = engine.process_key(&KeyEvent::new(0xff1b, 0));
    }

    let summary = |name: &str, times: &[f64]| {
        let max = times.iter().cloned().fold(0.0f64, f64::max);
        let avg = times.iter().sum::<f64>() / times.len().max(1) as f64;
        let over80 = times.iter().filter(|value| **value > 80.0).count();
        let over140 = times.iter().filter(|value| **value > 140.0).count();
        println!(
            "SUMMARY {name}: n={} avg={avg:.1}ms max={max:.1}ms 超过80ms={over80} 超过140ms(TIP宿主预算)={over140}",
            times.len()
        );
    };
    if !digit_times.is_empty() {
        summary("digit", &digit_times);
    }
    if !space_times.is_empty() {
        summary("space", &space_times);
    }
}
