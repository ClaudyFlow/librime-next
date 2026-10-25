//! 文件识别扫描器（"先看文件，再看文件夹"的系列文件夹校验）。
//!
//! 这是 librime-next `lever` 层资源识别规则的**逆操作**与**校验器**：
//! `paths.rs::resolve()` 负责"给定 id → 算出应放哪"，本模块负责"给定一个真实文件 →
//! 反推它的 id → 算出它**应当**放在哪 → 和实际父目录比对"。
//!
//! 遍历策略（用户要求）：
//!   对本层目录，**先处理文件**（对每文件反推 id + 期望系列文件夹 + 校验摆放），
//!   **再看文件夹**（递归子目录）。这样每拿到一个 yaml 文件就能立刻检查它的文件夹对不对，
//!   也最直观地体现"系列文件夹 = id 按点号分段"。
//!
//! 系列文件夹恒等式（与 `paths.rs` 完全一致）：
//!   id `yuling`            → `schema/yuling/yuling.schema.yaml`
//!   id `yuling.five`       → `schema/yuling/five/yuling.five.schema.yaml`
//!   id `a.b.c`             → `schema/a/b/c/a.b.c.schema.yaml`
//!   文件名永远是完整 id；文件夹链 == id 按点号分段，少任一级即摆放错误。

use std::path::{Path, PathBuf};

/// 资源类别。决定它的"分类根目录"与"摆放校验规则"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    /// `<id>.schema.yaml`：方案 —— 严格系列校验
    Schema,
    /// `<id>.dict.yaml`：词典 —— 严格系列校验
    Dict,
    /// `<id>.yaml`：根目录扁平配置（default/weasel/symbols…），不走系列
    Config,
    /// `<id>.theme.toml`：主题 —— 严格系列校验
    Theme,
    /// `<id>.lua`：脚本 —— 分组校验（`plugin/<组>/<任意>.lua`）
    Lua,
    /// `plugin/` 下的 `.txt` 等伴随数据（tiger/yuhao 的词频、白名单…）—— 分组校验
    PluginData,
    /// `eassy/` 下的 `.txt` 预设词库 —— 允许分组子目录
    Essay,
    /// `<id>.gram`：语言模型，`model/` 平铺
    Model,
    /// `<id>.bin`：编译产物 —— 严格系列校验（staging 根下）
    BuildBin,
}

/// 摆放校验规则。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementRule {
    /// 严格系列：期望目录 == id 按点号分段（schema/dict/theme/bin）。
    SeriesByDots,
    /// 分组：相对分类根至少有一级子目录（plugin 的脚本/伴随数据按作者或方案族分组）。
    Grouped,
    /// 分类根下任意层级（eassy 允许 default/ 等子目录；model 平铺）。
    UnderRoot,
    /// 必须在数据根下，不得藏在子目录（根级配置）。
    DataRoot,
}

impl ResourceKind {
    /// 该类别的分类根目录名（相对数据根）；`None` 表示就在数据根下扁平放置。
    pub fn category_root(self) -> Option<&'static str> {
        match self {
            ResourceKind::Schema | ResourceKind::Dict => Some("schema"),
            ResourceKind::Theme => Some("theme"),
            ResourceKind::Lua | ResourceKind::PluginData => Some("plugin"),
            ResourceKind::Essay => Some("eassy"),
            ResourceKind::Model => Some("model"),
            ResourceKind::BuildBin | ResourceKind::Config => None,
        }
    }

    /// 摆放校验规则。
    pub fn rule(self) -> PlacementRule {
        match self {
            ResourceKind::Schema | ResourceKind::Dict | ResourceKind::Theme | ResourceKind::BuildBin => {
                PlacementRule::SeriesByDots
            }
            ResourceKind::Lua | ResourceKind::PluginData => PlacementRule::Grouped,
            ResourceKind::Essay | ResourceKind::Model => PlacementRule::UnderRoot,
            ResourceKind::Config => PlacementRule::DataRoot,
        }
    }
}

/// 一个被识别出来的文件及其摆放校验结果。
pub struct ScannedFile {
    pub path: PathBuf,
    pub kind: ResourceKind,
    /// 由文件名反推的资源 id（点号分隔的系列 id）。
    pub resource_id: String,
    /// 期望的系列文件夹链（相对分类根目录的分段；非系列型为语义标签）。
    pub expected_dir: Vec<String>,
    /// 实际的文件夹链（相对分类根目录的分段）。
    pub actual_dir: Vec<String>,
    /// 实际摆放是否与期望一致。
    pub placed_correctly: bool,
    /// 不一致时的说明。
    pub note: String,
}

/// 给一个文件路径分类，并反推 id + 期望系列文件夹 + 实际摆放校验。
/// 不认识的后缀返回 `None`（如 `.toml` speller 配置、`.bak` 备份等）。
pub fn classify(path: &Path, data_root: &Path) -> Option<ScannedFile> {
    let file_name = path.file_name()?.to_str()?;
    let kind = if file_name.ends_with(".schema.yaml") {
        ResourceKind::Schema
    } else if file_name.ends_with(".dict.yaml") {
        ResourceKind::Dict
    } else if file_name.ends_with(".theme.toml") {
        ResourceKind::Theme
    } else if file_name.ends_with(".lua") {
        ResourceKind::Lua
    } else if file_name.ends_with(".txt") {
        // plugin/ 下的 .txt 是插件伴随数据；eassy/ 下的才是词库。按父目录区分。
        let parent = path.parent().unwrap_or(path);
        if parent
            .components()
            .any(|c| c.as_os_str() == "plugin")
        {
            ResourceKind::PluginData
        } else {
            ResourceKind::Essay
        }
    } else if file_name.ends_with(".gram") {
        ResourceKind::Model
    } else if file_name.ends_with(".bin") {
        ResourceKind::BuildBin
    } else if file_name.ends_with(".yaml") {
        ResourceKind::Config
    } else {
        return None;
    };

    // 去掉类型后缀得到裸 id（点号保留，它表达系列层级）。
    let id = match kind {
        ResourceKind::Schema => file_name.strip_suffix(".schema.yaml").unwrap().to_string(),
        ResourceKind::Dict => file_name.strip_suffix(".dict.yaml").unwrap().to_string(),
        ResourceKind::Theme => file_name.strip_suffix(".theme.toml").unwrap().to_string(),
        ResourceKind::Lua => file_name.strip_suffix(".lua").unwrap().to_string(),
        ResourceKind::PluginData | ResourceKind::Essay => {
            file_name.strip_suffix(".txt").unwrap().to_string()
        }
        ResourceKind::Model => file_name.strip_suffix(".gram").unwrap().to_string(),
        ResourceKind::BuildBin => file_name.strip_suffix(".bin").unwrap().to_string(),
        ResourceKind::Config => file_name.strip_suffix(".yaml").unwrap().to_string(),
    };

    // 期望的系列文件夹链（仅严格系列型有意义，其余为语义标签，仅用于展示）。
    let expected_dir: Vec<String> = match kind {
        ResourceKind::Schema | ResourceKind::Dict | ResourceKind::Theme | ResourceKind::BuildBin => {
            id.split('.').map(|s| s.to_string()).collect()
        }
        ResourceKind::Lua | ResourceKind::PluginData => vec!["<组>".to_string()],
        ResourceKind::Essay => vec!["eassy".to_string()],
        ResourceKind::Model => vec!["model".to_string()],
        ResourceKind::Config => Vec::new(),
    };

    // 实际文件夹链：相对"分类根目录"的分段。
    let rule = kind.rule();
    let (actual_dir, placed_correctly, note) = match kind.category_root() {
        Some(root_name) => {
            let abs_root = data_root.join(root_name);
            let parent = path.parent().unwrap_or(path);
            // 优先相对分类根取相对段；取不到再相对数据根。
            let rel = parent
                .strip_prefix(&abs_root)
                .or_else(|_| parent.strip_prefix(data_root))
                .map(|p| p.to_path_buf())
                .unwrap_or_default();
            let segs = seg(&rel);
            let (ok, note) = match rule {
                PlacementRule::SeriesByDots => {
                    let ok = segs == expected_dir;
                    let note = if ok {
                        String::new()
                    } else {
                        format!(
                            "series folder mismatch: expected `{}` but found `{}`",
                            expected_dir.join("/"),
                            segs.join("/")
                        )
                    };
                    (ok, note)
                }
                PlacementRule::Grouped => {
                    // 至少要一级分组目录，且不能直接散在分类根。
                    let ok = segs.len() >= 1;
                    let note = if ok {
                        String::new()
                    } else {
                        format!("should be grouped under `{root_name}/<组>/`, but sits at root")
                    };
                    (ok, note)
                }
                PlacementRule::UnderRoot => {
                    // 在分类根下（允许任意层级子目录）。
                    let ok = !parent.starts_with(data_root) || segs.first().is_some();
                    let note = if ok {
                        String::new()
                    } else {
                        format!("should live under `{root_name}/`, but found outside")
                    };
                    (ok, note)
                }
                PlacementRule::DataRoot => {
                    let ok = segs.is_empty();
                    let note = if ok {
                        String::new()
                    } else {
                        format!("should sit at data-root, but found under `{}`", segs.join("/"))
                    };
                    (ok, note)
                }
            };
            (segs, ok, note)
        }
        None => {
            // 根级扁平资源：必须在数据根下，不能躲在子目录里。
            let parent = path.parent().unwrap_or(path);
            let rel = parent
                .strip_prefix(data_root)
                .map(|p| p.to_path_buf())
                .unwrap_or_default();
            let segs = seg(&rel);
            let ok = segs.is_empty(); // expected 为空
            let note = if ok {
                String::new()
            } else {
                format!("should sit at data-root, but found under `{}`", segs.join("/"))
            };
            (segs, ok, note)
        }
    };

    Some(ScannedFile {
        path: path.to_path_buf(),
        kind,
        resource_id: id,
        expected_dir,
        actual_dir,
        placed_correctly,
        note,
    })
}

/// 把路径压成非空分段。
fn seg(p: &Path) -> Vec<String> {
    p.components()
        .filter_map(|c| c.as_os_str().to_str().map(|s| s.to_string()))
        .filter(|s| !s.is_empty())
        .collect()
}

/// 遍历 `dir`：先处理本层**文件**（识别 + 校验），再递归**文件夹**。
/// `data_root` 始终是最初的数据根，用于计算相对分段。
pub fn scan_dir(dir: &Path, data_root: &Path, out: &mut Vec<ScannedFile>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();

    // 阶段一：先看文件
    for p in &paths {
        if p.is_file() {
            if let Some(s) = classify(p, data_root) {
                out.push(s);
            }
        }
    }
    // 阶段二：再看文件夹
    for p in &paths {
        if p.is_dir() {
            scan_dir(p, data_root, out);
        }
    }
}
