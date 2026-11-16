//! 直接展示 prism(拼写→音节) 与 table(音节→词) 两段真实 I/O。
//! 喂 `nihao`,分别打印:
//!   1) prism 阶段 —— 每个起始位置命中的拼写、对应音节 id、音节串;
//!   2) table 阶段 —— translate 解出的前 10 个候选词 + 它的音节码(解码成可读串)。
//!
//! 运行: cargo run --example probe_prism_table [build_dir]
use std::path::Path;

use librime_next::gear::dict::dictionary::Dictionary;
use librime_next::gear::dict::prism::Prism;
use librime_next::gear::dict::table::Table;

fn main() {
    let build = if let Some(arg) = std::env::args().nth(1) {
        Path::new(&arg).to_path_buf()
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../build/pinyin/luna")
    };
    let dict_name = "pinyin.luna";
    let prism_name = "pinyin.luna";
    let input = "nihao";

    println!("build 目录: {}", build.display());
    println!("输入拼写串: {input}\n");

    // 两个结构各自打开;音节文本存在 table 的音节表里(prism 本身不内嵌文本)。
    let prism = Prism::open(build.join(format!("{prism_name}.prism.bin"))).expect("打开 prism.bin 失败");
    let table = Table::open(build.join(format!("{dict_name}.table.bin"))).expect("打开 table.bin 失败");
    let syl = |sid: i32| table.get_syllable_by_id(sid);

    // ---------------- PRISM 阶段:拼写串 -> 音节 ----------------
    println!("=== [1] PRISM 阶段(拼写 -> 音节) ===");
    let n = input.len();
    for start in 0..n {
        let suffix = &input[start..];
        for m in prism.common_prefix_search(suffix) {
            let sid = prism.spelling_syllable_id(m.value);
            println!(
                "  起点{start:2}  拼写 '{:<6}' 拼写id={:>4} -> 音节id={:>4} 音节={:?}",
                &input[start..start + m.length],
                m.value,
                sid,
                syl(sid),
            );
        }
    }
    println!("  (说明:prism 只把按键串译成「音节 id」,文本在 table 的音节表里。)");

    // ---------------- TABLE 阶段:音节 -> 词 ----------------
    println!("\n=== [2] TABLE 阶段(音节 -> 候选词, 前 10) ===");
    let dict = Dictionary::open(&build, dict_name, prism_name).expect("打开词典失败");
    let mut entries = dict.translate(input);
    entries.sort_by(|a, b| b.quality.partial_cmp(&a.quality).unwrap());
    for e in entries.iter().take(10) {
        let syls: Vec<String> = e.code.iter().map(|&sid| syl(sid)).collect();
        println!(
            "  词={:<8} weight={:>8.1} 音节码={:?} -> 音节串={}",
            e.text, e.weight, e.code, syls.join(" "),
        );
    }

    // 聚焦:恰好把 nihao 切成 ni+hao 那一组(2 音节)的候选。
    println!("\n=== [3] 聚焦: nihao 精确切成 ni+hao 的候选 ===");
    let exact: Vec<_> = entries
        .iter()
        .filter(|e| e.code.len() == 2)
        .take(10)
        .collect();
    if exact.is_empty() {
        println!("  (无 2 音节精确匹配)");
    }
    for e in exact {
        let syls: Vec<String> = e.code.iter().map(|&sid| syl(sid)).collect();
        println!("  词={:<6} weight={:>8.1} 音节串={}", e.text, e.weight, syls.join(" "));
    }
}
