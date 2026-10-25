//! `librime-next` —— **librime 的 Rust 替代品**（不是"工具"，是要把 C++ 的 librime 一层层换掉的下一棒）。
//!
//! 当前 `config` 层已整体移植（17 个 C++ 文件 / 2,516 行 → `src/config.rs` + `src/config/`）：
//! `__include` / `__patch` / `__append` / `__merge`、5 个编译插件、`__build_info` 注入都在里面。
//! 命令名与 crate 名都是 `librime-next`。
//!
//! ## 为什么从这里开始
//!
//! 换掉 `src/rime/config/`（librime 里 2,516 行）**不需要碰 C++、不需要重编 rime.dll**：
//! 接缝是**文件**，不是函数。
//!
//! ```text
//! 输入：schema\<方案>\<方案>.schema.yaml   (+ default.yaml 与各预设)
//!   ↓  ← 这一层换成 Rust
//! 输出：build\<方案>\<方案>.schema.yaml
//!   ↓
//! 引擎照旧读产物、照旧用同一份 rime.dll
//! ```
//!
//! 而且**基线已经躺在磁盘上**：librime 编好的 30 份产物已备份到
//! `librime-next/fixtures/build-20261023/`（逐字节校验过）。所以每一小步都能对拍。
//!
//! ## 命令
//!
//! * `parse <目录|文件>` —— 解析并报告成功/失败（证明解析器吃不吃得下 yaml-cpp 的产物）。
//! * `roundtrip <目录>` —— 读 → 写 → 再读，树必须完全相等。
//! * `diff <A> <B>` —— 把两份解析成 JSON 树，逐路径列出差异（两边自动剔掉 `__build_info`）。
//! * `emit <输入> <输出>` —— 解析成树、转成 `ConfigItem`，再用**唯一出口**发射一份。
//! * `compile <源schema> <default.yaml> <输出>` —— 按 config 层的真实语义编译一份方案。
//!
//! ## 读写各自的唯一出口
//!
//! * **读**：`librime_next::config::load::load_file`（crate 顶层 `load_file` 是它的
//!   re-export）。`parse` / `diff` / `roundtrip` / `emit` 全走它，不再各写一份。
//! * **写**：`librime_next::config::emit::to_yaml_string`。`compile` / `emit` 都调它。
//!   例外只有 `roundtrip`：那里**故意**用 `serde_yaml` 发射，因为它要拿一个
//!   **独立实现**当参照物来验证我们的读入器（读入器 + 同一个发射器互证是循环论证）。
//!   它不是产物写出路径。
//!
//! 语义等价优先于字节等价：`diff` 比树，yaml-cpp 的引号/缩进风格不追。
//! **键序追**（但不靠 `diff` 验）：树的键序来自源文件（`IndexMap`），`compile` / `emit`
//! 写出的键序与手写源一致；`diff` 只比值，所以键序改变不会掩盖值语义的差异。

use std::path::{Path, PathBuf};

use librime_next::config;
use librime_next::load_file;

type Tree = serde_json::Value;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [cmd, path] if cmd == "parse" => parse(Path::new(path)),
        [cmd, a, b] if cmd == "diff" => diff(Path::new(a), Path::new(b)),
        [cmd, a, b] if cmd == "emit" => emit(Path::new(a), Path::new(b)),
        [cmd, p] if cmd == "roundtrip" => roundtrip(Path::new(p)),
        // compile <源schema> <default.yaml> <输出>
        [cmd, a, b, c] if cmd == "compile" => compile(Path::new(a), Path::new(b), Path::new(c)),
        _ => {
            eprintln!("用法:");
            eprintln!("  librime-next parse <目录|文件>    解析并报告");
            eprintln!("  librime-next diff  <A> <B>        逐路径列出差异");
            std::process::exit(2);
        }
    }
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        out.push(dir.to_path_buf());
        return;
    };
    let mut items: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    items.sort();
    for item in items {
        if item.is_dir() {
            collect(&item, out);
        } else if item.extension().and_then(|e| e.to_str()) == Some("yaml") {
            out.push(item);
        }
    }
}

fn parse(path: &Path) {
    let mut files = Vec::new();
    collect(path, &mut files);
    let (mut ok, mut bad) = (0usize, 0usize);
    for file in &files {
        match load_file(file) {
            Ok(tree) => {
                ok += 1;
                if std::env::var_os("RIME_CONFIG_VERBOSE").is_some() {
                    let keys = tree.as_object().map(|m| m.len()).unwrap_or(0);
                    println!("OK   {keys:3} 个顶层键  {}", file.display());
                }
            }
            Err(error) => {
                bad += 1;
                println!("FAIL {error}");
            }
        }
    }
    println!("== 解析 {ok} 成功 / {bad} 失败（共 {} 份）==", files.len());
    if bad > 0 {
        std::process::exit(1);
    }
}

/// 解析 → 转成 `ConfigItem` → 用唯一出口发射 → 写出。
///
/// 用于「把我们的产物写回去替换 librime 生成的」：发射规则与 `compile` 完全同一套，
/// 所以写回去的文件与 librime 产物的标量类型/引号风格一致。
fn emit(input: &Path, output: &Path) {
    let tree = match load_file(input) {
        Ok(tree) => tree,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    // 词典（`...` 之后是词条表）不归这里管：`load_file` 只记了 `__entries` 行数，
    // 重写词典是 `dict/` 那层的事。硬写会把词条表整段丢掉，所以明确拒绝。
    if tree.get("__entries").is_some() || tree.get("__documents").is_some() {
        eprintln!(
            "{}: 这是 Rime 词典/多文档 YAML，emit 只处理单文档配置；重写词典等 dict 层移植",
            input.display()
        );
        std::process::exit(1);
    }
    let item = config::convert_from_yaml(&tree, None);
    let text = match config::to_yaml_string(&item) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("序列化失败: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = std::fs::write(output, text) {
        eprintln!("写 {} 失败: {error}", output.display());
        std::process::exit(1);
    }
    println!("已写出 {}", output.display());
}

/// 自检：读 → 写 → 再读，树必须完全相等。这是"能替换"的最低门槛。
///
/// 这里的"写"**故意**用 `serde_yaml`：拿一个独立实现当参照物，才能验证我们的
/// **读入器**没有丢信息（用同一个发射器互证是循环论证）。产物写出见 [`emit`] / [`compile`]。
fn roundtrip(path: &Path) {
    let mut files = Vec::new();
    collect(path, &mut files);
    let mut bad = 0usize;
    for file in &files {
        let first = match load_file(file) {
            Ok(tree) => tree,
            Err(error) => {
                println!("读失败 {error}");
                bad += 1;
                continue;
            }
        };
        let text = match serde_norway::to_string(&first) {
            Ok(text) => text,
            Err(error) => {
                println!("写失败 {}: {error}", file.display());
                bad += 1;
                continue;
            }
        };
        let second = match serde_norway::from_str::<Tree>(&text) {
            Ok(tree) => tree,
            Err(error) => {
                println!("重读失败 {}: {error}", file.display());
                bad += 1;
                continue;
            }
        };
        let mut diffs = Vec::new();
        compare("", &first, &second, &mut diffs);
        if !diffs.is_empty() {
            println!(
                "往返有差异 {}: {} 处，例：{}",
                file.display(),
                diffs.len(),
                diffs[0]
            );
            bad += 1;
        }
    }
    println!(
        "== 往返 {} 通过 / {} 失败（共 {} 份）==",
        files.len() - bad,
        bad,
        files.len()
    );
    if bad > 0 {
        std::process::exit(1);
    }
}

/// 从源 schema 编译出产物 —— 就是 librime `rime deploy` 里 "编译 config" 那一段
/// （`ConfigBuilder::LoadConfig` + `core_module.cc` 的插件顺序，见 [`librime_next::config`]）。
///
/// 写出前剔掉 `__build_info`（里面是各源文件 mtime，逐字节复刻没意义），
/// 发射走 [`config::to_yaml_string`]（全 crate 唯一出口）。
fn compile(schema: &Path, defaults: &Path, output: &Path) {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(parent) = schema.parent() {
        dirs.push(parent.to_path_buf());
    }
    if let Some(parent) = defaults.parent() {
        if !dirs.contains(&parent.to_path_buf()) {
            dirs.push(parent.to_path_buf());
        }
    }
    let mut tree = match config::compile(schema, defaults, &dirs) {
        Ok(tree) => tree,
        Err(error) => {
            eprintln!("编译失败: {error}");
            std::process::exit(1);
        }
    };
    config::strip_build_info(&mut tree);
    let text = match config::to_yaml_string(&tree) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("发射失败: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = std::fs::write(output, text) {
        eprintln!("写 {} 失败: {error}", output.display());
        std::process::exit(1);
    }
    println!("已写出 {}", output.display());
}

fn diff(a: &Path, b: &Path) {
    // 对拍时两边都剔掉 `__build_info`（里面是各源文件 mtime，不可能逐字节一致）
    let strip = |tree: Tree| {
        let mut tree = tree;
        config::strip_build_info_json(&mut tree);
        tree
    };
    let (Ok(left), Ok(right)) = (load_file(a).map(strip), load_file(b).map(strip)) else {
        eprintln!("解析失败，先跑 parse 看是哪份");
        std::process::exit(2);
    };
    let mut diffs = Vec::new();
    compare("", &left, &right, &mut diffs);
    for line in diffs.iter().take(80) {
        println!("{line}");
    }
    println!("== 差异 {} 处（显示前 80）==", diffs.len());
    if !diffs.is_empty() {
        std::process::exit(1);
    }
}

fn compare(path: &str, left: &Tree, right: &Tree, out: &mut Vec<String>) {
    match (left, right) {
        (Tree::Object(l), Tree::Object(r)) => {
            for (key, lv) in l {
                let child = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                match r.get(key) {
                    Some(rv) => compare(&child, lv, rv, out),
                    None => out.push(format!("- 仅 A 有: {child}")),
                }
            }
            for key in r.keys() {
                if !l.contains_key(key) {
                    let child = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    out.push(format!("+ 仅 B 有: {child}"));
                }
            }
        }
        (Tree::Array(l), Tree::Array(r)) => {
            if l.len() != r.len() {
                out.push(format!("{path}: 列表长度 A={} B={}", l.len(), r.len()));
            }
            for (i, (lv, rv)) in l.iter().zip(r.iter()).enumerate() {
                compare(&format!("{path}[{i}]"), lv, rv, out);
            }
        }
        _ if left != right => out.push(format!("{path}: A={left} B={right}")),
        _ => {}
    }
}
