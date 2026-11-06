//! mmap 与 fs::read 字节级一致性校验(2026-11-01 mmap 改造回归)
//!
//! 2026-11-03：样本从 `.gram`（octagram 语法模型已整体移除）换成
//! `build/` 下最大的几本 `.bin` 词典——mmap 的价值本来就在大文件上，
//! 换成词典不影响这条测试要守的东西（映射视图与直接读盘逐字节相同）。
use librime_next::dict::mapped_file::MappedFile;

fn collect_bins(dir: &std::path::Path, out: &mut Vec<(u64, std::path::PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_bins(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("bin") {
            if let Ok(meta) = entry.metadata() {
                out.push((meta.len(), path));
            }
        }
    }
}

#[test]
fn mmap_bytes_match_fs_read() {
    let appdata = std::env::var("APPDATA").unwrap();
    let build = std::path::PathBuf::from(format!("{appdata}/Rime-Next/build"));
    let mut bins = Vec::new();
    collect_bins(&build, &mut bins);
    assert!(!bins.is_empty(), "{} 下没有 .bin 可校验", build.display());
    // 只挑最大的四本：小文件读一次就完了，mmap 的意义在大文件。
    bins.sort_by(|a, b| b.0.cmp(&a.0));
    let mut checked = 0;
    for (len, path) in bins.iter().take(4) {
        let direct = std::fs::read(path).unwrap();
        let mapped = MappedFile::open_read_only(path).unwrap();
        let view = mapped.as_bytes_view_for_test();
        assert_eq!(direct.len(), view.len(), "{} 长度不一致", path.display());
        assert_eq!(direct, view, "{} 字节不一致", path.display());
        println!("mmap ok: {} ({len} bytes)", path.display());
        checked += 1;
    }
    assert!(checked >= 4, "应至少校验 4 本 .bin,实际 {checked}");
}
