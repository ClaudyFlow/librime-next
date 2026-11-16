//! 诊断探针：**虎整句（tiger_sentence）逐键耗时 / 可见性**。
//!
//! 用户 2026-11-08 报「虎码一直很闪，不是布局的问题」。主题侧的 hide/show 风暴
//! 需要知道引擎每键到底返回什么：`composing` 是否为假、候选是否为 0、这一键睡了多久。
//! 本探针按真实键盘序列逐键打印，用来判断「不可见快照」与长耗时帧是谁产生的。
//!
//! 用法（离线）：
//! ```text
//! cargo run --release --example probe_tiger_latency -- <用户目录副本> <共享目录> <按键串> [方案id]
//! ```

use std::path::PathBuf;
use std::time::Instant;

use librime_next::gear::filter::rescore;
use librime_next::engine::key_event::KeyEvent;
use librime_next::engine::runtime::Engine;

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let user_dir = PathBuf::from(raw.first().unwrap_or_else(|| {
        panic!("用法: probe_tiger_latency <用户目录副本> <共享目录> <按键串> [方案id]")
    }));
    let shared_dir = PathBuf::from(raw.get(1).unwrap_or_else(|| panic!("缺共享目录")));
    let keys = raw.get(2).cloned().unwrap_or_else(|| "bpeklbnr".into());
    let schema = raw
        .get(3)
        .cloned()
        .unwrap_or_else(|| "tiger_sentence".into());

    let roots = vec![user_dir.clone(), shared_dir.clone()];
    rescore::init_from_data_roots(&roots);
    println!(
        "rescore worker = {}",
        if rescore::global().is_some() { "Some" } else { "None" }
    );
    let build_dir = user_dir.join("build");
    let mut engine = Engine::create_with_roots(&build_dir, &schema, &roots).expect("方案装配失败");
    println!("schema={schema}");

    let mut slow = 0usize;
    let mut invisible = 0usize;
    for (index, ch) in keys.chars().enumerate() {
        let start = Instant::now();
        let eaten = engine.process_key(&KeyEvent::new(ch as u32, 0));
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        let composing = engine.context.is_composing();
        let candidate_count = engine
            .context
            .composition
            .segmentation
            .segments
            .last()
            .and_then(|segment| segment.menu.as_ref())
            .map(|menu| {
                menu.prepare(6);
                menu.candidate_count()
            })
            .unwrap_or(0);
        let visible_like = candidate_count > 0 || composing;
        if ms > 80.0 {
            slow += 1;
        }
        if !visible_like {
            invisible += 1;
        }
        if ms > 40.0 || !visible_like || index < 3 || index + 1 == keys.chars().count() {
            println!(
                "[{index:02}] '{ch}' eaten={eaten} {ms:6.1} ms composing={composing} 候选={candidate_count} 可视图={visible_like} input={:?}",
                engine.context.input
            );
        }
    }
    println!(
        "SUMMARY keys={} 慢帧(>80ms)={slow} 不可见帧={invisible}",
        keys.chars().count()
    );
}
