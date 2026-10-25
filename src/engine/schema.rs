//! `src/rime/schema.{h,cc}`：方案（包装编译后的 Config 树）。
//!
//! 运行时只读：引擎从方案里读 `engine/processors`、`engine/translators`、以及
//! `translator/<ns>` 下的 `dictionary` / `prism` / `packs` 等，决定加载哪个词典、怎么组字。

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
        Some(Schema {
            schema_id: schema_id.to_string(),
            schema_name,
            config,
            page_size,
            select_keys,
        })
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
    /// 找不到 `dictionary` 时退回顶层 `/translator/`。
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
            if let Some(dict) = self.get_string(&format!("/{base}/dictionary")) {
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
