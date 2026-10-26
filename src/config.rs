//! librime `src/rime/config/`（17 个文件 2,516 行）的 Rust 移植 —— 门面。
//!
//! | C++ 文件 | Rust 文件 |
//! |---|---|
//! | `config_types.{h,cc}` | [`types`] |
//! | `config_data.{h,cc}` + `config_cow_ref.h` | [`data`] |
//! | `config_compiler.{h,cc}` + `config_compiler_impl.h` | [`compiler`] |
//! | `plugins.h` + 5 个 `*_plugin.cc` + `config_component.cc` 的 `ConfigBuilder` | [`plugins`] |
//! | `config_data.cc` 的 `EmitYaml`（**全 crate 唯一发射出口**） | [`emit`] |
//! | （Rime 私有约定）YAML 读入 | [`load`] |
//! | `rime/resource.{h,cc}` 里 config 用到的子集 | [`resource`] |
//!
//! ## 编译一份 schema 到底发生了什么
//!
//! `compile()` 完全复刻 `ConfigBuilder::LoadConfig` + `core_module.cc` 的插件顺序：
//!
//! ```text
//! Compile(<方案>.schema)            ← 读源文件；__include/__patch 记成依赖，不进树
//!   ↓ AutoPatch（Compile 阶段）      ← 根节点挂 __patch: <方案>.custom:/patch?（optional）
//! Link: ResolveDependencies("<id>:")
//!   ↓ Default                      ← .schema：从 default 补 menu/navigator/selector（optional）
//!   ↓ LegacyPreset                 ← punctuator/recognizer/key_binder 的 import_preset
//!   ↓ LegacyDictionary             ← 空实现
//!   ↓ BuildInfo                    ← __build_info = rime_version + 各资源 mtime
//! SaveOutput（库不装；CLI 的 compile 自己写输出文件）
//! ```
//!
//! 关键语义（详见各子模块）：
//!
//! * 映射递归合并、列表整体替换、标量覆盖；
//! * `__patch` 目标是列表时**追加**；
//! * `EditNode`：`key/+` 追加（字符串拼接 / 列表追加）、`key/=` 覆盖、
//!   `key/3+` 索引插入、`__merge` 合并、`__append` 追加；
//! * 循环依赖检测在 `ResolveDependencies` 里（`resolve_chain` 前缀匹配）。
//!
//! ## 与 C++ 的有意差异
//!
//! * **键序 = 源文件顺序**（C++ 是 `std::map`，产物按字节序排列）。树里的映射是
//!   `IndexMap`，读入按源文件顺序插入、发射按同一顺序吐出；合并时**已有键保持原位、
//!   新键追加尾部**（规则写在 [`compiler::merge_tree`]，[`types`] 里也复述了一遍）。
//!   这是为了让生成物和手写源逐键对得上；`diff` 比的是树（与键序无关），所以对拍不受影响。
//! * YAML **读**走 [`load::load_file`]（`serde_yaml`，已处理块标量和 Rime 词典的
//!   `...` 约定），**写**只走 [`emit::to_yaml_string`]（yaml-cpp `EmitScalar` 规则的
//!   手写移植：裸写 / 双引号 / literal block）。标量仍按 C++ 的模型**一律存字符串**，
//!   保证重新解析后的类型与 librime 产物一致。`data::ConfigData::save*` / CLI 的
//!   `compile` / `emit` 都调这一个出口，不许再另起炉灶。
//! * `ConfigData` 里 `file_path` / `modified` / `auto_save` 保留；`LoadFromStream` /
//!   `SaveToStream`（std::istream/ostream）没有对应物，用 `load_from_file` /
//!   `save_to_string`（返回 `Result<_, EmitError>`）代替。
//! * `ConfigComponentBase` / `ConfigComponent`（按 config id 缓存 `weak<ConfigData>`）、
//!   `ConfigLoader`、三个 `*ResourceProvider` 没有移植；`Config` / `ConfigBuilder` /
//!   `MultiplePlugins` 已移植。命令行按文件编译，接缝是文件而不是组件实例。
//! * [`resource`] 只是 `rime/resource.cc` 的子集（config 用到的三个方法 + Weasel-Next
//!   的分目录补丁），完整的 `Resource` 那层还没移植。

// 可见性理由（2026-10-23）：这里是 C++ `rime/config/` 的逐文件移植，一部分入口
// （`Config::set_*`、`ConfigData::save*`、`ResourceResolver::roots`、
// `ConfigCompiler::pending` 等）当前**没有 crate 内调用者**，按"无引用即私有"的
// 规则已降为私有/`pub(crate)`。降级之后它们自然不会再被用到，`dead_code` 必然响；
// `algo` 各模块本来就在文件头统一 `#![allow(dead_code)]`，这里保持一致，
// 不为了让 lint 安静而把它们提回 `pub`。
#![allow(dead_code)]

use std::path::{Path, PathBuf};

mod compiler;
pub mod data;
mod emit;
pub(super) mod load;
mod plugins;
mod resource;
mod types;

pub use compiler::{
    APPEND_DIRECTIVE, ConfigCompiler, ConfigDependencyGraph, ConfigResource, Dependency,
    DependencyPriority, INCLUDE_DIRECTIVE, IncludeReference, MERGE_DIRECTIVE, PATCH_DIRECTIVE,
    PatchLiteral, PatchReference, PendingChild, Reference,
};
pub use data::{
    Config, ConfigData, convert_from_yaml, cow, strip_build_info, strip_build_info_json,
};
pub use emit::{EmitError, to_yaml_string};
pub use plugins::{
    AutoPatchConfigPlugin, BuildInfoPlugin, ConfigBuilder, ConfigCompilerPlugin,
    DefaultConfigPlugin, LegacyDictionaryConfigPlugin, LegacyPresetConfigPlugin, MultiplePlugins,
    RIME_VERSION, SaveOutputPlugin,
};
pub use resource::ResourceResolver;
pub use types::{Item, ItemRef, ListEntryRef, MapEntryRef, ValueType};

/// 从源 schema 编译出产物（`ConfigBuilder::LoadConfig` 的等价物）。
///
/// * `schema_path` —— 源 schema 文件（`.../<方案>/<方案>.schema.yaml`）；
/// * `defaults_path` —— `default.yaml`；它被钉成资源 `default`，其余资源按
///   librime 的根目录规则在 `schema_path` 所在的数据根 / `default.yaml` 所在目录里找；
/// * `search_dirs` —— 额外搜索根（CLI 传源目录与 `default.yaml` 目录，兼容旧行为）。
///
/// 返回值是编译后的树（含 `__build_info`；对拍前用 [`strip_build_info`] 剔掉）。
pub fn compile(
    schema_path: &Path,
    defaults_path: &Path,
    search_dirs: &[PathBuf],
) -> Result<Item, String> {
    let file_name = schema_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("无效的 schema 路径: {}", schema_path.display()))?;
    let schema_id = file_name
        .strip_suffix(".yaml")
        .unwrap_or(file_name)
        .to_string();

    // roots 顺序 = librime 的 [user_data_dir, shared_data_dir]；再补 CLI 的搜索目录。
    //
    // 数据根的推导：源文件在 `<数据根>/schema/…` 下时，数据根 = 那一级 `schema`
    // 目录的父目录；源文件在数据根本身（`default.yaml` 等根级配置）时，
    // 数据根 = 源文件所在目录。**不能**无条件跳 grandparent——那会把根级源文件的
    // 根推到数据根的上一级（真机 2026-10-26：symbols.yaml 因此被找到
    // `%APPDATA%\symbols.yaml`，少了 Rime-Next 一级）。
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(parent) = schema_path.parent() {
        let mut root = parent.to_path_buf();
        let mut cursor = Some(parent);
        while let Some(dir) = cursor {
            if dir.file_name().map_or(false, |name| name == "schema") {
                if let Some(data_root) = dir.parent() {
                    root = data_root.to_path_buf();
                }
                break;
            }
            cursor = dir.parent();
        }
        roots.push(root);
    }
    if let Some(parent) = defaults_path.parent() {
        roots.push(parent.to_path_buf());
    }
    if let Some(parent) = schema_path.parent() {
        roots.push(parent.to_path_buf());
    }
    roots.extend(search_dirs.iter().cloned());
    let mut deduped: Vec<PathBuf> = Vec::new();
    for root in roots {
        if !deduped.contains(&root) {
            deduped.push(root);
        }
    }

    let mut resolver = ResourceResolver::new(deduped);
    // 用户明确给定的两份文件必须就是被编译的那两份
    resolver.add_override(schema_id.clone(), schema_path.to_path_buf());
    resolver.add_override("default", defaults_path.to_path_buf());

    let data = ConfigBuilder::with_core_plugins().load_config(resolver, &schema_id);
    let root = data.root.borrow().clone();
    if root.is_null() {
        return Err(format!("读取 schema 失败: {}", schema_path.display()));
    }
    Ok(root)
}
