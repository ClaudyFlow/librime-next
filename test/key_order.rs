//! 键序不变量：**源文件的顶层键序，必须是产物顶层键序的保序子序列**。
//!
//! 2026-10-23 加。背景：`librime-next` 早先用 `serde_json::Value` 的 `BTreeMap` 存键，
//! 产物键被按字节序重排，和用户手写的源文件对不上 —— 用户原话「一堆乱序导致写个文件十分混乱」。
//!
//! 但「顺序对不对」不能靠人眼，判据必须是机械的。本文件就是那条判据：
//!
//! * 产物**允许**比源多出键（`menu`/`navigator`/`selector` 由 `default_config_plugin` 补，
//!   `__build_info` 由 `build_info_plugin` 注入，`import_preset` 会把预设的节展开进来）；
//! * 产物**不允许**打乱源里已有键的相对次序。
//!
//! 为什么是"子序列"而不是"相等"：`__include` 展开会把预设文件的键插到前面
//! （如 `punctuator` 被 `symbols` 的节整体替换），那是 librime 的合并语义，不是我们的 bug。
//!
//! `%APPDATA%` 不存在（换机器 / CI）时全部跳过，不失败。

use std::path::{Path, PathBuf};

use librime_next::config::{self, Item};

fn user_data() -> Option<PathBuf> {
    let dir = std::env::var_os("APPDATA")?;
    let path = PathBuf::from(dir).join("Rime-Next");
    path.is_dir().then_some(path)
}

fn shared_data() -> Option<PathBuf> {
    let path = PathBuf::from(r"C:\Program Files\Weasel-Next\Rime-Next");
    path.is_dir().then_some(path)
}

/// 读 YAML 成配置树。`load_file` 给的是 `serde_json::Value`（保序），
/// 再用 `convert_from_yaml` 转成 `Item`（`__` 指令键在这一步被识别并丢掉）。
fn load_item(path: &Path) -> Option<Item> {
    let value = librime_next::load_file(path).ok()?;
    Some(config::data::convert_from_yaml(&value, None))
}

/// 按出现顺序取顶层键。`MapRef` 是 `IndexMap`，迭代顺序 = 插入顺序。
fn top_keys(item: &Item) -> Vec<String> {
    match item.as_map() {
        Some(map) => map.borrow().keys().cloned().collect(),
        None => Vec::new(),
    }
}

/// 收集「有源文件的方案」：`<用户目录>/schema/<名>/<名>.schema.yaml`。
fn schemes(user: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(user.join("schema")).into_iter().flatten() {
        let Ok(entry) = entry else { continue };
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let source = entry.path().join(format!("{name}.schema.yaml"));
        if source.is_file() {
            out.push((name, source));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// `needles` 是否是 `haystack` 的保序子序列；且每个 needle 都必须出现。
fn assert_ordered_subsequence(source: &[String], compiled: &[String], label: &str) {
    let mut cursor = 0usize;
    for (index, key) in source.iter().enumerate() {
        match compiled[cursor..].iter().position(|k| k == key) {
            Some(offset) => cursor += offset + 1,
            None => {
                if !compiled.iter().any(|k| k == key) {
                    panic!("{label}: 源里的顶层键 `{key}` 在产物里消失了");
                }
                panic!(
                    "{label}: 键序被打乱 —— 源里第 {index} 个键 `{key}`（前面是 {:?}）\
                     在产物里跑到了更前面。\n源序:   {source:?}\n产物序: {compiled:?}",
                    &source[..index]
                );
            }
        }
    }
}

/// 主判据：每个方案的源顶层键序都要被产物保持（允许产物插入新键）。
#[test]
fn compiled_top_level_key_order_follows_source() {
    let Some(user) = user_data() else {
        println!("跳过：%APPDATA%\\Rime-Next 不存在");
        return;
    };
    let Some(shared) = shared_data() else {
        println!("跳过：程序文件夹的 Rime-Next 数据树不存在");
        return;
    };
    let defaults = shared.join("default.yaml");

    let mut checked = Vec::new();
    for (name, source_path) in schemes(&user) {
        let Some(source) = load_item(&source_path) else {
            continue;
        };
        let mut search = vec![shared.clone()];
        if let Some(parent) = source_path.parent() {
            search.insert(0, parent.to_path_buf());
        }
        let compiled = match config::compile(&source_path, &defaults, &search) {
            Ok(tree) => tree,
            Err(error) => panic!("{name}: 编译失败: {error}"),
        };
        // 源里的这两类键不是"普通键"，编译时必然被消费掉，不会以字面形式出现在产物里：
        //   * `__include` / `__patch` —— 编译器指令；
        //   * 含 `/` 的键（`speller/+`、`translator/+`、`key/=`）—— 路径编辑算子，
        //     `yuhao_quanpin.schema.yaml:19` 的 `speller/+: {algebra: [...]}` 就是追加到 `speller`。
        // 比对键序前先剔掉，否则是假报错。
        let source_keys: Vec<String> = top_keys(&source)
            .into_iter()
            .filter(|key| !key.starts_with("__") && !key.contains('/'))
            .collect();
        assert_ordered_subsequence(&source_keys, &top_keys(&compiled), &name);
        checked.push(name);
    }

    println!("键序检查通过：{} 个方案（{checked:?}）", checked.len());
    assert!(
        checked.len() >= 10,
        "只检查到 {} 个方案，用户目录可能不对",
        checked.len()
    );
}

/// 反向用例：确认判据真的会拒绝乱序（否则上面那条测试可能恒真）。
#[test]
fn subsequence_check_rejects_reordering() {
    let source: Vec<String> = ["schema", "switches", "engine"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    // 顺序保持 → 通过（中间允许插入新键）
    let ok: Vec<String> = ["schema", "menu", "switches", "engine"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_ordered_subsequence(&source, &ok, "ok");

    // 顺序被打乱 → 必须 panic
    let bad: Vec<String> = ["switches", "schema", "engine"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let caught = std::panic::catch_unwind(|| assert_ordered_subsequence(&source, &bad, "bad"));
    assert!(caught.is_err(), "判据没有拦住乱序，说明这条测试是假的");

    // 键消失 → 必须 panic
    let missing: Vec<String> = ["schema", "switches"].iter().map(|s| s.to_string()).collect();
    let caught = std::panic::catch_unwind(|| assert_ordered_subsequence(&source, &missing, "miss"));
    assert!(caught.is_err(), "判据没有拦住丢键");
}

/// 最强的一条：`emit`（纯读→写，不编译）后**逐键**等于源顺序。
#[test]
fn emit_preserves_exact_key_order() {
    let Some(user) = user_data() else {
        println!("跳过：%APPDATA%\\Rime-Next 不存在");
        return;
    };

    let mut checked = 0usize;
    let temp = std::env::temp_dir().join("librime-next-key-order.scratch.yaml");

    for (name, source_path) in schemes(&user) {
        let Some(source) = load_item(&source_path) else {
            continue;
        };
        let Ok(text) = config::to_yaml_string(&source) else {
            continue;
        };
        // 写临时文件再读回：走的就是产品自己的读入路径，不引入第二套解析器
        if std::fs::write(&temp, text.as_bytes()).is_err() {
            continue;
        }
        let Some(again) = load_item(&temp) else {
            continue;
        };
        assert_eq!(
            top_keys(&source),
            top_keys(&again),
            "{name}: emit 之后键序变了"
        );
        checked += 1;
    }
    let _ = std::fs::remove_file(&temp);

    println!("emit 键序逐键一致：{checked} 个文件");
    assert!(checked >= 10, "只验证到 {checked} 个文件");
}
