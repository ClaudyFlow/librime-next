//! 按输入类型探针：给定 `schema=pinyin` 与某个 `input_type`（如 flypy / zhuyin / luna），
//! 先在引擎里切到该类型，再喂入输入串，打印切分与候选，验证「菜单切类型 = 同方案不同拼式」正确。
//!
//! 与 `probe_candidates` 的区别：后者固定用方案默认（reset）输入类型；本例显式切到指定类型，
//! 用来确认双拼/注音等「非默认类型」也能从同一份 `pinyin` 方案产出正确候选。
//!
//! 用法：
//!   cargo run --example probe_input_types -- <build> <user> <shared> <schema> <type> [输入...]
//! 例：
//!   cargo run --example probe_input_types -- "$APPDATA/Rime-Next/build" "$APPDATA/Rime-Next" \
//!       "C:/Program Files/Rime-Next/Weasel-Next-0.18.5/data" pinyin flypy jms jmh

use librime_next::engine::Session;
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let build = PathBuf::from(args.next().expect("build_dir"));
    let user = PathBuf::from(args.next().expect("user_root"));
    let shared = PathBuf::from(args.next().expect("shared_root"));
    let schema = args.next().expect("schema_id");
    let itype = args.next().expect("input_type");
    let mut inputs: Vec<String> = args.collect();
    if inputs.is_empty() {
        inputs = [
            "nihao",
            "womenzaizheli",
            "zhongguoren",
            "xianzai",
            "shurufa",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
    }
    let roots = vec![user, shared];
    println!("# schema={schema} input_type={itype}  inputs={}", inputs.len());

    for input in &inputs {
        let mut s = Session::create_with_roots(&build, &schema, &roots);
        {
            let Some(engine) = s.engine_mut() else {
                println!("\n=== {input} ({itype}) ===\nENGINE: None（方案加载失败）");
                continue;
            };
            // 切到目标输入类型：打开它、关掉默认 luna，保证互斥单选（与菜单行为一致）。
            engine.set_option(&itype, true);
            if itype != "luna" {
                engine.set_option("luna", false);
            }
        }
        let mut eaten = 0usize;
        for ch in input.chars() {
            if s.process_key(ch as u32, 0) {
                eaten += 1;
            }
        }
        let Some(ctx) = s.context() else {
            println!("\n=== {input} ({itype}) ===\n没有上下文");
            continue;
        };
        let segs = &ctx.composition.segmentation.segments;
        println!(
            "\n=== {input} ({itype}) ===  input={}  eaten={}/{}  segs={}",
            ctx.input,
            eaten,
            input.chars().count(),
            segs.len()
        );
        for (i, seg) in segs.iter().enumerate() {
            let (count, shown) = match seg.menu.as_ref() {
                Some(m) => {
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
        {
            let engine = s2.engine_mut().unwrap();
            engine.set_option(&itype, true);
            if itype != "luna" {
                engine.set_option("luna", false);
            }
        }
        for ch in input.chars() {
            s2.process_key(ch as u32, 0);
        }
        s2.process_key(0x20, 0); // XK_SPACE
        let commit = s2.take_commit().unwrap_or_default();
        println!("  space→ commit=[{commit}]");
    }
}
