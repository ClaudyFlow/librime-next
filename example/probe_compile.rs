//! 单测编译器:直接 compile 用户目录的 pinyin.schema.yaml,输出 YAML 后查关键字段。
use librime_next::config;
use std::path::PathBuf;

fn main() {
    let user = PathBuf::from(std::env::var_os("APPDATA").unwrap()).join("Rime-Next");
    let shared = PathBuf::from(r"C:\Program Files\Weasel-Next\Rime-Next");
    let source = user.join("schema").join("pinyin").join("pinyin.schema.yaml");
    let defaults = shared.join("default.yaml");
    let tree = config::compile(&source, &defaults, &[shared.clone()]).expect("编译失败");
    let yaml = config::to_yaml_string(&tree).expect("发射失败");
    println!("含 script_translator@simp = {}", yaml.contains("script_translator@simp"));
    println!("含 simp: 节 = {}", yaml.contains("\nsimp:"));
    println!("含 trad: 节 = {}", yaml.contains("\ntrad:"));
    println!("含 translator: 节 = {}", yaml.contains("\ntranslator:"));
    let mut in_list = false;
    for line in yaml.lines() {
        if line.starts_with("  translators:") { in_list = true; }
        else if in_list && !line.starts_with("    ") { in_list = false; }
        if in_list { println!("  {line}"); }
    }
}
