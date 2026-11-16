//! 临时探针（2026-11-13 21:5x，候选窗 pill/页码扩协议用）：
//! 量「物化全量候选」的耗时与规模，判断底行「1 / N」总页数能否每键实时算。
//!
//! 背景：`RimeMenu` 只给当前页条数 + `is_last_page`，没有总候选数；候选流是懒式的
//! （`Menu::prepare(k)` 只取前 k 条），全量物化正是 2026-10-28 懒化要消灭的卡顿源。
//! 本探针给出真实数字：prepare(8)（UI 首屏）vs prepare(usize::MAX)（算总数）。
//!
//! 用法：
//!   cargo run --release --offline -p librime-next --example probe_menu_total -- \
//!       <build_dir> <user_root> <shared_root> <schema_id> [输入...]

use librime_next::engine::Session;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let build = PathBuf::from(args.next().expect("build_dir"));
    let user = PathBuf::from(args.next().expect("user_root"));
    let shared = PathBuf::from(args.next().expect("shared_root"));
    let schema = args.next().expect("schema_id");
    let inputs: Vec<String> = args.collect();
    let roots = vec![user, shared];
    println!("# schema={schema} inputs={}", inputs.len());

    for input in &inputs {
        let mut s = Session::create_with_roots(&build, &schema, &roots);
        if s.engine.is_none() {
            println!("{input}: ENGINE=None（方案加载失败）");
            continue;
        }
        let t_keys = Instant::now();
        for ch in input.chars() {
            s.process_key(ch as u32, 0);
        }
        let keys_us = t_keys.elapsed().as_micros();

        let stats = {
            let Some(ctx) = s.context() else {
                println!("{input}: 无上下文");
                continue;
            };
            let page_size = ctx.page_size;
            let menu = ctx
                .composition
                .segmentation
                .segments
                .first()
                .and_then(|seg| seg.menu.as_ref());
            match menu {
                None => None,
                Some(m) => {
                    let t = Instant::now();
                    let n8 = m.prepare(page_size.max(8));
                    let prep_us = t.elapsed().as_micros();
                    let t = Instant::now();
                    let total = m.prepare(usize::MAX);
                    let all_us = t.elapsed().as_micros();
                    Some((page_size, n8, prep_us, total, all_us))
                }
            }
        };
        match stats {
            None => println!("{input}: 无菜单"),
            Some((page_size, n8, prep_us, total, all_us)) => println!(
                "{input}: keys={keys_us}us page_size={page_size} prepare({n8})={prep_us}us  prepare(ALL)={all_us}us total={total}  pages={}",
                div_ceil(total, page_size.max(1))
            ),
        }
    }
}

fn div_ceil(a: usize, b: usize) -> usize {
    if b == 0 { 0 } else { a.div_ceil(b) }
}
