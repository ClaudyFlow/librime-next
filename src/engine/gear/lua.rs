//! Lua 兼容层 · 第一增量:插件文件夹发现与组件注册表(不含 VM)。
//!
//! 用户 2026-11-01 拍板:修 Lua,做到能打字为止;**lua 插件要套文件夹**。
//! 布局:`<数据根>/lua/<插件名>/`——每个插件独立文件夹,主脚本 `rime.lua`
//! 在文件夹内(与 rime-lua 社区习惯对齐,又满足"套文件夹")。
//!
//! 第二增量(2026-11-03 本提交):mlua VM 基座 + **translator 兼容**。
//! rime-lua 翻译器约定:`function translator(input, seg, env)` 返回
//! Candidate 表(数组)。Candidate 用 5 参构造(type,start,end,text,comment)。
//! env = { engine = { context = { input, commit_text, clear }, schema = {} },
//!         name_space = 组件名, first_time = true }。
//! processor/filter 兼容面(第三增量)按巧码脚本逐个补。

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


// ── 第二增量:VM 基座 ─────────────────────────────────────────────

use mlua::{Lua, Table};

/// 进程级 Lua VM 单例(一个 VM 服务所有插件的组件)。
pub struct LuaHost {
    pub lua: Lua,
    /// 已登记的翻译器组件:`组件名 → 脚本路径`(rime.lua 或同名 .lua)。
    pub translators: std::collections::HashMap<String, PathBuf>,
}

impl LuaHost {
    /// 启动 VM:package.path 指向各插件文件夹,扫描组件。
    pub fn boot(data_roots: &[PathBuf]) -> mlua::Result<Self> {
        let lua = Lua::new();
        let plugins = discover_plugins(data_roots);
        // package.path:每个插件文件夹都能 require
        let mut paths: Vec<String> = Vec::new();
        let mut translators = HashMap::new();
        for plugin in &plugins {
            let folder_str = plugin.folder.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
            paths.push(format!("{folder_str}/?.lua"));
            for script in &plugin.scripts {
                // 约定:脚本名 = 组件名(rime-lua 的 `*名字` = 同名 .lua)
                let stem = script.strip_suffix(".lua").unwrap_or(script);
                let path = plugin.folder.join(script);
                translators
                    .entry(stem.to_string())
                    .or_insert_with(|| path.clone());
                if script == "rime.lua" {
                    // rime.lua 里的组件按需注册(第三增量解析)
                }
            }
        }
        let globals = lua.globals();
        let package: Table = globals.get("package")?;
        let existing: String = package.get("path")?;
        package.set("path", format!("{};{}", paths.join(";"), existing))?;
        Ok(Self { lua, translators })
    }

    /// 加载脚本并取回翻译器函数:脚本文件 **return 一个函数**
    /// (`local function foo(...) end; return foo`),要先调用 chunk
    /// 拿到那个返回的函数——直接把 chunk 包进协程会只跑一次就结束。
    fn translator_fn(&self, component: &str) -> mlua::Result<mlua::Function> {
        let Some(path) = self.translators.get(component) else {
            return Err(mlua::Error::external(format!("组件 {component} 未找到")));
        };
        let source = std::fs::read_to_string(path)?;
        let chunk_name = format!("@{}", path.display());
        let chunk_fn = self.lua.load(&source).set_name(chunk_name).into_function()?;
        let returned: mlua::Value = chunk_fn.call(())?;
        match returned {
            mlua::Value::Function(f) => Ok(f),
            _ => Ok(chunk_fn), // 脚本没 return(直接定义全局)——退回 chunk
        }
    }

    /// 跑一个 lua 翻译器:input/seg 包装成 rime-lua 形态,收回候选表。
    /// 返回 (text, comment) 列表——调用方转成 Candidate。
    /// `config` = 方案配置树(env.engine.schema.config:get_string 的后端)。
    pub fn run_translator(
        &self,
        component: &str,
        input: &str,
        seg_start: usize,
        seg_end: usize,
        context_input: &str,
        config: &serde_json::Value,
    ) -> Vec<(String, String)> {
        let Ok(func) = self.translator_fn(component) else {
            return Vec::new();
        };
        let lua = &self.lua;
        // env.engine.context.input(只读);commit_text/clear 下一增量接回调
        let (Some(env), Some(engine), Some(context), Some(seg), Some(collected)) = (
            lua.create_table().ok(),
            lua.create_table().ok(),
            lua.create_table().ok(),
            lua.create_table().ok(),
            lua.create_table().ok(),
        ) else {
            return Vec::new();
        };
        let _ = context.set("input", context_input.to_string());
        let _ = context.set("input", context_input.to_string());
        let _ = engine.set("context", context);
        // schema.config:get_string(path):配置树点路径查询
        let schema = lua.create_table().ok();
        if let Some(schema) = &schema {
            let config_table = lua.create_table().ok();
            if let Some(config_table) = &config_table {
                let cfg = config.clone();
                let get_string = lua
                    .create_function(move |_, path: String| {
                        let pointer = format!(
                            "/{}",
                            path.split('/').collect::<Vec<_>>().join("/")
                        );
                        Ok(cfg
                            .pointer(&pointer)
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()))
                    })
                    .ok();
                if let Some(get_string) = get_string {
                    let _ = config_table.set("get_string", get_string);
                }
            }
            if let Some(config_table) = &config_table {
                let _ = schema.set("config", config_table.clone());
            }
            let _ = engine.set("schema", schema.clone());
        }
        let _ = env.set("engine", engine);
        let _ = env.set("name_space", component.to_string());
        let _ = env.set("first_time", false);
        let _ = seg.set("start", seg_start);
        let _ = seg.set("_end", seg_end);
        let _ = seg.set("start_pos", seg_start);
        let _ = seg.set("_end_pos", seg_end);
        let collected_clone = collected.clone();
        // 参数放宽为 Value:巧码脚本会传 number/nil(如 numberPart[i][1] 可能是数字)
        let candidate_fn = lua
            .create_function(
                move |lua, (ctype, start, end, text, comment): (
                    mlua::Value,
                    usize,
                    usize,
                    mlua::Value,
                    mlua::Value,
                )| {
                    let stringify = |value: &mlua::Value| -> String {
                        match value {
                            mlua::Value::String(str) => {
                                str.to_string_lossy().to_string()
                            }
                            mlua::Value::Integer(i) => i.to_string(),
                            mlua::Value::Number(n) => n.to_string(),
                            _ => String::new(),
                        }
                    };
                    let t = lua.create_table()?;
                    t.set("type", stringify(&ctype))?;
                    t.set("start", start)?;
                    t.set("_end", end)?;
                    t.set("text", stringify(&text))?;
                    t.set("comment", stringify(&comment))?;
                    collected_clone.set(collected_clone.raw_len() + 1, t.clone())?;
                    Ok(t)
                },
            )
            .ok();
        let Some(candidate_fn) = candidate_fn else { return Vec::new() };
        let _ = lua.globals().set("Candidate", candidate_fn);
        // rime-lua 翻译器用 **yield(Candidate(...))** 协程语义(不是 return):
        // 把脚本函数包进协程,resume 循环里收每一批 yield 出来的候选。
        let Ok(thread) = lua.create_thread(func) else { return Vec::new() };
        // 全局 yield = 协程让位(脚本里的裸 yield 调用)
        if let Ok(coroutine) = lua.globals().get::<Table>("coroutine")
            && let Ok(yield_fn) = coroutine.get::<mlua::Function>("yield")
        {
            let _ = lua.globals().set("yield", yield_fn);
        }
        let mut result = Vec::new();
        // 首次 resume 必须带上 (input, seg, env)——协程体的三个参数
        let lua_string_input = match lua.create_string(input) {
            Ok(value) => value,
            Err(_) => return Vec::new(),
        };
        let mut pending: Option<mlua::MultiValue> = Some(mlua::MultiValue::from_vec(vec![
            mlua::Value::String(lua_string_input),
            mlua::Value::Table(seg),
            mlua::Value::Table(env),
        ]));
        loop {
            let resume = match pending.take() {
                Some(args) => thread.resume::<mlua::MultiValue>(args),
                None => thread.resume::<mlua::MultiValue>(()),
            };
            match resume {
                Ok(values) => {
                    let mut finished = true;
                    for value in values {
                        match value {
                            mlua::Value::Table(t) => {
                                // yield 出来的候选表
                                if let Ok(text) = t.get::<String>("text") {
                                    let comment =
                                        t.get::<String>("comment").unwrap_or_default();
                                    result.push((text, comment));
                                }
                                finished = false;
                            }
                            _ => finished = false,
                        }
                    }
                    if finished {
                        break;
                    }
                }
                Err(error) => {
                    // 协程错误:脚本异常(正常返回后 resume 会 dead coroutine,
                    // 那是正常结束;其余是脚本错误,打出来)
                    let text = error.to_string();
                    if !text.contains("cannot resume dead coroutine") {
                        // 带 traceback 定位脚本行号
                        let trace = match lua.globals().raw_get::<Table>("debug") {
                            Ok(debug) => match debug.raw_get::<mlua::Function>("traceback") {
                                Ok(tb) => tb
                                    .call::<String>((text.clone(),))
                                    .unwrap_or_else(|_| text.clone()),
                                Err(_) => String::new(),
                            },
                            Err(_) => String::new(),
                        };
                        eprintln!("[lua] {component} 协程错误:\n{trace}");
                    }
                    break;
                }
            }
        }
        let _ = collected;
        result
    }
}

use std::collections::HashMap;
