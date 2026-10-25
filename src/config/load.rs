//! YAML 文件 → 树（`config_types.cc` 的 `ConvertFromYaml` 前半段：**读**）。
//!
//! 这是 `config` 层的职责，不是 crate 的职责：Rime 的 YAML 有两条私有约定，
//! 只有配置层知道怎么处理。`lib.rs` 为此只保留一层 `pub use` 兼容旧路径
//! （`librime_next::load_file`）。
//!
//! ## 两条 Rime 私有约定（都是对拍 30 份基线时实测撞出来的）
//!
//! 1. **块标量** `key: |` + 多行（`description` 大量使用）—— 交给 `serde_yaml`；
//! 2. **词典的 `...` 约定**：`...` 之后直接跟词条表，没有 `---`，所以那一段**不是
//!    YAML**，必须切开单独处理（`stroke.dict.yaml` 否则报
//!    `did not find expected <document start>`）。
//!
//! ## 与 C++ 的差异
//!
//! C++ 用 yaml-cpp 直接往 `ConfigItem` 上转换；这里先转成 `serde_json::Value`
//! （保住多文档、保住类型），再由 [`super::data::convert_from_yaml`] 转成树。
//! 中间这层 `Value` 同时是 `main.rs` 的 `diff` / `roundtrip` 与 `algo` 层
//! `Projection::from_config` 的输入类型。
//!
//! **键序在这条路上不能丢**：`serde_json` 在本 crate 开了 `preserve_order`
//! （见 `Cargo.toml`），`Value::Object` 的后备映射是 `IndexMap`，所以
//! `serde_yaml::Value` → `serde_json::Value` → `ConfigItem` 全程保持源文件里的键序，
//! 最后由 [`super::emit`] 按同一顺序发射。用默认的 `BTreeMap` 会把键按字节序重排，
//! 生成物与手写源对不上（这就是"键序问题"的根因）。
//!
//! 词条表只统计行数（`__entries`，够用于"识别词典"）；真要重写词典是 `dict/`
//! 那一层的事。

use std::path::Path;

/// 读一份 YAML 成树。
///
/// 返回值：
/// * 单文档 → 该文档（若是映射且文件有 `...` 词条表，额外插入 `__entries`: 行数）；
/// * 无文档 → `Value::Null`；
/// * 多文档 → `{"__documents": [...]}`（不丢信息）。
pub fn load_file(path: &Path) -> Result<serde_json::Value, String> {
    use serde::Deserialize as _;
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;

    // 按 `...` 切成"YAML 头"与"词条表"
    let mut header = text.as_str();
    let mut entries: Option<&str> = None;
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        if line.trim_end() == "..." {
            header = &text[..offset];
            entries = Some(&text[offset + line.len()..]);
            break;
        }
        offset += line.len();
    }

    let mut docs: Vec<serde_json::Value> = Vec::new();
    for document in serde_norway::Deserializer::from_str(header) {
        let value = serde_norway::Value::deserialize(document)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        docs.push(serde_json::to_value(&value).map_err(|e| format!("{}: {e}", path.display()))?);
    }
    match docs.len() {
        0 => Ok(serde_json::Value::Null),
        1 => {
            // 长度已判为 1，remove(0) 恒成功（不放 `expect`：这里本来就没有失败条件）
            let mut doc = docs.remove(0);
            if let Some(body) = entries {
                let rows = body.lines().filter(|line| !line.trim().is_empty()).count();
                if rows > 0
                    && let Some(map) = doc.as_object_mut()
                {
                    map.insert("__entries".into(), serde_json::json!(rows));
                }
            }
            Ok(doc)
        }
        _ => Ok(serde_json::json!({ "__documents": docs })),
    }
}
