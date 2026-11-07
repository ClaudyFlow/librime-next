//! 方案选单空格交互探针：`Control+grave` → `Space` ×N，逐步打印
//! `schema / input / commit_text / dumb / 菜单`，定位「空格要按两下」与「上屏空格」。
//!
//! 用法（Windows / Git Bash）：
//! ```text
//! cargo run --example probe_switcher_space -- <用户目录> <共享目录> [方案 id]
//! ```
//!
//! 注意：`commit_text` 等价于宿主 `get_commit` 取到的文本——它非空就是「上屏」。

use std::path::PathBuf;

use librime_next::engine::key_event::{KeyEvent, K_CONTROL_MASK, XK_SPACE};
use librime_next::engine::runtime::Engine;

fn main() {
    let user = PathBuf::from(std::env::args().nth(1).expect("user_root"));
    let shared = PathBuf::from(std::env::args().nth(2).expect("shared_root"));
    let schema_id =
        std::env::args().nth(3).unwrap_or_else(|| "double_pinyin.ziranma".to_string());
    let roots = vec![user.clone(), shared];
    let build = user.join("build");

    let Some(mut eng) = Engine::create_with_roots(&build, &schema_id, &roots) else {
        eprintln!("装配失败: {} / {schema_id}", build.display());
        return;
    };

    let dump = |tag: &str, eng: &mut Engine| {
        let ctx = &mut eng.context;
        // 宿主 get_commit 会 take 走，这里同样取一次，避免下一步累加误判
        let commit = ctx.commit_text.take();
        let last = ctx.composition.segmentation.segments.last();
        let cands: Vec<String> = last
            .and_then(|s| s.menu.as_ref())
            .map(|m| {
                m.all_candidates()
                    .iter()
                    .take(6)
                    .map(|c| c.text.clone())
                    .collect()
            })
            .unwrap_or_default();
        let sel = last.map_or(0, |s| s.selected_index);
        println!(
            "{tag}: schema={} input={:?} commit={:?} dumb={} fold={} segs={} sel={} cands={:?}",
            eng.schema.schema_id,
            ctx.input,
            commit,
            ctx.get_option("dumb"),
            ctx.get_option("_fold_options"),
            ctx.composition.segmentation.segments.len(),
            sel,
            cands,
        );
    };

    dump("INIT", &mut eng);

    // Control+grave = keysym 0x60 + Control 位
    let ctrl_grave = KeyEvent::new(0x60, K_CONTROL_MASK);
    println!("CTRL-GRAVE eaten={}", eng.process_key(&ctrl_grave));
    dump("AFTER-CTRL", &mut eng);

    for i in 1..=3 {
        let space = KeyEvent::new(XK_SPACE, 0);
        println!("SPACE#{i} eaten={}", eng.process_key(&space));
        dump(&format!("AFTER-SPACE#{i}"), &mut eng);
    }
}
