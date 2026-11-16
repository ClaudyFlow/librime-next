//! 示例：文件夹层级 10 步检测 + 前六行定式检测，跑在真实数据根上出 MD 报告。
//!
//! ```text
//! cargo run --example hierarchy_check -- ../Rime-Next ../doc/hierarchy-check.md
//! ```

use librime_next::config::rime_format::{
    check_header6, check_hierarchy, sniff_header6, HierarchyReport, Level,
};
use std::path::{Path, PathBuf};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = PathBuf::from(args.get(1).map(|s| s.as_str()).unwrap_or("../Rime-Next"));
    let out_path = PathBuf::from(
        args.get(2).map(|s| s.as_str()).unwrap_or("../doc/hierarchy-check.md"),
    );
    let rep = check_hierarchy(&root);

    let mut md = String::new();
    md.push_str("# 文件夹层级检测报告（10 步流程 + 前六行定式）\n\n");
    md.push_str(&format!("- 数据根：`{}`\n", root.display()));
    md.push_str(&format!(
        "- 生成：cargo run --example hierarchy_check（librime-next `config::rime_format`）\n\n"
    ));

    // ---- 根目录：不管（根下允许全局设置文件/任意文件夹）----
    md.push_str(&format!(
        "- 根目录：`{}`（不管，根下允许 default.yaml/aux_code.yaml 等全局设置）\n- 分类映射：方案→`schema`、皮肤→`theme`、词库→`dict`（数据根现成的一级目录）\n\n",
        root.display()
    ));

    // ---- 每个分类 ----
    let mut total_legal = 0usize;
    for cat in &rep.categories {
        let errs = cat.findings.iter().filter(|f| f.level == Level::Error).count();
        md.push_str(&format!(
            "## 分类：{}（{}）—— 合法 {} 个\n\n",
            cat.label,
            cat_label_dir(&rep, &cat.label),
            cat.legal_list.len()
        ));
        total_legal += cat.legal_list.len();

        // 分类级结论（共存冲突等）
        for f in &cat.findings {
            md.push_str(&format!("{} {}\n", f.level.badge(), f.message));
        }
        if errs > 0 {
            md.push('\n');
        }

        // 逐文件
        if !cat.files.is_empty() {
            md.push_str("\n| 文件 | 完整 ID | 判定 | 说明 |\n|---|---|---|---|\n");
            for f in &cat.files {
                let verdict = if f.legal { "✅" } else { "❌" };
                let notes: Vec<String> = f
                    .findings
                    .iter()
                    .filter(|x| x.level != Level::Ok)
                    .map(|x| x.message.clone())
                    .collect();
                let notes = if notes.is_empty() {
                    f.findings
                        .first()
                        .map(|x| x.message.clone())
                        .unwrap_or_default()
                } else {
                    notes.join("；")
                };
                md.push_str(&format!(
                    "| {} | `{}` | {} | {} |\n",
                    f.rel_path, f.id, verdict, notes
                ));
            }
        }

        // 缺失/非法层级
        if !cat.missing_levels.is_empty() {
            let list: Vec<String> =
                cat.missing_levels.iter().map(|l| format!("`{l}`")).collect();
            md.push_str(&format!(
                "\n⚠️ 缺失层级（允许）：{}\n",
                list.join("、")
            ));
        }
        if !cat.illegal_levels.is_empty() {
            let list: Vec<String> =
                cat.illegal_levels.iter().map(|l| format!("`{l}`")).collect();
            md.push_str(&format!(
                "\n❌ 非法层级（存在但不合法）：{}\n",
                list.join("、")
            ));
        }

        // 合法列表
        if !cat.legal_list.is_empty() {
            md.push_str("\n合法方案列表：\n");
            for (i, p) in cat.legal_list.iter().enumerate() {
                md.push_str(&format!("{}. ✅ {}\n", i + 1, p));
            }
        }
        md.push('\n');
    }

    // ---- 前六行定式（内容检测第一阶段）----
    md.push_str("## 前六行定式检测（层级合法的文件）\n\n");
    md.push_str("定式：`#rimetype:` / `#encode: utf-8` / `#newline: lf` / `---` / `type:` / `id:`\n\n");
    md.push_str("| 文件 | 六要素 | 结果 |\n|---|---|---|\n");
    let mut pass6 = 0usize;
    let mut seen6 = 0usize;
    for cat in &rep.categories {
        for rel in &cat.legal_list {
            let path: PathBuf = rel.split('/').fold(root.clone(), |p, seg| p.join(seg));
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let h6 = sniff_header6(&text);
            let id = rel
                .rsplit('/')
                .next()
                .unwrap_or("")
                .split_once(&format!(".{}.", cat_dir_of(&rep, &cat.label)).as_str())
                .map(|(id, _)| id.to_string())
                .unwrap_or_default();
            let findings = check_header6(&h6, cat_dir_of(&rep, &cat.label), &id);
            seen6 += 1;
            let all_ok = findings.iter().all(|f| f.level == Level::Ok);
            if all_ok {
                pass6 += 1;
            }
            let summary: Vec<String> = findings
                .iter()
                .filter(|f| f.level != Level::Ok)
                .map(|f| f.message.clone())
                .collect();
            let summary = if summary.is_empty() {
                "六行全合规".to_string()
            } else {
                summary.join("；")
            };
            md.push_str(&format!(
                "| {} | {} | {} |\n",
                rel,
                if all_ok { "✅" } else { "❌" },
                summary
            ));
        }
    }
    md.push_str(&format!(
        "\n**前六行合计**：{seen6} 个层级合法文件中 {pass6} 个符合六行定式。\n\n"
    ));
    md.push_str(&format!(
        "---\n\n**总计**：合法实体文件 {total_legal} 个；分类级共存冲突见各节。到这一步文件夹层级全部处理完，其后才进入 YAML 内容检测。\n"
    ));

    std::fs::write(&out_path, md).unwrap();
    println!("报告已写入 {}", out_path.display());
}

fn cat_label_dir(rep: &HierarchyReport, label: &str) -> &'static str {
    let _ = rep;
    match label {
        "方案" => "schema",
        "皮肤" => "theme",
        _ => "dict",
    }
}

fn cat_dir_of(rep: &HierarchyReport, label: &str) -> &'static str {
    cat_label_dir(rep, label)
}
