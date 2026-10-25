//! 用**真实样例**验证 `algo` 层（不是合成数据）。
//!
//! ```text
//! cargo run --release -p librime-next --example algo_real
//! ```
//!
//! 三块：
//!
//! 1. `algebra`：`%APPDATA%\Rime-Next\schema\yuling\yuling.schema.yaml:159-161`
//!    的真实规则（含 `derive/^([a-y])([a-y])/$1z/`）作用到输入 `aa`；
//! 2. `syllabifier`：`double_pinyin_ziranma` 的**拼写集**（由真实 schema 的
//!    `speller/algebra` + `luna_pinyin` 词库里的音节表演算出来）对输入 `ksdo` 切分；
//! 3. `dynamics`：`formula_d` / `formula_p` 抽点数值，供回归对比。
//!
//! 只读磁盘，不写任何东西（除了 stdout）。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use librime_next::algo::{
    dynamics, formula_d, formula_p, InMemoryPrism, Projection, Script, Syllabifier,
    SyllableGraph,
};

fn appdata() -> PathBuf {
    PathBuf::from(std::env::var("APPDATA").expect("APPDATA"))
}

fn rime_next_dir() -> PathBuf {
    appdata().join("Rime-Next")
}

/// 读一份 YAML 成树（走 crate 的 `load_file`，它已处理 Rime 的 `...` 约定）。
fn load(path: &Path) -> serde_json::Value {
    librime_next::load_file(path).unwrap_or_else(|e| panic!("读不了 {}: {e}", path.display()))
}

fn rules_at(tree: &serde_json::Value, path: &str) -> Vec<String> {
    let mut node = tree;
    for segment in path.split('/') {
        node = &node[segment];
    }
    node.as_array()
        .unwrap_or_else(|| panic!("{path} 不是列表"))
        .iter()
        .map(|v| v.as_str().expect("规则必须是字符串").to_string())
        .collect()
}

fn main() {
    real_algebra();
    real_syllabifier();
    real_dynamics();
}

// ---------------------------------------------------------------------------
// 1) algebra：yuling 的真实规则
// ---------------------------------------------------------------------------

fn real_algebra() {
    let path = rime_next_dir().join("schema/yuling/yuling.schema.yaml");
    println!("=== 1) algebra：{}（第 159-161 行） ===", path.display());
    let tree = load(&path);
    let rules = rules_at(&tree, "speller/algebra");
    for (i, rule) in rules.iter().enumerate() {
        println!("  规则 #{}: {rule}", i + 1);
    }
    let mut projection = Projection::from_rules(&rules).expect("yuling 的规则应当全部可解析");
    println!("  解析出 {} 条 Calculation", projection.len());

    // 对输入 "aa" 做变换：`derive/^([a-y])([a-y])/$1z/` 命中 → 原拼写 + 派生拼写
    let mut script = Script::new();
    assert!(script.add_syllable("aa"));
    let modified = projection.apply_script(&mut script);
    println!(
        "  Script{{\"aa\"}} --apply--> modified = {modified}，结果集合 = {:?}",
        script.keys().cloned().collect::<Vec<_>>()
    );
    for (key, spellings) in script.iter() {
        for s in spellings {
            println!(
                "    键 {:?}: 音节 {:?} type={:?} credibility={}",
                key, s.str, s.properties.r#type, s.properties.credibility
            );
        }
    }

    // 再单独看字符串路径（Projection::Apply(string*)）
    let mut value = "aa".to_string();
    println!("  apply_str(\"aa\") -> {} ({value:?})", projection.apply_str(&mut value));
    for probe in ["ab", "az", "za", "zz"] {
        let mut v = probe.to_string();
        let changed = projection.apply_str(&mut v);
        println!("  apply_str({probe:?}) -> {changed} {v:?}");
    }
    println!();
}

// ---------------------------------------------------------------------------
// 2) syllabifier：double_pinyin_ziranma 的拼写集 × "ksdo"
// ---------------------------------------------------------------------------

/// 从 `luna_pinyin.dict.yaml` 收音节表（= librime `EntryCollector` 收集的那份）。
fn luna_pinyin_syllables() -> BTreeSet<String> {
    let path = PathBuf::from("C:/Code/librime/data/minimal/luna_pinyin.dict.yaml");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    // `...` 之后才是词条：`字<TAB>音节[ 音节...]<TAB>权重`
    // 注意文件是 CRLF，按行找 `...` 而不是字符串查找。
    let mut body_start = 0usize;
    let mut offset = 0usize;
    for line in text.lines() {
        offset += line.len() + 1;
        if line.trim() == "..." {
            body_start = offset;
            break;
        }
    }
    assert!(body_start > 0, "{} 里没有 `...` 分隔", path.display());
    let mut syllables = BTreeSet::new();
    for line in text[body_start..].lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split('\t');
        let _word = fields.next();
        let Some(code) = fields.next() else { continue };
        for syllable in code.split(' ') {
            if !syllable.is_empty() {
                syllables.insert(syllable.to_string());
            }
        }
    }
    syllables
}

fn real_syllabifier() {
    // 优先用**部署产物**里的 schema（它已把 `__include: luna_pinyin_simp.schema:/` 合并进来，
    // `speller/delimiter` 这种继承字段只有产物里才有）；没有就退回源 schema。
    let compiled = rime_next_dir().join("build/double_pinyin_ziranma/double_pinyin_ziranma.schema.yaml");
    let source = rime_next_dir().join("schema/double_pinyin_ziranma/double_pinyin_ziranma.schema.yaml");
    let (path, tree) = if compiled.exists() {
        let tree = load(&compiled);
        (compiled, tree)
    } else {
        (source.clone(), load(&source))
    };
    println!("=== 2) syllabifier：{} ===", path.display());
    let rules = rules_at(&tree, "speller/algebra");
    let delimiters = tree["speller"]["delimiter"].as_str().unwrap_or(" '").to_string();
    println!("  algebra {} 条，delimiter = {delimiters:?}", rules.len());

    let syllabary = luna_pinyin_syllables();
    println!("  luna_pinyin 音节表 {} 个", syllabary.len());

    // 音节 → 拼写集（就是 `Prism::Build(syllabary, script)` 之前的那一步）
    let mut script = Script::new();
    for syllable in &syllabary {
        script.add_syllable(syllable);
    }
    let mut projection = Projection::from_rules(&rules).expect("ziranma 的规则应当全部可解析");
    let modified = projection.apply_script(&mut script);
    println!("  演算后拼写 {} 个（modified = {modified}）", script.len());
    for probe in ["kong", "duo", "ks", "do", "zhong"] {
        if let Some(list) = script.get(probe) {
            println!(
                "    键 {probe:?} → {:?}",
                list.iter()
                    .map(|s| (s.str.as_str(), s.properties.r#type))
                    .collect::<Vec<_>>()
            );
        }
    }

    // 一个拼写串对应多个音节（双拼里 inevitable：不同音节撞同一个码）
    let collisions: Vec<(&String, Vec<&str>)> = script
        .iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(k, v)| (k, v.iter().map(|s| s.str.as_str()).collect()))
        .collect();
    println!("  一码多音节的键 {} 个，例：", collisions.len());
    for (code, syllables) in collisions.iter().take(8) {
        println!("    {code:?} → {syllables:?}");
    }

    let prism = InMemoryPrism::from_script(&syllabary, &script);
    let syllables: Vec<String> = syllabary.iter().cloned().collect();

    let mut syllabifier = Syllabifier::with_options(&delimiters, false, false, None);
    for input in ["ksdo", "ks'do", "aa", "si", "vs", "ksss", "loxx"] {
        let mut graph = SyllableGraph::default();
        let farthest = syllabifier.build_syllable_graph(input, &prism, &mut graph);
        println!(
            "
  输入 {input:?}: farthest = {farthest} / {} （interpreted_length = {}）",
            input.len(),
            graph.interpreted_length
        );
        // 把所有从 0 走到末尾的路径列出来
        let mut paths: Vec<Vec<String>> = Vec::new();
        walk(&graph, 0, graph.interpreted_length, &syllables, &mut Vec::new(), &mut paths);
        paths.sort();
        paths.dedup();
        println!("  切分 {} 种:", paths.len());
        for path in &paths {
            println!("    {}", path.join(" | "));
        }
        // 每条边上的候选音节（含模糊/简拼等多义）
        for (start, ends) in graph.edges.iter() {
            for (end, spellings) in ends {
                let mut names: Vec<String> = spellings
                    .keys()
                    .map(|id| syllables.get(*id as usize).cloned().unwrap_or_else(|| format!("#{id}")))
                    .collect();
                names.sort();
                names.dedup();
                println!("    边 [{start}, {end}) → {:?}", names);
            }
        }
    }
    println!();
}

/// 列举音节图上从 `from` 到 `to` 的所有路径（同一条路径去重由调用方做）。
fn walk(
    graph: &SyllableGraph,
    from: usize,
    to: usize,
    syllables: &[String],
    stack: &mut Vec<String>,
    out: &mut Vec<Vec<String>>,
) {
    if from == to {
        out.push(stack.clone());
        return;
    }
    let Some(ends) = graph.edges.get(&from) else { return };
    for (end, spellings) in ends {
        if *end > to {
            continue;
        }
        for syllable_id in spellings.keys() {
            let name = syllables
                .get(*syllable_id as usize)
                .cloned()
                .unwrap_or_else(|| format!("#{syllable_id}"));
            stack.push(name);
            walk(graph, *end, to, syllables, stack, out);
            stack.pop();
        }
    }
}

// ---------------------------------------------------------------------------
// 3) dynamics：抽点数值，供回归对比
// ---------------------------------------------------------------------------

fn real_dynamics() {
    println!("=== 3) dynamics：formula_d / formula_p 抽样 ===");
    println!("  formula_d(d, t, da, ta) = d + da * exp((ta - t) / 200)");
    for (d, t, da, ta) in [
        (0.0, 0.0, 0.0, 0.0),
        (1.0, 0.0, 2.0, 200.0),
        (1.0, 500.0, 2.0, 500.0),
        (3.5, 1234.0, 0.0, 100.0),
        (1.0, 100000.0, 2.0, 0.0),
    ] {
        println!(
            "    formula_d({d}, {t}, {da}, {ta}) = {:.15e}",
            formula_d(d, t, da, ta)
        );
    }
    let k_m = 1.0 / (1.0 - (-0.005f64).exp());
    println!("  formula_p(s, u, t, d)，kM = {k_m:.15}");
    for (s, u, t, d) in [
        (1.0, 0.0, 0.0, 0.0),
        (1.0, 0.0, 5000.0, 0.0),
        (1.0, 0.0, 5000.0, 1.0),
        (1.0, 0.0, 5000.0, 10.0),
        (1.0, 0.0, 5000.0, 19.0),
        (1.0, 0.0, 5000.0, 20.0),
        (1.0, 0.0, 5000.0, 50.0),
        (1.0, 0.0, 1000.0, 0.0),
        (1.0, 0.0, 100000.0, 0.0),
        (5.0, 2.0, 86400.0, 30.0),
    ] {
        println!(
            "    formula_p({s}, {u}, {t}, {d}) = {:.15e}",
            formula_p(s, u, t, d)
        );
    }
    let _ = dynamics::formula_p; // 也让 `dynamics` 模块本身有引用点
    real_dynamics_note();
}

fn real_dynamics_note() {
    println!(
        "  注：t 变大不是逼近上限 s，而是从 s 滑回当前值 u；d = 0 时公式返回 m = s。"
    );
}
