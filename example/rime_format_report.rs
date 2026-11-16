//! 扫描一个 Rime 数据根（默认 `../Rime-Next`）里的全部 yaml，
//! 用 [`librime_next::config::rime_format`] 真实跑一遍「魔数头识别 + 格式自动检测」，
//! 输出 markdown 体检报告到 stdout。
//!
//! ```text
//! cargo run --example rime_format_report -- [数据根] > ../doc/rime-format-detect.md
//! ```

use librime_next::config::rime_format::{self, Analysis, Level, RimeKind};
use std::path::{Path, PathBuf};

fn collect_yaml(root: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_yaml(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("yaml") {
            out.push(path);
        }
    }
    Ok(())
}

fn rel(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn group_of(kind: &RimeKind) -> &'static str {
    match kind {
        RimeKind::Schema => "Schema（方案）",
        RimeKind::Dict => "Dict（词典）",
        RimeKind::Default => "Default（全局默认）",
        RimeKind::KeyBindings => "KeyBindings（键绑定）",
        RimeKind::Eassy => "Eassy（easy 词库）",
        RimeKind::Symbols => "Symbols（符号表）",
        RimeKind::Other(_) => "Other（# Rime 头但非标准种类）",
        RimeKind::None => "无魔数头",
    }
}

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn main() {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../Rime-Next"));
    let root = root.canonicalize().expect("数据根不存在");
    let mut files = Vec::new();
    collect_yaml(&root, &mut files).expect("遍历失败");
    files.sort();

    let mut analyzed: Vec<(String, Analysis)> = Vec::new();
    for path in &files {
        let a = match rime_format::analyze(path) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("跳过 {}：{e}", rel(path, &root));
                continue;
            }
        };
        analyzed.push((rel(path, &root), a));
    }

    // ---- 概览 ----
    let mut groups: Vec<(&'static str, Vec<(String, Analysis)>)> = Vec::new();
    for (path, a) in &analyzed {
        let g = group_of(&a.header.kind);
        match groups.iter_mut().find(|(k, _)| *k == g) {
            Some((_, v)) => v.push((path.clone(), a.clone())),
            None => groups.push((g, vec![(path.clone(), a.clone())])),
        }
    }

    let total_errors = analyzed
        .iter()
        .map(|(_, a)| a.findings.iter().filter(|f| f.level == Level::Error).count())
        .sum::<usize>();
    let total_warns = analyzed
        .iter()
        .map(|(_, a)| a.findings.iter().filter(|f| f.level == Level::Warn).count())
        .sum::<usize>();

    println!("# Rime yaml 魔数头识别与格式检测报告");
    println!();
    println!("- 扫描根：`{}`", root.display());
    println!("- 文件数：{}", analyzed.len());
    println!("- 结论：**{total_errors} 个错误、{total_warns} 个警告**（校验规则见下文）");
    println!();
    println!("## 概览");
    println!();
    println!("| 种类 | 数量 |");
    println!("|---|---:|");
    for (g, items) in &groups {
        println!("| {g} | {} |", items.len());
    }
    println!();

    // ---- 问题清单（错误/警告优先） ----
    println!("## 问题清单");
    println!();
    let problems: Vec<(String, Analysis)> = analyzed
        .iter()
        .filter(|(_, a)| {
            a.findings
                .iter()
                .any(|f| matches!(f.level, Level::Error | Level::Warn))
        })
        .cloned()
        .collect();
    if problems.is_empty() {
        println!("全部通过，无错误无警告。");
    } else {
        println!("| 级别 | 文件 | 问题 |");
        println!("|---|---|---|");
        for (path, a) in &problems {
            for f in a.findings.iter().filter(|f| f.level != Level::Ok) {
                println!("| {} | `{}` | {} |", f.level.badge(), cell(path), cell(&f.message));
            }
        }
    }
    println!();

    // ---- 分组明细 ----
    let order = [
        "Schema（方案）",
        "Dict（词典）",
        "Default（全局默认）",
        "KeyBindings（键绑定）",
        "Eassy（easy 词库）",
        "Symbols（符号表）",
        "Other（# Rime 头但非标准种类）",
        "无魔数头",
    ];
    println!("## 分组明细");
    for g in order {
        let Some(items) = groups.iter().find(|(k, _)| *k == g).map(|(_, v)| v) else {
            continue;
        };
        println!();
        println!("### {g}（{} 个）", items.len());
        println!();
        println!("| 文件 | 魔数头（第 1 处 # Rime 行）| 校验结论 |");
        println!("|---|---|---|");
        for (path, a) in items {
            let head = if a.header.raw.is_empty() {
                "—".to_string()
            } else {
                format!("`{}`（第 {} 行）", cell(&a.header.raw), a.header.line_no)
            };
            let verdict = a
                .findings
                .iter()
                .map(|f| format!("{} {}", f.level.badge(), f.message))
                .collect::<Vec<_>>()
                .join("<br>");
            println!("| `{}` | {} | {} |", cell(path), head, cell(&verdict));
        }
    }
    println!();
    println!("## 校验规则（`config::rime_format`）");
    println!();
    println!("1. **嗅探**：首行起跳过空行与 `# encoding/encode:` 行；第一条 `# Rime <种类词>` 注释");
    println!("   即魔数头（容忍 `#Rime` 无空格、`rime` 小写、`dict:` 简写、任意说明后缀）；");
    println!("   第一条异己注释或实体行即停 ⇒ 无头。");
    println!("2. **Schema**：前 20 行内有 `---` 文档起始；列 0 的 `schema:` 块；块内有 `id:`。");
    println!("3. **Dict**：`---` 头块起始；头块内有 `name:`；`...`/`---` 收尾；之后每条词条");
    println!("   至少一个 Tab 分隔（缺 Tab 是仓内真实事故形态：逗号分隔整句）。");
    println!("4. **文件名交叉核对**：`*.schema.yaml` 必须配 Schema 头、`*.dict.yaml` 必须配");
    println!("   Dict 头，反之亦然（Schema 头写进别的名字 ⇒ 警告）。");
}
