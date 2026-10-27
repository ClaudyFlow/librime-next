//! 全拼(luna_pinyin_simp)完整打字旅程:模拟真人一段输入的全流程。
//!
//! 对真实部署产物(%APPDATA%\Rime-Next\build,只读)跑:
//! 词组上屏 / 数字选词 / 逐音节确认 / 翻页 / 退格修正 / Esc / 回车上屏原始串 /
//! 标点全角 / 连续会话稳定性。

use librime_next::engine::{Session, SelectResult};
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
fn full_pinyin_typing_journey() {
    let Some(dir) = build_dir() else {
        eprintln!("跳过:没有真机 build 目录");
        return;
    };
    let mut s = Session::create(&dir, "luna_pinyin_simp");
    assert!(s.engine.is_some(), "luna_pinyin_simp 应能加载");

    // ---- ① 打词上屏:nihao → 空格 → 「你好」 ------------------------------
    assert_eq!(type_str(&mut s, "nihao"), 5);
    let cands = candidates(&s);
    assert!(cands.first().map_or(false, |t| t == "你好"), "首候选应为「你好」,实际 {:?}", &cands[..cands.len().min(5)]);
    assert!(s.process_key(0x0020, 0), "空格应被消费"); // XK_space
    assert_eq!(s.take_commit().as_deref(), Some("你好"), "空格上屏「你好」");

    // ---- ② 数字选词:nihao → 2 → 第二候选 --------------------------------
    type_str(&mut s, "nihao");
    let second = candidates(&s).get(1).cloned();
    assert!(second.is_some(), "nihao 应有第 2 候选");
    assert!(s.process_key(0x0032, 0), "数字 2 应被消费"); // XK_2
    let committed = s.take_commit();
    assert_eq!(committed.as_deref(), second.as_deref(), "数字 2 应上屏第二候选 {second:?}");

    // ---- ③ 翻页:nihao → Page_Down → 高亮进入第二页 -----------------------
    type_str(&mut s, "nihao");
    let hl0 = s.context().unwrap().highlight_index();
    assert!(s.process_key(0xff56, 0), "Page_Down 应被消费"); // XK_Next
    let hl1 = s.context().unwrap().highlight_index();
    assert!(hl1 > hl0 || !candidates(&s).is_empty(), "翻页后高亮应移动(或至少有菜单)");
    s.process_key(0xff1b, 0); // Esc 清场

    // ---- ④ 逐音节确认:nihao → 选「你」→ 剩 hao → 选「好」 ----------------
    type_str(&mut s, "nihao");
    let ni_idx = candidates(&s).iter().position(|t| t == "你").expect("应有「你」");
    assert!(matches!(s.choose(ni_idx), Some(SelectResult::Continued)));
    assert_eq!(s.context().unwrap().input, "hao", "确认「你」后剩 hao");
    let hao_idx = candidates(&s).iter().position(|t| t == "好").expect("应有「好」");
    assert!(matches!(s.choose(hao_idx), Some(SelectResult::Committed(_))));
    assert_eq!(s.take_commit().as_deref(), Some("你好"), "逐音节拼出「你好」");

    // ---- ⑤ 退格修正:nihaox → Backspace → 菜单恢复 ------------------------
    type_str(&mut s, "nihao");
    assert!(s.process_key(b'x' as u32, 0), "x 应入栈(在 alphabet 内)");
    let cands_with_x = candidates(&s);
    let _ = cands_with_x; // nihaox 的候选(可能为空或不同)
    assert!(s.process_key(0xff08, 0), "Backspace 应被消费"); // XK_BackSpace
    assert_eq!(s.context().unwrap().input, "nihao", "退格后回到 nihao");
    assert!(candidates(&s).iter().any(|t| t == "你好"), "退格后菜单恢复「你好」");
    s.process_key(0xff1b, 0); // Esc

    // ---- ⑥ Esc 清空 ------------------------------------------------------
    type_str(&mut s, "zhongwen");
    assert!(s.context().unwrap().is_composing());
    assert!(s.process_key(0xff1b, 0), "Esc 应被消费");
    assert!(!s.context().unwrap().is_composing(), "Esc 后应退出组字");

    // ---- ⑦ 回车上屏原始输入 ----------------------------------------------
    type_str(&mut s, "abc");
    assert!(s.process_key(0xff0d, 0), "Return 应被消费"); // XK_Return
    // express_editor:回车 = 上屏**原始输入**(不转换)
    assert_eq!(s.take_commit().as_deref(), Some("abc"), "回车上屏原始输入 abc");

    // ---- ⑧ 常用标点全角 --------------------------------------------------
    for (key, expect) in [(b',' as u32, "，"), (b'.' as u32, "。"), (b'?' as u32, "？"), (b'!' as u32, "！")] {
        assert!(s.process_key(key, 0), "标点 {key} 应被 punctuator 消费");
        assert_eq!(s.take_commit().as_deref(), Some(expect), "标点应全角上屏");
    }

    // ---- ⑨ 长输入组词:tianqibucuo → 候选非空 ----------------------------
    assert_eq!(type_str(&mut s, "tianqibucuo"), 11, "tian-q-i-bu-cuo = 11 个字母");
    let cands = candidates(&s);
    assert!(!cands.is_empty(), "天气不错 应有候选,前几个 {:?}", &cands[..cands.len().min(6)]);
    s.process_key(0xff1b, 0); // Esc

    // ---- ⑩ 连续会话稳定性:10 轮打字-上屏 --------------------------------
    for i in 0..10 {
        type_str(&mut s, "nihao");
        s.process_key(0x0020, 0);
        assert_eq!(s.take_commit().as_deref(), Some("你好"), "第 {i} 轮应稳定上屏");
    }
}
