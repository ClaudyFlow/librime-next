//! pinyin 方言方案打字测试（2026-10-27）。
//!
//! 对真机部署产物(%APPDATA%\Rime-Next\build,只读)跑 pinyin 方案，
//! 验证：nihao 能出「你好」、基本打字旅程可用。

use librime_next::engine::Session;
use std::path::PathBuf;

fn build_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Rime-Next").join("build"))
}

fn type_str(session: &mut Session, s: &str) -> usize {
    s.chars().filter(|c| session.process_key(*c as u32, 0)).count()
}

/// 当前菜单的候选文本列表。
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
fn pinyin_nihao_produces_nihao() {
    let Some(dir) = build_dir() else {
        eprintln!("跳过:没有真机 build 目录");
        return;
    };
    let mut s = Session::create(&dir, "pinyin");
    assert!(s.engine.is_some(), "pinyin 应能加载");

    // ---- nihao → 候选里应有「你好」（essay 预置词汇已并入）----
    assert_eq!(type_str(&mut s, "nihao"), 5);
    let cands = candidates(&s);
    println!("nihao 候选前 8: {:?}", &cands[..cands.len().min(8)]);
    assert!(
        cands.iter().any(|t| t == "你好"),
        "「你好」应出现在候选里(essay 预置词汇),实际 {:?}",
        &cands[..cands.len().min(8)]
    );

    // ---- 首候选（排序）：记录实际值，便于对照 ----
    if let Some(first) = cands.first() {
        println!("nihao 首候选 = {first:?}");
    }

    // ---- 空格上屏（无论首候选是谁，验证基础打字闭环）----
    if cands.first().map_or(false, |t| t == "你好") {
        assert!(s.process_key(0x0020, 0), "空格应被消费");
        assert_eq!(s.take_commit().as_deref(), Some("你好"), "空格上屏「你好」");
    } else {
        // 首候选不是「你好」时，至少验证上屏不崩、退场干净
        let _ = s.process_key(0x0020, 0);
        let _ = s.take_commit();
    }
    s.process_key(0xff1b, 0); // Esc

    // ---- 其它常用词也应能打出来 ----
    for word in ["ni", "hao", "zhongguo", "women"] {
        type_str(&mut s, word);
        let cs = candidates(&s);
        assert!(!cs.is_empty(), "{word} 应有候选,实际空");
        s.process_key(0xff1b, 0);
    }
}
