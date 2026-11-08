//! 验证「上屏后联想条不再重复刚上屏文字」：直接调双模型同步预测打印结果。
//!
//! ```text
//! cargo run -p librime-next --example probe_predict_repeat
//! ```
//!
//! 用 `CARGO_MANIFEST_DIR/test-data` 下的两份权重，和 live 完全同一条
//! `rescore::predict_sync(已上屏文本, 刚上屏词, 4)` 路径。
//! 修复前：context=提交前上文、word 当续写 prefix ⇒ 每条都是「word + 续写」。
//! 修复后：context=提交前上文+word、prefix 空 ⇒ 模型直接生成下一个词。
//! 逐条标注它与刚上屏词的关系，用来守「不再重复」。

use std::path::PathBuf;

use librime_next::engine::gear::rescore;

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-data");
    rescore::init_from_data_roots(&[root.clone()]);
    if rescore::global().is_none() {
        eprintln!("模型没加载：确认 {}/model 下有两个 safetensors", root.display());
        return;
    }
    println!("数据根: {}", root.display());
    println!("=== 上屏联想条（context = 提交前上文, word = 刚上屏词, total = 4）===\n");

    let cases: &[(&str, &str)] = &[
        ("", "你好"),
        ("", "中国"),
        ("我今天", "你好"),
        ("", "我们"),
        ("", "哈哈"),
        ("", "的"),
        ("他刚才说", "可以"),
    ];
    for (context, word) in cases {
        // 和 runtime.rs 一致：提交前上文 + 刚上屏的词 = 完整已上屏文本。
        let full_context = format!("{context}{word}");
        let out = rescore::predict_sync(&full_context, word, 4);
        println!("ctx=[{context}]  刚上屏=[{word}]  联想 {} 条:", out.len());
        if out.is_empty() {
            println!("   (空 → 回退个人 n-gram)");
        }
        for (i, (text, score)) in out.iter().enumerate() {
            let flag = if text == word {
                "   <== 完全重复刚上屏"
            } else if text.starts_with(word) {
                "   <== 以刚上屏词开头"
            } else {
                ""
            };
            println!("   #{i} {text:?}  score={score:.3}{flag}");
        }
        println!();
    }
}
