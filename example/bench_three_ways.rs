//! 两组对照：**只有 transformer** / **全无（纯词典）**。
//!
//! 历史：2026-11-03 之前还有第三组「有 gram（octagram 语法模型）」。
//! 实测三组 top-1/top-3 完全一致（gram 在这批用例上不出力），gram 已于同日
//! 从引擎整体移除，故这里只留剩下两组。
//!
//! 开关是进程级 static，所以每组必须独立进程跑：
//!
//! ```text
//! cargo run --release --example bench_three_ways -- trans
//! WEASEL_PREDICT=off cargo run --release --example bench_three_ways -- none
//! ```
//!
//! 度量口径：
//! * **引擎启动**：`Session::create` 到可用
//! * **每键耗时**：逐键 `process_key` 累加 / 键数，**不含**异步重排（宿主也不等它）
//! * **top-1 / top-3**：期望串是否落到第 1 位 / 前 3 位
//!
//! 神经重排是异步的：提交任务后要等它落地，再由**下一轮 compose** 消费。
//! 这里用「sleep 200ms + 手动 compose」模拟宿主的下一次按键。

use librime_next::engine::Session;
use librime_next::engine::gear::rescore;
use std::path::PathBuf;
use std::time::Instant;

/// (输入码, 期望上屏串)。前 8 条是词表就能搞定的常见词，用来验证两组不该有差异；
/// 后面是长句/多词组合，transformer 的差异只在这里可能显形。
const CASES: &[(&str, &str)] = &[
    ("nihao", "你好"),
    ("women", "我们"),
    ("xiexie", "谢谢"),
    ("jisuanji", "计算机"),
    ("diannao", "电脑"),
    ("zhongguorenmin", "中国人民"),
    ("tianqibucuo", "天气不错"),
    ("yijianzhongqing", "一见钟情"),
    ("jintiantianqihenhao", "今天天气很好"),
    ("womenyiqiqukandianying", "我们一起去看电影"),
    ("zhegewentibijiaofuza", "这个问题比较复杂"),
    ("tamenlianmangpaoguolai", "他们连忙跑过来"),
    ("womingtianbushangban", "我明天不上班"),
    ("qingchunyongbuhui", "青春永不会"),
    ("zhonghuarenmingongheguo", "中华人民共和国"),
    ("xuexishiyizhongxingfu", "学习是一种幸福"),
    ("shenghuoxuyaojianchi", "生活需要坚持"),
    ("haodewozhidaole", "好的我知道了"),
    ("jinnianxiangmaichezi", "今年想买车子"),
    ("mingtianhuihenleng", "明天会很冷"),
];

fn top_candidates(s: &Session, n: usize) -> Vec<String> {
    let Some(ctx) = s.context() else {
        return Vec::new();
    };
    let Some(seg) = ctx.composition.segmentation.segments.last() else {
        return Vec::new();
    };
    let Some(menu) = seg.menu.as_ref() else {
        return Vec::new();
    };
    let count = menu.prepare(n);
    let mut out = Vec::new();
    for i in 0..count {
        if let Some(c) = menu.candidate_at(i) {
            out.push(c.text);
        }
    }
    out
}

fn main() {
    let mut args = std::env::args().skip(1);
    let group = args.next().unwrap_or_else(|| "none".to_string());
    let user = PathBuf::from(std::env::var("APPDATA").expect("APPDATA")).join("Rime-Next");
    let build = user.join("build");
    // 共享根必须给：`simplifier` 的 opencc 数据在那儿，少了它整组输出会变繁体。
    let shared = PathBuf::from(
        args.next()
            .unwrap_or_else(|| "../Rime-Next".to_string()),
    );
    let roots = vec![user.clone(), shared.clone()];
    let neural = group == "trans";
    if neural {
        rescore::init_from_data_roots(&roots);
    }

    // 方案 id：默认老的 `luna_pinyin`（繁体）；2026-10-28 改名后用 `BENCH_SCHEMA=pinyin.luna`
    // 才是简化字口径——期望串是简体，拿繁体方案量会全是"miss"，那是口径错不是排序错。
    let schema = std::env::var("BENCH_SCHEMA").unwrap_or_else(|_| "luna_pinyin".to_string());
    // 引擎启动：`Session::create` 到可用
    let t0 = Instant::now();
    let mut s = Session::create_with_roots(&build, &schema, &roots);
    let boot_ms = t0.elapsed().as_secs_f64() * 1000.0;
    if s.engine.is_none() {
        eprintln!("引擎创建失败（schema={schema}, build={}）", build.display());
        std::process::exit(1);
    }
    println!(
        "组={group}  schema={schema}  引擎启动={boot_ms:.0}ms  神经层={}  用例={}",
        if neural { "on" } else { "off" },
        CASES.len()
    );

    let (mut hit1, mut hit3, mut total_us, mut total_keys) = (0usize, 0usize, 0u128, 0usize);

    // 预热：首次 compose 带着词表/缓存的懒加载，不计入统计，否则第一条吃掉全部账。
    for ch in "nihao".chars() {
        s.process_key(ch as u32, 0);
    }
    s.clear_composition();

    for (input, expect) in CASES {
        s.clear_composition();
        let mut us = 0u128;
        for ch in input.chars() {
            let t = Instant::now();
            s.process_key(ch as u32, 0);
            us += t.elapsed().as_micros();
        }
        let nkeys = input.chars().count();
        total_us += us;
        total_keys += nkeys;

        if neural {
            std::thread::sleep(std::time::Duration::from_millis(200));
            if let Some(engine) = s.engine.as_mut() {
                engine.compose();
            }
        }

        let cands = top_candidates(&s, 3);
        let ok1 = cands.first().is_some_and(|c| c == expect);
        let ok3 = cands.iter().any(|c| c == expect);
        if ok1 {
            hit1 += 1;
        }
        if ok3 {
            hit3 += 1;
        }
        let mark = if ok1 {
            "top1"
        } else if ok3 {
            "top3"
        } else {
            " miss"
        };
        println!(
            "  [{mark}] {:<24} 期望={:<12} 实得={:?}  每键{}µs",
            input,
            expect,
            cands,
            us / nkeys.max(1) as u128
        );
    }

    let n = CASES.len() as f64;
    println!(
        "== {group}: top1 {hit1}/{} ({:.0}%)  top3 {hit3}/{} ({:.0}%)  每键 {:.0}µs ==",
        CASES.len(),
        hit1 as f64 / n * 100.0,
        CASES.len(),
        hit3 as f64 / n * 100.0,
        total_us as f64 / total_keys.max(1) as f64
    );
}
