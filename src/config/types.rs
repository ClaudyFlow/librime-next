//! `librime/src/rime/config/config_types.{h,cc}` 的移植。
//!
//! ## 数据结构为什么长这样
//!
//! C++ 里 `ConfigItem` 是 `an<ConfigItem>`（`shared_ptr`）指向的四种节点：
//! `kNull / kScalar / kList / kMap`；标量**永远是字符串**（`ConfigValue::value_`），
//! `GetInt`/`GetBool`/`GetDouble` 都是"从字符串解析"，写回时再 `to_string`。
//!
//! Rust 侧：
//!
//! * `Item::Null` 同时充当 C++ 的**空指针**（`an<ConfigItem>()`）——C++ 里
//!   `ConvertFromYaml` 对 YAML null 返回 `nullptr`，从不构造 `kNull` 实例；
//! * `Item::List/Map` 内部是 `Rc<RefCell<...>>`，对应 C++ 的共享容器对象：
//!   拷贝一个 `Item` 只拷贝指针（浅拷贝），`ConfigCowRef` 的写时复制才有意义；
//! * `Map` 用 `IndexMap`：**按插入顺序**迭代，插入顺序就是源文件里的键序。
//!
//!   C++ 的 `ConfigMap::Map` 是 `std::map<string, ...>`（字典序），所以 librime 的
//!   产物是"字节序"排列的，跟手写源文件对不上。这里**有意偏离 C++**：读入时按源文件
//!   顺序插入，发射时按同一顺序吐出，产物与源文件逐键对得上，diff 才干净。
//!   插入语义（见 `Item::map` 的文档）：已有键**保持原位**只换值，新键**追加到尾部**
//!   —— `IndexMap::insert` 天生就是这个语义，`MergeTree` / `__patch` 直接沿用。
//!
//! `ItemRef` 对应 `ConfigItemRef`：`MapEntryRef` / `ListEntryRef` 直接持有容器
//! 的共享指针（与 C++ 完全一致），因此在父节点被替换后，旧 ref 仍然读写它当初
//! 抓住的那个容器——`legacy_preset_config_plugin` 正是靠这个别名行为工作的。

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ValueType {
    Null,
    Scalar,
    List,
    Map,
}

/// 配置映射：**插入顺序即键序**。
///
/// 用 `IndexMap` 而不是 `BTreeMap` 的理由只有一条：产物 YAML 的键序必须与源文件一致
/// （`BTreeMap` 会把键按字节序重排，生成物和手写源对不上、diff 很乱）。
///
/// 顺序规则（合并时也照这条走）：
/// * 读入：按源文件出现顺序 `insert`；
/// * 写一个**已存在**的键：`IndexMap::insert` 只换值，**键留在原位**；
/// * 写一个**新**键：追加到**尾部**（`__include` 的 overrides、`__patch`/`__merge`
///   带进来的新键都走这条，见 [`super::compiler::merge_tree`]）；
/// * 删除（`strip_build_info`）用 `shift_remove` 保住其余键的相对顺序。
pub(super) type MapRef = Rc<RefCell<IndexMap<String, Item>>>;
pub(super) type ListRef = Rc<RefCell<Vec<Item>>>;

/// 配置树节点。对应 C++ 的 `an<ConfigItem>` 所指内容。
#[derive(Clone, Debug)]
pub enum Item {
    /// 空节点 / 空指针。
    Null,
    Scalar(String),
    List(ListRef),
    Map(MapRef),
}

impl Default for Item {
    fn default() -> Self {
        Item::Null
    }
}

impl Item {
    pub(super) fn scalar(value: impl Into<String>) -> Self {
        Item::Scalar(value.into())
    }

    pub(super) fn list() -> Self {
        Item::List(Rc::new(RefCell::new(Vec::new())))
    }

    pub(super) fn map() -> Self {
        Item::Map(Rc::new(RefCell::new(IndexMap::new())))
    }

    pub(super) fn value_type(&self) -> ValueType {
        match self {
            Item::Null => ValueType::Null,
            Item::Scalar(_) => ValueType::Scalar,
            Item::List(_) => ValueType::List,
            Item::Map(_) => ValueType::Map,
        }
    }

    pub(super) fn is_null(&self) -> bool {
        matches!(self, Item::Null)
    }

    pub(super) fn is_scalar(&self) -> bool {
        matches!(self, Item::Scalar(_))
    }

    pub(super) fn is_list(&self) -> bool {
        matches!(self, Item::List(_))
    }

    pub(super) fn is_map(&self) -> bool {
        matches!(self, Item::Map(_))
    }

    /// `ConfigItem::empty()`。空指针按空处理（C++ 那几处调用点会解引用空指针，
    /// 实际输入不会走到；这里取安全且等价于"空"的语义）。
    pub(super) fn empty(&self) -> bool {
        match self {
            Item::Null => true,
            Item::Scalar(value) => value.is_empty(),
            Item::List(list) => list.borrow().is_empty(),
            Item::Map(map) => map.borrow().is_empty(),
        }
    }

    pub(super) fn as_scalar(&self) -> Option<&str> {
        if let Item::Scalar(value) = self {
            Some(value)
        } else {
            None
        }
    }

    pub fn as_list(&self) -> Option<ListRef> {
        if let Item::List(list) = self {
            Some(list.clone())
        } else {
            None
        }
    }

    pub fn as_map(&self) -> Option<MapRef> {
        if let Item::Map(map) = self {
            Some(map.clone())
        } else {
            None
        }
    }

    /// `ConfigMap::Get` 的读法：不存在或值为空指针都算"没有"。
    pub(super) fn get(&self, key: &str) -> Item {
        self.as_map()
            .and_then(|map| map.borrow().get(key).cloned())
            .unwrap_or(Item::Null)
    }

    pub(super) fn get_at(&self, index: usize) -> Item {
        self.as_list()
            .and_then(|list| list.borrow().get(index).cloned())
            .unwrap_or(Item::Null)
    }

    // ---- ConfigValue 的标量访问器（config_types.cc 34-111 行） ----

    /// `ConfigValue::GetBool`：只认 `true` / `false`（大小写不敏感）。
    pub(super) fn get_bool(&self) -> Option<bool> {
        let value = self.as_scalar()?;
        if value.is_empty() {
            return None;
        }
        let lower = value.to_ascii_lowercase();
        match lower.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        }
    }

    /// `ConfigValue::GetInt`：先试 `0x` 开头的十六进制（要求整串消费），
    /// 再退回 `std::stoi`（允许前导空白、正负号，多余后缀忽略）。
    pub(super) fn get_int(&self) -> Option<i32> {
        let value = self.as_scalar()?;
        if value.is_empty() {
            return None;
        }
        if let Some(hex_part) = value.strip_prefix("0x") {
            if !hex_part.is_empty() && hex_part.bytes().all(|b| b.is_ascii_hexdigit()) {
                let parsed = u32::from_str_radix(hex_part, 16).ok()?;
                return Some(parsed as i32);
            }
        }
        parse_stoi_prefix(value)
    }

    /// `ConfigValue::GetDouble`：`std::stod` 的前缀解析语义。
    pub(super) fn get_double(&self) -> Option<f64> {
        let value = self.as_scalar()?;
        if value.is_empty() {
            return None;
        }
        parse_stod_prefix(value)
    }

    pub(super) fn get_string(&self) -> Option<String> {
        self.as_scalar().map(str::to_string)
    }
}

/// `std::stoi`：跳过前导空白，取可选正负号 + 十进制数字前缀；无数字或溢出返回 None。
fn parse_stoi_prefix(value: &str) -> Option<i32> {
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let start = i;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digits_start {
        return None;
    }
    value[start..i].parse::<i32>().ok()
}

/// `std::stod`：跳过前导空白，取可选正负号 + 数字/小数点/指数前缀。
fn parse_stod_prefix(value: &str) -> Option<f64> {
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let start = i;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let mut has_digit = false;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
        has_digit = true;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
            has_digit = true;
        }
    }
    if !has_digit {
        return None;
    }
    // 指数部分只在语法完整时纳入；否则 stod 在指数处停止。
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        let exp_start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp_start {
            i = j;
        }
    }
    value[start..i].parse::<f64>().ok()
}

/// `ConfigItemRef`：一个可读写的节点位置。
///
/// C++ 里它是抽象基类，派生类有 `ConfigDataRootRef` / `ConfigMapEntryRef` /
/// `ConfigListEntryRef` / `ConfigResource` / `ConfigCowRef`。
pub trait ItemRef {
    fn get_item(&self) -> Item;
    fn set_item(&self, item: Item);

    fn is_null(&self) -> bool {
        self.get_item().is_null()
    }

    fn is_value(&self) -> bool {
        self.get_item().is_scalar()
    }

    fn is_list(&self) -> bool {
        self.get_item().is_list()
    }

    fn is_map(&self) -> bool {
        self.get_item().is_map()
    }

    fn to_bool(&self) -> bool {
        self.get_item().get_bool().unwrap_or(false)
    }

    fn to_int(&self) -> i32 {
        self.get_item().get_int().unwrap_or(0)
    }

    fn to_double(&self) -> f64 {
        self.get_item().get_double().unwrap_or(0.0)
    }

    fn to_string(&self) -> String {
        self.get_item().get_string().unwrap_or_default()
    }

    /// `ConfigItemRef::AsList`：类型不符就**新建一个空列表并写回**。
    fn as_list(&self) -> ListRef {
        if let Some(list) = self.get_item().as_list() {
            return list;
        }
        let list: ListRef = Rc::new(RefCell::new(Vec::new()));
        self.set_item(Item::List(list.clone()));
        list
    }

    /// `ConfigItemRef::AsMap`：同上。
    fn as_map(&self) -> MapRef {
        if let Some(map) = self.get_item().as_map() {
            return map;
        }
        let map: MapRef = Rc::new(RefCell::new(IndexMap::new()));
        self.set_item(Item::Map(map.clone()));
        map
    }

    fn clear(&self) {
        self.set_item(Item::Null);
    }

    fn append(&self, item: Item) -> bool {
        self.as_list().borrow_mut().push(item);
        true
    }

    fn size(&self) -> usize {
        self.get_item()
            .as_list()
            .map(|list| list.borrow().len())
            .unwrap_or(0)
    }

    fn has_key(&self, key: &str) -> bool {
        self.get_item()
            .as_map()
            .map(|map| map.borrow().get(key).map_or(false, |item| !item.is_null()))
            .unwrap_or(false)
    }
}

/// `ConfigMapEntryRef`：直接抓住 map 容器 + key。
pub struct MapEntryRef {
    map: MapRef,
    key: String,
}

impl MapEntryRef {
    pub(super) fn new(map: MapRef, key: impl Into<String>) -> Self {
        Self {
            map,
            key: key.into(),
        }
    }
}

impl ItemRef for MapEntryRef {
    fn get_item(&self) -> Item {
        self.map
            .borrow()
            .get(&self.key)
            .cloned()
            .unwrap_or(Item::Null)
    }

    fn set_item(&self, item: Item) {
        self.map.borrow_mut().insert(self.key.clone(), item);
    }
}

/// `ConfigListEntryRef`：直接抓住 list 容器 + 下标。
pub struct ListEntryRef {
    list: ListRef,
    index: usize,
}

impl ListEntryRef {
    pub(super) fn new(list: ListRef, index: usize) -> Self {
        Self { list, index }
    }
}

impl ItemRef for ListEntryRef {
    fn get_item(&self) -> Item {
        self.list
            .borrow()
            .get(self.index)
            .cloned()
            .unwrap_or(Item::Null)
    }

    fn set_item(&self, item: Item) {
        let mut list = self.list.borrow_mut();
        // ConfigList::SetAt：越界时 resize(i + 1)
        if self.index >= list.len() {
            list.resize(self.index + 1, Item::Null);
        }
        list[self.index] = item;
    }
}
