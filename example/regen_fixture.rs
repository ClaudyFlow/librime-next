//! 一次性工具：用**当前**编译器重生成 test/e2e/build 里的旧格式词典二进制。
//!
//! 背景：2026-11-15 字符串表从 marisa 切双数组 DAT（`.table.bin` / `.reverse.bin`
//! 内嵌的字符串段换格式），仓库里提交的 e2e fixture 二进制还是旧 marisa 格式，
//! 读端 `StringTable::from_bytes` 解不了 → 一批集成测试挂。这个工具从源
//! `.dict.yaml` 重新编译三份产物并拷回 fixture 目录：
//!
//! * `luna_pinyin.table.bin` + `luna_pinyin.reverse.bin`（源 = `.pr-review-librime/data/minimal`）
//! * `stroke.table.bin`（源 = `Rime-Next/dict/stroke`）
//!
//! 用法：`cargo run --example regen_fixture -- [--dry-run]`

use std::path::{Path, PathBuf};

use librime_next::config;
use librime_next::gear::dict::dict_compiler::{
    relocate_target, CompileOptions, DictCompiler,
};

const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");
const E2E_BUILD: &str = "test/e2e/build";

fn compile_one(
    name: &str,
    prism_name: &str,
    schema_source: &Path,
    defaults: &Path,
    source_root: &Path,
    out_dir: &str,
) -> Result<Vec<PathBuf>, String> {
    // ① 配置编译：源 schema → 编译后 schema（DictCompiler 只吃编译产物）。
    //    schema_source 已经是编译产物（fixture build 里的 *.schema.yaml）时，
    //    config::compile 也无害（纯 YAML 直通）。
    let tree = config::compile(schema_source, defaults, &[source_root.to_path_buf()])
        .map_err(|e| format!("{name}: 配置编译失败: {e}"))?;
    let yaml = config::to_yaml_string(&tree).map_err(|e| format!("{name}: 发射失败: {e}"))?;
    let compiled_schema = std::path::Path::new(out_dir).join(format!("{name}.{prism_name}.schema.compiled.yaml"));
    std::fs::write(&compiled_schema, &yaml)
        .map_err(|e| format!("{name}: 写编译 schema 失败: {e}"))?;

    // ② 词典编译：强制重建，产物进独立 staging（不碰 fixture 原件）
    let staging = std::path::Path::new(out_dir).join(format!("staging-{name}-{prism_name}"));
    std::fs::create_dir_all(&staging).map_err(|e| format!("{name}: staging: {e}"))?;
    let mut compiler = DictCompiler::new(
        name,
        prism_name,
        Vec::new(),
        "", // user_data_dir：空（源全在 shared）
        source_root,
        &staging,
        "", // prebuilt：禁用复用
    );
    compiler.set_options(CompileOptions::rebuild_all());
    compiler
        .compile(Some(&compiled_schema))
        .map_err(|e| format!("{name}: 词典编译失败: {e}"))?;

    // ③ 收集产物（产物按 id 逐级目录落盘，用 relocate_target 对齐）
    let mut produced = Vec::new();
    for suffix in ["table.bin", "reverse.bin", "prism.bin"] {
        let file = if suffix == "prism.bin" {
            format!("{prism_name}.{suffix}")
        } else {
            format!("{name}.{suffix}")
        };
        let path = relocate_target(&staging, &file);
        if path.is_file() {
            produced.push(path);
        }
    }
    Ok(produced)
}

fn main() {
    let dry_run = std::env::args().any(|arg| arg == "--dry-run");
    let build = PathBuf::from(MANIFEST).join(E2E_BUILD);
    let work = std::env::temp_dir().join(format!("regen-fixture-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("建工作目录");

    // ---- 源准备 ------------------------------------------------------------
    // luna_pinyin：上游 minimal 数据集（fixture 当初就是从它编的）。
    // classified_dict_source 只认 <root>/dict/<name>/<name>.dict.yaml，所以拷一层。
    let minimal = PathBuf::from(r"C:\code\Weasel-Next\.pr-review-librime\data\minimal");
    let luna_root = work.join("luna-root");
    let luna_dict_dir = luna_root.join("dict").join("luna_pinyin");
    std::fs::create_dir_all(&luna_dict_dir).expect("建 luna dict 目录");
    std::fs::copy(minimal.join("luna_pinyin.dict.yaml"), luna_dict_dir.join("luna_pinyin.dict.yaml"))
        .expect("拷 luna_pinyin.dict.yaml");
    std::fs::copy(minimal.join("luna_pinyin.schema.yaml"), luna_root.join("luna_pinyin.schema.yaml"))
        .expect("拷 luna_pinyin.schema.yaml");

    let rime_next = PathBuf::from(r"C:\code\Weasel-Next\Rime-Next");

    // ---- 编译 --------------------------------------------------------------
    // ⚠️ table 与 prism 必须成对重编：prism 的值 = 音节 id，换了表 id 全变。
    //    fixture 里的 luna 派生 prism（simp / flypy / ziranma）与 stroke.prism.bin
    //    都要跟着各自的表一起重出。
    let mut plan: Vec<(String, Vec<PathBuf>)> = Vec::new();

    // luna_pinyin 主表 / reverse（用主 schema 编一次）
    match compile_one(
        "luna_pinyin",
        "luna_pinyin",
        &luna_root.join("luna_pinyin.schema.yaml"),
        &minimal.join("default.yaml"),
        &luna_root,
        work.to_string_lossy().as_ref(),
    ) {
        Ok(files) => plan.push(("luna_pinyin".into(), files)),
        Err(e) => eprintln!("✗ {e}"),
    }
    // luna 派生 prism：schema 用 fixture build 里已编译的产物
    for prism in ["luna_pinyin_simp", "double_pinyin_flypy", "double_pinyin_ziranma"] {
        match compile_one(
            "luna_pinyin",
            prism,
            &build.join(format!("{prism}.schema.yaml")),
            &minimal.join("default.yaml"),
            &luna_root,
            work.to_string_lossy().as_ref(),
        ) {
            Ok(files) => plan.push((format!("luna_pinyin@{prism}"), files)),
            Err(e) => eprintln!("✗ {e}"),
        }
    }
    // stroke：表 + prism 成对
    match compile_one(
        "stroke",
        "stroke",
        &rime_next.join("schema").join("stroke").join("stroke.schema.yaml"),
        &rime_next.join("default.yaml"),
        &rime_next,
        work.to_string_lossy().as_ref(),
    ) {
        Ok(files) => plan.push(("stroke".into(), files)),
        Err(e) => eprintln!("✗ {e}"),
    }

    // ---- 拷回 fixture（只换内嵌字符串表的产物及其派生 prism） ----
    const WANTED: [&str; 7] = [
        "luna_pinyin.table.bin",
        "luna_pinyin.reverse.bin",
        "luna_pinyin_simp.prism.bin",
        "double_pinyin_flypy.prism.bin",
        "double_pinyin_ziranma.prism.bin",
        "stroke.table.bin",
        "stroke.prism.bin",
    ];
    let mut copied: Vec<String> = Vec::new();
    for (_name, files) in &plan {
        for file in files {
            let file_name = file.file_name().unwrap().to_string_lossy().to_string();
            if !WANTED.contains(&file_name.as_str()) || copied.contains(&file_name) {
                continue;
            }
            copied.push(file_name.clone());
            let target = build.join(&file_name);
            println!(
                "{}  {}  ({})",
                if dry_run { "DRY" } else { "COPY" },
                target.display(),
                std::fs::metadata(file).map(|m| m.len()).unwrap_or(0)
            );
            if !dry_run {
                std::fs::copy(file, &target).expect("拷贝产物");
            }
        }
    }
    let missing: Vec<&str> = WANTED
        .iter()
        .filter(|w| !copied.contains(&w.to_string()))
        .copied()
        .collect();
    if !missing.is_empty() {
        eprintln!("✗ 缺产物：{missing:?}");
        std::process::exit(1);
    }
    if dry_run {
        println!("（dry-run：未写任何文件）");
    }
}
