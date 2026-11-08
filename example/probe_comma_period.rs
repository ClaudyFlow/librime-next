//! 逗号/句号候选光标探针：验证 key_bindings 里的
//!   `{when: has_menu, accept: comma, send: Up}` / `{... period, send: Down}`
//! 是否真的移动 `selected_index`，并拿物理方向键 / 翻页键做对照。
//!
//! 用法（Windows / Git Bash）：
//! ```text
//! cargo run --release --example probe_comma_period -- <用户目录> <共享目录>
//! ```
//! 例：
//! ```text
//! cargo run --release --example probe_comma_period -- \
//!   'C:\Users\CloudyFlow\AppData\Roaming\Rime-Next' \
//!   'C:\Program Files\Rime-Next\Weasel-Next\data'
//! ```

use std::path::PathBuf;

use librime_next::engine::key_event::{KeyEvent, XK_DOWN, XK_NEXT, XK_PRIOR, XK_UP};
use librime_next::engine::runtime::Engine;

/// (方案 id, 触发候选的编码)。前半是点号命名（恒等式系列文件夹，新产物），
/// 后半是旧扁平/下划线命名（对照，看旧 Page_Up/Page_Down 是否真的不动）。
const CASES: &[(&str, &str)] = &[
    ("cangjie.5", "a"),
    ("double_pinyin.ziranma", "nihao"),
    ("pinyin.luna", "nihao"),
    ("double_pinyin.flypy", "nihao"),
    ("pinyin.terra", "nihao"),
    ("stroke", "123"),
    ("luna_pinyin", "nihao"),
    ("double_pinyin_ziranma", "nihao"),
];

fn dump(tag: &str, eng: &mut Engine) {
    let ctx = &mut eng.context;
    let commit = ctx.commit_text.take();
    let input = ctx.input.clone();
    let last = ctx.composition.segmentation.segments.last_mut();
    let (sel, cur, cands) = match last {
        Some(seg) => {
            let sel = seg.selected_index;
            match seg.menu.as_mut() {
                Some(m) => {
                    m.prepare(8);
                    let texts: Vec<String> = (0..m.candidate_count().min(8))
                        .filter_map(|i| m.candidate_at(i))
                        .map(|c| c.text.clone())
                        .collect();
                    let cur = m.candidate_at(sel).map(|c| c.text.clone());
                    (sel, cur, texts)
                }
                None => (sel, None, Vec::new()),
            }
        }
        None => (0, None, Vec::new()),
    };
    println!(
        "  {tag}: input={input:?} commit={commit:?} sel={sel} cur={cur:?} cands={cands:?}"
    );
}

fn main() {
    let user = PathBuf::from(std::env::args().nth(1).expect("user_root"));
    let shared = PathBuf::from(std::env::args().nth(2).expect("shared_root"));
    let roots = vec![user.clone(), shared];
    let build = user.join("build");

    println!("# build = {}", build.display());
    println!("# 判定：sel 变化 = 光标移动；cur 随之变化 = 高亮真的换了候选\n");

    for (schema, input) in CASES {
        println!("=== {schema}  input={input:?} ===");
        let Some(mut eng) = Engine::create_with_roots(&build, schema, &roots) else {
            println!("  装配失败（该 id 无可加载方案，运行时不会用到）\n");
            continue;
        };
        for ch in input.chars() {
            eng.process_key(&KeyEvent::new(ch as u32, 0));
        }
        dump("BASE        ", &mut eng);

        let comma = KeyEvent::new(0x2c, 0); // XK_comma
        let period = KeyEvent::new(0x2e, 0); // XK_period
        let down = KeyEvent::new(XK_DOWN, 0);
        let up = KeyEvent::new(XK_UP, 0);
        let page_down = KeyEvent::new(XK_NEXT, 0);
        let page_up = KeyEvent::new(XK_PRIOR, 0);

        println!("  KEY comma(,)   eaten={}", eng.process_key(&comma));
        dump("AFTER-COMMA ", &mut eng);
        println!("  KEY period(.)  eaten={}", eng.process_key(&period));
        dump("AFTER-PERIOD", &mut eng);
        println!("  KEY period(.)  eaten={}", eng.process_key(&period));
        dump("AFTER-PERIOD2", &mut eng);
        println!("  KEY Down(phys) eaten={}", eng.process_key(&down));
        dump("AFTER-DOWN  ", &mut eng);
        println!("  KEY Up(phys)   eaten={}", eng.process_key(&up));
        dump("AFTER-UP    ", &mut eng);
        println!("  KEY PageDown   eaten={}", eng.process_key(&page_down));
        dump("AFTER-PGDN  ", &mut eng);
        println!("  KEY PageUp     eaten={}", eng.process_key(&page_up));
        dump("AFTER-PGUP  ", &mut eng);
        println!();
    }
}
