//! 方案清单的生成 —— **部署时把程序文件夹识别到的方案写进用户 `schema/schema.yaml`**。
//!
//! ## 为什么需要这一步
//!
//! 清单的唯一落点是 `<user>/schema/schema.yaml`（**点名制**：`active` + `schemas` 列表）。
//! 首次安装时它根本不存在，而部署又必须知道要编译哪些方案。所以部署第一步是
//! **先造清单**：识别程序文件夹（`shared_data_dir`）里所有过 admit 四关的方案，点名进去。
//!
//! ## 规则（2026-10-28 定）
//!
//! * **只认 `schema/` 下的方案**：根目录的散文件、根目录下的文件夹、其它分类目录
//!   （`eassy/` `theme/` `model/`…）一概不认 —— 我们只扫 `<数据根>/schema/**`；
//! * **四关全过才收**：① 文件名形态 → ② 反推路径与实际路径比对 → ③ 头标记 + 字节实测
//!   → ④ `schema.id` 单数写法/内容/与文件名一致。任一关挂掉就**抛弃**，
//!   不进清单（半成品方案进列表，用户一点就白屏，比进不去更糟）；
//! * **清单已存在且非空 → 原样保留**。用户的选择是权威，部署不覆盖、不重排、不补漏；
//!   只有清单**缺失或为空**才重新生成；
//! * **`default.yaml` 的 `schema_list` 不再当兜底**：清单直接生成，不过 default。
//!
//! ## 顺序与 `active`
//!
//! `schemas` 按 id 字典序（确定性，与遍历顺序无关）；`active` 优先挑 `pinyin`
//! （仓库的标准样板），没有就取第一个。**首装的强制"至少选一个"是向导的事**，
//! 这里只保证清单不为空、能立刻打字。

use std::path::{Path, PathBuf};

use super::admit::{self, AdmitReport};
use super::deployment_tasks::{SchemaManifest, load_schema_manifest};
use super::paths::SCHEMA_DIR;
use super::tree::{log_info, log_warn};

/// 一个被识别（过四关）的方案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    /// 方案 id（`schema.id`，与文件名一致）。
    pub id: String,
    /// `schema.name`（人类可读名，可能没有）。
    pub name: Option<String>,
    /// 源文件路径（程序文件夹下）。
    pub path: PathBuf,
}

/// 一次扫描的结果：收下的 + 抛弃的（带原因，便于排查）。
#[derive(Debug, Default)]
pub struct DiscoverResult {
    /// 过四关、可进清单的方案（按 id 字典序）。
    pub accepted: Vec<Discovered>,
    /// 被抛弃的方案：`(路径, 第一条失败原因)`。
    pub rejected: Vec<(PathBuf, String)>,
}

impl DiscoverResult {
    /// 收下的 id 列表（按字典序）。
    pub fn ids(&self) -> Vec<String> {
        self.accepted.iter().map(|d| d.id.clone()).collect()
    }

    /// 报告里第一条失败原因（没有就给个兜底文案）。
    fn reject_reason(report: &AdmitReport) -> String {
        report
            .items
            .iter()
            .find(|item| !item.ok)
            .map(|item| format!("{}：{}", item.name, item.detail))
            .unwrap_or_else(|| "未知原因".to_string())
    }
}

/// 递归收集 `dir` 下所有 `*.schema.yaml`（只走 `schema/` 分类目录）。
///
/// 结果按路径排序 —— 保证同一份数据目录每次扫出的顺序一致。
fn collect_schema_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_schema_files(&path, out);
            continue;
        }
        let is_schema = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".schema.yaml"));
        if is_schema {
            out.push(path);
        }
    }
}

/// 扫描一个数据根（通常是程序文件夹的 `data`），返回过四关的方案 + 被抛弃的名单。
///
/// 只扫 `<数据根>/schema/**`；`schema` 目录不存在就返回空结果（不是错误 ——
/// 比如用户文件夹在首次部署时就是空的）。
pub fn discover(data_root: &Path) -> DiscoverResult {
    let mut result = DiscoverResult::default();
    let mut files = Vec::new();
    collect_schema_files(&data_root.join(SCHEMA_DIR), &mut files);
    for path in files {
        let report = admit::check_schema_file(&path);
        if !report.passed() {
            result
                .rejected
                .push((path.clone(), DiscoverResult::reject_reason(&report)));
            continue;
        }
        let Some(id) = report.schema_id.clone() else {
            // 过了检查却没 id 属内部错误，按抛弃处理，不进清单
            result
                .rejected
                .push((path.clone(), "通过了检查却取不到 id".to_string()));
            continue;
        };
        result.accepted.push(Discovered {
            id,
            name: report.schema_name.clone(),
            path,
        });
    }
    result.accepted.sort_by(|a, b| a.id.cmp(&b.id));
    result
}

/// 扫多个数据根并合并（程序文件夹 + 用户文件夹），id 去重（先到先得）。
pub fn discover_roots(roots: &[PathBuf]) -> DiscoverResult {
    let mut merged = DiscoverResult::default();
    let mut seen = std::collections::BTreeSet::new();
    for root in roots {
        let one = discover(root);
        for item in one.accepted {
            if seen.insert(item.id.clone()) {
                merged.accepted.push(item);
            }
        }
        merged.rejected.extend(one.rejected);
    }
    merged.accepted.sort_by(|a, b| a.id.cmp(&b.id));
    merged
}

/// 挑默认 `active`：`pinyin` 优先（标准样板），否则第一个。
fn pick_active(ids: &[String]) -> Option<String> {
    if ids.is_empty() {
        return None;
    }
    ids.iter()
        .find(|id| *id == "pinyin")
        .or_else(|| ids.first())
        .cloned()
}

/// 清单现状：已存在（原样返回）还是刚生成。
#[derive(Debug)]
pub enum EnsureOutcome {
    /// 清单本来就有内容 —— 用户的权威，一个字没动。
    Existing(SchemaManifest),
    /// 清单缺失/为空 —— 刚由程序文件夹识别生成。
    Generated(SchemaManifest),
}

impl EnsureOutcome {
    /// 不管哪种，返回清单本体。
    pub fn manifest(&self) -> &SchemaManifest {
        match self {
            EnsureOutcome::Existing(manifest) | EnsureOutcome::Generated(manifest) => manifest,
        }
    }

    /// 这次是不是新生成的。
    pub fn generated(&self) -> bool {
        matches!(self, EnsureOutcome::Generated(_))
    }
}

/// **确保清单存在**：有内容就不动，缺失/为空则由程序文件夹识别生成。
///
/// * `user_data_dir`：清单落点（`<user>/schema/schema.yaml`）。
/// * `shared_data_dir`：程序文件夹的数据目录（`data`），识别源。
///   用户文件夹自己也在扫描范围内（用户自制的方案同样该被点名），顺序是
///   **shared 优先**（出厂方案在前）。
pub fn ensure_manifest(user_data_dir: &Path, shared_data_dir: &Path) -> EnsureOutcome {
    let existing = load_schema_manifest(user_data_dir);
    if !existing.schemas.is_empty() {
        return EnsureOutcome::Existing(existing);
    }
    let found = discover_roots(&[shared_data_dir.to_path_buf(), user_data_dir.to_path_buf()]);
    let ids = found.ids();
    let manifest = SchemaManifest {
        active: pick_active(&ids),
        schemas: ids,
    };
    if manifest.schemas.is_empty() {
        // 一个都没识别出来：不写空清单（写了就等于把用户的清单清空），
        // 让调用方按"没有方案可用"处理并给出提示。
        return EnsureOutcome::Existing(manifest);
    }
    match admit::save_schema_manifest_with_header(user_data_dir, &manifest, GENERATED_HEADER) {
        Ok(path) => {
            log_info(&format!(
                "方案清单缺失，已由程序文件夹识别生成：{} 项 -> {}",
                manifest.schemas.len(),
                path.display()
            ));
            EnsureOutcome::Generated(manifest)
        }
        Err(error) => {
            log_warn(&format!("方案清单生成失败：{error}"));
            EnsureOutcome::Existing(manifest)
        }
    }
}

/// 生成的清单头注释（人看的；读入器不依赖）。
const GENERATED_HEADER: &str = "\
# 方案信息 —— 勾选清单 + 当前使用的方案。
# 由部署流程自动生成：识别程序文件夹里过准入检查的方案后写入。
# 之后由「方案选单设定」与输入法自己维护；手改请保持 `schemas` 是 id 列表。
";

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("librime-next-schema-list-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// 造一份合规方案：UTF-8 无 BOM、LF、三行标准头、`schema.id` 单数、摆放正确。
    fn good_schema(root: &Path, id: &str, name: &str) -> PathBuf {
        let dir = root.join(SCHEMA_DIR).join(id.replace('.', "/"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.schema.yaml"));
        std::fs::write(
            &path,
            format!(
                "# Rime schema\n# encode: utf-8\n# newline: lf\n---\nschema:\n  id: {id}\n  \
                 name: {name}\n...\n"
            ),
        )
        .unwrap();
        path
    }

    /// 造一份**不合格**方案：`schema_id` 复数写法（方言不纯）。
    fn bad_schema_native_id(root: &Path, id: &str) -> PathBuf {
        let dir = root.join(SCHEMA_DIR).join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.schema.yaml"));
        std::fs::write(
            &path,
            format!(
                "# Rime schema\n# encode: utf-8\n# newline: lf\n---\nschema:\n  schema_id: \
                 {id}\n  name: 坏方案\n...\n"
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn discovers_only_admitted_schemas() {
        let shared = temp("discover");
        good_schema(&shared, "pinyin", "拼音");
        good_schema(&shared, "wubi86", "五笔");
        bad_schema_native_id(&shared, "broken");

        let result = discover(&shared);
        assert_eq!(result.ids(), vec!["pinyin", "wubi86"], "只收过四关的");
        assert_eq!(result.rejected.len(), 1, "复数 schema_id 的那个要被抛弃");
        assert!(result.rejected[0].1.contains("schema_id"), "原因要写明是写法问题");
        let _ = std::fs::remove_dir_all(&shared);
    }

    #[test]
    fn root_level_schema_files_are_ignored() {
        let shared = temp("root-level");
        // 根目录散文件、根目录文件夹里的方案 —— 一概不认
        std::fs::create_dir_all(shared.join("pinyin")).unwrap();
        std::fs::write(
            shared.join("pinyin.schema.yaml"),
            "# Rime schema\n# encode: utf-8\n# newline: lf\n---\nschema:\n  id: pinyin\n...\n",
        )
        .unwrap();
        std::fs::write(
            shared.join("pinyin/pinyin.schema.yaml"),
            "# Rime schema\n# encode: utf-8\n# newline: lf\n---\nschema:\n  id: pinyin\n...\n",
        )
        .unwrap();
        // 正确摆法
        good_schema(&shared, "wubi86", "五笔");

        let result = discover(&shared);
        assert_eq!(result.ids(), vec!["wubi86"], "只认 schema/ 下的正确摆法");
        let _ = std::fs::remove_dir_all(&shared);
    }

    #[test]
    fn series_schema_is_discovered() {
        let shared = temp("series");
        good_schema(&shared, "yuling.five", "宇浩·五笔");
        let result = discover(&shared);
        assert_eq!(result.ids(), vec!["yuling.five"]);
        let _ = std::fs::remove_dir_all(&shared);
    }

    #[test]
    fn misplaced_series_is_rejected() {
        let shared = temp("misplaced");
        // 系列摆错：id 是 a.b，却直接放在 schema/a.b/ 下（应该 schema/a/b/）
        let dir = shared.join(SCHEMA_DIR).join("a.b");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.b.schema.yaml"),
            "# Rime schema\n# encode: utf-8\n# newline: lf\n---\nschema:\n  id: a.b\n...\n",
        )
        .unwrap();
        let result = discover(&shared);
        assert!(result.accepted.is_empty(), "摆放不对就是不合格");
        assert_eq!(result.rejected.len(), 1);
        let _ = std::fs::remove_dir_all(&shared);
    }

    #[test]
    fn generates_manifest_when_missing() {
        let root = temp("gen");
        let shared = root.join("shared");
        let user = root.join("user");
        good_schema(&shared, "pinyin", "拼音");
        good_schema(&shared, "double_pinyin_flypy", "小鹤双拼");

        let outcome = ensure_manifest(&user, &shared);
        assert!(outcome.generated(), "清单缺失应生成");
        let manifest = outcome.manifest();
        assert_eq!(manifest.schemas, vec!["double_pinyin_flypy", "pinyin"]);
        assert_eq!(manifest.active.as_deref(), Some("pinyin"), "active 优先 pinyin");

        // 落盘了，且能被读回来
        let written = user.join(SCHEMA_DIR).join("schema.yaml");
        assert!(written.is_file(), "清单要写到 <user>/schema/schema.yaml");
        let reread = load_schema_manifest(&user);
        assert_eq!(reread.schemas, manifest.schemas);
        assert_eq!(reread.active.as_deref(), Some("pinyin"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn keeps_existing_manifest_untouched() {
        let root = temp("keep");
        let shared = root.join("shared");
        let user = root.join("user");
        good_schema(&shared, "pinyin", "拼音");
        good_schema(&shared, "wubi86", "五笔");

        // 用户自己只勾了 wubi86，并且 active 就是它
        let mine = SchemaManifest {
            active: Some("wubi86".into()),
            schemas: vec!["wubi86".into()],
        };
        admit::save_schema_manifest(&user, &mine).unwrap();

        let outcome = ensure_manifest(&user, &shared);
        assert!(!outcome.generated(), "已有清单不许覆盖");
        assert_eq!(outcome.manifest().schemas, vec!["wubi86"], "用户的选择是权威");
        assert_eq!(outcome.manifest().active.as_deref(), Some("wubi86"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_shared_generates_nothing() {
        let root = temp("empty");
        let shared = root.join("shared");
        let user = root.join("user");
        std::fs::create_dir_all(&shared).unwrap();

        let outcome = ensure_manifest(&user, &shared);
        assert!(!outcome.generated(), "一个方案都没有时不写空清单");
        assert!(!user.join(SCHEMA_DIR).join("schema.yaml").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn user_dir_schemas_are_picked_up_too() {
        let root = temp("user-own");
        let shared = root.join("shared");
        let user = root.join("user");
        good_schema(&shared, "pinyin", "拼音");
        good_schema(&user, "my_own", "自制方案");

        let outcome = ensure_manifest(&user, &shared);
        assert_eq!(
            outcome.manifest().schemas,
            vec!["my_own", "pinyin"],
            "用户自制的方案同样该被点名"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
