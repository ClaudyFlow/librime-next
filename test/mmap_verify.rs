//! mmap 与 fs::read 字节级一致性校验(2026-11-01 mmap 改造回归)
use librime_next::dict::mapped_file::MappedFile;

#[test]
fn mmap_bytes_match_fs_read() {
    let appdata = std::env::var("APPDATA").unwrap();
    let model = std::path::PathBuf::from(format!("{appdata}/Rime-Next/model"));
    let mut checked = 0;
    for entry in std::fs::read_dir(&model).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("gram") { continue; }
        let direct = std::fs::read(&path).unwrap();
        let mapped = MappedFile::open_read_only(&path).unwrap();
        let view = mapped.as_bytes_view_for_test();
        assert_eq!(direct.len(), view.len(), "{} 长度不一致", path.display());
        assert_eq!(direct, view, "{} 字节不一致", path.display());
        checked += 1;
    }
    assert!(checked >= 4, "应至少校验 4 本 gram,实际 {checked}");
}
