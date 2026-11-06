//! 社区方案的 lua 组件装配探针：看 `lua_processor` / `lua_translator` /
//! `lua_filter` 到底挂上了几个、脚本有没有真的产出候选。
//!
//! 用法（Windows / Git Bash）：
//! ```text
//! cargo run --example probe_lua_schema -- <用户目录> <共享目录> <方案 id> [按键...]
//! ```
//!
//! 额外的数据根（例如把出厂 `Rime-Next/` 当 opencc 回退）用
//! `--root <目录>` 追加，可重复多次。

use std::path::PathBuf;

use librime_next::engine::key_event::KeyEvent;
use librime_next::engine::runtime::Engine;

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let fallback = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(".tmp_qiaoma");
    let mut positional: Vec<String> = Vec::new();
    let mut extra_roots: Vec<PathBuf> = Vec::new();
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == "--root" {
            if let Some(dir) = raw.get(index + 1) {
                extra_roots.push(PathBuf::from(dir));
            }
            index += 2;
            continue;
        }
        positional.push(raw[index].clone());
        index += 1;
    }
    let user_dir = positional
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.join("user"));
    let shared_dir = positional
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.join("shared"));
    let schema_id = positional
        .get(2)
        .cloned()
        .unwrap_or_else(|| "qiaoma".to_string());
    let keys: Vec<String> = if positional.len() > 3 {
        positional[3..].to_vec()
    } else {
        vec!["c".into(), "o".into()]
    };

    let mut roots = vec![user_dir.clone(), shared_dir.clone()];
    roots.extend(extra_roots);
    let build_dir = user_dir.join("build");
    let Some(mut engine) = Engine::create_with_roots(&build_dir, &schema_id, &roots) else {
        eprintln!("装配失败: {} / {schema_id}", build_dir.display());
        return;
    };
    println!("数据根 {roots:?}");

    println!("方案 {schema_id} 装配完成");
    println!("  processors : {:?}", engine.processor_names());
    println!("  segmentors : {:?}", engine.segmentor_names());
    println!("  translators: {:?}", engine.translator_names());
    println!("  filters    : {:?}", engine.filter_names());

    for name in keys {
        let key = if let Some(single) = name.strip_prefix("Shift+") {
            KeyEvent::new(
                single.chars().next().unwrap().to_ascii_uppercase() as u32,
                librime_next::engine::key_event::K_SHIFT_MASK,
            )
        } else if name.chars().count() == 1 {
            let ch = name.chars().next().unwrap();
            // 大写字母在真实宿主里是「Shift + 小写键」，只传大写 keysym 会被
            // ascii_composer 判成"要打 ASCII"而切进 ascii_mode（raw 段）
            let modifier = if ch.is_ascii_uppercase() {
                librime_next::engine::key_event::K_SHIFT_MASK
            } else {
                0
            };
            KeyEvent::new(ch as u32, modifier)
        } else {
            KeyEvent::new(
                librime_next::engine::key_event::keycode_by_name(&name).unwrap_or(0),
                0,
            )
        };
        let handled = engine.process_key(&key);
        println!(
            "\n按 `{name}` (0x{:x}+{:#x}) handled={handled} input=`{}`",
            key.keycode,
            key.modifier,
            engine.context.input
        );
        let count = match engine.context.composition.segmentation.segments.last().and_then(|s| s.menu.as_ref()) {
            Some(menu) => {
                menu.prepare(8);
                for index in 0..menu.candidate_count().min(8) {
                    if let Some(candidate) = menu.candidate_at(index) {
                        println!(
                            "  [{index}] {}  <{}>  type={}",
                            candidate.text, candidate.comment, candidate.kind
                        );
                    }
                }
                menu.candidate_count()
            }
            None => 0,
        };
        println!("  候选数 {count}");
        if let Some(text) = engine.context.commit_text.as_ref() {
            println!("  提交文本 {text:?}");
        }
        if engine.context.get_option("ascii_mode") {
            println!("  （ascii_mode 开着 ⇒ ascii_segmentor 会接管成 raw 段）");
        }
        for segment in &engine.context.composition.segmentation.segments {
            println!(
                "  seg {}..{} tags={:?} prompt={:?}",
                segment.start, segment.end, segment.tags, segment.prompt
            );
        }
    }
}
