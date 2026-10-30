//! 每键耗时基准:模拟连续打字,测每键平均延迟(微秒)。
use librime_next::engine::Session;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let user = std::env::var_os("APPDATA").map(|a| std::path::PathBuf::from(a).join("Rime-Next")).unwrap();
    let build = user.join("build");
    let mut session = Session::create(&build, "luna_pinyin_simp");
    println!("引擎加载: {}", session.engine.is_some());

    let inputs = ["nihao", "tianqibucuo", "zhongguorenmin", "jisuanjikepu", "shehuizhuyi"];
    for input in &inputs {
        // 预热
        for ch in input.chars() { session.process_key(ch as u32, 0); }
        session.process_key(0xff1b, 0); // Esc

        // 正式计时:逐键
        let mut total = std::time::Duration::ZERO;
        let mut max = std::time::Duration::ZERO;
        for ch in input.chars() {
            let t0 = Instant::now();
            session.process_key(ch as u32, 0);
            let dt = t0.elapsed();
            total += dt;
            if dt > max { max = dt; }
        }
        let n = input.chars().count();
        println!("  {input}: {n} 键,平均 {:.0}µs,最慢 {:.0}µs",
                 total.as_micros() as f64 / n as f64, max.as_micros());

        // Esc 清场
        session.process_key(0xff1b, 0);
    }

    // 长句压测
    let long_input = "zhonghuarenmingongheguowannsui";
    let mut total = std::time::Duration::ZERO;
    let mut max = std::time::Duration::ZERO;
    for ch in long_input.chars() {
        let t0 = Instant::now();
        session.process_key(ch as u32, 0);
        let dt = t0.elapsed();
        total += dt;
        if dt > max { max = dt; }
    }
    let n = long_input.chars().count();
    println!("  长句 {long_input}: {n} 键,平均 {:.0}µs,最慢 {:.0}µs",
             total.as_micros() as f64 / n as f64, max.as_micros());

    // 内存估算
    println!("Engine 会话已建立(内存看任务管理器)");
}
