//! `src/rime/schema.{h,cc}`：方案（包装编译后的 Config 树）。
//!
//! 运行时只读：引擎从方案里读 `engine/processors`、`engine/translators`、以及
//! `translator/<ns>` 下的 `dictionary` / `prism` / `packs` 等，决定加载哪个词典、怎么组字。

pub mod switches;
pub mod list_translator;

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::load_file;

/// 方案（对照 C++ `Schema`）。
pub struct Schema {
    pub schema_id: String,
    pub schema_name: String,
    /// 编译后的配置树（`build/<系列>/<id>.schema.yaml`）。
    pub config: Value,
    /// `menu/page_size`（C++ `Schema::page_size`）。
    pub page_size: usize,
    /// `menu/alternative_select_keys`（C++ `Schema::select_keys`）。
    pub select_keys: String,
    /// `menu/page_down_cycle`（C++ `Schema::page_down_cycle`）：
    /// 末页再下翻时是否回到第一页。
    pub page_down_cycle: bool,
    /// `schema/parent`：父方案 id（"方案 → 子方案"两级菜单）。
    ///
    /// 最小语法：在子方案编译产物的 `schema:` 块里写 `parent: <父方案 id>`。
    /// - 缺字段 = 平铺（与历史行为完全一致）；
    /// - 父 id 在 `schema_list` 里不存在 = 也按平铺处理（不吞掉方案）。
    /// 父方案本身是**真实方案**（可选中），不是文件夹。
    pub parent: Option<String>,
    /// `schema/parent_label`：父方案"自身默认输入方式"的标签（例如「全拼」）。
    ///
    /// 2026-11-12 起第二级菜单不再显示父方案自身，本字段已不参与菜单构造；
    /// 保留解析与公开访问以兼容存量方案数据。
    pub parent_label: Option<String>,
}

impl Schema {
    /// 在 `build_dir` 下定位并加载方案。先试 rime 的扁平布局 `build/<id>.schema.yaml`，
    /// 再试系列子目录（`id` 中 `.` → `/`），最后退化为递归搜索。
    pub fn open(build_dir: &Path, schema_id: &str) -> Option<Schema> {
        let series_path = schema_id.replace('.', "/");
        let candidates = [
            // rime 真实布局：扁平 `build/<id>.schema.yaml`
            build_dir.join(format!("{schema_id}.schema.yaml")),
            // 系列子目录：`build/<系列>/<id>.schema.yaml`
            build_dir.join(&series_path).join(format!("{schema_id}.schema.yaml")),
            build_dir.join(schema_id).join(format!("{schema_id}.schema.yaml")),
        ];
        for path in candidates {
            if let Some(s) = Self::from_file(&path, schema_id) {
                return Some(s);
            }
        }
        // 退化：递归找第一个 `<schema_id>.schema.yaml`
        if let Some(found) = find_schema_file(build_dir, schema_id) {
            return Self::from_file(&found, schema_id);
        }
        None
    }

    fn from_file(path: &Path, schema_id: &str) -> Option<Schema> {
        let config = load_file(path).ok()?;
        let schema_name = config
            .pointer("/schema/name")
            .and_then(|v| v.as_str())
            .unwrap_or(schema_id)
            .to_string();
        let page_size = config
            .pointer("/menu/page_size")
            .and_then(|v| v.as_i64())
            .filter(|n| *n > 0)
            .map_or(5, |n| n as usize);
        let select_keys = config
            .pointer("/menu/alternative_select_keys")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let page_down_cycle = config
            .pointer("/menu/page_down_cycle")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let parent = config
            .pointer("/schema/parent")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        let parent_label = config
            .pointer("/schema/parent_label")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .map(str::to_string);
        Some(Schema {
            schema_id: schema_id.to_string(),
            schema_name,
            config,
            page_size,
            select_keys,
            page_down_cycle,
            parent,
            parent_label,
        })
    }

    /// 测试专用：空的方案对象（无配置树）。
    #[cfg(test)]
    pub fn empty_for_test() -> Schema {
        Schema {
            schema_id: String::new(),
            schema_name: String::new(),
            config: serde_json::Value::Object(serde_json::Map::new()),
            page_size: 5,
            select_keys: String::new(),
            page_down_cycle: false,
            parent: None,
            parent_label: None,
        }
    }

    /// 测试专用：给定配置树构造方案对象（给组件单测喂真实的 `switches` / `key_binder`）。
    #[cfg(test)]
    pub fn from_config_for_test(config: serde_json::Value) -> Schema {
        let page_size = config
            .pointer("/menu/page_size")
            .and_then(|v| v.as_i64())
            .unwrap_or(5) as usize;
        let parent = config
            .pointer("/schema/parent")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        let parent_label = config
            .pointer("/schema/parent_label")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .map(str::to_string);
        Schema {
            schema_id: String::new(),
            schema_name: String::new(),
            config,
            page_size,
            select_keys: String::new(),
            page_down_cycle: false,
            parent,
            parent_label,
        }
    }

    pub fn get_string(&self, path: &str) -> Option<String> {
        self.config.pointer(path).and_then(|v| v.as_str()).map(str::to_string)
    }

    /// `engine/translators` 列表（命名空间列表，如 `["translator", "history_translator"]`）。
    pub fn translator_namespaces(&self) -> Vec<String> {
        self.config
            .pointer("/engine/translators")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 第一个 translator 的 `dictionary` / `prism` 名（MVP 只接一个拼音/形码 translator）。
    ///
    /// librime 把 `script_translator` 的配置放在顶层 `translator:` 命名空间（不是
    /// `translator/script_translator/`），所以先逐个试每个 `engine/translators` 命名空间，
    /// 找不到 `dict` 时退回顶层 `/translator/`。正式键是单数 `dict`（2026-10-28 20:5x 起；
    /// **不再认** `dictionary`）。
    pub fn first_translator_dict(&self) -> Option<(String, String)> {
        // 先试每个命名空间，再退回顶层默认命名空间
        let mut namespaces = self.translator_namespaces();
        namespaces.push(String::new()); // 顶层 `translator:`
        for ns in &namespaces {
            let base = if ns.is_empty() {
                "translator".to_string()
            } else {
                format!("translator/{ns}")
            };
            if let Some(dict) = self.get_string(&format!("/{base}/dict")) {
                let prism = self
                    .get_string(&format!("/{base}/prism"))
                    .unwrap_or_else(|| dict.clone());
                return Some((dict, prism));
            }
        }
        None
    }

    /// schema 列表里点名的依赖方案（用于 "next 替换" 时列出可切换方案）。
    pub fn schema_list(&self) -> Vec<(String, String)> {
        self.config
            .pointer("/schema/list")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| {
                        let id = v.get("schema").and_then(|x| x.as_str())?;
                        let name = v.get("name").and_then(|x| x.as_str()).unwrap_or(id);
                        Some((id.to_string(), name.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn find_schema_file(dir: &Path, schema_id: &str) -> Option<PathBuf> {
    let target = format!("{schema_id}.schema.yaml");
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|n| n.to_str()) == Some(&target) {
                return Some(path);
            }
        }
    }
    None
}
