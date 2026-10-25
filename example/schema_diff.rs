//! 语义对比：解析两份编译后的 schema.yaml（走 config 层的 load_file，
//! 与部署产物的解析一致），忽略 __build_info，逐 key 递归比对结构。

use std::collections::BTreeSet;
use std::path::PathBuf;

fn main() {
    let our = std::env::args().nth(1).expect("our schema.yaml");
    let fix = std::env::args().nth(2).expect("fixture schema.yaml");
    let our_path = PathBuf::from(&our);
    let fix_path = PathBuf::from(&fix);

    let our_val = load_yaml(&our_path);
    let fix_val = load_yaml(&fix_path);

    println!("=== 顶层 key 对比 ===");
    let our_keys: BTreeSet<String> = our_val
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    let fix_keys: BTreeSet<String> = fix_val
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    for k in our_keys.union(&fix_keys) {
        let in_our = our_keys.contains(k);
        let in_fix = fix_keys.contains(k);
        if in_our && in_fix {
            println!("  {k}: 两边都有");
        } else if in_our {
            println!("  {k}: ⚠️ 只我们有");
        } else {
            println!("  {k}: ⚠️ 只原版有");
        }
    }

    println!("\n=== 递归差异（忽略 __build_info）===");
    let mut diffs = 0;
    diff_json(&our_val, &fix_val, "", &mut diffs);
    println!("\n=== 差异总数: {diffs} ===");

    // translator 字段对比
    if let Some(t) = our_val.get("translator").and_then(|v| v.as_object()) {
        println!("\n=== translator 字段 ===");
        let fix_keys: BTreeSet<String> = fix_val
            .get("translator")
            .and_then(|v| v.as_object())
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        let all_keys: BTreeSet<String> = t.keys().cloned().chain(fix_keys).collect();
        for k in all_keys {
            let our_v = our_val.get("translator").and_then(|v| v.get(&k));
            let fix_v = fix_val.get("translator").and_then(|v| v.get(&k));
            match (our_v, fix_v) {
                (Some(a), Some(b)) => {
                    if a == b {
                        println!("  {k}: 一致 {a}");
                    } else {
                        println!("  {k}: ⚠️ 不同 our={a} fix={b}");
                        diffs += 1;
                    }
                }
                (Some(a), None) => {
                    println!("  {k}: ⚠️ 只我们有 {a}");
                    diffs += 1;
                }
                (None, Some(b)) => {
                    println!("  {k}: ⚠️ 只原版有 {b}");
                    diffs += 1;
                }
                _ => {}
            }
        }
    }

    // engine 组件对比
    if let Some(eng) = our_val.get("engine").and_then(|v| v.as_object()) {
        println!("\n=== engine 组件 ===");
        let fix_groups: BTreeSet<String> = fix_val
            .get("engine")
            .and_then(|v| v.as_object())
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        let all_groups: BTreeSet<String> = eng.keys().cloned().chain(fix_groups).collect();
        for g in all_groups {
            let our_list = eng
                .get(&g)
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let fix_list = fix_val
                .get("engine")
                .and_then(|v| v.get(&g))
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            if our_list == fix_list {
                println!("  {g}: 都是 {our_list} 个");
            } else {
                println!("  {g}: ⚠️ our={our_list} fix={fix_list}");
                diffs += 1;
            }
        }
    }

    println!("\n=== 最终差异总数: {diffs} ===");
}

fn load_yaml(path: &PathBuf) -> serde_json::Value {
    match librime_next::load_file(path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("解析 {} 失败: {e}", path.display());
            serde_json::json!({})
        }
    }
}

fn diff_json(a: &serde_json::Value, b: &serde_json::Value, path: &str, counter: &mut usize) {
    if path.starts_with("/__build_info") {
        return;
    }
    match (a, b) {
        (serde_json::Value::Object(ma), serde_json::Value::Object(mb)) => {
            let all_keys: BTreeSet<&String> =
                ma.keys().chain(mb.keys()).collect();
            for k in all_keys {
                let sub = format!("{path}/{k}");
                if let (Some(va), Some(vb)) = (ma.get(k), mb.get(k)) {
                    if va != vb {
                        println!("  {sub}: our={va} fix={vb}");
                        *counter += 1;
                    }
                } else {
                    println!("  {sub}: ⚠️ 只在一边存在");
                    *counter += 1;
                }
            }
        }
        (serde_json::Value::Array(aa), serde_json::Value::Array(ab)) => {
            if aa.len() != ab.len() {
                println!(
                    "  {path}: ⚠️ 数组长度 our={} fix={}",
                    aa.len(),
                    ab.len()
                );
                *counter += 1;
            }
        }
        (a, b) => {
            if a != b {
                println!("  {path}: our={a} fix={b}");
                *counter += 1;
            }
        }
    }
}
