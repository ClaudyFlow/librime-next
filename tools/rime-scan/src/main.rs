//! `rime-scan`：文件识别 + 系列文件夹校验 + 词典击键统计。
//!
//! 用法：
//!   rime-scan <Rime-Next 数据目录>
//! 不传参时默认扫描 `../../Rime-Next`（相对本工具 crate 目录，
//! 即 `Weasel-Next/Rime-Next`）。
//!
//! 步骤：
//!   1. 遍历数据目录，对每个 yaml/字典/主题/脚本/词库/模型文件做"先看文件再看文件夹"
//!      的识别，并校验它是否摆在了正确的系列文件夹里（id 点号分段 == 文件夹链）。
//!   2. 对每个 `.dict.yaml` 解析编码列，算平均击键 / 最大击键并汇总。

mod keystrokes;
mod scan;

use std::collections::BTreeMap;
use std::path::PathBuf;
use scan::{scan_dir, ResourceKind, ScannedFile};

fn kind_name(k: ResourceKind) -> &'static str {
    match k {
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

fn main() {
    let data_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| {
            let cand = PathBuf::from("../../Rime-Next");
            cand.exists().then_some(cand)
        })
        .unwrap_or_else(|| {
            eprintln!("usage: rime-scan <path-to-Rime-Next-data-dir>");
            std::process::exit(2);
        });
    let data_dir = std::fs::canonicalize(&data_dir).unwrap_or(data_dir);
    println!("== Scanning data dir: {}", data_dir.display());

    // ---- 1. 识别 + 系列文件夹校验 ----
    let mut files: Vec<ScannedFile> = Vec::new();
    scan_dir(&data_dir, &data_dir, &mut files);

    let mut by_kind: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut misplaced: Vec<&ScannedFile> = Vec::new();
    for f in &files {
        *by_kind.entry(kind_name(f.kind)).or_insert(0) += 1;
        if !f.placed_correctly {
            misplaced.push(f);
        }
    }

    println!("\n== Identified files by kind ==");
    for (k, n) in &by_kind {
        println!("  {k:<10} {n}");
    }
    println!("  TOTAL      {}", files.len());

    println!("\n== Series-folder placement check ==");
    if misplaced.is_empty() {
        println!("  All classified files are correctly placed in their series folders. [OK]");
    } else {
        println!("  {} misplaced file(s):", misplaced.len());
        for f in &misplaced {
            println!(
                "  - [{}] {}  ({})",
                kind_name(f.kind),
                f.path.display(),
                f.note
            );
        }
    }

    // 展示几个系列文件夹推导示例（用户要的 aaa.bbb -> aaa/bbb/aaa.bbb.yaml）
    println!("\n== Series-folder derivation examples ==");
    let mut shown = 0;
    for f in &files {
        if matches!(f.kind, ResourceKind::Schema | ResourceKind::Dict)
            && f.resource_id.contains('.')
            && shown < 6
        {
            println!(
                "  id `{id}` -> `{root}/{segs}/{file}`",
                id = f.resource_id,
                root = scan::ResourceKind::category_root(f.kind).unwrap_or(""),
                segs = f.expected_dir.join("/"),
                file = f.path.file_name().unwrap().to_string_lossy(),
            );
            shown += 1;
        }
    }

    // ---- 2. 词典击键统计 ----
    println!("\n== Average / Max keystrokes per dictionary ==");
    let mut all_entries = 0usize;
    let mut all_keys = 0usize;
    let mut overall_max = 0usize;
    let mut overall_max_example = String::new();
    let mut any = false;
    for f in &files {
        if f.kind != ResourceKind::Dict {
            continue;
        }
        if let Some(st) = keystrokes::compute_keystrokes(&f.path, &f.resource_id) {
            any = true;
            all_entries += st.entry_count;
            all_keys += st.total_keys;
            if st.max_keys > overall_max {
                overall_max = st.max_keys;
                overall_max_example = st.max_keys_example.clone();
            }
            println!(
                "  {id:>22}  entries={entries:>7}  avg={avg:5.2}  max={max:>2}  ({ex})",
                id = st.dict_id,
                entries = st.entry_count,
                avg = st.avg_keys,
                max = st.max_keys,
                ex = st.max_keys_example,
            );
        }
    }
    if !any {
        println!("  (no .dict.yaml found)");
    } else {
        println!("  -----------------------------------------------------------------------");
        println!(
            "  {:<22}  entries={:>7}  avg={:5.2}  max={:>2}  ({})",
            "OVERALL",
            all_entries,
            all_keys as f64 / all_entries as f64,
            overall_max,
            overall_max_example,
        );
    }
}
