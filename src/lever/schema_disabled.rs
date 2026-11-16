//! 方案「未勾选 / 显式禁用」状态（2026-11-11 用户再次拍板）。
//!
//! 修正后的原话：**「是不选 pinyin 的时候子方案禁用，不是 pinyin 也禁用了，那我怎么再次启动」**。
//!
//! 所以这里和 `deployment_tasks::load_schema_manifest` 读的 `schema.yaml` 的关系是：
//!
//! | 文件 | 含义 | 取消勾选 pinyin 时 |
//! |---|---|---|
//! | `<user>/schema/schema.yaml` | 勾选清单 + `active`（内部状态） | 只从 `schemas` 移除 `pinyin`；`active`、16 个子方案条目、全部 build 产物都留着 |
//! | `<user>/schema/disabled.yaml` | **显式**禁用（手工/兼容用） | **保持空**，不写 `pinyin` |
//! | `<user>/build/**` | 已编译产物 | 不主动删 |
//!
//! 父方案未勾选时，子方案按 `schema.parent` 自动压住（不部署/不进菜单）；
//! 重新勾选父方案即全部恢复。父方案自己不是「被禁用」，随时可以再点回来。
//!
//! 文件格式与 `weasel_style::switcher::DisabledTable` 完全一致（两侧各自解析，
//! librime-next **不依赖** `weasel-style`，免得 DLL 被拖进 `weasel-common` 的重依赖）：
//!
//! ```yaml
//! groups:            # 兼容手工整组禁用：按 schema.parent 展开全部子方案
//!   - pinyin
//! schemas:           # 单独禁用：不展开；组禁用不会往这里塞子方案
//!   - pinyin.zhuyin
//! ```
//!
//! 正常停用拼音请**取消勾选 `schema.yaml` 里的 `pinyin`**，不要写这份文件；
//! 这样再次启动时只勾回 `pinyin`，内部勾选关系与菜单都原样恢复。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::paths::{DISABLED_SCHEMAS_FILE, SCHEMA_DIR};

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct DisabledTable {
    /// 整组禁用的父方案 id（例如 `pinyin`），子方案由 `schema.parent` 展开。
    pub groups: Vec<String>,
    /// 单独禁用的方案 id。
    pub schemas: Vec<String>,
}

impl DisabledTable {
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty() && self.schemas.is_empty()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.groups.iter().any(|g| g == id) || self.schemas.iter().any(|s| s == id)
    }
}

/// 独立禁用状态只在用户目录（设置归属用户；出厂共享根不写）。
pub fn disabled_path(user_data_dir: &Path) -> PathBuf {
    user_data_dir.join(SCHEMA_DIR).join(DISABLED_SCHEMAS_FILE)
}

/// 读 `disabled.yaml`。文件缺失、坏 YAML、旧版本空文件都按「没有禁用」处理，
/// 绝不能因为这份可选文件把用户锁在空菜单里。
pub fn read_disabled(user_data_dir: &Path) -> DisabledTable {
    let Ok(text) = fs::read_to_string(disabled_path(user_data_dir)) else {
        return DisabledTable::default();
    };
    serde_norway::from_str::<DisabledTable>(&text).unwrap_or_default()
}

/// 写 `disabled.yaml`（LF + UTF-8，顺序保真；**不碰 `schema.yaml`**）。
///
/// 主要是给 CLI / 回归测试用；设置界面走 `weasel_style::switcher::write_disabled`，
/// 两边格式一字不差。
pub fn write_disabled(user_data_dir: &Path, table: &DisabledTable) -> io::Result<()> {
    let path = disabled_path(user_data_dir);
    let mut out = String::from(
        "# 方案显式禁用列表 —— 正常停用 pinyin 请直接取消勾选 schema.yaml 里的 pinyin。\n\
         # 这里只记手工/兼容用的「不部署 / 不进菜单」：不删 schemas、不改 active、不删 build。\n\
         # groups: 手工整组禁用的父方案（子方案由 schema.parent 展开）。\n\
         # schemas: 单独禁用的方案 id（组禁用不会往这里塞子方案，保留逐项状态）。\n",
    );
    write_id_list(&mut out, "groups", &table.groups);
    write_id_list(&mut out, "schemas", &table.schemas);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, out)
}

fn write_id_list(out: &mut String, key: &str, ids: &[String]) {
    if ids.is_empty() {
        out.push_str(&format!("{key}: []\n"));
        return;
    }
    out.push_str(&format!("{key}:\n"));
    for id in ids {
        out.push_str(&format!("  - {}\n", yaml_scalar(id)));
    }
}

/// 纯数字开头的 id 必须加引号，否则 YAML 当数字读，`Vec<String>` 反序列化整份失败。
fn yaml_scalar(value: &str) -> String {
    let plain = !value.is_empty()
        && !value.as_bytes()[0].is_ascii_digit()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        && !matches!(
            value.to_ascii_lowercase().as_str(),
            "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "~"
        );
    if plain {
        value.to_string()
    } else {
        format!(
            "\"{}\"",
            value.replace('\\', "\\\\").replace('"', "\\\"")
        )
    }
}

/// 扫描 `<root>/schema/**/*.schema.yaml` 的 `schema.parent:` 单行声明。
///
/// 只为展开 `groups:` 服务；用户根同名覆盖出厂根（与 `list_schemas` 的规则一致）。
/// 不读 build、不读 dict，避免把 90MB 词库目录扫进来。
pub fn scan_parents(shared: &Path, user: &Path) -> BTreeMap<String, String> {
    let mut parents = BTreeMap::new();
    for root in [shared, user] {
        let base = root.join(SCHEMA_DIR);
        if !base.is_dir() {
            continue;
        }
        let mut stack = vec![base];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let Some(id) = name.strip_suffix(".schema.yaml") else {
                    continue;
                };
                let Ok(text) = fs::read_to_string(&path) else {
                    continue;
                };
                if let Some(parent) = read_parent(&text) {
                    parents.insert(id.to_string(), parent);
                }
            }
        }
    }
    parents
}

/// 行级解析 `schema:` 段里的 `parent:`。接受单行标量、引号和行尾 `#` 注释；
/// 块状标量不当父级（与 `Setting::schema_tree::read_declared_parent` 同语义）。
fn read_parent(text: &str) -> Option<String> {
    let mut in_schema = false;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if !in_schema {
            if trimmed.starts_with("schema:") && !trimmed.starts_with("schema/") {
                in_schema = true;
            }
            continue;
        }
        if !trimmed.is_empty() && !trimmed.starts_with(' ') && !trimmed.starts_with('\t') {
            break;
        }
        let stripped = trimmed.trim_start();
        let Some(rest) = stripped.strip_prefix("parent:") else {
            continue;
        };
        let raw = rest.trim().split(" #").next().unwrap_or("").trim();
        let value = raw
            .trim_matches('"')
            .trim_matches('\'')
            .trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// 把 `groups` 按 `parent:` 展开，和显式 `schemas` 合并成**当前生效**的禁用集合。
pub fn effective_disabled_ids_from_table(
    table: &DisabledTable,
    parents: &BTreeMap<String, String>,
) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = table.schemas.iter().cloned().collect();
    for group in &table.groups {
        out.insert(group.clone());
        for (child, parent) in parents {
            if parent == group {
                out.insert(child.clone());
            }
        }
    }
    out
}

/// 读盘 + 展开的**显式**禁用集合（不含「父未选」自动压住的子方案）。
pub fn effective_disabled_ids(user_data_dir: &Path, shared_data_dir: &Path) -> BTreeSet<String> {
    let table = read_disabled(user_data_dir);
    if table.groups.is_empty() {
        return table.schemas.into_iter().collect();
    }
    let parents = scan_parents(shared_data_dir, user_data_dir);
    effective_disabled_ids_from_table(&table, &parents)
}

/// 父方案未勾选时，子方案自动视为禁用（计算态，不落 `disabled.yaml`）。
///
/// 父方案自己不在集合里：它只是「当前没选」，随时能重新勾上，子方案随之恢复。
pub fn blocked_children_by_unselected_parents(
    selected: &BTreeSet<String>,
    parents: &BTreeMap<String, String>,
) -> BTreeSet<String> {
    parents
        .iter()
        .filter(|(_, parent)| !selected.contains(parent.as_str()))
        .map(|(child, _)| child.clone())
        .collect()
}

/// 显式禁用 ∪ 父未选压住的子方案 = 真正不部署/不进菜单的 id。
pub fn effective_disabled_ids_from_selection(
    selected: &BTreeSet<String>,
    table: &DisabledTable,
    parents: &BTreeMap<String, String>,
) -> BTreeSet<String> {
    let mut out = effective_disabled_ids_from_table(table, parents);
    out.extend(blocked_children_by_unselected_parents(selected, parents));
    out
}

/// 读盘 + 扫描版本：给只有路径的调用方（部署器 / C API）用。
pub fn excluded_ids(
    user_data_dir: &Path,
    shared_data_dir: &Path,
    selected: &BTreeSet<String>,
) -> BTreeSet<String> {
    let table = read_disabled(user_data_dir);
    let parents = scan_parents(shared_data_dir, user_data_dir);
    effective_disabled_ids_from_selection(selected, &table, &parents)
}

/// 从原始清单里剔除禁用 id，**保序**、不改入参。
///
/// 注意：返回的 `Vec` 只用于「这次编译什么 / 菜单写什么」；原 `schema.yaml` 的
/// `schemas` 必须保持原样，这正是「内部状态保留」。
pub fn filter_enabled(raw: &[String], disabled: &BTreeSet<String>) -> Vec<String> {
    raw.iter()
        .filter(|id| !disabled.contains(id.as_str()))
        .cloned()
        .collect()
}

/// 一步到位：`(可部署 id, 不部署集合)`。
pub fn enabled_ids(
    user_data_dir: &Path,
    shared_data_dir: &Path,
    raw: &[String],
) -> (Vec<String>, BTreeSet<String>) {
    let selected: BTreeSet<String> = raw.iter().cloned().collect();
    let disabled = excluded_ids(user_data_dir, shared_data_dir, &selected);
    (filter_enabled(raw, &disabled), disabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("librime-disabled-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn schema_file(root: &Path, id: &str, parent: Option<&str>) {
        let path = root
            .join(SCHEMA_DIR)
            .join(id.replace('.', "/"))
            .join(format!("{id}.schema.yaml"));
        let parent_line = parent
            .map(|p| format!("  parent: \"{p}\" # 父方案\n"))
            .unwrap_or_default();
        touch(
            &path,
            &format!("schema:\n  schema_id: {id}\n  name: {id}\n{parent_line}"),
        );
    }

    #[test]
    fn expands_groups_and_keeps_individual_disables_separate() {
        let root = temp("expand");
        let shared = root.join("shared");
        let user = root.join("user");
        schema_file(&shared, "pinyin", None);
        schema_file(&shared, "pinyin.luna", Some("pinyin"));
        schema_file(&shared, "pinyin.zrm", Some("pinyin"));
        // 用户根同名/新增方案优先：terra 只在用户根声明
        schema_file(&user, "pinyin.terra", Some("pinyin"));
        schema_file(&shared, "wubi86", None);

        let table = DisabledTable {
            groups: vec!["pinyin".to_string()],
            schemas: vec!["pinyin.zhuyin".to_string(), "wubi86".to_string()],
        };
        write_disabled(&user, &table).unwrap();

        let parents = scan_parents(&shared, &user);
        assert_eq!(parents.get("pinyin.luna").map(String::as_str), Some("pinyin"));
        assert_eq!(parents.get("pinyin.zrm").map(String::as_str), Some("pinyin"));
        assert_eq!(parents.get("pinyin.terra").map(String::as_str), Some("pinyin"));
        assert!(!parents.contains_key("pinyin"), "父方案没有 parent");

        let effective = effective_disabled_ids_from_table(&read_disabled(&user), &parents);
        let expected: BTreeSet<String> = [
            "pinyin",
            "pinyin.luna",
            "pinyin.zrm",
            "pinyin.terra",
            "pinyin.zhuyin",
            "wubi86",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        assert_eq!(
            effective, expected,
            "输入 groups=[pinyin] + schemas=[pinyin.zhuyin,wubi86] 应展开成 6 个 id"
        );

        // 读盘入口（走扫描）结果必须一致
        assert_eq!(effective_disabled_ids(&user, &shared), expected);

        // 启用组：只删 groups，schemas 里逐项禁用的两个还在
        let mut after = read_disabled(&user);
        after.groups.clear();
        write_disabled(&user, &after).unwrap();
        let restored = read_disabled(&user);
        assert!(restored.groups.is_empty());
        assert_eq!(
            restored.schemas,
            vec!["pinyin.zhuyin".to_string(), "wubi86".to_string()],
            "组禁用不能吞掉逐项禁用状态"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn unselected_parent_blocks_children_and_rechecking_restores_them() {
        let root = temp("unselected");
        let shared = root.join("shared");
        let user = root.join("user");
        schema_file(&shared, "pinyin", None);
        schema_file(&shared, "pinyin.luna", Some("pinyin"));
        schema_file(&shared, "pinyin.zrm", Some("pinyin"));
        schema_file(&shared, "cangjie.5", None);

        // 用户取消勾选 pinyin：schemas 里没有父 id，但 16 个子方案条目仍在。
        let unchecked: Vec<String> = ["pinyin.luna", "pinyin.zrm", "cangjie.5"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let parents = scan_parents(&shared, &user);
        let selected: BTreeSet<String> = unchecked.iter().cloned().collect();
        let excluded =
            effective_disabled_ids_from_selection(&selected, &DisabledTable::default(), &parents);
        let expected: BTreeSet<String> = ["pinyin.luna", "pinyin.zrm"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(excluded, expected, "父未选只压住子方案，不把父 id 写进禁用表");
        assert!(read_disabled(&user).is_empty(), "父未选不能落盘到 disabled.yaml");

        let (enabled, _) = enabled_ids(&user, &shared, &unchecked);
        assert_eq!(enabled, vec!["cangjie.5".to_string()]);

        // 重新勾选 pinyin：子方案全部恢复，不需要删任何 build 产物。
        let rechecked: Vec<String> = ["pinyin", "pinyin.luna", "pinyin.zrm", "cangjie.5"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let selected_again: BTreeSet<String> = rechecked.iter().cloned().collect();
        let excluded_again = effective_disabled_ids_from_selection(
            &selected_again,
            &DisabledTable::default(),
            &parents,
        );
        assert!(excluded_again.is_empty(), "重新勾选父方案后子方案应自动恢复");
        let (enabled_again, _) = enabled_ids(&user, &shared, &rechecked);
        assert_eq!(enabled_again, rechecked);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn filter_enabled_keeps_manifest_order_and_does_not_touch_input() {
        let raw: Vec<String> = [
            "pinyin",
            "pinyin.luna",
            "pinyin.zrm",
            "wubi86",
            "liangfen",
            "pinyin.terra",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        let disabled: BTreeSet<String> = ["pinyin", "pinyin.luna", "pinyin.zrm", "pinyin.terra"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let (enabled, _) = enabled_ids_for_test(&raw, &disabled);
        assert_eq!(enabled, vec!["wubi86".to_string(), "liangfen".to_string()]);
        assert_eq!(raw.len(), 6, "过滤绝不能改原始清单");
        assert_eq!(raw[0], "pinyin", "源清单的 active/勾选状态必须原样留在磁盘上");
    }

    fn enabled_ids_for_test(raw: &[String], disabled: &BTreeSet<String>) -> (Vec<String>, BTreeSet<String>) {
        (filter_enabled(raw, disabled), disabled.clone())
    }

    #[test]
    fn missing_and_broken_files_mean_all_enabled() {
        let root = temp("broken");
        assert!(read_disabled(&root).is_empty());
        assert!(effective_disabled_ids(&root, &root).is_empty());

        fs::create_dir_all(root.join(SCHEMA_DIR)).unwrap();
        touch(
            &disabled_path(&root),
            "groups: [pinyin\n schemas: ???\n",
        );
        assert_eq!(read_disabled(&root), DisabledTable::default(), "坏 YAML 不能把菜单锁死");
        assert!(effective_disabled_ids(&root, &root).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn numeric_id_roundtrips_with_quotes() {
        let root = temp("numeric");
        let table = DisabledTable {
            groups: Vec::new(),
            schemas: vec!["092".to_string(), "pinyin.zhuyin".to_string()],
        };
        write_disabled(&root, &table).unwrap();
        let text = fs::read_to_string(disabled_path(&root)).unwrap();
        assert!(text.contains("  - \"092\"\n"), "纯数字 id 必须加引号：\n{text}");
        assert_eq!(read_disabled(&root), table);
        let _ = fs::remove_dir_all(&root);
    }
}
