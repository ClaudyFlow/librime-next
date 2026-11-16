//! 权值对拍:同一编码在 C++ 构建的 table.bin vs 我们构建的 table.bin 里的 weight。
use librime_next::gear::dict::table::Table;
use librime_next::gear::dict::vocabulary::Code;

fn dump(path: &str, code: &[&str]) {
    // argv 模式：`probe_weight <table.bin> <音节...>`（每个参数是一个音节拼写，
    // 巧码的全码就是一整串 `ls;p;l`）。存入表里的是 ln 权重，这里同时打原值。
    let argv_mode = code.len() > 1 || std::env::args().count() > 2;
    let Ok(table) = Table::open(std::path::PathBuf::from(path)) else {
        println!("{path}: 打不开");
        return;
    };
    let syl = table.syllabary().expect("syllabary");
    let mut ids: Vec<librime_next::gear::dict::vocabulary::SyllableId> = Vec::new();
    for c in code {
        match syl.id_of(c) {
            Some(id) => ids.push(id),
            None => {
                println!("{path}: 音节缺失 {code:?}");
                return;
            }
        }
    }
    let c = Code::from_slice(&ids);
    let mut acc = table.query_phrases(&c);
    let limit = if argv_mode { 15 } else { 5 };
    print!("{path} [{}]:", code.join(" "));
    let mut n = 0;
    while !acc.exhausted() && n < limit {
        if let Some(e) = acc.entry() {
            if argv_mode {
                print!(
                    " {}[ln={:.2} w={:.4}]",
                    table.entry_text(&e),
                    e.weight,
                    e.weight.exp()
                );
            } else {
                print!(" {}={:.2}", table.entry_text(&e), e.weight);
            }
            n += 1;
        }
        acc.next();
    }
    println!();
}

fn main() {
    // CLI：`probe_weight <table.bin> <音节...>`（音节按空格分开传）。
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if !argv.is_empty() {
        let path = argv[0].clone();
        let code: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
        dump(&path, &code);
        return;
    }
    dump(r"C:\Code\Weasel-Next\librime-next\fixture\build-20261023\luna_pinyin_simp\luna_pinyin.table.bin", &["ni"]);
    dump(r"C:\Users\CloudyFlow\AppData\Roaming\Rime-Next\build\luna_pinyin\luna_pinyin.table.bin", &["ni"]);
    dump(r"C:\Code\Weasel-Next\librime-next\test\e2e\build\luna_pinyin.table.bin", &["ni"]);
    dump(r"C:\Code\Weasel-Next\librime-next\fixture\build-20261023\luna_pinyin_simp\luna_pinyin.table.bin", &["ni", "hao"]);
    dump(r"C:\Users\CloudyFlow\AppData\Roaming\Rime-Next\build\luna_pinyin\luna_pinyin.table.bin", &["ni", "hao"]);
}
