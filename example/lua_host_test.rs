//! mlua 基座冒烟：①最小 yield 复现 ②真实社区脚本（有就测）。
//!
//! 用法：`cargo run --example lua_host_test -- <数据根> [组件名] [输入]`
//! 默认数据根是仓库旁的 `.tmp_qiaoma/shared`、组件 `number_translator`、输入 `R123`。

use librime_next::engine::gear::lua::LuaHost;
use std::path::PathBuf;

fn main() {
    let fallback = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(".tmp_qiaoma")
        .join("shared");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = args.first().map(PathBuf::from).unwrap_or(fallback);
    let component = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "number_translator".to_string());
    let input = args.get(2).cloned().unwrap_or_else(|| "R123".to_string());

    // 最小复现：纯 yield 脚本，验证协程收集语义
    let mini = std::env::temp_dir().join(format!("lrn_lua_mini_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&mini);
    std::fs::create_dir_all(mini.join("lua")).unwrap();
    std::fs::write(
        mini.join("lua").join("mini_yield.lua"),
        "local function mini(input, seg, env)\n  yield(Candidate(\"mini\", seg.start, seg._end, \"你好\", \"注释\"))\nend\nreturn mini",
    )
    .unwrap();

    let host = LuaHost::boot(&[mini.clone()]).expect("VM 启动");
    let out = host.run_translator("mini_yield", "ni", 0, 2, "ni", &serde_json::json!({}));
    println!("mini_yield → {} 条（预期 1: 你好/注释）", out.len());
    for (text, comment) in &out {
        println!("  {text} ({comment})");
    }

    // 真实脚本（数据根不存在就跳过）
    if root.is_dir() {
        let host = LuaHost::boot(&[root.clone()]).expect("VM 启动");
        let config = serde_json::json!({
            "recognizer": { "patterns": { "number": "^R[0-9]+[.]?[0-9]*" } }
        });
        let end = input.chars().count();
        let out = host.run_translator(&component, &input, 0, end, &input, &config);
        println!("\n{component}({input}) → {} 条候选", out.len());
        for (text, comment) in out.iter().take(6) {
            println!("  {text} ({comment})");
        }
    } else {
        println!("\n跳过真实脚本：{} 不存在", root.display());
    }
    let _ = std::fs::remove_dir_all(&mini);
}
