//! 方案选单探针：打印 default.yaml 的 schema_list 每一项能否被 Schema::open 解析、
//! 显示名是什么；再真的按 F4 唤起 switcher，打印用户看到的菜单。
//!
//! 用法:`cargo run --example probe_menu -- <build_dir> <user_root> <shared_root>`

use librime_next::engine::schema::Schema;
use librime_next::engine::Session;
use std::path::PathBuf;

fn main() {
    let build = PathBuf::from(std::env::args().nth(1).expect("build_dir"));
    let user = PathBuf::from(std::env::args().nth(2).expect("user_root"));
    let shared = PathBuf::from(std::env::args().nth(3).expect("shared_root"));
    let roots = vec![user.clone(), shared.clone()];

    for root in &roots {
        let path = root.join("default.yaml");
        println!("--- default.yaml: {} (exists={})", path.display(), path.is_file());
        if !path.is_file() {
            continue;
        }
        let config = librime_next::load_file(&path).expect("yaml");
        if let Some(list) = config.pointer("/schema_list").and_then(|v| v.as_array()) {
            for item in list {
                let id = item.get("schema").and_then(|v| v.as_str()).unwrap_or("?");
                let opened = Schema::open(&build, id);
                println!(
                    "  schema_list: {id:<22} open={} name={:?}",
                    opened.is_some(),
                    opened.as_ref().map(|s| s.schema_name.clone())
                );
            }
        } else {
            println!("  (no schema_list)");
        }
    }

    println!("\n=== F4 唤起 switcher(方案 pinyin.luna) ===");
    let mut s = Session::create_with_roots(&build, "pinyin.luna", &roots);
    println!("engine={}", s.engine.is_some());
    // F4 = 0xffc1
    let eaten = s.process_key(0xffc1, 0);
    println!("F4 eaten={eaten}");
    if let Some(ctx) = s.context() {
        for seg in &ctx.composition.segmentation.segments {
            if let Some(m) = seg.menu.as_ref() {
                println!("menu candidates={}", m.candidate_count());
                for (i, c) in m.candidates().iter().enumerate().take(40) {
                    println!("  [{i}] {:?}", c.text);
                }
            }
        }
    }
}
