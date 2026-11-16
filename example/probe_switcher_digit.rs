//! 诊断探针：**Ctrl+` 选单里按数字选方案这一键的耗时**，以及「方案切换是在哪一键生效」。
//!
//! 用户 2026-11-08 报：选单里按数字选方案 →「卡一下 → 数字上屏 → 才切换方案」。
//! 这是「响应超时被丢 ⇒ 数字透传」的典型形态。本探针在进程内逐键计时：
//!   ① Ctrl+`（打开选单）② 数字选方案 ③ 下一键。
//!
//! 用法：
//! ```text
//! cargo run --release --example probe_switcher_digit -- <用户目录副本> <共享目录> [数字] [起始方案]
//! ```

use std::path::PathBuf;
use std::time::Instant;

use librime_next::engine::key_event::{KeyEvent, K_CONTROL_MASK};
use librime_next::engine::runtime::Engine;

fn timed(engine: &mut Engine, label: &str, code: u32, modifier: u32) -> bool {
    let start = Instant::now();
    let eaten = engine.process_key(&KeyEvent::new(code, modifier));
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "{label:28} 耗时={ms:7.1} ms eaten={eaten} schema={} pending={:?}",
        engine.schema_id(),
        engine.pending_schema()
    );
    eaten
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let user_dir = PathBuf::from(raw.first().unwrap_or_else(|| {
        panic!("用法: probe_switcher_digit <用户目录副本> <共享目录> [数字] [起始方案]")
    }));
    let shared = PathBuf::from(raw.get(1).unwrap_or_else(|| panic!("缺共享目录")));
    let digit: u32 = raw
        .get(2)
        .and_then(|value| value.parse().ok())
        .unwrap_or(2);
    let start_schema = raw
        .get(3)
        .cloned()
        .unwrap_or_else(|| "double_pinyin.ziranma".to_string());
    let roots = vec![user_dir.clone(), shared.clone()];
    let build = user_dir.join("build");
    let Some(mut engine) = Engine::create_with_roots(&build, &start_schema, &roots) else {
        println!("方案装配失败");
        return;
    };
    println!(
        "起始 schema={} （TIP 预算：旧 80+60=140ms / 新 180+60=240ms）",
        engine.schema_id()
    );
    timed(&mut engine, "① Ctrl+` 打开选单", 0x60, K_CONTROL_MASK);
    timed(&mut engine, "② +1 秒后再开一次（对照）", 0x60, K_CONTROL_MASK);
    timed(&mut engine, &format!("③ 数字 {digit} 选方案"), b'0' as u32 + digit, 0);
    // 方案切换若被延后，下一键的耗时里会包含 apply_schema；若没延后，这一键应很快。
    timed(&mut engine, "④ 下一键（字母 a）", b'a' as u32, 0);
    timed(&mut engine, "⑤ 再下一键（字母 b）", b'b' as u32, 0);
    println!("最终 schema={}", engine.schema_id());
}
