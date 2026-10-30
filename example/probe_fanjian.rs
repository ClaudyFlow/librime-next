//! 繁简切换实测：加载拼音方案，切 `zh_simp` / `traditionalization` 两个开关状态，
//! 对比候选的简/繁，确认「设为简体用 t2s、设为繁体用 s2t」两个方向都生效。
//!
//! 用法:`cargo run --example probe_fanjian -- <build> <user> <shared> <schema> <seq>`

use librime_next::engine::Session;
use std::path::PathBuf;

fn dump(s: &Session, label: &str) {
    print!("  [{label}] ");
    match s.context() {
        Some(ctx) => {
            let mut out: Vec<String> = Vec::new();
            for seg in &ctx.composition.segmentation.segments {
                if let Some(m) = seg.menu.as_ref() {
                    for c in m.candidates().iter().take(8) {
                        out.push(c.text.clone());
                    }
                }
            }
            println!("{}", out.join(" | "));
        }
        None => println!("(no context)"),
    }
}

fn type_seq(s: &mut Session, seq: &str) {
    for ch in seq.chars() {
        s.process_key(ch as u32, 0);
    }
}

fn main() {
    let build = PathBuf::from(std::env::args().nth(1).expect("build_dir"));
    let user = PathBuf::from(std::env::args().nth(2).expect("user_root"));
    let shared = PathBuf::from(std::env::args().nth(3).expect("shared_root"));
    let roots = vec![user, shared];
    let schema = std::env::args().nth(4).unwrap_or_else(|| "luna_pinyin_simp".into());
    let seq = std::env::args().nth(5).unwrap_or_else(|| "tianqi".into());

    println!("===== {schema} 输入 {seq} =====");

    // 简体方向：zh_simp 开、traditionalization 关 → 期望简体候选
    let mut simp = Session::create_with_roots(&build, &schema, &roots);
    simp.engine_mut().unwrap().set_option("zh_simp", true);
    simp.engine_mut().unwrap().set_option("traditionalization", false);
    type_seq(&mut simp, &seq);
    dump(&simp, "简体 zh_simp=1,trad=0");

    // 繁体方向：zh_simp 关、traditionalization 开 → 期望繁体候选（s2t 转繁）
    let mut trad = Session::create_with_roots(&build, &schema, &roots);
    trad.engine_mut().unwrap().set_option("zh_simp", false);
    trad.engine_mut().unwrap().set_option("traditionalization", true);
    type_seq(&mut trad, &seq);
    dump(&trad, "繁体 zh_simp=0,trad=1");
}
