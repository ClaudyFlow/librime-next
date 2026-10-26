//! 探查 yuling 为什么 0 候选:组件创建日志 + 词典打开 + 分段/翻译过程。
use librime_next::engine::Session;
use std::path::PathBuf;

fn main() {
    let staging = std::env::args().nth(1).expect("用法: probe_yuling <staging_dir>");
    let staging = PathBuf::from(&staging);
    let mut session = Session::create(&staging, std::env::args().nth(2).unwrap_or("yuling".into()).as_str());
    println!("engine loaded: {}", session.engine.is_some());
    for ch in std::env::args().nth(3).unwrap_or("yywa".into()).chars() {
        let ok = session.process_key(ch as u32, 0);
        println!("key {ch}: consumed={ok}");
    }
    let ctx = session.context().unwrap();
    println!("input={:?} composing={}", ctx.input, ctx.is_composing());
    for (i, seg) in ctx.composition.segmentation.segments.iter().enumerate() {
        println!("seg[{i}] [{},{}) tags={:?} menu={}", seg.start, seg.end, seg.tags,
            seg.menu.as_ref().map_or(0, |m| m.candidate_count()));
    }
}
