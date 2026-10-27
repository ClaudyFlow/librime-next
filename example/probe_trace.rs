//! 逐键探针:每个键的消费情况、input、commit,用于定位多消费/误提交。
//!
//! 用法:`cargo run --example probe_trace -- <build_dir> <schema_id> <输入串>`

use librime_next::engine::Session;
use std::path::PathBuf;

fn main() {
    let build = PathBuf::from(std::env::args().nth(1).expect("build_dir"));
    let schema = std::env::args().nth(2).expect("schema_id");
    let input = std::env::args().nth(3).expect("input");
    // 与 c_api 相同的 roots:用户目录优先、共享目录其次(simplifier 的 opencc 数据在这)
    let user = std::env::var_os("APPDATA")
        .map(|a| std::path::PathBuf::from(a).join("Rime-Next"))
        .expect("APPDATA");
    let shared = std::path::PathBuf::from(r"C:\Program Files\Weasel-Next\Rime-Next");
    let roots = vec![user, shared];
    let mut session = Session::create_with_roots(&build, &schema, &roots);
    println!("engine loaded: {}", session.engine.is_some());
    println!("filters: {:?}", session.engine.as_ref().map(|e| e.filter_names()));
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
