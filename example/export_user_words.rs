//! 把用户词典导出成词云可用的 TSV。
//!
//! **只读**打开（`open_readonly`），不写 tick、不改数据。若词典正在被输入法占用
//! （`LOCK` 文件独占），先把 `.userdb` **复制**一份（跳过 `LOCK`）再指过来。
//!
//! 用法：
//!   cargo run --release --example export_user_words -- <userdb_dir> <out.tsv> [shared_data_dir]
//!
//! 给了 `shared_data_dir` 就顺手做一遍 `t2s` 简化（词云要把繁体的 `這個` 和简体的
//! `这个` 算成同一个词，否则字号被劈成两半）。
//!
//! 输出列（UTF-8，LF）：`schema \t text \t simp \t commits \t dee \t tick`
//! 同词同方案多次出现直接累加 `commits`（用户词典理论上一词一条，防御性合并）。

use librime_next::dict::db::Db;
use librime_next::dict::level_db::LevelDb;
use librime_next::dict::opencc::Opencc;
use librime_next::dict::user_db::UserDbValue;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Default)]
struct Agg {
    commits: i64,
    dee: f64,
    tick: u64,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().expect("userdb_dir"));
    let out = PathBuf::from(args.next().expect("out.tsv"));
    let shared = args.next().map(PathBuf::from);

    let t2s = shared
        .as_ref()
        .and_then(|root| Opencc::load(root.join("opencc").join("t2s.json")).ok());
    println!(
        "# 简化表：{}",
        if t2s.is_some() { "t2s.json 已加载" } else { "无（保持原样）" }
    );

    let mut tables: BTreeMap<(String, String), Agg> = BTreeMap::new();
    let mut dbs = 0usize;
    let mut records = 0usize;

    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("读 userdb 目录失败")
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".userdb").map(|s| s.to_string())
        })
        .collect();
    names.sort();

    for name in names {
        let db = LevelDb::new_user_db(&dir, &name);
        if !db.open_readonly() {
            eprintln!("# 跳过 {name}（打不开，可能被占用）");
            continue;
        }
        let Some(mut acc) = db.query_all() else {
            eprintln!("# 跳过 {name}（query_all 为空）");
            continue;
        };
        dbs += 1;
        let mut local = 0usize;
        while let Some((key, value)) = acc.get_next_record() {
            local += 1;
            let Some(v) = UserDbValue::unpack(&value) else {
                continue;
            };
            let Some((_code, text)) = key.split_once('\t') else {
                continue;
            };
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            let slot = tables
                .entry((name.clone(), text.to_string()))
                .or_default();
            slot.commits += v.commits as i64;
            slot.dee += v.dee;
            slot.tick = slot.tick.max(v.tick);
        }
        records += local;
        println!("# {name}: {local} 条");
    }

    let file = std::fs::File::create(&out).expect("建输出文件失败");
    let mut w = std::io::BufWriter::new(file);
    writeln!(w, "schema\ttext\tsimp\tcommits\tdee\ttick").unwrap();
    let rows = tables.len();
    for ((schema, text), a) in &tables {
        let simp = match &t2s {
            Some(occ) => occ.convert(text),
            None => text.clone(),
        };
        writeln!(
            w,
            "{schema}\t{text}\t{simp}\t{}\t{:.4}\t{}",
            a.commits, a.dee, a.tick
        )
        .unwrap();
    }
    w.flush().unwrap();

    println!(
        "# {dbs} 个 db / {records} 条记录 → {rows} 个 (方案,词) 组合\n# 输出：{}",
        Path::new(&out).display()
    );
}
