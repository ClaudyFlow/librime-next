//! 方案切换耗时探针：量 `apply_schema` 各步骤，定位「切方案按键 > TIP 180ms 预算」的慢点。
//!
//! 背景（2026-11-08 19:0x 用户实测）：`Ctrl+`` 打开方案选单后按数字/空格切换方案，
//! 该键在引擎里可能耗时 250-330ms，超过 TIP 的 RPC 预算（180ms）⇒ 宿主拿不到响应
//! ⇒ 键透传应用 ⇒ 数字/空格上屏。
//!
//! 本探针把一次「切方案」拆成：
//! 1. `Schema::open(build_dir, id)` —— 只解析编译后的 `build/**/<id>.schema.yaml`
//! 2. `Engine::create_with_roots(...)` —— 新引擎完整装配（= 1 + initialize_components）
//! 3. `Engine::apply_schema(id)` —— 真实切换路径（= 2 的组件部分 + record_active_schema）
//!
//! 用法（用户目录必须用副本，避免改到现网 user.yaml）：
//! ```text
//! cargo run --release --offline --example probe_schema_switch -- \
//!     <用户目录副本> <共享数据目录> [目标方案...]
//! ```
//! 目标方案缺省为 16 个 pinyin 子方案 + 几个顶层方案。
//! 末尾先回到父方案 `pinyin`，再打开两级菜单、按 Tab 进「拼音 ›」子菜单、
//! 按数字 1 选第一项（明月），打印切换后的 schema_id。
//! 2026-11-12 起第二级不再显示父方案自身，只列 16 子方案 + 返回上一级。
//! 2026-11-11 曾因 MergedTranslation 按 quality 重排，导致显示项与 confirm 下标
//! 错位（显示「汉心龙, 拼音, 明月…」，按 2 却切到 pinyin.hxlong）；这里一并核对。

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use librime_next::engine::key_event::{KeyEvent, XK_NEXT};
use librime_next::engine::runtime::Engine;
use librime_next::gear::schema::Schema;

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn menu_names(eng: &Engine) -> Vec<String> {
    eng.context
        .composition
        .segmentation
        .segments
        .last()
        .and_then(|s| s.menu.as_ref())
        .map(|m| m.all_candidates().iter().map(|c| c.text.clone()).collect())
        .unwrap_or_default()
}

fn main() {
    let user = PathBuf::from(std::env::args().nth(1).expect("user_root"));
    let shared = PathBuf::from(std::env::args().nth(2).expect("shared_root"));
    let mut targets: Vec<String> = std::env::args().skip(3).collect();
    if targets.is_empty() {
        targets = [
            "pinyin.luna",
            "pinyin.terra",
            "pinyin.zhuyin",
            "pinyin.zrm",
            "pinyin.flypy",
            "pinyin.mspy",
            "pinyin.sogou",
            "pinyin.znabc",
            "pinyin.ziguang",
            "pinyin.pyjj",
            "pinyin.gbpy",
            "pinyin.lxsq",
            "pinyin.ltsp",
            "pinyin.wxsp",
            "pinyin.zrlong",
            "pinyin.hxlong",
            "cangjie.5",
            "tiger_sentence",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
    }

    let roots = vec![user.clone(), shared.clone()];
    let build = user.join("build");
    println!("user = {}", user.display());
    println!("shared = {}", shared.display());
    println!("build = {}", build.display());

    let Some(mut eng) = Engine::create_with_roots(&build, "pinyin", &roots) else {
        eprintln!("装配失败: {} / pinyin", build.display());
        return;
    };
    // 打一个字再清掉，让会话处于"刚打过字"的真实状态。
    eng.process_key(&KeyEvent::new('n' as u32, 0));

    // ① 仅解析 schema yaml
    for target in &targets {
        let start = Instant::now();
        let ok = Schema::open(&build, target).is_some();
        println!("Schema::open({target:28}) = {ok}  {:8.1} ms", ms(start));
    }

    // ② 全新引擎完整装配
    for target in &targets {
        let start = Instant::now();
        let created = Engine::create_with_roots(&build, target, &roots).is_some();
        println!("Engine::create({target:25}) = {created}  {:8.1} ms", ms(start));
    }

    // ③ 真实切换路径：apply_schema（两轮，观察冷/热差异）
    for round in 1..=2 {
        for target in &targets {
            let start = Instant::now();
            let ok = eng.apply_schema(target);
            println!("[round {round}] apply_schema({target:25}) = {ok}  {:8.1} ms", ms(start));
        }
        // 两轮之间先回到起点方案，避免受上一轮目标影响
        if round == 1 {
            let start = Instant::now();
            eng.apply_schema("pinyin");
            println!("[round {round}] 回到 pinyin                         {:8.1} ms", ms(start));
        }
    }

    // ④ 两级选单真实交互：先回到父方案，再打开 → Tab 进「拼音 ›」→ 数字 1 选第 1 项。
    //    第二级现在只列子方案，首项就是明月（fix_schema_list_order 时）；
    //    不写死具体是哪个子方案：真正的判据是「显示第 N 项 == 数字 N 切到的方案」。
    let start = Instant::now();
    eng.apply_schema("pinyin");
    println!("回到父方案 pinyin                        {:8.1} ms", ms(start));

    let start = Instant::now();
    let eaten = eng.process_key(&KeyEvent::new(0x60, 1 << 2));
    println!("Ctrl+` 打开选单                          eaten={eaten}  {:8.1} ms", ms(start));
    let level1 = menu_names(&eng);
    println!("第一级候选: {level1:?}");

    let start = Instant::now();
    let eaten = eng.process_key(&KeyEvent::new(0xff09, 0)); // XK_TAB
    println!("Tab 进入子菜单                           eaten={eaten}  {:8.1} ms", ms(start));
    let level2 = menu_names(&eng);
    println!("第二级候选: {level2:?}");

    // 权威名称→id：只有 pinyin.<leaf> 属于第二级；其余顶层方案在第一级核对。
    let mut name_to_id: HashMap<String, String> = HashMap::new();
    let mut mismatch = 0;
    for target in &targets {
        let authoritative = Schema::open(&build, target)
            .map(|s| s.schema_name)
            .unwrap_or_else(|| target.clone());
        let (pool, level) = if target.starts_with("pinyin.") {
            (&level2, "第二级")
        } else {
            (&level1, "第一级")
        };
        if pool.iter().any(|name| name == &authoritative) {
            println!("  名称核对 OK[{level}]: {target} -> {authoritative}");
        } else {
            mismatch += 1;
            println!("  ⚠️ 名称核对失败[{level}]: {target} 权威={authoritative} {level}里没有");
        }
        if target.starts_with("pinyin.") {
            name_to_id.insert(authoritative, target.clone());
        }
    }
    println!("名称核对不一致数: {mismatch}");
    assert_eq!(mismatch, 0, "有方案没出现在对应层级的菜单里");

    let first_name = level2.first().map(String::as_str).unwrap_or("<无>");
    let expected_id = name_to_id.get(first_name).cloned().unwrap_or_default();
    println!("第二级第 1 项: {first_name:?} -> 期望切换 schema={expected_id}");
    assert!(!expected_id.is_empty(), "第二级第 1 项应是可切换子方案");

    // ⑤ 逐项验证第二级 16 个子方案：显示第 i 项，就用对应数字键切到它。
    //    page_size=9：首页用 1..9，第二页先 Page_Down 再用 1..7。
    assert_eq!(
        level2.len(),
        17,
        "第二级应为 16 子方案 + 返回上一级，不再含父方案自身"
    );
    assert!(
        !level2.iter().any(|name| name == "拼音"),
        "第二级不该再显示父方案「拼音」"
    );
    let mut switch_fail = 0;
    for i in 0..16usize {
        let start = Instant::now();
        eng.apply_schema("pinyin");
        let open = eng.process_key(&KeyEvent::new(0x60, 1 << 2));
        let tab = eng.process_key(&KeyEvent::new(0xff09, 0));
        let current_menu = menu_names(&eng);
        if current_menu != level2 {
            switch_fail += 1;
            println!("  ⚠️ 第 {i:2} 项菜单顺序漂移: {current_menu:?}");
            continue;
        }
        if i >= 9 {
            eng.process_key(&KeyEvent::new(XK_NEXT, 0));
        }
        let digit = (i % 9 + 1) as u32;
        let eaten = eng.process_key(&KeyEvent::new('0' as u32 + digit, 0));
        let name = level2[i].clone();
        let expected = name_to_id.get(&name).cloned().unwrap_or_default();
        let actual = eng.schema.schema_id.clone();
        let ok = eaten && actual == expected;
        if !ok {
            switch_fail += 1;
        }
        println!(
            "  第 {:2} 项 {:<6} 数字{} -> schema={:<14} 期望={:<14} {}  {:6.1}ms open={} tab={}",
            i + 1,
            name,
            digit,
            actual,
            expected,
            if ok { "OK" } else { "FAIL" },
            ms(start),
            open,
            tab
        );
    }
    // 探针跑在 live 用户根时不能把 `var/previously_selected_schema` 留在子方案：
    // 显式回到父方案，再下结论。
    let start = Instant::now();
    eng.apply_schema("pinyin");
    println!("探针收尾回到 pinyin                      {:8.1} ms", ms(start));

    println!("16 项菜单切换失败数: {switch_fail}");
    assert_eq!(switch_fail, 0, "16 个拼音子方案必须都能按显示项选中");
}

