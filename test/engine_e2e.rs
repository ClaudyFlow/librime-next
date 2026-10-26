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

#[test]
fn key_binder_forwards_control_h_as_backspace() {
    // 真实方案里有 `{accept: "Control+h", send: BackSpace, when: composing}`：
    // key_binder 命中后把 BackSpace 塞回处理器链，等于"用 Ctrl+H 当退格"。
    // 这条端到端断言同时证明了三件事：绑定读到、转发生效、datordeleted 真的发生了。
    let mut session = Session::create(&build_dir(), "luna_pinyin_simp");
    type_ascii(&mut session, "nihao");

    let control_mask = librime_next::engine::key_event::K_CONTROL_MASK;
    assert!(
        session.process_key(b'h' as u32, control_mask),
        "Control+h 应被 key_binder 消费"
    );

    let ctx = session.context().expect("应有上下文");
    assert_eq!(ctx.input, "niha", "Control+h 应等价退格，删掉最后一个字符");
}

#[test]
fn punctuator_turns_comma_into_full_width() {
    // 真实方案的 `punctuator/half_shape` 里 `,` 是 `{commit: "，"}`：
    // Punctuator 吃掉按键 → punct_segmentor 打标签 → punct_translator 出候选 → 直接上屏。
    let mut session = Session::create(&build_dir(), "luna_pinyin_simp");
    assert!(
        session.process_key(b',' as u32, 0),
        "逗号应被 punctuator 消费"
    );
    let commit = session.take_commit();
    assert_eq!(commit.as_deref(), Some("，"), "逗号应上屏为全角，实际 {commit:?}");
    assert_eq!(
        session.context().map(|c| c.input.clone()).as_deref(),
        Some(""),
        "已上屏的标点不该留在输入串里"
    );
}

// ── 双拼方案（natural-code 自然码）端到端 ─────────────────────────────────
//
// 自然码键位：ao=k，所以「好」hao = h + k。
// 输入 "nihk" = ni(ni) + hao(hk) → 候选里应有「你好」。

fn double_pinyin_build_dir() -> PathBuf {
    // 双拼词典复用 luna_pinyin.table.bin，prism 单独（double_pinyin_ziranma.prism.bin）。
    // 这两份都放在 test/e2e/build 下。
    build_dir()
}

#[test]
fn engine_loads_double_pinyin_ziranma() {
    let session = Session::create(&double_pinyin_build_dir(), "double_pinyin_ziranma");
    assert!(
        session.engine.is_some(),
        "double_pinyin_ziranma 引擎应能加载"
    );
}

#[test]
fn engine_translates_double_pinyin_ziranma() {
    let mut session = Session::create(&double_pinyin_build_dir(), "double_pinyin_ziranma");
    type_ascii(&mut session, "nihk");

    let ctx = session.context().expect("应有上下文");
    assert!(ctx.is_composing(), "nihk 应处于组字态");

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
}

#[test]
fn double_pinyin_preedit_shows_full_syllables() {
    // preedit_format 应该把双拼键位还原成完整拼音，
    // 而不是显示裸键位 "nihk"。
    let mut session = Session::create(&double_pinyin_build_dir(), "double_pinyin_ziranma");
    type_ascii(&mut session, "nihk");

    let preedit = session.context().map(|c| c.get_preedit()).expect("应有预编辑");
    // preedit_format 生效后，预编辑串应不同于裸键位 "nihk"
    assert_ne!(preedit.text, "nihk", "preedit_format 应把键位还原成完整拼音");
    // 且应包含还原后的音节（"hao" 由 "hk" 还原而来）
    assert!(preedit.text.contains("hao"), "预编辑应包含还原后的音节 hao，实际：{}", preedit.text);
}

// ── 形码方案（table_translator）端到端验证 ───────────────────────────────
//
// 用真实 wubi86 / stroke 词典（test/e2e/build/*.table.bin + *.prism.bin，
// 从已装 Rime 的 build/ 拷来）跑通「按键 → 整码命中/前缀补全 → 候选 → 选词 → 提交」。
// 注意：本仓库的 wubi86 词典不含单字母简码（一=ggl/ggll，不是 g），所以单字母 g/d/k/l/r
// 不是合法码；验证只用真实存在的码：wq=你, vb=好（五笔）；h=一, hs=十（笔画，h横 s竖 p撇 n捺 z折）。

#[test]
fn table_translator_wubi86_produces_candidates() {
    // wubi86 引擎（table_translator + 真实词典）应加载并出候选。
    let mut session = Session::create(&build_dir(), "wubi86");
    assert!(session.engine.is_some(), "wubi86 引擎应加载");

    type_ascii(&mut session, "wq");
    let texts = candidate_texts(&mut session);
    assert!(
        texts.iter().any(|t| t == "你"),
        "wq 应出「你」，实际前若干：{:?}",
        &texts[..texts.len().min(12)]
    );

    let mut session = Session::create(&build_dir(), "wubi86");
    type_ascii(&mut session, "vb");
    let texts = candidate_texts(&mut session);
    assert!(texts.iter().any(|t| t == "好"), "vb 应出「好」，实际：{:?}", texts);
}

#[test]
fn table_translator_stroke_produces_candidates() {
    let mut session = Session::create(&build_dir(), "stroke");
    assert!(session.engine.is_some(), "stroke 引擎应加载");

    type_ascii(&mut session, "h");
    let texts = candidate_texts(&mut session);
    assert!(texts.iter().any(|t| t == "一"), "h 应出「一」，实际：{:?}", texts);

    // 前缀补全：打字 "hs"，码为 "h"（一）的词应作为补全候选出现。
    let mut session = Session::create(&build_dir(), "stroke");
    type_ascii(&mut session, "hs");
    let texts = candidate_texts(&mut session);
    assert!(
        texts.iter().any(|t| t == "十"),
        "hs 应出「十」，实际：{:?}",
        &texts[..texts.len().min(12)]
    );
    assert!(
        texts.iter().any(|t| t == "一"),
        "hs 的补全候选里应含「一」（码 h 是 hs 的前缀），实际：{:?}",
        &texts[..texts.len().min(12)]
    );
}

#[test]
fn table_translator_wubi86_select_commit() {
    let mut session = Session::create(&build_dir(), "wubi86");
    type_ascii(&mut session, "wq");
    let idx = candidate_texts(&mut session)
        .iter()
        .position(|t| t == "你")
        .expect("候选里应有「你」");
    let result = session.choose(idx);
    assert!(matches!(result, Some(librime_next::engine::SelectResult::Committed(_))), "选「你」应整段提交");
    assert_eq!(session.take_commit().as_deref(), Some("你"), "提交文本应为「你」");
}

#[test]
fn table_translator_stroke_select_commit() {
    let mut session = Session::create(&build_dir(), "stroke");
    type_ascii(&mut session, "h");
    let idx = candidate_texts(&mut session)
        .iter()
        .position(|t| t == "一")
        .expect("候选里应有「一」");
    let result = session.choose(idx);
    assert!(matches!(result, Some(librime_next::engine::SelectResult::Committed(_))), "选「一」应整段提交");
    assert_eq!(session.take_commit().as_deref(), Some("一"), "提交文本应为「一」");
}

/// 取当前首个组合段的候选文本列表（端到端断言辅助）。
fn candidate_texts(session: &mut Session) -> Vec<String> {
    session
        .context()
        .and_then(|ctx| {
            ctx.composition
                .segmentation
                .segments
                .first()
                .and_then(|seg| seg.menu.as_ref())
                .map(|m| m.candidates().iter().map(|c| c.text.clone()).collect())
        })
        .unwrap_or_default()
}

// ── simplifier（简繁转换过滤器）端到端检测 ───────────────────────────────
//
// luna_pinyin_simp 的词典是繁体（語言），靠 simplifier@zh_hans（option_name=zh_hans，
// opencc_config=t2s.json）在开关打开时转简（语言）。下面把「yuyan」在 zh_hans
// 开/关两种状态下的候选真实打印出来，确认 simplifier 真的在方案链路里生效、且受开关门控。

#[test]
fn simplifier_probe_yuyan() {
    // 开关开着：应得到简体「语言」
    let mut on = Session::create(&build_dir(), "luna_pinyin_simp");
    on.context_mut().unwrap().set_option("zh_hans", true);
    type_ascii(&mut on, "yuyan");
    let on_texts = candidate_texts(&mut on);
    println!("zh_hans=ON  yuyan -> {:?}", &on_texts[..on_texts.len().min(12)]);

    // 开关关着：应保留繁体「語言」
    let mut off = Session::create(&build_dir(), "luna_pinyin_simp");
    off.context_mut().unwrap().set_option("zh_hans", false);
    type_ascii(&mut off, "yuyan");
    let off_texts = candidate_texts(&mut off);
    println!("zh_hans=OFF yuyan -> {:?}", &off_texts[..off_texts.len().min(12)]);
}

#[test]
fn simplifier_zh_hans_on_yields_simplified() {
    let mut session = Session::create(&build_dir(), "luna_pinyin_simp");
    session.context_mut().unwrap().set_option("zh_hans", true);
    type_ascii(&mut session, "yuyan");
    let texts = candidate_texts(&mut session);
    assert!(
        texts.iter().any(|t| t == "语言"),
        "zh_hans 开时应出简体「语言」，实际前若干：{:?}",
        &texts[..texts.len().min(12)]
    );
    assert!(
        !texts.iter().any(|t| t == "語言"),
        "zh_hans 开时不应出现繁体「語言」，实际：{:?}",
        &texts[..texts.len().min(12)]
    );
}

#[test]
fn simplifier_zh_hans_off_yields_traditional() {
    let mut session = Session::create(&build_dir(), "luna_pinyin_simp");
    session.context_mut().unwrap().set_option("zh_hans", false);
    type_ascii(&mut session, "yuyan");
    let texts = candidate_texts(&mut session);
    assert!(
        texts.iter().any(|t| t == "語言"),
        "zh_hans 关时应为繁体「語言」，实际前若干：{:?}",
        &texts[..texts.len().min(12)]
    );
}

// ── 整句组句（WordGraph + Poet）端到端检测 ───────────────────────────────
//
// grammar 的前置：把音节图上的词条拼成整句。三条断言分别验证
// (1) 长输入出整句且排第一、(2) 有精确词条时不组句、(3) 整句能整体上屏。

#[test]
fn sentence_nihaoshijie_yields_whole_sentence_first() {
    let mut session = Session::create(&build_dir(), "luna_pinyin_simp");
    type_ascii(&mut session, "nihaoshijie");
    let texts = candidate_texts(&mut session);
    assert_eq!(
        texts.first().map(String::as_str),
        Some("\u{4f60}\u{597d}\u{4e16}\u{754c}"),
        "整句「你好世界」应排第一，实际前若干：{:?}",
        &texts[..texts.len().min(8)]
    );
    // 词级候选必须同时保留，不能只给整句。
    assert!(
        texts.iter().any(|t| t == "\u{4f60}\u{597d}"),
        "词级「你好」应仍在候选里，实际：{:?}",
        &texts[..texts.len().min(8)]
    );
}

#[test]
fn sentence_nihao_skips_sentence_for_exact_phrase() {
    // `nihao` 整段能命中精确词条「你好」—— 按 C++ 的条件 2 不组句。
    let mut session = Session::create(&build_dir(), "luna_pinyin_simp");
    type_ascii(&mut session, "nihao");
    let texts = candidate_texts(&mut session);
    assert_eq!(
        texts.first().map(String::as_str),
        Some("\u{4f60}\u{597d}"),
        "首候选应是词级「你好」，实际：{:?}",
        &texts[..texts.len().min(8)]
    );
    assert!(
        !texts.iter().any(|t| t == "\u{4f60}\u{8017}"),
        "不应把「你耗」这类拼接当成精确匹配，实际：{:?}",
        &texts[..texts.len().min(8)]
    );
}

#[test]
fn sentence_commit_yields_full_text() {
    let mut session = Session::create(&build_dir(), "luna_pinyin_simp");
    type_ascii(&mut session, "nihaoshijie");
    session.choose(0).expect("应能选中首个候选");
    let commit = session.take_commit();
    assert_eq!(
        commit.as_deref(),
        Some("\u{4f60}\u{597d}\u{4e16}\u{754c}"),
        "整句上屏应是「你好世界」，实际：{commit:?}"
    );
}
