//! 用户词典只读探针：dump 一个 userdb 的实际内容（学词到底写进去没有）。
//!
//! **只读**打开（`open_readonly`），不写 tick、不改数据 —— 可以直接指到真实用户目录。
//!
//! 用法：
//!   cargo run --release --example probe_userdb -- <userdb_dir> <name> [最多打印条数]
//! 例（看拼音方案的学词）：
//!   cargo run --release --example probe_userdb -- "$APPDATA/Rime-Next/userdb" pinyin 40

use librime_next::dict::db::{Db, DbAccessor};
use librime_next::dict::level_db::LevelDb;
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().expect("userdb_dir"));
    let name = args.next().expect("userdb name（如 pinyin）");
    let limit: usize = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);

    let db = LevelDb::new_user_db(&dir, &name);
    if !db.open_readonly() {
        println!("open_readonly 失败：{:?}", db.file_path());
        return;
    }
    println!("# userdb={name}  路径={:?}", db.file_path());

    // 元数据：看 db 类型/tick
    if let Some(mut meta) = db.query_metadata() {
        let mut shown = 0;
        while let Some((k, v)) = meta.get_next_record() {
            println!("meta {k} = {v}");
            shown += 1;
            if shown > 12 {
                break;
            }
        }
    }

    let mut acc = match db.query_all() {
        Some(a) => a,
        None => {
            println!("query_all 返回 None（db 未加载？）");
            return;
        }
    };
    let mut total = 0usize;
    while let Some((key, value)) = acc.get_next_record() {
        if total < limit {
            println!("{key}\t{value}");
        }
        total += 1;
        if total > 200_000 {
            break;
        }
    }
    println!("# 总条数 >= {total}");
}
