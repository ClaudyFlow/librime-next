//! `librime/src/rime/config/config_compiler.{h,cc}` +
//! `config_compiler_impl.h` 的移植。
//!
//! 这是整个 config 层的核心：依赖图、`__include` / `__patch` 的解析与解析时机、
//! 合并/追加/索引编辑算子、循环依赖检测。
//!
//! ## 边界
//!
//! 资源路径解析（`ToResourceId` / `ToFilePath` / `ResolvePath`）**不在这里**：
//! 它在 C++ 属于 `rime/resource.cc`，已切到 [`super::resource`]。
//!
//! ## 关键不变量（照 C++ 抄，别"优化"）
//!
//! * **指令不存进树**：`ConvertFromYaml` 里 `Parse()` 返回 true 的键被丢弃，
//!   依赖记在"包含该指令的那个节点"（`node_stack.back()`）上。
//! * **依赖按优先级排序**：`PendingChild(0) < Include(1) < Patch(2)`；同一目标节点
//!   先把子节点的 pending 依赖解决掉，再应用 include / patch。
//! * **blocking 依赖**：只要某路径的依赖列表里还有 include/patch，
//!   访问该路径前必须先 `ResolveDependencies`（`ResolveBlockingDependencies`）。
//! * `resolve_chain` 只在成功时 pop；失败时留着，于是后续同前缀路径会被判成环。
//! * `PendingChild` 会沿祖先链冒泡，让父节点也知道"还有个孩子没解决"。

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use super::data::{
    ConfigData, log_error, log_info, log_warn, traverse_copy_on_write, type_checked_copy_on_write,
};
use super::plugins::ConfigCompilerPlugin;
use super::resource::ResourceResolver;
use super::types::{Item, ItemRef, ListEntryRef, ListRef, MapEntryRef, MapRef};

pub const INCLUDE_DIRECTIVE: &str = "__include";
pub const PATCH_DIRECTIVE: &str = "__patch";
pub const APPEND_DIRECTIVE: &str = "__append";
pub const MERGE_DIRECTIVE: &str = "__merge";

/// `Reference`：`resource_id:local_path[?]`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    resource_id: String,
    local_path: String,
    optional: bool,
}

impl Reference {
    pub(super) fn new(
        resource_id: impl Into<String>,
        local_path: impl Into<String>,
        optional: bool,
    ) -> Self {
        Self {
            resource_id: resource_id.into(),
            local_path: local_path.into(),
            optional,
        }
    }

    fn repr(&self) -> String {
        format!(
            "{}:{}{}",
            self.resource_id,
            self.local_path,
            if self.optional { " <optional>" } else { "" }
        )
    }
}

/// `ConfigResource`：一个被编译过的资源文件。
pub struct ConfigResource {
    pub(super) resource_id: String,
    pub(super) data: Rc<ConfigData>,
    pub(super) loaded: Cell<bool>,
}

impl ConfigResource {
    fn new(resource_id: impl Into<String>) -> Self {
        Self {
            resource_id: resource_id.into(),
            data: Rc::new(ConfigData::new()),
            loaded: Cell::new(false),
        }
    }
}

impl ItemRef for ConfigResource {
    fn get_item(&self) -> Item {
        self.data.root.borrow().clone()
    }

    fn set_item(&self, item: Item) {
        *self.data.root.borrow_mut() = item;
    }
}

// ---------------------------------------------------------------------------
// 依赖
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum DependencyPriority {
    PendingChild = 0,
    Include = 1,
    Patch = 2,
}

pub trait Dependency {
    fn priority(&self) -> DependencyPriority;
    fn repr(&self) -> String;
    fn target(&self) -> Option<Rc<dyn ItemRef>>;
    fn set_target(&self, target: Rc<dyn ItemRef>);
    fn resolve(&self, compiler: &mut ConfigCompiler) -> bool;

    fn blocking(&self) -> bool {
        self.priority() > DependencyPriority::PendingChild
    }
}

/// 子节点还有未解决的依赖；沿祖先链传播用。
pub struct PendingChild {
    child_path: String,
    child_ref: Rc<dyn ItemRef>,
    target: RefCell<Option<Rc<dyn ItemRef>>>,
}

impl PendingChild {
    fn new(child_path: impl Into<String>, child_ref: Rc<dyn ItemRef>) -> Self {
        Self {
            child_path: child_path.into(),
            child_ref,
            target: RefCell::new(None),
        }
    }
}

impl Dependency for PendingChild {
    fn priority(&self) -> DependencyPriority {
        DependencyPriority::PendingChild
    }

    fn repr(&self) -> String {
        format!("PendingChild({})", self.child_path)
    }

    fn target(&self) -> Option<Rc<dyn ItemRef>> {
        self.target.borrow().clone()
    }

    fn set_target(&self, target: Rc<dyn ItemRef>) {
        *self.target.borrow_mut() = Some(target);
    }

    fn resolve(&self, compiler: &mut ConfigCompiler) -> bool {
        compiler.resolve_dependencies(&self.child_path)
    }
}

/// `__include: resource:/path`。
pub struct IncludeReference {
    reference: Reference,
    target: RefCell<Option<Rc<dyn ItemRef>>>,
}

impl IncludeReference {
    pub(super) fn new(reference: Reference) -> Self {
        Self {
            reference,
            target: RefCell::new(None),
        }
    }

    pub(super) fn targeted_at(self, target: Rc<dyn ItemRef>) -> Self {
        self.set_target(target);
        self
    }
}

impl Dependency for IncludeReference {
    fn priority(&self) -> DependencyPriority {
        DependencyPriority::Include
    }

    fn repr(&self) -> String {
        format!("Include({})", self.reference.repr())
    }

    fn target(&self) -> Option<Rc<dyn ItemRef>> {
        self.target.borrow().clone()
    }

    fn set_target(&self, target: Rc<dyn ItemRef>) {
        *self.target.borrow_mut() = Some(target);
    }

    fn resolve(&self, compiler: &mut ConfigCompiler) -> bool {
        log_info(&format!(
            "IncludeReference::Resolve(reference = {:?})",
            self.reference
        ));
        let Some(target) = self.target() else {
            return false;
        };
        let included = resolve_reference(compiler, &self.reference);
        if included.is_null() {
            return self.reference.optional;
        }
        // 把本节点里除指令以外的键当作 overrides，合并到被包含的节点上。
        //
        // 键序（见 [`merge_tree`]）：先是被包含文件里那一串键的顺序，overrides 里
        // **已有的键留在原位**、**新键追加到尾部**。这正是"源文件写了什么就先看见什么"
        // 的顺序，也是 `yuling` 这类方案编译产物与手写源键序对得上的原因。
        let overrides = target.get_item().as_map();
        target.set_item(included);
        if let Some(overrides) = overrides {
            if !overrides.borrow().is_empty() && !merge_tree(&target, overrides) {
                log_error(&format!("failed to merge tree: {}", self.reference.repr()));
                return false;
            }
        }
        true
    }
}

/// `__patch: resource:/path`。
pub struct PatchReference {
    reference: Reference,
    target: RefCell<Option<Rc<dyn ItemRef>>>,
}

impl PatchReference {
    pub(super) fn new(reference: Reference) -> Self {
        Self {
            reference,
            target: RefCell::new(None),
        }
    }

    fn targeted_at(self, target: Rc<dyn ItemRef>) -> Self {
        self.set_target(target);
        self
    }
}

impl Dependency for PatchReference {
    fn priority(&self) -> DependencyPriority {
        DependencyPriority::Patch
    }

    fn repr(&self) -> String {
        format!("Patch({})", self.reference.repr())
    }

    fn target(&self) -> Option<Rc<dyn ItemRef>> {
        self.target.borrow().clone()
    }

    fn set_target(&self, target: Rc<dyn ItemRef>) {
        *self.target.borrow_mut() = Some(target);
    }

    fn resolve(&self, compiler: &mut ConfigCompiler) -> bool {
        log_info(&format!(
            "PatchReference::Resolve(reference = {:?})",
            self.reference
        ));
        let Some(target) = self.target() else {
            return false;
        };
        let item = resolve_reference(compiler, &self.reference);
        if item.is_null() {
            return self.reference.optional;
        }
        let Some(patch) = item.as_map() else {
            log_error(&format!("invalid patch at {}", self.reference.repr()));
            return false;
        };
        PatchLiteral::new(patch)
            .targeted_at(target)
            .resolve(compiler)
    }
}

/// `__patch: { key/alpha: value, key/beta: value }`（字面量形式）。
pub struct PatchLiteral {
    patch: MapRef,
    target: RefCell<Option<Rc<dyn ItemRef>>>,
}

impl PatchLiteral {
    fn new(patch: MapRef) -> Self {
        Self {
            patch,
            target: RefCell::new(None),
        }
    }

    fn targeted_at(self, target: Rc<dyn ItemRef>) -> Self {
        self.set_target(target);
        self
    }
}

impl Dependency for PatchLiteral {
    fn priority(&self) -> DependencyPriority {
        DependencyPriority::Patch
    }

    fn repr(&self) -> String {
        "Patch(<literal>)".to_string()
    }

    fn target(&self) -> Option<Rc<dyn ItemRef>> {
        self.target.borrow().clone()
    }

    fn set_target(&self, target: Rc<dyn ItemRef>) {
        *self.target.borrow_mut() = Some(target);
    }

    fn resolve(&self, _compiler: &mut ConfigCompiler) -> bool {
        let Some(target) = self.target() else {
            return false;
        };
        let mut success = true;
        let entries: Vec<(String, Item)> = self
            .patch
            .borrow()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        for (key, value) in entries {
            log_info(&format!("patching {key}"));
            if !edit_node(&target, &key, &value, false) {
                log_error(&format!("error applying patch to {key}"));
                success = false;
            }
        }
        success
    }
}

// ---------------------------------------------------------------------------
// 依赖图
// ---------------------------------------------------------------------------

pub struct ConfigDependencyGraph {
    resources: BTreeMap<String, Rc<ConfigResource>>,
    node_stack: Vec<Rc<dyn ItemRef>>,
    key_stack: Vec<String>,
    deps: BTreeMap<String, Rc<RefCell<Vec<Rc<dyn Dependency>>>>>,
    /// 检测循环依赖用的解析链
    resolve_chain: Vec<String>,
}

impl Default for ConfigDependencyGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigDependencyGraph {
    fn new() -> Self {
        Self {
            resources: BTreeMap::new(),
            node_stack: Vec::new(),
            key_stack: Vec::new(),
            deps: BTreeMap::new(),
            resolve_chain: Vec::new(),
        }
    }

    fn current_resource_id(&self) -> String {
        self.key_stack
            .first()
            .map(|key| key.trim_end_matches(':').to_string())
            .unwrap_or_default()
    }

    fn deps_at(&mut self, path: &str) -> Rc<RefCell<Vec<Rc<dyn Dependency>>>> {
        self.deps
            .entry(path.to_string())
            .or_insert_with(|| Rc::new(RefCell::new(Vec::new())))
            .clone()
    }

    /// `ConfigDependencyGraph::Add`。
    fn add(&mut self, dependency: Rc<dyn Dependency>) {
        log_info(&format!(
            "ConfigDependencyGraph::Add(), node_stack.size() = {}",
            self.node_stack.len()
        ));
        let Some(target) = self.node_stack.last().cloned() else {
            return;
        };
        dependency.set_target(target);
        let target_path = ConfigData::join_path(&self.key_stack);
        let target_deps = self.deps_at(&target_path);
        let target_was_pending = !target_deps.borrow().is_empty();
        insert_by_priority(&target_deps, dependency);
        log_info(&format!(
            "target_path = {target_path}, #deps = {}",
            target_deps.borrow().len()
        ));
        if target_was_pending || self.key_stack.len() == 1 {
            // 已经 pending（祖先也都是 pending），或者这里是根资源
            return;
        }
        // 当前 pending 节点变成父节点的一条低优先级依赖；再沿祖先链传播 pending 状态
        let mut keys = self.key_stack.clone();
        let node_stack = self.node_stack.clone();
        for child in node_stack.iter().rev() {
            let last_key = keys.pop().unwrap_or_default();
            let parent_path = ConfigData::join_path(&keys);
            let parent_deps = self.deps_at(&parent_path);
            let parent_was_pending = !parent_deps.borrow().is_empty();
            // PendingChild 必须排在 __include / __patch 之前被解决
            insert_by_priority(
                &parent_deps,
                Rc::new(PendingChild::new(
                    format!("{parent_path}/{last_key}"),
                    child.clone(),
                )),
            );
            log_info(&format!(
                "parent_path = {parent_path}, #deps = {}",
                parent_deps.borrow().len()
            ));
            if parent_was_pending || keys.len() == 1 {
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ConfigCompiler
// ---------------------------------------------------------------------------

pub struct ConfigCompiler {
    resolver: ResourceResolver,
    plugins: Vec<Rc<dyn ConfigCompilerPlugin>>,
    graph: ConfigDependencyGraph,
}

impl ConfigCompiler {
    pub(super) fn new(resolver: ResourceResolver, plugins: Vec<Rc<dyn ConfigCompilerPlugin>>) -> Self {
        Self {
            resolver,
            plugins,
            graph: ConfigDependencyGraph::new(),
        }
    }

    fn install_plugin(&mut self, plugin: Rc<dyn ConfigCompilerPlugin>) {
        self.plugins.push(plugin);
    }

    fn resolver(&self) -> &ResourceResolver {
        &self.resolver
    }

    /// `ConfigCompiler::CreateReference`。
    fn create_reference(&self, qualified_path: &str) -> Reference {
        let end = qualified_path.rfind('?');
        let optional = end.is_some();
        let separator = qualified_path.find(':');
        let resource_raw = match separator {
            None => self.graph.current_resource_id(),
            Some(0) => self.graph.current_resource_id(),
            Some(sep) => qualified_path[..sep].to_string(),
        };
        let resource_id = self.resolver.to_resource_id(&resource_raw);
        let local_path = match separator {
            None => {
                let end = end.unwrap_or(qualified_path.len());
                qualified_path[..end].to_string()
            }
            Some(sep) => {
                let start = sep + 1;
                let end = match end {
                    Some(end) => end,
                    None => qualified_path.len(),
                };
                qualified_path[start..end.max(start)].to_string()
            }
        };
        Reference {
            resource_id,
            local_path,
            optional,
        }
    }

    pub(super) fn add_dependency(&mut self, dependency: Rc<dyn Dependency>) {
        self.graph.add(dependency);
    }

    pub(super) fn push_resource(&mut self, resource: Rc<ConfigResource>) {
        let key = format!("{}:", resource.resource_id);
        let node: Rc<dyn ItemRef> = resource;
        self.graph.node_stack.push(node);
        self.graph.key_stack.push(key);
    }

    pub(super) fn push_list(&mut self, config_list: ListRef, index: usize) {
        let node: Rc<dyn ItemRef> = Rc::new(ListEntryRef::new(config_list, index));
        self.graph.node_stack.push(node);
        self.graph
            .key_stack
            .push(ConfigData::format_list_index(index));
    }

    pub(super) fn push_map(&mut self, config_map: MapRef, key: &str) {
        let node: Rc<dyn ItemRef> = Rc::new(MapEntryRef::new(config_map, key));
        self.graph.node_stack.push(node);
        self.graph.key_stack.push(key.to_string());
    }

    pub(super) fn pop(&mut self) {
        self.graph.node_stack.pop();
        self.graph.key_stack.pop();
    }

    pub(super) fn enumerate_resources(&self, mut process: impl FnMut(&Rc<ConfigResource>)) {
        for resource in self.graph.resources.values() {
            process(resource);
        }
    }

    fn get_compiled_resource(&self, resource_id: &str) -> Option<Rc<ConfigResource>> {
        self.graph.resources.get(resource_id).cloned()
    }

    /// `ConfigCompiler::Compile`。
    pub(super) fn compile(&mut self, file_name: &str) -> Rc<ConfigResource> {
        let resource_id = self.resolver.to_resource_id(file_name);
        let resource = Rc::new(ConfigResource::new(resource_id.clone()));
        self.graph
            .resources
            .insert(resource_id.clone(), resource.clone());
        self.push_resource(resource.clone());
        let file_path = self.resolver.resolve_path(&resource_id);
        let loaded = resource.data.load_from_file(&file_path, Some(self));
        resource.loaded.set(loaded);
        self.pop();
        let plugins = self.plugins.clone();
        for plugin in &plugins {
            if !plugin.review_compile_output(self, &resource) {
                break;
            }
        }
        resource
    }

    /// `ConfigCompiler::Link`。
    pub(super) fn link(&mut self, target: Rc<ConfigResource>) -> bool {
        log_info(&format!("Link({})", target.resource_id));
        if !self.graph.resources.contains_key(&target.resource_id) {
            log_error(&format!("resource not found: {}", target.resource_id));
            return false;
        }
        if !self.resolve_dependencies(&format!("{}:", target.resource_id)) {
            return false;
        }
        let plugins = self.plugins.clone();
        for plugin in &plugins {
            if !plugin.review_link_output(self, &target) {
                return false;
            }
        }
        true
    }

    fn blocking(&self, full_path: &str) -> bool {
        self.graph
            .deps
            .get(full_path)
            .and_then(|deps| deps.borrow().last().cloned())
            .map_or(false, |dependency| dependency.blocking())
    }

    fn pending(&self, full_path: &str) -> bool {
        !self.resolved(full_path)
    }

    fn resolved(&self, full_path: &str) -> bool {
        self.graph
            .deps
            .get(full_path)
            .map(|deps| deps.borrow().is_empty())
            .unwrap_or(true)
    }

    pub(super) fn get_dependencies(&self, path: &str) -> Vec<Rc<dyn Dependency>> {
        self.graph
            .deps
            .get(path)
            .map(|deps| deps.borrow().clone())
            .unwrap_or_default()
    }

    fn has_circular_dependencies(&self, path: &str) -> bool {
        let path_bytes = path.as_bytes();
        for x in &self.graph.resolve_chain {
            let x_bytes = x.as_bytes();
            if x_bytes.starts_with(path_bytes)
                && (x_bytes.len() == path_bytes.len()
                    || x_bytes.get(path_bytes.len()) == Some(&b'/'))
            {
                return true;
            }
        }
        false
    }

    /// `ConfigCompiler::ResolveDependencies`。
    fn resolve_dependencies(&mut self, path: &str) -> bool {
        log_info(&format!("ResolveDependencies({path})"));
        let Some(deps) = self.graph.deps.get(path).cloned() else {
            return true;
        };
        if self.has_circular_dependencies(path) {
            log_warn(&format!("circular dependencies detected in {path}"));
            return false;
        }
        self.graph.resolve_chain.push(path.to_string());
        loop {
            let dependency = { deps.borrow().first().cloned() };
            let Some(dependency) = dependency else {
                break;
            };
            if !dependency.resolve(self) {
                log_error(&format!("unresolved dependency: {}", dependency.repr()));
                // 与 C++ 一致：失败时 resolve_chain 不 pop，未解决的依赖留在表里
                return false;
            }
            log_info(&format!("resolved: {}", dependency.repr()));
            deps.borrow_mut().remove(0);
        }
        self.graph.resolve_chain.pop();
        log_info("all dependencies resolved.");
        true
    }

    /// `ConfigCompiler::Parse`。
    pub(super) fn parse(&mut self, key: &str, item: &Item) -> bool {
        log_info(&format!("ConfigCompiler::Parse({key})"));
        if key == INCLUDE_DIRECTIVE {
            return parse_include(self, item);
        }
        if key == PATCH_DIRECTIVE {
            return parse_list(parse_patch, self, item);
        }
        false
    }
}

fn insert_by_priority(list: &Rc<RefCell<Vec<Rc<dyn Dependency>>>>, dependency: Rc<dyn Dependency>) {
    // std::upper_bound：插到所有 priority <= 新值的元素之后（相同优先级保持先后）
    let priority = dependency.priority();
    let mut deps = list.borrow_mut();
    let position = deps
        .iter()
        .position(|existing| existing.priority() > priority)
        .unwrap_or(deps.len());
    deps.insert(position, dependency);
}

// ---------------------------------------------------------------------------
// 解析指令
// ---------------------------------------------------------------------------

// Includes contents of nodes at specified paths.
// __include: path/to/local/node
// __include: filename[.yaml]:/path/to/external/node
fn parse_include(compiler: &mut ConfigCompiler, item: &Item) -> bool {
    if let Some(path) = item.as_scalar() {
        log_info(&format!("ParseInclude({path})"));
        compiler.add_dependency(Rc::new(IncludeReference::new(
            compiler.create_reference(path),
        )));
        return true;
    }
    false
}

type Parser = fn(&mut ConfigCompiler, &Item) -> bool;

// Applies `parser` to every list element if `item` is a list.
// __patch: [ first/patch, filename:/second/patch ]
// __patch: [{list/@next: 1}, {list/@next: 2}]
fn parse_list(parser: Parser, compiler: &mut ConfigCompiler, item: &Item) -> bool {
    if let Some(list) = item.as_list() {
        let elements: Vec<Item> = list.borrow().clone();
        for list_item in elements {
            if !parser(compiler, &list_item) {
                return false;
            }
        }
        return true;
    }
    parser(compiler, item)
}

// Modifies subnodes or list elements at specified paths.
// __patch: path/to/node
// __patch: filename[.yaml]:/path/to/node
// __patch: { key/alpha: value, key/beta: value }
fn parse_patch(compiler: &mut ConfigCompiler, item: &Item) -> bool {
    if let Some(path) = item.as_scalar() {
        log_info(&format!("ParsePatch({path})"));
        compiler.add_dependency(Rc::new(PatchReference::new(
            compiler.create_reference(path),
        )));
        return true;
    }
    if let Some(map) = item.as_map() {
        log_info("ParsePatch(<literal>)");
        compiler.add_dependency(Rc::new(PatchLiteral::new(map)));
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// 引用解析
// ---------------------------------------------------------------------------

fn resolve_reference(compiler: &mut ConfigCompiler, reference: &Reference) -> Item {
    let resource = match compiler.get_compiled_resource(&reference.resource_id) {
        Some(resource) => resource,
        None => {
            log_info(&format!(
                "resource not loaded, compiling: {}",
                reference.resource_id
            ));
            let compiled = compiler.compile(&reference.resource_id);
            if !compiled.loaded.get() {
                if reference.optional {
                    log_info(&format!(
                        "optional resource not loaded: {}",
                        reference.resource_id
                    ));
                } else {
                    log_error(&format!(
                        "resource could not be loaded: {}",
                        reference.resource_id
                    ));
                }
                return Item::Null;
            }
            compiled
        }
    };
    get_resolved_item(compiler, &resource, &reference.local_path)
}

/// `ResolveBlockingDependencies`。
fn resolve_blocking_dependencies(compiler: &mut ConfigCompiler, path: &str) -> bool {
    if !compiler.blocking(path) {
        return true;
    }
    log_info(&format!("blocking node: {path}"));
    if compiler.resolve_dependencies(path) {
        log_info(&format!("resolved blocking node: {path}"));
        return true;
    }
    false
}

/// `GetResolvedItem`。
fn get_resolved_item(
    compiler: &mut ConfigCompiler,
    resource: &Rc<ConfigResource>,
    path: &str,
) -> Item {
    log_info(&format!("GetResolvedItem({}:{path})", resource.resource_id));
    let mut node_path = format!("{}:", resource.resource_id);
    let mut node: Rc<dyn ItemRef> = resource.clone();
    if path.is_empty() || path == "/" {
        return if compiler.resolve_dependencies(&node_path) {
            node.get_item()
        } else {
            Item::Null
        };
    }
    let keys = ConfigData::split_path(path);
    for key in keys {
        if !resolve_blocking_dependencies(compiler, &node_path) {
            log_warn(&format!(
                "accessing blocking node with unresolved dependencies: {node_path}"
            ));
            // CAVEAT（照抄 C++）：这里仍然继续访问，可能引用到旧数据。
        }
        let item = node.get_item();
        if let Some(list) = item.as_list() {
            if ConfigData::is_list_item_reference(&key) {
                let index = ConfigData::resolve_list_index(&item, &key, true);
                node_path = format!("{node_path}/{}", ConfigData::format_list_index(index));
                node = Rc::new(ListEntryRef::new(list, index));
            } else {
                return Item::Null;
            }
        } else if let Some(map) = item.as_map() {
            log_info(&format!("advance with key: {key}"));
            node_path = format!("{node_path}/{key}");
            node = Rc::new(MapEntryRef::new(map, &key));
        } else {
            return Item::Null;
        }
    }
    if compiler.resolve_dependencies(&node_path) {
        node.get_item()
    } else {
        Item::Null
    }
}

// ---------------------------------------------------------------------------
// 编辑算子（config_compiler.cc 96-278 行）
// ---------------------------------------------------------------------------

const ADD_SUFFIX_OPERATOR: &str = "/+";
const EQU_SUFFIX_OPERATOR: &str = "/=";

/// `ParseIndexedAppend`：`foo/3+` → Some(3)。
fn parse_indexed_append(key: &str) -> Option<usize> {
    if key.is_empty() || !key.ends_with('+') {
        return None;
    }
    let bytes = key.as_bytes();
    let plus_pos = bytes.len() - 1;
    let mut digit_begin = plus_pos;
    while digit_begin > 0 && bytes[digit_begin - 1].is_ascii_digit() {
        digit_begin -= 1;
    }
    if digit_begin == plus_pos {
        return None;
    }
    if digit_begin > 0 && bytes[digit_begin - 1] != b'/' {
        return None;
    }
    key[digit_begin..plus_pos].parse::<usize>().ok()
}

fn is_appending(key: &str, indexed_append: Option<usize>) -> bool {
    key == APPEND_DIRECTIVE || key.ends_with(ADD_SUFFIX_OPERATOR) || indexed_append.is_some()
}

fn is_merging(key: &str, value: &Item, merge_tree: bool, indexed_append: Option<usize>) -> bool {
    let has_plain_add_suffix = key.ends_with(ADD_SUFFIX_OPERATOR) && indexed_append.is_none();
    key == MERGE_DIRECTIVE
        || has_plain_add_suffix
        || (merge_tree
            && (value.is_null() || value.is_map())
            && !key.ends_with(EQU_SUFFIX_OPERATOR))
}

/// `StripOperator`。
fn strip_operator(key: &str, adding: bool, indexed_append: Option<usize>) -> String {
    if key == APPEND_DIRECTIVE || key == MERGE_DIRECTIVE {
        return String::new();
    }
    if let Some(index) = indexed_append {
        let suffix_with_slash = format!("/{index}+");
        if key.ends_with(&suffix_with_slash) {
            return key[..key.len() - suffix_with_slash.len()].to_string();
        }
        let suffix_plain = format!("{index}+");
        if key.ends_with(&suffix_plain) {
            return key[..key.len() - suffix_plain.len()].to_string();
        }
    }
    let operator = if adding {
        ADD_SUFFIX_OPERATOR
    } else {
        EQU_SUFFIX_OPERATOR
    };
    // boost::erase_last_copy：删掉最后一次出现的子串（不要求结尾）
    match key.rfind(operator) {
        Some(position) => {
            let mut stripped = String::with_capacity(key.len() - operator.len());
            stripped.push_str(&key[..position]);
            stripped.push_str(&key[position + operator.len()..]);
            stripped
        }
        None => key.to_string(),
    }
}

fn append_to_string(target: &Rc<dyn ItemRef>, value: &Item) -> bool {
    let Some(append) = value.as_scalar() else {
        return false;
    };
    let existing = target.get_item();
    let Some(existing) = existing.as_scalar() else {
        log_error("trying to append string to non scalar");
        return false;
    };
    let joined = format!("{existing}{append}");
    target.set_item(Item::Scalar(joined));
    true
}

fn append_to_list(target: &Rc<dyn ItemRef>, value: &Item, insert_pos: Option<usize>) -> bool {
    let Some(list) = value.as_list() else {
        return false;
    };
    // 顺序照 C++：先尝试拿已有的列表；不是列表但为"空节点"（通常只剩编译器指令）
    // 就就地转成列表（`AsList()` 会写回），否则是类型冲突。
    let existing_list = match target.get_item().as_list() {
        Some(existing) => existing,
        None => {
            if !target.get_item().empty() {
                log_error("trying to append list to incompatible node type");
                return false;
            }
            target.as_list()
        }
    };
    if list.borrow().is_empty() {
        return true;
    }
    let copy: ListRef = Rc::new(RefCell::new(existing_list.borrow().clone()));
    if let Some(position) = insert_pos {
        if position > copy.borrow().len() {
            log_error(&format!("list insert position out of range: {position}"));
            return false;
        }
    }
    let mut current_index = insert_pos.unwrap_or_else(|| copy.borrow().len());
    for element in list.borrow().iter() {
        if insert_pos.is_some() {
            copy.borrow_mut().insert(current_index, element.clone());
            current_index += 1;
        } else {
            copy.borrow_mut().push(element.clone());
        }
    }
    target.set_item(Item::List(copy));
    true
}

/// `MergeTree`。
///
/// **顺序规则（与 C++ 的唯一有意差异）**：被合并进来的键，**新键追加到目标映射的尾部**，
/// **已有键保持原位**（只换值）。依据是 `IndexMap::insert` 的语义 —— 键已存在时只更新
/// 值、不改变其下标；键不存在时压到末尾。`MergeTree` / `EditNode` 的写入全都汇到
/// `ConfigCowRef::set_item` 的那一次 `insert`，所以整条合并链（`__include` 的 overrides、
/// `__patch`、`__merge`、`__append` 之外的递归合并）都自动满足这条规则。
///
/// 于是产物键序 = "被包含文件的键序" + "本文件里那些**新**键按出现顺序追加在后面"，
/// 与手写源的直觉一致；值语义与 C++ 完全等价（C++ 那边 `std::map` 只是把这套写入
/// 排成了字典序，不影响谁覆盖谁）。
pub(crate) fn merge_tree(target: &Rc<dyn ItemRef>, map: MapRef) -> bool {
    let entries: Vec<(String, Item)> = map
        .borrow()
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    for (key, value) in entries {
        if !edit_node(target, &key, &value, true) {
            log_error(&format!("error merging branch {key}"));
            return false;
        }
    }
    true
}

/// `EditNode`：`/+` 追加、`/=` 覆盖、`@n+` / `n+` 索引插入、`__merge`、
/// 以及 `merge_tree` 模式下的递归合并。
pub(crate) fn edit_node(
    head: &Rc<dyn ItemRef>,
    key: &str,
    value: &Item,
    merge_tree_mode: bool,
) -> bool {
    log_info(&format!("edit node: {key}, merge_tree: {merge_tree_mode}"));
    let indexed_append = parse_indexed_append(key);
    let appending = is_appending(key, indexed_append);
    let merging = is_merging(key, value, merge_tree_mode, indexed_append);
    let path = strip_operator(key, appending || merging, indexed_append);
    log_info(&format!(
        "appending: {appending}, merging: {merging}, path: {path}"
    ));
    let target = if merge_tree_mode {
        type_checked_copy_on_write(head.clone(), &path)
    } else {
        traverse_copy_on_write(head.clone(), &path)
    };
    let Some(target) = target else {
        return false;
    };
    let existing = target.get_item();
    if (appending || merging) && !existing.is_null() {
        log_info("writer: editing node");
        if value.is_null() {
            return true;
        }
        if appending
            && (append_to_string(&target, value) || append_to_list(&target, value, indexed_append))
        {
            return true;
        }
        if merging {
            if let Some(map) = value.as_map() {
                return merge_tree(&target, map);
            }
        }
        false
    } else {
        log_info("writer: overwriting node");
        target.set_item(value.clone());
        true
    }
}
