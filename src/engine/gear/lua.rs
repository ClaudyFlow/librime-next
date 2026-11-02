//! Lua 兼容层 · 第一增量:插件文件夹发现与组件注册表(不含 VM)。
//!
//! 用户 2026-11-01 拍板:修 Lua,做到能打字为止;**lua 插件要套文件夹**。
//! 布局:`<数据根>/lua/<插件名>/`——每个插件独立文件夹,主脚本 `rime.lua`
//! 在文件夹内(与 rime-lua 社区习惯对齐,又满足"套文件夹")。
//!
//! 第二增量(下棒):mlua(lua54 vendored)嵌 VM,三类组件
//! (LuaProcessor/LuaTranslator/LuaFilter)实现我们的 trait,按目标脚本
//! 最小集做 rime-lua API 兼容面(巧码:*enter_code_space + 6 translator)。
//! 本模块只做:**发现**与**登记**——谁在哪个文件夹、暴露哪些组件名,
//! VM 接入后由同一张注册表实例化。

use std::path::{Path, PathBuf};

/// 一个已发现的 lua 插件文件夹。
#[derive(Debug, Clone)]
pub struct LuaPlugin {
    /// 插件名 = 文件夹名。
    pub name: String,
    /// 插件文件夹绝对路径。
    pub folder: PathBuf,
    /// 主脚本(rime.lua)是否存在——rime-lua 的入口约定。
    pub has_entry: bool,
    /// 文件夹里的全部 .lua 脚本名(诊断用)。
    pub scripts: Vec<String>,
}

/// 在数据根里发现 lua 插件(`<root>/lua/<插件名>/`,**套文件夹**)。
///
/// 返回按插件名排序的列表;`lua/` 不存在或为空 = 没有插件(不是错误)。
pub fn discover_plugins(data_roots: &[PathBuf]) -> Vec<LuaPlugin> {
    let mut out: Vec<LuaPlugin> = Vec::new();
    for root in data_roots {
        let lua_dir = root.join("lua");
        let Ok(entries) = std::fs::read_dir(&lua_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let folder = entry.path();
            if !folder.is_dir() {
                // 平铺的散脚本不合规矩(要套文件夹),忽略并留给诊断日志
                continue;
            }
            let name = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let mut scripts = Vec::new();
            let mut has_entry = false;
            if let Ok(files) = std::fs::read_dir(&folder) {
                for file in files.flatten() {
                    let path = file.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("lua") {
                        let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                        if file_name == "rime.lua" {
                            has_entry = true;
                        }
                        scripts.push(file_name);
                    }
                }
            }
            scripts.sort();
            if !out.iter().any(|p| p.name == name) {
                out.push(LuaPlugin { name, folder, has_entry, scripts });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// 从 schema engine 段的处方里收集 lua 组件需求:
/// `lua_processor@*组件名` / `lua_translator@*名` / `lua_filter@*名`
/// (`*` 前缀 = 从 rime.lua 的约定表取构造器;无 `*` = 直接同名全局函数)。
/// 返回 `(组件类型, 组件名)` 列表,按出现顺序。
pub fn collect_required_components(engine_config: &serde_json::Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let sections = [
        ("processors", "processor"),
        ("translators", "translator"),
        ("filters", "filter"),
    ];
    for (list_key, kind) in sections {
        let Some(list) = engine_config
            .pointer(&format!("/engine/{list_key}"))
            .and_then(|v| v.as_array())
        else {
            continue;
        };
        for item in list {
            let Some(spec) = item.as_str() else { continue };
            // 处方形如 `lua_processor@*enter_code_space` 或 `lua_translator@foo`
            let Some((class, name_part)) = spec.split_once('@') else { continue };
            if !class.starts_with("lua_") {
                continue;
            }
            let name = name_part.strip_prefix('*').unwrap_or(name_part).trim();
            if !name.is_empty() {
                out.push((kind.to_string(), name.to_string()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_folder_plugins_and_ignores_loose_files() {
        let dir = std::env::temp_dir().join(format!("lrn_lua_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let plugins = dir.join("lua");
        std::fs::create_dir_all(plugins.join("qiaoma")).unwrap();
        std::fs::write(plugins.join("qiaoma").join("rime.lua"), "-- entry").unwrap();
        std::fs::write(plugins.join("qiaoma").join("calc.lua"), "-- x").unwrap();
        std::fs::create_dir_all(plugins.join("tools")).unwrap();
        std::fs::write(plugins.join("tools").join("main.lua"), "-- t").unwrap();
        std::fs::write(plugins.join("loose.lua"), "-- loose script").unwrap();

        let found = discover_plugins(&[dir.clone()]);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].name, "qiaoma");
        assert!(found[0].has_entry);
        assert_eq!(found[0].scripts, vec!["calc.lua", "rime.lua"]);
        assert_eq!(found[1].name, "tools");
        assert!(!found[1].has_entry);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collects_lua_components_from_engine_prescriptions() {
        let config: serde_json::Value = serde_json::json!({
            "engine": {
                "processors": ["ascii_composer", "lua_processor@*enter_code_space"],
                "translators": ["script_translator", "lua_translator@*calc", "lua_translator@lunar"],
                "filters": ["uniquifier", "lua_filter@*code_len"]
            }
        });
        let got = collect_required_components(&config);
        assert_eq!(
            got,
            vec![
                ("processor".into(), "enter_code_space".into()),
                ("translator".into(), "calc".into()),
                ("translator".into(), "lunar".into()),
                ("filter".into(), "code_len".into()),
            ]
        );
        // 非 lua 组件不收
        let none: serde_json::Value = serde_json::json!({ "engine": { "processors": ["speller"] } });
        assert!(collect_required_components(&none).is_empty());
    }
}
