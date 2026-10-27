//! `librime/src/rime/resource.cc` 的**子集**：`ResourceResolver` 里 config 层用到的三个方法
//! （`ToResourceId` / `ToFilePath` / `ResolvePath`），外加 Weasel-Next 的分目录补丁。
//!
//! ## 为什么单独成模块
//!
//! C++ 里 `resource.cc` **不属于 `config/`**：它是 `rime/resource.{h,cc}` 的通用资源解析器，
//! 被 `config` / `dict` / `lever` 一起用。之前这三个方法塞在
//! `config/compiler.rs` 里（`ConfigCompiler` 顺带用它），归属是错的 —— 移植
//! `resource` 那层时会变成"两份 ResolvePath"，所以先按 C++ 的边界切出来。
//!
//! **这是 `src/rime/resource.cc` 的子集，完整版等 `resource` 那层单独移植。**
//! 缺口（都在本文件里显式标注）：
//!
//! | `resource.cc` 里的东西 | 这里有吗 |
//! |---|---|
//! | `ResourceType`（`name` / `prefix` / `suffix`，config 是 `{"config","",".yaml"}`） | 只留 `prefix`/`suffix` 字段，`name` 恒为 config 的 `"config"` |
//! | `ResourceResolver::root_path()` | 用 [`ResourceResolver::roots`]（多根搜索，CLI 要对拍多目录） |
//! | `FallbackResourceResolver` | 没有（多根 + override 已覆盖当前用法） |
//! | `.bin` / `.gram` / `.txt` / `.theme.toml` / `.lua` 的分目录规则 | 没有（那是 dict/lever 层的事） |
//!
//! ## 分目录规则（Weasel-Next 补丁版 `ResolvePath` 的子集，2026-10-25 系列文件夹）
//!
//! `.schema` / `.dict` 资源**只**在 `<root>/schema/<系列…>/<名字>.yaml`；staging 根
//! （`build/`）下是 `<root>/<系列…>/<名字>.yaml`；其余资源（`default.yaml` 等未归类
//! 配置）才在 `<root>/<名字>`。**逐条对应** `resource.cc` 的
//! `sfx == ".yaml" && (ends_with(".schema") || ends_with(".dict"))` 分支。
//!
//! 系列分层：stem 里的每个 '.' 对应一级文件夹，文件名始终是完整 id：
//! `yuling` → `schema/yuling/yuling.schema.yaml`；
//! `yuling.five` → `schema/yuling/five/yuling.five.schema.yaml`。
//! id 含 '/' 时视为显式相对路径，不套分类（与 C++ `is_explicit_path_id` 同规则）。
//!
//! 2026-10-25：**删掉全部扁平回退**（原先 nested 不存在时退回 `<root>/<名字>`，还
//! 多试一档 `<root>/<名字>/` 给夹具用）—— 与 C++ 补丁同步：有回退就会继续长出
//! 一坨扁平文件，布局等于没分。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// config 层的资源解析器（`resource.cc` 的 `ResourceResolver`，`Type = {"config", "", ".yaml"}`）。
pub struct ResourceResolver {
    /// 搜索根，按优先级排列（librime 是 `[user_data_dir, shared_data_dir]`）。
    roots: Vec<PathBuf>,
    /// 钉死的 `resource_id -> 路径`（CLI 用：保证编译的就是用户给的那一份）。
    overrides: BTreeMap<String, PathBuf>,
    /// `ResourceType::prefix`，config 层恒为空。
    prefix: String,
    /// `ResourceType::suffix`，config 层恒为 `.yaml`。
    suffix: String,
}

impl ResourceResolver {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            overrides: BTreeMap::new(),
            prefix: String::new(),
            suffix: ".yaml".to_string(),
        }
    }

    /// 给某个 resource id 钉死一个路径（CLI 用：保证编译的就是用户给的那一份）。
    pub(super) fn add_override(&mut self, resource_id: impl Into<String>, path: impl Into<PathBuf>) {
        self.overrides.insert(resource_id.into(), path.into());
    }

    /// `ResourceResolver::ToResourceId`：去掉 prefix / suffix。
    pub(super) fn to_resource_id(&self, file_path: &str) -> String {
        let generic = file_path.replace('\\', "/");
        let has_prefix = generic.starts_with(&self.prefix);
        let has_suffix = generic.ends_with(&self.suffix);
        let start = if has_prefix { self.prefix.len() } else { 0 };
        let end = generic.len() - if has_suffix { self.suffix.len() } else { 0 };
        generic[start..end.max(start)].to_string()
    }

    /// `ResourceResolver::ToFilePath`：缺 prefix / suffix 就补上。
    fn to_file_path(&self, resource_id: &str) -> String {
        let has_parent = Path::new(resource_id)
            .parent()
            .is_some_and(|parent| !parent.as_os_str().is_empty());
        let missing_prefix = !has_parent && !resource_id.starts_with(&self.prefix);
        let missing_suffix = !resource_id.ends_with(&self.suffix);
        format!(
            "{}{}{}",
            if missing_prefix { &self.prefix } else { "" },
            resource_id,
            if missing_suffix { &self.suffix } else { "" }
        )
    }

    /// `ResourceResolver::ResolvePath`：按 roots 顺序找，找不到就返回第一个候选（与 C++ 一致：
    /// 不存在也返回路径，由调用方 `exists()` 判失败）。
    ///
    /// 2026-10-25：`.schema` / `.dict` 资源**永不回退扁平** —— 只认
    /// `schema/<系列…>/<名字>.yaml`（staging 根为 `<系列…>/<名字>.yaml`）。
    /// 2026-10-25 系列文件夹：stem 里的 '.' 对应一级文件夹；id 含 '/' 时视为
    /// 显式相对路径，不套分类。
    pub(super) fn resolve_path(&self, resource_id: &str) -> PathBuf {
        if let Some(path) = self.overrides.get(resource_id) {
            return path.clone();
        }
        let base = self.to_file_path(resource_id);
        let stem = schema_stem(resource_id);
        let explicit = resource_id.contains('/') || resource_id.contains('\\');
        let mut first: Option<PathBuf> = None;
        for root in &self.roots {
            let candidate = if explicit {
                root.join(&base)
            } else {
                resolve_in_root(root, &base, stem.as_deref())
            };
            if first.is_none() {
                first = Some(candidate.clone());
            }
            if candidate.exists() {
                return candidate;
            }
        }
        // 2026-10-27 方言回退：数据根下**递归**找同名文件。parse 数据住
        // `parser\<大类>\` 子文件夹（如 `parser\pinyin\pinyin.parser.yaml`），
        // 方案里只写 `import_preset: pinyin.parser`，不做全名路径拼写。
        let normalized = base.replace('\\', "/");
        let file_name = normalized.rsplit('/').next().unwrap_or(normalized.as_str());
        for root in &self.roots {
            if let Some(found) = find_file_recursive(root, file_name) {
                return found;
            }
        }
        first.unwrap_or_else(|| PathBuf::from(&base))
    }

    fn roots(&self) -> &[PathBuf] {
        &self.roots
    }
}

/// `.schema` / `.dict` 资源的目录名（`<名字>`）。
fn schema_stem(resource_id: &str) -> Option<String> {
    if resource_id.ends_with(".schema") || resource_id.ends_with(".dict") {
        let stem = resource_id
            .rsplit_once('.')
            .map(|(head, _)| head)
            .unwrap_or(resource_id);
        Some(stem.to_string())
    } else {
        None
    }
}

/// `resource.cc` 里的 staging 根判定（`build/`，兼容尾斜杠写法）。
fn is_staging_root(root: &Path) -> bool {
    let generic = root.to_string_lossy().replace('\\', "/");
    let trimmed = generic.trim_end_matches('/');
    root.file_name().is_some_and(|name| name == "build") || trimmed.ends_with("/build")
}

/// 单根内解析：`.schema` / `.dict` 资源**永远**走分类路径 —— 源根是
/// `<root>/schema/<系列…>/<方案>.yaml`，staging 根（`build/`）是
/// `<root>/<系列…>/<方案>.yaml`。**没有扁平回退**（2026-10-25 删，与 C++ 补丁同步：
/// 有回退就会继续长出一坨扁平文件）。其余资源保持 `<root>/<名字>`。
/// 系列分层（2026-10-25）：stem 里的 '.' 对应一级文件夹（yuling.five → yuling/five）。
fn resolve_in_root(root: &Path, base: &str, schema_stem: Option<&str>) -> PathBuf {
    let Some(stem) = schema_stem else {
        return root.join(base);
    };
    let series = stem.replace('.', "/");
    let subdir = if is_staging_root(root) {
        series
    } else {
        format!("schema/{series}")
    };
    root.join(&subdir).join(base)
}

/// 在 `root` 下递归找文件名精确等于 `file_name` 的第一个文件（广度优先）。
/// 给方言的 `import_preset: <名字>` 用：parse 数据可以住任意子文件夹。
fn find_file_recursive(root: &Path, file_name: &str) -> Option<PathBuf> {
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(root.to_path_buf());
    while let Some(dir) = queue.pop_front() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut subdirs = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                subdirs.push(path);
            } else if path.file_name().and_then(|n| n.to_str()) == Some(file_name) {
                return Some(path);
            }
        }
        // 目录优先按名字序，保证结果稳定
        subdirs.sort();
        for dir in subdirs {
            queue.push_back(dir);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_id_and_file_path_roundtrip() {
        let resolver = ResourceResolver::new(vec![PathBuf::from(".")]);
        assert_eq!(
            resolver.to_resource_id("yuling.schema.yaml"),
            "yuling.schema"
        );
        assert_eq!(resolver.to_resource_id("dir\\yuling.yaml"), "dir/yuling");
        assert_eq!(resolver.to_file_path("yuling.schema"), "yuling.schema.yaml");
        assert_eq!(resolver.to_file_path("yuling.yaml"), "yuling.yaml");
        // 带父目录的不补 prefix（config 的 prefix 是空串，这条只验证判定方向）
        assert_eq!(resolver.to_file_path("sub/x"), "sub/x.yaml");
    }

    #[test]
    fn schema_stem_only_for_schema_and_dict() {
        assert_eq!(schema_stem("yuling.schema").as_deref(), Some("yuling"));
        assert_eq!(schema_stem("stroke.dict").as_deref(), Some("stroke"));
        assert_eq!(schema_stem("default"), None);
    }

    #[test]
    fn series_folders_for_dotted_stems() {
        let resolver = ResourceResolver::new(vec![PathBuf::from("C:/data")]);
        // stem 里的 '.' 对应一级文件夹（2026-10-25 系列文件夹）
        assert_eq!(
            resolver.resolve_path("yuling.schema"),
            PathBuf::from("C:/data/schema/yuling/yuling.schema.yaml")
        );
        assert_eq!(
            resolver.resolve_path("yuling.five.schema"),
            PathBuf::from("C:/data/schema/yuling/five/yuling.five.schema.yaml")
        );
        assert_eq!(
            resolver.resolve_path("yuling.five.dict"),
            PathBuf::from("C:/data/schema/yuling/five/yuling.five.dict.yaml")
        );
        // 未归类配置保持扁平
        assert_eq!(
            resolver.resolve_path("default"),
            PathBuf::from("C:/data/default.yaml")
        );
        // id 含 '/' 视为显式相对路径，不套分类
        assert_eq!(
            resolver.resolve_path("schema/yuhao/yuling.quick"),
            PathBuf::from("C:/data/schema/yuhao/yuling.quick.yaml")
        );
    }

    #[test]
    fn staging_root_detection() {
        assert!(is_staging_root(Path::new("C:/Rime-Next/build")));
        assert!(is_staging_root(Path::new("C:/Rime-Next/build/")));
        assert!(is_staging_root(Path::new("C:\\Rime-Next\\build")));
        assert!(!is_staging_root(Path::new("C:/Rime-Next/schema")));
    }

    #[test]
    fn override_wins() {
        let mut resolver = ResourceResolver::new(vec![PathBuf::from(".")]);
        resolver.add_override("yuling", PathBuf::from("C:/somewhere/yuling.schema.yaml"));
        assert_eq!(
            resolver.resolve_path("yuling"),
            PathBuf::from("C:/somewhere/yuling.schema.yaml")
        );
    }
}
