//! 两版引擎的**按键响应**对比：C++ 补丁版（`rime.dll`）vs Rust 版（`librime_next.dll`）。
//!
//! 用法（在 `librime-next/` 目录）：
//!
//! ```text
//! cargo run --release --example bench_two_engines
//! ```
//!
//! # 为什么必须走 DLL
//!
//! Rust 侧可以直接 `Session::create()` 调内部 API，但那不是用户在用的路径 ——
//! 宿主（Weasel）走的是 `rime_get_api()` 的 C ABI，中间隔着一层虚表调用、
//! 一次字符串转换、一次 `RimeContext` 编组。只比内部函数等于把这一层抹掉，
//! 得出的数字不能代表"打字手感"。
//!
//! 所以两个引擎都用**同一段宿主代码**加载：`LoadLibraryW` → `rime_get_api` →
//! `setup/initialize/create_session/select_schema` → 逐键 `process_key`。
//! 除了 DLL 文件不同，其余完全一致。
//!
//! # 测量口径
//!
//! * **只计 `process_key`**，不含 `get_context`（候选窗绘制是宿主的活，两边一样）；
//! * 每个输入串先**预热**一遍再跑 5 轮，取**中位数**（首轮要建索引/热页，混进去会偏大）；
//! * 每轮之间按 `Esc` 清场，避免上一轮的组合状态影响下一轮；
//! * 单位微秒（µs），一屏候选的等待阈值按人类感知算大约 30 ms = 30000 µs。

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::time::Instant;

use librime_next::c_api::{RimeApi, RimeTraits};

use windows::core::PCWSTR;
use windows::Win32::Foundation::FreeLibrary;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW, SetDllDirectoryW};

/// 出厂方案：拼音简体（两版都装得有，且词库同为 `luna_pinyin`）。
const SCHEMA: &str = "luna_pinyin_simp";

/// 测的打字序列：从两个字母到 20 个音节，覆盖"首屏 / 翻页 / 长句组句"三种开销。
const INPUTS: [&str; 6] = [
    "ni",
    "nihao",
    "tianqibucuo",
    "zhongguorenmin",
    "jishengbuyi",
    "zhonghuarenmingongheguo",
];

/// 一个已加载的引擎句柄。
struct Engine {
    label: &'static str,
    library: windows::Win32::Foundation::HMODULE,
    api: &'static RimeApi,
}

impl Engine {
    /// 加载 DLL 并取 `rime_get_api`。
    fn load(label: &'static str, path: &Path) -> Option<Self> {
        if !path.exists() {
            eprintln!("[{}] 找不到 {}", label, path.display());
            return None;
        }
        let wide: Vec<u16> = path
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let library = match unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) } {
            Ok(handle) => handle,
            Err(error) => {
                // 126 = ERROR_MOD_NOT_FOUND：C++ 版依赖 libc++ / libglog / libopencc，
                // 这些不在安装目录里，需要把 `runtime/` 加进搜索路径（见 `main`）。
                eprintln!("[{}] LoadLibrary 失败：{error}（{}）", label, path.display());
                return None;
            }
        };
        let proc = unsafe { GetProcAddress(library, windows::core::s!("rime_get_api")) }?;
        // SAFETY: `rime_get_api` 的签名是 `RimeApi* (*)()`，两版一致（ABI 校验见
        // `tools/abi_check`）；拿到的表在整个进程生命周期内有效。
        let get_api: unsafe extern "C" fn() -> *const RimeApi =
            unsafe { std::mem::transmute(proc) };
        let api = unsafe { get_api().as_ref() }?;
        Some(Self {
            label,
            library,
            api,
        })
    }

    /// `setup` + `initialize`：把三个根路径告诉引擎。
    fn initialize(&self, shared: &Path, user: &Path, log_dir: &Path) {
        let to_c = |p: &Path| CString::new(p.to_string_lossy().as_ref()).unwrap();
        let shared = to_c(shared);
        let user = to_c(user);
        let log = to_c(log_dir);
        let dist = CString::new("Weasel-Next").unwrap();
        let code = CString::new("weasel-next-bench").unwrap();
        let version = CString::new("0.0.0").unwrap();
        let app = CString::new("rime.bench").unwrap();
        let mut traits = RimeTraits {
            data_size: std::mem::size_of::<RimeTraits>() as i32,
            shared_data_dir: shared.as_ptr(),
            user_data_dir: user.as_ptr(),
            distribution_name: dist.as_ptr(),
            distribution_code_name: code.as_ptr(),
            distribution_version: version.as_ptr(),
            app_name: app.as_ptr(),
            modules: std::ptr::null(),
            min_log_level: 3,
            log_dir: log.as_ptr(),
            prebuilt_data_dir: std::ptr::null(),
            staging_dir: std::ptr::null(),
        };
        unsafe {
            if let Some(setup) = self.api.setup {
                setup(&mut traits);
            }
            if let Some(initialize) = self.api.initialize {
                initialize(&mut traits);
            }
        }
    }

    /// 建会话并切到目标方案；失败返回 `None`。
    fn session(&self) -> Option<usize> {
        let id = unsafe { (self.api.create_session?)() };
        if id == 0 {
            return None;
        }
        let schema = CString::new(SCHEMA).unwrap();
        let ok = unsafe { (self.api.select_schema?)(id, schema.as_ptr()) };
        if ok == 0 {
            return None;
        }
        Some(id)
    }

    /// 一拍按键（keysym, modifier）。
    fn key(&self, session: usize, keycode: u32, modifier: u32) {
        unsafe {
            if let Some(process_key) = self.api.process_key {
                process_key(session, keycode as i32, modifier as i32);
            }
        }
    }

    /// 宿主一帧：`process_key` **加** 一次 `get_context`。
    ///
    /// 为什么必须带上 `get_context`：C++ librime 的翻译是**惰性**的 ——
    /// `process_key` 里只增量切分新音节，真正的 `Translate()` 由 `get_context`
    /// 补菜单时触发。只计 `process_key` 会把 C++ 的翻译整段免掉（实测长串反而
    /// 比短串快，就是这个原因），而 Rust 版是在 `process_key` 内部就翻完的，
    /// 于是同一把尺子量出两类不同的东西。宿主（Weasel）每按一键必然紧接一次
    /// 取候选，所以"一帧"才是打字手感的真实边界。
    fn frame(&self, session: usize, keycode: u32, modifier: u32) {
        self.key(session, keycode, modifier);
        // 丢弃返回值也必须 free，否则 Rust 侧每帧泄漏一份 menu 字符串。
        let _ = self.candidate_count(session);
    }

    /// 当前一屏候选数（诊断用：确认两版真的出了候选，而不是空跑）。
    fn candidate_count(&self, session: usize) -> usize {
        use librime_next::c_api::RimeContext;
        let mut context: RimeContext = unsafe { std::mem::zeroed() };
        context.data_size = std::mem::size_of::<RimeContext>() as i32;
        let Some(get_context) = self.api.get_context else {
            return 0;
        };
        let ok = unsafe { get_context(session, &mut context) };
        if ok == 0 {
            return 0;
        }
        let count = context.menu.num_candidates.max(0) as usize;
        unsafe {
            if let Some(free) = self.api.free_context {
                free(&mut context);
            }
        }
        count
    }

    fn destroy(&self, session: usize) {
        unsafe {
            if let Some(destroy) = self.api.destroy_session {
                destroy(session);
            }
        }
    }

    fn finalize(&self) {
        unsafe {
            if let Some(finalize) = self.api.finalize {
                finalize();
            }
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe {
            let _ = FreeLibrary(self.library);
        }
    }
}

/// 一行测量结果。
struct Row {
    input: &'static str,
    /// 逐键"一帧"耗时的中位数（µs）—— 代表稳定态手感。
    median: f64,
    /// 整串打下去的总耗时（µs）—— 代表长句累计延迟。
    total: f64,
    /// 打完整串时的候选数（确认两版真的在算，而不是空跑）。
    candidates: usize,
}

/// 跑一遍全部输入。
fn measure(engine: &Engine, session: usize) -> Vec<Row> {
    const ROUNDS: usize = 5;
    const ESC: u32 = 0xff1b;
    const BACKSPACE: u32 = 0xff08;

    let mut out = Vec::new();
    for input in INPUTS {
        // 预热：把这条输入完整敲一遍（建音节图、热页、填缓存），再清场
        for ch in input.chars() {
            engine.frame(session, ch as u32, 0);
        }
        engine.key(session, ESC, 0);

        let mut per_key: Vec<f64> = Vec::with_capacity(ROUNDS * input.len());
        let mut totals: Vec<f64> = Vec::with_capacity(ROUNDS);
        let mut candidates = 0usize;
        for _ in 0..ROUNDS {
            // 清场：Esc 之后可能还有残留输入，退格几下更保险
            engine.key(session, ESC, 0);
            let start_round = Instant::now();
            for ch in input.chars() {
                let start = Instant::now();
                engine.frame(session, ch as u32, 0);
                per_key.push(start.elapsed().as_nanos() as f64 / 1000.0);
            }
            totals.push(start_round.elapsed().as_nanos() as f64 / 1000.0);
            candidates = candidates.max(engine.candidate_count(session));
            // 用退格回到空，避免 Esc 之外的路径残留
            for _ in 0..input.len() {
                engine.key(session, BACKSPACE, 0);
            }
            engine.key(session, ESC, 0);
        }
        per_key.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        totals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = per_key.get(per_key.len() / 2).copied().unwrap_or(0.0);
        let total = totals.get(totals.len() / 2).copied().unwrap_or(0.0);
        out.push(Row {
            input,
            median,
            total,
            candidates,
        });
    }
    out
}

fn main() {
    let install = PathBuf::from(r"C:\Program Files\Weasel-Next");
    let shared = PathBuf::from(r"C:\code\Weasel-Next\Rime-Next");
    let user = std::env::var_os("APPDATA")
        .map(|a| PathBuf::from(a).join("Rime-Next"))
        .expect("拿不到 APPDATA");
    // C++ 版 rime.dll 动态链接 libc++ / libglog / libopencc / libunwind，
    // 它们躺在仓库的 `runtime/` 里而不是安装目录。把该目录加进 DLL 搜索路径，
    // 否则 LoadLibrary 直接 126。
    let runtime = PathBuf::from(r"C://code//Weasel-Next//runtime");
    if runtime.exists() {
        let wide: Vec<u16> = runtime
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let _ = SetDllDirectoryW(PCWSTR(wide.as_ptr()));
        }
    }
    let log_dir = std::env::temp_dir().join("lrn-bench-log");
    let _ = std::fs::create_dir_all(&log_dir);

    println!("共享数据目录: {}", shared.display());
    println!("用户数据目录: {}", user.display());
    println!();

    // 两版**串行**跑：LevelDB 的 LOCK 按目录独占，同时开两个引擎会互相抢锁。
    //
    // 路径可用环境变量覆盖，默认指向安装目录。为什么要覆盖：`cargo build` 的产物
    // 比部署副本新的时候（常态），直接量安装目录等于量上一版代码。
    //   LRN_BENCH_CPP_DLL   / LRN_BENCH_RUST_DLL
    let cpp_dll = std::env::var_os("LRN_BENCH_CPP_DLL")
        .map(PathBuf::from)
        .unwrap_or_else(|| install.join("rime.dll"));
    let rust_dll = std::env::var_os("LRN_BENCH_RUST_DLL")
        .map(PathBuf::from)
        .unwrap_or_else(|| install.join("librime_next.dll"));
    let engines = [("C++ 补丁版", cpp_dll), ("Rust 版", rust_dll)];
    for (label, path) in &engines {
        println!("[{label}] {}", path.display());
    }
    println!();
    let mut results: Vec<(&'static str, Vec<Row>)> = Vec::new();

    for (label, path) in engines {
        let Some(engine) = Engine::load(label, &path) else {
            eprintln!("[{label}] 加载失败，跳过");
            continue;
        };
        engine.initialize(&shared, &user, &log_dir);
        let Some(session) = engine.session() else {
            eprintln!("[{label}] 建会话/切方案失败，跳过");
            engine.finalize();
            continue;
        };
        let rows = measure(&engine, session);
        engine.destroy(session);
        engine.finalize();
        results.push((label, rows));
        println!("[{label}] 完成");
    }

    // 表一：逐键中位数（手感）
    println!();
    println!(
        "| 输入 | 键数 | {}",
        results
            .iter()
            .map(|(label, _)| format!("{label}（µs/键）"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    println!(
        "|---|---|{}",
        results.iter().map(|_| "---").collect::<Vec<_>>().join("|")
    );
    for (index, input) in INPUTS.iter().enumerate() {
        let cells: Vec<String> = results
            .iter()
            .map(|(_, rows)| match rows.get(index) {
                Some(row) => format!("{:.1}", row.median),
                None => "-".to_string(),
            })
            .collect();
        println!("| `{input}` | {} | {} |", input.len(), cells.join(" | "));
    }
    println!();

    // 表二：整串总耗时（长句累计延迟）
    println!(
        "| 输入 | 键数 | {}",
        results
            .iter()
            .map(|(label, _)| format!("{label} 整串（µs）"))
            .collect::<Vec<_>>()
            .join(" | ")
    );
    println!(
        "|---|---|{}",
        results.iter().map(|_| "---").collect::<Vec<_>>().join("|")
    );
    for (index, input) in INPUTS.iter().enumerate() {
        let cells: Vec<String> = results
            .iter()
            .map(|(_, rows)| match rows.get(index) {
                Some(row) => format!("{:.0}", row.total),
                None => "-".to_string(),
            })
            .collect();
        println!("| `{input}` | {} | {} |", input.len(), cells.join(" | "));
    }
    println!();

    for (label, rows) in &results {
        println!("{label}：");
        for row in rows {
            println!(
                "  {:<28} {:>8.1} µs/键   整串 {:>8.0} µs   候选 {}",
                row.input, row.median, row.total, row.candidates
            );
        }
        let keys: usize = rows.iter().map(|row| row.input.len()).sum();
        let median_sum: f64 = rows.iter().map(|row| row.median).sum();
        let total_all: f64 = rows.iter().map(|row| row.total).sum();
        println!(
            "  合计 {keys} 键：整串总计 {total_all:.0} µs，折合每键 {:.1} µs（中位数均值 {:.1}）",
            total_all / keys as f64,
            median_sum / rows.len() as f64
        );
        println!();
    }

    // 两版并列的倍数差异（只对两边都跑通时输出）
    if results.len() == 2 {
        let (left_label, left) = &results[0];
        let (right_label, right) = &results[1];
        println!("| 输入 | {left_label} | {right_label} | 倍数 |");
        println!("|---|---|---|---|");
        for (left_row, right_row) in left.iter().zip(right.iter()) {
            let ratio = if left_row.total > 0.0 {
                right_row.total / left_row.total
            } else {
                0.0
            };
            println!(
                "| `{}` | {:.0} µs | {:.0} µs | {ratio:.2}× |",
                left_row.input, left_row.total, right_row.total
            );
        }
        let left_all: f64 = left.iter().map(|row| row.total).sum();
        let right_all: f64 = right.iter().map(|row| row.total).sum();
        if left_all > 0.0 {
            println!(
                "| **合计** | {left_all:.0} µs | {right_all:.0} µs | **{:.2}×** |",
                right_all / left_all
            );
        }
    }
}
