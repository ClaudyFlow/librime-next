//! 复现「同一输入反复 compose，候选/切分自己变」。
//!
//! 背景（2026-11-03 16:1x）：神经重排落表后由 `rime_get_context` 里的
//! `poll_rescore_landing` 触发 re-compose；但探针里看到**每一轮 compose 的候选批次都不一样**，
//! 于是 `order_for` 的键（context + 候选文本列表）永远对不上，重排从不命中。
//!
//! 本探针把这件事单独拎出来：一次打字，然后**不按键**地连续 compose N 次，
//! 每次打印切分与各段头部候选。若切分/候选变了 ⇒ `Segmentation::reset` 的
//! 「差量保留」在同输入下不是幂等的。
//!
//! 用法：
//!   cargo run --release --example probe_recompose -- <build_dir> <user_root> <shared_root> <schema> <input> [轮数]

use librime_next::engine::Session;
use std::path::PathBuf;

fn type_input(s: &mut Session, input: &str) {
    for ch in input.chars() {
        s.process_key(ch as u32, 0);
    }
}

fn dump(s: &Session, tag: &str) {
    let Some(ctx) = s.context() else {
        println!("{tag}: <没有上下文>");
        return;
    };
    let segs = ctx
        .composition
        .segmentation
        .segments
        .iter()
        .map(|g| format!("[{}..{}]{:?}", g.start, g.end, g.status))
        .collect::<Vec<_>>()
        .join(" ");
    println!("{tag} segs={segs}");
    for (i, seg) in ctx.composition.segmentation.segments.iter().enumerate() {
        let Some(menu) = seg.menu.as_ref() else {
            continue;
        };
        let cands: Vec<String> = (0..6)
            .filter_map(|k| {
                menu.candidate_at(k)
                    .map(|c| format!("{}@{:.0}", c.text, c.quality))
            })
            .collect();
        println!("      seg{i}: {}", cands.join(" "));
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let build = PathBuf::from(args.next().expect("build_dir"));
    let user = PathBuf::from(args.next().expect("user_root"));
    let shared = PathBuf::from(args.next().expect("shared_root"));
    let schema = args.next().expect("schema_id");
    let input = args.next().expect("input");
    let rounds: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(3);

    let roots = vec![user.clone(), shared];
    let mut s = Session::create_with_roots(&build, &schema, &roots);
    if s.engine.is_none() {
        println!("ENGINE: None（方案加载失败）");
        return;
    }
    println!("# schema={schema} input={input} 轮数={rounds}");
    type_input(&mut s, &input);
    dump(&s, "按键后");

    // ① 直接反复 compose —— 只测 `Segmentation::reset` 同输入下是否幂等。
    for i in 1..=rounds {
        if let Some(e) = s.engine.as_mut() {
            e.compose();
        }
        dump(&s, &format!("compose#{i}"));
    }

    // ② 宿主那条读法：`menu.page(5, 0)`（c_api 的 get_context 就是这么取页的）。
    //    注意 page 是 `&self` + RefCell 缓存，理论上只追加、不该改变已发出的顺序。
    for i in 1..=rounds {
        let Some(ctx) = s.context() else { break };
        let Some(menu) = ctx.composition.segmentation.segments.first().and_then(|g| g.menu.as_ref()) else {
            println!("page#{i}: <没有菜单>");
            continue;
        };
        let (page, is_last) = menu.page(5, 0);
        println!(
            "page#{i} is_last={is_last} => {}",
            page.iter()
                .map(|c| format!("{}@{:.0}", c.text, c.quality))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    // ③ 走宿主那条路：sleep → poll_rescore_landing（有落地才 compose）。
    librime_next::engine::gear::rescore::init_from_data_roots(&roots);
    for i in 1..=rounds {
        std::thread::sleep(std::time::Duration::from_millis(300));
        s.poll_rescore_landing();
        dump(&s, &format!("poll#{i}"));
    }

    // ④ 模拟「修好的 poll」：把最后一段的菜单清掉（等价 delete_candidate 的 reopen），
    //    再 compose —— 段会被**重新翻译**，滤镜套在**新流**上，结果表才有机会命中。
    //    （当前代码里 `translate_segments` 会跳过「Guess 且有菜单」的段，
    //     于是 apply_filters 每次都在**已消费过的流**上再套一个滤镜 —— 头部被啃。）
    use librime_next::engine::segmentation::SegStatus;
    for i in 1..=rounds {
        if let Some(seg) = s
            .engine
            .as_mut()
            .and_then(|e| e.context.composition.segmentation.segments.last_mut())
        {
            seg.menu = None;
            seg.status = SegStatus::Guess;
        }
        if let Some(e) = s.engine.as_mut() {
            e.compose();
        }
        dump(&s, &format!("retranslate#{i}"));
    }
}
