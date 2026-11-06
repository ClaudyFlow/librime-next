//! 神经重排 / 预测 A/B 探针：同一组输入，打印「静态顺序」与「神经落地后顺序」，
//! 并给出按键耗时与等待落地耗时。用来判断模型到底改了什么、值不值。
//!
//! 用法：
//!   cargo run --release --example probe_rescore_ab -- <build_dir> <user_root> <shared_root> <schema_id> [输入...]
//!
//! 环境变量（与引擎同一套）：
//!   WEASEL_RESCORE=off   关选字/整句重排
//!   WEASEL_PREDICT=off   关补位预测
//!   WEASEL_DEBUG_RESCORE=1  把后台批次打到 stderr
//!
//! 注意：重排是**异步**的。第一轮 compose 只提交批次、按静态顺序发候选；
//! 结果落地后要再 compose 一次才换序。探针用 `poll_rescore_landing()` 触发这一轮，
//! 与宿主读上下文前的动作一致（见 `Session::poll_rescore_landing`）。

use librime_next::engine::gear::rescore;
use librime_next::engine::Session;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// 取最后一段的前 `n` 条候选（文本 + 注释）。
fn top(session: &mut Session, n: usize) -> Vec<String> {
    let Some(ctx) = session.context() else {
        return Vec::new();
    };
    let Some(seg) = ctx.composition.segmentation.segments.last() else {
        return Vec::new();
    };
    let Some(menu) = seg.menu.as_ref() else {
        return Vec::new();
    };
    menu.prepare(n);
    (0..menu.candidate_count().min(n))
        .filter_map(|i| menu.candidate_at(i))
        .map(|c| {
            if c.comment.is_empty() {
                c.text
            } else {
                format!("{}[{}]", c.text, c.comment)
            }
        })
        .collect()
}

/// 把 after 相对 before 的位移标出来：`+` 是往前挪，`-` 是往后挪，`=` 不动。
fn diff_marks(before: &[String], after: &[String]) -> String {
    after
        .iter()
        .map(|t| match before.iter().position(|b| b == t) {
            Some(0) if after.first() == Some(t) => "=".to_string(),
            Some(_) => "+".to_string(),
            None => "*".to_string(), // 新出现的候选（预测补位）
        })
        .collect::<Vec<_>>()
        .join("")
}

fn main() {
    let mut args = std::env::args().skip(1);
    let build = PathBuf::from(args.next().expect("build_dir"));
    let user = PathBuf::from(args.next().expect("user_root"));
    let shared = PathBuf::from(args.next().expect("shared_root"));
    let schema = args.next().expect("schema_id");
    let mut inputs: Vec<String> = args.collect();
    if inputs.is_empty() {
        inputs = ["co", "eqc", "vb", "qz", "xk", "coc"]
            .into_iter()
            .map(str::to_string)
            .collect();
    }

    let roots = vec![user, shared.clone()];
    rescore::init_from_data_roots(&[shared.clone()]);
    let worker = rescore::global();

    println!("# schema={schema}  输入={} 个", inputs.len());
    println!(
        "# 模型：{}",
        match &worker {
            Some(_) => "已加载（model/sentence-model.safetensors）",
            None => "未找到（只跑静态顺序）",
        }
    );
    println!(
        "# 开关：RESCORE={}  PREDICT={}",
        std::env::var("WEASEL_RESCORE").unwrap_or_else(|_| "on".into()),
        std::env::var("WEASEL_PREDICT").unwrap_or_else(|_| "on".into()),
    );

    let mut moved_inputs = 0usize;
    let mut total_type = Duration::ZERO;
    let mut total_wait = Duration::ZERO;

    // **一个会话跑完全部输入**：输入法是常驻进程，词典/模型只加载一次。
    // 每个输入测完就清空组字区，这样按键耗时里不掺引擎创建的成本。
    let mut s = Session::create_with_roots(&build, &schema, &roots);
    if s.engine.is_none() {
        println!("\nENGINE: None（方案加载失败）");
        return;
    }
    // 预热：把首条方案的词表和模型都走热（首键冷启动 ~10ms，不预热统计会失真）。
    for ch in inputs[0].chars() {
        s.process_key(ch as u32, 0);
    }
    s.clear_composition();
    if let Some(w) = &worker {
        let deadline = Instant::now() + Duration::from_secs(5);
        while w.current_generation() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    for input in &inputs {
        let gen0 = worker.as_ref().map(|w| w.current_generation()).unwrap_or(0);

        let t0 = Instant::now();
        for ch in input.chars() {
            s.process_key(ch as u32, 0);
        }
        let first_us = t0.elapsed();

        let before = top(&mut s, 8);

        // 等后台批次落地（键盘模型一批 ~100ms，给足 5s）。
        let t1 = Instant::now();
        let mut landed = false;
        if let Some(w) = &worker {
            let deadline = Instant::now() + Duration::from_secs(5);
            while w.current_generation() == gen0 {
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            landed = w.current_generation() > gen0;
        }
        if landed {
            s.poll_rescore_landing();
        }
        let wait_us = t1.elapsed();
        total_wait += wait_us;

        let after = top(&mut s, 8);
        let marks = diff_marks(&before, &after);
        if after != before {
            moved_inputs += 1;
        }

        // 稳态按键耗时：清空重打 4 轮取最快（首轮含重排提交/预测排队，不是稳态）。
        let mut best = first_us;
        for _ in 0..4 {
            s.clear_composition();
            let t = Instant::now();
            for ch in input.chars() {
                s.process_key(ch as u32, 0);
            }
            best = best.min(t.elapsed());
        }
        total_type += best;

        println!(
            "\n=== {input} ===  按键(稳态最快) {:>6.0}µs / {} 键  落地 {:>7.1}ms  {}",
            best.as_secs_f64() * 1e6,
            input.chars().count(),
            wait_us.as_secs_f64() * 1e3,
            if landed { "结果已落地" } else { "无新结果" }
        );
        println!("  静态  {}", before.join("  "));
        println!("  神经  {}", after.join("  "));
        println!("  位移  {marks}   (首位是否变化：{})",
            if before.first() != after.first() { "是" } else { "否" });

        // 上屏验证：清空后重打再按空格（同一个会话，不重开会话/不重载词典）。
        s.clear_composition();
        for ch in input.chars() {
            s.process_key(ch as u32, 0);
        }
        s.process_key(0x20, 0); // XK_SPACE
        let commit = s.take_commit().unwrap_or_default();
        println!("  空格→ [{commit}]");
        s.clear_composition();
    }

    println!(
        "\n# 汇总：{}/{} 个输入的顺序被改；按键合计(稳态最快) {:.1}ms（均 {:.0}µs/键）；等待落地合计 {:.0}ms",
        moved_inputs,
        inputs.len(),
        total_type.as_secs_f64() * 1e3,
        total_type.as_secs_f64() * 1e6
            / (inputs.iter().map(|s| s.chars().count()).sum::<usize>().max(1) as f64),
        total_wait.as_secs_f64() * 1e3
    );
}
