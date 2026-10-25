//! lever 层的**数据文件读写层**：`installation.yaml` / `user.yaml` / `*.custom.yaml` /
//! 已编译的 `build/*.yaml` 都要"读进来 → 改几个键 → 写回去"。
//!
//! ## 为什么 lever 需要自己这一层（而不是直接用 `config::Config`）
//!
//! C++ 里这些活都是 `Config` 干的：`Config::LoadFromFile` / `SetString` / `SaveToFile`。
//! Rust 侧的 [`crate::config::Config`] 已经移植了读写**访问器**，但
//! `load_from_file` / `save_to_file` / `set_*` / `get_item` 目前是 `config` 模块私有
//! （见 `config.rs` 顶部"可见性理由"），`lever` 作为兄弟模块调不到。
//!
//! 于是这里用 config 层**公开**的三个入口拼出最小读写层：
//!
//! | 动作 | 用的公开入口 | 对应 C++ |
//! |---|---|---|
//! | 读 | [`crate::load_file`]（`config::load::load_file`） | `ConfigData::LoadFromFile` 的读 |
//! | 树类型 | `serde_json::Value`（`preserve_order`，键序 = 源文件序） | `an<ConfigItem>` |
//! | 写 | [`crate::config::to_yaml_string`]（`emit`，全 crate 唯一出口） | `ConfigData::EmitYaml` |
//!
//! 注意**写**这一步中间要过一次 [`crate::config::convert_from_yaml`]（`Value → Item`），
//! 且 `compiler` 传 `None`：C++ 的 `Config` 组件（`ConfigLoader`）加载时**不挂编译器插件**，
//! 所以 `__include` / `__patch` 这些键在数据文件里就是普通键，不会被解释掉。这里照抄。
//!
//! ## 与 C++ 的差异
//!
//! * `Config` 有"改过才写"（`modified` / `auto_save`）语义；这里 `save` 一律落盘，
//!   是否该写由调用方（各任务）自己判断 —— 任务本来就都有"要不要更新"的前置判断。
//! * `ctime` 格式是**本地时间**；本 crate 不许加依赖（`chrono` 之类），
//!   [`ctime`] 只能给 UTC。`install_time` / `update_time` 只给人看、不参与判断，
//!   但这条差异写在模块文档里，免得以后有人拿它对时间。
//! * `Checksum`（`algo/utilities.cc` 的 CRC32）在 algo 层是私有的，这里按同一张表重算，
//!   单测用标准向量 `"123456789" → 0xCBF43926` 锚死；等 algo 把它公开就能删掉这份。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Rime 配置树的"半成品"类型：`serde_json::Value`（crate 开了 `preserve_order`，
/// 所以 `Object` 的键序 = 源文件里的键序）。
pub type Tree = serde_json::Value;

/// 读一份 YAML。文件不存在 / 解析失败都返回 `None`（C++ 的 `LoadFromFile` 返回 false）。
pub fn load(path: &Path) -> Option<Tree> {
    if !path.exists() {
        return None;
    }
    match crate::load_file(path) {
        Ok(tree) => Some(tree),
        Err(error) => {
            log_error(&format!("Error parsing YAML: {error}"));
            None
        }
    }
}

/// 写一份 YAML（走 `config::emit` 这个唯一出口）。父目录不存在时先建出来
/// （`save_output_plugin.cc` 的 Weasel-Next 补丁也是这么做的）。
pub fn save(tree: &Tree, path: &Path) -> bool {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    let item = crate::config::convert_from_yaml(tree, None);
    match crate::config::to_yaml_string(&item) {
        Ok(text) => {
            log_info(&format!("saving config file '{}'.", path.display()));
            std::fs::write(path, text).is_ok()
        }
        Err(error) => {
            log_error(&format!("failed to emit YAML: {error}"));
            false
        }
    }
}

// ---------------------------------------------------------------------------
// 路径访问（`ConfigData::Traverse` 的只用得上的那半：`a/b/c` 与 `@n`）
// ---------------------------------------------------------------------------

/// `ConfigData::SplitPath`：去掉前导 `/` 后按 `/` 切。
fn split_path(node_path: &str) -> Vec<&str> {
    node_path.trim_start_matches('/').split('/').collect()
}

fn is_list_index(key: &str) -> bool {
    key.len() > 1 && key.as_bytes()[0] == b'@' && key.as_bytes()[1].is_ascii_digit()
}

fn index_of(key: &str) -> usize {
    key[1..].parse::<usize>().unwrap_or(0)
}

/// `ConfigData::Traverse`（只读版）。`@n` 走列表下标，其余走映射键。
pub fn get<'a>(tree: &'a Tree, node_path: &str) -> Option<&'a Tree> {
    let mut node = tree;
    for key in split_path(node_path) {
        node = if is_list_index(key) {
            node.as_array()?.get(index_of(key))?
        } else {
            node.as_object()?.get(key)?
        };
    }
    Some(node)
}

/// `Config::GetString`：标量才返回值（C++ 的 `As<ConfigValue>`；列表/映射一律失败）。
pub fn get_string(tree: &Tree, node_path: &str) -> Option<String> {
    match get(tree, node_path)? {
        Tree::String(value) => Some(value.clone()),
        // yaml-cpp 的 `as<string>()` 对数字/布尔也给字符串；树里的标量在 C++ 侧一律是
        // 字符串，这里把 JSON 标量也当字符串读，行为对齐 `ConfigValue::GetString`
        Tree::Number(value) => Some(value.to_string()),
        Tree::Bool(value) => Some(if *value { "true" } else { "false" }.to_string()),
        _ => None,
    }
}

/// `Config::GetInt`：`std::stoi` 前缀语义（这里只做十进制前缀，没有 `0x` 分支 ——
/// 数据文件里没有十六进制的整型字段）。
pub fn get_int(tree: &Tree, node_path: &str) -> Option<i32> {
    let value = get_string(tree, node_path)?;
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

/// `Config::GetBool`：只认 `true` / `false`（大小写不敏感），与 `ConfigValue::GetBool` 一致。
pub fn get_bool(tree: &Tree, node_path: &str) -> Option<bool> {
    let value = get_string(tree, node_path)?;
    if value.is_empty() {
        return None;
    }
    match value.to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// 按 `/` 路径写一个标量（等价 `Config::SetString`）。沿途缺映射就补映射。
///
/// `Config::SetItem` 走 `TraverseCopyOnWrite`：**沿途节点类型不符就失败**（返回 false），
/// 这里照抄 —— 不会把已有的标量节点强改成映射。
pub fn set_scalar(tree: &mut Tree, node_path: &str, value: &str) -> bool {
    set_subtree(tree, node_path, Tree::String(value.to_string()))
}

/// 按 `/` 路径写一棵子树（等价 `Config::SetItem`）。
pub fn set_subtree(tree: &mut Tree, node_path: &str, value: Tree) -> bool {
    let keys = split_path(node_path);
    if keys.is_empty() {
        *tree = value;
        return true;
    }
    let mut node = tree;
    for key in &keys[..keys.len() - 1] {
        // 类型检查：已有节点必须是同类型容器，否则失败（C++ TypeCheckedCopyOnWrite）
        if node.is_null() {
            *node = Tree::Object(serde_json::Map::new());
        }
        let Some(map) = node.as_object_mut() else {
            log_error(&format!("copy on write failed; incompatible node type: {key}"));
            return false;
        };
        node = map
            .entry((*key).to_string())
            .or_insert_with(|| Tree::Object(serde_json::Map::new()));
    }
    let last = keys[keys.len() - 1];
    if node.is_null() {
        *node = Tree::Object(serde_json::Map::new());
    }
    let Some(map) = node.as_object_mut() else {
        log_error(&format!("copy on write failed; incompatible node type: {last}"));
        return false;
    };
    // `IndexMap::insert` 语义：已有键留在原位，新键追加尾部（与 config 层一致）
    map.insert(last.to_string(), value);
    true
}

/// 读 `schema_list`：C++ 是 `list<map{schema: id}>`，只取 `schema` 标量，非映射项跳过。
pub fn schema_list_ids(tree: &Tree, node_path: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let Some(list) = get(tree, node_path).and_then(Tree::as_array) else {
        return ids;
    };
    for item in list {
        // `As<ConfigMap>(*it)` → 只要映射；`item->GetValue("schema")` → 只要标量
        let Some(map) = item.as_object() else {
            continue;
        };
        let Some(value) = map.get("schema") else {
            continue;
        };
        match value {
            Tree::String(id) => ids.push(id.clone()),
            Tree::Number(number) => ids.push(number.to_string()),
            _ => {}
        }
    }
    ids
}

/// 造 `schema_list` 的树：`[{schema: id}, ...]`（`WorkspaceUpdate` / `SwitcherSettings::Select` 都要）。
pub fn schema_list_of_ids(ids: &[String]) -> Tree {
    Tree::Array(
        ids.iter()
            .map(|id| {
                let mut map = serde_json::Map::new();
                map.insert("schema".to_string(), Tree::String(id.clone()));
                Tree::Object(map)
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// 文件系统小工具（照抄 `deployment_tasks.cc` 里那几个 static 函数）
// ---------------------------------------------------------------------------

/// `MaybeCreateDirectory`（`deployment_tasks.cc:316`）：建目录；已存在算成功。
pub fn maybe_create_directory(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_ok() {
        return true;
    }
    if dir.exists() {
        return true;
    }
    log_error(&format!("error creating directory '{}'.", dir.display()));
    false
}

/// `fs::last_write_time` → Unix 秒（`filesystem::to_time_t`）。
pub fn modified_time(path: &Path) -> Option<i64> {
    crate::algo::file_modified_time(path)
}

/// `time(NULL)`。
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// `ctime(&now)` + `boost::trim`。
///
/// **与 C++ 的差异**：C 的 `ctime` 给的是**本地时间**，这里只能给 UTC（本 crate 不许
/// 加时区依赖）。生成格式逐字符对齐：`Www Mmm dd hh:mm:ss yyyy`，日 < 10 时前面补空格。
pub fn ctime(unix_seconds: i64) -> String {
    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = unix_seconds.div_euclid(86_400);
    let seconds_of_day = unix_seconds.rem_euclid(86_400);
    let hour = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let second = seconds_of_day % 60;
    // 1970-01-01 是周四；`days` 可能为负，先取欧几里得模
    let weekday = WEEKDAYS[days.rem_euclid(7) as usize];
    let (year, month, day) = civil_from_days(days);
    format!(
        "{weekday} {} {:>2} {hour:02}:{minute:02}:{second:02} {year}",
        MONTHS[(month - 1) as usize], day
    )
}

/// Howard Hinnant 的 `civil_from_days`（与 C 的 `gmtime` 同口径）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// 一个"够随机"的 UUID 字符串（8-4-4-4-12，v4 布局）。
///
/// C++ 用 `boost::uuids::random_generator`（`random_device`）。本 crate 不许加依赖，
/// 所以拿 `时间纳秒 + 进程号 + 一枚进程内自增计数器 + 栈地址` 凑 128 位再摆成 v4 的
/// 版本/变体位。用途只有"安装 id"，不用于安全场景。
pub fn generate_installation_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id() as u64;
    let local = 0u8;
    let address = &local as *const u8 as u64;
    // splitmix64 两轮，把 5 个来源摊成 128 位
    let mut state = nanos ^ (pid << 40) ^ (sequence << 17) ^ address;
    let mut mix = || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let high = mix();
    let low = mix();
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&high.to_be_bytes());
    bytes[8..].copy_from_slice(&low.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0F) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3F) | 0x80; // variant RFC 4122
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// `Checksum(file_path)`（`algo/utilities.cc:29`）：文件 CRC32；读不到按空输入（0）。
pub fn checksum(path: &Path) -> u32 {
    match std::fs::read(path) {
        Ok(bytes) => crc32(&bytes),
        Err(_) => 0,
    }
}

/// `boost::crc_32_type` 等价物（标准 IEEE CRC-32，反射，初值/终值 `0xFFFFFFFF`）。
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut remainder: u32 = 0xFFFF_FFFF;
    for &byte in bytes {
        remainder = (remainder >> 8) ^ CRC32_TABLE[((remainder ^ byte as u32) & 0xFF) as usize];
    }
    remainder ^ 0xFFFF_FFFF
}

static CRC32_TABLE: [u32; 256] = build_crc32_table();

const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// `boost::ends_with`。
pub fn ends_with(value: &str, suffix: &str) -> bool {
    value.ends_with(suffix)
}

/// `boost::starts_with`。
pub fn starts_with(value: &str, prefix: &str) -> bool {
    value.starts_with(prefix)
}

/// 目录里按名字排序的条目（C++ 的 `directory_iterator` 顺序是不确定的；排序只为
/// **测试可复现**，凡是"顺序有意义"的地方（`schema_list`）仍然只认源文件里的顺序）。
pub fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = read_dir.filter_map(Result::ok).map(|entry| entry.path()).collect();
    paths.sort();
    paths
}

/// 文件名（`path.filename()`）的 UTF-8 形式；非 UTF-8 返回空串（C++ 用 `to_utf8_string()`）。
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// 扩展名（`path.extension()`，带点）。
pub fn extension(path: &Path) -> String {
    path.extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_default()
}

// 日志：与 config 层同一条约定（`RIME_CONFIG_VERBOSE` 打开时打到 stderr）。
pub(crate) fn log_info(message: &str) {
    crate::config::data::log_info(message);
}

pub(crate) fn log_warn(message: &str) {
    crate::config::data::log_warn(message);
}

pub(crate) fn log_error(message: &str) {
    crate::config::data::log_error(message);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "librime-lever-tree-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn crc32_matches_boost_vector() {
        // boost::crc_32_type 的文档示例：CRC32("123456789") == 0xCBF43926
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
        let dir = temp("crc");
        std::fs::write(dir.join("a.txt"), b"123456789").unwrap();
        assert_eq!(checksum(&dir.join("a.txt")), 0xCBF4_3926);
        assert_eq!(checksum(&dir.join("missing")), 0, "读不到按空输入");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ctime_formats_like_c() {
        assert_eq!(ctime(0), "Thu Jan  1 00:00:00 1970");
        // ⚠️ 本函数算的是 **UTC**；C++ 的 `std::ctime` 是**本地时**。
        // 实测对得上：`%APPDATA%\Rime-Next\installation.yaml` 里 `install_time` 是
        // `"Fri Oct 16 20:03:34 2026"`（本地 +08），对应 UTC 的 12:03:34。
        // ⇒ 写 `install_time` 时会有 8 小时差（TODO: 需要本地时区转换，Rust std 没有）。
        assert_eq!(ctime(1_792_152_214), "Fri Oct 16 12:03:34 2026");
        assert_eq!(ctime(1_000_000_000), "Sun Sep  9 01:46:40 2001");
    }

    #[test]
    fn installation_id_looks_like_uuid_v4() {
        let id = generate_installation_id();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(
            parts.iter().map(|part| part.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
        assert_eq!(&id[14..15], "4", "版本位");
        assert!(["8", "9", "a", "b"].contains(&&id[19..20]), "变体位");
        assert_ne!(id, generate_installation_id(), "每次都不同");
    }

    #[test]
    fn tree_get_set_follows_config_semantics() {
        let mut tree = load(&{
            let dir = temp("tree");
            let path = dir.join("a.yaml");
            std::fs::write(&path, "var:\n  last_build_time: 42\nlist:\n  - x\n").unwrap();
            path
        })
        .expect("load");
        assert_eq!(get_int(&tree, "var/last_build_time"), Some(42));
        assert_eq!(get_string(&tree, "var/last_build_time").as_deref(), Some("42"));
        assert_eq!(get_bool(&tree, "var/last_build_time"), None);
        assert_eq!(get(&tree, "list/@0").and_then(Tree::as_str), Some("x"));
        // 新键追加尾部、已有键留在原位
        assert!(set_scalar(&mut tree, "var/previously_selected_schema", "luna"));
        let keys: Vec<&String> = tree.as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["var", "list"], "顶层键序不变");
        let var_keys: Vec<&String> = tree["var"].as_object().unwrap().keys().collect();
        assert_eq!(var_keys, vec!["last_build_time", "previously_selected_schema"]);
        assert!(set_scalar(&mut tree, "var/last_build_time", "43"));
        let var_keys: Vec<&String> = tree["var"].as_object().unwrap().keys().collect();
        assert_eq!(var_keys, vec!["last_build_time", "previously_selected_schema"], "已有键留原位");
        // 类型不符就失败（不会把标量强改成映射）
        assert!(!set_scalar(&mut tree, "var/last_build_time/x", "y"));
    }

    #[test]
    fn schema_list_roundtrip() {
        let ids = vec!["luna".to_string(), "stroke".to_string()];
        let list = schema_list_of_ids(&ids);
        assert_eq!(
            list,
            serde_json::json!([{"schema": "luna"}, {"schema": "stroke"}])
        );
        let mut tree = serde_json::json!({ "schema_list": list });
        assert_eq!(schema_list_ids(&tree, "schema_list"), ids);
        // 非映射项 / 缺 schema 键都跳过（C++ `As<ConfigMap>` + `GetValue("schema")`）
        *get_mut_for_test(&mut tree, "schema_list") =
            serde_json::json!([{"schema": "a"}, 1, {"other": "b"}, {}]);
        assert_eq!(schema_list_ids(&tree, "schema_list"), vec!["a".to_string()]);
    }

    fn get_mut_for_test<'a>(tree: &'a mut Tree, key: &str) -> &'a mut Tree {
        &mut tree[key]
    }

    #[test]
    fn save_then_load_keeps_key_order() {
        let dir = temp("save");
        let mut tree = serde_json::json!({
            "b": "1",
            "a": {"z": "true", "y": "x"},
        });
        assert!(set_scalar(&mut tree, "a/new_key", "v"));
        let path = dir.join("nested/out.yaml");
        assert!(save(&tree, &path), "父目录自动创建");
        let text = std::fs::read_to_string(&path).unwrap();
        let order: Vec<usize> = ["b:", "a:", "z:", "y:", "new_key:"]
            .iter()
            .map(|key| text.find(key).unwrap_or_else(|| panic!("缺 {key}")))
            .collect();
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "键序: {text}");
        let reloaded = load(&path).unwrap();
        assert_eq!(get_string(&reloaded, "a/z").as_deref(), Some("true"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
