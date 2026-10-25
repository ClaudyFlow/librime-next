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

    // 还没移植的组件被跳过，但不能因此让链条断掉
    assert!(
        !names.contains(&"recognizer"),
        "recognizer 还没移植，不该出现在名单里"
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
