//! `librime/src/rime/config/` 下 5 个插件的逐个移植
//! （`plugins.h` + `*_config_plugin.cc` + `build_info_plugin.cc` + `save_output_plugin.cc`），
//! 外加 `config_component.cc` 里的 `ConfigBuilder` / `MultiplePlugins` 插件流水线。
//!
//! 插件顺序以 `core_module.cc` 为准：
//! `AutoPatch → Default → LegacyPreset → LegacyDictionary → BuildInfo → SaveOutput`。
//! 语义以 C++ 为准，注释里标了对应行号。

use std::path::PathBuf;
use std::rc::Rc;

use super::compiler::{
    Dependency, DependencyPriority, IncludeReference, PatchReference, Reference,
};
use super::data::{ConfigData, cow, log_error, log_info, traverse_copy_on_write};
use super::resource::ResourceResolver;
use super::types::{Item, ItemRef, MapEntryRef};

/// librime 的 `RIME_VERSION`（`librime/CMakeLists.txt` 里 `rime_version`）。
/// 夹具里 `__build_info.rime_version` 实测就是 `1.17.0`。
pub const RIME_VERSION: &str = "1.17.0";

use super::compiler::{ConfigCompiler, ConfigResource};

/// `ConfigCompilerPlugin`。
pub trait ConfigCompilerPlugin {
    fn review_compile_output(
        &self,
        compiler: &mut ConfigCompiler,
        resource: &Rc<ConfigResource>,
    ) -> bool;

    fn review_link_output(
        &self,
        compiler: &mut ConfigCompiler,
        resource: &Rc<ConfigResource>,
    ) -> bool;
}

fn resource_ref(resource: &Rc<ConfigResource>) -> Rc<dyn ItemRef> {
    resource.clone()
}

fn map_entry(parent: &Rc<dyn ItemRef>, key: &str) -> Rc<dyn ItemRef> {
    Rc::new(MapEntryRef::new(parent.as_map(), key))
}

fn remove_suffix<'a>(input: &'a str, suffix: &str) -> &'a str {
    input.strip_suffix(suffix).unwrap_or(input)
}

// ---------------------------------------------------------------------------
// auto_patch_config_plugin.cc
// ---------------------------------------------------------------------------

/// 自动在根节点挂 `__patch: <方案>.custom:/patch?`。
pub struct AutoPatchConfigPlugin;

impl ConfigCompilerPlugin for AutoPatchConfigPlugin {
    // auto-patch 作用于所有被加载的资源（含依赖），所以在 Compile 阶段末尾做。
    fn review_compile_output(
        &self,
        compiler: &mut ConfigCompiler,
        resource: &Rc<ConfigResource>,
    ) -> bool {
        if resource.resource_id.ends_with(".custom") {
            return true;
        }
        // 根节点已经有显式 __patch 时跳过
        let root_deps = compiler.get_dependencies(&format!("{}:", resource.resource_id));
        if root_deps.last().map_or(false, |dependency| {
            dependency.priority() >= DependencyPriority::Patch
        }) {
            return true;
        }
        let patch_resource_id =
            format!("{}.custom", remove_suffix(&resource.resource_id, ".schema"));
        log_info(&format!(
            "auto-patch {}:/__patch: {}:/patch?",
            resource.resource_id, patch_resource_id
        ));
        compiler.push_resource(resource.clone());
        compiler.add_dependency(Rc::new(PatchReference::new(Reference::new(
            patch_resource_id,
            "patch",
            true,
        ))));
        compiler.pop();
        true
    }

    fn review_link_output(
        &self,
        _compiler: &mut ConfigCompiler,
        _resource: &Rc<ConfigResource>,
    ) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// default_config_plugin.cc
// ---------------------------------------------------------------------------

/// 对 `.schema` 资源，从 `default` 补 `menu` / `navigator` / `selector`（optional）。
pub struct DefaultConfigPlugin;

impl ConfigCompilerPlugin for DefaultConfigPlugin {
    fn review_compile_output(
        &self,
        _compiler: &mut ConfigCompiler,
        _resource: &Rc<ConfigResource>,
    ) -> bool {
        true
    }

    fn review_link_output(
        &self,
        compiler: &mut ConfigCompiler,
        resource: &Rc<ConfigResource>,
    ) -> bool {
        if !resource.resource_id.ends_with(".schema") {
            return true;
        }
        let head = resource_ref(resource);
        for section in ["menu", "navigator", "selector"] {
            let target = cow(head.clone(), section);
            let reference = Reference::new("default", section, true);
            if !IncludeReference::new(reference)
                .targeted_at(target)
                .resolve(compiler)
            {
                log_error(&format!("failed to include section default:{section}"));
                return false;
            }
        }
        true
    }
}

// ---------------------------------------------------------------------------
// legacy_preset_config_plugin.cc
// ---------------------------------------------------------------------------

/// `punctuator` / `recognizer` / `key_binder` 的 `import_preset`。
///
/// `key_binder/bindings` 是**追加**：先把自身 bindings 挪到 `bindings/+`，
/// 再把 `bindings` 置空，这样 include 的预设同名节里的 bindings 不会被覆盖。
/// `import_preset` 键本身保留在产物里（它不参与合并，只留在 overrides 里）。
pub struct LegacyPresetConfigPlugin;

impl LegacyPresetConfigPlugin {
    fn include_preset(
        compiler: &mut ConfigCompiler,
        resource: &Rc<ConfigResource>,
        section: &str,
        preset_config_id: &str,
        target: Rc<dyn ItemRef>,
    ) -> bool {
        let reference = Reference::new(preset_config_id, section, false);
        if !IncludeReference::new(reference)
            .targeted_at(target)
            .resolve(compiler)
        {
            log_error(&format!(
                "failed to include section {preset_config_id}:{section}"
            ));
            let _ = resource;
            return false;
        }
        true
    }
}

impl ConfigCompilerPlugin for LegacyPresetConfigPlugin {
    fn review_compile_output(
        &self,
        _compiler: &mut ConfigCompiler,
        _resource: &Rc<ConfigResource>,
    ) -> bool {
        true
    }

    fn review_link_output(
        &self,
        compiler: &mut ConfigCompiler,
        resource: &Rc<ConfigResource>,
    ) -> bool {
        if !resource.resource_id.ends_with(".schema") {
            return true;
        }
        let head = resource_ref(resource);
        // key_binder：bindings 追加，其余合并
        let preset = resource.data.traverse("key_binder/import_preset");
        if !preset.is_null() {
            let Item::Scalar(preset_config_id) = preset else {
                return false;
            };
            log_info(&format!(
                "interpreting key_binder/import_preset: {preset_config_id}"
            ));
            let target = cow(head.clone(), "key_binder");
            let target_item = target.get_item();
            if let Some(map) = target_item.as_map() {
                if map
                    .borrow()
                    .get("bindings")
                    .map_or(false, |value| !value.is_null())
                {
                    // 改成 `bindings/+` 追加，而不是覆盖被 include 进来的列表
                    let appended = map.borrow().get("bindings").cloned().unwrap_or(Item::Null);
                    cow(target.clone(), "bindings/+").set_item(appended);
                    // `*target` 已经指向拷贝后的 map，可以直接改
                    let target_item = target.get_item();
                    if let Some(map) = target_item.as_map() {
                        map.borrow_mut().insert("bindings".to_string(), Item::Null);
                    }
                }
            }
            if !Self::include_preset(compiler, resource, "key_binder", &preset_config_id, target) {
                return false;
            }
        }
        // punctuator
        let preset = resource.data.traverse("punctuator/import_preset");
        if !preset.is_null() {
            let Item::Scalar(preset_config_id) = preset else {
                return false;
            };
            log_info(&format!(
                "interpreting punctuator/import_preset: {preset_config_id}"
            ));
            let target = cow(head.clone(), "punctuator");
            if !Self::include_preset(compiler, resource, "punctuator", &preset_config_id, target) {
                return false;
            }
        }
        // recognizer
        let preset = resource.data.traverse("recognizer/import_preset");
        if !preset.is_null() {
            let Item::Scalar(preset_config_id) = preset else {
                return false;
            };
            log_info(&format!(
                "interpreting recognizer/import_preset: {preset_config_id}"
            ));
            let target = cow(head, "recognizer");
            if !Self::include_preset(compiler, resource, "recognizer", &preset_config_id, target) {
                return false;
            }
        }
        true
    }
}

// ---------------------------------------------------------------------------
// rime_next_dialect_plugin.rs —— Rime-Next 方言扩展（2026-10-27）
// ---------------------------------------------------------------------------

/// **通用 `import_preset`**：方言允许任意节点写
///
/// ```yaml
/// speller:
///   algebra:
///     import_preset: pinyin.parser
/// ```
///
/// 语义：`<路径>/import_preset: <配置id>` —— 用配置 `<id>` 里**同路径**的节点
/// 整体替换当前节点（对应 legacy 三处那种"从预设文件抄同名节"的行为，只是
/// 不再限定 key_binder / punctuator / recognizer 三处）。
///
/// 只接管"**纯引用节点**"（映射里只有 `import_preset` 一个键）：
/// * 不会碰 legacy 三处（它们可能带额外键、bindings 有追加语义，仍走
///   [`LegacyPresetConfigPlugin`]）；
/// * 也不会碰"引用 + 局部覆盖"的混合写法（那种语义未定，先报错不静默）。
pub struct GeneralImportPresetPlugin;

impl GeneralImportPresetPlugin {
    /// 收集"纯引用节点"的路径。路径以 `/` 连接（`cow`/`traverse` 的同一风格）。
    fn collect(node: &Item, path: &str, out: &mut Vec<(String, String)>) {
        if let Some(map) = node.as_map() {
            let borrowed = map.borrow();
            // 纯引用节点：只有一个键且叫 import_preset
            if borrowed.len() == 1
                && let Some(Item::Scalar(id)) = borrowed.get("import_preset")
            {
                let first = path.split('/').next().unwrap_or("");
                // legacy 三处仍由 LegacyPresetConfigPlugin 处理（含 bindings 追加语义）
                if !matches!(first, "key_binder" | "punctuator" | "recognizer") {
                    out.push((path.to_string(), id.clone()));
                }
                return;
            }
            for (key, child) in borrowed.iter() {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}/{key}")
                };
                Self::collect(child, &child_path, out);
            }
        } else if let Some(list) = node.as_list() {
            for (index, child) in list.borrow().iter().enumerate() {
                let child_path = format!("{path}/{index}");
                Self::collect(child, &child_path, out);
            }
        }
    }
}

impl ConfigCompilerPlugin for GeneralImportPresetPlugin {
    fn review_compile_output(
        &self,
        _compiler: &mut ConfigCompiler,
        _resource: &Rc<ConfigResource>,
    ) -> bool {
        true
    }

    fn review_link_output(
        &self,
        compiler: &mut ConfigCompiler,
        resource: &Rc<ConfigResource>,
    ) -> bool {
        if !resource.resource_id.ends_with(".schema") {
            return true;
        }
        let head = resource_ref(resource);
        let mut targets = Vec::new();
        let root_item = head.get_item();
        Self::collect(&root_item, "", &mut targets);
        for (path, id) in targets {
            log_info(&format!("interpreting {path}/import_preset: {id}"));
            // ⚠️ cow() 是**单层键**；这里是多层路径，必须用 traverse_copy_on_write
            let Some(target) = traverse_copy_on_write(head.clone(), &path) else {
                log_error(&format!("cannot traverse to {path} for import_preset"));
                return false;
            };
            // 先清空"纯引用节点"，让 include 进来的同路径节点整体落位
            target.set_item(Item::Null);
            if !LegacyPresetConfigPlugin::include_preset(
                compiler,
                resource,
                &path,
                &id,
                target.clone(),
            ) {
                log_error(&format!("failed to import preset {id} at {path}"));
                return false;
            }
            match target.get_item() {
                Item::List(_) => log_info(&format!("import {path}: list ok")),
                Item::Map(_) => log_info(&format!("import {path}: map ok")),
                Item::Scalar(v) => log_info(&format!("import {path}: scalar {v:?}")),
                Item::Null => log_info(&format!("import {path}: NULL (!!)")),
            }
        }
        true
    }
}

// ---------------------------------------------------------------------------
// legacy_dictionary_config_plugin.cc
// ---------------------------------------------------------------------------

/// C++ 里两个 review 都是 `// TODO: unimplemented`，照抄。
pub struct LegacyDictionaryConfigPlugin;

impl ConfigCompilerPlugin for LegacyDictionaryConfigPlugin {
    fn review_compile_output(
        &self,
        _compiler: &mut ConfigCompiler,
        _resource: &Rc<ConfigResource>,
    ) -> bool {
        true
    }

    fn review_link_output(
        &self,
        _compiler: &mut ConfigCompiler,
        _resource: &Rc<ConfigResource>,
    ) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// build_info_plugin.cc
// ---------------------------------------------------------------------------

/// 写 `__build_info`：`rime_version` + `timestamps{资源id: 源文件 mtime 秒}`。
/// 没加载的资源、没有持久化路径的资源都记 0。
pub struct BuildInfoPlugin;

fn resource_timestamp(resource: &ConfigResource) -> i64 {
    if !resource.loaded.get() {
        log_info(&format!("resource '{}' not loaded.", resource.resource_id));
        return 0;
    }
    let file_path = resource.data.file_path();
    if file_path.as_os_str().is_empty() {
        log_info(&format!(
            "resource '{}' is not persisted.",
            resource.resource_id
        ));
        return 0;
    }
    match std::fs::metadata(&file_path).and_then(|metadata| metadata.modified()) {
        Ok(time) => time
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0),
        Err(_) => 0,
    }
}

impl ConfigCompilerPlugin for BuildInfoPlugin {
    fn review_compile_output(
        &self,
        _compiler: &mut ConfigCompiler,
        _resource: &Rc<ConfigResource>,
    ) -> bool {
        true
    }

    fn review_link_output(
        &self,
        compiler: &mut ConfigCompiler,
        resource: &Rc<ConfigResource>,
    ) -> bool {
        let head = resource_ref(resource);
        let build_info = map_entry(&head, "__build_info");
        map_entry(&build_info, "rime_version").set_item(Item::scalar(RIME_VERSION));
        let timestamps = map_entry(&build_info, "timestamps");
        let timestamps_map = timestamps.as_map();
        let mut rows: Vec<(String, i64)> = Vec::new();
        compiler.enumerate_resources(|resource| {
            rows.push((resource.resource_id.clone(), resource_timestamp(resource)));
        });
        for (resource_id, timestamp) in rows {
            Rc::new(MapEntryRef::new(timestamps_map.clone(), resource_id))
                .set_item(Item::scalar(timestamp.to_string()));
        }
        true
    }
}

// ---------------------------------------------------------------------------
// save_output_plugin.cc
// ---------------------------------------------------------------------------

/// 把编译结果写到 staging 根（Weasel-Next 里是 `build/`）。
///
/// C++ 构造时从 `Service` 拿 staging resolver；这里显式传根目录。CLI 的 `compile`
/// 命令**不安装**这个插件（它自己写用户指定的输出文件），库的 `ConfigBuilder`
/// 默认也不装。
pub struct SaveOutputPlugin {
    resolver: ResourceResolver,
}

impl SaveOutputPlugin {
    fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            resolver: ResourceResolver::new(vec![root.into()]),
        }
    }
}

impl ConfigCompilerPlugin for SaveOutputPlugin {
    fn review_compile_output(
        &self,
        _compiler: &mut ConfigCompiler,
        _resource: &Rc<ConfigResource>,
    ) -> bool {
        true
    }

    fn review_link_output(
        &self,
        _compiler: &mut ConfigCompiler,
        resource: &Rc<ConfigResource>,
    ) -> bool {
        let file_path = self.resolver.resolve_path(&resource.resource_id);
        // [Weasel-Next 补丁] 父目录可能还不存在，先建出来。
        if let Some(parent) = file_path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        resource.data.save_to_file(&file_path)
    }
}

// ---------------------------------------------------------------------------
// config_component.cc：MultiplePlugins / ConfigBuilder
// ---------------------------------------------------------------------------

/// `MultiplePlugins`：按安装顺序逐个 review，一个失败就停。
pub struct MultiplePlugins {
    plugins: Vec<Rc<dyn ConfigCompilerPlugin>>,
}

impl ConfigCompilerPlugin for MultiplePlugins {
    fn review_compile_output(
        &self,
        compiler: &mut ConfigCompiler,
        resource: &Rc<ConfigResource>,
    ) -> bool {
        for plugin in &self.plugins {
            if !plugin.review_compile_output(compiler, resource) {
                return false;
            }
        }
        true
    }

    fn review_link_output(
        &self,
        compiler: &mut ConfigCompiler,
        resource: &Rc<ConfigResource>,
    ) -> bool {
        for plugin in &self.plugins {
            if !plugin.review_link_output(compiler, resource) {
                return false;
            }
        }
        true
    }
}

/// `ConfigBuilder`：装好插件，`Compile` + `Link`，返回编译后的 `ConfigData`。
pub struct ConfigBuilder {
    plugins: Vec<Rc<dyn ConfigCompilerPlugin>>,
}

impl Default for ConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigBuilder {
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
        }
    }

    /// 与 `core_module.cc` 一致的插件集合（不含 SaveOutput：那是部署动作）。
    pub(super) fn with_core_plugins() -> Self {
        let mut builder = Self::new();
        builder.install_plugin(Rc::new(AutoPatchConfigPlugin));
        builder.install_plugin(Rc::new(DefaultConfigPlugin));
        builder.install_plugin(Rc::new(LegacyPresetConfigPlugin));
        builder.install_plugin(Rc::new(GeneralImportPresetPlugin));
        builder.install_plugin(Rc::new(LegacyDictionaryConfigPlugin));
        builder.install_plugin(Rc::new(BuildInfoPlugin));
        builder
    }

    fn install_plugin(&mut self, plugin: Rc<dyn ConfigCompilerPlugin>) {
        self.plugins.push(plugin);
    }

    /// `ConfigBuilder::LoadConfig`。编译/链接失败时仍返回 data（C++ 只记日志）。
    pub fn load_config(&self, resolver: ResourceResolver, config_id: &str) -> Rc<ConfigData> {
        let multiple: Rc<dyn ConfigCompilerPlugin> = Rc::new(MultiplePlugins {
            plugins: self.plugins.clone(),
        });
        let mut compiler = ConfigCompiler::new(resolver, vec![multiple]);
        let resource = compiler.compile(config_id);
        if resource.loaded.get() && !compiler.link(resource.clone()) {
            log_error(&format!("error building config: {config_id}"));
        }
        resource.data.clone()
    }
}
