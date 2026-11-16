//! 虎整句「切完方案后头几个字母很卡」探针。
//!
//! 用户 2026-11-08 19:3x 反馈：切到虎码方案后，前几个字母很卡、候选窗定位也不对，
//! 打几次之后才恢复正常。怀疑是 `tiger_sentence.lua` 的懒加载。
//!
//! 本探针把嫌疑拆开量：
//! 1. `apply_schema("tiger_sentence")` —— 切方案本身（含 3 个 lua 组件各自的 VM 启动）；
//! 2. 切完后**逐键**打字，量每个键的引擎耗时（首键若含 `ensure_lexicon` 会特别大）；
//! 3. 切回 ziranma 再切回虎码，重复 2 —— 验证 `apply_schema` 是否每次都重建 Lua VM、
//!    前几个键的卡是否每次切换都复现（= 每次都要重新懒加载码表）。
//!
//! 用法：
//! ```text
//! cargo run --release --offline --example probe_tiger_switch_keys -- \
//!     <用户目录副本> <共享数据目录> [逐键字母...]
//! ```

use std::path::PathBuf;
use std::time::Instant;

use librime_next::engine::key_event::KeyEvent;
use librime_next::engine::runtime::Engine;

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn type_keys(engine: &mut Engine, keys: &[char], label: &str) {
    for (index, ch) in keys.iter().enumerate() {
        let start = Instant::now();
        let eaten = engine.process_key(&KeyEvent::new(*ch as u32, 0));
        let elapsed = ms(start);
        let menu = engine
            .context
            .composition
            .segmentation
            .segments
            .last()
            .and_then(|s| s.menu.as_ref())
            .map(|menu| {
                menu.prepare(8);
                menu.candidate_count()
            })
            .unwrap_or(0);
        println!(
            "{label} 键 {index} `{ch}` eaten={eaten} 耗时={elapsed:8.2} ms  input={:?} 候选数={menu}",
            engine.context.input
        );
    }
}

fn main() {
    let user = PathBuf::from(std::env::args().nth(1).expect("user_root"));
    let shared = PathBuf::from(std::env::args().nth(2).expect("shared_root"));
    let keys: Vec<char> = match std::env::args().nth(3) {
        Some(text) => text.chars().collect(),
        // 虎码 zw 段常用例：day log 里的 zhhbi（其父/虎娘）
        None => "zhhbi".chars().collect(),
    };

    let roots = vec![user.clone(), shared.clone()];
    let build = user.join("build");
    println!("user = {}", user.display());
    println!("shared = {}", shared.display());

    // ② 从 ziranma 热切到虎码：切方案本身 + 切完后的逐键（= 真实用户路径：
    //    引擎启动时是拼音，用户按 Ctrl+`/数字切到虎码，然后开始打字）。
    let Some(mut engine) = Engine::create_with_roots(&build, "double_pinyin.ziranma", &roots) else {
        eprintln!("装配失败: double_pinyin.ziranma");
        return;
    };
    for round in 1..=2 {
        let start = Instant::now();
        let ok = engine.apply_schema("tiger_sentence");
        println!(
            "\n[B{round}] apply_schema(ziranma -> tiger_sentence) = {ok} 耗时={:8.2} ms",
            ms(start)
        );
        type_keys(&mut engine, &keys, &format!("[B{round} 切后-tiger]"));

        let start = Instant::now();
        let ok = engine.apply_schema("double_pinyin.ziranma");
        println!(
            "[B{round}] apply_schema(tiger -> ziranma) = {ok} 耗时={:8.2} ms",
            ms(start)
        );
    }
    drop(engine);

    // ① 全新引擎直接装配虎码（若 VM 已共享则这里只剩装配成本，作对照）
    let start = Instant::now();
    let Some(mut tiger_engine) = Engine::create_with_roots(&build, "tiger_sentence", &roots) else {
        eprintln!("装配失败: tiger_sentence");
        return;
    };
    println!("\n[A] Engine::create(tiger_sentence) = {:8.2} ms", ms(start));
    type_keys(&mut tiger_engine, &keys, "[A 直接装配-tiger]");
}
