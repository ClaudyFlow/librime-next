//! 权值对拍:同一编码在 C++ 构建的 table.bin vs 我们构建的 table.bin 里的 weight。
use librime_next::dict::table::Table;
use librime_next::dict::vocabulary::Code;

fn dump(path: &str, code: &[&str]) {
    let Ok(table) = Table::open(std::path::PathBuf::from(path)) else {
        println!("{path}: 打不开");
        return;
    };
    let syl = table.syllabary().expect("syllabary");
    let mut ids: Vec<librime_next::dict::vocabulary::SyllableId> = Vec::new();
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
    print!("{path} [{}]:", code.join(" "));
    let mut n = 0;
    while !acc.exhausted() && n < 5 {
        if let Some(e) = acc.entry() {
            print!(" {}={:.2}", table.entry_text(&e), e.weight);
            n += 1;
        }
        acc.next();
    }
    println!();
}

fn main() {
    dump(r"C:\Code\Weasel-Next\librime-next\fixture\build-20261023\luna_pinyin_simp\luna_pinyin.table.bin", &["ni"]);
    dump(r"C:\Users\CloudyFlow\AppData\Roaming\Rime-Next\build\luna_pinyin\luna_pinyin.table.bin", &["ni"]);
    dump(r"C:\Code\Weasel-Next\librime-next\test\e2e\build\luna_pinyin.table.bin", &["ni"]);
    dump(r"C:\Code\Weasel-Next\librime-next\fixture\build-20261023\luna_pinyin_simp\luna_pinyin.table.bin", &["ni", "hao"]);
    dump(r"C:\Users\CloudyFlow\AppData\Roaming\Rime-Next\build\luna_pinyin\luna_pinyin.table.bin", &["ni", "hao"]);
}
