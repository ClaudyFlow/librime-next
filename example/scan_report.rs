//! 用 lrn 正式的 `lever::scan` 扫描真实数据目录，输出识别/校验/击键报告。
//! 用法：cargo run --example scan_report -- <数据目录>
//!
//! 与独立工具 `tool/rime-scan` 的算法同源；本 example 证明镜像进 lrn 后
//! 对真实数据的输出与工具基线一致（文件数、0 误报、击键数）。

use std::path::Path;

use librime_next::lever::scan::{keystrokes_for, misplaced, scan_data_root, ResourceKind};

fn main() {
    let root = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "C:/code/Weasel-Next/Rime-Next".to_string());
    let root = Path::new(&root);
    if !root.is_dir() {
        eprintln!("数据目录不存在: {}", root.display());
        std::process::exit(2);
    }

    let files = scan_data_root(root);
    println!("数据目录: {}", root.display());
    println!("识别文件总数: {}", files.len());

    // 按类别统计
    let count = |kind: ResourceKind| files.iter().filter(|f| f.kind == kind).count();
    println!(
        "  schema={} dict={} config={} theme={} lua={} plugin-data={} essay={} model={}",
        count(ResourceKind::Schema),
        count(ResourceKind::Dict),
        count(ResourceKind::Config),
        count(ResourceKind::Theme),
        count(ResourceKind::Lua),
        count(ResourceKind::PluginData),
        count(ResourceKind::Essay),
        count(ResourceKind::Model),
    );

    // 摆放校验
    let bad = misplaced(&files);
    println!("摆放错误: {}", bad.len());
    for f in &bad {
        println!("  [{}] {}: {}", f.kind.kind_label(), f.path.display(), f.note);
    }

    // 击键统计
    let stats = keystrokes_for(&files);
    println!("词典击键统计 ({} 个):", stats.len());
    for s in &stats {
        println!(
            "  {:<24} entries={:>7} avg={:>5.2} max={:>3} (示例 `{}`)",
            s.dict_id, s.entry_count, s.avg_keys, s.max_keys, s.max_keys_example
        );
    }
    let total_entries: usize = stats.iter().map(|s| s.entry_count).sum();
    let total_keys: usize = stats.iter().map(|s| s.total_keys).sum();
    let max_keys = stats.iter().map(|s| s.max_keys).max().unwrap_or(0);
    if !stats.is_empty() {
        println!(
            "  OVERALL entries={} total_keys={} avg={:.2} max={}",
            total_entries,
            total_keys,
            total_keys as f64 / total_entries as f64,
            max_keys
        );
    }
}
