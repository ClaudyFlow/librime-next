//! 隔离验证并行会话的 opencc 链:load(共享目录 t2s.json) → convert("天氣不錯")。
use librime_next::dict::opencc::Opencc;

fn main() {
    let shared = std::path::PathBuf::from(r"C:\Program Files\Weasel-Next\Rime-Next");
    let opencc = Opencc::load(shared.join("opencc").join("t2s.json"));
    match opencc {
        Ok(o) => {
            let out = o.convert("天氣不錯");
            println!("天氣不錯 -> {out}");
            let out2 = o.convert("妳好");
            println!("妳好 -> {out2}");
        }
        Err(e) => println!("load 失败: {e}"),
    }
}
