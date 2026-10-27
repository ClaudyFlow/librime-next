//! 探查 yuling 为什么 0 候选:组件创建日志 + 词典打开 + 分段/翻译过程。
use librime_next::engine::Session;
use std::path::PathBuf;

fn main() {
    let staging = std::env::args().nth(1).expect("用法: probe_yuling <staging_dir>");
    let staging = PathBuf::from(&staging);
    let mut session = Session::create(&staging, std::env::args().nth(2).unwrap_or("yuling".into()).as_str());
    println!("engine loaded: {}", session.engine.is_some());
    let input = std::env::args().nth(3).unwrap_or("yywa".into());
    for ch in input.chars() {
        let ok = session.process_key(ch as u32, 0);
        println!("key {ch}: consumed={ok}");
    }
    let ctx = session.context().unwrap();
    println!("input={:?} composing={}", ctx.input, ctx.is_composing());
    for (i, seg) in ctx.composition.segmentation.segments.iter().enumerate() {
        if let Some(m) = seg.menu.as_ref() {
            for (j, c) in m.candidates().iter().enumerate().take(8) {
                println!("seg[{i}] cand[{j}] [{},{}) q={:.0} text={}", c.start, c.end, c.quality, c.text);
            }
        }
    }
}
