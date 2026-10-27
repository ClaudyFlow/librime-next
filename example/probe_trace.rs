//! 逐键探针:每个键的消费情况、input、commit,用于定位多消费/误提交。
//!
//! 用法:`cargo run --example probe_trace -- <build_dir> <schema_id> <输入串>`

use librime_next::engine::Session;
use std::path::PathBuf;

fn main() {
    let build = PathBuf::from(std::env::args().nth(1).expect("build_dir"));
    let schema = std::env::args().nth(2).expect("schema_id");
    let input = std::env::args().nth(3).expect("input");
    let mut session = Session::create(&build, &schema);
    println!("engine loaded: {}", session.engine.is_some());
    for ch in input.chars() {
        let ok = session.process_key(ch as u32, 0);
        let now = session
            .context()
            .map(|c| c.input.clone())
            .unwrap_or_default();
        let commit = session.take_commit();
        println!("key {ch}: consumed={ok} input={now:?} commit={commit:?}");
    }
    if let Some(ctx) = session.context() {
        for (i, seg) in ctx.composition.segmentation.segments.iter().enumerate() {
            let menu = seg.menu.as_ref().map_or(0, |m| m.candidate_count());
            println!("seg[{i}] [{},{}) tags={:?} menu={menu}", seg.start, seg.end, seg.tags);
            if let Some(m) = seg.menu.as_ref() {
                for (j, c) in m.candidates().iter().enumerate().take(6) {
                    println!("  cand[{j}] [{},{}) q={:.0} {}", c.start, c.end, c.quality, c.text);
                }
            }
        }
    }
}
