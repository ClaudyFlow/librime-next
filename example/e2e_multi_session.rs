//! 多会话学词探针：复现**真机**的进程形态。
//!
//! 真机 `Weasel-Next-Engine.exe` 是长驻进程，`server.log` 里能看到
//! `idle sessions reclaimed; process and connections stay alive reclaimed=4/6`——
//! 也就是说**同一进程里会同时存在多个会话**，而每个会话都会新建一个
//! `Engine`，各自去打开同一个 `userdb`。LevelDB 的 `LOCK` 按目录独占，
//! 于是"先开的赢、后开的静默丢掉用户词典"，学词就时好时坏。
//!
//! 单会话的 `e2e_learn` 永远测不出这个坑，所以单独写这一支：
//! 会话 A 先占住 userdb，会话 B（相当于用户切到另一个输入框）必须**照样能学**。
//!
//! 用法：
//!   cargo run --release --example e2e_multi_session -- <build_dir> <user_root> <shared_root> <schema> <input>

use librime_next::engine::Session;
use std::path::{Path, PathBuf};

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

/// 通过**进程级共享句柄**读库 —— 会话还活着时也能读（证明拿的是同一个实例）。
fn dump(dir: &Path, name: &str) -> Vec<String> {
    use librime_next::dict::db::{Db, DbAccessor};
    use librime_next::dict::level_db::LevelDb;
    let db = LevelDb::shared_user_db(dir, name);
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

    let roots = vec![user.clone(), shared];
    std::fs::create_dir_all(&user).ok();
    let userdb = user.join("userdb");

    // ---- 会话 A：先占住 userdb（真机里最先打开输入框的那个会话）----
    let mut a = Session::create_with_roots(&build, &schema, &roots);
    if a.engine.is_none() {
        println!("会话A: ENGINE None（方案加载失败）");
        return;
    }
    type_input(&mut a, &input);
    println!("会话A 初始候选: {:?}", top(&a, 6));
    let Some(_) = a.choose(0) else {
        println!("会话A choose(0) 失败");
        return;
    };
    println!("会话A 上屏: {:?}", a.take_commit());

    // ---- 会话 B：A 还活着时新建（真机里换一个输入框/应用）----
    let mut b = Session::create_with_roots(&build, &schema, &roots);
    if b.engine.is_none() {
        println!("会话B: ENGINE None（方案加载失败）");
        return;
    }
    type_input(&mut b, &input);
    let before = top(&b, 6);
    println!("会话B 初始候选: {:?}", before);
    // 故意选一个非首选，这样写进库的词条在候选里一眼能认出来
    let pick = 2.min(before.len().saturating_sub(1));
    let Some(_) = b.choose(pick) else {
        println!("会话B choose({pick}) 失败");
        return;
    };
    let picked = b.take_commit().unwrap_or_default();
    println!("会话B 选[{pick}] 上屏: {picked:?}");

    // ---- 会话 B 再打一遍：学过的词应该已经升上来 ----
    b.clear_composition();
    type_input(&mut b, &input);
    println!("会话B 学完后候选: {:?}", top(&b, 6));

    // ---- 两个会话都活着时直接读库 ----
    println!("=== 库内容（A、B 均未释放）===");
    let mut lines = dump(&userdb, "pinyin");
    if lines.is_empty() {
        println!("<空：一个词都没写进去>");
    }
    for line in lines.drain(..) {
        println!("{line}");
    }

    // ---- 会话 A 还活着也必须能继续学（共享句柄不能把 A 弄坏）----
    a.clear_composition();
    type_input(&mut a, &input);
    println!("会话A 第二轮回候选: {:?}", top(&a, 6));
    if let Some(_) = a.choose(1.min(top(&a, 6).len().saturating_sub(1))) {
        println!("会话A 第二次上屏: {:?}", a.take_commit());
    }

    println!("=== 库内容（A 第二次上屏之后）===");
    for line in dump(&userdb, "pinyin") {
        println!("{line}");
    }
}
