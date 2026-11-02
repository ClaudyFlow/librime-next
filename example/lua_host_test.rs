//! mlua 基座冒烟:①最小 yield 复现 ②巧码真脚本(number_translator)。
use librime_next::engine::gear::lua::LuaHost;
use std::path::PathBuf;

fn main() {
    // 套文件夹:巧码的平铺脚本包进 lua/qiaoma/(用户规矩)
    let root = std::env::temp_dir().join("lrn_lua_smoke");
    let _ = std::fs::remove_dir_all(&root);
    let plugin = root.join("lua").join("qiaoma");
    std::fs::create_dir_all(&plugin).unwrap();
    for entry in std::fs::read_dir("C:/Code/Weasel-Next/install/巧码/lua").unwrap() {
        let from = entry.unwrap().path();
        if from.extension().and_then(|e| e.to_str()) == Some("lua") {
            let to = plugin.join(from.file_name().unwrap());
            std::fs::copy(&from, &to).unwrap();
        }
    }
    // 最小复现:纯 yield 脚本,验证协程收集语义
    let mini_dir = root.join("lua").join("_mini");
    std::fs::create_dir_all(&mini_dir).unwrap();
    std::fs::write(
        mini_dir.join("mini_yield.lua"),
        "local function mini(input, seg, env)\n  yield(Candidate(\"mini\", seg.start, seg._end, \"你好\", \"\"))\nend\nreturn mini",
    )
    .unwrap();
    let roots = vec![root];
    let host = LuaHost::boot(&roots).expect("VM 启动");
    println!("已登记组件 {} 个", host.translators.len());
    let out = host.run_translator("mini_yield", "ni", 0, 2, "ni", &serde_json::json!({}));
    println!("mini_yield → {} 条(预期 1:你好)", out.len());
    for (text, comment) in &out {
        println!("  {} ({})", text, comment);
    }
    // 巧码真脚本:数字翻译(R 前缀 + 数字)
    let schema_yaml = std::fs::read_to_string(
        "C:/Code/Weasel-Next/install/巧码/schema/qiaoma/qiaoma.schema.yaml",
    )
    .unwrap_or_default();
    let config: serde_json::Value =
        serde_norway::from_str(&schema_yaml).unwrap_or(serde_json::json!({}));
    // 配置链自检:get_string('recognizer/patterns/number') 应返回 R 前缀正则
    let probe_config: serde_json::Value = serde_json::json!({
        "recognizer": { "patterns": { "number": "^R[0-9]+[.]?[0-9]*" } }
    });
    let out_probe = host.run_translator(
        "number_translator",
        "R123",
        0,
        4,
        "R123",
        &probe_config,
    );
    println!(
        "number_translator(自造配置) → {} 条候选",
        out_probe.len()
    );
    for (text, comment) in out_probe.iter().take(3) {
        println!("  {} ({})", text, comment);
    }
    let out = host.run_translator("number_translator", "R123", 0, 4, "R123", &config);
    println!("number_translator(qiaoma配置) → {} 条候选", out.len());
    for (text, comment) in out.iter().take(5) {
        println!("  {} ({})", text, comment);
    }
}
