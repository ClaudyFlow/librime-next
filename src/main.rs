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
//! `librime-next/fixture/build-20261023/`（逐字节校验过）。所以每一小步都能对拍。
//!
//! ## 命令
//!
//! * `parse <目录|文件>` —— 解析并报告成功/失败（证明解析器吃不吃得下 yaml-cpp 的产物）。
//! * `roundtrip <目录>` —— 读 → 写 → 再读，树必须完全相等。
//! * `diff <A> <B>` —— 把两份解析成 JSON 树，逐路径列出差异（两边自动剔掉 `__build_info`）。
//! * `emit <输入> <输出>` —— 解析成树、转成 `ConfigItem`，再用**唯一出口**发射一份。
//! * `compile <源schema> <default.yaml> <输出>` —— 按 config 层的真实语义编译一份方案。
//! * `admit <schema.yaml> [--user <用户目录>] [--active] [--dry-run]` ——
//!   方案准入检查（文件头标记 / 实际格式 / `schema:id` 三关），过了才写进
//!   `<用户目录>/schema/schema.yaml` 点名清单（可视化方案列表）。
//! * `deploy [--shared <共享目录>] [--user <用户目录>]` —— 跑一次完整部署
//!   （等价于 Weasel-Next-Engine.exe --deploy）：重编译 `schema/schema.yaml`
//!   清单里的每个方案，产物落到 `<用户文件夹>/build`。
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
use librime_next::lever::deployer::Deployer;
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
        [cmd, path, rest @ ..] if cmd == "admit" => admit_cli(Path::new(path), rest),
        [cmd, rest @ ..] if cmd == "deploy" => deploy_cli(rest),
        [cmd, rest @ ..] if cmd == "manifest" => manifest_cli(rest),
        [cmd, rest @ ..] if cmd == "dicts" => dicts_cli(rest),
        _ => {
            eprintln!("用法:");
            eprintln!("  librime-next parse <目录|文件>    解析并报告");
            eprintln!("  librime-next diff  <A> <B>        逐路径列出差异");
            eprintln!(
                "  librime-next admit <schema.yaml> [--user <目录>] [--active] [--dry-run]"
            );
            eprintln!("                             方案准入检查，过了才进方案列表");
            eprintln!("  librime-next dicts [--shared <目录>] [--user <目录>]");
            eprintln!("                             词库识别：列 `<根>/dict/**` 的收下/抛弃名单");
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

/// `admit`：方案准入检查器 CLI。
///
/// 三关（文件头标记 / 实际格式匹配 / `schema:id` 内容）全过才把 id 写进
/// `<用户目录>/schema/schema.yaml` 点名清单；任何一关挂掉不动清单。
fn admit_cli(path: &Path, flags: &[String]) {
    use librime_next::lever::admit;

    let mut user_dir: Option<std::path::PathBuf> = None;
    let mut set_active = false;
    let mut dry_run = false;
    let mut i = 0;
    while i < flags.len() {
        match flags[i].as_str() {
            "--active" => set_active = true,
            "--dry-run" => dry_run = true,
            "--user" => {
                i += 1;
                match flags.get(i) {
                    Some(dir) => user_dir = Some(std::path::PathBuf::from(dir)),
                    None => {
                        eprintln!("--user 后面要跟目录");
                        std::process::exit(2);
                    }
                }
            }
            other => {
                eprintln!("不认识的参数: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let report = admit::check_schema_file(path);
    print!("{}", admit::format_report(&report));
    if !report.passed() {
        std::process::exit(1);
    }

    if dry_run {
        println!("（--dry-run：只检查，不写清单）");
        return;
    }

    let user_dir = match user_dir.or_else(admit::default_user_data_dir) {
        Some(dir) => dir,
        None => {
            eprintln!("找不到用户数据目录：请用 --user <目录> 或设 RIME_USER_DIR");
            std::process::exit(2);
        }
    };
    match admit::admit(path, &user_dir, set_active) {
        Ok((_, added)) => {
            let manifest =
                librime_next::lever::deployment_tasks::load_schema_manifest(&user_dir);
            if added {
                println!(
                    "已加入方案清单 {}（现共 {} 项）",
                    user_dir.join("schema").join("schema.yaml").display(),
                    manifest.schemas.len()
                );
            } else {
                println!("该方案已在清单里（不重复加）");
            }
            if set_active {
                println!("active 已指向 {}", manifest.active.unwrap_or_default());
            }
            println!("提示：跑一次部署（Weasel-Next-Engine.exe --deploy）后界面列表才更新");
        }
        Err(error) => {
            eprintln!("写清单失败: {error}");
            std::process::exit(1);
        }
    }
}

/// `deploy`：跑一次完整部署（installation_update → workspace_update → ...）。
///
/// 等价于 Weasel-Next-Engine.exe --deploy：重编译 `schema/schema.yaml` 清单里的每个方案，
/// 产物落到 `<用户文件夹>/build`。默认目录取真实安装位置，可用参数覆盖：
/// `librime-next deploy [--shared <共享目录>] [--user <用户目录>]`。
fn deploy_cli(flags: &[String]) {
    let mut shared: Option<PathBuf> = None;
    let mut user: Option<PathBuf> = None;
    let mut i = 0;
    while i < flags.len() {
        match flags[i].as_str() {
            "--shared" => {
                i += 1;
                match flags.get(i) {
                    Some(d) => shared = Some(PathBuf::from(d)),
                    None => {
                        eprintln!("--shared 后面要跟目录");
                        std::process::exit(2);
                    }
                }
            }
            "--user" => {
                i += 1;
                match flags.get(i) {
                    Some(d) => user = Some(PathBuf::from(d)),
                    None => {
                        eprintln!("--user 后面要跟目录");
                        std::process::exit(2);
                    }
                }
            }
            other => {
                eprintln!("不认识的参数: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let user = user
        .or_else(|| std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("Rime-Next")))
        .unwrap_or_else(|| PathBuf::from("."));
    let shared = shared.unwrap_or_else(default_shared_dir);

    let mut deployer = Deployer::with_data_dirs(&shared, &user);
    let ok = deployer.deploy_workspace();
    println!(
        "部署完成：ok={ok}（shared={}，user={}）",
        shared.display(),
        user.display()
    );
    if !ok {
        std::process::exit(1);
    }
}

/// 程序文件夹（内置数据目录 `shared_data_dir`）的候选，按优先级找第一个有 `default.yaml` 的。
///
/// 2026-10-28 新布局优先：`Program Files\Rime-Next\data`（安装路径 + 产品名 + data）；
/// 其次是旧装机 `Program Files\Weasel-Next\Rime-Next`，最后是代码仓里的源数据目录。
fn default_shared_dir() -> PathBuf {
    for candidate in [
        PathBuf::from(r"C:\Program Files\Rime-Next\data"),
        PathBuf::from(r"C:\Program Files\Weasel-Next\Rime-Next"),
        PathBuf::from(r"C:\Code\Weasel-Next\Rime-Next"),
        PathBuf::from(r"C:\code\Weasel-Next\Rime-Next"),
    ] {
        if candidate.join("default.yaml").is_file() {
            return candidate;
        }
    }
    PathBuf::from(".")
}

/// `manifest`：方案清单的识别与生成（部署第一步干的事，单独拎出来便于排查）。
///
/// * 不带参数：扫程序文件夹 → 列出收下/抛弃 → 清单缺失才生成（与部署同规则）。
/// * `--rebuild`：先删掉现有清单再生成（**会丢掉用户已勾选的内容**，仅排查用）。
fn manifest_cli(flags: &[String]) {
    use librime_next::lever::schema_list;

    let mut shared: Option<PathBuf> = None;
    let mut user: Option<PathBuf> = None;
    let mut rebuild = false;
    let mut i = 0;
    while i < flags.len() {
        match flags[i].as_str() {
            "--shared" => {
                i += 1;
                shared = flags.get(i).map(PathBuf::from);
                if shared.is_none() {
                    eprintln!("--shared 后面要跟目录");
                    std::process::exit(2);
                }
            }
            "--user" => {
                i += 1;
                user = flags.get(i).map(PathBuf::from);
                if user.is_none() {
                    eprintln!("--user 后面要跟目录");
                    std::process::exit(2);
                }
            }
            "--rebuild" => rebuild = true,
            other => {
                eprintln!("不认识的参数: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let user = user
        .or_else(|| std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("Rime-Next")))
        .unwrap_or_else(|| PathBuf::from("."));
    let shared = shared.unwrap_or_else(default_shared_dir);
    println!("程序文件夹（shared）: {}", shared.display());
    println!("用户文件夹（user）  : {}", user.display());

    if rebuild {
        let path = user.join("schema").join("schema.yaml");
        if path.is_file() {
            let _ = std::fs::remove_file(&path);
            println!("\n（--rebuild：已删除现有清单 {}）", path.display());
        }
    }

    let found = schema_list::discover_roots(&[shared.clone(), user.clone()]);
    println!(
        "\n== 识别结果：收下 {} / 抛弃 {} ==",
        found.accepted.len(),
        found.rejected.len()
    );
    for item in &found.accepted {
        println!("  [收下] {:<28} {}", item.id, item.name.as_deref().unwrap_or(""));
    }
    for (path, reason) in &found.rejected {
        println!("  [抛弃] {}\n         └─ {reason}", path.display());
    }

    let outcome = schema_list::ensure_manifest(&user, &shared);
    let manifest = outcome.manifest();
    println!("\n== 清单 {} ==", user.join("schema").join("schema.yaml").display());
    if outcome.generated() {
        println!("  本次新生成：{} 项", manifest.schemas.len());
    } else if manifest.schemas.is_empty() {
        println!("  仍是空的（程序文件夹里没识别出任何合规方案）");
    } else {
        println!("  已存在，未改动：{} 项", manifest.schemas.len());
    }
    println!("  active: {}", manifest.active.as_deref().unwrap_or("<无>"));
    for id in &manifest.schemas {
        println!("    - {id}");
    }
}

/// `dicts`：词库识别（`<数据根>/dict/**/*.dict.yaml`），与 `manifest` 同构。
///
/// 四关与方案完全同一套：文件头标记 → 实际编码/换行 → 顶层 `id:` 单数写法/内容/
/// 与文件名一致 → 系列摆放（`dict/<id 按点分段>/…`）。只报告，不写任何清单。
fn dicts_cli(flags: &[String]) {
    use librime_next::lever::schema_list;

    let mut shared: Option<PathBuf> = None;
    let mut user: Option<PathBuf> = None;
    let mut i = 0;
    while i < flags.len() {
        match flags[i].as_str() {
            "--shared" | "--root" => {
                i += 1;
                shared = flags.get(i).map(PathBuf::from);
                if shared.is_none() {
                    eprintln!("--shared 后面要跟目录");
                    std::process::exit(2);
                }
            }
            "--user" => {
                i += 1;
                user = flags.get(i).map(PathBuf::from);
                if user.is_none() {
                    eprintln!("--user 后面要跟目录");
                    std::process::exit(2);
                }
            }
            other => {
                eprintln!("不认识的参数: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let user = user
        .or_else(|| std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("Rime-Next")))
        .unwrap_or_else(|| PathBuf::from("."));
    let shared = shared.unwrap_or_else(default_shared_dir);
    println!("程序文件夹（shared）: {}", shared.display());
    println!("用户文件夹（user）  : {}", user.display());

    let found = schema_list::discover_roots_dicts(&[shared.clone(), user.clone()]);
    println!(
        "\n== 词库识别：收下 {} / 抛弃 {} ==",
        found.accepted.len(),
        found.rejected.len()
    );
    for item in &found.accepted {
        println!(
            "  [收下] {:<24} {}  ({})",
            item.id,
            item.name.as_deref().unwrap_or(""),
            item.path.display()
        );
    }
    for (path, reason) in &found.rejected {
        println!("  [抛弃] {}\n         └─ {reason}", path.display());
    }
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
