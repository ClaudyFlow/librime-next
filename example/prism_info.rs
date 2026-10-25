// 快速对比两个 prism.bin 的结构标量，判断「功能等价」还是「拼写集合不同」。
// 用法：cargo run --example prism_info <a.prism.bin> <b.prism.bin>
use librime_next::dict::prism::Prism;

fn info(path: &str) {
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
    // 抽样几个输入，看 common_prefix_search 返回的 spelling 数量
    for s in ["nihao", "ni", "hao", "zhong", "guo", "wo", "de", "a"] {
        let m = prism.common_prefix_search(s);
        let ids: Vec<_> = m.iter().map(|x| x.value).collect();
        println!("  search('{s}') -> spelling_ids={ids:?}");
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
    for p in std::env::args().skip(1) {
        info(&p);
        println!();
    }
}
