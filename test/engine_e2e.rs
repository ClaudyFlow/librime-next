//! 端到端：用真实 Rime 词典数据（test/e2e/build/，从已装 Rime 的 build/ 拷来）
//! 跑通「按键 → 拼音分词 → 查词 → 候选 → 选择 → 提交」。
//!
//! 对照 C++ librime：同样喂 "nihao"，应能在候选里拿到「你好」并提交。

use librime_next::dict::dictionary::Dictionary;
use librime_next::engine::{SelectResult, Session};
use std::path::PathBuf;

fn build_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/e2e/build")
}

/// 把一个 ASCII 串逐个字符作为按键喂给会话。
fn type_ascii(session: &mut Session, s: &str) {
    for ch in s.chars() {
        let keycode = ch as u32;
        assert!(session.process_key(keycode, 0), "按键 {ch} 应被消费");
    }
}

#[test]
fn engine_loads_luna_pinyin_simp() {
    let session = Session::create(&build_dir(), "luna_pinyin_simp");
    assert!(
        session.engine.is_some(),
        "luna_pinyin_simp 引擎应能加载（检查 .schema.yaml / .bin 路径）"
    );
    assert_eq!(session.schema_id(), Some("luna_pinyin_simp"));
}

#[test]
fn debug_translate() {
    use librime_next::dict::prism::Prism;
    use librime_next::dict::table::Table;
    let dir = build_dir();
    let tp = dir.join("luna_pinyin.table.bin");
    let pp = dir.join("luna_pinyin_simp.prism.bin");
    match Table::open(&tp) {
        Ok(_) => println!("table OK"),
        Err(e) => println!("table ERR: {e}"),
    }
    match Prism::open(&pp) {
        Ok(_) => println!("prism OK"),
        Err(e) => println!("prism ERR: {e}"),
    }
    let dict = Dictionary::open(&build_dir(), "luna_pinyin", "luna_pinyin_simp")
        .expect("字典应打开");
    let m = dict.prism.common_prefix_search("nihao");
    println!("prefix matches for 'nihao': {:?}", m);
    let entries = dict.translate("nihao");
    println!("translate('nihao') -> {} entries", entries.len());
    for e in entries.iter().take(15) {
        println!("  end={} text={} weight={}", e.end, e.text, e.weight);
    }
    let has_nihao = entries.iter().any(|e| e.text == "你好");
    println!("  >>> contains 你好? {has_nihao}");
    let end5: Vec<&str> = entries.iter().filter(|e| e.end == 5).map(|e| e.text.as_str()).collect();
    println!("  >>> end==5 entries (first 10): {:?}", &end5[..end5.len().min(10)]);
    let m2 = dict.prism.common_prefix_search("ni");
    println!("prefix matches for 'ni': {:?}", m2);
}

#[test]
fn debug_spelling_map() {
    use librime_next::dict::prism::Prism;
    use librime_next::dict::table::Table;
    let dir = build_dir();
    let table = Table::open(dir.join("luna_pinyin.table.bin")).expect("table");
    let prism = Prism::open(dir.join("luna_pinyin_simp.prism.bin")).expect("prism");

    let syl = table.syllabary().expect("syllabary");
    println!("table num_syllables = {}", syl.size());
    println!("table id_of('ni') = {:?}", syl.id_of("ni"));
    println!("table id_of('hao') = {:?}", syl.id_of("hao"));
    println!("table id_of('n') = {:?}", syl.id_of("n"));
    println!("table id_of('na') = {:?}", syl.id_of("na"));

    for spelling in ["n", "ni", "hao", "h"] {
        let sid = prism.get_value(spelling);
        println!("prism spelling {spelling:?} -> spelling_id {sid:?}");
        if let Some(sid) = sid {
            let syl_id = prism.spelling_syllable_id(sid);
            println!("   spelling_syllable_id({sid}) = {syl_id}");
            println!("   table syllable at {syl_id} = {:?}", table.get_syllable_by_id(syl_id));
            // also show first descriptor
            if let Some(d) = prism.spelling_descriptor(sid, 0) {
                println!("   descriptor: syllable_id={} type={} cred={} tips={:?}", d.syllable_id, d.r#type, d.credibility, d.tips);
            } else {
                println!("   descriptor(0) is None -> degraded to spelling_id");
            }
        }
    }

    // Direct table lookup with the real syllable ids
    let ni_id = syl.id_of("ni").unwrap();
    let hao_id = syl.id_of("hao").unwrap();
    let code = librime_next::dict::vocabulary::Code::from_slice(&[ni_id, hao_id]);
    let mut acc = table.query_phrases(&code);
    println!("query_phrases([ni={ni_id}, hao={hao_id}]) ->");
    while !acc.exhausted() {
        if let Some(e) = acc.entry() {
            println!("   text={} weight={}", table.entry_text(&e), e.weight);
        }
        acc.next();
    }
    // single ni
    let code1 = librime_next::dict::vocabulary::Code::from_slice(&[ni_id]);
    let mut acc1 = table.query_phrases(&code1);
    println!("query_phrases([ni={ni_id}]) ->");
    let mut cnt = 0;
    while !acc1.exhausted() && cnt < 10 {
        if let Some(e) = acc1.entry() {
            println!("   text={} weight={}", table.entry_text(&e), e.weight);
        }
        acc1.next();
        cnt += 1;
    }
}

#[test]
fn engine_translates_nihao_to_candidates() {
    let mut session = Session::create(&build_dir(), "luna_pinyin_simp");
    type_ascii(&mut session, "nihao");

    let ctx = session.context().expect("应有上下文");
    assert!(ctx.is_composing(), "nihao 应处于组字态");

    let seg = ctx
        .composition
        .segmentation
        .segments
        .first()
        .expect("应有组合段");
    let menu = seg.menu.as_ref().expect("应有候选菜单");
    let texts: Vec<&str> = menu.candidates().iter().map(|c| c.text.as_str()).collect();

    assert!(
        texts.iter().any(|t| *t == "你好"),
        "「你好」应出现在候选里，实际前若干个：{:?}",
        &texts[..texts.len().min(12)]
    );
    // 单字「你」(覆盖 ni) 也应存在 —— 逐音节确认的基础
    assert!(texts.iter().any(|t| *t == "你"), "「你」应存在");
    // 注：「好」单独不是整段 "nihao" 的合法候选（它只覆盖 input[2..5]），
    // 只有确认「你」后剩余 "hao" 作为新段才会出现「好」，见 engine_per_syllable_confirmation。
}

#[test]
fn engine_select_commit_nihao() {
    let mut session = Session::create(&build_dir(), "luna_pinyin_simp");
    type_ascii(&mut session, "nihao");

    let idx = {
        let ctx = session.context().unwrap();
        let menu = ctx
            .composition
            .segmentation
            .segments
            .first()
            .unwrap()
            .menu
            .as_ref()
            .unwrap();
        menu.candidates()
            .iter()
            .position(|c| c.text == "你好")
            .expect("候选里应有「你好」")
    };

    // 选「你好」：覆盖整段输入 → 提交
    let result = session.choose(idx);
    assert!(matches!(result, Some(SelectResult::Committed(_))), "应选到「你好」并整段提交");

    let commit = session.take_commit();
    assert_eq!(commit.as_deref(), Some("你好"), "提交文本应为「你好」");
}

#[test]
fn engine_per_syllable_confirmation() {
    // 逐音节确认：先选「你」(end=2)，剩余 "hao" 继续组字，再选「好」。
    let mut session = Session::create(&build_dir(), "luna_pinyin_simp");
    type_ascii(&mut session, "nihao");

    let ni_idx = {
        let ctx = session.context().unwrap();
        let menu = ctx
            .composition
            .segmentation
            .segments
            .first()
            .unwrap()
            .menu
            .as_ref()
            .unwrap();
        menu.candidates()
            .iter()
            .position(|c| c.text == "你")
            .expect("候选里应有「你」")
    };
    // choose 后引擎会重排组合，输入应剩 "hao"
    let result = session.choose(ni_idx);
    assert!(matches!(result, Some(SelectResult::Continued)), "选「你」应只确认前缀");

    // 「你」被确认后，输入应剩 "hao"，仍在组字态
    let ctx = session.context().unwrap();
    assert_eq!(ctx.input, "hao", "确认「你」后应剩 hao");
    assert!(ctx.is_composing());

    // 此时菜单应是 "hao" 的候选，含「好」
    let hao_idx = {
        let menu = ctx
            .composition
            .segmentation
            .segments
            .first()
            .unwrap()
            .menu
            .as_ref()
            .unwrap();
        menu.candidates()
            .iter()
            .position(|c| c.text == "好")
            .expect("候选里应有「好」")
    };
    let result = session.choose(hao_idx);
    assert!(matches!(result, Some(SelectResult::Committed(_))), "选「好」应整段提交");

    assert_eq!(session.take_commit().as_deref(), Some("你好"));
}
