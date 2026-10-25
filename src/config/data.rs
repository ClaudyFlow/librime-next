//! `librime/src/rime/config/config_data.{h,cc}` 的移植，
//! 外加 `config_cow_ref.h` 的 `ConfigCowRef` / `Cow`（写时复制引用）。
//!
//! 负责：树、`/` 路径、`@n` 列表索引语法、YAML → 树的转换。
//! **树的发射不在这里** —— 全 crate 唯一出口是 [`super::emit`]（`EmitYaml` 的移植）。

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use indexmap::IndexMap;

use super::compiler::ConfigCompiler;
use super::emit::EmitError;
use super::types::{Item, ItemRef, MapRef, ValueType};

/// `ConfigData`。`root` 可整体替换（C++ 的 `an<ConfigItem> root` 是 public 字段）。
///
/// 用 `Rc<RefCell<Item>>` 而不是裸 `RefCell`，是因为 `ConfigDataRootRef` 需要
/// 一个不带生命周期的"根位置"引用（C++ 里是 `ConfigData*`）。
pub struct ConfigData {
    pub(super) root: Rc<RefCell<Item>>,
    file_path: RefCell<PathBuf>,
    modified: Cell<bool>,
    auto_save: Cell<bool>,
}

impl Default for ConfigData {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigData {
    pub(super) fn new() -> Self {
        Self {
            root: Rc::new(RefCell::new(Item::Null)),
            file_path: RefCell::new(PathBuf::new()),
            modified: Cell::new(false),
            auto_save: Cell::new(false),
        }
    }

    pub(super) fn file_path(&self) -> PathBuf {
        self.file_path.borrow().clone()
    }

    fn modified(&self) -> bool {
        self.modified.get()
    }

    fn set_modified(&self) {
        self.modified.set(true);
    }

    fn set_auto_save(&self, auto_save: bool) {
        self.auto_save.set(auto_save);
    }

    /// `ConfigData::Save`：只有改过、有路径、且写成功才算保存。
    fn save(&self) -> bool {
        if !self.modified.get() || self.file_path.borrow().as_os_str().is_empty() {
            return false;
        }
        let path = self.file_path();
        self.save_to_file(&path)
    }

    /// `ConfigData::LoadFromFile`：先更新状态（路径、modified、清空 root），
    /// 文件不存在或解析失败返回 false。
    pub(super) fn load_from_file(&self, file_path: &Path, compiler: Option<&mut ConfigCompiler>) -> bool {
        *self.file_path.borrow_mut() = file_path.to_path_buf();
        self.modified.set(false);
        *self.root.borrow_mut() = Item::Null;
        if !file_path.exists() {
            if !file_path.to_string_lossy().ends_with(".custom.yaml") {
                log_warn(&format!(
                    "nonexistent config file '{}'.",
                    file_path.display()
                ));
            }
            return false;
        }
        log_info(&format!("loading config file '{}'.", file_path.display()));
        match super::load::load_file(file_path) {
            Ok(yaml) => {
                let item = convert_from_yaml(&yaml, compiler);
                *self.root.borrow_mut() = item;
                true
            }
            Err(error) => {
                log_error(&format!(
                    "Error parsing YAML \"{}\" : {error}",
                    file_path.display()
                ));
                false
            }
        }
    }

    /// `ConfigData::SaveToFile`。
    pub(super) fn save_to_file(&self, file_path: &Path) -> bool {
        *self.file_path.borrow_mut() = file_path.to_path_buf();
        self.modified.set(false);
        if file_path.as_os_str().is_empty() {
            return false;
        }
        log_info(&format!("saving config file '{}'.", file_path.display()));
        let text = match self.save_to_string() {
            Ok(text) => text,
            Err(error) => {
                log_error(&format!("failed to emit YAML: {error}"));
                return false;
            }
        };
        std::fs::write(file_path, text).is_ok()
    }

    /// `ConfigData::SaveToStream` 的字符串版本（走 [`super::emit`] 这个唯一出口）。
    fn save_to_string(&self) -> Result<String, EmitError> {
        super::emit::to_yaml_string(&self.root.borrow())
    }

    fn set_root(&self, item: Item) {
        *self.root.borrow_mut() = item;
    }

    /// `ConfigData::Traverse`。
    pub(super) fn traverse(&self, node_path: &str) -> Item {
        let root = self.root.borrow().clone();
        if node_path.is_empty() || node_path == "/" {
            return root;
        }
        let keys = Self::split_path(node_path);
        let mut p = root;
        for key in &keys {
            let mut node_type = ValueType::Map;
            let mut list_index = 0usize;
            if Self::is_list_item_reference(key) {
                node_type = ValueType::List;
                list_index = Self::resolve_list_index(&p, key, true);
            }
            if p.is_null() || p.value_type() != node_type {
                return Item::Null;
            }
            p = if node_type == ValueType::List {
                p.get_at(list_index)
            } else {
                p.get(key)
            };
        }
        p
    }

    /// `ConfigData::TraverseWrite`。
    fn traverse_write(&self, node_path: &str, item: Item) -> bool {
        log_info(&format!("write: {node_path}"));
        let root: Rc<dyn ItemRef> = Rc::new(ConfigDataRootRef {
            root: self.root.clone(),
        });
        if let Some(target) = traverse_copy_on_write(root, node_path) {
            target.set_item(item);
            self.set_modified();
            true
        } else {
            false
        }
    }

    /// `ConfigData::SplitPath`：去掉前导 `/`，按 `/` 切。
    pub(super) fn split_path(node_path: &str) -> Vec<String> {
        node_path
            .trim_start_matches('/')
            .split('/')
            .map(str::to_string)
            .collect()
    }

    /// `ConfigData::JoinPath`。
    pub(super) fn join_path(keys: &[String]) -> String {
        keys.join("/")
    }

    /// `ConfigData::IsListItemReference`：`@` + 字母数字。
    pub(super) fn is_list_item_reference(key: &str) -> bool {
        let bytes = key.as_bytes();
        bytes.len() > 1 && bytes[0] == b'@' && bytes[1].is_ascii_alphanumeric()
    }

    /// `ConfigData::FormatListIndex`。
    pub(super) fn format_list_index(index: usize) -> String {
        format!("@{index}")
    }

    /// `ConfigData::ResolveListIndex`：`@next` / `@last` / `@before N` /
    /// `@after N` / `@N`；非只读且是插入语义时先塞一个空位。
    pub(super) fn resolve_list_index(item: &Item, key: &str, read_only: bool) -> usize {
        if !Self::is_list_item_reference(key) {
            return 0;
        }
        let Some(list) = item.as_list() else {
            return 0;
        };
        let bytes = key.as_bytes();
        let mut cursor = 1usize;
        let mut index: usize = 0;
        let mut will_insert = false;
        if bytes[cursor..].starts_with(b"next") {
            cursor += 4;
            index = list.borrow().len();
        } else if bytes[cursor..].starts_with(b"before") {
            cursor += 6;
            will_insert = true;
        } else if bytes[cursor..].starts_with(b"after") {
            cursor += 5;
            index += 1; // after i == before i+1
            will_insert = true;
        }
        if cursor < bytes.len() && bytes[cursor] == b' ' {
            cursor += 1;
        }
        if bytes[cursor..].starts_with(b"last") {
            index += list.borrow().len();
            if index != 0 {
                // 空列表时 (before|after) last == 0
                index -= 1;
            }
        } else {
            index += strtoul(&key[cursor..]);
        }
        if will_insert && !read_only {
            let mut seq = list.borrow_mut();
            // ConfigList::Insert：越界先 resize 到 index
            if index > seq.len() {
                seq.resize(index, Item::Null);
            }
            seq.insert(index, Item::Null);
        }
        index
    }
}

/// `ConfigDataRootRef`：`ConfigData` 的根位置。
struct ConfigDataRootRef {
    root: Rc<RefCell<Item>>,
}

impl ItemRef for ConfigDataRootRef {
    fn get_item(&self) -> Item {
        self.root.borrow().clone()
    }

    fn set_item(&self, item: Item) {
        *self.root.borrow_mut() = item;
    }
}

impl ItemRef for ConfigData {
    fn get_item(&self) -> Item {
        self.root.borrow().clone()
    }

    fn set_item(&self, item: Item) {
        *self.root.borrow_mut() = item;
    }
}

/// `ConfigCowRef<T>`：首次写时把父容器浅拷贝一份、装回父引用，再写自己这一格。
struct ConfigCowRef {
    parent: Rc<dyn ItemRef>,
    key: String,
    copied: Cell<bool>,
}

impl ConfigCowRef {
    fn new(parent: Rc<dyn ItemRef>, key: impl Into<String>) -> Self {
        Self {
            parent,
            key: key.into(),
            copied: Cell::new(false),
        }
    }
}

impl ItemRef for ConfigCowRef {
    fn get_item(&self) -> Item {
        let container = self.parent.get_item();
        if ConfigData::is_list_item_reference(&self.key) {
            let Some(list) = container.as_list() else {
                return Item::Null;
            };
            let index = ConfigData::resolve_list_index(&container, &self.key, true);
            list.borrow().get(index).cloned().unwrap_or(Item::Null)
        } else {
            container.get(&self.key)
        }
    }

    fn set_item(&self, item: Item) {
        let mut container = self.parent.get_item();
        if !self.copied.get() {
            let is_list = ConfigData::is_list_item_reference(&self.key);
            let new_container = if is_list {
                match container.as_list() {
                    Some(list) => Item::List(Rc::new(RefCell::new(list.borrow().clone()))),
                    None => Item::list(),
                }
            } else {
                match container.as_map() {
                    Some(map) => Item::Map(Rc::new(RefCell::new(map.borrow().clone()))),
                    None => Item::map(),
                }
            };
            self.parent.set_item(new_container.clone());
            container = new_container;
            self.copied.set(true);
        }
        if ConfigData::is_list_item_reference(&self.key) {
            if let Item::List(list) = &container {
                let index = ConfigData::resolve_list_index(&container, &self.key, false);
                let mut seq = list.borrow_mut();
                if index >= seq.len() {
                    seq.resize(index + 1, Item::Null);
                }
                seq[index] = item;
            }
        } else if let Item::Map(map) = &container {
            map.borrow_mut().insert(self.key.clone(), item);
        }
    }
}

/// `Cow()`：按 key 是否列表引用选出列表/映射两种写时复制语义。
pub fn cow(parent: Rc<dyn ItemRef>, key: impl Into<String>) -> Rc<dyn ItemRef> {
    Rc::new(ConfigCowRef::new(parent, key))
}

/// `TypeCheckedCopyOnWrite`：空 key 直接返回 parent；否则父容器类型必须匹配
/// （列表引用要求 List，普通 key 要求 Map），不匹配返回 None。
pub(super) fn type_checked_copy_on_write(parent: Rc<dyn ItemRef>, key: &str) -> Option<Rc<dyn ItemRef>> {
    if key.is_empty() {
        return Some(parent);
    }
    let expected = if ConfigData::is_list_item_reference(key) {
        ValueType::List
    } else {
        ValueType::Map
    };
    let existing = parent.get_item();
    if !existing.is_null() && existing.value_type() != expected {
        log_error(&format!(
            "copy on write failed; incompatible node type: {key}"
        ));
        return None;
    }
    Some(cow(parent, key))
}

/// `TraverseCopyOnWrite`：沿路径逐级做类型检查 + 写时复制，返回最深一层的引用。
pub(super) fn traverse_copy_on_write(head: Rc<dyn ItemRef>, node_path: &str) -> Option<Rc<dyn ItemRef>> {
    log_info(&format!("TraverseCopyOnWrite({node_path})"));
    if node_path.is_empty() || node_path == "/" {
        return Some(head);
    }
    let keys = ConfigData::split_path(node_path);
    let mut head = head;
    for key in &keys {
        match type_checked_copy_on_write(head, key) {
            Some(child) => head = child,
            None => {
                log_error(&format!("while writing to {node_path}"));
                return None;
            }
        }
    }
    Some(head)
}

// ---------------------------------------------------------------------------
// YAML → 树
// ---------------------------------------------------------------------------

/// `ConvertFromYaml`。
///
/// [`super::load::load_file`] 已经把 YAML 读成 `serde_json::Value`（并处理了 Rime
/// 词典的 `...` 约定），这一步按 C++ 的规则转成 `ConfigItem`：
///
/// * 标量统统变成字符串（yaml-cpp `node.as<string>()` 的行为）；
/// * null 变成空节点；
/// * **map 的每个键都会在转换时 push/pop 编译器节点栈**，然后 `Parse` 决定它是不是
///   `__include`/`__patch` 指令：是的话不存进树，否的话原样存。
///
/// 键序：`serde_json` 开了 `preserve_order`，所以这里 `mapping` 的迭代顺序**就是源文件
/// 里的键序**，`insert` 到 `IndexMap` 后原样保留（指令键被丢掉，也不影响其余键的相对
/// 顺序）。树 → YAML 时 [`super::emit`] 按同一顺序吐出。
pub fn convert_from_yaml(
    yaml: &serde_json::Value,
    mut compiler: Option<&mut ConfigCompiler>,
) -> Item {
    match yaml {
        serde_json::Value::Null => Item::Null,
        serde_json::Value::Bool(value) => {
            Item::Scalar(if *value { "true" } else { "false" }.into())
        }
        serde_json::Value::Number(value) => Item::Scalar(value.to_string()),
        serde_json::Value::String(value) => Item::Scalar(value.clone()),
        serde_json::Value::Array(sequence) => {
            let list: super::types::ListRef = Rc::new(RefCell::new(Vec::new()));
            for child in sequence {
                if let Some(c) = compiler.as_deref_mut() {
                    let index = list.borrow().len();
                    c.push_list(list.clone(), index);
                }
                let value = convert_from_yaml(child, compiler.as_deref_mut());
                if let Some(c) = compiler.as_deref_mut() {
                    c.pop();
                }
                list.borrow_mut().push(value);
            }
            Item::List(list)
        }
        serde_json::Value::Object(mapping) => {
            let map: MapRef = Rc::new(RefCell::new(IndexMap::new()));
            for (key, child) in mapping {
                if let Some(c) = compiler.as_deref_mut() {
                    c.push_map(map.clone(), key);
                }
                let value = convert_from_yaml(child, compiler.as_deref_mut());
                if let Some(c) = compiler.as_deref_mut() {
                    c.pop();
                }
                let parsed = if let Some(c) = compiler.as_deref_mut() {
                    c.parse(key, &value)
                } else {
                    false
                };
                if !parsed {
                    map.borrow_mut().insert(key.clone(), value);
                }
            }
            Item::Map(map)
        }
    }
}

/// 去掉顶层 `__build_info`（对拍时两边都剔，里面是 mtime）。
///
/// 用 `shift_remove`（O(n) 但保序）：`swap_remove` 会把最后一个键挪到这个位置，
/// 剩下的键序就乱了。
pub fn strip_build_info(item: &mut Item) {
    if let Item::Map(map) = item {
        map.borrow_mut().shift_remove("__build_info");
    }
}

/// `main.rs diff` 用：直接作用在 `serde_json::Value` 上。
pub fn strip_build_info_json(value: &mut serde_json::Value) {
    if let serde_json::Value::Object(map) = value {
        map.remove("__build_info");
    }
}

// ---------------------------------------------------------------------------
// Config（config_component.cc 18-147 行）
// ---------------------------------------------------------------------------

/// `Config`：`ConfigData` 的门面。`ConfigItemRef` 的根引用就是它的 root。
#[derive(Clone)]
pub struct Config {
    data: Rc<ConfigData>,
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

impl Config {
    fn new() -> Self {
        Self {
            data: Rc::new(ConfigData::new()),
        }
    }

    pub fn with_data(data: Rc<ConfigData>) -> Self {
        Self { data }
    }

    fn data(&self) -> Rc<ConfigData> {
        self.data.clone()
    }

    fn save(&self) -> bool {
        self.data.save()
    }

    fn load_from_file(&self, file_path: &Path) -> bool {
        self.data.load_from_file(file_path, None)
    }

    fn save_to_file(&self, file_path: &Path) -> bool {
        self.data.save_to_file(file_path)
    }

    pub fn is_null(&self, path: &str) -> bool {
        self.data.traverse(path).is_null()
    }

    /// 注意：C++ 这里写的是 `return !p || p->type() == kScalar;`，路径不存在也返回 true。
    fn is_value(&self, path: &str) -> bool {
        let p = self.data.traverse(path);
        p.is_null() || p.is_scalar()
    }

    pub fn is_list(&self, path: &str) -> bool {
        let p = self.data.traverse(path);
        p.is_null() || p.is_list()
    }

    pub fn is_map(&self, path: &str) -> bool {
        let p = self.data.traverse(path);
        p.is_null() || p.is_map()
    }

    pub fn get_bool(&self, path: &str) -> Option<bool> {
        self.data.traverse(path).get_bool()
    }

    pub fn get_int(&self, path: &str) -> Option<i32> {
        self.data.traverse(path).get_int()
    }

    fn get_double(&self, path: &str) -> Option<f64> {
        self.data.traverse(path).get_double()
    }

    pub fn get_string(&self, path: &str) -> Option<String> {
        self.data.traverse(path).get_string()
    }

    pub fn get_list_size(&self, path: &str) -> usize {
        self.data
            .traverse(path)
            .as_list()
            .map(|list| list.borrow().len())
            .unwrap_or(0)
    }

    fn get_item(&self, path: &str) -> Item {
        self.data.traverse(path)
    }

    fn get_value(&self, path: &str) -> Option<String> {
        self.data.traverse(path).get_string()
    }

    fn get_list(&self, path: &str) -> Option<super::types::ListRef> {
        self.data.traverse(path).as_list()
    }

    fn get_map(&self, path: &str) -> Option<MapRef> {
        self.data.traverse(path).as_map()
    }

    fn set_item(&self, path: &str, item: Item) -> bool {
        self.data.traverse_write(path, item)
    }

    fn set_bool(&self, path: &str, value: bool) -> bool {
        self.set_item(
            path,
            Item::Scalar(if value { "true" } else { "false" }.into()),
        )
    }

    fn set_int(&self, path: &str, value: i32) -> bool {
        self.set_item(path, Item::Scalar(value.to_string()))
    }

    fn set_double(&self, path: &str, value: f64) -> bool {
        // std::to_string(double) 固定 6 位小数
        self.set_item(path, Item::Scalar(format!("{value:.6}")))
    }

    fn set_string(&self, path: &str, value: &str) -> bool {
        self.set_item(path, Item::Scalar(value.to_string()))
    }
}

impl ItemRef for Config {
    fn get_item(&self) -> Item {
        self.data.root.borrow().clone()
    }

    fn set_item(&self, item: Item) {
        *self.data.root.borrow_mut() = item;
        self.data.set_modified();
    }
}

// ---------------------------------------------------------------------------
// 日志（对应 C++ 的 LOG(INFO)/LOG(WARNING)/LOG(ERROR)）
// ---------------------------------------------------------------------------

pub(crate) fn log_info(message: &str) {
    if std::env::var_os("RIME_CONFIG_VERBOSE").is_some() {
        eprintln!("[config] {message}");
    }
}

pub(crate) fn log_warn(message: &str) {
    if std::env::var_os("RIME_CONFIG_VERBOSE").is_some() {
        eprintln!("[config] warning: {message}");
    }
}

pub(crate) fn log_error(message: &str) {
    if std::env::var_os("RIME_CONFIG_VERBOSE").is_some() {
        eprintln!("[config] error: {message}");
    }
}

/// `std::strtoul` 的前缀解析（跳过空白、可选正负号、十进制数字），饱和到 usize。
fn strtoul(value: &str) -> usize {
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let mut negative = false;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        negative = bytes[i] == b'-';
        i += 1;
    }
    let mut result: usize = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        result = result
            .saturating_mul(10)
            .saturating_add((bytes[i] - b'0') as usize);
        i += 1;
    }
    if negative {
        result.wrapping_neg()
    } else {
        result
    }
}
