//! 诊断探针：把「切方案的重建」按阶段拆开计时。
//!
//! 2026-11-10 背景：`rime_select_schema` 在全局锁里 `Session::create_with_roots()`
//! 整个重建，实测静默 25~84 秒；但旧日志分不清慢在建词典、prism、重绑码表、aux
//! 还是 lua 脚本。这个探针在**进程内**按同样的入口重建，配合 `WEASEL_TRACE_SCHEMA=1`
//! 输出逐阶段耗时（stderr + `schema-switch.log`）。
//!
//! 用法（离线，只读用户目录副本）：
//! ```text
//! WEASEL_TRACE_SCHEMA=1 cargo run --release --example probe_rebuild_stages -- \
//!     <用户目录副本> <共享目录> tiger_sentence double_pinyin.ziranma ...
//! ```
//!
//! 说明：同一个进程/线程里连续重建时，lua VM 与码表缓存是**复用**的（对应线上
//! "切过一次之后再切"的热路径）；第一份才是冷路径。

use std::ffi::{c_int, CStr, CString};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use librime_next::engine::session::Session;

/// 走宿主入口（`rime_initialize` → `rime_create_session` → `rime_select_schema`），
/// 把「引擎启动静默 N 秒」与「切方案是否还堵按键」放在同一条时间线上量。
fn probe_c_api(user_dir: &Path, shared: &Path) {
    use librime_next::c_api::*;

    let api = unsafe { &*rime_get_api() };
    let shared_c = CString::new(shared.to_string_lossy().as_ref()).unwrap();
    let user_c = CString::new(user_dir.to_string_lossy().as_ref()).unwrap();
    let app = CString::new("rime.probe.rebuild").unwrap();
    let mut traits = RimeTraits {
        data_size: std::mem::size_of::<RimeTraits>() as c_int,
        shared_data_dir: shared_c.as_ptr(),
        user_data_dir: user_c.as_ptr(),
        distribution_name: std::ptr::null(),
        distribution_code_name: std::ptr::null(),
        distribution_version: std::ptr::null(),
        app_name: app.as_ptr(),
        modules: std::ptr::null(),
        min_log_level: 0,
        log_dir: std::ptr::null(),
        prebuilt_data_dir: std::ptr::null(),
        staging_dir: std::ptr::null(),
    };
    unsafe { (api.initialize.unwrap())(&mut traits) };

    let t = Instant::now();
    let id = unsafe { (api.create_session.unwrap())() };
    println!("create_session       = {:8.1} ms (id={id})", ms(t));

    let target = std::env::var("PROBE_TARGET").unwrap_or_else(|_| "tiger_sentence".into());
    let target_c = CString::new(target.as_str()).unwrap();
    let mut buf = vec![0i8; 256];
    unsafe {
        (api.get_current_schema.unwrap())(id, buf.as_mut_ptr(), buf.len());
    }
    println!("切换前方案            = {}", cstr(&buf));

    let t = Instant::now();
    let accepted = unsafe { (api.select_schema.unwrap())(id, target_c.as_ptr()) };
    println!("select_schema 返回     = {:8.1} ms (ok={accepted})", ms(t));

    // 立刻发一次按键（重建窗口内）：应当很快被旧会话吃掉。
    let t = Instant::now();
    let consumed = unsafe { (api.process_key.unwrap())(id, 'n' as c_int, 0) };
    println!("重建窗口内一次按键     = {:8.1} ms (consumed={consumed})", ms(t));

    let t = Instant::now();
    let mut swapped = false;
    while t.elapsed() < Duration::from_secs(300) {
        buf.iter_mut().for_each(|b| *b = 0);
        unsafe {
            (api.get_current_schema.unwrap())(id, buf.as_mut_ptr(), buf.len());
        }
        if cstr(&buf) == target {
            swapped = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    println!(
        "新方案换入耗时         = {:8.1} ms (swapped={swapped})",
        ms(t)
    );

    unsafe {
        (api.destroy_session.unwrap())(id);
        (api.finalize.unwrap())();
    }
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn cstr(buf: &[i8]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let user_dir = PathBuf::from(
        raw.first()
            .unwrap_or_else(|| panic!("用法: probe_rebuild_stages <用户目录副本> <共享目录> [方案...]")),
    );
    let shared = PathBuf::from(raw.get(1).unwrap_or_else(|| panic!("缺共享目录")));
    let schemas: Vec<String> = if raw.len() > 2 {
        raw[2..].to_vec()
    } else {
        vec!["tiger_sentence".into(), "double_pinyin.ziranma".into()]
    };
    let roots = vec![user_dir.clone(), shared.clone()];
    let build = user_dir.join("build");

    println!("== probe_rebuild_stages ==");
    println!("user   = {}", user_dir.display());
    println!("build  = {}", build.display());
    println!("trace  = {}", librime_next::engine::switch_trace::enabled());
    println!();

    // `PROBE_C_API=1`：走宿主真正用的入口（rescore → eassy → create_with_roots），
    // 用来看"引擎启动静默 N 秒"里 rescore（神经模型加载）占多少。
    if std::env::var_os("PROBE_C_API").is_some() {
        probe_c_api(&user_dir, &shared);
        return;
    }

    for schema in &schemas {
        let t = Instant::now();
        let session = Session::create_with_roots(&build, schema, &roots);
        let total = t.elapsed().as_secs_f64() * 1000.0;
        let ok = session.engine.is_some();
        println!("{schema:24} 重建总计={total:10.1} ms  可用={ok}");
        // 立刻丢弃，下一轮才是"从零重建"（lua 缓存仍在线程内保留，与线上一致）
        drop(session);
    }

    // 再跑一遍：看同一进程内第二次重建（热缓存）还剩多少。
    println!();
    println!("-- 第二轮（同线程热缓存） --");
    for schema in &schemas {
        let t = Instant::now();
        let session = Session::create_with_roots(&build, schema, &roots);
        println!(
            "{schema:24} 重建总计={:10.1} ms  可用={}",
            t.elapsed().as_secs_f64() * 1000.0,
            session.engine.is_some()
        );
        drop(session);
    }
}
