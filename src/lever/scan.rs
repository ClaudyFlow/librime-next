//! 文件识别扫描器：**先看文件，再看文件夹**的系列文件夹校验 + 词典击键统计。
//!
//! 这是 `paths.rs` 资源识别规则的**逆操作与校验器**：
//! `paths::Layout::resolve()` 负责"给定 id → 算出应放哪"；本模块负责"给定一个真实
//! 文件 → 反推它的 id → 算出它**应当**放在哪 → 和实际父目录比对"。
//!
//! ## 遍历策略（先看文件，再看文件夹）
//!
//! 对本层目录，**先处理本层所有文件**（识别、反推 id、校验摆放），**再递归子目录**。
//! 这样每拿到一个 yaml 文件就能立刻检查它的文件夹链对不对，也最直观地体现
//! "系列文件夹 = id 按点号分段"。
//!
//! ## 系列文件夹恒等式（与 `paths.rs` 完全一致）
//!
//! ```text
//! id `yuling`      → schema/yuling/yuling.schema.yaml
//! id `yuling.five` → schema/yuling/five/yuling.five.schema.yaml
//! id `a.b.c`       → schema/a/b/c/a.b.c.schema.yaml
//! ```
//!
//! 文件名永远是完整 id；文件夹链 == id 按点号分段，少任一级即摆放错误。
//! **零扁平回退**（2026-10-25 硬规则）：找不到分类路径就是错，不存在
//! `schema/yuling.five.schema.yaml` 这种摆法。
//!
//! ## 击键统计
//!
//! 击键 = 词典编码列的**字符数**（每字符一次按键：拼音 `nihao`=5、五笔 `gg`=2）。
//! 平均击键 = 总击键 / 词条数；最大击键 = 最长编码字符数。这是输入法键步效率的
//! 静态近似（不依赖引擎运行）。

use std::path::{Path, PathBuf};

use super::paths::{BUILD_DIR, DICT_DIR, EASSY_DIR, MODEL_DIR, PLUGIN_DIR, SCHEMA_DIR, THEME_DIR};

// --------------------------------------------------------------------------
// 资源类别与摆放规则
// --------------------------------------------------------------------------

/// 资源类别。决定它的"分类根目录"与"摆放校验规则"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    /// `<id>.schema.yaml`：方案 —— 严格系列校验
    Schema,
    /// `<id>.dict.yaml`：词典 —— 严格系列校验
    Dict,
    /// `<id>.yaml`：根目录扁平配置（default/weasel/symbols…），不走系列
    Config,
    /// `<id>.theme.yaml`（旧：`<id>.theme.toml`）：主题 —— 严格系列校验
    Theme,
    /// `<id>.lua`：脚本 —— 分组校验（`plugin/<组>/<任意>.lua`）
    Lua,
    /// `plugin/` 下的 `.txt` 等伴随数据（tiger/yuhao 的词频、白名单…）—— 分组校验
    PluginData,
    /// `eassy/` 下的 `.txt` 预设词库 —— 允许分组子目录
    Essay,
    /// `<id>.gram`：语言模型 —— 分类根下平铺
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
    /// 该类别的分类根目录名（相对数据根，取 `paths` 的写死常量）；
    /// `None` 表示就在数据根下扁平放置。
    pub fn category_root(self) -> Option<&'static str> {
        match self {
            ResourceKind::Schema => Some(SCHEMA_DIR),
            ResourceKind::Dict => Some(DICT_DIR),
            ResourceKind::Theme => Some(THEME_DIR),
            ResourceKind::Lua | ResourceKind::PluginData => Some(PLUGIN_DIR),
            ResourceKind::Essay => Some(EASSY_DIR),
            ResourceKind::Model => Some(MODEL_DIR),
            // `.bin` 只在 staging（用户文件夹 build）下；`Config` 在数据根。
            ResourceKind::BuildBin => Some(BUILD_DIR),
            ResourceKind::Config => None,
        }
    }

    /// 摆放校验规则。
    pub fn rule(self) -> PlacementRule {
        match self {
            ResourceKind::Schema
            | ResourceKind::Dict
            | ResourceKind::Theme
            | ResourceKind::BuildBin => PlacementRule::SeriesByDots,
            ResourceKind::Lua | ResourceKind::PluginData => PlacementRule::Grouped,
            ResourceKind::Essay | ResourceKind::Model => PlacementRule::UnderRoot,
            ResourceKind::Config => PlacementRule::DataRoot,
        }
    }

    /// 报告用的短标签。
    pub fn kind_label(self) -> &'static str {
        match self {
            ResourceKind::Schema => "schema",
            ResourceKind::Dict => "dict",
            ResourceKind::Config => "config",
            ResourceKind::Theme => "theme",
            ResourceKind::Lua => "lua",
            ResourceKind::PluginData => "plugin-data",
            ResourceKind::Essay => "essay",
            ResourceKind::Model => "model",
            ResourceKind::BuildBin => "build-bin",
        }
    }
}

/// 一个被识别出来的文件及其摆放校验结果。
#[derive(Debug, Clone)]
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
    } else if file_name.ends_with(".theme.toml") || file_name.ends_with(".theme.yaml") {
        ResourceKind::Theme
    } else if file_name.ends_with(".lua") {
        ResourceKind::Lua
    } else if file_name.ends_with(".txt") {
        // plugin/ 下的 .txt 是插件伴随数据；eassy/ 下的才是词库。按路径分段区分。
        if has_segment(path, PLUGIN_DIR) {
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
        // 2026-10-27：主题改用 YAML，两种后缀都要能剥（旧 .toml 仍认，存量文件还能扫到）。
        ResourceKind::Theme => file_name
            .strip_suffix(".theme.toml")
            .or_else(|| file_name.strip_suffix(".theme.yaml"))
            .unwrap()
            .to_string(),
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
        ResourceKind::Schema
        | ResourceKind::Dict
        | ResourceKind::Theme
        | ResourceKind::BuildBin => id.split('.').map(|s| s.to_string()).collect(),
        ResourceKind::Lua | ResourceKind::PluginData => vec!["<组>".to_string()],
        ResourceKind::Essay => vec![EASSY_DIR.to_string()],
        ResourceKind::Model => vec![MODEL_DIR.to_string()],
        ResourceKind::Config => Vec::new(),
    };
    // `.bin` 特例：`build/<系列…>/<id>.bin` 的 id 末段是类型词
    // （table / prism / reverse），它进**文件名**而不是文件夹链。
    //   a.b.table → build/a/b/a.b.table.bin（系列链 [a, b]）
    let expected_dir = if kind == ResourceKind::BuildBin {
        let mut segs = expected_dir;
        segs.pop();
        segs
    } else {
        expected_dir
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

/// 路径里是否出现过某一级目录名（如 `.txt` 是否位于 `plugin/` 下）。
fn has_segment(path: &Path, name: &str) -> bool {
    path.components().any(|c| c.as_os_str() == name)
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

/// 扫描整个数据根（程序文件夹的共享数据目录或用户文件夹）。
pub fn scan_data_root(data_root: &Path) -> Vec<ScannedFile> {
    let mut out = Vec::new();
    scan_dir(data_root, data_root, &mut out);
    out
}

/// 摆放错误清单（`placed_correctly == false` 的项）。
pub fn misplaced(files: &[ScannedFile]) -> Vec<&ScannedFile> {
    files.iter().filter(|f| !f.placed_correctly).collect()
}

// --------------------------------------------------------------------------
// 击键统计
// --------------------------------------------------------------------------

/// 单个词典的击键统计。
#[derive(Debug, Clone)]
pub struct KeystrokeStats {
    pub dict_id: String,
    pub path: String,
    pub entry_count: usize,
    pub total_keys: usize,
    pub avg_keys: f64,
    pub max_keys: usize,
    /// 最大击键对应的编码示例。
    pub max_keys_example: String,
}

/// 解析 `.dict.yaml` 的 frontmatter（`---` 包裹的 YAML 块），取出 `columns`。
/// 没有 frontmatter 或缺 `columns` 时退回 Rime 默认 `[text, code]`。
fn parse_columns(frontmatter: &str) -> Vec<String> {
    if let Ok(value) = serde_norway::from_str::<serde_norway::Value>(frontmatter) {
        if let Some(cols) = value.get("columns").and_then(|c| c.as_sequence()) {
            let names: Vec<String> = cols
                .iter()
                .filter_map(|c| c.as_str().map(str::to_string))
                .collect();
            if !names.is_empty() {
                return names;
            }
        }
    }
    vec!["text".to_string(), "code".to_string()]
}

/// 把 `.dict.yaml` 文本拆成 (frontmatter, 数据行)。数据行跳过空行与 `#` 注释，
/// 只收至少两列（制表符分隔）的行。
fn split_dict(text: &str) -> (String, Vec<Vec<String>>) {
    let lines: Vec<&str> = text.lines().collect();

    // 找 frontmatter 边界：第一个 `---` 到下一个 `---`。
    let mut start = None;
    let mut end = None;
    for (i, l) in lines.iter().enumerate() {
        if l.trim() == "---" {
            if start.is_none() {
                start = Some(i);
            } else {
                end = Some(i);
                break;
            }
        }
    }

    let frontmatter = match (start, end) {
        (Some(s), Some(e)) => lines[s + 1..=e].join("\n"),
        _ => String::new(),
    };

    let data_start = end.map(|e| e + 1).unwrap_or(0);
    let mut rows = Vec::new();
    for l in &lines[data_start..] {
        let line = l.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<String> = line.split('\t').map(|s| s.to_string()).collect();
        if parts.len() >= 2 {
            rows.push(parts);
        }
    }
    (frontmatter, rows)
}

/// 计算单个词典的平均/最大击键。无法解析或 0 词条返回 `None`。
///
/// 击键 = 编码列字符数。编码列优先取名为 `code` / `spelling` / `input` 的列，
/// 否则取第二列（默认 `[text, code]`）。一个词条可带多个编码（空格分隔），
/// 取第一段作为主编码。
pub fn compute_keystrokes(dict_path: &Path, resource_id: &str) -> Option<KeystrokeStats> {
    let text = std::fs::read_to_string(dict_path).ok()?;
    let (frontmatter, rows) = split_dict(&text);
    let columns = parse_columns(&frontmatter);

    let code_idx = columns
        .iter()
        .position(|c| matches!(c.as_str(), "code" | "spelling" | "input"))
        .unwrap_or(1);

    let mut total = 0usize;
    let mut count = 0usize;
    let mut max_keys = 0usize;
    let mut max_example = String::new();

    for row in &rows {
        let Some(code) = row.get(code_idx) else { continue };
        let code = code.trim();
        if code.is_empty() {
            continue;
        }
        let primary = code.split_whitespace().next().unwrap_or(code);
        let k = primary.chars().count();
        total += k;
        count += 1;
        if k > max_keys {
            max_keys = k;
            max_example = primary.to_string();
        }
    }

    if count == 0 {
        return None;
    }

    Some(KeystrokeStats {
        dict_id: resource_id.to_string(),
        path: dict_path.display().to_string(),
        entry_count: count,
        total_keys: total,
        avg_keys: total as f64 / count as f64,
        max_keys,
        max_keys_example: max_example,
    })
}

/// 对一批扫描结果里的词典批量算击键（跳过非词典与解析失败的）。
pub fn keystrokes_for(files: &[ScannedFile]) -> Vec<KeystrokeStats> {
    files
        .iter()
        .filter(|f| f.kind == ResourceKind::Dict)
        .filter_map(|f| compute_keystrokes(&f.path, &f.resource_id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("librime-next-scan-test-{name}"))
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, b"x").unwrap();
    }

    #[test]
    fn series_identity_yuling_five() {
        // yuling.five → schema/yuling/five/yuling.five.schema.yaml
        let root = temp("yuling-five");
        let _ = std::fs::remove_dir_all(&root);
        let good = root.join("schema/yuling/five/yuling.five.schema.yaml");
        touch(&good);
        let scanned = classify(&good, &root).expect("应识别");
        assert_eq!(scanned.kind, ResourceKind::Schema);
        assert_eq!(scanned.resource_id, "yuling.five");
        assert_eq!(scanned.expected_dir, vec!["yuling", "five"]);
        assert_eq!(scanned.actual_dir, vec!["yuling", "five"]);
        assert!(scanned.placed_correctly, "{}", scanned.note);

        // 扁平摆放（零扁平回退：必须判错）
        let flat = root.join("schema/yuling.five.schema.yaml");
        touch(&flat);
        let scanned = classify(&flat, &root).expect("应识别");
        assert!(!scanned.placed_correctly, "扁平摆放必须判错");
        assert_eq!(scanned.actual_dir, Vec::<String>::new());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dict_series_and_bin_naming() {
        // a.b.c → dict/a/b/c/a.b.c.dict.yaml；文件名永远是完整 id
        let root = temp("dict-series");
        let _ = std::fs::remove_dir_all(&root);
        let dict = root.join("dict/a/b/c/a.b.c.dict.yaml");
        touch(&dict);
        let scanned = classify(&dict, &root).expect("应识别");
        assert_eq!(scanned.resource_id, "a.b.c");
        assert!(scanned.placed_correctly, "{}", scanned.note);

        // build 下的 .bin 走同一套系列规则
        let bin = root.join(BUILD_DIR).join("a/b/a.b.table.bin");
        touch(&bin);
        let scanned = classify(&bin, &root).expect("应识别");
        assert_eq!(scanned.kind, ResourceKind::BuildBin);
        assert_eq!(scanned.resource_id, "a.b.table");
        assert!(scanned.placed_correctly, "{}", scanned.note);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 2026-10-27：主题文件改用 YAML（全仓库退出 TOML）。`.theme.yaml` 必须和旧的
    /// `.theme.toml` 一样被认成 **Theme** 并按系列分层 —— 否则会落到通用 `.yaml`
    /// 分支（Config），被当成"未归类配置"摆到根目录。
    #[test]
    fn theme_yaml_is_classified_as_theme() {
        let root = temp("theme-yaml");
        let _ = std::fs::remove_dir_all(&root);

        for (file, id) in [
            ("theme/weasel/weasel.theme.yaml", "weasel"),
            ("theme/yuling/five/yuling.five.theme.yaml", "yuling.five"),
            ("theme/weasel/weasel.theme.toml", "weasel"),
        ] {
            let path = root.join(file);
            touch(&path);
            let scanned = classify(&path, &root).expect("应识别");
            assert_eq!(scanned.kind, ResourceKind::Theme, "{file}");
            assert_eq!(scanned.resource_id, id, "{file}");
            assert!(scanned.placed_correctly, "{}: {}", file, scanned.note);
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_order_files_before_folders() {
        // 「先看文件，再看文件夹」是**每层内**的次序：本层文件先于子目录里的文件。
        let root = temp("order");
        let _ = std::fs::remove_dir_all(&root);
        touch(&root.join("schema/yuling/yuling.schema.yaml")); // 深层文件
        touch(&root.join("schema/root_level.yaml")); // schema/ 本层的文件（Config）
        let files = scan_data_root(&root);
        let ids: Vec<&str> = files.iter().map(|f| f.resource_id.as_str()).collect();
        // schema/ 本层的 root_level.yaml 必须先于子目录 yuling/ 里的 yuling
        assert_eq!(ids, vec!["root_level", "yuling"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn keystrokes_pinyin_and_wubi() {
        // 具体输入输出：合成词典，验证 avg/max
        let root = temp("keys");
        let _ = std::fs::remove_dir_all(&root);
        let dict = root.join("schema/luna/luna.dict.yaml");
        std::fs::create_dir_all(dict.parent().unwrap()).unwrap();
        std::fs::write(
            &dict,
            "---\nname: luna\ncolumns: [text, code, weight]\n...\n\
             你好\tni\t1\n\
             你\tnihao\t2\n\
             好\thao\t3\n\
             世界\tshijie\t4\n",
        )
        .unwrap();
        let stats = compute_keystrokes(&dict, "luna").expect("应算出");
        // 击键 = 编码字符数：ni=2, nihao=5, hao=3, shijie=6 → 总 16 / 4 条 = 4.0，max=6
        assert_eq!(stats.entry_count, 4);
        assert_eq!(stats.total_keys, 16);
        assert_eq!(stats.avg_keys, 4.0);
        assert_eq!(stats.max_keys, 6);
        assert_eq!(stats.max_keys_example, "shijie");

        // 默认列（无 columns 声明）取第二列为编码
        let dict2 = root.join("schema/wubi/wubi.dict.yaml");
        std::fs::create_dir_all(dict2.parent().unwrap()).unwrap();
        std::fs::write(&dict2, "一\tg\n二\tf\n").unwrap();
        let stats2 = compute_keystrokes(&dict2, "wubi").expect("应算出");
        assert_eq!(stats2.entry_count, 2);
        assert_eq!(stats2.total_keys, 2);
        assert_eq!(stats2.avg_keys, 1.0);
        assert_eq!(stats2.max_keys, 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn root_config_must_not_hide_in_subdir() {
        let root = temp("rootcfg");
        let _ = std::fs::remove_dir_all(&root);
        let good = root.join("default.yaml");
        touch(&good);
        assert!(classify(&good, &root).expect("应识别").placed_correctly);
        let bad = root.join("sub/default.yaml");
        touch(&bad);
        assert!(!classify(&bad, &root).expect("应识别").placed_correctly);
        let _ = std::fs::remove_dir_all(&root);
    }
}
