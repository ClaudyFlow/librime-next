//! mlua 基座冒烟:加载巧码的 number_translator.lua,跑一次翻译。
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
    let roots = vec![root];
    let host = LuaHost::boot(&roots).expect("VM 启动");
    println!("已登记组件 {} 个", host.translators.len());
    for name in host.translators.keys() {
        println!("  - {name}");
    }
    // number_translator:触发前缀 R + 数字(配置来自 qiaoma 方案)
    let schema_yaml = std::fs::read_to_string(
        "C:/Code/Weasel-Next/install/巧码/schema/qiaoma/qiaoma.schema.yaml",
    )
    .unwrap_or_default();
    let config: serde_json::Value =
        serde_norway::from_str(&schema_yaml).unwrap_or(serde_json::json!({}));
    let out = host.run_translator("number_translator", "R123", 0, 4, "R123", &config);
    println!("R123 → {} 条候选", out.len());
    for (text, comment) in out.iter().take(5) {
        println!("  {} ({})", text, comment);
    }
}
