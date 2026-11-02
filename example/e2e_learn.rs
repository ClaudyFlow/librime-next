//! 端到端「学词」探针：用真实引擎打字 → 选词上屏 → 立刻查候选是否提前。
//!
//! 用途：验证 `apply_commit → memorize → UserDictionary::update_entry` 这条链
//! 在**某个具体方案**上到底有没有把词写进用户词典，以及下次同样输入时
//! 学过的词有没有排到前面。
//!
//! 用法：
//!   cargo run --release --example e2e_learn -- <build_dir> <user_root> <shared_root> <schema> <input> [轮数]
//! 例：
//!   cargo run --release --example e2e_learn -- "$APPDATA/Rime-Next/build" /tmp/lrn \
//!       "C:/Program Files/Rime-Next/Weasel-Next-0.18.5/Rime-Next" pinyin yibutini 12

use librime_next::engine::Session;
use std::path::PathBuf;

fn type_input(s: &mut Session, input: &str) {
    for ch in input.chars() {
        s.process_key(ch as u32, 0);
    }
}

fn top(s: &Session, n: usize) -> Vec<String> {
    let Some(ctx) = s.context() else {
        return Vec::new();
    };
    let Some(seg) = ctx.composition.segmentation.segments.first() else {
        return Vec::new();
    };
    let Some(menu) = seg.menu.as_ref() else {
        return Vec::new();
    };
    (0..n)
        .filter_map(|i| menu.candidate_at(i).map(|c| c.text.clone()))
        .collect()
}

/// 带质量分（`E2E_Q=1` 时用来核对排序依据）。
fn top_q(s: &Session, n: usize) -> Vec<String> {
    let Some(ctx) = s.context() else {
        return Vec::new();
    };
    let Some(seg) = ctx.composition.segmentation.segments.first() else {
        return Vec::new();
    };
    let Some(menu) = seg.menu.as_ref() else {
        return Vec::new();
    };
    (0..n)
        .filter_map(|i| {
            menu.candidate_at(i)
                .map(|c| format!("{}({:.1})", c.text, c.quality))
        })
        .collect()
}

fn dump_userdb(dir: &PathBuf) -> Vec<String> {
    use librime_next::dict::db::{Db, DbAccessor};
    use librime_next::dict::level_db::LevelDb;
    let db = LevelDb::shared_user_db(dir, "pinyin");
    if !db.loaded() && !db.open() {
        return vec!["<无法打开 userdb>".into()];
    }
    let Some(mut acc) = db.query_all() else {
        return vec!["<query_all 为空>".into()];
    };
    let mut out = Vec::new();
    while let Some((k, v)) = acc.get_next_record() {
        out.push(format!("{k}\t{v}"));
        if out.len() > 40 {
            break;
        }
    }
    out
}

fn main() {
    let mut args = std::env::args().skip(1);
    let build = PathBuf::from(args.next().expect("build_dir"));
    let user = PathBuf::from(args.next().expect("user_root"));
    let shared = PathBuf::from(args.next().expect("shared_root"));
    let schema = args.next().expect("schema_id");
    let input = args.next().expect("input");
    let rounds: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);
    // 选第几个候选上屏（默认 0）。想验证「学过之后能排到第一页」就设成 2/3。
    let pick: usize = std::env::var("E2E_PICK")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let roots = vec![user.clone(), shared];
    std::fs::create_dir_all(&user).ok();

    let mut s = Session::create_with_roots(&build, &schema, &roots);
    if s.engine.is_none() {
        println!("ENGINE: None（方案加载失败）");
        return;
    }
    println!("# schema={schema} input={input} 轮数={rounds}");

    // 逐字选（模拟"候选里没有词组、只能一个字一个字选"）：
    //   E2E_PICK=char cargo run --release --example e2e_learn -- ... <input> 1
    if std::env::var("E2E_PICK").as_deref() == Ok("char") {
        type_input(&mut s, &input);
        let mut picked = Vec::new();
        for _ in 0..12 {
            let Some(ctx) = s.context() else { break };
            let Some(seg) = ctx.composition.segmentation.segments.first() else {
                break;
            };
            let Some(menu) = seg.menu.as_ref() else { break };
            // 找第一个单字候选（跨度为 1 个音节、文本 1 个字）
            let mut idx = None;
            for i in 0..12 {
                if let Some(c) = menu.candidate_at(i) {
                    if c.text.chars().count() == 1 {
                        idx = Some(i);
                        break;
                    }
                }
            }
            let Some(i) = idx else {
                println!("没有单字候选了，停止");
                break;
            };
            let text = menu.candidate_at(i).map(|c| c.text.clone()).unwrap_or_default();
            picked.push(text);
            if s.choose(i).is_none() {
                break;
            }
            if let Some(c) = s.take_commit() {
                println!("上屏: {c:?}");
                break;
            }
        }
        println!("逐字选过程: {picked:?}");
        println!("=== userdb/pinyin.userdb ===");
        for line in dump_userdb(&user.join("userdb")) {
            println!("{line}");
        }
        return;
    }

    // 第 1 轮：看没学过的初始顺序
    type_input(&mut s, &input);
    if std::env::var_os("E2E_Q").is_some() {
        println!("第 1 次候选(含质量): {:?}", top_q(&s, 6));
    } else {
        println!("第 1 次候选: {:?}", top(&s, 6));
    }

    // 选中并上屏，然后再打一遍（同会话），看有没有提前
    for round in 2..=rounds {
        s.clear_composition();
        type_input(&mut s, &input);
        let before = top(&s, 6);
        let Some(res) = s.choose(pick) else {
            println!("第 {round} 轮 choose({pick}) 失败");
            break;
        };
        let committed = s.take_commit().unwrap_or_default();
        println!("第 {round} 轮: 选[{pick}]={committed:?} 候选原序={before:?}");
    }

    // 学完再打一次，看首选有没有变成学过的词
    s.clear_composition();
    type_input(&mut s, &input);
    println!("学完后候选: {:?}", top(&s, 6));
    drop(s);

    // 直接看落盘的用户词典
    println!("=== userdb/pinyin.userdb ===");
    for line in dump_userdb(&user.join("userdb")) {
        println!("{line}");
    }
}
