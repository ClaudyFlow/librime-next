//! Rime-Next 方言识别（2026-10-27 定稿）。
//!
//! 两层能力，都以"用户怎么写，识别就怎么认"为准：
//!
//! 1. **单数键归一化**（`config::load::load_file` 里做）：`dependency` / `switch` /
//!    `state` / `processor` / `initial` / `dict` / 反查的 `schema` / `schema:id`
//!    在读入时翻成 librime 的复数键；
//! 2. **通用 `import_preset`**（`GeneralImportPresetPlugin`）：任意节点写
//!    `import_preset: <配置id>`，用该配置**同路径**的节点整体替换；
//!    parse 数据可以住任意子文件夹（`parser\<大类>\<名>.parser.yaml`）。
//!
//! 本测试用临时目录搭一棵最小数据树，走完整的 `ConfigBuilder::with_core_plugins()`
//! 编译管线，断言产物里方言键全部落位。

use std::fs;

use librime_next::config::Item;

/// 沿 `/` 路径取节点（每步克隆，测试用足够）。`@N` = 列表第 N 项。
fn at(item: &Item, path: &str) -> Option<Item> {
    let mut current = item.clone();
    for seg in path.split('/') {
        if let Some(index) = seg.strip_prefix('@') {
            let list = current.as_list()?;
            let next = list.borrow().get(index.parse::<usize>().ok()?).cloned()?;
            current = next;
        } else {
            let map = current.as_map()?;
            let next = map.borrow().get(seg).cloned()?;
            current = next;
        }
    }
    Some(current)
}

fn s(item: &Item, path: &str) -> Option<String> {
    match at(item, path) {
        Some(Item::Scalar(v)) => Some(v),
        _ => None,
    }
}

fn list_at(item: &Item, path: &str) -> Option<Vec<Item>> {
    match at(item, path) {
        Some(Item::List(list)) => Some(list.borrow().clone()),
        _ => None,
    }
}

fn write(path: &std::path::Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn setup_tree(root: &std::path::Path) {
    // 方案：全单数方言 + 两处通用 import_preset
    write(
        &root.join("schema/pinyin/pinyin.schema.yaml"),
        r#"---
schema:
  id: pinyin
  name: 全拼
dependency:
  - stroke
###
switch:
  - name: ascii_mode
    state: [ 中文, 西文 ]
    reset: 0
  - name: full_shape
    state: [ 半角, 全角 ]
###
engine:
  processor:
    - ascii_composer
    - speller
    - selector
  segmentor:
    - abc_segmentor
    - fallback_segmentor
  translator:
    - punct_translator
    - script_translator
  filter:
    - uniquifier
###
speller:
  alphabet: zyxwvutsrqponmlkjihgfedcba
  initial: zyxwvutsrqponmlkjihgfedcba
  delimiter: " '"
  algebra:
    import_preset: pinyin.parser
###
translator:
  dict: luna_pinyin
  prism: pinyin
  preedit_format:
    import_preset: pinyin.parser
###
reverse_lookup:
  schema: stroke
  prefix: "`"
###
punctuator:
  import_preset: symbols
...
"#,
    );
    // parse 数据住子文件夹：parser/pinyin/pinyin.parser.yaml
    write(
        &root.join("parser/pinyin/pinyin.parser.yaml"),
        r#"speller:
  algebra:
    - abbrev/^([a-z]).+$/$1/
    - derive/^([jqxy])u/$1v/
translator:
  preedit_format:
    - xform/([nl])v/$1ü/
"#,
    );
    // import_preset 引用到的预设：symbols（走 legacy 插件，确认不受方言影响）
    write(
        &root.join("symbols.yaml"),
        r#"punctuator:
  half_shape:
    "," : "，"
"#,
    );
}

fn compile_tree(root: &std::path::Path) -> Item {
    let schema_path = root.join("schema/pinyin/pinyin.schema.yaml");
    let defaults_path = root.join("default.yaml");
    fs::write(&defaults_path, "config_version: \"0.1\"\n").unwrap();
    librime_next::config::compile(&schema_path, &defaults_path, &[]).expect("方言方案编译失败")
}

#[test]
fn dialect_keys_are_recognized_and_imports_resolved() {
    let root = std::env::temp_dir().join(format!(
        "rime_next_dialect_test_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    setup_tree(&root);

    let compiled = compile_tree(&root);

    // schema:id → schema_id
    assert_eq!(
        s(&compiled, "schema/schema_id").as_deref(),
        Some("pinyin"),
        "schema:id 应被识别为 schema_id"
    );
    // dependency → dependencies
    assert!(
        list_at(&compiled, "dependencies").is_some(),
        "dependency 应被识别为 dependencies"
    );
    // switch → switches，列表项 state → states
    assert!(list_at(&compiled, "switches").is_some(), "switch 应被识别为 switches");
    assert_eq!(
        s(&compiled, "switches/@0/states/@0").as_deref(),
        Some("中文"),
        "switch 列表项的 state 应被识别为 states"
    );
    // engine 下四个单数键 → 复数
    assert_eq!(
        s(&compiled, "engine/processors/@1").as_deref(),
        Some("speller"),
        "engine/processor 应被识别为 processors"
    );
    assert_eq!(
        s(&compiled, "engine/segmentors/@0").as_deref(),
        Some("abc_segmentor"),
    );
    assert_eq!(
        s(&compiled, "engine/translators/@1").as_deref(),
        Some("script_translator"),
    );
    assert_eq!(
        s(&compiled, "engine/filters/@0").as_deref(),
        Some("uniquifier"),
    );
    // speller/initial → initials
    assert!(
        s(&compiled, "speller/initials").is_some(),
        "speller/initial 应被识别为 initials"
    );
    // 通用 import_preset：algebra 从子文件夹的 parser 数据整体替换进来
    eprintln!("== speller 节点 ==\n{}", librime_next::config::to_yaml_string(
        &at(&compiled, "speller").expect("speller 节点存在")).expect("emit"));
    assert_eq!(
        s(&compiled, "speller/algebra/@1").as_deref(),
        Some("derive/^([jqxy])u/$1v/"),
        "speller/algebra 的 import_preset 应解析出 parser 数据的同路径节点"
    );
    assert!(
        s(&compiled, "speller/algebra/import_preset").is_none(),
        "import_preset 引用键应被消费掉"
    );
    // translator/dict：词库键的正式写法就是单数 `dict`（2026-10-28 20:5x 定），
    // 没有 `dictionary` 兼容层 ⇒ 编译产物里也只会出现 `dict`
    assert_eq!(
        s(&compiled, "translator/dict").as_deref(),
        Some("luna_pinyin"),
        "translator/dict 是词库键的正式写法"
    );
    assert_eq!(
        s(&compiled, "translator/dictionary"),
        None,
        "编译产物不该出现 dictionary"
    );
    assert_eq!(
        s(&compiled, "translator/preedit_format/@0").as_deref(),
        Some("xform/([nl])v/$1ü/"),
        "translator/preedit_format 的 import_preset 应解析出 parser 数据"
    );
    // reverse_lookup/schema 保持原样：它是"用哪份方案反查"的正式键，由反查翻译器
    // 直接按方案引用解析（取目标方案自己的 `translator/dict` + `prism`），不做改名。
    assert_eq!(
        s(&compiled, "reverse_lookup/schema").as_deref(),
        Some("stroke"),
        "reverse_lookup/schema 应原样保留（反查 = 用方案）"
    );
    assert_eq!(
        s(&compiled, "reverse_lookup/dictionary"),
        None,
        "不该再被改写成 dictionary"
    );
    // legacy import_preset 不受影响：punctuator 仍从 symbols 预设并进来
    assert_eq!(
        s(&compiled, "punctuator/half_shape/,").as_deref(),
        Some("，"),
        "punctuator/import_preset: symbols 应继续走 legacy 插件"
    );

    let _ = fs::remove_dir_all(&root);
}
