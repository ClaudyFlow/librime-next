//! 方案准入检查器 —— **可视化方案列表的门卫**。
//!
//! 一份 `<id>.schema.yaml` 想进入方案列表（`<user>/schema/schema.yaml` 点名清单，
//! 也就是界面里那个勾选清单），必须先过这里的三道关：
//!
//! 1. **文件头标记**：注释头里要有 `# encode: utf-8` 与 `# newline: lf`（缺一不可）；
//! 2. **实际格式匹配标记**：文件真的是 UTF-8（无 BOM）、换行真的是 LF
//!    （标记说 lf 却混着 CRLF，或反过来，都算不符 —— 标记撒谎比没标记更坏）；
//! 3. **`schema: id:` 内容**：YAML 能解析、`schema.id` 存在且干净、与文件名一致
//!    （系列恒等式：文件名永远是完整 id）、系列文件夹摆放正确。
//!
//! **强制 `schema: id:`**（2026-10-27 拍板）：第二关验的是源文件里的**原样键名**，
//! 只认方言单数 `id:`。写 librime 原生 `schema_id`、或 `id`/`schema_id` 同时写
//! （归一化会静默丢弃 `id`），一律拒收 —— 方言要纯，混养不如强制。
//!
//! 注意：`load_file` 会做方言归一化，源里的 `schema.id` 读出来已是 librime 标准键
//! `schema.schema_id`（`config::load::normalize_dialect` 的规矩），所以这道关走
//! **原样解析**（[`raw_parse`]），不看归一化后的树。
//!
//! 三关全过才准入（[`admit`] 把 id 追加进清单并写回）；任何一关挂掉，
//! **不会**动清单 —— 半成品方案进了列表，用户一点就白屏，比进不去更糟。
//!
//! ## 与部署的关系
//!
//! 清单的唯一落点是 `<user>/schema/schema.yaml`（`deployment_tasks` 的
//! `load_schema_manifest` 读它、`workspace_update` 按它逐方案编译），所以本模块
//! 只负责"把关 + 写清单"；写完提示跑一次部署，编译产物才会进 `build/`，
//! 界面列表才看得到。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::deployment_tasks::{load_schema_manifest, SchemaManifest};
use super::paths::{DICT_DIR, SCHEMA_DIR, SCHEMA_MANIFEST};
use super::scan::{self, ResourceKind};

// --------------------------------------------------------------------------
// 检查结果模型
// --------------------------------------------------------------------------

/// 文件头声明：从注释头里读到的 `encode:` / `newline:` 值。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HeaderDecl {
    pub encode: Option<String>,
    pub newline: Option<String>,
}

/// 单条检查。`ok=false` 时 `detail` 说明原因。
#[derive(Debug, Clone)]
pub struct CheckItem {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

/// 一份方案的准入报告。`schema_id` 只有解析成功才有。
#[derive(Debug, Clone)]
pub struct AdmitReport {
    pub path: PathBuf,
    pub schema_id: Option<String>,
    pub schema_name: Option<String>,
    pub items: Vec<CheckItem>,
}

impl AdmitReport {
    /// 三关全过才为真。
    pub fn passed(&self) -> bool {
        !self.items.is_empty() && self.items.iter().all(|i| i.ok)
    }

    fn push(&mut self, name: &'static str, ok: bool, detail: impl Into<String>) {
        self.items.push(CheckItem {
            name,
            ok,
            detail: detail.into(),
        });
    }
}

// --------------------------------------------------------------------------
// 第一关：文件头标记 + 实际格式
// --------------------------------------------------------------------------

/// 从文本里抠文件头声明：只看开头连续的 `#` 注释行，遇到第一行非注释即停。
/// `---` 之前的空行也容忍（r间于手滑）。
pub fn parse_header_decl(text: &str) -> HeaderDecl {
    let mut decl = HeaderDecl::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(comment) = line.strip_prefix('#') else {
            break; // 注释头结束
        };
        let comment = comment.trim();
        if let Some(v) = comment.strip_prefix("encode:") {
            decl.encode = Some(v.trim().to_ascii_lowercase());
        } else if let Some(v) = comment.strip_prefix("newline:") {
            decl.newline = Some(v.trim().to_ascii_lowercase());
        }
    }
    decl
}

/// 统计实际换行：`(CRLF 数, 裸 LF 数, 裸 CR 数)`。
/// 裸 LF = 前面不是 `\r` 的 `\n`；裸 CR = 后面不是 `\n` 的 `\r`。
pub fn count_newlines(bytes: &[u8]) -> (usize, usize, usize) {
    let (mut crlf, mut lf, mut cr) = (0usize, 0usize, 0usize);
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' if bytes.get(i + 1) == Some(&b'\n') => {
                crlf += 1;
                i += 2;
            }
            b'\n' => {
                lf += 1;
                i += 1;
            }
            b'\r' => {
                cr += 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    (crlf, lf, cr)
}

/// 第一关：标记在不在、实际格式配不配。
fn check_format(bytes: &[u8], report: &mut AdmitReport) {
    let text = String::from_utf8_lossy(bytes);
    let decl = parse_header_decl(&text);

    // —— encode 标记 ——
    match &decl.encode {
        Some(enc) if enc == "utf-8" || enc == "utf8" => {
            report.push("encode 标记", true, format!("# encode: {enc}"));
        }
        Some(enc) => {
            report.push(
                "encode 标记",
                false,
                format!("声明了 `# encode: {enc}`，本方案只认 utf-8"),
            );
        }
        None => report.push("encode 标记", false, "注释头缺 `# encode: utf-8`"),
    }

    // —— newline 标记 ——
    match &decl.newline {
        Some(nl) if nl == "lf" || nl == "crlf" => {
            report.push("newline 标记", true, format!("# newline: {nl}"));
        }
        Some(nl) => {
            report.push(
                "newline 标记",
                false,
                format!("声明了 `# newline: {nl}`，只认 lf / crlf"),
            );
        }
        None => report.push("newline 标记", false, "注释头缺 `# newline: lf`"),
    }

    // —— 实际编码：UTF-8 且无 BOM ——
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        report.push("实际编码", false, "文件带 UTF-8 BOM，与 `utf-8`（无 BOM）不符");
    } else if std::str::from_utf8(bytes).is_err() {
        report.push("实际编码", false, "字节流不是合法 UTF-8，与 `utf-8` 标记不符");
    } else {
        report.push("实际编码", true, "合法 UTF-8，无 BOM");
    }

    // —— 实际换行：与声明一致 ——
    let (crlf_n, lf_n, cr_n) = count_newlines(bytes);
    match decl.newline.as_deref() {
        Some("lf") if crlf_n == 0 && cr_n == 0 => {
            report.push("实际换行", true, format!("{lf_n} 行 LF，无 CRLF/CR"));
        }
        Some("lf") => report.push(
            "实际换行",
            false,
            format!(
                "声明 lf 但实际有 {crlf_n} 处 CRLF、{cr_n} 处裸 CR —— 标记撒谎，拒收"
            ),
        ),
        Some("crlf") if lf_n == 0 && cr_n == 0 => {
            report.push("实际换行", true, format!("{crlf_n} 行 CRLF"));
        }
        Some("crlf") => report.push(
            "实际换行",
            false,
            format!("声明 crlf 但实际混了 {lf_n} 处裸 LF、{cr_n} 处裸 CR"),
        ),
        _ => {} // 标记缺失，已在上面记过，不重复罚
    }
}

// --------------------------------------------------------------------------
// 第二关：schema: id: 内容
// --------------------------------------------------------------------------

/// id 只允许字母数字与 `._-`；不允许空、首尾点、连续点（系列分段才用点）。
pub fn id_is_sane(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        && !id.starts_with('.')
        && !id.ends_with('.')
        && !id.contains("..")
}

/// 从文件名剥出系列 id：`pinyin.schema.yaml` → `pinyin`；`yuling.five.schema.yaml` → `yuling.five`。
pub fn id_from_file_name(file_name: &str) -> Option<&str> {
    file_name.strip_suffix(".schema.yaml")
}

/// 从文件名剥出**词典 id**：`pinyin.luna.dict.yaml` → `pinyin.luna`。
pub fn id_from_dict_file_name(file_name: &str) -> Option<&str> {
    file_name.strip_suffix(".dict.yaml")
}

/// 由文件路径反推数据根：向上找名为 `schema` 的目录，其父即数据根。
/// 找不到返回 `None`（摆放检查跳过，只记提示）。
fn find_data_root(path: &Path) -> Option<PathBuf> {
    for anc in path.ancestors().skip(1) {
        if anc.file_name().is_some_and(|n| n == SCHEMA_DIR) {
            return anc.parent().map(|p| p.to_path_buf());
        }
    }
    None
}

/// 由词典文件路径反推数据根：向上找名为 `dict` 的目录，其父即数据根。
fn find_dict_data_root(path: &Path) -> Option<PathBuf> {
    for anc in path.ancestors().skip(1) {
        if anc.file_name().is_some_and(|n| n == DICT_DIR) {
            return anc.parent().map(|p| p.to_path_buf());
        }
    }
    None
}

/// 源文件**原样**解析（不过方言归一化）。
///
/// 检查器验的是"人写了什么"，不是归一化之后的样子 —— `load_file` 会把
/// `schema.id` 静默改名成 `schema_id`（`normalize_dialect` 的规矩），要强制
/// 方言单数写法，就必须看原名。切分规则与 `load_file` 一致：`...` 之后是词条表。
fn raw_parse(path: &Path) -> Result<serde_json::Value, String> {
    use serde::Deserialize as _;
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut header = text.as_str();
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        if line.trim_end() == "..." {
            header = &text[..offset];
            break;
        }
        offset += line.len();
    }
    for document in serde_norway::Deserializer::from_str(header) {
        let value = serde_norway::Value::deserialize(document)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        return serde_json::to_value(&value).map_err(|e| format!("{}: {e}", path.display()));
    }
    Ok(serde_json::Value::Null)
}

/// 第二关：YAML 解析 + `schema.id` 内容 + 系列摆放。
///
/// **强制 `schema: id:`**：只认方言单数键。写 librime 原生 `schema_id`、或两个
/// 键同时写（归一化会静默丢掉 `id`），都拒收。
fn check_identity(path: &Path, report: &mut AdmitReport) {
    // 解析走部署同一条读入器（方言归一化也过一遍）：这里挂了，部署时同样挂。
    let tree = match crate::load_file(path) {
        Ok(tree) => tree,
        Err(error) => {
            report.push("YAML 解析", false, format!("{error}"));
            return;
        }
    };
    report.push("YAML 解析", true, "读入器通过（与部署同一入口）");

    // 原样树：验源文件里的键名（归一化会把 id 改名，这里必须看原名）。
    let raw = match raw_parse(path) {
        Ok(raw) => raw,
        Err(error) => {
            report.push("schema.id 写法", false, format!("源文件读不了: {error}"));
            return;
        }
    };
    let raw_schema = raw.get("schema");
    let raw_id = raw_schema
        .and_then(|s| s.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let native_id = raw_schema
        .and_then(|s| s.get("schema_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let name = raw_schema
        .and_then(|s| s.get("name"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // —— 强制 `schema: id:`（方言单数）——
    let id = match (raw_id, native_id) {
        (Some(id), None) => {
            report.push(
                "schema.id 写法",
                true,
                format!("schema.id = \"{id}\"（方言单数，合规）"),
            );
            id
        }
        (None, Some(_)) => {
            report.push(
                "schema.id 写法",
                false,
                "写的是 librime 原生键 `schema_id`；本仓库强制方言写法 `schema: id:`",
            );
            return;
        }
        (Some(_), Some(_)) => {
            report.push(
                "schema.id 写法",
                false,
                "`id` 与 `schema_id` 同时写，归一化会静默丢弃 `id`；请只写 `id`",
            );
            return;
        }
        (None, None) => {
            report.push("schema.id 写法", false, "`schema:` 下没有 `id:`");
            return;
        }
    };
    report.schema_id = Some(id.clone());
    report.schema_name = name;

    if !id_is_sane(&id) {
        report.push(
            "schema.id 内容",
            false,
            format!("`{id}` 含非法字符/空分段（只允许字母数字与 . _ -，点表达系列层级）"),
        );
    } else {
        report.push("schema.id 内容", true, format!("id = \"{id}\""));
    }

    // 系列恒等式：文件名主干必须等于 schema.id。
    let stem = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(id_from_file_name);
    match stem {
        Some(stem) if stem == id => {
            report.push("id 与文件名一致", true, format!("{stem}.schema.yaml"));
        }
        Some(stem) => report.push(
            "id 与文件名一致",
            false,
            format!("文件名主干 `{stem}` 与 schema.id `{id}` 不符（系列恒等式：文件名永远是完整 id）"),
        ),
        None => report.push(
            "id 与文件名一致",
            false,
            "文件名不是 `<id>.schema.yaml` 形态",
        ),
    }

    // 系列摆放：schema/<id 按点分段>/…/<id>.schema.yaml。
    let data_root = find_data_root(path);
    match &data_root {
        Some(data_root) => match scan::classify(path, data_root) {
            Some(scanned) if scanned.kind == ResourceKind::Schema && scanned.placed_correctly => {
                report.push(
                    "系列摆放",
                    true,
                    format!("schema/{}", scanned.actual_dir.join("/")),
                );
            }
            Some(scanned) => report.push("系列摆放", false, scanned.note),
            None => report.push("系列摆放", false, "文件类型未被识别"),
        },
        None => report.push(
            "系列摆放",
            false,
            "找不到数据根（文件不在 `schema/` 目录链下）",
        ),
    }

    // 词典提示：translator 块里声明的词典若在，记一笔（不设卡）。
    // 键是单数 `dict`（2026-10-28 20:5x 起；不再认 `dictionary`）。
    let dict = tree
        .pointer("/translator/dict")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    if let Some(dict) = dict {
        let dict_file = data_root.map(|r| {
            r.join(DICT_DIR)
                .join(dict.replace('.', "/"))
                .join(format!("{dict}.dict.yaml"))
        });
        match dict_file {
            Some(p) if p.is_file() => {
                report.push("词典声明", true, format!("translator.dict = {dict}（文件在）"));
            }
            _ => report.push(
                "词典声明",
                true,
                format!("translator.dict = {dict}（未找到同名 .dict.yaml，部署时可能为空）"),
            ),
        }
    }
}

// --------------------------------------------------------------------------
// 总装：检查 + 准入
// --------------------------------------------------------------------------

/// 只检查，不动清单。供脚本/测试复用。
pub fn check_schema_file(path: &Path) -> AdmitReport {
    let mut report = AdmitReport {
        path: path.to_path_buf(),
        schema_id: None,
        schema_name: None,
        items: Vec::new(),
    };
    let Ok(bytes) = std::fs::read(path) else {
        report.push("读取文件", false, format!("读不了 {}", path.display()));
        return report;
    };
    check_format(&bytes, &mut report);
    check_identity(path, &mut report);
    report
}

/// 词典的第三关：YAML 头解析 + 顶层 `id:` + 系列摆放（与方案同一套规矩）。
///
/// 词典身份键是**顶层 `id:`**（方言单数，与文件名/文件夹一致）；
/// `name:` 是显示名（允许中文），不参与身份；写 `dict_id:` 或两个都写一律拒收。
fn check_dict_identity(path: &Path, report: &mut AdmitReport) {
    // 部署期读词典头走同一个读入器，这里挂了部署也会挂。
    if let Err(error) = crate::load_file(path) {
        report.push("YAML 解析", false, format!("{error}"));
        return;
    }
    report.push("YAML 解析", true, "读入器通过（与部署同一入口）");

    let raw = match raw_parse(path) {
        Ok(raw) => raw,
        Err(error) => {
            report.push("dict.id 写法", false, format!("源文件读不了: {error}"));
            return;
        }
    };
    let raw_id = raw.get("id").and_then(|v| v.as_str()).map(str::to_string);
    let native_id = raw
        .get("dict_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let name = raw.get("name").and_then(|v| v.as_str()).map(str::to_string);

    let id = match (raw_id, native_id) {
        (Some(id), None) => {
            report.push(
                "dict.id 写法",
                true,
                format!("dict.id = \"{id}\"（方言单数，合规）"),
            );
            id
        }
        (None, Some(_)) => {
            report.push(
                "dict.id 写法",
                false,
                "写的是 `dict_id`；本仓库强制方言写法 `id:`",
            );
            return;
        }
        (Some(_), Some(_)) => {
            report.push(
                "dict.id 写法",
                false,
                "`id` 与 `dict_id` 同时写；请只写 `id`",
            );
            return;
        }
        (None, None) => {
            report.push(
                "dict.id 写法",
                false,
                "词典头没有 `id:`（身份键必填，须与文件名/文件夹一致）",
            );
            return;
        }
    };
    report.schema_id = Some(id.clone());
    report.schema_name = name;

    if !id_is_sane(&id) {
        report.push(
            "dict.id 内容",
            false,
            format!("`{id}` 含非法字符/空分段（只允许字母数字与 . _ -，点表达系列层级）"),
        );
    } else {
        report.push("dict.id 内容", true, format!("id = \"{id}\""));
    }

    // 系列恒等式：文件名主干必须等于 id。
    let stem = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(id_from_dict_file_name);
    match stem {
        Some(stem) if stem == id => {
            report.push("id 与文件名一致", true, format!("{stem}.dict.yaml"));
        }
        Some(stem) => report.push(
            "id 与文件名一致",
            false,
            format!("文件名主干 `{stem}` 与 id `{id}` 不符（文件名永远是完整 id）"),
        ),
        None => report.push(
            "id 与文件名一致",
            false,
            "文件名不是 `<id>.dict.yaml` 形态",
        ),
    }

    // 摆放：`dict/<id 分段的前缀…>/<id>.dict.yaml`。
    //
    // 方案是「全部分段」严格对齐；**词库放宽到前缀**（2026-10-28 用户定）：
    // `cangjie.5.base` 这类被 `import_tables` 吃掉的片段不必各自建文件夹，
    // 只要目录是 id 分段的某个前缀（`dict/`、`dict/cangjie/`、`dict/cangjie/5/`…）
    // 就算摆放合适；查找侧按「精确优先、再逐级退前缀」找（见
    // `dict_compiler::classified_dict_source`）。
    match &find_dict_data_root(path) {
        Some(data_root) => match dict_placement(path, data_root, &id) {
            Ok(under) => report.push(
                "系列摆放",
                true,
                if under.is_empty() {
                    "dict/（id 分段的前缀）".to_string()
                } else {
                    format!("dict/{under}（id 分段的前缀）")
                },
            ),
            Err(note) => report.push("系列摆放", false, note),
        },
        None => report.push(
            "系列摆放",
            false,
            "找不到数据根（文件不在 `dict/` 目录链下）",
        ),
    }
}

/// 词库摆放判定：目录必须是 `dict/` + **id 分段的某个前缀**。
///
/// 返回目录相对 `dict/` 的部分（空串 = 直接放在 `dict/` 根下）；不合法返回原因。
/// 例：`id = cangjie.5.base` 时，`dict/`、`dict/cangjie/`、`dict/cangjie/5/`、
/// `dict/cangjie/5/base/` 全部合格；`dict/wubi/` 不合格。
fn dict_placement(path: &Path, data_root: &Path, id: &str) -> Result<String, String> {
    let dir = path.parent().unwrap_or(Path::new(""));
    let not_under = || "文件不在 `dict/` 目录链下".to_string();
    let Ok(rel) = dir.strip_prefix(data_root) else {
        return Err(not_under());
    };
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    if parts.first().map(String::as_str) != Some(DICT_DIR) {
        return Err(not_under());
    }
    let under: Vec<&str> = parts[1..].iter().map(String::as_str).collect();
    let segments: Vec<&str> = id.split('.').filter(|s| !s.is_empty()).collect();
    if under.len() > segments.len()
        || under.iter().zip(segments.iter()).any(|(actual, want)| actual != want)
    {
        return Err(format!(
            "目录 `dict/{}` 不是 id `{id}` 分段的前缀（可放 `dict/{}` 下任一层前缀目录，或直接放 `dict/`）",
            under.join("/"),
            segments.join("/")
        ));
    }
    Ok(under.join("/"))
}

/// 只检查词典文件，不动任何清单（与 [`check_schema_file`] 同构）。
pub fn check_dict_file(path: &Path) -> AdmitReport {
    let mut report = AdmitReport {
        path: path.to_path_buf(),
        schema_id: None,
        schema_name: None,
        items: Vec::new(),
    };
    let Ok(bytes) = std::fs::read(path) else {
        report.push("读取文件", false, format!("读不了 {}", path.display()));
        return report;
    };
    check_format(&bytes, &mut report);
    check_dict_identity(path, &mut report);
    report
}

/// 报告里第一条失败原因（`项名：说明`），供日志/抛弃名单用。
pub fn first_failure(report: &AdmitReport) -> String {
    report
        .items
        .iter()
        .find(|item| !item.ok)
        .map(|item| format!("{}：{}", item.name, item.detail))
        .unwrap_or_else(|| "未知原因".to_string())
}

/// 人看的报告（门卫小票）。
pub fn format_report(report: &AdmitReport) -> String {
    let mut out = String::new();
    let title = report
        .schema_id
        .clone()
        .unwrap_or_else(|| "<id 未取到>".to_string());
    out.push_str(&format!("== 方案准入检查：{title} ==\n"));
    for item in &report.items {
        let mark = if item.ok { "[OK]  " } else { "[FAIL]" };
        out.push_str(&format!("{mark} {:<12} {}\n", item.name, item.detail));
    }
    out.push_str(if report.passed() {
        "== 结论：通过 ==\n"
    } else {
        "== 结论：不通过，未加入方案列表 ==\n"
    });
    out
}

/// 清单头注释的默认版本（手改过的文件读入时不依赖它，纯注释）。
pub const DEFAULT_MANIFEST_HEADER: &str = "# 方案信息 —— 勾选清单 + 当前使用的方案\n";

/// 把清单写回 `<user_data_dir>/schema/schema.yaml`（默认头注释）。
/// 发射走 `config::to_yaml_string`（全 crate 唯一出口），保留未知键与键序。
pub fn save_schema_manifest(user_data_dir: &Path, manifest: &SchemaManifest) -> Result<PathBuf, String> {
    save_schema_manifest_with_header(user_data_dir, manifest, DEFAULT_MANIFEST_HEADER)
}

/// 同上，但头注释由调用方给（生成清单时会写一段"这是自动生成的"说明）。
pub fn save_schema_manifest_with_header(
    user_data_dir: &Path,
    manifest: &SchemaManifest,
    header: &str,
) -> Result<PathBuf, String> {
    let path = user_data_dir.join(SCHEMA_DIR).join(SCHEMA_MANIFEST);
    let mut map = serde_json::Map::new();
    if let Some(active) = &manifest.active {
        map.insert("active".into(), json!(active));
    }
    map.insert(
        "schemas".into(),
        Value::Array(manifest.schemas.iter().map(|s| json!(s)).collect()),
    );
    let item = crate::config::convert_from_yaml(&Value::Object(map), None);
    let mut text = crate::config::to_yaml_string(&item).map_err(|e| format!("序列化清单失败: {e}"))?;
    // 头部注释（纯人看的；读入器不依赖它）
    if !text.starts_with(header) {
        text = format!("{header}{text}");
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建目录 {} 失败: {e}", parent.display()))?;
    }
    std::fs::write(&path, text).map_err(|e| format!("写 {} 失败: {e}", path.display()))?;
    Ok(path)
}

/// 检查 → 通过则把 id 加进点名清单（幂等：已在清单里只提示不重复加）。
///
/// * `set_active`：同时把 `active` 指到本方案（刚装完想立刻用就传 true）。
/// * 返回 `(报告, 是否新加入)`。
pub fn admit(
    path: &Path,
    user_data_dir: &Path,
    set_active: bool,
) -> Result<(AdmitReport, bool), String> {
    let report = check_schema_file(path);
    if !report.passed() {
        return Ok((report, false));
    }
    let id = report
        .schema_id
        .clone()
        .ok_or_else(|| "通过了检查却取不到 id（内部错误）".to_string())?;

    let mut manifest = load_schema_manifest(user_data_dir);
    if manifest.schemas.iter().any(|s| s == &id) {
        if set_active && manifest.active.as_deref() != Some(id.as_str()) {
            manifest.active = Some(id.clone());
            save_schema_manifest(user_data_dir, &manifest)?;
        }
        return Ok((report, false)); // 已在清单，不重复加
    }
    manifest.schemas.push(id.clone());
    if set_active {
        manifest.active = Some(id);
    }
    save_schema_manifest(user_data_dir, &manifest)?;
    Ok((report, true))
}

/// CLI 用的用户目录兜底：`--user` > `RIME_USER_DIR` > `%APPDATA%\Rime-Next`。
pub fn default_user_data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("RIME_USER_DIR") {
        let dir = PathBuf::from(dir);
        if !dir.as_os_str().is_empty() {
            return Some(dir);
        }
    }
    std::env::var_os("APPDATA").map(|appdata| Path::new(&appdata).join("Rime-Next"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("librime-next-admit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// 造一份合规方案文件：UTF-8 无 BOM、LF、标记齐全、id 与文件名一致、摆放正确。
    fn good_schema(root: &Path, id: &str) -> PathBuf {
        let dir = root.join(SCHEMA_DIR).join(id.replace('.', "/"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.schema.yaml"));
        std::fs::write(
            &path,
            format!(
                "# Rime schema\n# encode: utf-8\n# newline: lf\n---\nschema:\n  id: \
                 {id}\n  name: 测试方案\n...\n"
            ),
        )
        .unwrap();
        path
    }

    fn item<'a>(report: &'a AdmitReport, name: &str) -> &'a CheckItem {
        report
            .items
            .iter()
            .find(|i| i.name == name)
            .unwrap_or_else(|| panic!("报告里没有检查项 `{name}`"))
    }

    /// 造一份合规词库：头三行标记 + `id/name/version`。
    fn good_dict(root: &Path, id: &str) -> PathBuf {
        let dir = root.join(DICT_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.dict.yaml"));
        std::fs::write(
            &path,
            format!(
                "# Rime Dictionary\n# encode: utf-8\n# newline: lf\n---\nid: {id}\nname: {id}\nversion: 2026.10.28\n...\n"
            ),
        )
        .unwrap();
        path
    }

    /// 词库摆放 = 「`dict/` + id 分段的某个前缀」，片段词库不必各建文件夹。
    #[test]
    fn dict_placement_accepts_id_prefixes() {
        let root = temp_dir("dict-prefix");
        let id = "cangjie.5.base";

        // 每一层前缀都合格
        for under in ["", "cangjie", "cangjie/5", "cangjie/5/base"] {
            let dir = if under.is_empty() {
                root.join(DICT_DIR)
            } else {
                root.join(DICT_DIR).join(under)
            };
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("{id}.dict.yaml"));
            std::fs::write(&path, "x").unwrap();
            assert_eq!(
                dict_placement(&path, &root, id),
                Ok(under.to_string()),
                "dict/{under} 应是合法前缀"
            );
        }

        // 不是前缀 → 不合格（写错系列）
        let wrong = root.join(DICT_DIR).join("wubi/86");
        std::fs::create_dir_all(&wrong).unwrap();
        let path = wrong.join(format!("{id}.dict.yaml"));
        std::fs::write(&path, "x").unwrap();
        assert!(dict_placement(&path, &root, id).is_err(), "dict/wubi/86 不是前缀");

        // 比 id 更深的目录也不合格（多了分段）
        let deeper = root.join(DICT_DIR).join("cangjie/5/base/x");
        std::fs::create_dir_all(&deeper).unwrap();
        let path = deeper.join(format!("{id}.dict.yaml"));
        std::fs::write(&path, "x").unwrap();
        assert!(dict_placement(&path, &root, id).is_err(), "多一层分段不合格");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 词库可以平铺在 `dict/` 下；不在 `dict/` 下则不合格。
    #[test]
    fn dict_outside_dict_dir_fails() {
        let root = temp_dir("dict-outside");
        let dir = root.join(SCHEMA_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.dict.yaml");
        std::fs::write(&path, "x").unwrap();
        assert!(dict_placement(&path, &root, "a").is_err());

        let flat = good_dict(&root, "a");
        assert_eq!(dict_placement(&flat, &root, "a"), Ok(String::new()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn good_file_passes_all_gates() {
        let root = temp_dir("good");
        let path = good_schema(&root, "pinyin");
        let report = check_schema_file(&path);
        assert!(report.passed(), "{}", format_report(&report));
        assert_eq!(report.schema_id.as_deref(), Some("pinyin"));
        assert_eq!(report.schema_name.as_deref(), Some("测试方案"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_markers_fail() {
        let root = temp_dir("no-markers");
        let dir = root.join(SCHEMA_DIR).join("nomark");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nomark.schema.yaml");
        std::fs::write(&path, "---\nschema:\n  id: nomark\n  name: x\n...\n").unwrap();
        let report = check_schema_file(&path);
        assert!(!report.passed());
        assert!(!item(&report, "encode 标记").ok);
        assert!(!item(&report, "newline 标记").ok);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn crlf_lying_about_lf_fails() {
        let root = temp_dir("crlf");
        let dir = root.join(SCHEMA_DIR).join("crlf");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("crlf.schema.yaml");
        std::fs::write(
            &path,
            "# encode: utf-8\n# newline: lf\n---\r\nschema:\r\n  id: crlf\r\n...\r\n",
        )
        .unwrap();
        let report = check_schema_file(&path);
        assert!(!item(&report, "实际换行").ok, "声明 lf 却有 CRLF 必须挂");
        assert!(!report.passed());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bom_fails() {
        let root = temp_dir("bom");
        let dir = root.join(SCHEMA_DIR).join("bom");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bom.schema.yaml");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"# encode: utf-8\n# newline: lf\n---\nschema:\n  id: bom\n...\n");
        std::fs::write(&path, bytes).unwrap();
        let report = check_schema_file(&path);
        assert!(!item(&report, "实际编码").ok);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn native_schema_id_key_rejected() {
        // 强制方言单数：写 librime 原生 schema_id 一律拒收
        let root = temp_dir("native-id");
        let dir = root.join(SCHEMA_DIR).join("aaa");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("aaa.schema.yaml");
        std::fs::write(
            &path,
            "# encode: utf-8\n# newline: lf\n---\nschema:\n  schema_id: aaa\n  name: x\n...\n",
        )
        .unwrap();
        let report = check_schema_file(&path);
        assert!(!item(&report, "schema.id 写法").ok, "原生 schema_id 必须拒收");
        assert!(!report.passed());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn both_id_and_schema_id_rejected() {
        // 两个键同时写：归一化会静默丢弃 id，必须拒收
        let root = temp_dir("both-id");
        let dir = root.join(SCHEMA_DIR).join("aaa");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("aaa.schema.yaml");
        std::fs::write(
            &path,
            "# encode: utf-8\n# newline: lf\n---\nschema:\n  id: aaa\n  schema_id: aaa\n  name: x\n...\n",
        )
        .unwrap();
        let report = check_schema_file(&path);
        assert!(!item(&report, "schema.id 写法").ok, "id/schema_id 混写必须拒收");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn id_mismatch_with_file_name_fails() {
        let root = temp_dir("mismatch");
        let dir = root.join(SCHEMA_DIR).join("aaa");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("aaa.schema.yaml");
        std::fs::write(
            &path,
            "# encode: utf-8\n# newline: lf\n---\nschema:\n  id: bbb\n  name: x\n...\n",
        )
        .unwrap();
        let report = check_schema_file(&path);
        assert!(!item(&report, "id 与文件名一致").ok);
        assert!(!report.passed());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn flat_placement_fails() {
        // 零扁平回退：schema/aaa.schema.yaml 是错摆
        let root = temp_dir("flat");
        let dir = root.join(SCHEMA_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("aaa.schema.yaml");
        std::fs::write(
            &path,
            "# encode: utf-8\n# newline: lf\n---\nschema:\n  id: aaa\n  name: x\n...\n",
        )
        .unwrap();
        let report = check_schema_file(&path);
        assert!(!item(&report, "系列摆放").ok);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn series_id_two_levels_pass() {
        let root = temp_dir("series");
        let path = good_schema(&root, "yuling.five");
        let report = check_schema_file(&path);
        assert!(report.passed(), "{}", format_report(&report));
        assert_eq!(report.schema_id.as_deref(), Some("yuling.five"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn admit_appends_once_and_sets_active() {
        let root = temp_dir("admit");
        let user = root.join("user");
        std::fs::create_dir_all(&user).unwrap();
        let path = good_schema(&root, "pinyin");

        let (report, added) = admit(&path, &user, true).expect("准入应成功");
        assert!(report.passed());
        assert!(added, "第一次应新加入");
        let manifest = load_schema_manifest(&user);
        assert_eq!(manifest.schemas, vec!["pinyin".to_string()]);
        assert_eq!(manifest.active.as_deref(), Some("pinyin"));

        // 幂等：再加一次不重复
        let (_, added2) = admit(&path, &user, false).expect("重复准入应成功");
        assert!(!added2);
        let manifest = load_schema_manifest(&user);
        assert_eq!(manifest.schemas, vec!["pinyin".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bad_file_never_touches_manifest() {
        let root = temp_dir("reject");
        let user = root.join("user");
        std::fs::create_dir_all(&user).unwrap();
        // 先在清单里放一个别的方案，确保不被清掉
        save_schema_manifest(&user, &SchemaManifest {
            active: Some("luna_pinyin_simp".into()),
            schemas: vec!["luna_pinyin_simp".into()],
        })
        .expect("写清单");
        // 缺标记的坏方案
        let dir = root.join(SCHEMA_DIR).join("bad");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.schema.yaml");
        std::fs::write(&path, "---\nschema:\n  id: bad\n...\n").unwrap();

        let (report, added) = admit(&path, &user, false).expect("检查应跑完");
        assert!(!report.passed());
        assert!(!added);
        let manifest = load_schema_manifest(&user);
        assert_eq!(manifest.schemas, vec!["luna_pinyin_simp".to_string()], "坏方案不能进清单，也不能动老清单");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn header_decl_parsing() {
        let decl = parse_header_decl("# Rime schema\n# encode: utf-8\n# newline: lf\n---\nschema:\n");
        assert_eq!(decl.encode.as_deref(), Some("utf-8"));
        assert_eq!(decl.newline.as_deref(), Some("lf"));
        // 注释头之后的 encode: 不算（YAML 正文里的键不能误当文件头）
        let decl2 = parse_header_decl("---\nencode: utf-8\n");
        assert_eq!(decl2.encode, None);
    }

    #[test]
    fn newline_counts() {
        assert_eq!(count_newlines(b"a\nb\n"), (0, 2, 0));
        assert_eq!(count_newlines(b"a\r\nb\r\n"), (2, 0, 0));
        assert_eq!(count_newlines(b"a\r\nb\n"), (1, 1, 0));
        assert_eq!(count_newlines(b"a\rb"), (0, 0, 1));
    }

    #[test]
    fn id_sanity() {
        assert!(id_is_sane("pinyin"));
        assert!(id_is_sane("yuling.five"));
        assert!(id_is_sane("terra_pinyin-2"));
        assert!(!id_is_sane(""));
        assert!(!id_is_sane(".pinyin"));
        assert!(!id_is_sane("pinyin."));
        assert!(!id_is_sane("a..b"));
        assert!(!id_is_sane("pin yin"));
        assert!(!id_is_sane("拼/yin"));
    }
}
