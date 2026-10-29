//! 逐方案探针：对 schema 清单里每个 id 建一个 Session，喂一组键序，打印候选/上屏。
//!
//! 用法:`cargo run --example probe_all_schemas -- <build_dir> <user_root> <shared_root> [id...]`

use librime_next::engine::Session;
use std::path::PathBuf;

fn main() {
    let build = PathBuf::from(std::env::args().nth(1).expect("build_dir"));
    let user = PathBuf::from(std::env::args().nth(2).expect("user_root"));
    let shared = PathBuf::from(std::env::args().nth(3).expect("shared_root"));
    let mut ids: Vec<String> = std::env::args().skip(4).collect();
    if ids.is_empty() {
        ids = vec![
            "cangjie.5".into(),
            "double_pinyin.flypy".into(),
            "double_pinyin.ziranma".into(),
            "pinyin.luna".into(),
            "pinyin.terra".into(),
            "stroke".into(),
            "wubi.86".into(),
            "zhuyin".into(),
        ];
    }
    let roots = vec![user, shared];
    for id in &ids {
        println!("\n========== {id} ==========");
        let mut session = Session::create_with_roots(&build, id, &roots);
        match session.engine.as_ref() {
            None => {
                println!("ENGINE: None  <-- 方案加载失败");
                continue;
            }
            Some(e) => {
                println!(
                    "ENGINE: ok  schema_id={:?}",
                    e.schema.schema_id
                );
                println!("  processors={:?}", e.processor_names());
                println!("  translators={:?}", e.translator_names());
                println!("  filters={:?}", e.filter_names());
                println!("  dict={:?}", e.schema.first_translator_dict());
            }
        }
        for input in ["nihao", "ni", "12345", "ttyy", "hq", "a"] {
            let mut s = Session::create_with_roots(&build, id, &roots);
            let mut consumed = 0usize;
            for ch in input.chars() {
                if s.process_key(ch as u32, 0) {
                    consumed += 1;
                }
            }
            let commit = s.take_commit();
            let ctx_input = s.context().map(|c| c.input.clone()).unwrap_or_default();
            let (segs, first) = match s.context() {
                Some(ctx) => {
                    let mut n = 0usize;
                    let mut firsts: Vec<String> = Vec::new();
                    for seg in &ctx.composition.segmentation.segments {
                        if let Some(m) = seg.menu.as_ref() {
                            n += m.candidate_count();
                            if let Some(c) = m.candidates().first() {
                                firsts.push(c.text.clone());
                            }
                        }
                    }
                    (n, firsts.join(" | "))
                }
                None => (0, String::new()),
            };
            println!(
                "  input {input:>6}: consumed={consumed}/{} input={ctx_input:?} commit={commit:?} cands={segs} first=[{first}]",
                input.chars().count()
            );
        }
    }
}
