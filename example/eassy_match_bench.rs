//! eassy 汉字前缀树基准：构建耗时 / 内存 / 精确查 / 前缀查。

use librime_next::dict::eassy_matcher::EassyIndex;
use std::path::Path;
use std::time::Instant;

fn main() {
    let eassy = Path::new("../Rime-Next/eassy/default/essay-zh-hans.txt");
    let t0 = Instant::now();
    let index = EassyIndex::from_file(eassy).expect("读不到 eassy");
    let build = t0.elapsed();

    let nodes = index.node_count();
    let words = index.word_count();
    let bytes = nodes * 96 + words * 24;
    println!(
        "构建 {:.0} ms | 节点 {nodes} | 词 {words} | 跳过 {} | 估算内存 {:.1} MB",
        build.as_secs_f64() * 1e3,
        index.skipped(),
        bytes as f64 / (1024.0 * 1024.0)
    );

    let rounds = 200_000usize;
    let words_to_check = ["你好", "中国", "拟好", "世界", "不存在的词"];
    let t1 = Instant::now();
    let mut hit = 0usize;
    for _ in 0..rounds {
        for w in &words_to_check {
            if index.contains(w).is_some() {
                hit += 1;
            }
        }
    }
    let per = t1.elapsed().as_secs_f64() / (rounds * words_to_check.len()) as f64;
    println!("精确查 {:.3} µs/次（命中 {hit}）", per * 1e6);

    let prefixes = ["你", "中", "世", "不"];
    let prefix_rounds = 20_000usize;
    let t2 = Instant::now();
    let mut total = 0usize;
    for _ in 0..prefix_rounds {
        for p in &prefixes {
            total += index.prefix(p, 32).len();
        }
    }
    let per_prefix = t2.elapsed().as_secs_f64() / (prefix_rounds * prefixes.len()) as f64;
    println!("前缀查 {:.3} µs/次（累计 {total} 条）", per_prefix * 1e6);

    for p in &prefixes {
        let hits = index.prefix(p, 5);
        let show: Vec<String> = hits.iter().map(|(w, f)| format!("{w}({f:.0})")).collect();
        println!("  [{p}*] {}", if show.is_empty() { "(无)".into() } else { show.join(", ") });
    }
    for w in &words_to_check {
        println!("  [{w}] = {:?}", index.contains(w).map(|f| f as i64));
    }
}
