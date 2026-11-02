//! 把一条词组直接写进拼音用户词典（userdb），等价"用户教过这个词一次"。
//!
//! 用途：伊布替尼 这类不在系统词库里的专名，输入 yibutini 时本不会成词；
//! 写进 userdb 后，script_translator 的 `user_dict_candidates` 就会把它当候选顶出来。
//!
//! 用法：
//!   cargo run --release --example seed_phrase -- --dict luna_pinyin --prism luna_pinyin_simp \
//!       --userdb luna_pinyin_simp --code "yi bu ti ni " --text "伊布替尼"
//!
//! 注意：userdb 被活引擎占用时 LevelDB 会锁；运行前先停掉 Weasel-Next 相关进程。

use librime_next::dict::db::Db;
use librime_next::dict::level_db::LevelDb;
use librime_next::dict::user_dictionary::UserDictionary;
use librime_next::dict::vocabulary::DictEntry;
use std::path::PathBuf;
use std::sync::Arc;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut dict_name = "luna_pinyin".to_string();
    let mut prism_name = "luna_pinyin_simp".to_string();
    let mut userdb_name = "luna_pinyin_simp".to_string();
    let mut code = String::new();
    let mut text = String::new();

    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--dict" => dict_name = args.next().expect("--dict <name>"),
            "--prism" => prism_name = args.next().expect("--prism <name>"),
            "--userdb" => userdb_name = args.next().expect("--userdb <name>"),
            "--code" => code = args.next().expect("--code <space-sep syllables + trailing space>"),
            "--text" => text = args.next().expect("--text <phrase>"),
            other => eprintln!("忽略未知参数：{other}"),
        }
    }
    assert!(!code.is_empty() && !text.is_empty(), "必须给 --code 和 --text");

    let appdata = std::env::var("APPDATA").expect("需要 APPDATA 环境变量");
    let build_dir = PathBuf::from(&appdata).join("Rime-Next/build");
    let userdb_dir = PathBuf::from(&appdata).join("Rime-Next/userdb");

    println!("# build_dir = {build_dir:?}  userdb_dir = {userdb_dir:?}");
    let Some(dict) = librime_next::dict::dictionary::Dictionary::open(&build_dir, &dict_name, &prism_name)
    else {
        eprintln!("打开词典失败：{dict_name}/{prism_name}");
        return;
    };

    let db = Arc::new(LevelDb::new_user_db(&userdb_dir, &userdb_name)) as Arc<dyn Db>;
    let mut user_dict = UserDictionary::new(&userdb_name, db);
    if !user_dict.load() {
        eprintln!("打开 userdb 失败：{}（引擎可能在占用，先停进程）", userdb_name);
        return;
    }

    let mut entry = DictEntry::default();
    entry.text = text.clone();
    // 拼音编码：空格分隔的音节 + 尾随空格，与引擎 `translate_code_to_string` 一致
    entry.custom_code = code.clone();

    println!("# 写入 userdb：键 = \"{code}\t{text}\"");
    if user_dict.update_entry(&dict, &entry, 1) {
        println!("OK：{text} 已写入用户词典（commits=1）");
    } else {
        eprintln!("写入失败");
    }
}
