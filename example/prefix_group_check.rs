//! 示例：把「同前缀同后缀、中间不同」文件组校验跑在真实数据根上。
//!
//! ```text
//! cargo run --example prefix_group_check -- ../Rime-Next
//! ```
//!
//! 对每个类型目录（schema/dict/theme/eassy）递归找含 `*.<type>.yaml` 的目录，
//! 按 `check_prefix_group` 的三条规则体检并打印结果。

use librime_next::config::rime_format::{check_prefix_group, Level};
use std::path::{Path, PathBuf};

const KINDS: [&str; 4] = ["schema", "dict", "theme", "eassy"];

fn main() {
    let root = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "../Rime-Next".into());
    let root = PathBuf::from(&root);
    println!("# 前缀组校验：数据根 {}\n", root.display());

    let mut total_err = 0usize;
    let mut total_warn = 0usize;
    for kind in KINDS {
        let type_dir = root.join(kind);
        if !type_dir.is_dir() {
            continue;
        }
        for dir in dirs_with_kind_files(&type_dir, kind) {
            let dir_rel = dir
                .strip_prefix(&type_dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let mut names: Vec<String> = Vec::new();
            let mut sub_dirs: Vec<String> = Vec::new();
            for e in std::fs::read_dir(&dir).unwrap() {
                let e = e.unwrap();
                let name = e.file_name().to_string_lossy().to_string();
                if e.path().is_dir() {
                    sub_dirs.push(name);
                } else if name.ends_with(&format!(".{kind}.yaml")) {
                    names.push(name);
                }
            }
            names.sort();
            if names.is_empty() {
                continue;
            }
            let findings = check_prefix_group(&dir_rel, kind, &names, &sub_dirs);
            let errs = findings.iter().filter(|f| f.level == Level::Error).count();
            let warns = findings.iter().filter(|f| f.level == Level::Warn).count();
            total_err += errs;
            total_warn += warns;
            println!(
                "## {}/{}/  （{} 个 {} 文件，{} 个子文件夹）",
                kind,
                dir_rel,
                names.len(),
                kind,
                sub_dirs.len()
            );
            for f in &findings {
                println!("  {} {}", f.level.badge(), f.message);
            }
            println!();
        }
    }
    println!("---- 合计：{total_err} 错误 / {total_warn} 警告");
}

/// 递归收集「含 `*.{kind}.yaml` 文件」的目录。
fn dirs_with_kind_files(dir: &Path, kind: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let suffix = format!(".{kind}.yaml");
    let has = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().ends_with(&suffix))
        })
        .unwrap_or(false);
    if has {
        out.push(dir.to_path_buf());
    }
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.filter_map(|e| e.ok()) {
            if e.path().is_dir() {
                out.extend(dirs_with_kind_files(&e.path(), kind));
            }
        }
    }
    out
}
