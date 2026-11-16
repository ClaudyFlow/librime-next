//! 隔离端到端探针：**出厂方案族**（拼音 = 全拼 父方案 + 自然码 / 小鹤 / 注音 三个子方案）。
//!
//! 做四件事：
//! 1. 把 `examples/pinyin*.schema.yaml` 拷进隔离 build，并按各自 algebra 现编 prism；
//! 2. 四个 schema 各自起引擎，验证都能切换、都能用对应键位打出「中国」；
//! 3. 在自然码里学一个词，验证另外三套（含注音）都能看到它（共用 pinyin.userdb）；
//! 4. 在 `pinyin` 引擎上按 Ctrl+` 打开选单，验证「先主后子」两级流程（Esc 返回、第二级再选才切换）。
//!
//! 全程只读写参数给的隔离目录，不碰 `%APPDATA%`。
//!
//! 用法：
//! ```text
//! cargo run --release --example probe_family_e2e -- <隔离根> <Rime-Next/schema> <examples 目录>
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use librime_next::gear::dict::dict_compiler::{CompileOptions, DictCompiler};
use librime_next::engine::key_event::{KeyEvent, K_CONTROL_MASK};
use librime_next::engine::runtime::Engine;
use librime_next::load_file;

const FAMILY: [&str; 4] = ["pinyin", "pinyin.ziranma", "pinyin.flypy", "pinyin.zhuyin"];
const XK_ESCAPE: u32 = 0xff1b;

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

fn top(engine: &mut Engine, n: usize) -> Vec<String> {
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

fn rank_of(engine: &mut Engine, keys: &str, word: &str) -> Option<usize> {
    type_keys(engine, keys);
    let rank = engine
        .composition()
        .back()
        .and_then(|seg| seg.menu.as_ref())
        .and_then(|menu| {
            menu.all_candidates()
                .into_iter()
                .position(|c| c.text == word)
        });
    engine.process_key(&KeyEvent::new(XK_ESCAPE, 0));
    rank
}

fn type_keys(engine: &mut Engine, keys: &str) {
    for ch in keys.chars() {
        engine.process_key(&KeyEvent::new(ch as u32, 0));
    }
}

fn selected(engine: &Engine) -> Option<String> {
    engine
        .composition()
        .back()
        .and_then(|seg| seg.get_selected_candidate())
        .map(|c| c.text.clone())
}

fn menu_texts(engine: &mut Engine) -> (Vec<String>, usize) {
    let segs = &engine.context.composition.segmentation.segments;
    match segs.last() {
        Some(seg) => {
            let texts = seg
                .menu
                .as_ref()
                .map(|m| m.all_candidates().into_iter().map(|c| c.text).collect())
                .unwrap_or_default();
            (texts, seg.selected_index)
        }
        None => (Vec::new(), 0),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = PathBuf::from(args.first().expect("用法: <隔离根> <schema 根> <examples 目录>"));
    let schema_root = PathBuf::from(args.get(1).expect("缺 Rime-Next/schema 根"));
    let examples = PathBuf::from(args.get(2).expect("缺 examples 目录"));
    let build = root.join("build");
    let user_dir = root.join("user");
    std::fs::create_dir_all(&user_dir).expect("建 user 目录");
    let shared = schema_root.parent().expect("schema 根的上级应存在").to_path_buf();

    // ---- 0. 把族 schema 放进隔离 build ----
    for id in FAMILY {
        let dir = build.join(id);
        std::fs::create_dir_all(&dir).expect("建方案目录");
        std::fs::copy(
            examples.join(format!("{id}.schema.yaml")),
            dir.join(format!("{id}.schema.yaml")),
        )
        .unwrap_or_else(|e| panic!("拷 {id}.schema.yaml: {e}"));
    }
    // 方案清单（Ctrl+` 的数据源）
    std::fs::write(
        build.join("default.yaml"),
        format!(
            "switcher:\n  caption: \"〔方案〕\"\n  hotkeys: [\"Control+grave\"]\nschema_list:\n{}\n",
            FAMILY
                .iter()
                .map(|id| format!("  - schema: {id}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    )
    .expect("写 default.yaml");

    // ---- 1. 现编三份额外 prism（复用主表） ----
    let mut extra: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (prism, file) in [
        ("pinyin.ziranma", "pinyin.ziranma.schema.yaml"),
        ("pinyin.flypy", "pinyin.flypy.schema.yaml"),
        ("pinyin.zhuyin", "pinyin.zhuyin.schema.yaml"),
    ] {
        extra.insert(prism.to_string(), algebra_of(&examples.join(file)));
    }
    let main_schema = build.join("pinyin/pinyin.schema.yaml");
    let mut compiler = DictCompiler::new("pinyin", "pinyin", Vec::new(), &root, &root, &build, &build);
    compiler.set_extra_prisms(extra);
    compiler.set_options(CompileOptions {
        rebuild_table: false,
        rebuild_prism: false,
    });
    compiler.compile(Some(&main_schema)).expect("编译额外 prism");

    // prism 查找路径是 `build/<点号转斜杠>/<名>.prism.bin`；把编好的复制过去。
    for (prism, group) in [
        ("pinyin.ziranma", "ziranma"),
        ("pinyin.flypy", "flypy"),
        ("pinyin.zhuyin", "zhuyin"),
    ] {
        let dst_dir = build.join("pinyin").join(group);
        std::fs::create_dir_all(&dst_dir).expect("建 prism 目录");
        std::fs::copy(
            build.join(format!("pinyin/{prism}.prism.bin")),
            dst_dir.join(format!("{prism}.prism.bin")),
        )
        .unwrap_or_else(|e| panic!("拷 {prism}: {e}"));
    }
    println!("== 族 schema / prism 就绪 ==");

    // ---- 2. 四套 schema 各自出词 ----
    let cases: [(&str, &str); 4] = [
        ("pinyin", "zhongguo"),
        ("pinyin.ziranma", "vsgo"),
        ("pinyin.flypy", "vsgo"),
        ("pinyin.zhuyin", "5j/eji"),
    ];
    println!("\n== 四套 schema 独立出词 ==");
    for (id, keys) in cases {
        let mut engine = Engine::create_with_roots(&build, id, &[user_dir.clone(), shared.clone()])
            .unwrap_or_else(|| panic!("Engine 装配失败：{id}"));
        let name = engine.schema_name().to_string();
        type_keys(&mut engine, keys);
        let texts = top(&mut engine, 6);
        println!(
            "  {id:15} ({name}) keys={keys:9} 中国={} 候选={:?}",
            if texts.iter().any(|t| t == "中国") { "有" } else { "无" },
            texts
        );
        engine.process_key(&KeyEvent::new(XK_ESCAPE, 0));
        drop(engine);
    }

    // ---- 3. 自然码学词 → 四套（含注音）都能看到 ----
    let mut learner =
        Engine::create_with_roots(&build, "pinyin.ziranma", &[user_dir.clone(), shared.clone()])
            .expect("自然码引擎");
    type_keys(&mut learner, "vsgo");
    learner.process_key(&KeyEvent::new(0xff54, 0)); // Down
    learner.process_key(&KeyEvent::new(0xff54, 0)); // Down
    let picked = selected(&learner).expect("自然码应有选中候选");
    learner.process_key(&KeyEvent::new(b' ' as u32, 0));
    learner.apply_commit();
    let db = user_dir.join("userdb/pinyin.userdb");
    println!(
        "\n== 学词：自然码上屏「{picked}」，userdb={} ==",
        std::fs::metadata(&db).map(|m| format!("{} B", m.len())).unwrap_or_else(|_| "缺失".into())
    );
    drop(learner);

    for (id, keys) in cases {
        let mut engine = Engine::create_with_roots(&build, id, &[user_dir.clone(), shared.clone()])
            .unwrap_or_else(|| panic!("Engine 装配失败：{id}"));
        let rank = rank_of(&mut engine, keys, &picked);
        println!("  {id:15} 重打 {keys:9} → 「{picked}」排名={rank:?}");
        drop(engine);
    }

    // ---- 4. Ctrl+` 先主后子两级 + 返回 + 真切换 ----
    let mut engine = Engine::create_with_roots(&build, "pinyin", &[user_dir.clone(), shared.clone()])
        .expect("pinyin 引擎");
    engine.process_key(&KeyEvent::new(0x0060, K_CONTROL_MASK));
    let (texts, sel) = menu_texts(&mut engine);
    println!("\n== Ctrl+` 第一级（当前={sel}）==");
    for (i, t) in texts.iter().enumerate() {
        println!("  [{i}] {t}");
    }
    // 选中「拼音」→ 同一菜单进第二级（不切方案）
    engine.process_key(&KeyEvent::new(0x0020, 0));
    let (second, _) = menu_texts(&mut engine);
    println!("  选中「拼音」后第二级 = {second:?}");
    // Esc 返回第一级（菜单不关）
    engine.process_key(&KeyEvent::new(XK_ESCAPE, 0));
    let (back, _) = menu_texts(&mut engine);
    println!("  第二级 Esc 后 = {back:?}（段数={}）", engine.context.composition.segmentation.segments.len());
    // 再进第二级，选「注音」→ 真正切到 pinyin.zhuyin
    engine.process_key(&KeyEvent::new(0x0020, 0));
    for _ in 0..3 {
        engine.process_key(&KeyEvent::new(0xff54, 0)); // Down 到第 4 项
    }
    engine.process_key(&KeyEvent::new(0x0020, 0));
    println!(
        "  第二级选中后：菜单段数={} 当前 schema={}",
        engine.context.composition.segmentation.segments.len(),
        engine.schema_id()
    );
    type_keys(&mut engine, "5j/eji");
    println!("  切换后打注音 5j/eji → {:?}", top(&mut engine, 3));
    assert_eq!(engine.schema_id(), "pinyin.zhuyin", "第二级选择后应切到注音");
    assert!(
        top(&mut engine, 3).iter().any(|t| t == "中国"),
        "注音应能打出「中国」"
    );
}
