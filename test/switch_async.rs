//! 回归用例：**方案重建期间仍然有按键进来**。
//!
//! 2026-11-10 把方案切换从「全局锁里同步重建」改成「后台重建 + 原子换入」，
//! 这个用例锁住三条语义（改坏了必须红）：
//!
//! 1. 切换动作 **立刻**返回（不再把重建耗时算进这次调用）；
//! 2. 重建窗口里的按键**进旧会话**：能正常组字、能出候选，而且延迟正常
//!    —— 老实现这里会一直等到重建结束（线上 25~84 秒）；
//! 3. 换入是原子的：换入后新会话是**空**的（旧组合串不迁移，否则跨方案必然错位），
//!    新方案下的按键只反映新按键。
//!
//! 两个入口都要覆盖，因为它们共用同一条队列（`c_api::request_session_rebuild`）：
//! - [`keys_during_schema_rebuild_go_to_old_session_then_swap_is_atomic`]：宿主 API
//!   `rime_select_schema`；
//! - [`schema_menu_switch_is_asynchronous_and_reports_new_schema`]：**交互式**
//!   `Ctrl+` / F4 选单（`Engine::apply_schema` 登记 → `rime_process_key` 派发）。
//!
//! 为了让"重建窗口"稳定存在，用 `RIME_REBUILD_TEST_DELAY_MS` 给后台任务加延迟
//! （见 `engine::session_rebuild`）。数据用仓里的 `test/e2e` fixture，不碰真机数据。

use std::ffi::{c_int, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use librime_next::c_api::*;

/// `XK_F4`：fixture 里 `switcher/hotkeys` 的第一个键（另一个是 `Control+grave`）。
const XK_F4: c_int = 0xffc1;
/// `XK_Down`：选单里往下走。
const XK_DOWN: c_int = 0xff54;
/// `XK_Return`：确认选单。
const XK_RETURN: c_int = 0xff0d;

/// 集成测试默认并行；这些用例都读写进程级的 `Global`（数据目录、会话表），
/// 且各自 `initialize` / `finalize` 一次，必须串行。
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|err| err.into_inner())
}

struct Sandbox(PathBuf);

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap().flatten() {
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}

/// 起一套隔离的沙箱（fixture 的 build/opencc + 带 `schema_list` 的 default.yaml），
/// 让 `Global` 指向它，并建一个会话。
///
/// `default_schema_id` 只看 `<root>/schema/schema.yaml` 与 `<root>/default.yaml`
/// （不看 build/ 里的那份），所以要在用户目录根上放一份清单。
/// 用**块式**写法：`extract_first_schema_id` 不解析 `- { schema: x }` 行内映射。
fn boot(tag: &str) -> (usize, Sandbox) {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/e2e");
    let sandbox = std::env::temp_dir().join(format!(
        "librime-switch-async-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&sandbox);
    copy_dir(&fixture.join("build"), &sandbox.join("build"));
    copy_dir(&fixture.join("opencc"), &sandbox.join("opencc"));
    std::fs::write(
        sandbox.join("default.yaml"),
        "schema_list:\n  - schema: luna_pinyin_simp\n  - schema: wubi86\n  - schema: stroke\n",
    )
    .unwrap();

    let shared = CString::new(fixture.to_string_lossy().as_bytes()).unwrap();
    let user = CString::new(sandbox.to_string_lossy().as_bytes()).unwrap();
    let app = CString::new("rime.librime-next.switch-async").unwrap();
    let mut traits = RimeTraits {
        data_size: std::mem::size_of::<RimeTraits>() as c_int,
        shared_data_dir: shared.as_ptr(),
        user_data_dir: user.as_ptr(),
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
    unsafe {
        api().initialize.expect("initialize 未装")(&mut traits);
    }
    let id = unsafe { api().create_session.expect("create_session 未装")() };
    assert!(id != 0, "应创建出会话");
    (id, Sandbox(sandbox))
}

fn api() -> &'static RimeApi {
    unsafe { &*rime_get_api() }
}

fn current_schema(id: usize) -> String {
    let mut buf = vec![0i8; 256];
    let ok = unsafe {
        api()
            .get_current_schema
            .expect("get_current_schema 未装")(id, buf.as_mut_ptr(), buf.len())
    };
    assert!(ok != 0, "应能取到当前方案");
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// 宿主读 `RimeStatus.schema_id`（TSF 侧拿"当前方案"的另一个入口）。
fn status_schema(id: usize) -> String {
    let mut status: RimeStatus = unsafe { std::mem::zeroed() };
    let ok = unsafe { api().get_status.expect("get_status 未装")(id, &mut status) };
    assert!(ok != 0, "应能取到状态");
    let schema = if status.schema_id.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(status.schema_id) }
            .to_string_lossy()
            .into_owned()
    };
    unsafe { api().free_status.expect("free_status 未装")(&mut status) };
    schema
}

fn context_input(id: usize) -> (String, usize) {
    let mut ctx: RimeContext = unsafe { std::mem::zeroed() };
    let ok = unsafe { api().get_context.expect("get_context 未装")(id, &mut ctx) };
    assert!(ok != 0, "应能取到上下文");
    let preedit = if ctx.composition.preedit.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(ctx.composition.preedit) }
            .to_string_lossy()
            .into_owned()
    };
    let n = ctx.menu.num_candidates.max(0) as usize;
    unsafe { api().free_context.expect("free_context 未装")(&mut ctx) };
    (preedit, n)
}

/// 候选文字（选单里按名字找目标方案）。
fn menu_texts(id: usize) -> Vec<String> {
    let mut ctx: RimeContext = unsafe { std::mem::zeroed() };
    let ok = unsafe { api().get_context.expect("get_context 未装")(id, &mut ctx) };
    assert!(ok != 0, "应能取到上下文");
    let n = ctx.menu.num_candidates.max(0) as usize;
    let mut out = Vec::with_capacity(n);
    if !ctx.menu.candidates.is_null() {
        for i in 0..n {
            let item = unsafe { &*ctx.menu.candidates.add(i) };
            if !item.text.is_null() {
                out.push(
                    unsafe { CStr::from_ptr(item.text) }
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    unsafe { api().free_context.expect("free_context 未装")(&mut ctx) };
    out
}

/// 只留字母数字：组合串里的音节分隔符随方案/分段策略变化（`ni hao` / `ni·hao`），
/// 用例只关心"按键有没有进会话、有没有串味"。
fn norm(preedit: &str) -> String {
    preedit
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

fn press(id: usize, text: &str) -> Vec<Duration> {
    let mut out = Vec::new();
    for ch in text.chars() {
        let t = Instant::now();
        let consumed = unsafe {
            api()
                .process_key
                .expect("process_key 未装")(id, ch as c_int, 0)
        };
        out.push(t.elapsed());
        assert_eq!(consumed, 1, "按键 `{ch}` 应被消费");
    }
    out
}

/// 送一个功能键（不做"必须被消费"的断言），返回这一次调用的耗时。
fn press_key(id: usize, keycode: c_int) -> Duration {
    let t = Instant::now();
    unsafe {
        api().process_key.expect("process_key 未装")(id, keycode, 0);
    }
    t.elapsed()
}

fn wait_for_schema(id: usize, want: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if current_schema(id) == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "后台重建 20 秒内没换入 `{want}`，当前方案仍是 {}",
            current_schema(id)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn keys_during_schema_rebuild_go_to_old_session_then_swap_is_atomic() {
    let _serial = serial();
    // 700ms 的重建窗口：足够稳定地在里面按键，又不至于拖慢测试。
    // SAFETY: 此刻还没有起任何后台线程（重建线程在首次切方案时才 spawn），
    // 单线程环境下改环境变量是安全的（Rust 2024 把 set_var 标成了 unsafe）。
    unsafe {
        std::env::set_var("RIME_REBUILD_TEST_DELAY_MS", "700");
    }

    let (id, _sandbox) = boot("select");
    assert_eq!(
        current_schema(id),
        "luna_pinyin_simp",
        "起始方案取 default.yaml 第一条"
    );

    // 先在旧会话里组起来，制造一个"换入时正在组字"的场景。
    press(id, "ni");
    let (preedit, n) = context_input(id);
    assert_eq!(norm(&preedit), "ni", "旧会话应组出 ni");
    assert!(n > 0, "旧会话应出候选");

    // ① 切方案必须立刻返回（重建在后台）。
    let sid = CString::new("stroke").unwrap();
    let t_select = Instant::now();
    let accepted = unsafe { api().select_schema.expect("select_schema 未装")(id, sid.as_ptr()) };
    let select_elapsed = t_select.elapsed();
    assert_eq!(accepted, 1, "select_schema 应受理");
    assert!(
        select_elapsed < Duration::from_millis(200),
        "select_schema 不应在调用里同步重建，实测 {select_elapsed:?}"
    );

    // ② 重建窗口内：还挂在旧方案上，按键照常进旧会话且不吃延迟。
    assert_eq!(
        current_schema(id),
        "luna_pinyin_simp",
        "重建没完成前不应提前报告新方案"
    );
    let latencies = press(id, "hao");
    let worst = latencies.iter().max().copied().unwrap();
    assert!(
        worst < Duration::from_millis(200),
        "重建期间按键不应被重建堵住，最慢 {worst:?}"
    );
    let (preedit, n) = context_input(id);
    assert_eq!(norm(&preedit), "nihao", "重建期间按键应继续进旧会话");
    assert!(n > 0, "重建期间旧会话仍应出候选");

    // ③ 等后台换入完成。
    wait_for_schema(id, "stroke");

    // ④ 换入是原子的：旧组合串不迁移（跨方案迁移必然错位）。
    let (preedit, _) = context_input(id);
    assert!(
        preedit.is_empty(),
        "换入后应是空会话，旧组合串不迁移；实得 `{preedit}`"
    );

    // ⑤ 新方案下按键只反映新按键（没有半新半旧的组合串）。
    press(id, "h");
    let (preedit, n) = context_input(id);
    assert_eq!(norm(&preedit), "h", "新会话应只含新按键");
    assert!(n > 0, "仓颉/笔画方案下 h 应出候选");

    // ⑥ 连续两次切换：只认最后一次（旧重建结果按代号丢弃）。
    let wubi = CString::new("wubi86").unwrap();
    let stroke = CString::new("stroke").unwrap();
    unsafe {
        api().select_schema.expect("select_schema 未装")(id, stroke.as_ptr());
        api().select_schema.expect("select_schema 未装")(id, wubi.as_ptr());
    }
    wait_for_schema(id, "wubi86");
    // 再等一会儿：迟到的 stroke 结果不允许把会话"回滚"。
    std::thread::sleep(Duration::from_millis(900));
    assert_eq!(current_schema(id), "wubi86", "迟到的旧重建结果不能覆盖最新切换");

    unsafe {
        api().destroy_session.expect("destroy_session 未装")(id);
        api().finalize.expect("finalize 未装")();
    }
}

/// 两个入口（C API 选方案 / 交互式选单）**共用同一条重建队列与同一张代号表**：
/// 前者在跑时后者又发起一次，旧的迟到结果必须作废（后到者赢）。
///
/// 如果两条路各有一份队列/代号，stroke 的重建会在 wubi86 之后落地，把会话翻回去
/// —— 这正是"切完方案又自己变回去"这类现场反馈的成因。
#[test]
fn both_entries_share_one_rebuild_queue() {
    let _serial = serial();
    // SAFETY: 同前：串行执行、此刻没有后台线程。
    unsafe {
        std::env::set_var("RIME_REBUILD_TEST_DELAY_MS", "700");
    }

    let (id, _sandbox) = boot("shared");
    assert_eq!(current_schema(id), "luna_pinyin_simp", "起始方案");

    // ① C API 先发起 stroke（进后台队列）。
    let stroke = CString::new("stroke").unwrap();
    let accepted = unsafe { api().select_schema.expect("select_schema 未装")(id, stroke.as_ptr()) };
    assert_eq!(accepted, 1);

    // ② 重建还没换入，用选单再发起一次（交互式 → 五笔）。
    press_key(id, XK_F4);
    let texts = menu_texts(id);
    let index = texts
        .iter()
        .position(|t| t.starts_with("五笔86"))
        .unwrap_or_else(|| panic!("菜单里应有五笔，实际 {texts:?}"));
    for _ in 0..index {
        press_key(id, XK_DOWN);
    }
    press_key(id, XK_RETURN);
    assert_eq!(
        current_schema(id),
        "luna_pinyin_simp",
        "两个重建都还没完成，不应提前报方案"
    );

    // ③ 最终只认最后一次（五笔），stroke 的迟到结果被代号判作废。
    wait_for_schema(id, "wubi86");
    std::thread::sleep(Duration::from_millis(900));
    assert_eq!(
        current_schema(id),
        "wubi86",
        "另一个入口的迟到结果不能覆盖最新切换（共用一张代号表）"
    );

    unsafe {
        api().destroy_session.expect("destroy_session 未装")(id);
        api().finalize.expect("finalize 未装")();
    }
}

/// 换入后**不需要用户再按任何键**：宿主读到的就是"新方案 + 空 preedit"，
/// 同时收到一条 `("schema", "schema_id/<sid>")` 通知（宿主据此在按键路径之外
/// 推一帧给 TIP，清掉界面上挂着的旧组合串）。
///
/// 这是「换入到下一次按键之间 preedit 还挂着旧文字」的回归点：以前只有下一条按键
/// 的响应才会冲掉旧组合串；换入改到后台线程后，必须主动通知，否则窗口一直挂着。
static NOTIFIED: AtomicUsize = AtomicUsize::new(0);
static NOTIFIED_SCHEMA: Mutex<String> = Mutex::new(String::new());

unsafe extern "C" fn on_schema_notification(
    _ctx: *mut std::ffi::c_void,
    _session_id: usize,
    message_type: *const std::ffi::c_char,
    message_value: *const std::ffi::c_char,
) {
    if message_type.is_null() || message_value.is_null() {
        return;
    }
    let ty = unsafe { CStr::from_ptr(message_type) }.to_string_lossy().into_owned();
    if ty != "schema" {
        return;
    }
    let value = unsafe { CStr::from_ptr(message_value) }
        .to_string_lossy()
        .into_owned();
    NOTIFIED.fetch_add(1, Ordering::SeqCst);
    *NOTIFIED_SCHEMA.lock().unwrap_or_else(|err| err.into_inner()) = value;
}

#[test]
fn schema_switch_notifies_host_without_any_key() {
    let _serial = serial();
    // SAFETY: 串行执行；此刻没有后台线程。
    unsafe {
        std::env::set_var("RIME_REBUILD_TEST_DELAY_MS", "300");
    }

    let (id, _sandbox) = boot("notify");
    unsafe {
        api()
            .set_notification_handler
            .expect("set_notification_handler 未装")(
            Some(on_schema_notification),
            std::ptr::null_mut(),
        );
    }

    // 先在旧方案里组字：换入时这个组合串必须消失。
    press(id, "ni");
    assert_eq!(norm(&context_input(id).0), "ni", "旧会话应组出 ni");

    let stroke = CString::new("stroke").unwrap();
    let accepted = unsafe { api().select_schema.expect("select_schema 未装")(id, stroke.as_ptr()) };
    assert_eq!(accepted, 1);
    wait_for_schema(id, "stroke");

    // ★ 关键：这里**一个键都不按**，宿主看到的状态就应该是新的。
    let (preedit, _) = context_input(id);
    assert!(
        preedit.is_empty(),
        "换入后无需按键 preedit 就该为空（这正是用户能看到的窗口）；实得 `{preedit}`"
    );
    assert_eq!(current_schema(id), "stroke", "换入后当前方案应立即更新");
    assert_eq!(status_schema(id), "stroke", "宿主读 status 也应是新方案");

    // 通知是换入之后在后台线程发出的，给几毫秒落定。
    let deadline = Instant::now() + Duration::from_secs(2);
    while NOTIFIED.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        NOTIFIED.load(Ordering::SeqCst) >= 1,
        "换入后应主动通知宿主（Engine 据此在按键之外推一帧给 TIP）"
    );
    let value = NOTIFIED_SCHEMA
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone();
    assert_eq!(value, "schema_id/stroke", "通知应带新方案 id");

    unsafe {
        api().set_notification_handler.expect("set_notification_handler 未装")(
            None,
            std::ptr::null_mut(),
        );
        api().destroy_session.expect("destroy_session 未装")(id);
        api().finalize.expect("finalize 未装")();
    }
}

/// **交互式**切换（Ctrl+` / F4 选单）走的是另一条路：
/// `Switcher::process_key_event` → `Engine::request_schema`（只登记）→
/// `Engine::process_key` 跑完处理器链 → `Engine::apply_schema`（异步模式下只登记
/// `pending_rebuild`）→ `rime_process_key` 放锁后派发到同一个重建队列。
///
/// 这个用例钉住：确认选单的那一键不等重建；重建期间按键正常；换入后宿主从
/// `get_current_schema` / `get_status` 看到新方案、`get_context` 的 preedit 为空。
#[test]
fn schema_menu_switch_is_asynchronous_and_reports_new_schema() {
    let _serial = serial();
    // SAFETY: 同上一个用例：此时没有后台线程，串行执行下改环境变量安全。
    unsafe {
        std::env::set_var("RIME_REBUILD_TEST_DELAY_MS", "700");
    }

    let (id, _sandbox) = boot("menu");
    assert_eq!(current_schema(id), "luna_pinyin_simp", "起始方案");

    // ① 打开方案選單（fixture 的 switcher/hotkeys 含 F4）。
    let open = press_key(id, XK_F4);
    assert!(
        open < Duration::from_millis(200),
        "打开选单不该卡：{open:?}"
    );
    let texts = menu_texts(id);
    let index = texts
        .iter()
        .position(|t| t.starts_with("五笔86"))
        .unwrap_or_else(|| panic!("菜单里应有五笔，实际 {texts:?}"));

    // ② 高亮到五笔，按回车确认。**这一键就是切换动作**，它必须立刻返回。
    for _ in 0..index {
        press_key(id, XK_DOWN);
    }
    let confirm = press_key(id, XK_RETURN);
    assert!(
        confirm < Duration::from_millis(200),
        "确认选单的那一键不应同步重建（交互式卡顿就卡在这里），实测 {confirm:?}"
    );
    assert_eq!(
        current_schema(id),
        "luna_pinyin_simp",
        "重建没完成前不应提前报告新方案"
    );

    // ③ 重建窗口内：按键照常进旧会话、延迟正常、组合串不串味。
    let latencies = press(id, "hao");
    let worst = latencies.iter().max().copied().unwrap();
    assert!(
        worst < Duration::from_millis(200),
        "重建期间按键不应被重建堵住，最慢 {worst:?}"
    );
    let (preedit, n) = context_input(id);
    assert_eq!(norm(&preedit), "hao", "重建期间按键应继续进旧会话");
    assert!(n > 0, "重建期间旧会话仍应出候选");

    // ④ 换入后：宿主读得到新方案（两个入口都要对），preedit 为空。
    wait_for_schema(id, "wubi86");
    assert_eq!(status_schema(id), "wubi86", "宿主读 status 也应看到新方案");
    let (preedit, _) = context_input(id);
    assert!(
        preedit.is_empty(),
        "换入后 preedit 必须为空（旧组合串不跨方案迁移），实得 `{preedit}`"
    );

    // ⑤ 新方案下按键只反映新按键。
    press(id, "w");
    let (preedit, n) = context_input(id);
    assert_eq!(norm(&preedit), "w", "新会话应只含新按键");
    assert!(n > 0, "五笔下 w 应出候选");

    unsafe {
        api().destroy_session.expect("destroy_session 未装")(id);
        api().finalize.expect("finalize 未装")();
    }
}
