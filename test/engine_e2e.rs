//! 端到端：用真实 Rime 词典数据（test/e2e/build/，从已装 Rime 的 build/ 拷来）
//! 跑通「按键 → 拼音分词 → 查词 → 候选 → 选择 → 提交」。
//!
//! 对照 C++ librime：同样喂 "nihao"，应能在候选里拿到「你好」并提交。

use librime_next::dict::dictionary::Dictionary;
use librime_next::engine::gear::charset_filter::is_extended_cjk;
use librime_next::engine::{SelectResult, Session};
use std::path::{Path, PathBuf};

fn build_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/e2e/build")
}

/// 用户文件夹的临时根：学词会写 `<user>/userdb/`。
///
/// **绝不能让它落到 `test/e2e/` 里** —— 上一轮学到的词会改变下一轮的候选顺序，
/// 测试就不确定了。每次建 Session 给一个全新的临时目录，测试之间互不影响。
fn fresh_user_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("lrn_e2e_user_{}_{n}_{tag}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 建一个会话：数据根 = [本次专属临时用户目录, 构建目录]。
fn new_session(build: &std::path::Path, schema: &str) -> Session {
    let user_dir = fresh_user_dir(schema);
    Session::create_with_roots(build, schema, &[user_dir, build.to_path_buf()])
}

/// 把一个 ASCII 串逐个字符作为按键喂给会话。
fn type_ascii(session: &mut Session, s: &str) {
    for ch in s.chars() {
        let keycode = ch as u32;
        assert!(session.process_key(keycode, 0), "按键 {ch} 应被消费");
    }
}

/// 复现真实用户配置下新 DLL 的崩溃：指向 %APPDATA%/Rime-Next/build。
///
/// 部署时新引擎在加载 `luna_pinyin` 资源阶段崩了，单测 fixture 不覆盖真实配置，
/// 这里直接用真实 build 目录跑一遍拿 panic 回溯。
///
/// **关键**：必须按真机宿主的 `data_roots` 顺序传根 —— 用户目录优先、共享目录其次，
/// 否则 `grammar`（`model/*.gram`）和 `simplifier`（`opencc/`）找不到资源，
/// 会误判成"引擎没修好"。
/// 真机部署产物是不是"新键"那一版（`translator/dict`）。
///
/// 2026-10-28 20:5x 起词库键只认 `dict`、**去掉** `dictionary:` 兼容层。旧的部署产物
/// 读不出词库（0 候选），属于"还没重新部署"，不是代码回归 —— 这种环境跳过真机复现。
fn real_build_uses_dict_key(build: &Path) -> bool {
    let mut stack = vec![build.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".schema.yaml"))
            {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if text.contains("\n  dict:") {
                        return true;
                    }
                }
            }
        }
    }
    false
}

#[test]
fn repro_real_luna_pinyin() {
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    let real_build = PathBuf::from(format!("{appdata}/Rime-Next/build"));
    let user_dir = PathBuf::from(format!("{appdata}/Rime-Next"));
    let shared_dir = PathBuf::from("C:/Program Files/Weasel-Next/Rime-Next");
    if !real_build.exists() {
        eprintln!("skip: 真实 build 目录不存在");
        return;
    }
    if !real_build_uses_dict_key(&real_build) {
        eprintln!("skip: 真实产物还是旧键（dictionary:），重新部署后再跑");
        return;
    }
    println!("REAL build dir: {}", real_build.display());
    println!("REAL roots: {:?}", [&user_dir, &shared_dir]);
    let mut session = Session::create_with_roots(
        &real_build,
        "luna_pinyin_simp",
        &[user_dir, shared_dir],
    );
    assert!(
        session.engine.is_some(),
        "luna_pinyin_simp 引擎应能从真实配置加载"
    );
    type_ascii(&mut session, "nihao");
    let texts = candidate_texts(&mut session);
    println!(
        "REAL luna_pinyin_simp candidates: {:?}",
        &texts[..texts.len().min(10)]
    );
    assert!(!texts.is_empty(), "真实配置下应产出候选（之前因系列文件夹路径 bug 全空）");
    assert!(
        texts.iter().any(|t| t.contains("你好")),
        "nihao 应能在候选里拿到「你好」，实得 {texts:?}"
    );
}

#[test]
fn engine_loads_luna_pinyin_simp() {
    let session = new_session(&build_dir(), "luna_pinyin_simp");
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
    let dict =
        Dictionary::open(&build_dir(), "luna_pinyin", "luna_pinyin_simp").expect("字典应打开");
    let m = dict.prism.common_prefix_search("nihao");
    println!("prefix matches for 'nihao': {:?}", m);
    let entries = dict.translate("nihao");
    println!("translate('nihao') -> {} entries", entries.len());
    for e in entries.iter().take(15) {
        println!("  end={} text={} weight={}", e.end, e.text, e.weight);
    }
    let has_nihao = entries.iter().any(|e| e.text == "你好");
    println!("  >>> contains 你好? {has_nihao}");
    let end5: Vec<&str> = entries
        .iter()
        .filter(|e| e.end == 5)
        .map(|e| e.text.as_str())
        .collect();
    println!(
        "  >>> end==5 entries (first 10): {:?}",
        &end5[..end5.len().min(10)]
    );
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
            println!(
                "   table syllable at {syl_id} = {:?}",
                table.get_syllable_by_id(syl_id)
            );
            // also show first descriptor
            if let Some(d) = prism.spelling_descriptor(sid, 0) {
                println!(
                    "   descriptor: syllable_id={} type={} cred={} tips={:?}",
                    d.syllable_id, d.r#type, d.credibility, d.tips
                );
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
    let mut session = new_session(&build_dir(), "luna_pinyin_simp");
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
    let mut session = new_session(&build_dir(), "luna_pinyin_simp");
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
    assert!(
        matches!(result, Some(SelectResult::Committed(_))),
        "应选到「你好」并整段提交"
    );

    let commit = session.take_commit();
    assert_eq!(commit.as_deref(), Some("你好"), "提交文本应为「你好」");
}

#[test]
fn engine_per_syllable_confirmation() {
    // 逐音节确认：先选「你」(end=2)，剩余 "hao" 继续组字，再选「好」。
    let mut session = new_session(&build_dir(), "luna_pinyin_simp");
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
    assert!(
        matches!(result, Some(SelectResult::Continued)),
        "选「你」应只确认前缀"
    );

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
    assert!(
        matches!(result, Some(SelectResult::Committed(_))),
        "选「好」应整段提交"
    );

    assert_eq!(session.take_commit().as_deref(), Some("你好"));
}

#[test]
fn key_binder_forwards_control_h_as_backspace() {
    // 真实方案里有 `{accept: "Control+h", send: BackSpace, when: composing}`：
    // key_binder 命中后把 BackSpace 塞回处理器链，等于"用 Ctrl+H 当退格"。
    // 这条端到端断言同时证明了三件事：绑定读到、转发生效、datordeleted 真的发生了。
    let mut session = new_session(&build_dir(), "luna_pinyin_simp");
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
    let mut session = new_session(&build_dir(), "luna_pinyin_simp");
    assert!(
        session.process_key(b',' as u32, 0),
        "逗号应被 punctuator 消费"
    );
    let commit = session.take_commit();
    assert_eq!(
        commit.as_deref(),
        Some("，"),
        "逗号应上屏为全角，实际 {commit:?}"
    );
    assert_eq!(
        session.context().map(|c| c.input.clone()).as_deref(),
        Some(""),
        "已上屏的标点不该留在输入串里"
    );
}

// ── 数字分隔符（punctuator 的 digit_separators）─────────────────────────────
//
// 出厂 `punctuator` 没写 `digit_separators`，用的是 C++ 默认值 `.:`。
// 效果：数字后面紧跟的 `.` / `:` 不会被翻成「。」「：」，而是原样留在输入串里。

/// 数字键不该被引擎消费 —— 宿主看到 `false` 就自己把字符上屏，引擎只记提交历史。
#[test]
fn digit_keys_pass_through_to_the_host() {
    let mut session = new_session(&build_dir(), "luna_pinyin_simp");
    assert!(
        !session.process_key(b'3' as u32, 0),
        "`3` 不该被任何 processor 吃掉"
    );
    assert_eq!(
        session.context().unwrap().commit_history.latest_text(),
        "3",
        "穿透的可打印键要记一条 thru 记录，punctuator 靠它判断「上一条是数字」"
    );
}

/// `3.14`：`. `留在输入串里、跟后面的数字拼成一段一起上屏，最终打出来就是 `3.14`。
#[test]
fn digit_separator_keeps_decimal_point_in_numbers() {
    let mut session = new_session(&build_dir(), "luna_pinyin_simp");

    assert!(!session.process_key(b'3' as u32, 0), "`3` 应穿透");
    assert!(
        session.process_key(b'.' as u32, 0),
        "小数点应被 punctuator 的数字分隔符逻辑消费"
    );
    assert_eq!(
        session.context().unwrap().input,
        ".",
        "小数点应原样留在输入串里（不是被翻成「。」）"
    );
    // 段的标签是 `punct_number`，候选就是 `.` 本身
    {
        let ctx = session.context().unwrap();
        let seg = ctx.composition.segmentation.segments.first().unwrap();
        assert!(
            seg.has_tag("punct_number"),
            "应打 punct_number 标签，实际 {:?}",
            seg.tags
        );
        assert_eq!(
            seg.get_selected_candidate().map(|c| c.text.as_str()),
            Some("."),
            "数字里的分隔符候选应是字符本身"
        );
    }

    assert!(
        session.process_key(b'1' as u32, 0),
        "紧跟分隔符的数字应触发「拼成整串上屏」"
    );
    assert_eq!(
        session.take_commit().as_deref(),
        Some(".1"),
        "`.` 与 `1` 应一起上屏"
    );
    assert!(!session.process_key(b'4' as u32, 0), "`4` 应穿透");
}

/// `12:30`：冒号同理。
#[test]
fn digit_separator_keeps_colon_in_numbers() {
    let mut session = new_session(&build_dir(), "luna_pinyin_simp");
    assert!(!session.process_key(b'1' as u32, 0));
    assert!(!session.process_key(b'2' as u32, 0));
    assert!(session.process_key(b':' as u32, 0), "冒号应被消费");
    assert_eq!(session.context().unwrap().input, ":");
    assert!(session.process_key(b'3' as u32, 0));
    assert_eq!(session.take_commit().as_deref(), Some(":3"));
    assert!(!session.process_key(b'0' as u32, 0));
}

/// 连按两次同一个分隔符 → 改回普通标点并立刻上屏。
///
/// 用 `digit_demo` 而不是出厂方案：出厂方案里 `{accept: period, send: Page_Down,
/// when: has_menu}` 会在"组合里已有候选"时把第二次 `.` 抢走（转 Page_Down），
/// 根本到不了 punctuator —— 所以这条路径只能在没有该绑定的方案上验证。
///
/// 另外与 C++ 一致：`ReconvertDigitSeparatorAsPunct` 不重译，菜单里留的还是上一轮
/// 的候选，所以上屏的是那个字符本身（`3..` → `3.`）。
#[test]
fn repeating_the_separator_falls_back_to_plain_punctuation() {
    let mut session = new_session(&build_dir(), "digit_demo");
    assert!(!session.process_key(b'3' as u32, 0));
    assert!(session.process_key(b'.' as u32, 0));
    {
        let ctx = session.context().unwrap();
        let seg = ctx.composition.segmentation.segments.first().unwrap();
        assert!(seg.has_tag("punct_number"), "第一个 . 是数字分隔符");
    }
    assert!(session.process_key(b'.' as u32, 0), "第二次 . 也应被消费");
    assert_eq!(
        session.take_commit().as_deref(),
        Some("."),
        "连按后按普通标点路径上屏"
    );
    assert_eq!(
        session.context().unwrap().input,
        "",
        "上屏后输入串应清空"
    );
}

/// `digit_demo` 上的完整 `3.14`：与出厂方案同样的分段提交。
#[test]
fn digit_demo_types_a_decimal_number() {
    let mut session = new_session(&build_dir(), "digit_demo");
    assert!(!session.process_key(b'3' as u32, 0));
    assert!(session.process_key(b'.' as u32, 0));
    assert!(session.process_key(b'1' as u32, 0));
    assert_eq!(session.take_commit().as_deref(), Some(".1"));
    assert!(!session.process_key(b'4' as u32, 0));
}

/// 默认分隔符只有 `.:` —— 逗号不在其中，`1,000` 里的 `,` 会被翻成「，」。
#[test]
fn comma_is_not_a_digit_separator_by_default() {
    let mut session = new_session(&build_dir(), "digit_demo");
    assert!(!session.process_key(b'1' as u32, 0));
    assert!(session.process_key(b',' as u32, 0));
    assert_eq!(
        session.take_commit().as_deref(),
        Some("，"),
        "`,` 不在默认 digit_separators 里，走普通标点路径"
    );
}

/// full_shape 开着时，数字里的分隔符也要变全角（`ShapeFormatter` 参与）。
#[test]
fn digit_separator_respects_full_shape() {
    let mut session = new_session(&build_dir(), "luna_pinyin_simp");
    session.context_mut().unwrap().set_option("full_shape", true);
    assert!(!session.process_key(b'3' as u32, 0));
    assert!(session.process_key(b'.' as u32, 0));
    assert_eq!(
        session.context().unwrap().input,
        ".",
        "输入串仍是半角字符"
    );
    {
        let ctx = session.context().unwrap();
        let seg = ctx.composition.segmentation.segments.first().unwrap();
        assert_eq!(
            seg.get_selected_candidate().map(|c| c.text.as_str()),
            Some("．"),
            "候选应被 shape formatter 转成全角"
        );
    }
    assert!(session.process_key(b'1' as u32, 0));
    assert_eq!(session.take_commit().as_deref(), Some("．1"));
}

/// 分隔符后面紧跟空格：空格被吃进 `punct_number` 段（`3. 14` 里打空格不该变「。」）。
#[test]
fn space_after_digit_separator_is_swallowed() {
    let mut session = new_session(&build_dir(), "luna_pinyin_simp");
    assert!(!session.process_key(b'3' as u32, 0));
    assert!(session.process_key(b'.' as u32, 0));
    assert!(session.process_key(0x20, 0), "空格应被消费");
    assert_eq!(
        session.take_commit().as_deref(),
        Some(". "),
        "分隔符与空格一起上屏"
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
    let session = new_session(&double_pinyin_build_dir(), "double_pinyin_ziranma");
    assert!(
        session.engine.is_some(),
        "double_pinyin_ziranma 引擎应能加载"
    );
}

#[test]
fn engine_translates_double_pinyin_ziranma() {
    let mut session = new_session(&double_pinyin_build_dir(), "double_pinyin_ziranma");
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
    let mut session = new_session(&double_pinyin_build_dir(), "double_pinyin_ziranma");
    type_ascii(&mut session, "nihk");

    let preedit = session
        .context()
        .map(|c| c.get_preedit())
        .expect("应有预编辑");
    // preedit_format 生效后，预编辑串应不同于裸键位 "nihk"
    assert_ne!(
        preedit.text, "nihk",
        "preedit_format 应把键位还原成完整拼音"
    );
    // 且应包含还原后的音节（"hao" 由 "hk" 还原而来）
    assert!(
        preedit.text.contains("hao"),
        "预编辑应包含还原后的音节 hao，实际：{}",
        preedit.text
    );
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
    let mut session = new_session(&build_dir(), "wubi86");
    assert!(session.engine.is_some(), "wubi86 引擎应加载");

    type_ascii(&mut session, "wq");
    let texts = candidate_texts(&mut session);
    assert!(
        texts.iter().any(|t| t == "你"),
        "wq 应出「你」，实际前若干：{:?}",
        &texts[..texts.len().min(12)]
    );

    let mut session = new_session(&build_dir(), "wubi86");
    type_ascii(&mut session, "vb");
    let texts = candidate_texts(&mut session);
    assert!(
        texts.iter().any(|t| t == "好"),
        "vb 应出「好」，实际：{:?}",
        texts
    );
}

#[test]
fn table_translator_stroke_produces_candidates() {
    let mut session = new_session(&build_dir(), "stroke");
    assert!(session.engine.is_some(), "stroke 引擎应加载");

    type_ascii(&mut session, "h");
    let texts = candidate_texts(&mut session);
    assert!(
        texts.iter().any(|t| t == "一"),
        "h 应出「一」，实际：{:?}",
        texts
    );
    // 整码命中必须排在补全前面（`TableTranslation` 的出词顺序 + 类别基数）
    assert_eq!(
        texts.first().map(String::as_str),
        Some("一"),
        "「一」应是 h 的首候选，实际：{:?}",
        &texts[..texts.len().min(8)]
    );

    // 前缀补全：C++ 的 `Prism::ExpandSearch` 只**往前**扩 —— 打出 "h" 时，
    // 码更长（hh=二、hs=十）的词以补全候选出现。
    assert!(
        texts.iter().any(|t| t == "二"),
        "h 的补全候选里应含「二」（码 hh），实际：{:?}",
        &texts[..texts.len().min(12)]
    );

    // 反过来**不**成立：打 "hs" 不会再回头给出码为 "h" 的「一」。
    // （老实现对 `common_prefix_search` 的用法是错的：那是"输入的前缀"，
    //   而 C++ 要的是"键的前缀族"，方向刚好相反。）
    let mut session = new_session(&build_dir(), "stroke");
    type_ascii(&mut session, "hs");
    let texts = candidate_texts(&mut session);
    assert!(
        texts.iter().any(|t| t == "十"),
        "hs 应出「十」，实际：{:?}",
        &texts[..texts.len().min(12)]
    );
    assert!(
        !texts.iter().any(|t| t == "一"),
        "C++ 的补全不回头找更短的码，hs 不该出「一」，实际：{:?}",
        &texts[..texts.len().min(12)]
    );
}

#[test]
fn table_translator_wubi86_select_commit() {
    let mut session = new_session(&build_dir(), "wubi86");
    type_ascii(&mut session, "wq");
    let idx = candidate_texts(&mut session)
        .iter()
        .position(|t| t == "你")
        .expect("候选里应有「你」");
    let result = session.choose(idx);
    assert!(
        matches!(
            result,
            Some(librime_next::engine::SelectResult::Committed(_))
        ),
        "选「你」应整段提交"
    );
    assert_eq!(
        session.take_commit().as_deref(),
        Some("你"),
        "提交文本应为「你」"
    );
}

#[test]
fn table_translator_stroke_select_commit() {
    let mut session = new_session(&build_dir(), "stroke");
    type_ascii(&mut session, "h");
    let idx = candidate_texts(&mut session)
        .iter()
        .position(|t| t == "一")
        .expect("候选里应有「一」");
    let result = session.choose(idx);
    assert!(
        matches!(
            result,
            Some(librime_next::engine::SelectResult::Committed(_))
        ),
        "选「一」应整段提交"
    );
    assert_eq!(
        session.take_commit().as_deref(),
        Some("一"),
        "提交文本应为「一」"
    );
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
    let mut on = new_session(&build_dir(), "luna_pinyin_simp");
    on.context_mut().unwrap().set_option("zh_hans", true);
    type_ascii(&mut on, "yuyan");
    let on_texts = candidate_texts(&mut on);
    println!(
        "zh_hans=ON  yuyan -> {:?}",
        &on_texts[..on_texts.len().min(12)]
    );

    // 开关关着：应保留繁体「語言」
    let mut off = new_session(&build_dir(), "luna_pinyin_simp");
    off.context_mut().unwrap().set_option("zh_hans", false);
    type_ascii(&mut off, "yuyan");
    let off_texts = candidate_texts(&mut off);
    println!(
        "zh_hans=OFF yuyan -> {:?}",
        &off_texts[..off_texts.len().min(12)]
    );
}

#[test]
fn simplifier_zh_hans_on_yields_simplified() {
    let mut session = new_session(&build_dir(), "luna_pinyin_simp");
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
    let mut session = new_session(&build_dir(), "luna_pinyin_simp");
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
    let mut session = new_session(&build_dir(), "luna_pinyin_simp");
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
    let mut session = new_session(&build_dir(), "luna_pinyin_simp");
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
    let mut session = new_session(&build_dir(), "luna_pinyin_simp");
    type_ascii(&mut session, "nihaoshijie");
    session.choose(0).expect("应能选中首个候选");
    let commit = session.take_commit();
    assert_eq!(
        commit.as_deref(),
        Some("\u{4f60}\u{597d}\u{4e16}\u{754c}"),
        "整句上屏应是「你好世界」，实际：{commit:?}"
    );
}

// ── 语法模型（octagram grammar）端到端检测 ──────────────────────────────
//
// `grammar/language` → `<数据根>/model/<language>.gram`。模型不在编译产物目录里，
// 所以必须走 `create_with_roots` 把用户目录一并传进去。

/// `%APPDATA%/Rime-Next` —— 语法模型的出厂落点；没有模型时返回 `None` 让测试跳过。
fn rime_next_root() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    let root = PathBuf::from(appdata).join("Rime-Next");
    let gram = root.join("model/zh-hant-t-essay-bgc.gram");
    gram.is_file().then_some(root)
}

#[test]
fn grammar_schema_loads_and_makes_sentence() {
    let Some(root) = rime_next_root() else {
        eprintln!("跳过：%APPDATA%/Rime-Next/model 下没有 zh-hant-t-essay-bgc.gram");
        return;
    };
    let mut session = Session::create_with_roots(
        &build_dir(),
        "luna_pinyin_grammar",
        std::slice::from_ref(&root),
    );
    type_ascii(&mut session, "zhongguoren");
    let texts = candidate_texts(&mut session);
    println!("grammar: zhongguoren -> {:?}", &texts[..texts.len().min(8)]);
    // 词典是繁体，整句自然是繁体（本方案故意不挂 simplifier）。
    assert_eq!(
        texts.first().map(String::as_str),
        Some("\u{4e2d}\u{570b}\u{4eba}"),
        "整句「中國人」应排第一，实际：{:?}",
        &texts[..texts.len().min(8)]
    );
}

#[test]
fn grammar_schema_works_without_model_root() {
    // 找不到模型时应退化为 constant penalty，但方案本身仍能打字。
    let mut session = new_session(&build_dir(), "luna_pinyin_grammar");
    type_ascii(&mut session, "zhongguo");
    let texts = candidate_texts(&mut session);
    assert!(
        !texts.is_empty(),
        "缺语法模型时仍应产出候选（退回 non_collocation_penalty）"
    );
}

// ── 用户词典（学词 / 调频）端到端检测 ────────────────────────────────────
//
// 闭环：打 `nihao` → 选「你好」→ 提交 → 写进 `<用户目录>/userdb/luna_pinyin.userdb`
// → 换一个会话（同一个用户目录）再打 `nihao`，用户词排在词表词前面。

#[test]
fn user_dict_learns_committed_phrase() {
    use librime_next::dict::db::Db;
    use librime_next::dict::level_db::LevelDb;

    let user_dir = fresh_user_dir("learn");
    let build = build_dir();
    let roots = [user_dir.clone(), build.clone()];

    // 第一次：正常打字 + 选第一个候选（「你好」）。
    // 作用域收在这里 —— 会话持着 LevelDB 的独占锁，不放掉就打不开同一个库。
    {
        let mut session = Session::create_with_roots(&build, "luna_pinyin_simp", &roots);
        type_ascii(&mut session, "nihao");
        let texts = candidate_texts(&mut session);
        assert_eq!(
            texts.first().map(String::as_str),
            Some("\u{4f60}\u{597d}"),
            "首候选应是「你好」，实际：{:?}",
            &texts[..texts.len().min(8)]
        );
        let result = session.choose(0).expect("应能选中第一个候选");
        assert!(
            format!("{result:?}").contains("Committed"),
            "选「你好」应整段提交，实际 {result:?}"
        );
        assert_eq!(session.take_commit().as_deref(), Some("\u{4f60}\u{597d}"));
    }

    // 落盘检查：userdb 里应有一条 `ni hao \t你好`，且 commits = 1
    let db_dir = user_dir.join("userdb");
    let db = LevelDb::new_user_db(&db_dir, "luna_pinyin");
    assert!(
        db.open_readonly(),
        "学词后应能只读打开 {db_dir:?}/luna_pinyin.userdb"
    );
    let raw = db.fetch("ni hao \t\u{4f60}\u{597d}").unwrap_or_else(|| {
        let mut all = db.query_all().expect("应能扫 userdb");
        let mut keys = Vec::new();
        while let Some((key, value)) = all.get_next_record() {
            keys.push((key, value));
        }
        panic!("userdb 里没有「ni hao \t你好」，实际内容：{keys:?}");
    });
    let value = librime_next::dict::user_db::UserDbValue::unpack(&raw).expect("值应可解析");
    assert_eq!(value.commits, 1, "学过一次 commits 应为 1，实际值 {raw:?}");
    assert!(value.tick >= 1, "tick 应被推进，实际值 {raw:?}");
    assert!(db.close());

    // 第二次：同一用户目录、新会话 —— 用户词典候选带上学过的记录，排第一
    let mut again = Session::create_with_roots(&build, "luna_pinyin_simp", &roots);
    type_ascii(&mut again, "nihao");
    let texts = candidate_texts(&mut again);
    assert_eq!(
        texts.first().map(String::as_str),
        Some("\u{4f60}\u{597d}"),
        "用户词典候选应排第一，实际：{:?}",
        &texts[..texts.len().min(8)]
    );
    let first_quality = again
        .context()
        .and_then(|ctx| ctx.composition.segmentation.segments.first())
        .and_then(|seg| seg.menu.as_ref())
        .and_then(|menu| menu.candidate_at(0))
        .map(|c| c.quality);
    // 用户词典候选 = `USER_DICT_QUALITY_BASE(1e5) + weight`，weight 是对数概率（负），
    // 所以落在 9e4~1e5 之间；词表词条的量级只有 1e3~1e4，一眼能分开。
    assert!(
        first_quality.is_some_and(|q| q >= 9.0e4),
        "首候选应来自用户词典（quality >= 9e4），实际 {first_quality:?}"
    );
    // 用户词条必须带着 code，否则下次学词接不上
    let has_entry = again
        .context()
        .and_then(|ctx| ctx.composition.segmentation.segments.first())
        .and_then(|seg| seg.menu.as_ref())
        .and_then(|menu| menu.candidate_at(0))
        .and_then(|c| c.entry.as_ref())
        .is_some_and(|e| !e.code.0.is_empty());
    assert!(has_entry, "用户词典候选应挂着 code 非空的词条");
}

/// 默认 `enable_user_dict` 为 true：没在方案里写配置也必须学词。
#[test]
fn user_dict_is_enabled_by_default() {
    use librime_next::dict::db::Db;
    use librime_next::dict::level_db::LevelDb;

    let user_dir = fresh_user_dir("enabled_default");
    let build = build_dir();
    let roots = [user_dir.clone(), build.clone()];
    {
        let mut session = Session::create_with_roots(&build, "luna_pinyin_simp", &roots);
        type_ascii(&mut session, "nihao");
        session.choose(0).expect("选中首候选");
        assert_eq!(session.take_commit().as_deref(), Some("\u{4f60}\u{597d}"));
    }

    let db = LevelDb::new_user_db(&user_dir.join("userdb"), "luna_pinyin");
    assert!(db.open_readonly(), "userdb 应已存在");
    let mut all = db.query_all().expect("应能扫 userdb");
    let mut count = 0;
    while all.get_next_record().is_some() {
        count += 1;
    }
    assert_eq!(count, 1, "默认 enable_user_dict=true，提交后应恰好一条记录");
    assert!(db.close());
}

/// 取当前首个组合段的候选（含质量分与类型），供形码学词断言用。
fn candidate_list(session: &mut Session) -> Vec<librime_next::engine::Candidate> {
    session
        .context()
        .and_then(|ctx| {
            ctx.composition
                .segmentation
                .segments
                .first()
                .and_then(|seg| seg.menu.as_ref())
                .map(|m| m.candidates().to_vec())
        })
        .unwrap_or_default()
}

/// 形码方案（`table_translator`）的学词闭环：
/// 打码 → 选词 → 提交 → userdb 落盘 → 新会话里该词以 `user_table` 身份出现。
///
/// 与拼音那条（`user_dict_learns_committed_phrase`）的关键差别：形码的用户词键是
/// **按键码**（`wq`），不是音节串（`ni hao`）；查询走 `LookupWords(code)` 那条
/// 前缀扫，而不是音节图上的 DFS。
#[test]
fn table_translator_wubi86_learns_committed_word() {
    use librime_next::dict::db::Db;
    use librime_next::dict::level_db::LevelDb;

    let user_dir = fresh_user_dir("wubi_learn");
    let build = build_dir();
    let roots = [user_dir.clone(), build.clone()];

    {
        let mut session = Session::create_with_roots(&build, "wubi86", &roots);
        type_ascii(&mut session, "wq");
        let candidates = candidate_list(&mut session);
        let index = candidates
            .iter()
            .position(|c| c.text == "你")
            .unwrap_or_else(|| {
                panic!(
                    "wq 的候选里应有「你」，实际：{:?}",
                    candidates
                        .iter()
                        .map(|c| (c.kind.as_str(), c.text.as_str()))
                        .take(8)
                        .collect::<Vec<_>>()
                )
            });
        let result = session.choose(index).expect("应能选中「你」");
        assert!(
            format!("{result:?}").contains("Committed"),
            "选「你」应整段提交，实际 {result:?}"
        );
        assert_eq!(session.take_commit().as_deref(), Some("你"));
    }

    // 落盘检查：形码的键是「码串 + 空格 + Tab + 词面」
    let db_dir = user_dir.join("userdb");
    let db = LevelDb::new_user_db(&db_dir, "wubi86");
    assert!(
        db.open_readonly(),
        "学词后应能只读打开 {db_dir:?}/wubi86.userdb"
    );
    let mut all = db.query_all().expect("应能扫 userdb");
    let mut keys = Vec::new();
    while let Some((key, value)) = all.get_next_record() {
        keys.push((key, value));
    }
    let (_, raw) = keys
        .iter()
        .find(|(key, _)| key.ends_with("\t你"))
        .unwrap_or_else(|| panic!("userdb 里没有学到「你」，实际内容：{keys:?}"));
    let value = librime_next::dict::user_db::UserDbValue::unpack(raw).expect("值应可解析");
    assert_eq!(value.commits, 1, "学过一次 commits 应为 1，实际值 {raw:?}");
    assert!(db.close());

    // 新会话：学过的词以 `user_table` 身份出现，且质量分落在用户词那一档。
    let mut again = Session::create_with_roots(&build, "wubi86", &roots);
    type_ascii(&mut again, "wq");
    let candidates = candidate_list(&mut again);
    let user_candidate = candidates
        .iter()
        .find(|c| c.kind == "user_table" && c.text == "你")
        .unwrap_or_else(|| {
            panic!(
                "新会话应有从 userdb 来的「你」，实际：{:?}",
                candidates
                    .iter()
                    .map(|c| (c.kind.as_str(), c.text.as_str(), c.quality))
                    .take(8)
                    .collect::<Vec<_>>()
            )
        });
    assert!(
        user_candidate.quality >= 3.9e6,
        "用户词候选的质量分应落在 USER_EXACT_BASE(4e6) 一档（减去对数权重后略低），实际 {}",
        user_candidate.quality
    );
    // 用户词必须压过同码的表词条（TABLE_EXACT_BASE = 3e6 + 表权重）
    assert!(
        candidates
            .iter()
            .filter(|c| c.text == "你" && c.kind == "table")
            .all(|c| c.quality < user_candidate.quality),
        "用户词应压过同码的表词条"
    );
    assert_eq!(
        candidates.first().map(|c| c.text.as_str()),
        Some("你"),
        "学过的「你」应是首候选，实际：{:?}",
        candidates
            .iter()
            .map(|c| (c.kind.as_str(), c.text.as_str()))
            .take(8)
            .collect::<Vec<_>>()
    );
}

// ── 反查（reverse_lookup_translator）──────────────────────────────────────

/// 反查闭环：`` `h `` → 笔画段（tag = `reverse_lookup`）→ 「一」→ 注释 `yi` → 提交。
///
/// 这条链路要同时用到两份**方向相反**的词典：`stroke`（码 → 字）出候选，
/// `luna_pinyin.reverse.bin`（字 → 码）出注释。任何一边没接上都会表现为
/// "候选出得来但注释空"或"根本没候选"。
#[test]
fn reverse_lookup_finds_word_by_stroke_code() {
    let mut session = new_session(&build_dir(), "reverse_lookup_demo");
    assert!(session.engine.is_some(), "reverse_lookup_demo 引擎应加载");

    type_ascii(&mut session, "`h");
    let candidates = candidate_list(&mut session);
    assert!(!candidates.is_empty(), "`h 应出候选");
    let yi = candidates
        .iter()
        .find(|c| c.text == "一")
        .unwrap_or_else(|| {
            panic!(
                "`h 应出「一」（stroke 里 h = 橫），实际：{:?}",
                candidates
                    .iter()
                    .map(|c| (c.kind.as_str(), c.text.as_str(), c.comment.as_str()))
                    .take(10)
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(yi.kind, "reverse_lookup", "候选类型应是 reverse_lookup");
    assert_eq!(yi.comment, "yi", "注释来自 luna_pinyin 反查表");
    assert_eq!(yi.preedit, "`一", "preedit 是完整输入经 xlit 后的结果");

    // 反查段要带 prompt（C++ 在 Query 里主动改 segment.prompt）
    let prompt = session
        .context()
        .and_then(|ctx| ctx.composition.segmentation.segments.first())
        .map(|seg| seg.prompt.clone())
        .unwrap_or_default();
    assert_eq!(prompt, "〔筆畫〕", "打反查前缀后段上应有提示");

    // 选中 → 整段提交
    let index = candidates.iter().position(|c| c.text == "一").unwrap();
    let result = session.choose(index).expect("应能选中「一」");
    assert!(
        format!("{result:?}").contains("Committed"),
        "选「一」应整段提交，实际 {result:?}"
    );
    assert_eq!(session.take_commit().as_deref(), Some("一"));
}

/// 只打前缀（还没打码）时不该出候选 —— C++ 的 `if (start < input.length())`。
#[test]
fn reverse_lookup_prefix_alone_yields_nothing() {
    let mut session = new_session(&build_dir(), "reverse_lookup_demo");
    type_ascii(&mut session, "`");
    assert!(
        candidate_list(&mut session).is_empty(),
        "只打 `` ` `` 不该有候选"
    );
    let prompt = session
        .context()
        .and_then(|ctx| ctx.composition.segmentation.segments.first())
        .map(|seg| seg.prompt.clone())
        .unwrap_or_default();
    assert_eq!(prompt, "〔筆畫〕", "但提示照设");
}

/// 没有 `reverse_lookup` 标签的普通段，反查翻译器不认领。
#[test]
fn reverse_lookup_does_not_claim_plain_segments() {
    let mut session = new_session(&build_dir(), "reverse_lookup_demo");
    type_ascii(&mut session, "h");
    let candidates = candidate_list(&mut session);
    assert!(
        candidates.iter().all(|c| c.kind != "reverse_lookup"),
        "不带反引号的段不该有 reverse_lookup 候选，实际：{:?}",
        candidates
            .iter()
            .map(|c| (c.kind.as_str(), c.text.as_str()))
            .take(6)
            .collect::<Vec<_>>()
    );
}

/// 反查过滤器：给候选补上"反查码"注释（出厂 0 引用，这里单独验证行为）。
#[test]
fn reverse_lookup_filter_fills_codes() {
    use librime_next::engine::gear::filter::Filter;
    use librime_next::engine::gear::reverse_lookup_filter::ReverseLookupFilter;
    use librime_next::engine::Candidate;
    use serde_json::json;

    let build = build_dir();
    let config = json!({ "reverse_lookup": { "dictionary": "luna_pinyin" } });
    let mut filter = ReverseLookupFilter::load(&config, "filter", &build);
    let mut list = vec![
        Candidate::new("script", 0, 3, "你".to_string(), 1.0),
        Candidate::new("script", 0, 3, "好".to_string(), 1.0).with_comment("已有的"),
    ];
    filter.apply(&librime_next::engine::Context::new(), &mut list);
    assert_eq!(list[0].comment, "ni", "没注释的候选应被补上反查码");
    assert_eq!(list[1].comment, "已有的", "默认不覆盖别人的注释");
}

// ── 提交历史（history_translator）──────────────────────────────────────────

/// 提交历史的最小闭环：提交一个词 → 打触发键 → 该词作为候选出现。
///
/// 这条链路验证的是 `Context::commit_history` 本身：`commit()` / 整段 `select()`
/// 都要把记录写进去，`history_translator` 再从里面倒着取。任何一环断了都表现为
/// "打 z 什么都不出"。
#[test]
fn history_translator_offers_recently_committed_words() {
    let mut session = new_session(&build_dir(), "history_demo");
    assert!(session.engine.is_some(), "history_demo 引擎应加载");

    // 1. 先正常打「你好」并提交，让它进历史
    type_ascii(&mut session, "nihao");
    let candidates = candidate_list(&mut session);
    let index = candidates
        .iter()
        .position(|c| c.text == "你好")
        .unwrap_or_else(|| {
            panic!(
                "nihao 应出「你好」，实际：{:?}",
                candidates
                    .iter()
                    .map(|c| c.text.as_str())
                    .take(8)
                    .collect::<Vec<_>>()
            )
        });
    session.choose(index).expect("应能选中「你好」");
    assert_eq!(session.take_commit().as_deref(), Some("你好"));

    // 2. 打触发键 z，历史候选应出现
    type_ascii(&mut session, "z");
    let candidates = candidate_list(&mut session);
    let learned = candidates
        .iter()
        .find(|c| c.text == "你好")
        .unwrap_or_else(|| {
            panic!(
                "打 z 应出刚提交的「你好」，实际：{:?}",
                candidates
                    .iter()
                    .map(|c| (c.kind.as_str(), c.text.as_str(), c.quality))
                    .take(12)
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(learned.kind, "script", "候选类型沿用记录里的 kind");
    assert_eq!(learned.quality, 1.0, "quality 来自 history/initial_quality");
}

/// 触发键是**精确匹配**：打 `zh`（多一个字母）就不该再出历史。
#[test]
fn history_translator_requires_exact_trigger() {
    let mut session = new_session(&build_dir(), "history_demo");
    type_ascii(&mut session, "nihao");
    let candidates = candidate_list(&mut session);
    let index = candidates.iter().position(|c| c.text == "你好").unwrap();
    session.choose(index).expect("应能选中");
    session.take_commit();

    type_ascii(&mut session, "zh");
    let candidates = candidate_list(&mut session);
    assert!(
        !candidates.iter().any(|c| c.text == "你好"),
        "打 zh 不该出历史，实际：{:?}",
        candidates
            .iter()
            .map(|c| (c.kind.as_str(), c.text.as_str()))
            .take(10)
            .collect::<Vec<_>>()
    );
}

/// `history/size` 是上限，且**最新的排最前**。
#[test]
fn history_translator_respects_size_and_order() {
    let mut session = new_session(&build_dir(), "history_demo");
    // 依次提交四个单字，每个都成为一条历史记录
    for (code, word) in [("ni", "你"), ("hao", "好"), ("shi", "是"), ("wo", "我")] {
        type_ascii(&mut session, code);
        let candidates = candidate_list(&mut session);
        let index = candidates
            .iter()
            .position(|c| c.text == word)
            .unwrap_or_else(|| {
                panic!(
                    "{code} 应出「{word}」，实际：{:?}",
                    candidates.iter().map(|c| c.text.as_str()).take(8).collect::<Vec<_>>()
                )
            });
        session.choose(index).expect("应能选中");
        assert_eq!(session.take_commit().as_deref(), Some(word));
    }

    type_ascii(&mut session, "z");
    let candidates = candidate_list(&mut session);
    // 历史候选的 quality 都是 1（配置值），据此把它们挑出来
    let history: Vec<&str> = candidates
        .iter()
        .filter(|c| c.quality == 1.0)
        .map(|c| c.text.as_str())
        .collect();
    assert_eq!(history, vec!["我", "是", "好"], "size=3 只留最近三条，且新的在前");
}

// ── 用户造词（affix_segmentor）────────────────────────────────────────────

/// 取**最后一段**的候选。用户造词方案里第一段是 `phony` 前缀段（没有菜单），
/// 真正装着候选的是后面那段。
fn candidate_list_of_last_segment(session: &mut Session) -> Vec<librime_next::engine::Candidate> {
    session
        .context()
        .and_then(|ctx| {
            ctx.composition
                .segmentation
                .segments
                .last()
                .and_then(|seg| seg.menu.as_ref())
                .map(|m| m.candidates().to_vec())
        })
        .unwrap_or_default()
}

/// 段列表的 `(start, end, tags)` 摘要，便于断言劈段结果。
fn segment_snapshot(session: &mut Session) -> Vec<(usize, usize, Vec<String>)> {
    session
        .context()
        .map(|ctx| {
            ctx.composition
                .segmentation
                .segments
                .iter()
                .map(|s| (s.start, s.end, s.tags.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// 打反引号后，`affix_segmentor` 要把这一段劈成「前缀段 + 码段」。
#[test]
fn affix_segmentor_splits_prefix_from_code() {
    let mut session = new_session(&build_dir(), "affix_demo");
    assert!(session.engine.is_some(), "affix_demo 引擎应加载");

    type_ascii(&mut session, "`ni");
    let snap = segment_snapshot(&mut session);
    assert_eq!(snap.len(), 2, "应劈成两段，实际：{snap:?}");
    assert_eq!((snap[0].0, snap[0].1), (0, 1), "前缀段是反引号");
    assert!(
        snap[0].2.iter().any(|t| t == "zaoci_prefix"),
        "前缀段要打 zaoci_prefix，实际 {:?}",
        snap[0].2
    );
    assert!(
        snap[0].2.iter().any(|t| t == "phony"),
        "前缀段必须 phony，否则反引号会被一起提交，实际 {:?}",
        snap[0].2
    );
    assert_eq!((snap[1].0, snap[1].1), (1, 3), "码段是 ni");
    assert!(
        snap[1].2.iter().any(|t| t == "zaoci"),
        "码段要打 zaoci 标签，实际 {:?}",
        snap[1].2
    );

    // 前缀段的 prompt 应显示提示文字
    let prompt = session
        .context()
        .and_then(|ctx| ctx.composition.segmentation.segments.first())
        .map(|s| s.prompt.clone())
        .unwrap_or_default();
    assert_eq!(prompt, "〔用户造詞〕");
}

/// `phony` 的实际作用：提交时**不含**反引号。
#[test]
fn affix_prefix_is_not_committed() {
    let mut session = new_session(&build_dir(), "affix_demo");
    type_ascii(&mut session, "`ni");
    let candidates = candidate_list_of_last_segment(&mut session);
    let index = candidates
        .iter()
        .position(|c| c.text == "你")
        .unwrap_or_else(|| {
            panic!(
                "`ni 的码段应出「你」，实际：{:?}",
                candidates
                    .iter()
                    .map(|c| (c.kind.as_str(), c.text.as_str()))
                    .take(8)
                    .collect::<Vec<_>>()
            )
        });
    let result = session.choose(index).expect("应能选中「你」");
    assert!(
        format!("{result:?}").contains("Committed"),
        "应整段提交，实际 {result:?}"
    );
    assert_eq!(
        session.take_commit().as_deref(),
        Some("你"),
        "phony 前缀不能进提交文本"
    );
}

/// 只打前缀（还没打码）时不分段，只改当前段的标签与提示。
#[test]
fn affix_prefix_alone_retags_without_splitting() {
    let mut session = new_session(&build_dir(), "affix_demo");
    type_ascii(&mut session, "`");
    let snap = segment_snapshot(&mut session);
    assert_eq!(snap.len(), 1, "只打前缀不劈段，实际：{snap:?}");
    assert!(
        snap[0].2.iter().any(|t| t == "zaoci_prefix"),
        "当前段改成 zaoci_prefix，实际 {:?}",
        snap[0].2
    );
    assert!(
        !snap[0].2.iter().any(|t| t == "zaoci"),
        "原来的 zaoci 标签要去掉，实际 {:?}",
        snap[0].2
    );
}

// ── 形码整句（`table_translator::MakeSentence`）───────────────────────────
//
// C++ 只在**词级候选全空**时才组句（`table_translator.cc:296`）。而
// `enable_completion` 默认 true —— 只要整串码是某个词条码的前缀，就会有补全候选，
// 轮不到组句。所以验证组句必须让整串码查不到任何词：用 `speller/delimiter`
// 里的 `;` 把两段独立的码隔开。
//
//   `wq` = 你、`vb` = 好（五笔）；`wq;vb` 整串不是任何词条的码，
//   但词图上有 0→3(你) 与 3→5(好) 两条边，最短路就是「你好」。

#[test]
fn table_translator_makes_sentence_across_delimiters() {
    let mut session = new_session(&build_dir(), "wubi86");
    type_ascii(&mut session, "wq;vb");

    let candidates = candidate_list_of_last_segment(&mut session);
    let first = candidates.first().expect("应至少有一条候选");
    assert_eq!(
        first.kind, "sentence",
        "整句候选的 kind 应为 sentence，实际：{:?}",
        candidates
            .iter()
            .map(|c| (&c.kind, &c.text))
            .collect::<Vec<_>>()
    );
    assert_eq!(first.text, "你好", "整句应为「你好」");
    assert_eq!(
        first.comment,
        " \u{262f} ",
        "整句候选的注释是 C++ 的 kUnitySymbol"
    );
    assert_eq!(
        first.preedit, "wq;vb",
        "边界前一个字符是分隔符时不插空格"
    );
    assert_eq!((first.start, first.end), (0, 5), "整句覆盖整段输入");
}

#[test]
fn table_translator_sentence_can_span_three_codes() {
    let mut session = new_session(&build_dir(), "wubi86");
    type_ascii(&mut session, "wq;vb;wq");
    let texts = candidate_texts(&mut session);
    assert!(
        texts.iter().any(|t| t == "你好你"),
        "三段码应组出「你好你」，实际：{:?}",
        &texts[..texts.len().min(8)]
    );
}

#[test]
fn table_translator_sentence_commits_as_a_whole() {
    let mut session = new_session(&build_dir(), "wubi86");
    type_ascii(&mut session, "wq;vb");
    let idx = candidate_texts(&mut session)
        .iter()
        .position(|t| t == "你好")
        .expect("候选里应有整句「你好」");
    session.choose(idx);
    assert_eq!(
        session.take_commit().as_deref(),
        Some("你好"),
        "整句应一次提交"
    );
}

// ── 字符集过滤（`enable_charset_filter`）─────────────────────────────────
//
// 出厂 `wubi86` / `cangjie5` / `cangjie5_express` 都开了这个开关：候选文本里只要有
// 一个字符落在 CJK 扩展区 / 兼容汉字区，整条候选就丢掉。不开的话 `wqvbwq` 会冒出
// 「𠛈」（U+20718，扩展 B 区）—— 字库里有，但用户一辈子不会去打。
//
// 用户可以在菜单里打开 `extended_charset`（"显示生僻字"），开关一开过滤器整个失效。

#[test]
fn wubi86_charset_filter_drops_extended_cjk() {
    let mut session = new_session(&build_dir(), "wubi86");
    type_ascii(&mut session, "wqvbwq");
    let texts = candidate_texts(&mut session);
    assert!(
        texts.iter().any(|t| t == "您好"),
        "常用词候选该在，实际：{:?}",
        &texts[..texts.len().min(8)]
    );
    assert!(
        !texts
            .iter()
            .any(|t| t.chars().any(is_extended_cjk)),
        "扩展区汉字应被过滤掉，实际：{:?}",
        &texts[..texts.len().min(8)]
    );
}

#[test]
fn extended_charset_option_disables_the_filter() {
    let mut session = new_session(&build_dir(), "wubi86");
    session
        .context_mut()
        .expect("composing")
        .set_option("extended_charset", true);
    type_ascii(&mut session, "wqvbwq");
    let texts = candidate_texts(&mut session);
    assert!(
        texts.iter().any(|t| t.chars().any(is_extended_cjk)),
        "开了 extended_charset 就该看见扩展区汉字（𠛈），实际：{:?}",
        &texts[..texts.len().min(8)]
    );
}

#[test]
fn table_translator_word_match_suppresses_sentence() {
    // 词级候选非空时绝不组句：单字 `wq` 有精确命中「你」。
    let mut session = new_session(&build_dir(), "wubi86");
    type_ascii(&mut session, "wq");
    let candidates = candidate_list_of_last_segment(&mut session);
    assert!(
        candidates.iter().all(|c| c.kind != "sentence"),
        "有词级候选时不该出整句：{:?}",
        candidates
            .iter()
            .map(|c| (&c.kind, &c.text))
            .collect::<Vec<_>>()
    );
}

// ── 兜底翻译器（`echo_translator`）───────────────────────────────────────
//
// 它永远能产出一条 `raw` 候选，但**只在同段正经翻译器交白卷时**才露头。
// 两个方向的输入缺一不可，只测一边会把"候选总是出现"这种错误实现当成通过。
//
// 挑输入有讲究：`qqq` 看着象乱码，其实 luna_pinyin 的单字母简拼把它切成了
// 「七七」（q + q + q）—— 有正经候选。真正切不开的是 `vvv`：拼写表里没有
// `v` 这个音节（`v` 在拼音里只作 ü 的输入替身，不单独成音节）。

#[test]
fn echo_translator_rescues_an_untranslatable_segment() {
    let mut session = new_session(&build_dir(), "echo_demo");
    type_ascii(&mut session, "vvv");

    let candidates = candidate_list_of_last_segment(&mut session);
    assert_eq!(
        candidates.len(),
        1,
        "`vvv` 切不出音节，菜单里只该有兜底那一条，实际：{:?}",
        candidates.iter().map(|c| (&c.kind, &c.text)).collect::<Vec<_>>()
    );
    let only = &candidates[0];
    assert_eq!(only.kind, "raw");
    assert_eq!(only.text, "vvv", "原样回显输入");
    assert_eq!((only.start, only.end), (0, 3), "覆盖整段输入");
}

#[test]
fn echo_candidate_yields_to_real_translations() {
    let mut session = new_session(&build_dir(), "echo_demo");
    type_ascii(&mut session, "nihao");

    let candidates = candidate_list_of_last_segment(&mut session);
    assert!(
        candidates.iter().any(|c| c.text == "你好"),
        "正常翻译应生效，实际：{:?}",
        candidates.iter().map(|c| (&c.kind, &c.text)).collect::<Vec<_>>()
    );
    assert!(
        candidates.iter().all(|c| c.kind != "raw"),
        "有正经候选时兜底候选必须让位，实际：{:?}",
        candidates.iter().map(|c| (&c.kind, &c.text)).collect::<Vec<_>>()
    );
}

#[test]
fn echo_fallback_can_be_committed_as_is() {
    let mut session = new_session(&build_dir(), "echo_demo");
    type_ascii(&mut session, "vvv");
    session.choose(0);
    assert_eq!(
        session.take_commit().as_deref(),
        Some("vvv"),
        "兜底候选存在的意义就是让这段能原样上屏"
    );
}

// ── 方案 / 开关菜单（`Switcher`）─────────────────────────────────────────
//
// `F4`（X11 keysym 0xffc1）唤起。菜单其实就是一个**空段**（`[0, 0)`）+ 菜单：
// 候选全是 0..0 的"动作条目"，选中后执行、菜单收起。
//
// 顺序照抄 C++：方案在前（当前方案第一），开关在后。这个顺序不是排出来的，
// 是建菜单时按"先 schema_list_translator、后 switch_translator"拼的 —— 所以
// `Menu::prepare()` 特意没调。

const XK_F4: u32 = 0xffc1;

/// 建会话并保留用户目录路径（菜单会往 `user.yaml` 写东西，要验）。
fn new_session_tracking_user(build: &std::path::Path, schema: &str) -> (Session, PathBuf) {
    let user_dir = fresh_user_dir(schema);
    let session = Session::create_with_roots(build, schema, &[user_dir.clone(), build.to_path_buf()]);
    (session, user_dir)
}

/// 从第 0 条往下走 `steps` 步（菜单没有环绕，用 clamp）。
fn walk_down(session: &mut Session, steps: usize) {
    for _ in 0..steps {
        session.process_key(0xff54, 0); // XK_Down
    }
}

#[test]
fn f4_opens_the_switcher_menu() {
    let mut session = new_session(&build_dir(), "luna_pinyin_simp");
    assert!(session.process_key(XK_F4, 0), "F4 应被消费");

    {
        let ctx = session.context().expect("引擎可读");
        assert!(ctx.is_composing(), "菜单算「组合中」，宿主才会画候选窗");
        assert_eq!(ctx.composition.segmentation.segments.len(), 1);
        let seg = &ctx.composition.segmentation.segments[0];
        assert_eq!(seg.prompt, "〔方案選單〕", "提示语来自 switcher/caption");
        assert_eq!((seg.start, seg.end), (0, 0), "空段：菜单不覆盖任何输入");
        assert_eq!(ctx.input, "", "菜单展开时输入串是空的");
    }

    let texts = candidate_texts(&mut session);
    assert_eq!(
        &texts[..3],
        &[
            "朙月拼音·简化字",
            "五笔86 (e2e 验证)",
            "五筆畫 (e2e 验证)"
        ],
        "方案在前且当前方案第一，实际：{texts:?}"
    );
    assert!(texts.contains(&"中文".to_string()), "应有 ascii_mode 开关");
    assert!(texts.contains(&"半角".to_string()), "应有 full_shape 开关");
    assert!(
        texts.iter().any(|t| t == "傳統漢字"),
        "单选组应铺开：{texts:?}"
    );
}

#[test]
fn escape_closes_the_switcher() {
    let mut session = new_session(&build_dir(), "luna_pinyin_simp");
    session.process_key(XK_F4, 0);
    assert!(session.process_key(0xff1b, 0), "Escape 应被消费");

    let ctx = session.context().expect("引擎可读");
    assert!(!ctx.is_composing(), "退出后不该还在组合");
    assert!(ctx.composition.segmentation.segments.is_empty());
    assert!(
        !ctx.get_option("dumb"),
        "dumb 要清掉，否则后续输入永远上不了屏"
    );
}

#[test]
fn pressing_the_hotkey_again_jumps_to_the_next_schema() {
    let mut session = new_session(&build_dir(), "luna_pinyin_simp");
    session.process_key(XK_F4, 0);
    session.process_key(XK_F4, 0); // 再按一次 → 高亮下一个方案

    let ctx = session.context().expect("引擎可读");
    assert_eq!(
        ctx.composition.segmentation.segments[0].selected_index,
        1,
        "应跳到第二个方案候选"
    );
    // 菜单还在（只是高亮变了）
    assert!(ctx.is_composing());
}

#[test]
fn switcher_toggles_a_switch_and_persists_it() {
    let (mut session, user_dir) = new_session_tracking_user(&build_dir(), "luna_pinyin_simp");
    session.process_key(XK_F4, 0);

    let texts = candidate_texts(&mut session);
    let index = texts
        .iter()
        .position(|t| t == "半角")
        .expect("菜单里应有 full_shape");
    walk_down(&mut session, index);
    assert!(session.process_key(0xff0d, 0), "回车应被消费");

    let ctx = session.context().expect("引擎可读");
    assert!(ctx.get_option("full_shape"), "full_shape 应被打开");
    assert!(!ctx.is_composing(), "选中后菜单收起");

    // full_shape 在 save_options 里 ⇒ 必须落盘到 user.yaml
    let saved = std::fs::read_to_string(user_dir.join("user.yaml")).unwrap_or_default();
    assert!(
        saved.contains("full_shape"),
        "开关状态应写进 user.yaml，实际内容：{saved:?}"
    );
}

#[test]
fn switcher_selects_a_radio_option() {
    let mut session = new_session(&build_dir(), "luna_pinyin_simp");
    session.process_key(XK_F4, 0);

    let texts = candidate_texts(&mut session);
    let index = texts
        .iter()
        .position(|t| t == "臺灣字形")
        .expect("单选组应铺出臺灣字形");
    walk_down(&mut session, index);
    session.process_key(0xff0d, 0);

    let ctx = session.context().expect("引擎可读");
    assert!(ctx.get_option("zh_hant_tw"), "应选中臺灣字形");
    assert!(!ctx.get_option("zh_hans"), "单选组里其余必须关掉");
}

#[test]
fn switcher_switches_the_schema() {
    let mut session = new_session(&build_dir(), "luna_pinyin_simp");
    session.process_key(XK_F4, 0);

    let texts = candidate_texts(&mut session);
    let index = texts
        .iter()
        .position(|t| t.starts_with("五笔86"))
        .expect("菜单里应有五笔");
    walk_down(&mut session, index);
    assert!(session.process_key(0xff0d, 0));

    assert_eq!(
        session.schema_id(),
        Some("wubi86"),
        "应切到五笔（引擎整个重建）"
    );
    // 切完还能正常打字 —— 这一步能过说明组件是按新方案重新装配过的
    assert!(session.process_key(b'w' as u32, 0));
    let texts = candidate_texts(&mut session);
    assert!(!texts.is_empty(), "切完方案应能出候选，实际 {texts:?}");
}

#[test]
fn switcher_remembers_the_schema_for_next_time() {
    let (mut session, user_dir) = new_session_tracking_user(&build_dir(), "luna_pinyin_simp");
    session.process_key(XK_F4, 0);
    let texts = candidate_texts(&mut session);
    let index = texts
        .iter()
        .position(|t| t.starts_with("五笔86"))
        .expect("菜单里应有五笔");
    walk_down(&mut session, index);
    session.process_key(0xff0d, 0);

    let saved = std::fs::read_to_string(user_dir.join("user.yaml")).unwrap_or_default();
    assert!(
        saved.contains("previously_selected_schema") && saved.contains("wubi86"),
        "应记下上次选的方案（菜单排序靠它），实际：{saved:?}"
    );
    assert!(
        saved.contains("schema_access_time"),
        "访问时间也要记，否则菜单按 recency 排不出来",
    );
}


#[test]
fn next_schema_id_walks_the_schema_list() {
    // `key_binder` 的 `select: .next` 靠它（C++ 临时建 `Switcher` 调 `SelectNextSchema`）
    let session = new_session(&build_dir(), "luna_pinyin_simp");
    let engine = session.engine.as_ref().expect("引擎可读");
    assert_eq!(
        engine.next_schema_id().as_deref(),
        Some("wubi86"),
        "清单是 luna → wubi86 → stroke，下一个应是五笔"
    );

    let session = new_session(&build_dir(), "wubi86");
    let engine = session.engine.as_ref().expect("引擎可读");
    // ⚠️ 反直觉：`.next` 不是"清单里的下一个"，而是"菜单第二条"。而菜单里
    // **当前方案永远排第一**（`LoadSchemaList` 先 Append 当前，再遍历其余），
    // 所以第二条永远是"清单里第一个不是当前方案的"。从 wubi86 出发是 luna，
    // 不是 stroke —— 这就是那条规则的体现。
    assert_eq!(
        engine.next_schema_id().as_deref(),
        Some("luna_pinyin_simp"),
        "当前方案占第一条，取的是「除当前外的第一条」"
    );
}

#[test]
fn default_schema_id_is_resolved_from_the_schema_list() {
    // C++ `engine.cc:570-574`：`schema_id == ".default"` 时由 `switcher_->CreateSchema()`
    // 挑一个。宿主没指定方案时走这条路。
    let session = new_session(&build_dir(), ".default");
    assert_eq!(
        session.schema_id(),
        Some("luna_pinyin_simp"),
        "没有历史记录时取清单第一条"
    );
}

#[test]
fn default_schema_prefers_the_previously_selected_one() {
    let user_dir = fresh_user_dir("defaultpick");
    std::fs::write(
        user_dir.join("user.yaml"),
        // 用 raw 多行字符串：这里的换行必须是真换行，不能靠转义序列
        r#"var:
  previously_selected_schema: stroke
"#,
    )
    .unwrap();
    let session = Session::create_with_roots(
        &build_dir(),
        ".default",
        &[user_dir, build_dir()],
    );
    assert_eq!(
        session.schema_id(),
        Some("stroke"),
        "上次用的是笔画，就该接着用"
    );
}


// ── 并击输入（`chord_composer` / `streaming_chord_processor`）─────────────
//
// 两种实现的分工见各自模块文档，这里只钉三条外部可见的行为：
//   1. 并击中时输入串是空的，但**组合态成立**，提示挂在末段 prompt 上；
//   2. 松手后编码串被转发给 speller，候选照常出；
//   3. 流式版靠时间切音节 —— 隔太久就插隔音符。

/// `K_RELEASE_MASK`：抬键事件（宿主送 KeyUp 时带这个位）。
use librime_next::engine::key_event::K_RELEASE_MASK;

/// 按下 + 抬起（没有抬键的平台上只会有前一半，这里两半都送）。
fn press_and_release(session: &mut Session, ch: char) {
    assert!(session.process_key(ch as u32, 0), "按下 {ch} 应被消费");
    assert!(
        session.process_key(ch as u32, K_RELEASE_MASK),
        "抬起 {ch} 应被消费"
    );
}

#[test]
fn chord_composer_shows_the_chord_as_a_prompt_while_holding() {
    let mut session = new_session(&build_dir(), "chord_demo");
    // 两个键都按住不放
    assert!(session.process_key(b'n' as u32, 0));
    assert!(session.process_key(b'i' as u32, 0));

    let ctx = session.context().expect("引擎可读");
    assert!(ctx.is_composing(), "并击中必须算组合态，否则宿主不画候选窗");
    assert_eq!(ctx.input, "", "松手之前一个字符都还没进输入串");
    assert_eq!(
        ctx.composition.segmentation.segments.len(),
        1,
        "靠一个 phony 段撑着组合态"
    );
    let seg = &ctx.composition.segmentation.segments[0];
    assert!(seg.has_tag("phony"), "承载提示的是临时垫的虚拟段");
    assert!(seg.has_tag("chord_prompt"));
    assert_eq!(
        seg.prompt, "ni",
        "和弦按 alphabet 顺序拼成 in，algebra 换成 ni 后当提示"
    );
    assert!(ctx.get_option("_chord_typing"), "要告诉宿主本方案用并击");
}

#[test]
fn chord_composer_releases_into_the_regular_translator() {
    let mut session = new_session(&build_dir(), "chord_demo");
    press_and_release(&mut session, 'n');
    press_and_release(&mut session, 'i');

    {
        let ctx = session.context().expect("引擎可读");
        assert_eq!(ctx.input, "ni", "编码串应被转发进输入串");
        assert_eq!(
            ctx.composition.get_prompt(),
            "",
            "听完和弦就该把提示收掉"
        );
    }
    let texts = candidate_texts(&mut session);
    // 和弦只出一个音节 `ni`，所以菜单里是单字候选而不是「你好」那条双音节词
    assert_eq!(
        texts.first().map(String::as_str),
        Some("你"),
        "编码串进了 speller 就该照常出词，实际：{:?}",
        &texts[..texts.len().min(8)]
    );
}

#[test]
fn chord_composer_commits_the_raw_sequence_on_return() {
    // bindings 里 `Return: commit_raw_input` —— 并击期间攒的**原始**输入原样上屏。
    //
    // 这个用例必须是"原始输入 ≠ 输入串"才有效：方案里 `derive/^z$/nihao/`
    // 让一个 `z` 展开成 "nihao"，于是
    //   raw_sequence = "z"（用户敲的），input = "nihao"（转发出来的）。
    // 回车若走上屏原始串会得到 "z"，走普通提交会得到 "nihao" —— 两者可区分。
    let mut session = new_session(&build_dir(), "chord_demo");
    press_and_release(&mut session, 'z');

    {
        let ctx = session.context().expect("引擎可读");
        assert_eq!(ctx.input, "nihao", "和弦编码应被逐键转发进输入串");
    }
    let texts = candidate_texts(&mut session);
    assert_eq!(
        texts.first().map(String::as_str),
        Some("你好"),
        "多键转发的顺序必须保住（否则会得到 oahin 这种乱序），实际：{:?}",
        &texts[..texts.len().min(8)]
    );

    assert!(session.process_key(0xff0d, 0), "回车应被组件吃掉");
    assert_eq!(
        session.take_commit().as_deref(),
        Some("z"),
        "上屏的是攒下来的原始输入，不是转发后的输入串"
    );
}

#[test]
fn streaming_chord_joins_keys_that_land_together() {
    let mut session = new_session(&build_dir(), "stream_demo");
    assert!(session.process_key(b'n' as u32, 0));
    assert!(session.process_key(b'i' as u32, 0));

    let ctx = session.context().expect("引擎可读");
    assert_eq!(
        ctx.input, "ni",
        "两个键落在容差窗口内 ⇒ 同一个和弦，中间不插隔音符"
    );

    assert!(session.process_key(b'n' as u32, K_RELEASE_MASK));
    assert!(session.process_key(b'i' as u32, K_RELEASE_MASK));
    let texts = candidate_texts(&mut session);
    assert_eq!(
        texts.first().map(String::as_str),
        Some("你"),
        "流式版也走同一条翻译链，实际：{:?}",
        &texts[..texts.len().min(8)]
    );
}

#[test]
fn streaming_chord_breaks_syllables_on_timeout() {
    let mut session = new_session(&build_dir(), "stream_demo");
    assert!(session.process_key(b'n' as u32, 0));
    // 方案里 `chord_timeout_ms: 60`，睡 150ms 必然越过超时边界
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(session.process_key(b'i' as u32, 0));

    let ctx = session.context().expect("引擎可读");
    assert_eq!(
        ctx.input, "n'i",
        "停顿超过超时阈值 ⇒ 认定前一音节结束，插 speller 的隔音符"
    );
}

// ---------------------------------------------------------------------------
// table_translator（形码）端到端：wubi86
//
// 依赖真实源布局 `C:\code\Weasel-Next\Rime-Next`（仅本机有）：
// 沙箱部署 wubi86 → 在 staging 产物上建会话 → 打 `aaaa` → 应出「工」并可提交。
// ---------------------------------------------------------------------------

#[test]
fn wubi86_table_translator_end_to_end() {
    let shared = PathBuf::from(r"C:\code\Weasel-Next\Rime-Next");
    if !shared.is_dir() {
        eprintln!("跳过：真实源布局不存在（{}）", shared.display());
        return;
    }
    use librime_next::lever::deployer::Deployer;
    use librime_next::lever::deployment_tasks::DeploymentTask;

    let work = std::env::temp_dir().join(format!("wubi-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let sandbox_user = work.join("user");
    let staging = work.join("build");
    std::fs::create_dir_all(&sandbox_user).unwrap();

    let mut deployer = Deployer::with_data_dirs(&shared, &sandbox_user);
    deployer.staging_dir = staging.clone();
    let ok = DeploymentTask::SchemaUpdate {
        source_path: shared.join("schema").join("wubi86").join("wubi86.schema.yaml"),
        verbose: false,
    }
    .run(&mut deployer);
    assert!(ok, "wubi86 SchemaUpdate 应成功");

    let mut session = Session::create(&staging, "wubi86");
    assert!(session.engine.is_some(), "wubi86 引擎应能加载（table_translator 链）");

    for ch in "aaaa".chars() {
        assert!(session.process_key(ch as u32, 0), "wubi 键 {ch} 应被消费");
    }
    let ctx = session.context().expect("应有上下文");
    let texts: Vec<String> = ctx
        .composition
        .segmentation
        .segments
        .iter()
        .filter_map(|s| s.menu.as_ref())
        .flat_map(|m| m.candidates().iter().map(|c| c.text.clone()).collect::<Vec<_>>())
        .collect();
    assert!(
        texts.iter().any(|t| t == "工"),
        "aaaa 应出「工」，实际前若干个：{:?}",
        &texts[..texts.len().min(10)]
    );

    // 选首候选（质量最高，即「工」）→ express_editor 的 _auto_commit 直接上屏
    let _ = session.choose(0);
    assert_eq!(
        session.take_commit().as_deref(),
        Some("工"),
        "选词后应提交「工」"
    );
    let _ = std::fs::remove_dir_all(&work);
}
