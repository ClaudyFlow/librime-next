//! 组件的**动态装配**：方案里声明的 `engine/processors` / `engine/translators`
//! 必须真的被读进来并按名建 Kč对象。
//!
//! 对照 C++ `ConcreteEngine::InitializeComponents`（`engine.cc:549-575`）：
//! 名单来自方案配置，而不是硬编码 —— 这份断言就是在证明这件事。
//!
//! 数据：`test/e2e/build/`（真实 `luna_pinyin_simp` 的编译产物）。

use librime_next::engine::runtime::Engine;
use std::path::PathBuf;

fn build_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/e2e/build")
}

#[test]
fn real_schema_assembles_processors_from_config() {
    let engine = Engine::create(&build_dir(), "luna_pinyin_simp")
        .expect("luna_pinyin_simp 引擎应能加载");
    let names = engine.processor_names();
    println!("processors = {names:?}");
    println!("translators = {:?}", engine.translator_names());

    // 方案里声明的已移植组件必须都在，并且**按源文件顺序**
    assert!(
        names.contains(&"ascii_composer"),
        "应有 ascii_composer，实际 {names:?}"
    );
    assert!(names.contains(&"speller"), "应有 speller，实际 {names:?}");
    assert!(
        names.contains(&"express_editor"),
        "应有 express_editor，实际 {names:?}"
    );
    assert!(names.contains(&"navigator"), "应有 navigator，实际 {names:?}");
    assert!(names.contains(&"selector"), "应有 selector，实际 {names:?}");

    // recognizer 已移植（2026-10-26）：它的作用是放行 `@` 这类不在 alphabet 里的键
    assert!(
        names.contains(&"recognizer"),
        "应有 recognizer，实际 {names:?}"
    );
    // key_binder / punctuator 还没移植：被跳过，但链条不能断
    assert!(
        !names.contains(&"key_binder") && !names.contains(&"punctuator"),
        "未移植组件不该混进名单：{names:?}"
    );

    // 分段器名单同样来自配置（真实 luna_pinyin_simp 的 `engine/segmentors`
    // 就是 matcher → abc_segmentor → fallback_segmentor，不是我们编的顺序）
    let seg_names = engine.segmentor_names();
    println!("segmentors = {seg_names:?}");
    assert_eq!(
        seg_names,
        vec!["matcher", "abc_segmentor", "fallback_segmentor"],
        "分段器装配必须来自配置：{seg_names:?}"
    );
    // 顺序必须与方案配置一致（luna_pinyin_simp: ascii_composer 在 speller 之前）
    let i_ascii = names.iter().position(|n| *n == "ascii_composer").unwrap();
    let i_speller = names.iter().position(|n| *n == "speller").unwrap();
    assert!(i_ascii < i_speller, "名单顺序应来自配置：{names:?}");
}

#[test]
fn translator_is_created_from_engine_translators_list() {
    let engine = Engine::create(&build_dir(), "luna_pinyin_simp")
        .expect("luna_pinyin_simp 引擎应能加载");
    let names = engine.translator_names();
    println!("translators = {names:?}");
    assert!(
        names.iter().any(|n| n.starts_with("script_translator")),
        "luna_pinyin_simp 的 engine/translators 应产出 script_translator，实际 {names:?}"
    );
}

#[test]
fn multi_segment_input_produces_raw_then_abc() {
    // 分段框架的意义：输入不再是"一大段"，而是按规则切成若干段分别翻译。
    let mut engine = Engine::create(&build_dir(), "luna_pinyin_simp")
        .expect("luna_pinyin_simp 引擎应能加载");
    // 空格不在 `speller/alphabet` 里 → fallback_segmentor 切成 raw 段；
    // 后面的字母由 abc_segmentor 切成 abc 段。
    engine.context.input = " zho".to_string();
    engine.context.caret_pos = 4;
    engine.compose();

    let shapes: Vec<(usize, usize, Vec<String>)> = engine
        .context
        .composition
        .segmentation
        .segments
        .iter()
        .map(|s| (s.start, s.end, s.tags.clone()))
        .collect();
    println!("segments = {shapes:?}");
    assert!(
        shapes.len() >= 2,
        "「 zho」应切成 raw + abc 两段，实际 {shapes:?}"
    );
    assert!(shapes[0].2.contains(&"raw".to_string()), "首段应是 raw");
    assert!(
        shapes.iter().any(|s| s.2.contains(&"abc".to_string())),
        "应有一段带 abc 标签：{shapes:?}"
    );
}

#[test]
fn filters_are_assembled_from_config() {
    let engine = Engine::create(&build_dir(), "luna_pinyin_simp")
        .expect("luna_pinyin_simp 引擎应能加载");
    let names = engine.filter_names();
    println!("filters = {names:?}");
    // 真实方案至少声明了 uniquifier；它必须真的挂到链上（不再只是个名字）
    assert!(
        names.iter().any(|n| *n == "uniquifier"),
        "应有 uniquifier，实际 {names:?}"
    );
}

#[test]
fn uniquifier_removes_duplicate_candidates_real_dict() {
    let mut engine = Engine::create(&build_dir(), "luna_pinyin_simp")
        .expect("luna_pinyin_simp 引擎应能加载");
    engine.context.input = "nihao".to_string();
    engine.context.caret_pos = 5;
    engine.compose();

    let texts: Vec<String> = engine
        .context
        .composition
        .segmentation
        .segments
        .first()
        .and_then(|s| s.menu.as_ref())
        .map(|m| {
            m.candidates()
                .iter()
                .map(|c| c.text.clone())
                .collect()
        })
        .expect("应有菜单");
    // 实测（2026-10-26，真实 luna_pinyin_simp 产物）：
    //   关掉 filter 链时 nihao 有 294 条候选，挂上 uniquifier 后 293 条
    //   —— 正好删掉 1 条重复。这条断言就是在守住"filter 链真的被调用"。
    let total = texts.len();
    let mut deduped = texts.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(
        total,
        deduped.len(),
        "uniquifier 生效后不应出现重复词条：{:?}",
        &texts[..total.min(15)]
    );
}
