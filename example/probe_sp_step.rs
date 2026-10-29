//! 双拼逐键探针：每按一键打印一次 input / 候选数 / 首选，用来定位"第几个键才出候选"。
//!
//! 用法:`cargo run --example probe_sp_step -- <build_dir> <user_root> <shared_root>`

use librime_next::engine::Session;
use std::path::PathBuf;

fn dump(s: &Session, key: char) {
    let ctx_input = s.context().map(|c| c.input.clone()).unwrap_or_default();
    let mut n = 0usize;
    let mut firsts: Vec<String> = Vec::new();
    if let Some(ctx) = s.context() {
        for seg in &ctx.composition.segmentation.segments {
            if let Some(m) = seg.menu.as_ref() {
                n += m.candidate_count();
                if let Some(c) = m.candidates().first() {
                    firsts.push(c.text.clone());
                }
            }
        }
    }
    println!(
        "   [{key}] input={ctx_input:?} cands={n} first=[{}]",
        firsts.join(" | ")
    );
}

fn main() {
    let build = PathBuf::from(std::env::args().nth(1).expect("build_dir"));
    let user = PathBuf::from(std::env::args().nth(2).expect("user_root"));
    let shared = PathBuf::from(std::env::args().nth(3).expect("shared_root"));
    let roots = vec![user, shared];

    let seqs: Vec<String> = std::env::args().skip(4).collect();
    let seqs: Vec<&str> = if seqs.is_empty() {
        vec!["nihk", "wo", "w", "ni", "n", "ziji", "womf", "uh", "nh"]
    } else {
        seqs.iter().map(String::as_str).collect()
    };

    for id in ["double_pinyin.ziranma", "double_pinyin.flypy", "pinyin.luna"] {
        println!("\n===== {id} =====");
        for seq in &seqs {
            let mut s = Session::create_with_roots(&build, id, &roots);
            if s.engine.is_none() {
                println!("  ENGINE None <-- 方案加载失败");
                continue;
            }
            println!(" seq {seq:?}");
            for ch in seq.chars() {
                let t0 = std::time::Instant::now();
                let eaten = s.process_key(ch as u32, 0);
                let elapsed = t0.elapsed();
                dump(&s, ch);
                let commit = s.take_commit();
                println!(
                    "      eaten={eaten} commit={commit:?} 键耗时 {:.2} ms",
                    elapsed.as_secs_f64() * 1000.0
                );
            }
        }
    }
}
