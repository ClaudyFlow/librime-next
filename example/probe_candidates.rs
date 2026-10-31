//! 候选/切分探针：同一组输入，打印「切分 + 每段候选」，用来做改前/改后、原版/自研对比。
//!
//! 用法：
//!   cargo run --release --example probe_candidates -- <build_dir> <user_root> <shared_root> <schema_id> [输入...]
//! 例：
//!   cargo run --release --example probe_candidates -- "$APPDATA/Rime-Next/build" "$APPDATA/Rime-Next" \
//!       "C:/Program Files/Rime-Next/Weasel-Next-0.18.5/data" double_pinyin.flypy nihao nihaoshijie

use librime_next::engine::Session;
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let build = PathBuf::from(args.next().expect("build_dir"));
    let user = PathBuf::from(args.next().expect("user_root"));
    let shared = PathBuf::from(args.next().expect("shared_root"));
    let schema = args.next().expect("schema_id");
    let mut inputs: Vec<String> = args.collect();
    if inputs.is_empty() {
        inputs = [
            "nihao",
            "nihaoshijie",
            "womenzaizheli",
            "zhongguoren",
            "jintiantianqihenhao",
            "xianzai",
            "shurufa",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
    }
    let roots = vec![user, shared];
    println!("# schema={schema}  inputs={}", inputs.len());

    for input in &inputs {
        let mut s = Session::create_with_roots(&build, &schema, &roots);
        if s.engine.is_none() {
            println!("\n=== {input} ===\nENGINE: None（方案加载失败）");
            continue;
        }
        let mut eaten = 0usize;
        for ch in input.chars() {
            if s.process_key(ch as u32, 0) {
                eaten += 1;
            }
        }
        let ctx = match s.context() {
            Some(c) => c,
            None => {
                println!("\n=== {input} ===\n没有上下文");
                continue;
            }
        };
        let segs = &ctx.composition.segmentation.segments;
        println!(
            "\n=== {input} ===  input={}  按键被吃={}/{}  segs={}",
            ctx.input,
            eaten,
            input.chars().count(),
            segs.len()
        );
        for (i, seg) in segs.iter().enumerate() {
            let (count, shown) = match seg.menu.as_ref() {
                Some(m) => {
                    // 菜单是懒式的：先 prepare 到 8 条，再取（UI 的翻页也是这么做的）。
                    m.prepare(8);
                    let texts: Vec<String> = (0..m.candidate_count().min(8))
                        .filter_map(|i| m.candidate_at(i))
                        .map(|c| format!("{}[{}]", c.text, c.comment))
                        .collect();
                    (m.candidate_count(), texts.join("  "))
                }
                None => (0, "（无菜单）".to_string()),
            };
            println!(
                "  seg[{i}] {}..{} tags={:?} n={count}\n        {}",
                seg.start,
                seg.end,
                seg.tags,
                shown
            );
        }
        // 空格提交后的文本（第一候选上屏）
        let mut s2 = Session::create_with_roots(&build, &schema, &roots);
        for ch in input.chars() {
            s2.process_key(ch as u32, 0);
        }
        s2.process_key(0x20, 0); // XK_SPACE
        let commit = s2.take_commit().unwrap_or_default();
        println!("  space→ commit=[{commit}]");
    }
}
