//! 全链路里 **引擎内部** 的分段计时采集（2026-11-08）。
//!
//! 目的：把 `process_key` 拆成 处理器链 / 音节切分 / 查词装配 / 神经重排 / 上屏学词，
//! 按键类型（组字 / 提交 / 翻页 / 首键 / 稳态）分别落进 CSV，供统计脚本出表。
//!
//! - 埋点在 `runtime.rs`，`WEASEL_STAGE_TRACE=1` 才计时，平时全 0；
//! - 本探针只在 `%TEMP%\rime_perf_copy` 的副本上打字，模型从共享目录只读加载，
//!   不写 `%APPDATA%`、不动 `C:\Program Files`。
//!
//! 输出：`<TEMP>\stage-trace.csv`（单位全是**微秒**），列：
//! `idx,kind,key,total_us,proc_us,seg_us,trans_us,filter_us,commit_us,eaten,composing,committed,input_chars,cands`
use librime_next::engine::Session;
use std::io::Write as _;
use std::path::PathBuf;

const SPACE: u32 = 0x20;
const PAGE_DOWN: u32 = 0xFF56;
const PAGE_UP: u32 = 0xFF55;
const ESC: u32 = 0x1B;

fn kind_of(key: u32) -> &'static str {
    match key {
        SPACE | 0x31..=0x39 => "commit",
        PAGE_DOWN | PAGE_UP | 0x5B | 0x5D => "page",
        0x41..=0x5A | 0x61..=0x7A => "compose",
        _ => "other",
    }
}

fn main() {
    // 埋点开关：必须在第一次 process_key 之前设置（库内 OnceLock 读一次）。
    unsafe { std::env::set_var("WEASEL_STAGE_TRACE", "1") };

    let copy = std::env::temp_dir().join("rime_perf_copy");
    let build = copy.join("build");
    let data = PathBuf::from(r"C:\Program Files\Rime-Next\Weasel-Next\data");
    librime_next::gear::filter::rescore::init_from_data_roots(&[copy.clone(), data.clone()]);
    let mut session =
        Session::create_with_roots(&build, "luna_pinyin_simp", &[copy.clone(), data.clone()]);

    let words = [
        "nihao", "jintian", "tianqi", "henhao", "women", "xiexie", "mingtian", "gongzuo",
        "shunli", "zaijian", "pengyou", "xiangshou",
    ];

    let mut rows = String::new();
    let mut idx = 0usize;
    for _round in 0..6 {
        for (i, code) in words.iter().enumerate() {
            for ch in code.chars() {
                let key = ch as u32;
                let eaten = session.process_key(key, 0);
                let committed = session.take_commit().map_or(0, |t| t.chars().count());
                push_row(&mut rows, &mut idx, &session, key, eaten, committed);
            }
            // 每个词都翻两页（下/上），保证翻页键有足够样本算 p99。
            for page_key in [PAGE_DOWN, PAGE_UP] {
                let eaten = session.process_key(page_key, 0);
                let committed = session.take_commit().map_or(0, |t| t.chars().count());
                push_row(&mut rows, &mut idx, &session, page_key, eaten, committed);
            }
            // 一半空格上屏，一半数字 1 选首候选。
            let commit_key = if i % 2 == 0 { SPACE } else { 0x31 };
            let eaten = session.process_key(commit_key, 0);
            let committed = session.take_commit().map_or(0, |t| t.chars().count());
            push_row(&mut rows, &mut idx, &session, commit_key, eaten, committed);
            // 没上屏就清场，避免下一页输入被上一页的残留污染。
            if session.engine.as_ref().is_some_and(|e| e.context.is_composing()) {
                let eaten = session.process_key(ESC, 0);
                let committed = session.take_commit().map_or(0, |t| t.chars().count());
                push_row(&mut rows, &mut idx, &session, ESC, eaten, committed);
            }
        }
    }

    let out = std::env::temp_dir().join("stage-trace.csv");
    let mut f = std::fs::File::create(&out).expect("写 stage-trace.csv");
    f.write_all(
        b"idx,kind,key,total_us,proc_us,seg_us,trans_us,filter_us,commit_us,eaten,composing,committed,input_chars,cands\n",
    )
    .unwrap();
    f.write_all(rows.as_bytes()).unwrap();
    println!("写出 {} 行：{}", idx, out.display());
}

fn push_row(
    rows: &mut String,
    idx: &mut usize,
    session: &Session,
    key: u32,
    eaten: bool,
    committed: usize,
) {
    let Some(engine) = session.engine.as_ref() else {
        return;
    };
    let s = engine.last_stage_timings();
    let ctx = &engine.context;
    let kind = if *idx == 0 { "first" } else { kind_of(key) };
    rows.push_str(&format!(
        "{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
        *idx,
        kind,
        key,
        s.total_us,
        s.proc_us,
        s.seg_us,
        s.trans_us,
        s.filter_us,
        s.commit_us,
        u8::from(eaten),
        u8::from(ctx.is_composing()),
        committed,
        ctx.input.chars().count(),
        ctx.menu_candidate_count(),
    ));
    *idx += 1;
}
