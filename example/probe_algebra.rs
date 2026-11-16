//! 临时探针：应用某方案 speller/algebra 到样例音节，打印逐规则累积变换。
//! 用法: cargo run --example probe_algebra -- <schema.yaml> <syllable>...
use librime_next::algo::algebra::Projection;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let schema_path = &args[1];
    let text = std::fs::read_to_string(schema_path).expect("read schema");
    let mut rules: Vec<String> = Vec::new();
    let mut in_algebra = false;
    for line in text.lines() {
        if line.trim_end() == "  algebra:" {
            in_algebra = true;
            continue;
        }
        if in_algebra {
            let t = line.trim_start();
            if t.starts_with("- ") {
                rules.push(t[2..].trim().trim_matches('"').to_string());
                continue;
            }
            if !t.starts_with('#') && !t.is_empty() {
                in_algebra = false;
            }
        }
    }
    println!("rules={}", rules.len());
    for syl in &args[2..] {
        let mut cur = syl.clone();
        print!("── {syl} ── ");
        let mut trace = vec![cur.clone()];
        for n in 1..=rules.len() {
            let p = match Projection::from_rules(&rules[..n]) {
                Ok(p) => p,
                Err(e) => {
                    println!("\n  PROJ ERR at #{n}: {e}");
                    break;
                }
            };
            let mut p = p;
            let mut s = syl.clone();
            p.apply_str(&mut s);
            if s != cur {
                trace.push(format!("#{}:{}", n, s));
                cur = s;
            }
        }
        println!("{:?}", trace);
    }
}
