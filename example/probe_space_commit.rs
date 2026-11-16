//! 诊断探针：**空格上屏**在虎整句（tiger_sentence）里还灵不灵。
//!
//! 2026-11-08 18:2x 用户报「虎码好像不能用空格上屏了」。
//! 逐键打印 response 的 composing / commit_text / candidates，定位是
//! ① 引擎没提交（lua/处理器吞掉了空格）还是 ② 引擎提交了但宿主侧没上屏（TIP 层）。
//!
//! 用法：
//! ```text
//! cargo run --release --example probe_space_commit -- <用户目录副本> <共享目录> [方案] [字母串]
//! ```

use std::path::PathBuf;
use std::time::Instant;

use librime_next::engine::key_event::KeyEvent;
use librime_next::engine::runtime::Engine;

/// 一次按键后的引擎快照：组合串、末段候选数、本键产生的上屏文本。
fn snapshot(engine: &mut Engine) -> (String, usize, Option<String>) {
    let composing = engine.context.composition.input().to_string();
    let cands = engine
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
        .unwrap_or(0);
    let commit = engine.context.commit_text.take();
    (composing, cands, commit)
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let user_dir = PathBuf::from(raw.first().unwrap_or_else(|| {
        panic!("用法: probe_space_commit <用户目录副本> <共享目录> [方案] [字母串]")
    }));
    let shared = PathBuf::from(raw.get(1).unwrap_or_else(|| panic!("缺共享目录")));
    let schema = raw
        .get(2)
        .cloned()
        .unwrap_or_else(|| "tiger_sentence".to_string());
    let letters = raw
        .get(3)
        .cloned()
        .unwrap_or_else(|| "nihao".to_string());
    let roots = vec![user_dir.clone(), shared.clone()];
    let build = user_dir.join("build");
    let Some(mut engine) = Engine::create_with_roots(&build, &schema, &roots) else {
        println!("方案装配失败: {schema}");
        return;
    };
    println!("schema={} 字母串={letters}", engine.schema_id());
    for ch in letters.chars() {
        let start = Instant::now();
        let eaten = engine.process_key(&KeyEvent::new(ch as u32, 0));
        let (composing, cands, commit) = snapshot(&mut engine);
        println!(
            "  '{ch}': {:6.1} ms eaten={} composing={:?} cands={} commit={:?}",
            start.elapsed().as_secs_f64() * 1000.0,
            eaten,
            composing,
            cands,
            commit,
        );
    }
    // 空格：虎整句里应由 lua 处理器确认首选并提交
    let start = Instant::now();
    let eaten = engine.process_key(&KeyEvent::new(0x20, 0));
    let (composing, cands, commit) = snapshot(&mut engine);
    println!(
        "  SPACE: {:6.1} ms eaten={} composing={:?} cands={} commit={:?}",
        start.elapsed().as_secs_f64() * 1000.0,
        eaten,
        composing,
        cands,
        commit,
    );
    // 再打一个字母，看是不是组字还活着
    let _ = engine.process_key(&KeyEvent::new(b'x' as u32, 0));
    let (composing, cands, commit) = snapshot(&mut engine);
    println!(
        "  之后 'x': composing={:?} cands={} commit={:?}",
        composing, cands, commit,
    );
}
