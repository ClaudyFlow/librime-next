//! 隔离端到端探针：一个方案内置「全拼 / 自然码 / 小鹤 / 注音」四种输入类型。
//!
//! 做两件事：
//! 1. 用 [`DictCompiler`] 在**隔离 build 目录**里，基于已有的 `pinyin.table.bin`
//!    现编三份额外 prism（`pinyin.ziranma` / `pinyin.flypy` / `pinyin.zhuyin`）；
//! 2. 起一个 [`Engine`]，依次切换四种输入类型并输入对应的键，打印候选。
//!
//! 全程只读写参数给的隔离目录，不碰 `%APPDATA%`。
//!
//! 用法：
//! ```text
//! cargo run --release --example probe_multi_input_e2e -- <隔离根> <Rime-Next/schema>
//! ```
//! `<隔离根>` 下需要：`build/pinyin/{pinyin.table.bin,pinyin.prism.bin,pinyin.reverse.bin}`、
//! `build/pinyin.multi/pinyin.multi.schema.yaml`；`user/` 会按需创建。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use librime_next::gear::dict::dict_compiler::{CompileOptions, DictCompiler};
use librime_next::gear::dict::prism::Prism;
use librime_next::engine::key_event::KeyEvent;
use librime_next::engine::runtime::Engine;
use librime_next::load_file;

fn algebra_of(path: &Path) -> Vec<String> {
    let value = load_file(path).unwrap_or_else(|e| panic!("读 {}: {e}", path.display()));
    value
        .pointer("/speller/algebra")
        .and_then(|node| node.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_else(|| panic!("{} 没有 speller/algebra", path.display()))
}

/// `key` 在 `name` 这份 prism 里指向的**音节 id**（拼写 id → descriptor → 音节 id）。
fn syllable_of(build: &Path, name: &str, key: &str) -> Option<i32> {
    let path = build.join(format!("pinyin/{name}.prism.bin"));
    let prism = Prism::open(&path).unwrap_or_else(|e| panic!("开 {}: {e}", path.display()));
    let spelling_id = prism.get_value(key)?;
    Some(prism.query_spelling(spelling_id).syllable_id())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = PathBuf::from(
        args.first()
            .expect("用法: probe_multi_input_e2e <隔离根> <Rime-Next/schema>"),
    );
    let schema_root = PathBuf::from(args.get(1).expect("缺 Rime-Next/schema 根"));
    let build = root.join("build");
    let user_dir = root.join("user");
    std::fs::create_dir_all(&user_dir).expect("建 user 目录");
    let schema_file = build.join("pinyin.multi/pinyin.multi.schema.yaml");

    // ---- 1. 现编三份额外 prism（复用主表，不重建表） ----
    let mut extra: BTreeMap<String, Vec<String>> = BTreeMap::new();
    extra.insert(
        "pinyin.ziranma".to_string(),
        algebra_of(&schema_root.join("double_pinyin/ziranma/double_pinyin.ziranma.schema.yaml")),
    );
    extra.insert(
        "pinyin.flypy".to_string(),
        algebra_of(&schema_root.join("double_pinyin/flypy/double_pinyin.flypy.schema.yaml")),
    );
    extra.insert(
        "pinyin.zhuyin".to_string(),
        algebra_of(&schema_root.join("zhuyin/zhuyin.schema.yaml")),
    );
    let mut compiler = DictCompiler::new(
        "pinyin",
        "pinyin",
        Vec::new(),
        &root,
        &root,
        &build,
        &build,
    );
    compiler.set_extra_prisms(extra);
    compiler.set_options(CompileOptions {
        rebuild_table: false,
        rebuild_prism: false,
    });
    compiler.compile(Some(&schema_file)).expect("编译额外 prism");
    println!("== 额外 prism 已生成 ==");
    for name in ["pinyin", "pinyin.ziranma", "pinyin.flypy", "pinyin.zhuyin"] {
        let path = build.join(format!("pinyin/{name}.prism.bin"));
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        println!("  {name:18} {} B  {}", size, path.display());
    }

    // ---- 2. 音节 id 对齐检查：同一「中 / 国」在四份 prism 里必须是同一个 id ----
    let zhong = syllable_of(&build, "pinyin", "zhong").expect("主 prism 无 zhong");
    let guo = syllable_of(&build, "pinyin", "guo").expect("主 prism 无 guo");
    let checks = [
        ("自然码", "pinyin.ziranma", "vs", "go", zhong, guo),
        ("小鹤", "pinyin.flypy", "vs", "go", zhong, guo),
        ("注音", "pinyin.zhuyin", "5j/", "eji", zhong, guo),
    ];
    println!("\n== prism 音节 id 对齐（主 prism: zhong={zhong}, guo={guo}）==");
    for (label, prism, k1, k2, want1, want2) in checks {
        let a = syllable_of(&build, prism, k1);
        let b = syllable_of(&build, prism, k2);
        println!(
            "  {label:6} {k1:5} -> {:?}    {k2:5} -> {:?}   {}",
            a,
            b,
            if a == Some(want1) && b == Some(want2) {
                "OK"
            } else {
                "不一致"
            }
        );
    }

    // ---- 3. 引擎端到端 ----
    // 第二个数据根（只读）用来让 `simplifier` 找到 `opencc/`：仓库里的 Rime-Next 目录。
    let shared = schema_root.parent().expect("schema 根的上级应存在").to_path_buf();
    let Some(mut engine) =
        Engine::create_with_roots(&build, "pinyin.multi", &[user_dir.clone(), shared.clone()])
    else {
        panic!("Engine 装配失败（方案 id = pinyin.multi）");
    };
    println!("\n== 引擎：{} / {} ==", engine.schema_id(), engine.schema_name());

    let cases: [(&str, Option<&str>, &str); 4] = [
        ("全拼（主 prism）", None, "zhongguo"),
        ("自然码", Some("double_ziranma"), "vsgo"),
        ("小鹤", Some("double_flypy"), "vsgo"),
        ("注音", Some("zhuyin"), "5j/eji"),
    ];
    let all_options = ["double_ziranma", "double_flypy", "zhuyin"];

    for (label, option, keys) in cases {
        for name in all_options {
            engine.set_option(name, Some(name) == option);
        }
        for ch in keys.chars() {
            engine.process_key(&KeyEvent::new(ch as u32, 0));
        }
        let comp = engine.composition();
        let input = comp.input().to_string();
        let texts: Vec<String> = comp
            .back()
            .and_then(|seg| seg.menu.as_ref())
            .map(|menu| {
                menu.all_candidates()
                    .into_iter()
                    .take(6)
                    .map(|c| c.text)
                    .collect()
            })
            .unwrap_or_default();
        let hit = texts.iter().any(|t| t == "中国");
        println!(
            "  {label:10} keys={keys:9} input={input:9} 中国={} 候选={:?}",
            if hit { "有" } else { "无" },
            texts
        );
        // 清掉组合串，避免影响下一轮（切 option 时也会清，这里是双保险）
        engine.process_key(&KeyEvent::new(0xff1b, 0));
    }

    // ---- 4. 用户词跨输入类型：自然码里选第 3 个候选上屏，再回全拼/注音看它是否升到首位 ----
    fn top(engine: &mut Engine, n: usize) -> Vec<String> {
        engine
            .composition()
            .back()
            .and_then(|seg| seg.menu.as_ref())
            .map(|menu| menu.all_candidates().into_iter().take(n).map(|c| c.text).collect())
            .unwrap_or_default()
    }
    fn type_keys(engine: &mut Engine, keys: &str) {
        for ch in keys.chars() {
            engine.process_key(&KeyEvent::new(ch as u32, 0));
        }
    }

    for name in all_options {
        engine.set_option(name, name == "double_ziranma");
    }
    type_keys(&mut engine, "vsgo");
    let before = top(&mut engine, 6);
    // Down = Selector 的 NextCandidate（不依赖数字键，注音方案里数字被 alphabet 吃掉）
    engine.process_key(&KeyEvent::new(0xff54, 0));
    engine.process_key(&KeyEvent::new(0xff54, 0));
    let picked = engine
        .composition()
        .back()
        .and_then(|seg| seg.get_selected_candidate())
        .map(|c| c.text)
        .unwrap_or_default();
    engine.process_key(&KeyEvent::new(b' ' as u32, 0));
    engine.apply_commit(); // 宿主上屏后必调：memorize 在这里写 userdb
    let db = user_dir.join("userdb/pinyin.userdb");
    println!(
        "\n== 用户词学习（自然码上屏「{picked}」）==\n  上屏前候选={before:?}\n  上屏后 input={:?}  userdb={:?}",
        engine.composition().input(),
        std::fs::metadata(&db).map(|m| m.len())
    );

    for (label, option, keys) in [
        ("全拼", None, "zhongguo"),
        ("注音", Some("zhuyin"), "5j/eji"),
    ] {
        for name in all_options {
            engine.set_option(name, Some(name) == option);
        }
        type_keys(&mut engine, keys);
        let texts = top(&mut engine, 6);
        println!(
            "  {label:4} 重打 {keys:9} input={:9} → {:?}{}",
            engine.composition().input(),
            texts,
            if texts.first().map(String::as_str) == Some(picked.as_str()) {
                "   ← 学到的词已升到首位"
            } else {
                ""
            }
        );
    }

    // ---- 5. 出厂注音方案（dict pinyin.terra + prism zhuyin + user_dict: pinyin）----
    // 验证「只改 userdb 指向」就能与拼音族学词互通：同一本 userdb，独立 table/prism。
    drop(engine);
    // 诊断：出厂 zhuyin.prism.bin 与 pinyin.terra.table.bin 是否对得上（v5 无内嵌
    // syllabary，id 直接是表里的下标，错位就会出乱码候选）。
    {
        use librime_next::gear::dict::table::Table;
        let zp = Prism::open(build.join("zhuyin/zhuyin.prism.bin")).expect("开 zhuyin prism");
        let tp = Prism::open(build.join("pinyin/terra/pinyin.terra.prism.bin"))
            .expect("开 terra prism");
        let zid = zp
            .get_value("5j/")
            .map(|id| zp.query_spelling(id).syllable_id());
        let tid = tp
            .get_value("zhong")
            .map(|id| tp.query_spelling(id).syllable_id());
        let table = Table::open(build.join("pinyin/terra/pinyin.terra.table.bin"))
            .expect("开 terra table");
        println!(
            "  诊断 zhuyin: has_key(5j/)={} 5j/→{:?}  terra zhong→{:?}  checksum(prism/table)={}/{}",
            zp.has_key("5j/"),
            zid,
            tid,
            zp.dict_file_checksum(),
            table.dict_file_checksum()
        );
        let ps = zp.syllabary().map(|s| s.len());
        let ts = table.syllabary_ordered().map(|s| s.len());
        println!(
            "  版本 prism={:?} table={:?}  音节数 prism(v6 union)={:?} table={:?}",
            zp.metadata().and_then(|m| m.format_version()),
            table.metadata().and_then(|m| m.format_version()),
            ps,
            ts
        );
        if let Some(order) = table.syllabary_ordered() {
            let i = zid.unwrap_or(0) as usize;
            println!(
                "  table 第 {i} 个音节 = {:?}（前后 {:?}）  terra 'zhong' 的局部 id = {:?}",
                order.get(i),
                order.get(i.saturating_sub(2)..(i + 3).min(order.len())),
                order.iter().position(|s| s == "zhong")
            );
        }
        if let Some(union) = zp.syllabary() {
            let i = zid.unwrap_or(0) as usize;
            println!(
                "  prism 第 {i} 个音节 = {:?}  len={}",
                union.get(i),
                union.len()
            );
        }
        // 直接看 terra 表里该音节桶：到底是词典本身生僻，还是 id 错位。
        let mut words = table.query_words(zid.unwrap_or(0));
        let mut dump: Vec<(String, f32)> = Vec::new();
        let mut zhong_at = None;
        let mut index = 0usize;
        loop {
            if let Some(entry) = words.entry() {
                let text = table.entry_text(&entry);
                if zhong_at.is_none() && text == "中" {
                    zhong_at = Some(index);
                }
                if dump.len() < 8 {
                    dump.push((text, entry.weight));
                }
            }
            index += 1;
            if index > 5000 || !words.next() {
                break;
            }
        }
        println!(
            "  terra 表 id={:?} 共 {index} 条，头几条(text,weight)={dump:?}，「中」在第 {zhong_at:?} 条",
            zid
        );
    }
    let Some(mut zhuyin_engine) =
        Engine::create_with_roots(&build, "zhuyin", &[user_dir.clone(), shared.clone()])
    else {
        println!("\n== 出厂注音方案装配失败（跳过）==");
        return;
    };
    for keys in ["5j/", "eji", "5j/eji"] {
        for ch in keys.chars() {
            zhuyin_engine.process_key(&KeyEvent::new(ch as u32, 0));
        }
        let rows: Vec<(String, String)> = zhuyin_engine
            .composition()
            .back()
            .and_then(|seg| seg.menu.as_ref())
            .map(|menu| {
                menu.all_candidates()
                    .into_iter()
                    .take(4)
                    .map(|c| (c.text, c.preedit))
                    .collect()
            })
            .unwrap_or_default();
        println!(
            "\n== 出厂注音（pinyin.terra + user_dict: pinyin）==\n  keys={keys:8} → {rows:?}{}",
            if rows.first().map(|(t, _)| t.as_str()) == Some(picked.as_str()) {
                "   ← 与拼音族共用用户词"
            } else {
                ""
            }
        );
        zhuyin_engine.process_key(&KeyEvent::new(0xff1b, 0));
    }
}
