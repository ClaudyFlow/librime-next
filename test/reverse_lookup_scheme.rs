//! 反查走"方案"的端到端验证（2026-10-28）。
//!
//! 方案源码里 `reverse_lookup: schema: pinyin.luna` 只写了"用哪份方案反查"；
//! 棱镜名归目标方案自己（`pinyin.luna` 的棱镜叫 `pinyin`），所以
//! `engine/gear/reverse_lookup_translator.rs::resolve_prism_name` 会去 `build/` 里
//! 打开那份方案、取它的 `translator/prism`。
//!
//! 这里用**真机部署产物**验一遍：`stroke` 打 `` `nihao ``、`wubi.86` 打 `znihao`
//! 都要出「你好」。产物不是新布局（没有 `pinyin/luna/pinyin.luna.schema.yaml`）就跳过，
//! 老布局下这条路径本来就不存在。

use librime_next::engine::Session;
use std::path::{Path, PathBuf};

/// 部署产物目录：优先 `LRN_BUILD_DIR`，否则 `%APPDATA%\Rime-Next\build`。
fn build_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("LRN_BUILD_DIR") {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Rime-Next").join("build"))
}

/// 新布局守卫：`dict/` 拆家后 `pinyin.luna` 的编译产物才在 `build/pinyin/luna/` 下。
fn new_layout(dir: &Path) -> bool {
    dir.join("pinyin/luna/pinyin.luna.schema.yaml").is_file()
}

fn type_str(session: &mut Session, s: &str) {
    for c in s.chars() {
        session.process_key(c as u32, 0);
    }
}

fn candidates(session: &Session) -> Vec<String> {
    session
        .context()
        .map(|c| {
            c.composition
                .segmentation
                .segments
                .iter()
                .filter_map(|s| s.menu.as_ref())
                .flat_map(|m| m.candidates().iter().map(|c| c.text.clone()))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn stroke_reverse_lookup_borrows_luna_scheme() {
    let Some(dir) = build_dir() else {
        eprintln!("跳过：没有部署产物目录");
        return;
    };
    if !new_layout(&dir) {
        eprintln!("跳过：部署产物不是新布局（无 build/pinyin/luna/）");
        return;
    }
    let mut s = Session::create(&dir, "stroke");
    assert!(s.engine.is_some(), "stroke 方案应能加载");
    type_str(&mut s, "`nihao");
    let cands = candidates(&s);
    println!("stroke 反查 `nihao 候选前 8: {:?}", &cands[..cands.len().min(8)]);
    assert!(
        cands.iter().any(|c| c == "你好"),
        "反查 `nihao 应出「你好」（棱镜名取自 pinyin.luna 方案），实际: {cands:?}"
    );
}

#[test]
fn wubi_reverse_lookup_borrows_luna_scheme() {
    let Some(dir) = build_dir() else {
        eprintln!("跳过：没有部署产物目录");
        return;
    };
    if !new_layout(&dir) {
        eprintln!("跳过：部署产物不是新布局（无 build/pinyin/luna/）");
        return;
    }
    let mut s = Session::create(&dir, "wubi.86");
    assert!(s.engine.is_some(), "wubi.86 方案应能加载");
    type_str(&mut s, "znihao");
    let cands = candidates(&s);
    println!("wubi 反查 znihao 候选前 8: {:?}", &cands[..cands.len().min(8)]);
    assert!(
        cands.iter().any(|c| c == "你好"),
        "反查 znihao 应出「你好」，实际: {cands:?}"
    );
}

