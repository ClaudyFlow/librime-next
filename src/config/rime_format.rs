//! `# Rime <Kind>` 魔数头识别 + 方案/词典格式自动检测（通用工具）。
//!
//! Rime 系 yaml 按约定在文件开头带魔数注释头：`# Rime Schema`、
//! `# Rime Dictionary`、`# Rime default settings`……但实测仓内写法相当不齐：
//!
//! ```text
//! # Rime Schema                      ← 规范
//! # Rime schema 输入方案配置文件      ← 小写 + 说明后缀
//! #Rime schema settings              ← 没有空格
//! # rime dictionary: 基础词库        ← 小写 r + 冒号后缀
//! # Rime dict: 通用字8105            ← 简写 dict
//! # Rime Schema — 明月（主方案…）     ← 破折号长说明
//! # encoding: utf-8                  ← 编码行占首行，魔数头退居其后
//! 呵	kk,kk,…                        ← 个别词典干脆没有头，直接裸词条
//! ```
//!
//! 本模块做一个**通用**的识别函数（读纯文本、不依赖加载器）：
//!
//! 1. [`sniff`]：容错嗅探魔数头 → [`RimeKind`] 分类 + 原始行 + 说明后缀；
//! 2. [`validate`]：按识别出的种类做轻量结构校验（`---` 文档结构、`schema.id`、
//!    词典头块 + Tab 词条）——「识别方案后自动检测格式」；
//! 3. [`analyze`]：文件级一步到位（嗅探 + 校验 + 文件名↔魔数头交叉核对）。
//!
//! 典型用途：部署器入鹅前验格式、CLI `doctor`、批量体检报告（见
//! `example/rime_format_report.rs`）。

use std::path::Path;

/// 魔数头识别出的文件种类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RimeKind {
    /// `# Rime Schema …`：输入方案。
    Schema,
    /// `# Rime Dictionary / dict / dictionary …`：词典。
    Dict,
    /// `# Rime default settings`：全局默认（`default.yaml`）。
    Default,
    /// `# Rime key bindings`：键绑定。
    KeyBindings,
    /// `# Rime Eassy`：easy 匹配词库。
    Eassy,
    /// `# Rime's punctuation and symbols`：符号表。
    Symbols,
    /// 带 `# Rime` 头但不属于以上任何一种（保留说明后缀）。
    Other(String),
    /// 没有 `# Rime` 魔数头。
    None,
}

impl RimeKind {
    /// 展示名（报告/日志用）。
    pub fn label(&self) -> String {
        match self {
            RimeKind::Schema => "Schema（方案）".into(),
            RimeKind::Dict => "Dict（词典）".into(),
            RimeKind::Default => "Default（全局默认）".into(),
            RimeKind::KeyBindings => "KeyBindings（键绑定）".into(),
            RimeKind::Eassy => "Eassy（easy 词库）".into(),
            RimeKind::Symbols => "Symbols（符号表）".into(),
            RimeKind::Other(note) => format!("Other（# Rime {note}）"),
            RimeKind::None => "无魔数头".into(),
        }
    }

    /// 是否真的嗅到了 `# Rime` 头。
    pub fn present(&self) -> bool {
        !matches!(self, RimeKind::None)
    }
}

/// 嗅探结果：种类 + 在第几行 + 原始头行 + 种类词之后的说明后缀。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RimeHeader {
    pub kind: RimeKind,
    /// 魔数头所在行（1-based）；`None` 种类时为 0。
    pub line_no: usize,
    /// 原始头行（去首尾空白后）。
    pub raw: String,
    /// 种类词之后的说明后缀（原样保留；如 `— 明月（主方案…）`、`输入方案配置文件`）。
    pub note: String,
}

impl Default for RimeHeader {
    fn default() -> Self {
        RimeHeader {
            kind: RimeKind::None,
            line_no: 0,
            raw: String::new(),
            note: String::new(),
        }
    }
}

/// 校验结论级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// 结构符合预期。
    Ok,
    /// 可疑但不必然错。
    Warn,
    /// 破坏性结构错误（加载多半会失败）。
    Error,
}

impl Level {
    pub fn badge(&self) -> &'static str {
        match self {
            Level::Ok => "✅",
            Level::Warn => "⚠️",
            Level::Error => "❌",
        }
    }
}

/// 一条校验结论。
#[derive(Debug, Clone)]
pub struct Finding {
    pub level: Level,
    pub message: String,
}

impl Finding {
    fn ok(message: impl Into<String>) -> Self {
        Finding { level: Level::Ok, message: message.into() }
    }
    fn warn(message: impl Into<String>) -> Self {
        Finding { level: Level::Warn, message: message.into() }
    }
    fn error(message: impl Into<String>) -> Self {
        Finding { level: Level::Error, message: message.into() }
    }
}

/// [`analyze`] 的一步到位结果。
#[derive(Debug, Clone)]
pub struct Analysis {
    pub header: RimeHeader,
    pub findings: Vec<Finding>,
}

impl Analysis {
    /// 汇总一行（报告用）：`✅ Schema（方案）· schema.id=pinyin · 3 项检查全过`。
    pub fn summary(&self) -> String {
        let errors = self.findings.iter().filter(|f| f.level == Level::Error).count();
        let warns = self.findings.iter().filter(|f| f.level == Level::Warn).count();
        let oks = self.findings.iter().filter(|f| f.level == Level::Ok).count();
        let mut s = self.header.kind.label();
        match errors + warns {
            0 => s.push_str(&format!(" · {oks} 项检查全过")),
            _ => {
                if errors > 0 {
                    s.push_str(&format!(" · {errors} 项错误"));
                }
                if warns > 0 {
                    s.push_str(&format!(" · {warns} 项警告"));
                }
            }
        }
        s
    }
}

/// 识别一行是不是 `# Rime <Kind>` 魔数头。
///
/// 容错点：`#` 后有无空格、`rime` 大小写、种类词后跟 `:`/`'s`/说明后缀都接受；
/// 返回 `None` 表示该行不是魔数头（调用方决定继续跳过还是停止扫描）。
fn parse_header_line(line: &str) -> Option<RimeHeader> {
    let trimmed = line.trim();
    let body = trimmed.strip_prefix('#')?.trim_start();
    let lower = body.to_lowercase();
    let rest = lower.strip_prefix("rime")?;
    // `Rime's …`：种类词直接跟 `'s`（符号表的老写法）。
    if rest.starts_with("'s") {
        return Some(RimeHeader {
            kind: RimeKind::Symbols,
            line_no: 0, // 由调用方回填
            raw: trimmed.to_string(),
            note: body["rime".len()..].trim().to_string(),
        });
    }
    let note_full = body["rime".len()..].trim();
    let word = rest
        .trim_start()
        .split(|c: char| c.is_whitespace())
        .next()
        .unwrap_or("")
        .trim_end_matches(':');
    let kind = match word {
        "" => RimeKind::Other(String::new()),
        w if w.starts_with("schema") => RimeKind::Schema,
        "dict" | "dictionary" => RimeKind::Dict,
        "default" => RimeKind::Default,
        "key" => RimeKind::KeyBindings,
        "eassy" => RimeKind::Eassy,
        other => RimeKind::Other(other.to_string()),
    };
    // note 去掉种类词本身，只留说明后缀。
    let note = {
        let full = note_full.to_string();
        let lower_full = full.to_lowercase();
        let stem = match &kind {
            RimeKind::Schema => "schema",
            RimeKind::Dict => "dict",
            RimeKind::Default => "default",
            RimeKind::KeyBindings => "key",
            RimeKind::Eassy => "eassy",
            RimeKind::Symbols | RimeKind::None => "",
            RimeKind::Other(w) => w.as_str(),
        };
        if stem.is_empty() {
            full.trim().to_string()
        } else if let Some(idx) = lower_full.find(stem) {
            full[idx + stem.len()..]
                .trim_start_matches([':', ' '])
                .trim()
                .to_string()
        } else {
            full
        }
    };
    Some(RimeHeader { kind, line_no: 0, raw: trimmed.to_string(), note })
}

/// 从 yaml 纯文本里嗅探 `# Rime` 魔数头。
///
/// 扫描规则：从首行起，空行与 `# encoding/encode:` 行跳过；注释行里第一条
/// **非**魔数头的注释即停（那是有意义的文件说明，后面不会再有魔数头）；
/// 第一条非注释非空行（`---`、`patch:`、裸词条…）也停。
pub fn sniff(text: &str) -> RimeHeader {
    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(body) = trimmed.strip_prefix('#') {
            let lower = body.trim_start().to_lowercase();
            // 编码声明行只是元信息，魔数头可能在它后面。
            if lower.starts_with("encoding:") || lower.starts_with("encode:") {
                continue;
            }
            if let Some(mut header) = parse_header_line(line) {
                header.line_no = idx + 1;
                return header;
            }
            // 其它注释（文件说明）出现 ⇒ 魔数头不在文件头，停。
            return RimeHeader::default();
        }
        // 第一条实体行（`---` / `patch:` / 裸词条）之前都没看到魔数头。
        return RimeHeader::default();
    }
    RimeHeader::default()
}

/// [`sniff`] 的文件版。
pub fn sniff_file(path: &Path) -> std::io::Result<RimeHeader> {
    let text = std::fs::read_to_string(path)?;
    Ok(sniff(&text))
}

/// 在 `from` 行（1-based）之后找第一条列 0 的 `marker` 行，返回其行号（1-based）。
fn find_doc_marker(lines: &[&str], from: usize, markers: &[&str]) -> Option<usize> {
    lines[from..]
        .iter()
        .position(|l| markers.contains(&l.trim_end()))
        .map(|i| from + i)
}

/// 剥掉行内注释：`"092"  #方案ID` → `"092"`（只在 `#` 前有空白时才算注释）。
fn strip_inline_comment(value: &str) -> String {
    let mut out = String::new();
    let mut prev_space = false;
    for c in value.chars() {
        if c == '#' && prev_space {
            break;
        }
        prev_space = c.is_whitespace();
        out.push(c);
    }
    out.trim_end().to_string()
}

/// 第一条**实体行**（非空、非注释、非 `---`/`...` 文档标记）的下标；注释前言再长也不影响。
fn first_entity(lines: &[&str]) -> Option<usize> {
    lines.iter().position(|l| {
        let t = l.trim();
        !t.is_empty() && !t.starts_with('#') && t != "---" && t != "..."
    })
}

/// 按识别出的种类做轻量结构校验（「识别方案后自动检测格式」）。
///
/// 只查会**直接导致加载失败**的结构，不做完整 YAML 解析：
/// * Schema：`---` 文档起始、列 0 的 `schema:` 块、块内 `id:`；
/// * Dict：`---` 头块、头块内 `name:`、头块结束符（`---`/`...`）、
///   之后每条词条至少一个 Tab 分隔；
/// * 其余种类：只确认有实体内容。
pub fn validate(header: &RimeHeader, text: &str) -> Vec<Finding> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    match &header.kind {
        RimeKind::Schema => {
            match find_doc_marker(&lines, 0, &["---"]) {
                Some(doc) if first_entity(&lines).map_or(true, |e| doc < e) => out.push(
                    Finding::ok(format!("第 {} 行 `---` 文档起始（在实体内容之前）", doc + 1)),
                ),
                Some(doc) => out.push(Finding::error(format!(
                    "第 {} 行才有 `---`，却已在实体内容之后（文档起始标记必须在正文前）",
                    doc + 1
                ))),
                // YAML 单文档可省略 `---`（仓内 092/李诗歌魔改系即无 ---），但 Rime 惯例应带。
                None => out.push(Finding::warn(
                    "全文没有 `---` 文档起始（YAML 单文档可省略，但 Rime 惯例应带）",
                )),
            }
            match lines.iter().position(|l| {
                let l = l.trim_end();
                l == "schema:" || l.starts_with("schema:")
            }) {
                Some(s) => {
                    out.push(Finding::ok(format!("第 {} 行找到 `schema:` 块", s + 1)));
                    // 块内找 id:（缩进任意，直到下一个列 0 键）。
                    let id = lines[s + 1..]
                        .iter()
                        .take_while(|l| l.starts_with([' ', '\t']) || l.trim().is_empty())
                        .find_map(|l| {
                            let t = l.trim();
                            t.starts_with("id:").then(|| strip_inline_comment(t[3..].trim()))
                        });
                    match id.filter(|v| !v.is_empty()) {
                        Some(v) => out.push(Finding::ok(format!("schema.id = `{v}`"))),
                        None => out.push(Finding::error("`schema:` 块内没有 `id:`（部署器靠它命名编译产物）")),
                    }
                }
                None => out.push(Finding::error("整份文件没有列 0 的 `schema:` 块")),
            }
        }
        RimeKind::Dict => {
            let doc = match find_doc_marker(&lines, 0, &["---"]) {
                Some(d) if first_entity(&lines).map_or(true, |e| d < e) => {
                    out.push(Finding::ok(format!("第 {} 行 `---` 头块起始", d + 1)));
                    d
                }
                Some(d) => {
                    out.push(Finding::error(format!(
                        "第 {} 行才有 `---`，却已在实体内容之后（词典头块必须在词条前）",
                        d + 1
                    )));
                    return out;
                }
                _ => {
                    out.push(Finding::error(
                        "全文没有 `---` 头块起始（词典必须先有 yaml 头块再上词条）",
                    ));
                    return out;
                }
            };
            let name = lines[doc..].iter().take(15).find_map(|l| {
                let t = l.trim();
                t.starts_with("name:").then(|| strip_inline_comment(t[5..].trim()))
            });
            match name.filter(|v| !v.is_empty()) {
                Some(v) => out.push(Finding::ok(format!("词典 name = `{v}`"))),
                None => out.push(Finding::error("头块前 15 行内没有 `name:`")),
            }
            // 头块结束：`...` 或下一条 `---`。
            let sep = find_doc_marker(&lines, doc + 1, &["...", "---"]);
            match sep {
                Some(sep) => {
                    out.push(Finding::ok(format!(
                        "第 {} 行 `{}` 结束头块",
                        sep + 1,
                        lines[sep].trim_end()
                    )));
                    let mut entries = 0usize;
                    let mut bad: Vec<String> = Vec::new();
                    for l in &lines[sep + 1..] {
                        let t = l.trim();
                        if t.is_empty() || t.starts_with('#') {
                            continue;
                        }
                        entries += 1;
                        if !t.contains('\t') {
                            if bad.len() < 3 {
                                bad.push(t.chars().take(24).collect());
                            }
                        }
                    }
                    // packs/导入型词典：头块声明 import_tables / vocabulary，词条
                    // 在被导入的子词典里，头块后无词条是合法形态（仓内 qiaoma 系）。
                    let imports = lines[doc..sep].iter().any(|l| {
                        let t = l.trim_end();
                        t == "import_tables:" || t.starts_with("import_tables:")
                            || t == "vocabulary:" || t.starts_with("vocabulary:")
                    });
                    if entries == 0 && imports {
                        out.push(Finding::ok("导入型词典（import_tables/vocabulary），头块后无词条属正常"));
                    } else if entries == 0 {
                        out.push(Finding::warn("头块之后没有词条行（若是 packs 导入型词典，头块里应写 `import_tables:`）"));
                    } else if bad.is_empty() {
                        out.push(Finding::ok(format!("{entries} 条词条均为 Tab 分隔")));
                    } else {
                        out.push(Finding::warn(format!(
                            "{entries} 条词条中有 {} 条缺 Tab 分隔，如：{}",
                            bad.len(),
                            bad.join("、")
                        )));
                    }
                }
                None => out.push(Finding::error("头块之后没有 `...` / `---` 分隔符（词条没法和元数据分开）")),
            }
        }
        RimeKind::None => out.push(Finding::warn("没有 `# Rime` 魔数头，跳过结构校验")),
        _ => {
            if lines.iter().any(|l| !l.trim().is_empty()) {
                out.push(Finding::ok("文件有实体内容"));
            } else {
                out.push(Finding::warn("文件是空的"));
            }
        }
    }
    out
}

/// 文件级一步到位：嗅探 + 校验 + **文件名↔魔数头交叉核对**。
///
/// 交叉核对规则（`xxx.schema.yaml` / `xxx.dict.yaml` 的命名约定）：
/// * 扩展名说是 schema 而魔数头不是 Schema ⇒ Error（反之亦然）；
/// * 扩展名说是 dict 而魔数头不是 Dict ⇒ Error（反之亦然）。
pub fn analyze(path: &Path) -> std::io::Result<Analysis> {
    let text = std::fs::read_to_string(path)?;
    let header = sniff(&text);
    let mut findings = validate(&header, &text);
    let file = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if file.ends_with(".schema.yaml") && !matches!(header.kind, RimeKind::Schema) {
        findings.push(Finding::error(format!(
            "文件名按 `.schema.yaml` 命名，但魔数头是 {}",
            header.kind.label()
        )));
    }
    if file.ends_with(".dict.yaml") && !matches!(header.kind, RimeKind::Dict) {
        findings.push(Finding::error(format!(
            "文件名按 `.dict.yaml` 命名，但魔数头是 {}",
            header.kind.label()
        )));
    }
    if matches!(header.kind, RimeKind::Schema) && !file.ends_with(".schema.yaml") && !file.is_empty()
    {
        findings.push(Finding::warn(format!(
            "魔数头是 Schema，但文件名不是 `*.schema.yaml`（{file}）"
        )));
    }
    Ok(Analysis { header, findings })
}

// ---------------------------------------------------------------------------
// 示例函数：校验「同前缀同后缀、中间不同」的文件组（流程函数一的核心）
// ---------------------------------------------------------------------------

/// 校验一个目录里的同名组文件，规则（按口述流程逐步落实）：
///
/// 输入形态：`dict/pinyin/` 下有 `pinyin.dict.yaml`、`pinyin.cuoyin.dict.yaml`、
/// `pinyin.diming.dict.yaml`…——前缀 `pinyin` 相同、后缀 `.dict.yaml` 相同、
/// 中间段（`cuoyin`/`diming`/…）不同。同级还可能有子文件夹（`luna/`）。
///
/// 1. **前缀对不对**：组的公共前缀（点分段）必须等于目录在类型目录下的
///    点分路径（`dict/pinyin/` → `pinyin`；`dict/cangjie/5/` → `cangjie.5`）。
/// 2. **中间段 vs 同级文件夹**：每个带中间段的文件，若它的中间段与同目录
///    里某个子文件夹同名（如 `pinyin.luna.dict.yaml` vs `luna/`），说明这个
///    文件属于那个文件夹的辖域，不该放在这里（luna 的本体在 `luna/` 里）；
///    **没有**同名文件夹（如 `cuoyin`）就说明文件放这里是对的。
/// 3. **最短的是否合适**：组内最短文件名应当是「无中间段」的本体文件
///    （`pinyin.dict.yaml`），且其前缀正好等于目录点分路径——它是这一堆
///    的主词典；若最短的还带中间段，说明本体缺失或组前缀推断有问题。
///
/// `dir_rel`：类型目录下的相对路径（`"pinyin"`、`"cangjie/5"`）；`kind`：
/// 类型后缀（`"dict"`/`"schema"`/…）；`names`：该目录下匹配
/// `*.{kind}.yaml` 的文件名；`sub_dirs`：该目录下的子文件夹名。
pub fn check_prefix_group(
    dir_rel: &str,
    kind: &str,
    names: &[String],
    sub_dirs: &[String],
) -> Vec<Finding> {
    let mut out = Vec::new();
    if names.is_empty() {
        return out;
    }
    let suffix = format!(".{kind}.yaml");

    // 每个文件的点分前缀段（去掉后缀）。
    let prefixes: Vec<Vec<&str>> = names
        .iter()
        .map(|n| n.strip_suffix(&suffix).unwrap_or(n).split('.').collect())
        .collect();
    let dir_chain: Vec<&str> = dir_rel.split('/').filter(|s| !s.is_empty()).collect();

    // 规则①：组的公共前缀（按段取最长公共）。
    let mut common: Vec<&str> = Vec::new();
    'outer: for i in 0..prefixes[0].len() {
        let seg = prefixes[0][i];
        for p in &prefixes {
            if p.get(i) != Some(&seg) {
                break 'outer;
            }
        }
        common.push(seg);
    }
    let common_str = common.join(".");
    if common.is_empty() {
        // 组内连首段都不同（如 pinyin.schema.yaml + wanxiang.schema.yaml 混居），
        // 逐个核对：文件前缀必须以目录链开头（文件应住在自己前缀对应的目录里）。
        out.push(Finding::warn("组内无公共前缀，逐个核对目录归属"));
        for (name, segs) in names.iter().zip(&prefixes) {
            if segs.len() >= dir_chain.len() && segs[..dir_chain.len()] == *dir_chain {
                out.push(Finding::ok(format!(
                    "{name}：前缀以目录路径 `{dir_rel}` 开头，目录归属正确"
                )));
            } else {
                out.push(Finding::error(format!(
                    "{name}：前缀 `{}` ≠ 目录路径 `{dir_rel}`（前缀与目录不符）",
                    segs.join(".")
                )));
            }
        }
    } else if common == dir_chain {
        out.push(Finding::ok(format!(
            "组公共前缀 `{common_str}` = 目录路径 `{dir_rel}`（前缀正确）"
        )));
    } else {
        out.push(Finding::error(format!(
            "组公共前缀 `{common_str}` ≠ 目录路径 `{dir_rel}`（前缀与目录不符）"
        )));
    }

    // 规则②：中间段 vs 同级子文件夹（相对目录链算，而非公共前缀——
    // 公共前缀为空的混合组里 dir_chain 之前的段不是「中间」）。
    for (name, segs) in names.iter().zip(&prefixes) {
        let base = common.len().max(dir_chain.len()).min(segs.len());
        for m in &segs[base..] {
            if sub_dirs.iter().any(|d| d == m) {
                out.push(Finding::error(format!(
                    "{name}：中间段 `{m}` 与同级文件夹 `{m}/` 同名——它应放进 `{m}/` 里，不该留在本目录"
                )));
            } else {
                out.push(Finding::ok(format!(
                    "{name}：中间段 `{m}` 无同名文件夹，作为本目录的平行子词典正确"
                )));
            }
        }
    }

    // 规则③：最短的那个（段数最少）应是「无中间段」的本体。
    let shortest = names
        .iter()
        .zip(&prefixes)
        .min_by_key(|(_, segs)| segs.len())
        .unwrap();
    if shortest.1.len() == dir_chain.len() {
        out.push(Finding::ok(format!(
            "最短文件 `{}` 无中间段，且前缀 = 目录路径，是本组主文件",
            shortest.0
        )));
    } else {
        out.push(Finding::warn(format!(
            "最短文件 `{}` 仍带中间段（{}）——目录 `{dir_rel}` 下缺少本体 `{}/.{kind}.yaml` 形态的主文件",
            shortest.0,
            shortest.1.join("."),
            dir_chain.join(".")
        )));
    }
    out
}

// ---------------------------------------------------------------------------
// 文件夹层级检测：10 步流程（根目录→分类→同类型文件→ID 解析→共存→位置→逐级）
// ---------------------------------------------------------------------------

/// 层级检测的类别：中文名 ↔ 仓内目录名 + 实体文件后缀。
#[derive(Debug, Clone, Copy)]
pub struct HierarchyCategory {
    /// 中文名（报告/日志用）。
    pub label: &'static str,
    /// 仓内一级目录名。
    pub dir: &'static str,
    /// 实体文件后缀（yaml/yml 两种）。
    pub suffixes: [&'static str; 2],
}

/// 三个分类（用户流程：方案/皮肤/词库）。
pub const HIERARCHY_CATEGORIES: [HierarchyCategory; 3] = [
    HierarchyCategory { label: "方案", dir: "schema", suffixes: [".schema.yaml", ".schema.yml"] },
    HierarchyCategory { label: "皮肤", dir: "theme", suffixes: [".theme.yaml", ".theme.yml"] },
    HierarchyCategory { label: "词库", dir: "dict", suffixes: [".dict.yaml", ".dict.yml"] },
];

/// 测试/样例文件前缀：这些文件不参与层级与内容合规检测。
/// `wanxiang`（万象）是开发期样例方案，跳过以避免误报位置错误。
const IGNORED_NAME_PREFIXES: &[&str] = &["wanxiang"];

/// 文件名是否以某个忽略前缀开头（测试/样例文件）。
fn is_ignored_name(name: &str) -> bool {
    IGNORED_NAME_PREFIXES.iter().any(|p| {
        name.starts_with(p)
            && (name.len() == p.len() || name[p.len()..].starts_with('.'))
    })
}

/// 一个候选实体文件的层级检查结果。
#[derive(Debug, Clone)]
pub struct FileCheck {
    /// 相对数据根的路径（`schema/pinyin/pinyin.schema.yaml`）。
    pub rel_path: String,
    /// 从文件名解析出的完整 ID（去掉 `.{类型}.yaml`）。
    pub id: String,
    /// 文件所在目录在分类目录下的点分链（位置检查用）。
    pub dir_chain: Vec<String>,
    /// 基础合法性（格式 + 位置 + 所在文件夹无共存冲突 + 内容 id 字段）。
    pub legal: bool,
    pub findings: Vec<Finding>,
}

/// 一个分类的层级报告。
#[derive(Debug, Clone, Default)]
pub struct CategoryReport {
    pub label: String,
    /// 分类级结论（文件夹存在性、共存冲突整目录忽略等）。
    pub findings: Vec<Finding>,
    /// 进入逐级检查的候选文件。
    pub files: Vec<FileCheck>,
    /// 缺失层级（允许缺失，仅提示）。
    pub missing_levels: Vec<String>,
    /// 非法层级（存在但不合法）。
    pub illegal_levels: Vec<String>,
    /// 最终合法列表（相对路径）。
    pub legal_list: Vec<String>,
}

/// 完整层级报告（根目录 + 三个分类）。
#[derive(Debug, Clone, Default)]
pub struct HierarchyReport {
    /// 第 1/2 步：根目录结论。
    pub root_findings: Vec<Finding>,
    pub categories: Vec<CategoryReport>,
}

/// 文件夹层级检测（用户 10 步流程，根目录不管——根下允许任意文件/文件夹，
/// 如 `default.yaml`/`aux_code.yaml` 等全局设置；只对分类目录内部做体检）。
/// `root` 为数据根，其下的 `schema/`、`theme/`、`dict/` 即 方案/皮肤/词库。
pub fn check_hierarchy(root: &Path) -> HierarchyReport {
    let mut report = HierarchyReport::default();

    // 分类文件夹存在才检，不存在静默跳过（根目录不管）。
    let cats: Vec<_> = HIERARCHY_CATEGORIES
        .iter()
        .map(|c| (*c, root.join(c.dir)))
        .filter(|(_, d)| d.is_dir())
        .collect();

    for (cat, type_dir) in cats {
        let mut crep = CategoryReport { label: cat.label.to_string(), ..Default::default() };
        // ---- 第 5/6/7 步：递归扫同类型实体文件，解析 ID，做共存检测 ----
        let mut candidates: Vec<FileCheck> = Vec::new();
        collect_category(&cat, &type_dir, &type_dir, &mut candidates, &mut crep.findings);

        // ---- 第 8 步：最短名称位置（修正版：锚 = 最短**存在**前缀，不一定是首段）----
        // 沿 ID 前缀链从短到长找第一个实际存在（同分类里有实体文件）的前缀当锚；
        // 一个都不存在则锚 = 文件自己。期望目录链 = 锚 ID 的点分段逐级目录。
        let id_set: std::collections::HashSet<String> =
            candidates.iter().map(|f| f.id.clone()).collect();
        for f in &mut candidates {
            if !f.legal {
                continue;
            }
            let segs: Vec<&str> = f.id.split('.').collect();
            let mut anchor_len = segs.len(); // 默认锚 = 自己
            for depth in 1..segs.len() {
                if id_set.contains(&segs[..depth].join(".")) {
                    anchor_len = depth;
                    break;
                }
            }
            let expected: Vec<&str> = segs[..anchor_len].to_vec();
            let actual: Vec<&str> = f.dir_chain.iter().map(|s| s.as_str()).collect();
            let anchor = expected.join(".");
            if actual == expected {
                f.findings.push(Finding::ok(format!(
                    "位置正确：锚 `{anchor}`，目录链 `{}/` 与锚的逐级目录一致",
                    actual.join("/")
                )));
            } else {
                let expected_s = format!("{}/{}", cat.dir, expected.join("/"));
                let actual_s = actual.join("/");
                f.findings.push(Finding::error(format!(
                    "位置错误：ID `{}` 锚 `{anchor}`，应放在 `{expected_s}/`，实际在 `{actual_s}/`",
                    f.id
                )));
                f.legal = false;
            }
        }

        // ---- 第 9 步：逐级检查 A → A.B → …（缺失允许，存在必须合法）----
        // ID → 基础合法性快照（供逐级检查找「上级是否存在且合法」）。
        let by_id: std::collections::HashMap<String, bool> = candidates
            .iter()
            .map(|f| (f.id.clone(), f.legal))
            .collect();
        for f in &mut candidates {
            let segs: Vec<&str> = f.id.split('.').collect();
            // 逐级检查 A → A.B → …（不含文件自己）。
            for depth in 1..segs.len() {
                let level = segs[..depth].join(".");
                match by_id.get(&level) {
                    Some(true) => {
                        f.findings.push(Finding::ok(format!(
                            "上级 `{level}` 存在且合法"
                        )));
                    }
                    Some(false) => {
                        f.findings.push(Finding::warn(format!(
                            "上级 `{level}` 存在但不合法（非法层级，单独记录）"
                        )));
                        crep.illegal_levels.push(level.clone());
                    }
                    None => {
                        f.findings.push(Finding::warn(format!(
                            "上级 `{level}` 缺失（允许缺失，仅提示）"
                        )));
                        if !crep.missing_levels.contains(&level) {
                            crep.missing_levels.push(level);
                        }
                    }
                }
            }
            // ---- 第 9 步附则：内容基础字段（id / name）----
            check_content_id(&f.rel_path, root, &cat, &f.id, &mut f.findings);
            f.legal = f.legal && f.findings.iter().all(|x| x.level != Level::Error);
        }

        // ---- 第 10 步：输出合法列表 ----
        for f in &candidates {
            if f.legal {
                crep.legal_list.push(f.rel_path.clone());
            }
        }
        crep.files = candidates;
        report.categories.push(crep);
    }
    report
}

/// 递归收集一个目录下的同类型实体文件（第 5/6/7/8 步）。
///
/// `dir` 当前目录；`type_dir` 分类根（算相对点分路径用）。
/// 共存冲突（本层实体文件 + 子文件夹）⇒ 整目录忽略（含全部后代）。
#[allow(clippy::too_many_arguments)]
fn collect_category(
    cat: &HierarchyCategory,
    type_dir: &Path,
    dir: &Path,
    out: &mut Vec<FileCheck>,
    findings: &mut Vec<Finding>,
) {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect::<Vec<_>>(),
        Err(e) => {
            findings.push(Finding::error(format!(
                "目录 `{}` 打不开：{e}", dir.display()
            )));
            return;
        }
    };
    let dir_dot = dir
        .strip_prefix(type_dir)
        .unwrap_or(dir)
        .to_string_lossy()
        .replace('\\', "/");
    let dir_chain: Vec<&str> = dir_dot.split('/').filter(|s| !s.is_empty()).collect();

    let mut own_files: Vec<(String, std::path::PathBuf)> = Vec::new();
    let mut sub_dirs: Vec<std::path::PathBuf> = Vec::new();
    for e in rd {
        let path = e.path();
        if path.is_dir() {
            sub_dirs.push(path);
        } else {
            let name = e.file_name().to_string_lossy().to_string();
            if cat.suffixes.iter().any(|s| name.ends_with(s)) {
                if is_ignored_name(&name) {
                    // 测试/样例文件不参与层级与内容合规检测。
                    findings.push(Finding::ok(format!(
                        "跳过测试/样例文件 `{name}`（不参与合规检测）"
                    )));
                } else {
                    own_files.push((name, path));
                }
            }
            // 不匹配后缀的文件不进候选（第 5 步），也不报错。
        }
    }

    // ---- 第 7 步：共存检测 —— 本层实体文件与子文件夹不能同时存在 ----
    // 本层实体 = 完整 ID 恰好等于本目录点分路径的文件（如 方案/A/A.方案.yaml）。
    let own_layer: Vec<&(String, std::path::PathBuf)> = own_files
        .iter()
        .filter(|(name, _)| {
            let id = strip_kind_suffix(name, &cat.suffixes);
            id.split('.').collect::<Vec<_>>() == dir_chain
        })
        .collect();
    if !own_layer.is_empty() && !sub_dirs.is_empty() {
        let names: Vec<&str> = own_layer.iter().map(|(n, _)| n.as_str()).collect();
        let subs: Vec<String> = sub_dirs
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        findings.push(Finding::error(format!(
            "共存冲突：`{dir_dot}`/ 同时有本层实体文件 [{}] 和子文件夹 [{}]——整目录（含后代）忽略",
            names.join(", "),
            subs.join(", ")
        )));
        return; // 整目录忽略，不下钻
    }

    for (name, path) in &own_files {
        // ---- 第 6 步：解析完整 ID ----
        let id = strip_kind_suffix(name, &cat.suffixes);
        let mut f = FileCheck {
            rel_path: path
                .strip_prefix(type_dir.parent().unwrap_or(type_dir))
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/"),
            id: id.to_string(),
            dir_chain: dir_chain.iter().map(|s| s.to_string()).collect(),
            legal: true,
            findings: Vec::new(),
        };
        if id.is_empty() || id.starts_with('.') || id.ends_with('.') {
            f.findings.push(Finding::error(format!(
                "文件名 `{name}` 解析不出完整 ID（{id:?}）——淘汰"
            )));
            f.legal = false;
        }
        // 第 8 步位置检查在 check_hierarchy 里做（需要全分类 ID 集合定锚）。
        out.push(f);
    }

    // 无共存冲突才下钻（子文件夹里可能还有更深实体）。
    for sub in sub_dirs {
        collect_category(cat, type_dir, &sub, out, findings);
    }
}

/// 去掉类型后缀得到完整 ID（`.schema.yaml`/`.schema.yml`/…）。
fn strip_kind_suffix<'a>(name: &'a str, suffixes: &[&'static str; 2]) -> String {
    for s in suffixes {
        if let Some(stripped) = name.strip_suffix(s) {
            return stripped.to_string();
        }
    }
    String::new()
}

/// 内容基础字段检查（第 9 步附则）：schema 的 `schema.id`、dict 的 `name`
/// 必须与文件名解析出的完整 ID 一致；theme 无此约定，跳过。
fn check_content_id(
    rel_path: &str,
    root: &Path,
    cat: &HierarchyCategory,
    id: &str,
    findings: &mut Vec<Finding>,
) {
    if !matches!(cat.dir, "schema" | "dict") {
        return;
    }
    let path: std::path::PathBuf =
        rel_path.split('/').fold(root.to_path_buf(), |p, seg| p.join(seg));
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return, // 读不了留给内容检测阶段处理
    };
    let lines: Vec<&str> = text.lines().collect();
    // 新定式优先：前 10 行内的顶层 `id:`（六行定式的第 6 行）。
    let top_level_id = lines
        .iter()
        .take(10)
        .find_map(|l| l.trim().strip_prefix("id:").map(|v| strip_inline_comment(v.trim())));
    let field = if let Some(v) = top_level_id {
        Some(v)
    } else if cat.dir == "schema" {
        // 旧式：`schema:` 块内缩进的 `id:`
        lines.iter().position(|l| l.trim_end() == "schema:").and_then(|s| {
            lines[s + 1..]
                .iter()
                .take_while(|l| l.starts_with([' ', '\t']) || l.trim().is_empty())
                .find_map(|l| l.trim().strip_prefix("id:").map(|v| v.trim().to_string()))
        })
    } else if cat.dir == "dict" {
        // 旧式：词典头块 `name:`
        lines.iter().take(20).find_map(|l| {
            l.trim().strip_prefix("name:").map(|v| strip_inline_comment(v.trim()))
        })
    } else {
        None
    };
    match field {
        Some(v) if v == id => findings.push(Finding::ok(format!(
            "内容 id 一致：`{}` = 文件名 ID `{id}`", cat.dir
        ))),
        Some(v) => findings.push(Finding::error(format!(
            "内容 id 不一致：{} 内的 `{v}` ≠ 文件名 ID `{id}`",
            cat.dir
        ))),
        None => findings.push(Finding::error(format!(
            "文件里找不到 {} 的 id 字段（`{}`）——内容基础字段缺失",
            cat.dir,
            if cat.dir == "schema" { "schema.id" } else { "name" }
        ))),
    }
}

// ---------------------------------------------------------------------------
// 前六行定式识别（内容检测第一阶段，层级检测通过后进入）
// ---------------------------------------------------------------------------

/// 前六行定式：
///
/// ```text
/// #rimetype: schema      ← 第 1 行：魔数头（rimetype + 类型小写）
/// #encode: utf-8         ← 第 2 行：编码
/// #newline: lf           ← 第 3 行：换行符
/// ---                    ← 第 4 行：YAML 文档起始
/// type: schema           ← 第 5 行：类型（小写，与 rimetype 一致）
/// id: pinyin             ← 第 6 行：完整 ID（= 文件名去掉 .{类型}.yaml）
/// ```
///
/// 识别容忍：`#` 后空格可有可无、键与值大小写不敏感、`utf8`≈`utf-8`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Header6 {
    /// 第 1 行 `#rimetype:` 的值（小写化）。
    pub rimetype: Option<String>,
    /// 第 2 行 `#encode:` 的值（小写化，`utf8` 归一为 `utf-8`）。
    pub encode: Option<String>,
    /// 第 3 行 `#newline:` 的值（小写化）。
    pub newline: Option<String>,
    /// 第 4 行是否是 `---`。
    pub doc_marker: bool,
    /// 第 5 行 `type:` 的值（小写化）。
    pub yaml_type: Option<String>,
    /// 第 6 行 `id:` 的值（原样，剥行内注释）。
    pub yaml_id: Option<String>,
}

/// 读取文本前六行，按 [`Header6`] 定式识别。
pub fn sniff_header6(text: &str) -> Header6 {
    let mut h = Header6::default();
    let lines: Vec<&str> = text.lines().take(6).collect();
    // 前 3 行：`#键: 值` 注释（容忍 `#` 后空格、键大小写）。
    for line in lines.iter().take(3) {
        let t = line.trim().trim_start_matches('#').trim();
        let (key, val) = match t.split_once(':') {
            Some(kv) => kv,
            None => continue,
        };
        let key = key.trim().to_ascii_lowercase();
        let val = val.trim().to_ascii_lowercase();
        match key.as_str() {
            "rimetype" => h.rimetype = Some(val),
            "encode" => h.encode = Some(if val == "utf8" { "utf-8".into() } else { val }),
            "newline" => h.newline = Some(val),
            _ => {}
        }
    }
    // 第 4 行：`---`。
    h.doc_marker = lines.get(3).map_or(false, |l| l.trim() == "---");
    // 第 5/6 行：顶层 `type:` / `id:`（键大小写不敏感；字节安全，防中文注释切边）。
    if let Some(l) = lines.get(4) {
        let t = l.trim();
        if t.to_ascii_lowercase().starts_with("type:") {
            h.yaml_type = Some(t[5..].trim().to_ascii_lowercase());
        }
    }
    if let Some(l) = lines.get(5) {
        let t = l.trim();
        if t.to_ascii_lowercase().starts_with("id:") {
            h.yaml_id = Some(strip_inline_comment(t[3..].trim()));
        }
    }
    h
}

/// 前六行校验：定式六要素逐项核对，`cat_dir` 为分类目录名
/// （`schema`/`theme`/`dict`），`id` 为文件名解析出的完整 ID。
pub fn check_header6(h: &Header6, cat_dir: &str, id: &str) -> Vec<Finding> {
    fn expect(
        out: &mut Vec<Finding>,
        got: &Option<String>,
        what: &str,
        want: &str,
    ) {
        match got {
            Some(v) if v == want => out.push(Finding::ok(format!(
                "第 {what} 合规：`{v}`"
            ))),
            Some(v) => out.push(Finding::error(format!(
                "第 {what} 不合规：`{v}` ≠ 期望 `{want}`"
            ))),
            None => out.push(Finding::error(format!(
                "第 {what} 缺失（期望 `{want}`）"
            ))),
        }
    }
    let mut out = Vec::new();
    expect(&mut out, &h.rimetype, "1 行 rimetype", cat_dir);
    expect(&mut out, &h.encode, "2 行 encode", "utf-8");
    expect(&mut out, &h.newline, "3 行 newline", "lf");
    if h.doc_marker {
        out.push(Finding::ok("第 4 行 `---` 文档起始"));
    } else {
        out.push(Finding::error("第 4 行不是 `---` 文档起始"));
    }
    expect(&mut out, &h.yaml_type, "5 行 type", cat_dir);
    expect(&mut out, &h.yaml_id, "6 行 id", id);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 规范五类魔数头，输入 → 期望种类。
    #[test]
    fn sniff_recognizes_canonical_kinds() {
        let cases = [
            ("# Rime Schema\n---\nschema:\n  id: pinyin", RimeKind::Schema),
            ("# Rime Dictionary\n---\nname: x\n...", RimeKind::Dict),
            ("# Rime default settings\nconfig_version: 1", RimeKind::Default),
            ("# Rime key bindings\nkey_binder:", RimeKind::KeyBindings),
            ("# Rime Eassy\n---\n...", RimeKind::Eassy),
        ];
        for (text, want) in cases {
            let got = sniff(text);
            assert_eq!(got.kind, want, "输入 {text:?} 的识别结果");
            assert_eq!(got.line_no, 1, "{text:?} 的魔数头行号");
        }
    }

    /// 容错：无空格 / 小写 / 简写 dict / 长破折号说明，全都认得。
    #[test]
    fn sniff_tolerates_case_space_and_notes() {
        let cases = [
            // 输入（仓内真实写法）→（种类，行号 1）
            ("#Rime schema settings", RimeKind::Schema),
            ("# rime dictionary: 基础词库", RimeKind::Dict),
            ("# Rime dict: 通用字8105", RimeKind::Dict),
            ("# Rime Schema — 明月（主方案）", RimeKind::Schema),
            ("# Rime schema 输入方案配置文件", RimeKind::Schema),
        ];
        for (text, want) in cases {
            assert_eq!(sniff(text).kind, want, "输入 {text:?}");
        }
        // 说明后缀原样保留
        let h = sniff("# Rime Schema — 明月（主方案）");
        assert_eq!(h.note, "— 明月（主方案）", "Schema 后的说明后缀");
        let h = sniff("# Rime dict: 单字");
        assert_eq!(h.note, "单字", "dict: 后的说明后缀");
    }

    /// 编码行让位 / 异己注释截停 / 无头文件。
    #[test]
    fn sniff_skips_encoding_and_stops_at_foreign_comment() {
        // encoding 行在前，魔数头退居第 2 行
        let h = sniff("# encoding: utf-8\n# Rime Schema\n---\n");
        assert_eq!(h.kind, RimeKind::Schema);
        assert_eq!(h.line_no, 2, "魔数头在 encoding 行之后");

        // 第一条注释不是魔数头 ⇒ 直接判无头
        assert_eq!(sniff("# 出厂皮肤（只有配色）\nmode: dark\n").kind, RimeKind::None);
        // 裸词条开头（仓内真实存在的无头词典）
        assert_eq!(sniff("呵\tkk,kk,kk,kk,dz,ks,kk,kh,kh\n").kind, RimeKind::None);
        // 空白/空文件
        assert_eq!(sniff("").kind, RimeKind::None);
        // 符号表老写法
        assert_eq!(sniff("# Rime's punctuation and symbols\n").kind, RimeKind::Symbols);
    }

    /// Schema 校验：具体 I/O —— 合法 / 缺 id / 无 ---。
    #[test]
    fn validate_schema_checks_doc_and_id() {
        let header = sniff("# Rime Schema\n---\nschema:\n  id: pinyin\n  name: 拼音\n");
        let f = validate(&header, "# Rime Schema\n---\nschema:\n  id: pinyin\n  name: 拼音\n");
        assert!(f.iter().all(|x| x.level == Level::Ok), "合法方案应全过：{f:?}");
        assert!(f.iter().any(|x| x.message.contains("schema.id = `pinyin`")), "{f:?}");

        let text = "# Rime Schema\n---\nschema:\n  name: 无id\n";
        let f = validate(&sniff(text), text);
        assert!(
            f.iter().any(|x| x.level == Level::Error && x.message.contains("没有 `id:`")),
            "缺 schema.id 应报错：{f:?}"
        );

        // 无 --- 在 Schema 只是告警（YAML 可省略，仓内 092 系即如此）
        let text = "# Rime Schema\nschema: 缩进不对\n";
        let f = validate(&sniff(text), text);
        assert!(
            f.iter().any(|x| x.level == Level::Warn && x.message.contains("---")),
            "无 --- 文档起始应告警：{f:?}"
        );
    }

    /// Dict 校验：具体 I/O —— 合法 / 词条缺 Tab / 头块缺 name。
    #[test]
    fn validate_dict_checks_header_block_and_tab_entries() {
        let text = "# Rime Dictionary\n---\nname: qiaoma\nversion: \"1.0\"\n...\n啊\ta\n爱\tai\t3\n";
        let f = validate(&sniff(text), text);
        assert!(f.iter().all(|x| x.level == Level::Ok), "合法词典应全过：{f:?}");
        assert!(f.iter().any(|x| x.message.contains("2 条词条")), "{f:?}");

        // 头块用 `---` 收尾也认
        let text = "# Rime dict\n---\nname: x\n---\n呵\tkk\n";
        let f = validate(&sniff(text), text);
        assert!(f.iter().all(|x| x.level == Level::Ok), "--- 收尾也应全过：{f:?}");

        // 词条缺 Tab（仓内真实事故形态：整句逗号分隔）
        let text = "# Rime Dictionary\n---\nname: x\n...\n呵,kk,kk\n啊,a\n";
        let f = validate(&sniff(text), text);
        assert!(
            f.iter().any(|x| x.level == Level::Warn && x.message.contains("缺 Tab")),
            "缺 Tab 词条应告警：{f:?}"
        );

        // packs 导入型词典：头块带 import_tables，无词条属正常
        let text = "# Rime dict\n---\nname: qiaoma\nimport_tables:\n  - qiaoma_dz\n...\n";
        let f = validate(&sniff(text), text);
        assert!(
            f.iter().any(|x| x.level == Level::Ok && x.message.contains("导入型词典")),
            "import_tables 词典无词条应判正常：{f:?}"
        );

        // 头块缺 name
        let text = "# Rime Dictionary\n---\nversion: \"1.0\"\n...\n啊\ta\n";
        let f = validate(&sniff(text), text);
        assert!(
            f.iter().any(|x| x.level == Level::Error && x.message.contains("`name:`")),
            "头块缺 name 应报错：{f:?}"
        );
    }

    /// 文件名 ↔ 魔数头交叉核对：`xxx.schema.yaml` 必须配 Schema 头，反之亦然。
    #[test]
    fn analyze_cross_checks_filename_against_header() {
        let dir = std::env::temp_dir().join("rime_format_test");
        std::fs::create_dir_all(&dir).unwrap();

        // schema 文件 + Dict 头 ⇒ Error
        let p = dir.join("fake.schema.yaml");
        std::fs::write(&p, "# Rime Dictionary\n---\nname: x\n...\n啊\ta\n").unwrap();
        let a = analyze(&p).unwrap();
        assert!(
            a.findings.iter().any(|f| f.level == Level::Error && f.message.contains(".schema.yaml")),
            "schema 文件配 dict 头应报错：{:?}",
            a.findings
        );

        // 词典文件无头（仓内 qiaoma 真实形态）⇒ Error 点名
        let p = dir.join("bare.dict.yaml");
        std::fs::write(&p, "呵\tkk\n啊\ta\n").unwrap();
        let a = analyze(&p).unwrap();
        assert_eq!(a.header.kind, RimeKind::None);
        assert!(
            a.findings.iter().any(|f| f.level == Level::Error && f.message.contains(".dict.yaml")),
            "dict 文件无头应报错：{:?}",
            a.findings
        );

        // Schema 头写进普通名字文件 ⇒ Warn
        let p = dir.join("misc.yaml");
        std::fs::write(&p, "# Rime Schema\n---\nschema:\n  id: x\n").unwrap();
        let a = analyze(&p).unwrap();
        assert!(
            a.findings.iter().any(|f| f.level == Level::Warn && f.message.contains("不是 `*.schema.yaml`")),
            "Schema 头配错文件名应告警：{:?}",
            a.findings
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prefix_group_pinyin_shape() {
        // 真实形态：dict/pinyin/ 下主词典 + 平行子词典 + luna 子文件夹（其本体已搬进去）。
        let names = [
            "pinyin.dict.yaml".to_string(),
            "pinyin.cuoyin.dict.yaml".to_string(),
            "pinyin.diming.dict.yaml".to_string(),
            "pinyin.luna.dict.yaml".to_string(),
        ];
        let sub_dirs = ["luna".to_string(), "terra".to_string()];
        let f = check_prefix_group("pinyin", "dict", &names, &sub_dirs);
        assert!(
            f.iter().any(|x| x.level == Level::Ok && x.message.contains("前缀正确")),
            "公共前缀 pinyin = 目录 pinyin 应通过：{f:?}"
        );
        assert!(
            f.iter().any(|x| x.level == Level::Error && x.message.contains("pinyin.luna.dict.yaml")),
            "pinyin.luna 与同级文件夹 luna/ 同名应报错（该进 luna/）：{f:?}"
        );
        assert!(
            f.iter().any(|x| x.level == Level::Ok && x.message.contains("cuoyin")),
            "cuoyin 无同名文件夹应通过：{f:?}"
        );
        assert!(
            f.iter().any(|x| x.level == Level::Ok && x.message.contains("主文件")),
            "最短文件 pinyin.dict.yaml 应判主文件：{f:?}"
        );
    }

    #[test]
    fn prefix_group_cangjie5_shape() {
        // 真实形态：dict/cangjie/5/ 下 cangjie.5.dict.yaml + 四个二级子词典，无子文件夹。
        let names = [
            "cangjie.5.dict.yaml".to_string(),
            "cangjie.5.base.dict.yaml".to_string(),
            "cangjie.5.char.dict.yaml".to_string(),
            "cangjie.5.extended.dict.yaml".to_string(),
            "cangjie.5.stem.dict.yaml".to_string(),
        ];
        let f = check_prefix_group("cangjie/5", "dict", &names, &[]);
        assert!(
            f.iter().any(|x| x.level == Level::Ok && x.message.contains("cangjie.5")),
            "公共前缀 cangjie.5 = 目录 cangjie/5 应通过：{f:?}"
        );
        assert!(
            f.iter().all(|x| x.level != Level::Error),
            "该目录整体应零错误：{f:?}"
        );
    }

    #[test]
    fn prefix_group_mixed_no_common_prefix() {
        // 真实违规形态：schema/pinyin/ 下 pinyin.schema.yaml + wanxiang.schema.yaml 混居。
        let names = [
            "pinyin.schema.yaml".to_string(),
            "wanxiang.schema.yaml".to_string(),
        ];
        let f = check_prefix_group("pinyin", "schema", &names, &[]);
        assert!(
            f.iter().any(|x| x.level == Level::Error
                && x.message.contains("wanxiang.schema.yaml")),
            "wanxiang 前缀 ≠ 目录 pinyin 应报错：{f:?}"
        );
        assert!(
            f.iter().any(|x| x.level == Level::Ok
                && x.message.contains("pinyin.schema.yaml：前缀以目录路径")),
            "pinyin.schema.yaml 目录归属应正确：{f:?}"
        );
    }

    #[test]
    fn prefix_group_mismatch_dir() {
        // 违规形态：wanxiang.dict.yaml 住在 dict/pinyin/ 下（前缀 ≠ 目录）。
        let names = ["wanxiang.dict.yaml".to_string()];
        let f = check_prefix_group("pinyin", "dict", &names, &[]);
        assert!(
            f.iter().any(|x| x.level == Level::Error && x.message.contains("≠ 目录路径")),
            "前缀 wanxiang ≠ 目录 pinyin 应报错：{f:?}"
        );
    }

    // ==================== 层级 10 步流程 ====================

    /// 搭一个最小数据根：root/{schema,theme,dict}/…，写文件并返回报告。
    fn build_root(tag: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("hierarchy_test_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        for (rel, content) in files {
            let p = dir.join(rel.replace('/', "\\"));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
        }
        dir
    }

    fn canon(id: &str) -> String {
        format!("#rimetype: schema\n#encode: utf-8\n#newline: lf\n---\ntype: schema\nid: {id}\n")
    }

    #[test]
    fn hierarchy_flat_family_and_missing_level() {
        // A 与 A.B.C 同住 方案/A/（扁平家族），A.B 缺失 → 允许。
        let root = build_root("flat", &[
            ("schema/A/A.schema.yaml", &canon("A")),
            ("schema/A/A.B.C.schema.yaml", &canon("A.B.C")),
        ]);
        let rep = check_hierarchy(&root);
        let cat = &rep.categories[0];
        assert_eq!(cat.legal_list.len(), 2, "两个文件都应合法：{cat:?}");
        assert!(
            cat.missing_levels.iter().any(|l| l == "A.B"),
            "A.B 应记缺失层级：{cat:?}"
        );
        let abc = cat.files.iter().find(|f| f.id == "A.B.C").unwrap();
        assert!(
            abc.findings.iter().any(|x| x.level == Level::Ok && x.message.contains("锚 `A`")),
            "A.B.C 的锚应是最短存在前缀 A（不是自己）：{abc:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hierarchy_anchor_self_nested_chain() {
        // 单独的 cangjie.5：锚 = 自己，期望目录链 = cangjie/5（逐级）。
        let root = build_root("cj5", &[
            ("schema/cangjie/5/cangjie.5.schema.yaml", &canon("cangjie.5")),
        ]);
        let rep = check_hierarchy(&root);
        let cat = &rep.categories[0];
        assert_eq!(cat.legal_list.len(), 1, "cangjie/5/ 位置应合法：{cat:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hierarchy_conflict_ignores_whole_dir() {
        // 方案/A/ 里 A.方案.yaml 与子文件夹 B/ 共存 → 整目录忽略，B 也不进候选。
        let root = build_root("conflict", &[
            ("schema/A/A.schema.yaml", &canon("A")),
            ("schema/A/B/B.schema.yaml", &canon("B")),
        ]);
        let rep = check_hierarchy(&root);
        let cat = &rep.categories[0];
        assert!(
            cat.findings.iter().any(|x| x.level == Level::Error && x.message.contains("共存冲突")),
            "应报共存冲突：{cat:?}"
        );
        assert!(cat.files.is_empty(), "整目录忽略后应无候选：{cat:?}");
        assert!(cat.legal_list.is_empty(), "合法列表应为空：{cat:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hierarchy_anchor_mismatch_reports_location_error() {
        // pinyin.cuoyin 单独住在 pinyin/（无 pinyin 本体）→ 锚 = 自己，期望 pinyin/cuoyin/。
        // pinyin.luna 住在 pinyin/luna/ → 锚 = 自己，合法。
        let root = build_root("anchor", &[
            ("schema/pinyin/pinyin.cuoyin.schema.yaml", &canon("pinyin.cuoyin")),
            ("schema/pinyin/luna/pinyin.luna.schema.yaml", &canon("pinyin.luna")),
        ]);
        let rep = check_hierarchy(&root);
        let cat = &rep.categories[0];
        let cuoyin = cat.files.iter().find(|f| f.id == "pinyin.cuoyin").unwrap();
        assert!(
            !cuoyin.legal
                && cuoyin.findings.iter().any(|x| x.message.contains("位置错误")),
            "pinyin.cuoyin 在 pinyin/ 应位置错误（锚=自己，期望 pinyin/cuoyin/）：{cuoyin:?}"
        );
        let luna = cat.files.iter().find(|f| f.id == "pinyin.luna").unwrap();
        assert!(luna.legal, "pinyin/luna/ 里的 pinyin.luna 应合法：{luna:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    // ==================== 前六行定式 ====================

    #[test]
    fn header6_canonical_and_tolerant() {
        // 规范六行：全 ✅
        let text = "#rimetype: schema\n#encode: utf-8\n#newline: lf\n---\ntype: schema\nid: pinyin\n";
        let f = check_header6(&sniff_header6(text), "schema", "pinyin");
        assert!(f.iter().all(|x| x.level == Level::Ok), "规范六行应全过：{f:?}");

        // 容忍：# 后空格、大小写、utf8 归一
        let text = "#RimeType: SCHEMA\n#Encode: utf8\n#NewLine: LF\n---\nType: Schema\nID: pinyin\n";
        let f = check_header6(&sniff_header6(text), "schema", "pinyin");
        assert!(f.iter().all(|x| x.level == Level::Ok), "容忍写法应全过：{f:?}");

        // id 不一致 → 第 6 行报错
        let text = "#rimetype: dict\n#encode: utf-8\n#newline: lf\n---\ntype: dict\nid: other\n";
        let f = check_header6(&sniff_header6(text), "dict", "qiaoma");
        assert!(
            f.iter().any(|x| x.level == Level::Error && x.message.contains("6 行 id")),
            "id ≠ 文件名 ID 应报错：{f:?}"
        );

        // 缺 --- → 第 4 行报错
        let text = "#rimetype: theme\n#encode: utf-8\n#newline: lf\n\ntype: theme\nid: default\n";
        let f = check_header6(&sniff_header6(text), "theme", "default");
        assert!(
            f.iter().any(|x| x.level == Level::Error && x.message.contains("4 行")),
            "缺 --- 应报错：{f:?}"
        );
    }
}
