//! 隔离端到端探针：把 evo 的双拼拼式装进统一 `pinyin` 方案后，按输入类型切棱镜实打出词。
//!
//! 复用已编译好的 `build/pinyin`（table / prism / reverse），只现编新增的两份棱镜，
//! 不做 996k 词库的冷编译。
//!
//! 用法：
//! ```text
//! cargo run --release --example probe_evo_subtypes -- <隔离根> <Rime-Next/schema> <已编译 build 目录>
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use librime_next::algo::algebra::{Projection, Script};
use librime_next::gear::dict::dict_compiler::{CompileOptions, DictCompiler};
use librime_next::gear::dict::prism::Prism;
use librime_next::engine::key_event::KeyEvent;
use librime_next::engine::runtime::Engine;
use librime_next::load_file;

const TYPES: [&str; 2] = ["sogou", "znabc"];
const ALL: [&str; 7] = ["pinyin", "terra", "zrm", "flypy", "zhuyin", "sogou", "znabc"];

fn extra_prisms(schema: &Path) -> BTreeMap<String, Vec<String>> {
    let value = load_file(schema).unwrap_or_else(|e| panic!("读 {}: {e}", schema.display()));
    let mut out = BTreeMap::new();
    for key in TYPES {
        let items = value
            .pointer(&format!("/build/dictionaries/pinyin/algebra/{key}"))
            .and_then(|node| node.as_array())
            .unwrap_or_else(|| panic!("{} 缺 build/.../algebra/{key}", schema.display()));
        out.insert(
            format!("pinyin.{key}"),
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect(),
        );
    }
    out
}

fn type_keys(engine: &mut Engine, keys: &str) -> String {
    for ch in keys.chars() {
        engine.process_key(&KeyEvent::new(ch as u32, 0));
    }
    engine.composition().input().to_string()
}

fn cands(engine: &mut Engine, n: usize) -> Vec<String> {
    engine
        .composition()
        .back()
        .and_then(|seg| seg.menu.as_ref())
        .map(|menu| {
            menu.all_candidates()
                .into_iter()
                .take(n)
                .map(|c| c.text)
                .collect()
        })
        .unwrap_or_default()
}

fn activate(engine: &mut Engine, active: &str) {
    engine.process_key(&KeyEvent::new(0xff1b, 0));
    for t in ALL {
        engine.set_option(t, t == active);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = PathBuf::from(
        args.first()
            .expect("用法: probe_evo_subtypes <隔离根> <schema 根> <已编译 build 目录>"),
    );
    let schema_root = PathBuf::from(args.get(1).expect("缺 Rime-Next/schema 根"));
    let src_build = PathBuf::from(args.get(2).expect("缺已编译 build 目录"));
    let build = root.join("build");
    let user = root.join("user");
    std::fs::create_dir_all(build.join("pinyin")).expect("建 build/pinyin");
    std::fs::create_dir_all(&user).expect("建 user");

    for name in ["pinyin.table.bin", "pinyin.prism.bin", "pinyin.reverse.bin"] {
        std::fs::copy(
            src_build.join("pinyin").join(name),
            build.join("pinyin").join(name),
        )
        .unwrap_or_else(|e| panic!("拷 {name}: {e}"));
    }
    std::fs::copy(src_build.join("default.yaml"), build.join("default.yaml")).ok();
    if let Ok(entries) = std::fs::read_dir(src_build.join("pinyin")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".prism.bin") && name != "pinyin.prism.bin" {
                std::fs::copy(entry.path(), build.join("pinyin").join(&name)).ok();
            }
        }
    }
    let schema_file = build.join("pinyin/pinyin.schema.yaml");
    std::fs::copy(
        schema_root.join("pinyin/pinyin.schema.evo.yaml"),
        &schema_file,
    )
    .unwrap_or_else(|e| panic!("拷 evo schema: {e}"));

    let extra = extra_prisms(&schema_file);
    for key in TYPES {
        let mut projection = Projection::from_rules(&extra[&format!("pinyin.{key}")])
            .unwrap_or_else(|e| panic!("{key} algebra: {e}"));
        let mut cells = Vec::new();
        for syllable in ["zhong1", "guo2", "a1", "er2", "ju4", "de5", "ying1", "shuang1"] {
            let mut script = Script::new();
            script.add_syllable(syllable);
            projection.apply_script(&mut script);
            let mut keys: Vec<String> = script.keys().cloned().collect();
            keys.sort();
            cells.push(format!("{syllable}→{keys:?}"));
        }
        println!("  {key} 代数：{}", cells.join("  "));
    }
    let mut compiler =
        DictCompiler::new("pinyin", "pinyin", Vec::new(), &root, &root, &build, &build);
    compiler.set_extra_prisms(extra);
    compiler.set_options(CompileOptions {
        rebuild_table: false,
        rebuild_prism: false,
    });
    compiler.compile(Some(&schema_file)).expect("编译新增 prism");

    for key in TYPES {
        let path = build.join(format!("pinyin/pinyin.{key}.prism.bin"));
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        println!("  pinyin.{key:6} {size:8} B  {}", path.display());
    }
    for entry in std::fs::read_dir(build.join("pinyin")).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".prism.bin") {
            continue;
        }
        let mut prism = Prism::open(entry.path()).expect("打开 prism");
        prism.load().expect("加载 prism");
        let mut found = String::new();
        for probe in ["vs", "vs1", "zhong1", "ip", "iy", "as", "as1"] {
            if let Some(id) = prism.get_value(probe) {
                found.push_str(&format!(" {probe}={:?}", prism.syllable_by_id(id)));
            }
        }
        println!("    键位探测 {name}:{found}");
        if name == "pinyin.sogou.prism.bin" || name == "pinyin.zrm.prism.bin" {
            for probe in ["vs", "vs1", "vs2", "vs3", "vs4", "go", "go1", "go2", "go3", "go4"] {
                for m in prism.common_prefix_search(probe) {
                    if m.length != probe.len() {
                        continue;
                    }
                    let mut spelling = prism.query_spelling(m.value);
                    let mut ids = Vec::new();
                    while !spelling.exhausted() {
                        if let Some(d) = spelling.descriptor() {
                            ids.push(d.syllable_id);
                        }
                        spelling.next();
                    }
                    ids.sort();
                    println!("      {probe} → {ids:?}");
                }
            }
            // 音节图：看 edges[start][end] 里到底有哪些音节 id
            let mut syllabifier =
                librime_next::algo::syllabifier::Syllabifier::with_options("' ", true, false, None);
            let mut graph = librime_next::algo::syllabifier::SyllableGraph::default();
            let consumed = syllabifier.build_syllable_graph("vsgo", &prism, &mut graph);
            println!("      图 vsgo consumed={consumed} vertices={:?}", graph.vertices());
            for (start, ends) in &graph.edges {
                for (end, spellings) in ends {
                    let mut ids: Vec<i32> = spellings.keys().copied().collect();
                    ids.sort();
                    let names: Vec<String> = ids
                        .iter()
                        .map(|id| prism.syllable_by_id(*id).unwrap_or_else(|| format!("?{id}")))
                        .collect();
                    println!("        {start}->{end}: {ids:?} {names:?}");
                }
            }
            for one in ["v", "o", "s", "g"] {
                for m in prism.common_prefix_search(one) {
                    if m.length != 1 {
                        continue;
                    }
                    let mut sp = prism.query_spelling(m.value);
                    let mut ids = Vec::new();
                    while !sp.exhausted() {
                        if let Some(d) = sp.descriptor() {
                            ids.push((
                                d.syllable_id,
                                prism.syllable_by_id(d.syllable_id).unwrap_or_default(),
                            ));
                        }
                        sp.next();
                    }
                    ids.sort();
                    println!("        1字母 {one} → {ids:?}");
                }
            }
        }
    }

    let shared = schema_root.parent().expect("schema 根的上级").to_path_buf();
    let Some(mut engine) = Engine::create_with_roots(&build, "pinyin", &[user, shared]) else {
        panic!("Engine 装配失败（方案 id = pinyin）");
    };
    println!("== 引擎：{} / {} ==", engine.schema_id(), engine.schema_name());

    for (kind, keys) in [
        ("pinyin", "zhong"),
        ("pinyin", "guo"),
        ("zrm", "vs"),
        ("zrm", "go"),
        ("sogou", "vs"),
        ("sogou", "go"),
        ("zrm", "vsgo"),
        ("sogou", "vsgo"),
        ("sogou", "vs'go"),
        ("sogou", "vsgo2"),
        ("sogou", "vs1go"),
    ] {
        activate(&mut engine, kind);
        type_keys(&mut engine, keys);
        println!("  [dump] {kind:6} {keys:6} → {:?}", cands(&mut engine, 64));
    }

    let cases: [(&str, &str, &str, bool); 9] = [
        ("pinyin", "zhongguo", "中国", true),
        ("zrm", "vsgo", "中国", true),
        ("sogou", "vs1go2", "中国", true),
        ("sogou", "vsgo", "中国", true),
        ("sogou", "vs-", "中", true),
        ("sogou", "vs1go1", "中国", false),
        ("znabc", "as1go2", "中国", true),
        ("znabc", "asgo", "中国", true),
        ("znabc", "as-", "中", true),
    ];
    let mut failed = 0;
    for (kind, keys, want, expect_hit) in cases {
        activate(&mut engine, kind);
        let input = type_keys(&mut engine, keys);
        let texts = cands(&mut engine, 12);
        let hit = texts.iter().any(|c| c == want);
        let ok = hit == expect_hit;
        if !ok {
            failed += 1;
        }
        println!(
            "[{}] {kind:6} keys={keys:8} input={input:9} 期望{} {want:?} 候选={texts:?}",
            if ok { "OK" } else { "FAIL" },
            if expect_hit { "有" } else { "无" },
        );
        if !ok {
            if let Some(seg) = engine.composition().back() {
                println!(
                    "        段 {:?} status={:?} tags={:?}",
                    seg.start..seg.end,
                    seg.status,
                    seg.tags
                );
            }
        }
    }

    // 选重：sogou 打完 vsgo 后按两次 1（第一次作声调进编码，第二次回落 selector 选重）
    activate(&mut engine, "sogou");
    type_keys(&mut engine, "vsgo");
    let first = cands(&mut engine, 1);
    engine.process_key(&KeyEvent::new('1' as u32, 0));
    let after_one = engine.composition().input().to_string();
    engine.process_key(&KeyEvent::new('1' as u32, 0));
    let after_two = engine.composition().input().to_string();
    println!("  选重 sogou vsgo: 首选={first:?} 按1后 input={after_one:?} 再按1后 input={after_two:?}");
    assert!(after_one.ends_with('1'), "第一次按 1 应作声调进编码");
    assert!(after_two.is_empty(), "第二次按 1 应选中并上屏，实得 {after_two:?}");

    assert_eq!(failed, 0, "{failed} 条用例失败");
    println!("== 全部通过 ==");
}
