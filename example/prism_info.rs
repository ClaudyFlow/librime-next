// 快速对比两个 prism.bin 的结构标量，判断「功能等价」还是「拼写集合不同」。
// 用法：
//   cargo run --example prism_info <a.prism.bin> <b.prism.bin>
//   cargo run --example prism_info <prism.bin> <自定义检索键>...
// 不传检索键时用一组固定抽样；传了就只查这些键（用来找双拼/注音的真实编码）。
use librime_next::gear::dict::prism::Prism;

const DEFAULT_KEYS: [&str; 8] = ["nihao", "ni", "hao", "zhong", "guo", "wo", "de", "a"];

fn info(path: &str, keys: &[String]) {
    let mut prism = match Prism::open(path) {
        Ok(p) => p,
        Err(e) => {
            println!("  [打开失败] {path}: {e}");
            return;
        }
    };
    if let Err(e) = prism.load() {
        println!("  [load 失败] {path}: {e}");
        return;
    }
    println!("文件: {path}");
    println!("  format          = {}", prism.format());
    println!("  array_size      = {} (拼写总数)", prism.array_size());
    println!("  max_key_length  = {}", prism.max_key_length());
    println!("  dict_checksum   = {:#010x}", prism.dict_file_checksum());
    println!("  schema_checksum = {:#010x}", prism.schema_file_checksum());
    let show = |s: &str| {
        let m = prism.common_prefix_search(s);
        let ids: Vec<_> = m.iter().map(|x| x.value).collect();
        println!("  search('{s}') -> spelling_ids={ids:?}");
        match prism.get_value(s) {
            Some(id) => {
                let syllable_id = prism
                    .query_spelling(id)
                    .descriptor()
                    .map(|d| d.syllable_id)
                    .unwrap_or(-1);
                println!(
                    "  exact('{s}') -> id=Some({id}) syllable_id={syllable_id}"
                );
            }
            None => println!("  exact('{s}') -> id=None"),
        }
    };
    if keys.is_empty() {
        for s in DEFAULT_KEYS {
            show(s);
        }
    } else {
        for s in keys {
            show(s);
        }
    }
    // 若拼写数 > 1800，dump 1792..1800 的额外拼写性质（确认是否 correction）
    if prism.array_size() > 1800 {
        println!("  -- 额外拼写抽样 (1792..1800) --");
        for sid in 1792..1800i32 {
            let acc = prism.query_spelling(sid);
            if let Some(d) = acc.descriptor() {
                println!(
                    "    [{sid}] type={:?} is_correction={} syllable_id={}",
                    d.spelling_type(),
                    d.is_correction(),
                    d.syllable_id
                );
            }
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("用法: prism_info <prism.bin> [检索键...]");
        return;
    };
    let keys: Vec<String> = args.collect();
    info(&path, &keys);
}
