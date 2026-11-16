//! librime#1268 量化探针：真词库上 `lookup_words` + `collect_entries` 的每键耗时。
//!
//! 补全/空码这类形状一次会装进成百上千页（chunk），`collect_entries` 又要全遍历；
//! 线性 `sort()` 每取一条都扫一遍剩余页，正是 O(N·k) 的最坏场景。
//!
//! 用法（GNU 工具链、独立 target）：
//!
//! ```text
//! $env:PATH = 'C:\Program Files\llvm-mingw\bin;' + $env:PATH
//! $env:LIBCLANG_PATH = 'C:\Program Files\llvm-mingw\bin'
//! $env:CARGO_TARGET_DIR = 'C:\Code\Weasel-Next\target-1268'
//! $env:BENCH_EMPTY = '1'   # 空码全量很慢，且部分 prism 的 expand_search("") 会跑飞，默认跳过
//! cargo run --release --example bench_dict_iterator -- <build_dir> <dict> <prism> [rounds]
//! ```
//!
//! 输出形状的 keys（命中键数）、N（词条数）、k（页数），以及两段各自的分位耗时：
//! `lookup` = `lookup_words`（装页），`walk` = `collect_entries`（出词，`sort()` 在这里跑）。

use std::path::Path;
use std::time::Instant;

use librime_next::gear::dict::dict_entry_iterator::DictEntryIterator;
use librime_next::gear::dict::dictionary::Dictionary;

struct Row {
    label: &'static str,
    code: &'static str,
    predictive: bool,
    keys: usize,
    entries: usize,
    chunks: usize,
    lookup: Vec<f64>,
    walk: Vec<f64>,
    advance: Vec<f64>,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    // 最近秩法（nearest-rank）：第 ceil(p/100 * n) 个
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn mean_std(xs: &[f64]) -> (f64, f64) {
    if xs.is_empty() {
        return (0.0, 0.0);
    }
    let n = xs.len() as f64;
    let mean = xs.iter().sum::<f64>() / n;
    let var = if xs.len() > 1 {
        xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0)
    } else {
        0.0
    };
    (mean, var.sqrt())
}

fn run_case(
    dict: &Dictionary,
    label: &'static str,
    code: &'static str,
    predictive: bool,
    rounds: usize,
) -> Row {
    let limit = 0; // 0 = 不限键，量的是"全量"这一侧
    for _ in 0..2 {
        let mut iter = DictEntryIterator::new();
        dict.lookup_words(&mut iter, code, predictive, limit, None);
        let _ = iter.collect_entries();
    }

    let mut lookup = Vec::with_capacity(rounds);
    let mut walk = Vec::with_capacity(rounds);
    let mut advance = Vec::with_capacity(rounds);
    let mut keys = 0usize;
    let mut entries = 0usize;
    let mut chunks = 0usize;
    for _ in 0..rounds {
        let mut iter = DictEntryIterator::new();
        let t0 = Instant::now();
        keys = dict.lookup_words(&mut iter, code, predictive, limit, None);
        entries = iter.entry_count();
        chunks = iter.chunk_count();
        lookup.push(t0.elapsed().as_secs_f64() * 1e6);

        let t1 = Instant::now();
        let out = iter.collect_entries();
        walk.push(t1.elapsed().as_secs_f64() * 1e6);
        assert_eq!(out.len(), entries, "collect_entries 不能漏条目");

        // 只推进游标（不造 DictEntry、不收集），单看迭代器本身的成本。
        let mut iter = DictEntryIterator::new();
        dict.lookup_words(&mut iter, code, predictive, limit, None);
        let t2 = Instant::now();
        while iter.next() {}
        advance.push(t2.elapsed().as_secs_f64() * 1e6);
    }
    Row {
        label,
        code,
        predictive,
        keys,
        entries,
        chunks,
        lookup,
        walk,
        advance,
    }
}

fn print_row(row: &Row) {
    let mut lk = row.lookup.clone();
    lk.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut wk = row.walk.clone();
    wk.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut av = row.advance.clone();
    av.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let (lmean, lsd) = mean_std(&row.lookup);
    let (wmean, wsd) = mean_std(&row.walk);
    let (amean, asd) = mean_std(&row.advance);
    println!(
        "{:<16} {:>5} {:>7} {:>5} | {:>9.1} {:>9.1} {:>9.1} {:>9.1}±{:<7.1} | {:>10.1} {:>10.1} {:>10.1} {:>10.1}±{:<7.1} | {:>9.1} {:>9.1} {:>9.1} {:>9.1}±{:<7.1}",
        format!("{} {:?}", row.label, row.code),
        row.keys,
        row.entries,
        row.chunks,
        percentile(&lk, 50.0),
        percentile(&lk, 95.0),
        percentile(&lk, 99.0),
        lmean,
        lsd,
        percentile(&wk, 50.0),
        percentile(&wk, 95.0),
        percentile(&wk, 99.0),
        wmean,
        wsd,
        percentile(&av, 50.0),
        percentile(&av, 95.0),
        percentile(&av, 99.0),
        amean,
        asd
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let build_dir = args
        .get(1)
        .map(String::as_str)
        .unwrap_or(r"..\librime-next\test\e2e\build");
    let dict_name = args.get(2).map(String::as_str).unwrap_or("luna_pinyin");
    let prism_name = args.get(3).map(String::as_str).unwrap_or("luna_pinyin_simp");
    let rounds: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(20);
    let with_empty = std::env::var("BENCH_EMPTY").is_ok();

    let Some(dict) = Dictionary::open(Path::new(build_dir), dict_name, prism_name) else {
        eprintln!("打不开词库：{build_dir} / {dict_name} / {prism_name}");
        std::process::exit(2);
    };

    let mut cases: Vec<(&'static str, &'static str, bool)> = vec![
        ("整码", "a", false),
        ("单字母补全", "a", true),
        ("双字母补全", "ni", true),
        ("整码", "ni", false),
    ];
    if with_empty {
        cases.insert(2, ("空码全量", "", true));
    }

    println!(
        "词库 {build_dir} / {dict_name}（prism {prism_name}），单位 µs，lookup=lookup_words，walk=collect_entries"
    );
    println!(
        "{:<16} {:>5} {:>7} {:>5} | {:<47} | {:<50} | {}",
        "形状/码", "keys", "N", "k", "lookup p50/p95/p99/mean±sd", "walk p50/p95/p99/mean±sd", "advance p50/p95/p99/mean±sd"
    );

    for (label, code, predictive) in cases {
        eprintln!("[run] {label} code={code:?} predictive={predictive}");
        let probe = run_case(&dict, label, code, predictive, 1);
        eprintln!(
            "[run]   keys={} N={} k={} lookup={:.1}µs walk={:.1}µs advance={:.1}µs",
            probe.keys, probe.entries, probe.chunks, probe.lookup[0], probe.walk[0], probe.advance[0]
        );
        let rounds = if probe.lookup[0] + probe.walk[0] + probe.advance[0] > 20_000.0 {
            5
        } else {
            rounds
        };
        let row = run_case(&dict, label, code, predictive, rounds);
        print_row(&row);
        println!("  └ walk 原始样本 µs = {:?}", row.walk);
    }
}
