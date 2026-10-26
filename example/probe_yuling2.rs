//! 宇浩系词典查询链路探针:get_value → spelling_syllable_id → query_phrases,
//! 以及 import_tables 展开的编码提取(与 all_schemas_e2e 同逻辑)。
use librime_next::dict::dictionary::Dictionary;
use librime_next::dict::vocabulary::Code;
use std::path::PathBuf;

fn collect_codes(path: &PathBuf, codes: &mut Vec<String>, limit: usize) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines().skip_while(|l| !l.starts_with("...")) {
        if line.starts_with('#') || line.starts_with("---") || line.starts_with("...") {
            continue;
        }
        let mut f = line.split('\t');
        let _ = f.next();
        if let Some(code) = f.next().map(str::trim) {
            if !code.is_empty() && code.chars().all(|c| c.is_ascii_lowercase()) {
                codes.push(code.to_string());
            }
        }
        if codes.len() >= limit {
            break;
        }
    }
}

fn main() {
    let user = PathBuf::from(std::env::var_os("APPDATA").unwrap()).join("Rime-Next");
    let shared = PathBuf::from(r"C:\Program Files\Weasel-Next\Rime-Next");
    let build = user.join("build");
    let dict = Dictionary::open(&build, "yuling", "yuling").expect("词典应打开");
    let source = librime_next::dict::dict_compiler::classified_dict_source(&user, &shared, "yuling")
        .expect("词典源应找到");
    println!("词典源: {}", source.display());

    let mut codes: Vec<String> = Vec::new();
    collect_codes(&source, &mut codes, 5);
    if codes.is_empty() {
        // 展开 import_tables(宇浩系:主词典正文为空)
        let text = std::fs::read_to_string(&source).unwrap();
        let mut in_imports = false;
        for line in text.lines() {
            if line.starts_with("import_tables:") {
                in_imports = true;
                continue;
            }
            if in_imports {
                if line.starts_with('#') || line.trim().is_empty() {
                    continue;
                }
                if !line.starts_with(' ') && !line.starts_with('-') {
                    in_imports = false;
                    continue;
                }
                let Some(name) = line.trim().strip_prefix("- ") else { continue };
                match librime_next::dict::dict_compiler::classified_dict_source(&user, &shared, name.trim()) {
                    Some(src) => {
                        println!("import {name} -> {}", src.display());
                        collect_codes(&src, &mut codes, 5);
                    }
                    None => println!("import {name} -> 未找到!"),
                }
            }
            if !codes.is_empty() {
                break;
            }
        }
    }
    println!("样例编码: {codes:?}");

    for code in &codes {
        let gv = dict.prism.get_value(code);
        let sid = gv.map(|id| dict.prism.spelling_syllable_id(id));
        let mut n = 0usize;
        if let Some(syllable) = sid {
            let mut acc = dict.table.query_phrases(&Code::from_slice(&[syllable]));
            while !acc.exhausted() {
                if acc.entry().is_some() {
                    n += 1;
                }
                acc.next();
            }
        }
        let via_translate = dict.translate(code).len();
        println!("code={code}: get_value={gv:?} syllable={sid:?} query_phrases={n} 条 | translate()={via_translate} 条");
    }
}
