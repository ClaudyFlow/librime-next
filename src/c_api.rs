//! `src/rime_api.h` + `rime_api.cc`：对外 ABI 边界。
//!
//! 这是 `librime-next` 作为 **rime.dll 替代品** 的对外契约。结构体（`RimeApi` / `RimeTraits` /
//! `RimeContext` / …）的字段顺序与 `src/rime/rime_api.h` **逐字对齐**（含 `data_size` 偏移约定），
//! 这样现有前端（如 Weasel）无需改头即可链接。
//!
//! 本文件只做「C 边界 ↔ 内部 `Session`」的翻译；真正的组字逻辑在 `engine` 模块。

#![allow(non_snake_case, dead_code)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::os::raw::c_double;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::engine::key_event::KeyEvent;
use crate::engine::session::Session;
use crate::lever::deployer::Deployer;

type Bool = c_int;
type RimeSessionId = usize;

// ----------------------------------------------------------------- 全局状态

struct Global {
    shared_data_dir: String,
    user_data_dir: String,
    /// 程序文件夹（安装目录）。运行期可经 `set_program_dir` 注入/覆盖，
    /// 缺省在 `deployer_initialize` 时由 `shared_data_dir` 的父目录推出。
    program_dir: String,
    prebuilt_data_dir: String,
    staging_dir: String,
    sessions: HashMap<RimeSessionId, Session>,
    next_id: RimeSessionId,
    /// 每个会话**最新一次**方案重建的代号 + 目标方案（见 `rime_select_schema`）。
    ///
    /// 语义：键在表里 = 该会话有后台重建在跑（或已受理还没跑完）。
    /// 后台任务建好后回到这里比对代号，不一致说明期间又被切过，
    /// 本次结果直接丢弃（避免"先受理的慢重建"覆盖"后受理的切换"）。
    /// 目标方案用来吞掉**重复请求同一个方案**（宿主重复调用不该再重建一遍）。
    rebuild_generation: HashMap<RimeSessionId, (u64, String)>,
    /// 重建代号自增器。
    next_rebuild_generation: u64,
    /// 部署器（C++ `Service::instance().deployer()`）。`deployer_initialize` 建它，
    /// `deploy` / `prebuild` / `deploy_schema` … 用它。
    deployer: Option<Deployer>,
    /// 后台维护线程(并行部署,用户 2026-11-08):启动维护时取走 Deployer,
    /// 线程跑完 HRRN 调度的任务后送回。JoinHandle 供 join。
    maintenance: Option<std::thread::JoinHandle<bool>>,
}

impl Global {
    fn build_dir(&self) -> PathBuf {
        if !self.staging_dir.is_empty() {
            PathBuf::from(&self.staging_dir)
        } else if !self.prebuilt_data_dir.is_empty() {
            PathBuf::from(&self.prebuilt_data_dir)
        } else {
            // 真实 build 恒在**用户目录**下（`<user>/build`，见 rime_layout 契约）；
            // 宿主只传 `shared_data_dir`/`user_data_dir`、`staging_dir` 留空，
            // 因此这里必须回退到用户目录的 build，而不是 `shared_data_dir/build`
            // （程序目录的 `build` 不存在、也没有任何 .bin）。
            PathBuf::from(&self.user_data_dir).join("build")
        }
    }

    /// 数据根列表，按 C++ `ResourceResolver` 的顺序：**用户目录优先、共享目录其次**。
    ///
    /// `simplifier`（`opencc/`）与神经模型（`model/*.safetensors`）靠它找外部数据 ——
    /// 两者不一定在同一个根下：`model/` 在共享目录、opencc 数据也可能另置一份。
    fn data_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = Vec::new();
        for dir in [&self.user_data_dir, &self.shared_data_dir] {
            if dir.is_empty() {
                continue;
            }
            let path = PathBuf::from(dir);
            if !roots.contains(&path) {
                roots.push(path);
            }
        }
        roots
    }
}

fn global() -> &'static Mutex<Global> {
    static G: OnceLock<Mutex<Global>> = OnceLock::new();
    G.get_or_init(|| {
        Mutex::new(Global {
            shared_data_dir: String::new(),
            user_data_dir: String::new(),
            program_dir: String::new(),
            prebuilt_data_dir: String::new(),
            staging_dir: String::new(),
            sessions: HashMap::new(),
            next_id: 1,
            rebuild_generation: HashMap::new(),
            next_rebuild_generation: 1,
            maintenance: None,
            deployer: None,
        })
    })
}

// ----------------------------------------------------------------- C 结构体（repr(C)，顺序对齐 rime_api.h）

#[repr(C)]
pub struct RimeTraits {
    pub data_size: c_int,
    pub shared_data_dir: *const c_char,
    pub user_data_dir: *const c_char,
    pub distribution_name: *const c_char,
    pub distribution_code_name: *const c_char,
    pub distribution_version: *const c_char,
    pub app_name: *const c_char,
    pub modules: *const *const c_char,
    pub min_log_level: c_int,
    pub log_dir: *const c_char,
    pub prebuilt_data_dir: *const c_char,
    pub staging_dir: *const c_char,
}

#[repr(C)]
pub struct RimeComposition {
    pub length: c_int,
    pub cursor_pos: c_int,
    pub sel_start: c_int,
    pub sel_end: c_int,
    pub preedit: *mut c_char,
}

#[repr(C)]
pub struct RimeCandidate {
    pub text: *mut c_char,
    pub comment: *mut c_char,
    pub reserved: *mut c_void,
}

#[repr(C)]
pub struct RimeMenu {
    pub page_size: c_int,
    pub page_no: c_int,
    pub is_last_page: Bool,
    pub highlighted_candidate_index: c_int,
    pub num_candidates: c_int,
    pub candidates: *mut RimeCandidate,
    pub select_keys: *mut c_char,
}

#[repr(C)]
pub struct RimeCommit {
    pub data_size: c_int,
    pub text: *mut c_char,
}

#[repr(C)]
pub struct RimeContext {
    pub data_size: c_int,
    pub composition: RimeComposition,
    pub menu: RimeMenu,
    pub commit_text_preview: *mut c_char,
    pub select_labels: *mut *mut c_char,
    /// 预测候选(上屏后联想,2026-11-01):null 结尾的字符串数组;无组字时
    /// 宿主可显示。数量 = predictions_len。
    pub predictions: *mut *mut c_char,
    pub predictions_len: c_int,
    /// 菜单总候选数(2026-11-13 扩展):懒式菜单要排空流才知道;0 = 未知/无候选。
    /// 宿主据此算底行「1 / N」页码。必须与 `vendor/librime/include/rime_api.h` 同序。
    pub total_candidates: c_int,
}

#[repr(C)]
pub struct RimeStatus {
    pub data_size: c_int,
    pub schema_id: *mut c_char,
    pub schema_name: *mut c_char,
    pub is_disabled: Bool,
    pub is_composing: Bool,
    pub is_ascii_mode: Bool,
    pub is_full_shape: Bool,
    pub is_simplified: Bool,
    pub is_traditional: Bool,
    pub is_ascii_punct: Bool,
}

#[repr(C)]
pub struct RimeCandidatePreview {
    pub data_size: c_int,
    pub text_before_selection: *mut c_char,
    pub selected_text: *mut c_char,
    pub text_after_selection: *mut c_char,
}

#[repr(C)]
pub struct RimeSchemaListItem {
    pub schema_id: *mut c_char,
    pub name: *mut c_char,
    pub reserved: *mut c_void,
}

#[repr(C)]
pub struct RimeSchemaList {
    pub size: usize,
    pub list: *mut RimeSchemaListItem,
}

#[repr(C)]
pub struct RimeCandidateListIterator {
    pub ptr: *mut c_void,
    pub index: c_int,
    pub candidate: RimeCandidate,
}

#[repr(C)]
pub struct RimeStringSlice {
    pub str: *const c_char,
    pub length: usize,
}

#[repr(C)]
pub struct RimeCustomApi {
    pub data_size: c_int,
}

#[repr(C)]
pub struct RimeModule {
    pub data_size: c_int,
    pub module_name: *const c_char,
    pub initialize: Option<unsafe extern "C" fn()>,
    pub finalize: Option<unsafe extern "C" fn()>,
    pub get_api: Option<unsafe extern "C" fn() -> *mut RimeCustomApi>,
}

type RimeNotificationHandler =
    Option<unsafe extern "C" fn(*mut c_void, RimeSessionId, *const c_char, *const c_char)>;

// ----------------------------------------------------------------- RimeApi 结构体（字段顺序对齐 rime_api.h）

#[repr(C)]
pub struct RimeApi {
    pub data_size: c_int,
    pub setup: Option<unsafe extern "C" fn(*mut RimeTraits)>,
    pub set_notification_handler:
        Option<unsafe extern "C" fn(RimeNotificationHandler, *mut c_void)>,
    pub initialize: Option<unsafe extern "C" fn(*mut RimeTraits)>,
    pub finalize: Option<unsafe extern "C" fn()>,
    pub start_maintenance: Option<unsafe extern "C" fn(Bool) -> Bool>,
    pub is_maintenance_mode: Option<unsafe extern "C" fn() -> Bool>,
    pub join_maintenance_thread: Option<unsafe extern "C" fn()>,
    pub deployer_initialize: Option<unsafe extern "C" fn(*mut RimeTraits)>,
    pub prebuild: Option<unsafe extern "C" fn() -> Bool>,
    pub deploy: Option<unsafe extern "C" fn() -> Bool>,
    pub deploy_schema: Option<unsafe extern "C" fn(*const c_char) -> Bool>,
    pub deploy_config_file: Option<unsafe extern "C" fn(*const c_char, *const c_char) -> Bool>,
    pub sync_user_data: Option<unsafe extern "C" fn() -> Bool>,
    pub create_session: Option<unsafe extern "C" fn() -> RimeSessionId>,
    pub find_session: Option<unsafe extern "C" fn(RimeSessionId) -> Bool>,
    pub destroy_session: Option<unsafe extern "C" fn(RimeSessionId) -> Bool>,
    pub cleanup_stale_sessions: Option<unsafe extern "C" fn()>,
    pub cleanup_all_sessions: Option<unsafe extern "C" fn()>,
    pub process_key: Option<unsafe extern "C" fn(RimeSessionId, c_int, c_int) -> Bool>,
    pub commit_composition: Option<unsafe extern "C" fn(RimeSessionId) -> Bool>,
    /// 直拟上屏（符号面板点击）：不经组合直接提交 UTF-8 文本。
    pub commit_string: Option<unsafe extern "C" fn(RimeSessionId, *const c_char) -> Bool>,
    pub clear_composition: Option<unsafe extern "C" fn(RimeSessionId)>,
    pub get_commit: Option<unsafe extern "C" fn(RimeSessionId, *mut RimeCommit) -> Bool>,
    pub free_commit: Option<unsafe extern "C" fn(*mut RimeCommit)>,
    pub get_context: Option<unsafe extern "C" fn(RimeSessionId, *mut RimeContext) -> Bool>,
    pub free_context: Option<unsafe extern "C" fn(*mut RimeContext)>,
    pub get_status: Option<unsafe extern "C" fn(RimeSessionId, *mut RimeStatus) -> Bool>,
    pub free_status: Option<unsafe extern "C" fn(*mut RimeStatus)>,
    pub set_option: Option<unsafe extern "C" fn(RimeSessionId, *const c_char, Bool)>,
    pub get_option: Option<unsafe extern "C" fn(RimeSessionId, *const c_char) -> Bool>,
    pub set_property: Option<unsafe extern "C" fn(RimeSessionId, *const c_char, *const c_char)>,
    pub get_property:
        Option<unsafe extern "C" fn(RimeSessionId, *const c_char, *mut c_char, usize) -> Bool>,
    pub get_schema_list: Option<unsafe extern "C" fn(*mut RimeSchemaList) -> Bool>,
    pub free_schema_list: Option<unsafe extern "C" fn(*mut RimeSchemaList)>,
    pub get_current_schema:
        Option<unsafe extern "C" fn(RimeSessionId, *mut c_char, usize) -> Bool>,
    pub select_schema: Option<unsafe extern "C" fn(RimeSessionId, *const c_char) -> Bool>,
    pub schema_open: Option<unsafe extern "C" fn(*const c_char, *mut RimeConfig) -> Bool>,
    pub config_open: Option<unsafe extern "C" fn(*const c_char, *mut RimeConfig) -> Bool>,
    pub config_close: Option<unsafe extern "C" fn(*mut RimeConfig)>,
    pub config_get_bool: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *mut Bool) -> Bool>,
    pub config_get_int: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *mut c_int) -> Bool>,
    pub config_get_double:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *mut c_double) -> Bool>,
    pub config_get_string:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *mut c_char, usize) -> Bool>,
    pub config_get_cstring:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> *const c_char>,
    pub config_update_signature:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> Bool>,
    pub config_begin_map:
        Option<unsafe extern "C" fn(*mut RimeConfigIterator, *mut RimeConfig, *const c_char) -> Bool>,
    pub config_next: Option<unsafe extern "C" fn(*mut RimeConfigIterator) -> Bool>,
    pub config_end: Option<unsafe extern "C" fn(*mut RimeConfigIterator)>,
    pub simulate_key_sequence:
        Option<unsafe extern "C" fn(RimeSessionId, *const c_char) -> Bool>,
    pub register_module: Option<unsafe extern "C" fn(*mut RimeModule) -> Bool>,
    pub find_module: Option<unsafe extern "C" fn(*const c_char) -> *mut RimeModule>,
    pub run_task: Option<unsafe extern "C" fn(*const c_char) -> Bool>,
    pub get_shared_data_dir: Option<unsafe extern "C" fn() -> *const c_char>,
    pub get_user_data_dir: Option<unsafe extern "C" fn() -> *const c_char>,
    pub get_sync_dir: Option<unsafe extern "C" fn() -> *const c_char>,
    pub get_user_id: Option<unsafe extern "C" fn() -> *const c_char>,
    pub get_user_data_sync_dir: Option<unsafe extern "C" fn(*mut c_char, usize)>,
    pub config_init: Option<unsafe extern "C" fn(*mut RimeConfig) -> Bool>,
    pub config_load_string: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> Bool>,
    pub config_set_bool: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, Bool) -> Bool>,
    pub config_set_int: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, c_int) -> Bool>,
    pub config_set_double:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, c_double) -> Bool>,
    pub config_set_string:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *const c_char) -> Bool>,
    pub config_get_item:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *mut RimeConfig) -> Bool>,
    pub config_set_item:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *mut RimeConfig) -> Bool>,
    pub config_clear: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> Bool>,
    pub config_create_list: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> Bool>,
    pub config_create_map: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> Bool>,
    pub config_list_size: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> usize>,
    pub config_begin_list:
        Option<unsafe extern "C" fn(*mut RimeConfigIterator, *mut RimeConfig, *const c_char) -> Bool>,
    pub get_input: Option<unsafe extern "C" fn(RimeSessionId) -> *const c_char>,
    pub get_caret_pos: Option<unsafe extern "C" fn(RimeSessionId) -> usize>,
    pub select_candidate: Option<unsafe extern "C" fn(RimeSessionId, usize) -> Bool>,
    pub get_version: Option<unsafe extern "C" fn() -> *const c_char>,
    pub set_caret_pos: Option<unsafe extern "C" fn(RimeSessionId, usize)>,
    pub select_candidate_on_current_page: Option<unsafe extern "C" fn(RimeSessionId, usize) -> Bool>,
    pub candidate_list_begin:
        Option<unsafe extern "C" fn(RimeSessionId, *mut RimeCandidateListIterator) -> Bool>,
    pub candidate_list_next: Option<unsafe extern "C" fn(*mut RimeCandidateListIterator) -> Bool>,
    pub candidate_list_end: Option<unsafe extern "C" fn(*mut RimeCandidateListIterator)>,
    pub user_config_open: Option<unsafe extern "C" fn(*const c_char, *mut RimeConfig) -> Bool>,
    pub candidate_list_from_index:
        Option<unsafe extern "C" fn(RimeSessionId, *mut RimeCandidateListIterator, c_int) -> Bool>,
    pub get_prebuilt_data_dir: Option<unsafe extern "C" fn() -> *const c_char>,
    pub get_staging_dir: Option<unsafe extern "C" fn() -> *const c_char>,
    pub commit_proto: Option<unsafe extern "C" fn(RimeSessionId, *mut c_void)>,
    pub context_proto: Option<unsafe extern "C" fn(RimeSessionId, *mut c_void)>,
    pub status_proto: Option<unsafe extern "C" fn(RimeSessionId, *mut c_void)>,
    pub get_state_label:
        Option<unsafe extern "C" fn(RimeSessionId, *const c_char, Bool) -> *const c_char>,
    pub delete_candidate: Option<unsafe extern "C" fn(RimeSessionId, usize) -> Bool>,
    pub delete_candidate_on_current_page: Option<unsafe extern "C" fn(RimeSessionId, usize) -> Bool>,
    pub get_state_label_abbreviated: Option<
        unsafe extern "C" fn(RimeSessionId, *const c_char, Bool, Bool) -> RimeStringSlice,
    >,
    pub set_input: Option<unsafe extern "C" fn(RimeSessionId, *const c_char)>,
    pub get_shared_data_dir_s: Option<unsafe extern "C" fn(*mut c_char, usize)>,
    pub get_user_data_dir_s: Option<unsafe extern "C" fn(*mut c_char, usize)>,
    pub get_prebuilt_data_dir_s: Option<unsafe extern "C" fn(*mut c_char, usize)>,
    pub get_staging_dir_s: Option<unsafe extern "C" fn(*mut c_char, usize)>,
    pub get_sync_dir_s: Option<unsafe extern "C" fn(*mut c_char, usize)>,
    pub highlight_candidate: Option<unsafe extern "C" fn(RimeSessionId, usize) -> Bool>,
    pub highlight_candidate_on_current_page: Option<unsafe extern "C" fn(RimeSessionId, usize) -> Bool>,
    pub change_page: Option<unsafe extern "C" fn(RimeSessionId, Bool) -> Bool>,
    pub get_candidate_preview:
        Option<unsafe extern "C" fn(RimeSessionId, *mut RimeCandidatePreview) -> Bool>,
    pub free_candidate_preview: Option<unsafe extern "C" fn(*mut RimeCandidatePreview) -> Bool>,
    // ---- 三个根目录的 get/set（2026-10-25 增补，DLL 尾部扩展）。
    //      **必须置于 RimeApi 末位**：否则会挤占 `get_candidate_preview` /
    //      `free_candidate_preview` 这两位 C++ 原生函数的偏移，宿主按 C++ rime_api.h
    //      编译时调用它们会落到错误的函数指针上。
    /// 程序文件夹（`program_dir`）。返回内部静态副本，调用方不应释放/改写。
    pub get_program_dir: Option<unsafe extern "C" fn() -> *const c_char>,
    /// 程序文件夹（带缓冲区的线程安全版本）。
    pub get_program_dir_s: Option<unsafe extern "C" fn(*mut c_char, usize)>,
    /// 设置程序文件夹（运行期覆盖，部署时 `layout()` 会用到）。
    pub set_program_dir: Option<unsafe extern "C" fn(*const c_char)>,
    /// 设置共享数据目录（= `shared_data_dir`，即 `Rime-Next` 数据目录）。
    pub set_shared_data_dir: Option<unsafe extern "C" fn(*const c_char)>,
    /// 设置用户数据目录（= `user_data_dir`）。
    pub set_user_data_dir: Option<unsafe extern "C" fn(*const c_char)>,
}

#[repr(C)]
pub struct RimeConfig {
    pub ptr: *mut c_void,
}

#[repr(C)]
pub struct RimeConfigIterator {
    pub list: *mut c_void,
    pub map: *mut c_void,
    pub index: c_int,
    pub key: *const c_char,
    pub path: *const c_char,
}

// ----------------------------------------------------------------- 辅助

unsafe fn cstr_dup(s: &str) -> *mut c_char {
    CString::new(s).map(|c| c.into_raw()).unwrap_or(std::ptr::null_mut())
}

unsafe fn cstr_free(p: *mut c_char) {
    if !p.is_null() {
        drop(CString::from_raw(p));
    }
}

/// `dup_string`（evo `rime_api_impl.h`）：空串返回 `nullptr`，不是空字符串。
unsafe fn dup_string(s: &str) -> *mut c_char {
    if s.is_empty() {
        std::ptr::null_mut()
    } else {
        cstr_dup(s)
    }
}

fn read_cstr(p: *const c_char) -> Option<String> {
    if p.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(p) }.to_str().ok().map(str::to_string)
    }
}

// ----------------------------------------------------------------- 核心实现

unsafe extern "C" fn rime_setup(_traits: *mut RimeTraits) {
    // 目录在 initialize 里读取
}

unsafe extern "C" fn rime_initialize(traits: *mut RimeTraits) {
    let mut g = global().lock().unwrap();
    if !traits.is_null() {
        let t = &*traits;
        if !t.shared_data_dir.is_null() {
            g.shared_data_dir = read_cstr(t.shared_data_dir).unwrap_or_default();
        }
        if !t.user_data_dir.is_null() {
            g.user_data_dir = read_cstr(t.user_data_dir).unwrap_or_default();
        }
        if !t.prebuilt_data_dir.is_null() {
            g.prebuilt_data_dir = read_cstr(t.prebuilt_data_dir).unwrap_or_default();
        }
        if !t.staging_dir.is_null() {
            g.staging_dir = read_cstr(t.staging_dir).unwrap_or_default();
        }
    }
}

unsafe extern "C" fn rime_finalize() {
    clear_sessions(&mut global().lock().unwrap());
    // 进程退出前摘掉 C 通知槽，避免 DLL 卸载后总线里留一个悬垂函数指针。
    clear_notification_slot();
    // 退出前把还没落盘的个人 n-gram / 重排头参数同步写一次
    // （后台线程随进程退出会被直接杀掉）。
    crate::engine::user_ngram::flush_on_exit();
    crate::gear::neural::rerank_head::flush_on_exit();
}

/// 宿主注册的通知回调（C++ `RimeNotificationHandler`）＝ **信号总线上的一个槽**。
///
/// 四种消息（`schema` / `option` / `property` / `deploy`）都由
/// [`crate::signal::emit_for`] 统一编码成官方 `(message_type, message_value)`；
/// 这里只负责把 C 函数指针挂到总线上，不再有「只有方案切换才通知」的特例。
/// `*mut c_void` 不是 `Send`，而信号可能从**后台重建线程**或部署线程发出，
/// 所以上下文存成 `usize`、裸 fn 从 `Option<fn>` 里拆出来。
/// C 槽在总线里的句柄；`0` = 未注册。重复注册先摘旧槽，`None` = 只摘不装。
static NOTIFY_SLOT: Mutex<u64> = Mutex::new(0);

fn clear_notification_slot() {
    let mut slot = NOTIFY_SLOT.lock().unwrap_or_else(|err| err.into_inner());
    if *slot != 0 {
        crate::signal::disconnect(*slot);
        *slot = 0;
    }
}

unsafe extern "C" fn rime_set_notification_handler(
    handler: RimeNotificationHandler,
    ctx: *mut c_void,
) {
    clear_notification_slot();
    let Some(func) = handler else { return };
    let ctx = ctx as usize;
    let id = crate::signal::connect(Arc::new(
        move |note: &crate::signal::Notification| {
            let Ok(message_type) = CString::new(note.message_type()) else {
                return;
            };
            let Ok(message_value) = CString::new(note.message_value()) else {
                return;
            };
            // SAFETY: `ctx` / `func` 的生命周期由注册方保证；两个字符串在本次调用期间存活。
            // 槽只做非阻塞投递（Engine 侧是 `try_send`），不取引擎锁、不做 I/O。
            unsafe {
                func(
                    ctx as *mut c_void,
                    note.session_id,
                    message_type.as_ptr(),
                    message_value.as_ptr(),
                )
            };
        },
    ));
    *NOTIFY_SLOT.lock().unwrap_or_else(|err| err.into_inner()) = id;
}

// ----------------------------------------------------------------- 部署（deployer）

/// `RimeDeployerInitialize(traits)`（C++ `rime_api_impl.h:107`）。
///
/// 等价于 C++ 的 `SetupDeployer(traits)`：把 traits 里的目录/版本字段搬进 `Deployer`。
/// `prebuilt_data_dir` / `staging_dir` 缺省分别是 `<shared>/build` 与 `<user>/build`。
unsafe extern "C" fn rime_deployer_initialize(traits: *mut RimeTraits) {
    let mut g = global().lock().unwrap();
    if traits.is_null() {
        return;
    }
    let t = &*traits;
    let mut d = Deployer::new();
    if let Some(v) = read_cstr(t.shared_data_dir) {
        d.shared_data_dir = PathBuf::from(v);
    }
    if let Some(v) = read_cstr(t.user_data_dir) {
        d.user_data_dir = PathBuf::from(v);
    }
    if let Some(v) = read_cstr(t.distribution_name) {
        d.distribution_name = v;
    }
    if let Some(v) = read_cstr(t.distribution_code_name) {
        d.distribution_code_name = v;
    }
    if let Some(v) = read_cstr(t.distribution_version) {
        d.distribution_version = v;
    }
    if let Some(v) = read_cstr(t.app_name) {
        d.app_name = v;
    }
    // 2026-10-25 硬规则（执行点在注入侧；`Deployer::layout()` 忠实反映字段）：
    // * 出厂预编译（prebuilt）取消——程序文件夹禁止任何 build，prebuilt **恒空**，
    //   无视 `RimeTraits` 传入值；
    // * **build 只存在于用户文件夹**——不论宿主传什么 staging_dir，都强制为
    //   `<user_data_dir>/build`。要沙箱化部署的调用方（测试）可在
    //   `deployer_initialize` 之后直接改 `Deployer.staging_dir` 字段。
    d.prebuilt_data_dir = PathBuf::new();
    d.staging_dir = d.user_data_dir.join("build");
    d.sync_dir = d.user_data_dir.join(crate::lever::paths::SYNC_DIR);
    if let Some(v) = read_cstr(t.log_dir) {
        d.log_dir = PathBuf::from(v);
    }
    // 程序文件夹（`PROGRAM_DIR`）= 共享数据目录（被宿主经 API 设成 `Rime-Next` 数据目录）
    // 的父目录；无父目录时退回共享数据目录自身。宿主也可用 `RimeTraits` 之外的 setter 显式覆盖。
    d.program_dir = d
        .shared_data_dir
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| d.shared_data_dir.clone());
    g.deployer = Some(d);
}

/// `RimeDeployWorkspace()`（C++ `rime_api_impl.h:118`）。
///
/// `installation_update && workspace_update && user_dict_upgrade && cleanup_trash`
/// —— **短路**语义：前一步失败后面根本不跑。
unsafe extern "C" fn rime_deploy() -> Bool {
    let mut g = global().lock().unwrap();
    match g.deployer.as_mut() {
        Some(d) => d.deploy_workspace() as Bool,
        None => 0,
    }
}

/// `RimePrebuildAllSchemas()`。
unsafe extern "C" fn rime_prebuild() -> Bool {
    let mut g = global().lock().unwrap();
    match g.deployer.as_mut() {
        Some(d) => d.prebuild_all_schemas() as Bool,
        None => 0,
    }
}

/// `RimeDeploySchema(schema_file)`。
unsafe extern "C" fn rime_deploy_schema(schema_file: *const c_char) -> Bool {
    let Some(path) = read_cstr(schema_file) else {
        return 0;
    };
    let mut g = global().lock().unwrap();
    match g.deployer.as_mut() {
        Some(d) => d.deploy_schema(path) as Bool,
        None => 0,
    }
}

/// `RimeDeployConfigFile(file_name, version_key)`。
unsafe extern "C" fn rime_deploy_config_file(
    file_name: *const c_char,
    version_key: *const c_char,
) -> Bool {
    let (Some(name), Some(key)) = (read_cstr(file_name), read_cstr(version_key)) else {
        return 0;
    };
    let mut g = global().lock().unwrap();
    match g.deployer.as_mut() {
        Some(d) => d.deploy_config_file(&name, &key) as Bool,
        None => 0,
    }
}

/// `RimeSyncUserData()`。
unsafe extern "C" fn rime_sync_user_data() -> Bool {
    let mut g = global().lock().unwrap();
    match g.deployer.as_mut() {
        Some(d) => d.sync_user_data() as Bool,
        None => 0,
    }
}

/// `RimeStartMaintenance(full_check)`。
unsafe extern "C" fn rime_start_maintenance(full_check: Bool) -> Bool {
    let mut g = global().lock().unwrap();
    // 并行部署(用户 2026-11-08):维护挪进后台线程,HRRN 调度在线程里跑,
    // 打字不再被部署阻塞。Deployer 被取走 ⇒ 维护模式自然生效。
    let Some(d) = g.deployer.take() else { return 0 };
    let handle = std::thread::spawn(move || {
        let mut d = d;
        let ok = d.start_maintenance(full_check != 0);
        // 跑完把 Deployer 送回全局(会话恢复创建能力)
        let mut g = global().lock().unwrap();
        g.deployer = Some(d);
        ok
    });
    g.maintenance = Some(handle);
    1
}

/// `RimeIsMaintenanceMode()`。
unsafe extern "C" fn rime_is_maintenance_mode() -> Bool {
    let g = global().lock().unwrap();
    // Deployer 不在全局 = 后台维护线程正持有它 ⇒ 维护模式。
    (g.deployer.is_none() && g.maintenance.is_some()) as Bool
}

/// `RimeJoinMaintenanceThread()` —— 我们的部署是同步执行的，没有工作线程可 join。
unsafe extern "C" fn rime_join_maintenance_thread() {
    let mut g = global().lock().unwrap();
    if let Some(handle) = g.maintenance.take() {
        let _ = handle.join();
    }
}

unsafe extern "C" fn rime_create_session() -> RimeSessionId {
    let mut g = global().lock().unwrap();
    let id = g.next_id;
    g.next_id += 1;
    let build_dir = g.build_dir();
    let roots = g.data_roots();
    // 2026-11-10：分段计时。线上「rime.lua 开始加载之后引擎静默 N 秒」的时刻正好
    // 落在建会话里，所以把这里的三个阶段也打点（默认关闭，开关见 switch_trace）。
    crate::engine::switch_trace::begin_rebuild();
    let t_rescore = std::time::Instant::now();
    // 神经重排(进程级单例):数据根里有 safetensors 模型才启用。
    crate::gear::filter::rescore::init_from_data_roots(&roots);
    crate::engine::switch_trace::done("session:rescore 初始化", Some(t_rescore));
    let t_eassy = std::time::Instant::now();
    // eassy（细胞词库）运行时索引：后台建 trie，未就绪/失败都不影响输入。
    crate::gear::dict::eassy_matcher::init_from_data_roots(&roots);
    crate::engine::switch_trace::done("session:eassy 启动(后台)", Some(t_eassy));
    let default_schema = default_schema_id(&g);
    crate::engine::switch_trace::note(&format!(
        "session:默认方案 {default_schema} @ {}",
        build_dir.display()
    ));
    let mut session = Session::create_with_roots(&build_dir, &default_schema, &roots);
    // 信号归属：这个会话发出的 option / property 通知都带 `id`。
    session.bind_signal_session(id);
    // 交互式切方案也走后台重建：`Engine::apply_schema` 只登记请求，由
    // `rime_process_key` 放锁后派发（见 `request_session_rebuild`）。不打开这个
    // 开关的话，Ctrl+` 选完方案的那一键会卡在全局锁里同步重建整个引擎。
    session.set_async_rebuild(rebuild_async_enabled());
    g.sessions.insert(id, session);
    id
}

/// `<user>/schema/schema.yaml` 的相对路径（点名制方案清单，取代已废弃的 `schema.toml`）。
fn crate_schema_manifest_rel() -> std::path::PathBuf {
    std::path::PathBuf::from(crate::lever::paths::SCHEMA_DIR)
        .join(crate::lever::paths::SCHEMA_MANIFEST)
}

/// 用户 `schema.yaml` 的 `schemas` 勾选集合（内部状态）。
///
/// 取消勾选 `pinyin` 只从这里移除父 id；子方案条目仍留在文件里。
/// 用户根有非空清单时不再看共享根那份出厂清单。
fn selected_schema_ids(g: &Global) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for dir in [&g.user_data_dir, &g.shared_data_dir] {
        if dir.is_empty() {
            continue;
        }
        let path = PathBuf::from(dir).join(crate_schema_manifest_rel());
        if let Ok(text) = std::fs::read_to_string(&path) {
            let ids = extract_schema_list_ids(&text);
            if !ids.is_empty() {
                out.extend(ids);
                break;
            }
        }
    }
    out
}

/// 真正不部署 / 不进菜单的 id：显式 `disabled.yaml` ∪ 父未选压住的子方案。
fn excluded_schema_ids(g: &Global) -> BTreeSet<String> {
    let mut out = disabled_ids(g);
    let selected = selected_schema_ids(g);
    if selected.is_empty() {
        return out;
    }
    let user = PathBuf::from(&g.user_data_dir);
    let shared = if g.shared_data_dir.is_empty() {
        user.clone()
    } else {
        PathBuf::from(&g.shared_data_dir)
    };
    let parents = crate::lever::schema_disabled::scan_parents(&shared, &user);
    out.extend(crate::lever::schema_disabled::blocked_children_by_unselected_parents(
        &selected, &parents,
    ));
    out
}

fn default_schema_id(g: &Global) -> String {
    // 2026-11-11 修正：用户取消勾选 pinyin 时，active 仍原样留在 schema.yaml，
    // 但 pinyin 不在勾选集合里，新会话只在**内存**里退到第一个可用的已勾选方案。
    // 不是把 pinyin 写进 disabled.yaml —— 那样父方案也灰掉，就没法重新勾回来了。
    let selected = selected_schema_ids(g);
    let excluded = excluded_schema_ids(g);
    let mut candidates: Vec<String> = Vec::new();

    // 用户 schema.yaml 优先：active 只有在仍被勾选时才可用，其余按 schemas 顺序。
    if !g.user_data_dir.is_empty() {
        let path = PathBuf::from(&g.user_data_dir).join(crate_schema_manifest_rel());
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Some(active) = extract_active_id(&text) {
                if selected.is_empty() || selected.contains(&active) {
                    push_unique(&mut candidates, active);
                }
            }
            for id in extract_schema_list_ids(&text) {
                if selected.is_empty() || selected.contains(&id) {
                    push_unique(&mut candidates, id);
                }
            }
        }
    }
    // 其余文件只补充仍在勾选集合里的 id（保留首次运行 / 老版本兜底顺序）。
    for dir in [g.shared_data_dir.clone(), g.user_data_dir.clone()] {
        if dir.is_empty() {
            continue;
        }
        for name in [Path::new("default.yaml"), Path::new("schema/default.yaml")] {
            let path = PathBuf::from(&dir).join(name);
            if let Ok(text) = std::fs::read_to_string(&path) {
                for id in extract_schema_list_ids(&text) {
                    if selected.is_empty() || selected.contains(&id) {
                        push_unique(&mut candidates, id);
                    }
                }
            }
        }
    }
    for id in &candidates {
        if !excluded.contains(id) {
            return id.clone();
        }
    }
    // 全被压住：宁可仍用第一候选（输入不中断），也不在磁盘上改 active。
    candidates
        .first()
        .cloned()
        .unwrap_or_else(|| "luna_pinyin".to_string())
}

/// 当前**显式**禁用集合（用户目录的 disabled.yaml；组按 schema.parent 展开）。
/// 父方案未勾选自动压住的子方案不写在这里，由 [`excluded_schema_ids`] 合并。
fn disabled_ids(g: &Global) -> BTreeSet<String> {
    if g.user_data_dir.is_empty() {
        return BTreeSet::new();
    }
    let user = PathBuf::from(&g.user_data_dir);
    let shared = if g.shared_data_dir.is_empty() {
        user.clone()
    } else {
        PathBuf::from(&g.shared_data_dir)
    };
    crate::lever::schema_disabled::effective_disabled_ids(&user, &shared)
}

/// 从 `schema/schema.yaml` 的 `active:` / `schemas:`，或 `default.yaml` 的 `schema_list`
/// 取**全部候选 id**，顺序即优先级。
///
/// 顺序：**`active:` 优先**（那是"当前方案"，新会话一建出来就该是它），
/// 其后是清单里的每一项（首次运行、或 active 被禁用时按这个顺序回退）。
///
/// ⚠️ 2026-10-29：原来只看清单第一条，而 `schema.yaml` 的清单是按 id 排序的
/// （第一条是 `cangjie.5`），于是每个新会话先落在**仓颉**上；靠 Engine 侧再
/// `select_schema(active)` 纠正 —— 而那条路当时又是坏的（见 `Engine/src/librime/api.rs`
/// 漏装 `select_schema`），结果"设置里选的方案不生效"。两道修一起才对。
///
/// 2026-11-11：返回的**候选序列**让「active 被禁用」能退到清单里下一个未禁用方案，
/// 而不是只看第一个就 return。
fn extract_schema_id_candidates(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(id) = extract_active_id(text) {
        push_unique(&mut out, id);
    }
    for id in extract_schema_list_ids(text) {
        push_unique(&mut out, id);
    }
    out
}

/// 只取 `active: <id>` 这一行（注释不算），供「active 仍被勾选吗」判断。
fn extract_active_id(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("active:") {
            let id = rest.trim().trim_matches(['"', '\''].as_ref()).trim();
            return (!id.is_empty()).then(|| id.to_string());
        }
    }
    None
}

/// 只取 `schema_list:` / `schemas:` 里的 id，**不含 active**。
/// 顺序即优先级；`default.yaml` 用 `- schema: xxx`，`schema.yaml` 用 `- xxx`。
fn extract_schema_list_ids(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_list = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with("schema_list:") || trimmed.starts_with("schemas:") {
            in_list = true;
            continue;
        }
        if in_list {
            if trimmed.starts_with('-') {
                let body = trimmed.trim_start_matches('-').trim();
                if let Some(rest) = body.strip_prefix("schema:") {
                    push_unique(&mut out, rest.trim().to_string());
                } else if !body.is_empty() && !body.contains(':') {
                    push_unique(&mut out, body.to_string());
                }
            } else if !trimmed.is_empty() {
                break;
            }
        }
    }
    out
}

fn push_unique(out: &mut Vec<String>, id: String) {
    if !id.is_empty() && !out.contains(&id) {
        out.push(id);
    }
}

fn extract_first_schema_id(text: &str) -> Option<String> {
    extract_schema_id_candidates(text).into_iter().next()
}

unsafe extern "C" fn rime_find_session(id: RimeSessionId) -> Bool {
    global().lock().unwrap().sessions.contains_key(&id) as Bool
}

unsafe extern "C" fn rime_destroy_session(id: RimeSessionId) -> Bool {
    remove_session(&mut global().lock().unwrap(), id).is_some() as Bool
}

unsafe extern "C" fn rime_cleanup_stale_sessions() {}
unsafe extern "C" fn rime_cleanup_all_sessions() {
    clear_sessions(&mut global().lock().unwrap());
}

/// 清空全部会话，同时丢掉它们的后台重建代号（否则迟到的重建结果会空转一圈）。
fn clear_sessions(g: &mut Global) {
    g.sessions.clear();
    g.rebuild_generation.clear();
}

/// 销毁单个会话：先撤掉重建代号，让在跑的后台任务建完直接作废。
fn remove_session(g: &mut Global, id: RimeSessionId) -> Option<Session> {
    g.rebuild_generation.remove(&id);
    g.sessions.remove(&id)
}

/// 是否走「后台重建 + 原子换入」。
///
/// `RIME_REBUILD_SYNC=1` 时**整体**退回历史同步行为：`rime_select_schema` 与
/// 交互式切换（`Engine::apply_schema`）一起回退，方便线上 A/B 与一键回滚。
fn rebuild_async_enabled() -> bool {
    std::env::var_os("RIME_REBUILD_SYNC").is_none()
}

unsafe extern "C" fn rime_process_key(id: RimeSessionId, keycode: c_int, mask: c_int) -> Bool {
    // 交互式切方案（Ctrl+` / F4 菜单）的处理分两段：
    // ① 本键只跑到 `Engine::apply_schema`，它把目标方案登记进 `pending_rebuild`
    //    就返回 —— 这一键不进重建；
    // ② 锁一放掉，才把请求交给重建线程（与 `rime_select_schema` 同一个队列）。
    // 这样切换那一刻的按键、以及重建窗口里的按键，都不会排在全局锁后面。
    let (eaten, pending) = {
        let mut g = global().lock().unwrap();
        match g.sessions.get_mut(&id) {
            Some(s) => {
                let eaten = s.process_key(keycode as u32, mask as u32) as Bool;
                (eaten, s.take_pending_rebuild())
            }
            None => return 0,
        }
    }; // ← 锁在这里放掉
    if let Some(sid) = pending {
        request_session_rebuild(id, &sid);
    }
    eaten
}

unsafe extern "C" fn rime_commit_composition(id: RimeSessionId) -> Bool {
    let mut g = global().lock().unwrap();
    if let Some(s) = g.sessions.get_mut(&id) {
        s.commit_composition() as Bool
    } else {
        0
    }
}

unsafe extern "C" fn rime_commit_string(id: RimeSessionId, text: *const c_char) -> Bool {
    if text.is_null() {
        return 0;
    }
    let Ok(text) = CStr::from_ptr(text).to_str() else {
        return 0;
    };
    let mut g = global().lock().unwrap();
    if let Some(s) = g.sessions.get_mut(&id) {
        s.commit_string(text) as Bool
    } else {
        0
    }
}

unsafe extern "C" fn rime_clear_composition(id: RimeSessionId) {
    if let Some(s) = global().lock().unwrap().sessions.get_mut(&id) {
        s.clear_composition();
    }
}

unsafe extern "C" fn rime_get_commit(id: RimeSessionId, commit: *mut RimeCommit) -> Bool {
    if commit.is_null() {
        return 0;
    }
    let mut g = global().lock().unwrap();
    if let Some(s) = g.sessions.get_mut(&id) {
        if let Some(text) = s.take_commit() {
            (*commit).text = cstr_dup(&text);
            return 1;
        }
    }
    0
}

unsafe extern "C" fn rime_free_commit(commit: *mut RimeCommit) {
    if commit.is_null() {
        return;
    }
    cstr_free((*commit).text);
    (*commit).text = std::ptr::null_mut();
}

unsafe extern "C" fn rime_get_context(id: RimeSessionId, ctx: *mut RimeContext) -> Bool {
    if ctx.is_null() {
        return 0;
    }
    let mut g = global().lock().unwrap();
    // 神经重排结果落地检查在快照之前:后台算完一批,这次读到的就是重排后的列表。
    g.sessions
        .get_mut(&id)
        .map(|s| s.poll_rescore_landing());
    let Some(session) = g.sessions.get(&id) else {
        return 0;
    };
    let Some(context) = session.context() else {
        return 0;
    };
    let comp = &context.composition;
    let preedit = context.get_preedit();
    (*ctx).composition.length = preedit.text.len() as c_int;
    (*ctx).composition.cursor_pos = preedit.caret_pos as c_int;
    (*ctx).composition.sel_start = preedit.sel_start as c_int;
    (*ctx).composition.sel_end = preedit.sel_end as c_int;
    (*ctx).composition.preedit = cstr_dup(&preedit.text);

    // 菜单
    let menu = &comp.segmentation.segments.first().and_then(|s| s.menu.as_ref());
    // 翻页：**必须按高亮所在的页号取页**，不能永远取第 0 页。
    // 2026-10-31 修：以前写死 `m.page(page_size, 0)`，逗号/句号（
    // `key_binder` 的 `send: Page_Up/Page_Down`）把高亮移到第 6 条后，
    // 导出的仍是前 5 条 ⇒ 用户看到的就是"翻页没反应"。
    let page_size_units = context.page_size.max(1) as usize;
    let page_no = (context.highlight_index() / page_size_units) as usize;
    let mut is_last_flag: i32 = 1;
    let (cands, page_size, highlight, num) = if let Some(m) = menu {
        let (page, is_last) = m.page(context.page_size, page_no);
        is_last_flag = i32::from(is_last);
        let n = page.len();
        let mut buf: Vec<RimeCandidate> = Vec::with_capacity(n);
        for c in &page {
            buf.push(RimeCandidate {
                text: cstr_dup(&c.text),
                comment: cstr_dup(&c.comment),
                reserved: std::ptr::null_mut(),
            });
        }
        let raw = if n > 0 {
            Box::into_raw(buf.into_boxed_slice()) as *mut RimeCandidate
        } else {
            std::ptr::null_mut()
        };
        (
            raw,
            context.page_size as c_int,
            // 页内下标（渲染器按"本页第几条"用）
            (context.highlight_index() % page_size_units) as c_int,
            n as c_int,
        )
    } else {
        (std::ptr::null_mut(), context.page_size as c_int, 0, 0)
    };
    (*ctx).menu.candidates = cands;
    (*ctx).menu.num_candidates = num;
    (*ctx).menu.page_size = page_size;
    (*ctx).menu.page_no = page_no as c_int;
    (*ctx).menu.is_last_page = is_last_flag as c_int;
    (*ctx).menu.highlighted_candidate_index = highlight;
    // 「菜单键」：schema 的 `menu/alternative_select_keys`；方案没写就沿用旧行为。
    // evo `rime_api_impl.h:263-268` 只拷 schema 的值，空则留 null；这里保留
    // "123456789" 兜底，宿主（Weasel）一直依赖它画数字标签。
    let schema = session.engine.as_ref().map(|engine| &engine.schema);
    let select_keys = schema
        .map(|schema| schema.select_keys.as_str())
        .filter(|keys| !keys.is_empty())
        .unwrap_or("123456789");
    (*ctx).menu.select_keys = cstr_dup(select_keys);

    // 上屏预览（`rime_api_impl.h:230-236`）：组合非空且预览非空才填。
    (*ctx).commit_text_preview = if comp.empty() {
        std::ptr::null_mut()
    } else {
        dup_string(&comp.get_commit_text())
    };

    // 菜单备选标签（`rime_api_impl.h:269-280`）：`menu/alternative_select_labels`
    // 长度必须够一页，分配 `page_size` 个 `char*`（**不带** NULL 结尾，与 C++ 一致）。
    (*ctx).select_labels = std::ptr::null_mut();
    let label_count = page_size.max(0) as usize;
    if label_count > 0 {
        if let Some(labels) = schema
            .and_then(|schema| schema.config.pointer("/menu/alternative_select_labels"))
            .and_then(|value| value.as_array())
            .filter(|labels| labels.len() >= label_count)
        {
            let arr: Vec<*mut c_char> = labels
                .iter()
                .take(label_count)
                .map(|label| cstr_dup(label.as_str().unwrap_or("")))
                .collect();
            (*ctx).select_labels = Box::into_raw(arr.into_boxed_slice()) as *mut *mut c_char;
        }
    }
    // 预测:无组字时才有意义(有组字时清空,避免和候选混淆)。
    //
    // ⚠️ 2026-11-03 事故:`predictions` / `predictions_len` 是本工程**扩展**的字段,
    // 上游 `rime_api.h` 里的 `RimeContext` 只有 5 个字段。5e32988（11-03 01:17）加字段时
    // 没同步头文件,于是宿主(Engine 按头文件生成绑定)传进来的是 5 字段结构,
    // 这边却按 7 字段写 → **越界写 8 字节** → Engine 一按键就 0xc0000005(实测 7 次 APPCRASH)。
    // 现在按调用方声明的 `data_size` 判断:结构不够大就一个字节都不碰。
    //
    // ⚠️ 2026-11-07 修:门槛原来写的是 `>= size_of::<RimeContext>()`,而本工程的
    // `data_size` 约定是 `RIME_STRUCT_INIT` 的 `sizeof(Type) - sizeof(data_size)`
    // (宿主 `struct_data_size()` 同口径)。于是**即便宿主正确声明了这两个扩展字段,
    // data_size 也永远差 4 字节** → 预测一个都送不出去(链路第二处断点)。
    // 现在按约定减掉 `data_size` 自己那 4 字节。
    let required = std::mem::size_of::<RimeContext>() - std::mem::size_of::<c_int>();
    let extended = (*ctx).data_size as usize >= required;
    // 总候选数（本工程扩展字段 `total_candidates`，2026-11-13「方案②」加）：
    // 菜单是**懒式**的（`Menu::prepare` 按需物化），`menu.num_candidates` 只是当前页
    // 条数、`is_last_page` 也只在最后一页能反推总页数 ⇒ 想要「1 / N」页码必须排空流。
    // 代价实测（2026-11-13 探针）：`a`=11 条/1µs，`ni`=185/690µs，`jie`=1033/4567µs。
    // 无菜单（空组字 / 联想条 `predictions` / 纯预编辑）时为 0 ⇒ 宿主当「总数未知」。
    // 老宿主不声明这块内存就一个字节都不写（越界写会 0xc0000005，见 11-03 事故）。
    if extended {
        (*ctx).total_candidates = menu
            .as_ref()
            .map(|m| m.prepare(usize::MAX))
            .unwrap_or(0) as c_int;
    }
    if !extended {
        // 老宿主:没有这两个字段,别写(写了就是越界)。
    } else if g
        .sessions
        .get(&id)
        .and_then(|session| session.context())
        .map(|context| context.input.is_empty())
        .unwrap_or(true)
    {
        let predictions = g
            .sessions
            .get(&id)
            .and_then(|session| session.context())
            .map(|context| context.predictions.clone())
            .unwrap_or_default();
        (*ctx).predictions_len = predictions.len() as c_int;
        let mut arr: Vec<*mut c_char> = predictions
            .iter()
            .map(|text| cstr_dup(text))
            .collect();
        arr.push(std::ptr::null_mut());
        (*ctx).predictions = Box::into_raw(arr.into_boxed_slice()) as *mut *mut c_char;
    } else {
        (*ctx).predictions_len = 0;
        (*ctx).predictions = std::ptr::null_mut();
    }
    1
}

unsafe extern "C" fn rime_free_context(ctx: *mut RimeContext) {
    if ctx.is_null() {
        return;
    }
    cstr_free((*ctx).composition.preedit);
    (*ctx).composition.preedit = std::ptr::null_mut();
    cstr_free((*ctx).menu.select_keys);
    (*ctx).menu.select_keys = std::ptr::null_mut();
    let n = (*ctx).menu.num_candidates as usize;
    if !(*ctx).menu.candidates.is_null() && n > 0 {
        let arr = std::slice::from_raw_parts_mut((*ctx).menu.candidates, n);
        for c in arr.iter_mut() {
            cstr_free(c.text);
            cstr_free(c.comment);
        }
        drop(Box::from_raw((*ctx).menu.candidates as *mut RimeCandidate));
        (*ctx).menu.candidates = std::ptr::null_mut();
    }
    cstr_free((*ctx).commit_text_preview);
    (*ctx).commit_text_preview = std::ptr::null_mut();
    // `select_labels` 是 `new char*[page_size]`，**没有** NULL 结尾，长度只能从
    // `menu.page_size` 取（evo `rime_api_impl.h:297-303`）。
    if !(*ctx).select_labels.is_null() {
        let count = (*ctx).menu.page_size.max(0) as usize;
        if count > 0 {
            let arr = std::slice::from_raw_parts_mut((*ctx).select_labels, count);
            for label in arr.iter_mut() {
                cstr_free(*label);
            }
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                (*ctx).select_labels,
                count,
            )));
        }
        (*ctx).select_labels = std::ptr::null_mut();
    }
    // 同上:`predictions*` 是扩展字段 —— 宿主结构不够大时**连读都不能读**（读到的是别人的栈/堆）。
    // 门槛口径同 `rime_get_context`：`data_size` = sizeof(结构) - sizeof(data_size)。
    if (*ctx).data_size as usize
        >= std::mem::size_of::<RimeContext>() - std::mem::size_of::<c_int>()
        && !(*ctx).predictions.is_null()
    {
        let n = (*ctx).predictions_len.max(0) as usize;
        let arr = std::slice::from_raw_parts_mut((*ctx).predictions, n + 1);
        for p in arr.iter_mut() {
            cstr_free(*p);
        }
        drop(Box::from_raw((*ctx).predictions as *mut *mut c_char));
        (*ctx).predictions = std::ptr::null_mut();
    }
}

unsafe extern "C" fn rime_get_status(id: RimeSessionId, status: *mut RimeStatus) -> Bool {
    if status.is_null() {
        return 0;
    }
    let g = global().lock().unwrap();
    let Some(session) = g.sessions.get(&id) else {
        return 0;
    };
    let schema_id = session.schema_id().unwrap_or("").to_string();
    let schema_name = schema_name_for(session).unwrap_or_default();
    (*status).schema_id = cstr_dup(&schema_id);
    (*status).schema_name = cstr_dup(&schema_name);
    (*status).is_disabled = 0;
    (*status).is_composing = session
        .context()
        .map(|c| c.is_composing())
        .unwrap_or(false) as Bool;
    // C++ `RimeGetStatus` 是**现场读**引擎选项的，这里原来把 `is_ascii_mode` 写死成 0。
    // 后果（2026-11-02 定案）：引擎永远以为自己在中文态 —— `KeyEventResponse.ascii_mode`
    // 恒为 `Some(false)` ⇒ `previous_ascii != response.ascii_mode` 永不成立 ⇒
    // 「中英提示」一次都发不出去（`theme.log` 32 条全 `zh`、0 条 `en`），
    // 任务栏「中/英」也不会变。**这才是根因**，不是渲染器那道门。
    (*status).is_ascii_mode = session
        .context()
        .map(|c| c.get_option("ascii_mode"))
        .unwrap_or(false) as Bool;
    (*status).is_full_shape = 0;
    (*status).is_simplified = 1;
    (*status).is_traditional = 0;
    (*status).is_ascii_punct = 0;
    1
}

fn schema_name_for(session: &Session) -> Option<String> {
    session
        .engine
        .as_ref()
        .map(|e| e.schema_name().to_string())
}

unsafe extern "C" fn rime_free_status(status: *mut RimeStatus) {
    if status.is_null() {
        return;
    }
    cstr_free((*status).schema_id);
    cstr_free((*status).schema_name);
}

unsafe extern "C" fn rime_set_option(id: RimeSessionId, option: *const c_char, value: Bool) {
    let Some(name) = read_cstr(option) else { return };
    if let Some(s) = global().lock().unwrap().sessions.get_mut(&id) {
        match s.engine_mut() {
            // 走引擎:开关变化立即重算组件条件(C++ OnOptionUpdate 语义)
            Some(engine) => engine.set_option(&name, value != 0),
            None => {
                if let Some(c) = s.context_mut() {
                    c.set_option(&name, value != 0);
                }
            }
        }
    }
}

unsafe extern "C" fn rime_get_option(id: RimeSessionId, option: *const c_char) -> Bool {
    let Some(name) = read_cstr(option) else { return 0 };
    if let Some(s) = global().lock().unwrap().sessions.get(&id) {
        s.context()
            .map(|c| c.get_option(&name) as Bool)
            .unwrap_or(0)
    } else {
        0
    }
}

unsafe extern "C" fn rime_set_property(id: RimeSessionId, prop: *const c_char, value: *const c_char) {
    let (Some(p), Some(v)) = (read_cstr(prop), read_cstr(value)) else {
        return;
    };
    if let Some(s) = global().lock().unwrap().sessions.get_mut(&id) {
        if let Some(c) = s.context_mut() {
            c.set_property(&p, &v);
        }
    }
}

unsafe extern "C" fn rime_get_property(
    id: RimeSessionId,
    prop: *const c_char,
    buffer: *mut c_char,
    buffer_size: usize,
) -> Bool {
    let Some(p) = read_cstr(prop) else { return 0 };
    if buffer.is_null() || buffer_size == 0 {
        return 0;
    }
    if let Some(s) = global().lock().unwrap().sessions.get(&id) {
        if let Some(v) = s.context().and_then(|c| c.properties.get(&p)).cloned() {
            let bytes = v.as_bytes();
            let n = bytes.len().min(buffer_size - 1);
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer as *mut u8, n);
                *buffer.add(n) = 0;
            }
            return 1;
        }
    }
    0
}

unsafe extern "C" fn rime_get_schema_list(list: *mut RimeSchemaList) -> Bool {
    if list.is_null() {
        return 0;
    }
    let g = global().lock().unwrap();
    let items = collect_schema_list(&g);
    let n = items.len();
    let mut buf: Vec<RimeSchemaListItem> = Vec::with_capacity(n);
    for (id, name) in items {
        buf.push(RimeSchemaListItem {
            schema_id: cstr_dup(&id),
            name: cstr_dup(&name),
            reserved: std::ptr::null_mut(),
        });
    }
    (*list).size = n;
    (*list).list = if n > 0 {
        Box::into_raw(buf.into_boxed_slice()) as *mut RimeSchemaListItem
    } else {
        std::ptr::null_mut()
    };
    1
}

fn collect_schema_list(g: &Global) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for dir in [g.shared_data_dir.clone(), g.user_data_dir.clone()] {
        if dir.is_empty() {
            continue;
        }
        let manifest = crate_schema_manifest_rel();
        for name in [manifest.as_path(), Path::new("default.yaml"), Path::new("schema/default.yaml")] {
            let p = PathBuf::from(&dir).join(name);
            if let Ok(text) = std::fs::read_to_string(&p) {
                parse_schema_list(&text, &mut out);
            }
        }
    }
    // 2026-11-11 修正：不是把 pinyin 写进 disabled.yaml，而是用户取消勾选父方案。
    // 有用户勾选清单时，宿主菜单只暴露仍被勾选的 id（出厂 default.yaml 里的
    // pinyin 不会漏进来），再剔除显式禁用与父未选压住的子方案。
    let selected = selected_schema_ids(g);
    if !selected.is_empty() {
        out.retain(|(id, _)| selected.contains(id));
    }
    let excluded = excluded_schema_ids(g);
    out.retain(|(id, _)| !excluded.contains(id));
    if out.is_empty() {
        out.push(("luna_pinyin".to_string(), "Luna Pinyin".to_string()));
    }
    out
}

fn parse_schema_list(text: &str, out: &mut Vec<(String, String)>) {
    let mut in_list = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("schema_list:") || t.starts_with("schemas:") {
            in_list = true;
            continue;
        }
        if in_list {
            if t.starts_with('-') {
                let body = t.trim_start_matches('-').trim();
                let (id, name) = if let Some(rest) = body.strip_prefix("schema:") {
                    (rest.trim().to_string(), rest.trim().to_string())
                } else if !body.is_empty() && !body.contains(':') {
                    (body.to_string(), body.to_string())
                } else {
                    continue;
                };
                if !out.iter().any(|(i, _)| i == &id) {
                    out.push((id, name));
                }
            } else if !t.is_empty() && !t.starts_with('-') {
                break;
            }
        }
    }
}

unsafe extern "C" fn rime_free_schema_list(list: *mut RimeSchemaList) {
    if list.is_null() {
        return;
    }
    let n = (*list).size;
    if !(*list).list.is_null() && n > 0 {
        let arr = std::slice::from_raw_parts_mut((*list).list, n);
        for it in arr.iter_mut() {
            cstr_free(it.schema_id);
            cstr_free(it.name);
        }
        drop(Box::from_raw((*list).list as *mut RimeSchemaListItem));
        (*list).list = std::ptr::null_mut();
    }
    (*list).size = 0;
}

unsafe extern "C" fn rime_get_current_schema(
    id: RimeSessionId,
    buffer: *mut c_char,
    buffer_size: usize,
) -> Bool {
    if buffer.is_null() || buffer_size == 0 {
        return 0;
    }
    let g = global().lock().unwrap();
    let Some(s) = g.sessions.get(&id) else { return 0 };
    let sid = s.schema_id().unwrap_or("");
    let bytes = sid.as_bytes();
    let n = bytes.len().min(buffer_size - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer as *mut u8, n);
        *buffer.add(n) = 0;
    }
    1
}

unsafe extern "C" fn rime_select_schema(id: RimeSessionId, schema_id: *const c_char) -> Bool {
    let Some(sid) = read_cstr(schema_id) else { return 0 };
    request_session_rebuild(id, &sid)
}

/// 方案切换的**唯一**派发点：C API 选方案与交互式切换共用一条队列。
///
/// 两个调用方：
/// - [`rime_select_schema`]：宿主直接选方案（TIP/Engine 的 `select_schema`）；
/// - [`rime_process_key`] / [`rime_simulate_key_sequence`]：**交互式**切换。
///   `switcher`/`key_binder` 只调 `Engine::request_schema` 登记，`Engine::process_key`
///   在处理器链跑完后调 `Engine::apply_schema`，后者（`async_rebuild == true` 时）
///   只把目标方案留在 `Session::take_pending_rebuild()` 里；按键放锁后由宿主交到这里。
///
/// 共用同一个 `Global.rebuild_generation`（会话 id → 代号 + 目标方案）：
/// 同一会话同一方案已在重建时直接吞掉 ⇒ **两个入口不会并发重建同一方案**；
/// 新目标会让上一个任务的结果按代号作废（后到者赢）。
///
/// 2026-11-10：**异步重建 + 原子换入**。
///
/// 旧实现：在 `global()` 锁里跑 `Session::create_with_roots()`，整个重建
/// （建词典 / prism / 重绑码表 / aux / lua）期间所有客户端按键排在全局锁后面，
/// 用户侧表现就是"打几个拼音突然卡住、然后候选窗才爆出来"。
///
/// 语义（三条，别改坏）：
/// 1) 本函数只登记请求并**立刻返回 1 = 已受理**；重建在常驻后台线程上跑
///    （见 `engine::session_rebuild`），不持有全局锁。
/// 2) 重建期间按键继续进**旧会话**（旧引擎是只读快照，新方案的状态不会混进来），
///    所以不会出现"半新半旧"的组合串。
/// 3) 建好之后在全局锁里原子换入，临界区只有一次 `HashMap::insert`。换入时
///    **旧组合串不迁移**（两套方案码表/分词不同，迁移必然错位），只搬运
///    "已确认但宿主还没取走"的待提交文本；旧引擎在锁外析构（mmap/leveldb 释放慢）。
///
/// 返回值仍是 Bool：1 = 受理（生效可能稍后），0 = 会话不存在/参数非法/后台线程不可用
/// 且同步重建也失败。旧调用方（Engine）只在 0 时告警，语义兼容。
fn request_session_rebuild(id: RimeSessionId, sid: &str) -> Bool {
    let (build_dir, roots, generation) = {
        let mut g = global().lock().unwrap();
        let Some(current) = g.sessions.get(&id) else {
            return 0;
        };
        // 已经在这个方案上（宿主启动时对齐 `active:`、或用户重复点选）：不必重建。
        if current.engine.is_some() && current.schema_id() == Some(sid) {
            return 1;
        }
        // 已经有一个"切到同一个方案"的后台任务在跑：重复请求直接吞掉。
        if let Some((_, pending)) = g.rebuild_generation.get(&id) {
            if pending == sid {
                return 1;
            }
        }
        let build_dir = g.build_dir();
        let roots = g.data_roots();
        let generation = g.next_rebuild_generation;
        g.next_rebuild_generation += 1;
        g.rebuild_generation.insert(id, (generation, sid.to_string()));
        (build_dir, roots, generation)
    }; // ← 锁在这里放掉：从这里到换入之间，按键照常进旧会话
    let sid = sid.to_string();

    let job: crate::engine::session_rebuild::Job = Box::new(move || {
        // 重建失败（含 panic）也要把"重建中"标记摘掉，否则这个会话以后每次切到
        // 同一方案都会被当成"已经有一个在跑"而永远不再重建。
        let rebuilt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Session::create_with_roots(&build_dir, &sid, &roots)
        }));
        let mut fresh = match rebuilt {
            Ok(fresh) => fresh,
            Err(_) => {
                // panic 在哪一步都可能发生，锁可能已经中毒；用 poisoning 容忍的取法。
                let mut g = match global().lock() {
                    Ok(g) => g,
                    Err(err) => err.into_inner(),
                };
                if g.rebuild_generation.get(&id).map(|(g0, _)| *g0) == Some(generation) {
                    g.rebuild_generation.remove(&id);
                }
                eprintln!("[session-rebuild] 方案 `{sid}` 重建 panic，保留旧会话");
                return;
            }
        };
        let mut g = global().lock().unwrap();
        // 期间又被切过一次（代号变了）或会话已销毁：本次结果作废。
        if g.rebuild_generation.get(&id).map(|(g0, _)| *g0) != Some(generation) {
            return;
        }
        g.rebuild_generation.remove(&id);
        // 方案文件缺失/编译产物不全时 engine 是 None。**不能**换掉原会话：
        // 换了之后这个会话对任何按键都返回"没吃掉、没候选"，用户看到的就是
        // "输入法突然哑了"（2026-10-29 的教训：选择必须是原子的"成功才换"）。
        if fresh.engine.is_none() {
            eprintln!("[session-rebuild] 方案 `{sid}` 重建失败，保留旧会话");
            return;
        }
        // 换入后要写 `user.yaml`（"上次用的方案"），路径得在借用 `old` 之前取。
        let user_dir = if g.user_data_dir.is_empty() {
            None
        } else {
            Some(PathBuf::from(g.user_data_dir.clone()))
        };
        let Some(old) = g.sessions.get_mut(&id) else {
            return;
        };
        // 旧组合串丢弃（跨方案不可迁移）；把已确认未取走的提交文本搬过去。
        let discarded = old.context().map(|c| c.input.clone()).unwrap_or_default();
        // 交互式切换的两枚"吞掉宿主重放键"标记要跟着换入：物理按键的重放可能正好
        // 落在换入之后（旧引擎还没来得及消费），丢了就会"换方案时数字/空格上屏"。
        let (swallow_space, swallow_digit) = old
            .engine
            .as_ref()
            .map(|e| (e.swallow_next_space, e.swallow_next_digit))
            .unwrap_or((false, false));
        fresh.transfer_pending_commit(old);
        // 新会话是 `Session::create_with_roots` 的产物，默认同步语义；必须重新打开
        // 异步开关，否则下一次交互式切换会退回"按键里同步重建"。
        fresh.set_async_rebuild(rebuild_async_enabled());
        // 换入的新会话要重新绑一次会话号，否则后续 option 通知会落到 0。
        fresh.bind_signal_session(id);
        if let Some(engine) = fresh.engine.as_mut() {
            engine.swallow_next_space = swallow_space;
            engine.swallow_next_digit = swallow_digit;
        }
        let Some(old) = g.sessions.insert(id, fresh) else {
            return;
        };
        // 锁外析构旧引擎：mmap / leveldb 句柄释放可能要几毫秒，别占着全局锁。
        drop(g);
        // 换入即"当前方案已变更"：写 user.yaml（菜单排序/下次启动靠它），并让宿主
        // 在下一次 `get_current_schema` / `get_context` / `get_status` 看到新方案、
        // 空 preedit —— 那三个入口都直接读换进来的这个会话，不需要额外通知。
        crate::gear::processor::switcher::record_active_schema(user_dir.as_deref(), &sid);
        drop(old);
        // 换入完成 → 由信号总线统一通知宿主（`("schema", "schema_id/<sid>")`）。
        // 宿主要在**按键之外**推一帧给 TIP，否则界面上会一直挂着旧方案的 preedit。
        // 发射在全局锁之外，槽只做投递，不阻塞按键。
        crate::signal::emit_for(
            id,
            crate::signal::Signal::SchemaChanged {
                schema_id: sid.clone(),
            },
        );
        if discarded.is_empty() {
            eprintln!(
                "[session-rebuild] 换入 `{sid}`（新方案已生效，宿主下次读 status/context 即可看到）"
            );
        } else {
            eprintln!(
                "[session-rebuild] 换入 `{sid}` 时丢弃旧组合串 `{discarded}`（跨方案不迁移）"
            );
        }
    });

    // 后台线程起不来时退回同步重建（旧行为），至少不把这次切换丢掉。
    // `RIME_REBUILD_SYNC=1` 也可以强制走同步：线上 A/B 或需要一键回退旧行为时用。
    if !rebuild_async_enabled() {
        job();
    } else if let Err(job) = crate::engine::session_rebuild::dispatch(job) {
        job();
    }
    1
}

unsafe extern "C" fn rime_get_input(id: RimeSessionId) -> *const c_char {
    let g = global().lock().unwrap();
    let Some(s) = g.sessions.get(&id) else {
        return std::ptr::null();
    };
    let input = s.context().map(|c| c.input.clone()).unwrap_or_default();
    // 注意：返回内部字符串的静态副本（调用方用完即弃，MVP 简单 leak 一帧不现实，
    // 这里用 thread-local 缓存避免泄漏）
    INPUT_CACHE.with(|c| {
        let mut slot = c.borrow_mut();
        *slot = CString::new(input).unwrap_or_default();
        slot.as_ptr()
    })
}

thread_local! {
    static INPUT_CACHE: std::cell::RefCell<CString> = std::cell::RefCell::new(CString::default());
}

unsafe extern "C" fn rime_get_caret_pos(id: RimeSessionId) -> usize {
    let g = global().lock().unwrap();
    g.sessions
        .get(&id)
        .and_then(|s| s.context())
        .map(|c| c.caret_pos)
        .unwrap_or(0)
}

unsafe extern "C" fn rime_set_caret_pos(id: RimeSessionId, pos: usize) {
    if let Some(s) = global().lock().unwrap().sessions.get_mut(&id) {
        if let Some(c) = s.context_mut() {
            c.caret_pos = pos.min(c.input.len());
        }
    }
}

unsafe extern "C" fn rime_select_candidate(id: RimeSessionId, index: usize) -> Bool {
    let mut g = global().lock().unwrap();
    if let Some(s) = g.sessions.get_mut(&id) {
        // 走 `Session::choose`：它除了选中候选，还负责把 `context.commit_text` 搬到会话的
        // 提交通道（否则 `rime_get_commit` 永远取不到），并在「只确认前缀」时重排组合。
        return s.choose(index).is_some() as Bool;
    }
    0
}

unsafe extern "C" fn rime_select_candidate_on_current_page(id: RimeSessionId, index: usize) -> Bool {
    // 2026-11-08 修：旧注释说「get_context 始终渲染第 0 页，页内=全局」，
    // 但 2026-10-31 起 get_context 已改为**按高亮所在页**导出候选（修逗号翻页）。
    // 渲染器/主题传来的下标因此是「页内相对」，必须先换算成全局下标，
    // 否则第 2 页点第 1 条会选中第 1 页的第 1 条（C++ 同名 API 同为页内相对语义）。
    let mut g = global().lock().unwrap();
    let page_start = {
        let Some(s) = g.sessions.get(&id) else {
            return 0;
        };
        let Some(c) = s.context() else {
            return 0;
        };
        let page_size = c.page_size.max(1) as usize;
        // 渲染器只可能送页内下标；越界（≥页容量）拒绝，防误选
        if index >= page_size {
            return 0;
        }
        c.highlight_index() / page_size * page_size
    };
    if let Some(s) = g.sessions.get_mut(&id) {
        return s.choose(page_start + index).is_some() as Bool;
    }
    0
}

unsafe extern "C" fn rime_change_page(id: RimeSessionId, backward: Bool) -> Bool {
    let mut g = global().lock().unwrap();
    if let Some(s) = g.sessions.get_mut(&id) {
        if let Some(c) = s.context_mut() {
            let delta = if backward != 0 {
                -(c.page_size as isize)
            } else {
                c.page_size as isize
            };
            c.move_highlight(delta);
            return 1;
        }
    }
    0
}

unsafe extern "C" fn rime_delete_candidate(id: RimeSessionId, index: usize) -> Bool {
    let mut g = global().lock().unwrap();
    if let Some(s) = g.sessions.get_mut(&id) {
        if let Some(c) = s.context_mut() {
            // 原来这里是空桩（恒 0）——宿主调 `RimeDeleteCandidate` 什么都不会发生
            // （2026-11-01 用户报"打错的词删不掉"）。现在转发到 `Context`，
            // 拉黑 + 重算本段；末段只剩一条候选时不给删（删光了就没法输入了）。
            if c.menu_candidate_count() <= 1 {
                return 0;
            }
            return c.delete_candidate(index) as Bool;
        }
    }
    0
}
unsafe extern "C" fn rime_delete_candidate_on_current_page(
    id: RimeSessionId,
    index: usize,
) -> Bool {
    rime_delete_candidate(id, index)
}

unsafe extern "C" fn rime_highlight_candidate(id: RimeSessionId, index: usize) -> Bool {
    let mut g = global().lock().unwrap();
    if let Some(s) = g.sessions.get_mut(&id) {
        if let Some(c) = s.context_mut() {
            let idx = index.min(c.menu_candidate_count().saturating_sub(1));
            c.highlight(idx);
            return 1;
        }
    }
    0
}
unsafe extern "C" fn rime_highlight_candidate_on_current_page(
    id: RimeSessionId,
    index: usize,
) -> Bool {
    rime_highlight_candidate(id, index)
}

unsafe extern "C" fn rime_candidate_list_begin(
    id: RimeSessionId,
    iter: *mut RimeCandidateListIterator,
) -> Bool {
    if iter.is_null() {
        return 0;
    }
    let mut g = global().lock().unwrap();
    let Some(s) = g.sessions.get_mut(&id) else {
        return 0;
    };
    (*iter).index = 0;
    let count = s
        .engine_mut()
        .and_then(|e| e.context.composition.segmentation.segments.first().map(|seg| seg.menu.as_ref().map(|m| m.candidate_count()).unwrap_or(0)))
        .unwrap_or(0);
    if count == 0 {
        return 0;
    }
    let _ = count;
    (*iter).index = 0;
    1
}

unsafe extern "C" fn rime_candidate_list_next(iter: *mut RimeCandidateListIterator) -> Bool {
    if iter.is_null() {
        return 0;
    }
    (*iter).index += 1;
    1
}

unsafe extern "C" fn rime_candidate_list_end(_iter: *mut RimeCandidateListIterator) {}

unsafe extern "C" fn rime_candidate_list_from_index(
    _id: RimeSessionId,
    _iter: *mut RimeCandidateListIterator,
    _index: c_int,
) -> Bool {
    0
}

unsafe extern "C" fn rime_simulate_key_sequence(id: RimeSessionId, sequence: *const c_char) -> Bool {
    let Some(seq) = read_cstr(sequence) else { return 0 };
    let mut ok = true;
    for token in seq.split_whitespace() {
        let Some(key) = KeyEvent::parse(token) else {
            ok = false;
            continue;
        };
        // 每个键处理完就放锁；方案切换请求（若有）在锁外派发 —— 与真实按键同一条路。
        let (eaten, pending) = {
            let mut g = global().lock().unwrap();
            match g.sessions.get_mut(&id) {
                Some(s) => {
                    let eaten = s.process_key(key.keycode, key.modifier);
                    (eaten, s.take_pending_rebuild())
                }
                // 会话不存在：与原行为一致（不改 `ok`，因为没有键被处理）
                None => (true, None),
            }
        };
        if let Some(sid) = pending {
            request_session_rebuild(id, &sid);
        }
        if !eaten {
            ok = false;
        }
    }
    ok as Bool
}

unsafe extern "C" fn rime_get_version() -> *const c_char {
    b"librime-next 0.1.0\0".as_ptr() as *const c_char
}

unsafe extern "C" fn rime_set_input(id: RimeSessionId, input: *const c_char) {
    let Some(text) = read_cstr(input) else { return };
    // 两段分开借用：先改 input，再重算组合（共用同一 session 的两次可变借用不可重叠）
    {
        let mut g = global().lock().unwrap();
        if let Some(sess) = g.sessions.get_mut(&id) {
            if let Some(c) = sess.context_mut() {
                c.set_input(&text);
            }
        }
    }
    {
        let mut g = global().lock().unwrap();
        if let Some(sess) = g.sessions.get_mut(&id) {
            if let Some(e) = sess.engine_mut() {
                e.compose();
            }
        }
    }
}

unsafe extern "C" fn copy_dir(out: *mut c_char, buffer_size: usize, dir: &str) {
    if out.is_null() || buffer_size == 0 {
        return;
    }
    let bytes = dir.as_bytes();
    let n = bytes.len().min(buffer_size - 1);
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), out as *mut u8, n);
    *out.add(n) = 0;
}

unsafe extern "C" fn rime_get_shared_data_dir_s(out: *mut c_char, n: usize) {
    let g = global().lock().unwrap();
    copy_dir(out, n, &g.shared_data_dir);
}
unsafe extern "C" fn rime_get_user_data_dir_s(out: *mut c_char, n: usize) {
    let g = global().lock().unwrap();
    copy_dir(out, n, &g.user_data_dir);
}
unsafe extern "C" fn rime_get_prebuilt_data_dir_s(out: *mut c_char, n: usize) {
    let g = global().lock().unwrap();
    copy_dir(out, n, &g.prebuilt_data_dir);
}
unsafe extern "C" fn rime_get_staging_dir_s(out: *mut c_char, n: usize) {
    let g = global().lock().unwrap();
    copy_dir(out, n, &g.staging_dir);
}
unsafe extern "C" fn rime_get_sync_dir_s(out: *mut c_char, n: usize) {
    copy_dir(out, n, "");
}
unsafe extern "C" fn rime_get_user_id() -> *const c_char {
    b"librime-next\0".as_ptr() as *const c_char
}
unsafe extern "C" fn rime_get_shared_data_dir() -> *const c_char {
    b"\0".as_ptr() as *const c_char
}
unsafe extern "C" fn rime_get_user_data_dir() -> *const c_char {
    b"\0".as_ptr() as *const c_char
}
unsafe extern "C" fn rime_get_sync_dir() -> *const c_char {
    b"\0".as_ptr() as *const c_char
}
unsafe extern "C" fn rime_get_prebuilt_data_dir() -> *const c_char {
    b"\0".as_ptr() as *const c_char
}
unsafe extern "C" fn rime_get_staging_dir() -> *const c_char {
    b"\0".as_ptr() as *const c_char
}
unsafe extern "C" fn rime_get_user_data_sync_dir(_out: *mut c_char, _n: usize) {}

// ----------------------------------------------------------------- 三个根目录的 get/set
//
// 三个路径（程序文件夹 / 共享数据目录 / 用户数据目录）各自有 get + set：
// - `get_*_s(out, n)`：线程安全地把路径拷进调用方缓冲区（标准做法，无泄漏）。
// - `get_*()`：返回线程本地缓冲区的指针，调用方应在下次调用前用完（同 librime 的
//   `get_user_data_dir` 约定：返回内部缓冲、可覆盖、不可 free）。
// - `set_*`：运行期注入/覆盖，同时写进 `Global` 与已存在的 `Deployer`，使后续 `deploy()`
//   经 `layout()` 读到最新路径（2026-10-25：部署强制走用户文件夹，但根可由宿主经 API 设）。

thread_local! {
    static PROGRAM_DIR_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

unsafe extern "C" fn rime_get_program_dir() -> *const c_char {
    let g = global().lock().unwrap();
    PROGRAM_DIR_BUF.with(|b| {
        let mut buf = b.borrow_mut();
        buf.clear();
        buf.extend_from_slice(g.program_dir.as_bytes());
        buf.push(0);
        buf.as_ptr() as *const c_char
    })
}

unsafe extern "C" fn rime_get_program_dir_s(out: *mut c_char, n: usize) {
    let g = global().lock().unwrap();
    copy_dir(out, n, &g.program_dir);
}

unsafe extern "C" fn rime_set_program_dir(dir: *const c_char) {
    if let Some(s) = read_cstr(dir) {
        let mut g = global().lock().unwrap();
        g.program_dir = s.clone();
        if let Some(d) = g.deployer.as_mut() {
            d.set_program_dir(PathBuf::from(s));
        }
    }
}

unsafe extern "C" fn rime_set_shared_data_dir(dir: *const c_char) {
    if let Some(s) = read_cstr(dir) {
        let mut g = global().lock().unwrap();
        g.shared_data_dir = s.clone();
        if let Some(d) = g.deployer.as_mut() {
            d.set_shared_data_dir(PathBuf::from(s));
        }
    }
}

unsafe extern "C" fn rime_set_user_data_dir(dir: *const c_char) {
    if let Some(s) = read_cstr(dir) {
        let user = PathBuf::from(&s);
        // build 只存在于用户文件夹（2026-10-25 硬规则）：用户目录变更 ⇒ staging 重算
        let staging = user.join("build");
        let mut g = global().lock().unwrap();
        g.user_data_dir = s.clone();
        g.staging_dir = staging.to_string_lossy().to_string();
        if let Some(d) = g.deployer.as_mut() {
            d.set_user_data_dir(user);
            d.staging_dir = staging;
        }
    }
}

// 其余函数：安全打桩（MVP 暂不实现 deploy / config 读写 / proto）
unsafe extern "C" fn stub_false() -> Bool {
    0
}
unsafe extern "C" fn stub_true() -> Bool {
    1
}

/// `RimeGetCandidatePreview`（evo `rime_api_impl.h:1051-1065`）。
///
/// 返回 `False` 的条件与 C++ 一致：结构体指针/`data_size` 不合法、会话不存在、
/// 或者预览的 `selected_text` 为空（`fill_candidate_preview` 的判定）。
unsafe extern "C" fn rime_get_candidate_preview(
    id: RimeSessionId,
    preview: *mut RimeCandidatePreview,
) -> Bool {
    if preview.is_null() || (*preview).data_size <= 0 {
        return 0;
    }
    // `RIME_STRUCT_CLEAR`：先把三个输出字段清零，失败时宿主不会读到脏指针。
    (*preview).text_before_selection = std::ptr::null_mut();
    (*preview).selected_text = std::ptr::null_mut();
    (*preview).text_after_selection = std::ptr::null_mut();

    let g = global().lock().unwrap();
    let Some(session) = g.sessions.get(&id) else {
        return 0;
    };
    let Some(context) = session.context() else {
        return 0;
    };
    let value = context
        .composition
        .get_candidate_preview(&context.input);
    if value.selected_text.is_empty() {
        return 0;
    }
    (*preview).text_before_selection = dup_string(&value.text_before_selection);
    (*preview).selected_text = dup_string(&value.selected_text);
    (*preview).text_after_selection = dup_string(&value.text_after_selection);
    1
}

/// `RimeFreeCandidatePreview`（evo `rime_api_impl.h:1067-1075`）。
unsafe extern "C" fn rime_free_candidate_preview(preview: *mut RimeCandidatePreview) -> Bool {
    if preview.is_null() {
        return 0;
    }
    cstr_free((*preview).text_before_selection);
    cstr_free((*preview).selected_text);
    cstr_free((*preview).text_after_selection);
    (*preview).text_before_selection = std::ptr::null_mut();
    (*preview).selected_text = std::ptr::null_mut();
    (*preview).text_after_selection = std::ptr::null_mut();
    1
}

// ----------------------------------------------------------------- RimeApi 单例

fn build_api() -> RimeApi {
    RimeApi {
        data_size: std::mem::size_of::<RimeApi>() as c_int - std::mem::size_of::<c_int>() as c_int,
        setup: Some(rime_setup),
        set_notification_handler: Some(rime_set_notification_handler),
        initialize: Some(rime_initialize),
        finalize: Some(rime_finalize),
        // —— 以下 deploy / config / proto / state-label 函数 MVP 暂不实现：设为 `None` ——
        // rime 约定「可选函数指针为 null 即未实现」，前端调用前会判空；它们都不在
        // 「按键 → 候选 → 提交」的主路径上。后续要接部署/配置读写再补 `unsafe extern "C" fn`。
        start_maintenance: Some(rime_start_maintenance),
        is_maintenance_mode: Some(rime_is_maintenance_mode),
        join_maintenance_thread: Some(rime_join_maintenance_thread),
        deployer_initialize: Some(rime_deployer_initialize),
        prebuild: Some(rime_prebuild),
        deploy: Some(rime_deploy),
        deploy_schema: Some(rime_deploy_schema),
        deploy_config_file: Some(rime_deploy_config_file),
        sync_user_data: Some(rime_sync_user_data),
        create_session: Some(rime_create_session),
        find_session: Some(rime_find_session),
        destroy_session: Some(rime_destroy_session),
        cleanup_stale_sessions: Some(rime_cleanup_stale_sessions),
        cleanup_all_sessions: Some(rime_cleanup_all_sessions),
        process_key: Some(rime_process_key),
        commit_composition: Some(rime_commit_composition),
        commit_string: Some(rime_commit_string),
        clear_composition: Some(rime_clear_composition),
        get_commit: Some(rime_get_commit),
        free_commit: Some(rime_free_commit),
        get_context: Some(rime_get_context),
        free_context: Some(rime_free_context),
        get_status: Some(rime_get_status),
        free_status: Some(rime_free_status),
        set_option: Some(rime_set_option),
        get_option: Some(rime_get_option),
        set_property: Some(rime_set_property),
        get_property: Some(rime_get_property),
        get_schema_list: Some(rime_get_schema_list),
        free_schema_list: Some(rime_free_schema_list),
        get_current_schema: Some(rime_get_current_schema),
        select_schema: Some(rime_select_schema),
        schema_open: None,
        config_open: None,
        config_close: None,
        config_get_bool: None,
        config_get_int: None,
        config_get_double: None,
        config_get_string: None,
        config_get_cstring: None,
        config_update_signature: None,
        config_begin_map: None,
        config_next: None,
        config_end: None,
        simulate_key_sequence: Some(rime_simulate_key_sequence),
        register_module: None,
        find_module: None,
        run_task: None,
        get_shared_data_dir: Some(rime_get_shared_data_dir),
        get_user_data_dir: Some(rime_get_user_data_dir),
        get_sync_dir: Some(rime_get_sync_dir),
        get_user_id: Some(rime_get_user_id),
        get_user_data_sync_dir: Some(rime_get_user_data_sync_dir),
        config_init: None,
        config_load_string: None,
        config_set_bool: None,
        config_set_int: None,
        config_set_double: None,
        config_set_string: None,
        config_get_item: None,
        config_set_item: None,
        config_clear: None,
        config_create_list: None,
        config_create_map: None,
        config_list_size: None,
        config_begin_list: None,
        get_input: Some(rime_get_input),
        get_caret_pos: Some(rime_get_caret_pos),
        select_candidate: Some(rime_select_candidate),
        get_version: Some(rime_get_version),
        set_caret_pos: Some(rime_set_caret_pos),
        select_candidate_on_current_page: Some(rime_select_candidate_on_current_page),
        candidate_list_begin: Some(rime_candidate_list_begin),
        candidate_list_next: Some(rime_candidate_list_next),
        candidate_list_end: Some(rime_candidate_list_end),
        user_config_open: None,
        candidate_list_from_index: Some(rime_candidate_list_from_index),
        get_prebuilt_data_dir: Some(rime_get_prebuilt_data_dir),
        get_staging_dir: Some(rime_get_staging_dir),
        commit_proto: None,
        context_proto: None,
        status_proto: None,
        get_state_label: None,
        delete_candidate: Some(rime_delete_candidate),
        delete_candidate_on_current_page: Some(rime_delete_candidate_on_current_page),
        get_state_label_abbreviated: None,
        set_input: Some(rime_set_input),
        get_shared_data_dir_s: Some(rime_get_shared_data_dir_s),
        get_user_data_dir_s: Some(rime_get_user_data_dir_s),
        get_prebuilt_data_dir_s: Some(rime_get_prebuilt_data_dir_s),
        get_staging_dir_s: Some(rime_get_staging_dir_s),
        get_sync_dir_s: Some(rime_get_sync_dir_s),
        highlight_candidate: Some(rime_highlight_candidate),
        highlight_candidate_on_current_page: Some(rime_highlight_candidate_on_current_page),
        change_page: Some(rime_change_page),
        get_program_dir: Some(rime_get_program_dir),
        get_program_dir_s: Some(rime_get_program_dir_s),
        set_program_dir: Some(rime_set_program_dir),
        set_shared_data_dir: Some(rime_set_shared_data_dir),
        set_user_data_dir: Some(rime_set_user_data_dir),
        get_candidate_preview: Some(rime_get_candidate_preview),
        free_candidate_preview: Some(rime_free_candidate_preview),
    }
}

/// 对外 ABI 单例。`RimeApi` 只含函数指针与裸指针，故 `Sync`；用 `OnceLock` 取代
/// `static mut`（edition 2024 已禁止对 `static mut` 取引用）。
static RIME_API: OnceLock<RimeApi> = OnceLock::new();

/// 对外入口：`RimeApi* rime_get_api()`。
#[unsafe(no_mangle)]
pub extern "C" fn rime_get_api() -> *const RimeApi {
    RIME_API.get_or_init(build_api) as *const RimeApi
}

// ----------------------------------------------------------------- 宿主路径冒烟测试
//
// 这条测试刻意走和宿主（`Engine/src/librime/mod.rs` 的 `Librime::load`）**完全一致**的
// 调用序列：`rime_initialize`（只传 shared/user，staging_dir = null）→ `rime_create_session`
// → 逐个 `rime_process_key` → `rime_get_context` 读候选。
//
// 之前的 `repro_real_luna_pinyin` 直接 `Session::create_with_roots` 绕过了 C-API 全局态，
// 因此没暴露 `build_dir()` 在 staging_dir 为空时回退到 `shared_data_dir/build`（程序目录，
// 那里没有 .bin）的 bug。这条测试能直接复现宿主线上行为。

#[cfg(test)]
mod host_smoke {
    use super::*;
    use std::ffi::{CStr, CString};

    /// 测试沙箱的自动清理守卫。
    ///
    /// 这个用例要拷一份完整的 `build/`（200MB+），之前只在**开头**清一次旧目录，
    /// 每次跑完都留一份在 `%TEMP%`（PID 每次不同 ⇒ 越积越多，实测攒到 7GB、
    /// 把系统盘写满）。改成 Drop 收尾，提前 `return` 的失败分支也能清掉。
    struct Sandbox(std::path::PathBuf);

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 新会话的初始方案：`active:` 优先，其次清单第一条。
    ///
    /// 真机后果（2026-10-29）：只看清单第一条时，`schema.yaml` 排第一的 `cangjie.5`
    /// 会成为每个新会话的起始方案 —— 表现是"刚开一个输入框，打拼音出来的是仓颉码"。
    #[test]
    fn active_schema_wins_for_initial_session() {
        let manifest = "# 方案信息\nactive: pinyin.luna\nschemas:\n  - cangjie.5\n  - pinyin.luna\n";
        assert_eq!(extract_first_schema_id(manifest).as_deref(), Some("pinyin.luna"));

        // 没有 active：退到清单第一条（首次运行）
        let no_active = "schemas:\n  - cangjie.5\n  - stroke\n";
        assert_eq!(extract_first_schema_id(no_active).as_deref(), Some("cangjie.5"));

        // default.yaml 的写法（`- schema: xxx`）
        let deployed = "schema_list:\n  - schema: zhuyin\n  - schema: stroke\n";
        assert_eq!(extract_first_schema_id(deployed).as_deref(), Some("zhuyin"));

        // 注释里的 active 不算数
        let commented = "# active: stroke\nschemas:\n  - pinyin.terra\n";
        assert_eq!(extract_first_schema_id(commented).as_deref(), Some("pinyin.terra"));

        // 2026-11-11：active 被组禁用时，候选序列要能继续往下退。
        let disabled = "active: pinyin.zrm\nschemas:\n  - pinyin\n  - wubi86\n  - liangfen\n";
        assert_eq!(
            extract_schema_id_candidates(disabled),
            vec![
                "pinyin.zrm".to_string(),
                "pinyin".to_string(),
                "wubi86".to_string(),
                "liangfen".to_string()
            ],
            "输入 active=pinyin.zrm + 4 项清单应产出 4 个候选（去重），供禁用过滤逐个跳过"
        );
    }

    #[test]
    fn disabled_group_skips_active_without_rewriting_manifest() {
        let sandbox = Sandbox(
            std::env::temp_dir().join(format!("rime-disabled-capi-{}", std::process::id())),
        );
        let _ = std::fs::remove_dir_all(&sandbox.0);
        let shared = sandbox.0.join("shared");
        let user = sandbox.0.join("user");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::create_dir_all(user.join("schema")).unwrap();
        let manifest = "# 方案信息\nactive: pinyin.zrm\nschemas:\n  - pinyin\n  - pinyin.zrm\n  - wubi86\n  - liangfen\n";
        let manifest_path = user.join("schema").join(crate::lever::paths::SCHEMA_MANIFEST);
        std::fs::write(&manifest_path, manifest).unwrap();
        // 让 `groups: [pinyin]` 能按 parent: 展开到 active 的 pinyin.zrm
        let child_dir = user.join("schema").join("pinyin").join("zrm");
        std::fs::create_dir_all(&child_dir).unwrap();
        std::fs::write(
            child_dir.join("pinyin.zrm.schema.yaml"),
            "schema:\n  schema_id: pinyin.zrm\n  name: 自然码\n  parent: pinyin\n",
        )
        .unwrap();
        std::fs::write(
            user.join("schema").join(crate::lever::paths::DISABLED_SCHEMAS_FILE),
            "groups:\n  - pinyin\nschemas: []\n",
        )
        .unwrap();

        let g = Global {
            shared_data_dir: shared.to_string_lossy().into_owned(),
            user_data_dir: user.to_string_lossy().into_owned(),
            program_dir: String::new(),
            prebuilt_data_dir: String::new(),
            staging_dir: String::new(),
            sessions: HashMap::new(),
            next_id: 1,
            rebuild_generation: HashMap::new(),
            next_rebuild_generation: 1,
            deployer: None,
            maintenance: None,
        };
        assert_eq!(
            default_schema_id(&g),
            "wubi86",
            "active=pinyin.zrm 落在禁用组内：运行时只退到清单里第一个未禁用方案"
        );
        assert_eq!(
            std::fs::read_to_string(&manifest_path).unwrap(),
            manifest,
            "禁用绝不能改 schema.yaml：active 与 4 项 schemas 必须逐字节原样"
        );
        let items = collect_schema_list(&g);
        let ids: Vec<&str> = items.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["wubi86", "liangfen"],
            "宿主方案列表同样不能出现被禁用的 pinyin / pinyin.zrm"
        );
    }

    #[test]
    fn unchecked_parent_blocks_children_but_can_be_rechecked() {
        let sandbox = Sandbox(
            std::env::temp_dir().join(format!("rime-unchecked-capi-{}", std::process::id())),
        );
        let _ = std::fs::remove_dir_all(&sandbox.0);
        let shared = sandbox.0.join("shared");
        let user = sandbox.0.join("user");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::create_dir_all(user.join("schema/pinyin/luna")).unwrap();
        std::fs::create_dir_all(user.join("schema/pinyin/zrm")).unwrap();
        // 出厂 default.yaml 仍列着 pinyin 与全部子方案，不能让它漏进宿主菜单。
        std::fs::write(
            shared.join("default.yaml"),
            "schema_list:\n  - schema: pinyin\n  - schema: pinyin.luna\n  - schema: pinyin.zrm\n  - schema: cangjie.5\n  - schema: stroke\n",
        )
        .unwrap();
        for (leaf, name) in [("luna", "明月"), ("zrm", "自然码")] {
            std::fs::write(
                user
                    .join("schema/pinyin")
                    .join(leaf)
                    .join(format!("pinyin.{leaf}.schema.yaml")),
                format!("schema:\n  schema_id: pinyin.{leaf}\n  name: {name}\n  parent: pinyin\n"),
            )
            .unwrap();
        }
        let manifest_path = user.join("schema").join(crate::lever::paths::SCHEMA_MANIFEST);
        let disabled_path = user.join("schema").join(crate::lever::paths::DISABLED_SCHEMAS_FILE);
        let unchecked =
            "active: pinyin\nschemas:\n  - pinyin.luna\n  - pinyin.zrm\n  - cangjie.5\n  - stroke\n";
        std::fs::write(&manifest_path, unchecked).unwrap();
        std::fs::write(&disabled_path, "groups: []\nschemas: []\n").unwrap();

        let g = Global {
            shared_data_dir: shared.to_string_lossy().into_owned(),
            user_data_dir: user.to_string_lossy().into_owned(),
            program_dir: String::new(),
            prebuilt_data_dir: String::new(),
            staging_dir: String::new(),
            sessions: HashMap::new(),
            next_id: 1,
            rebuild_generation: HashMap::new(),
            next_rebuild_generation: 1,
            deployer: None,
            maintenance: None,
        };
        assert_eq!(
            default_schema_id(&g),
            "cangjie.5",
            "active=pinyin 只是没被勾选：内存回退到第一个可部署方案，不改源文件"
        );
        assert_eq!(
            std::fs::read_to_string(&manifest_path).unwrap(),
            unchecked,
            "父未选绝不能改 schema.yaml 的 active / schemas"
        );
        let items = collect_schema_list(&g);
        let ids: Vec<&str> = items.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["cangjie.5", "stroke"],
            "宿主菜单不能出现未勾选的 pinyin，也不能出现被父未选压住的子方案"
        );
        assert_eq!(
            std::fs::read_to_string(&disabled_path).unwrap(),
            "groups: []\nschemas: []\n",
            "父未选不能写进 disabled.yaml，否则父方案自己也回不来了"
        );

        let rechecked =
            "active: pinyin\nschemas:\n  - pinyin\n  - pinyin.luna\n  - pinyin.zrm\n  - cangjie.5\n  - stroke\n";
        std::fs::write(&manifest_path, rechecked).unwrap();
        assert_eq!(
            default_schema_id(&g),
            "pinyin",
            "重新勾选 pinyin 后 active 立即恢复成当前方案"
        );
        let items_again = collect_schema_list(&g);
        let ids_again: Vec<&str> = items_again.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            ids_again,
            vec!["pinyin", "pinyin.luna", "pinyin.zrm", "cangjie.5", "stroke"],
            "重新勾选父方案后子方案全部恢复，不需要删 build"
        );
        assert_eq!(
            std::fs::read_to_string(&disabled_path).unwrap(),
            "groups: []\nschemas: []\n"
        );
    }

    /// 宿主机上有没有按新键（`translator/dict`）部署的产物。
    ///
    /// 2026-10-28 20:5x 起词库键只认 `dict`、去掉 `dictionary` 兼容层，所以
    /// **旧部署产物**（`build/**/*.schema.yaml` 里还是 `dictionary:`）读不出词库。
    /// 这种情况下跳过冒烟，免得把"没重新部署"误报成代码回归。
    fn host_has_dict_keyed_build(user_dir: &str) -> bool {
        let build = std::path::Path::new(user_dir).join("build");
        let mut stack = vec![build];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".schema.yaml"))
                {
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        if text.contains("\n  dict:") {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    fn copy_dir_contents(src: &std::path::Path, dst: &std::path::Path) {
        std::fs::create_dir_all(dst).unwrap();
        for entry in std::fs::read_dir(src).unwrap().flatten() {
            let to = dst.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_dir_contents(&entry.path(), &to);
            } else {
                let _ = std::fs::copy(entry.path(), to);
            }
        }
    }

    #[test]
    fn host_path_typing_nihao_yields_candidates() {
        // 沙箱隔离:真机 UserDb 里学过的词(如「你和傲」)会优先顶到候选第一位,
        // 污染本测试的「你好」断言。用临时目录装 build 产物(不含 userdb),
        // 测的是同一条部署代码路径,但不带用户学习数据。
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        let real_user = format!("{appdata}/Rime-Next");
        if !host_has_dict_keyed_build(&real_user) {
            eprintln!(
                "跳过：%APPDATA%\\Rime-Next\\build 里没有 `dict:` 的新产物（重新部署后再测；\
                 旧产物的 dictionary: 已不兼容）"
            );
            return;
        }
        // 沙箱:拷 build\ 和 schema\ 到临时目录,不带 userdb\ 和 installation.yaml
        let sandbox = std::env::temp_dir().join(format!("host-smoke-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sandbox);
        let _sandbox_guard = Sandbox(sandbox.clone());
        std::fs::create_dir_all(sandbox.join("build")).unwrap();
        // 拷 build 产物
        let src_build = format!("{real_user}/build");
        copy_dir_contents(std::path::Path::new(&src_build), &sandbox.join("build"));
        // 拷 schema 源(reload 用)
        let src_schema = format!("{real_user}/schema");
        copy_dir_contents(std::path::Path::new(&src_schema), &sandbox.join("schema"));
        let shared = CString::new("C:/Program Files/Rime-Next/Weasel-Next/data").unwrap();
        let user = CString::new(sandbox.to_string_lossy().as_bytes()).unwrap();
        let app = CString::new("rime.weasel-next").unwrap();

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
            rime_initialize(&mut traits);
            let id = rime_create_session();
            assert!(id != 0, "宿主路径下应创建出会话");
            for ch in "nihao".chars() {
                let consumed = rime_process_key(id, ch as c_int, 0);
                assert!(consumed != 0, "按键 {ch} 应被消费");
            }
            let mut ctx: RimeContext = std::mem::zeroed();
            // 宿主约定：`data_size` = sizeof(结构) - sizeof(data_size)（= `RIME_STRUCT_INIT`）。
            // 保持 0 等于自称「老宿主」，本工程扩展字段（预测 / total_candidates）一个都不会写。
            ctx.data_size =
                (std::mem::size_of::<RimeContext>() - std::mem::size_of::<c_int>()) as c_int;
            let ok = rime_get_context(id, &mut ctx);
            assert!(ok != 0, "应能取得上下文");
            let n = ctx.menu.num_candidates as usize;
            // 「1 / N」页码的数据源：懒式菜单必须排空流才有总数（`num_candidates` 只是本页条数）。
            let total = ctx.total_candidates;
            assert!(
                total > 0 && total as usize >= n,
                "宿主声明 data_size 后应拿到总候选数（total={total}, page={n}）"
            );
            let mut texts = Vec::new();
            if n > 0 && !ctx.menu.candidates.is_null() {
                let arr = std::slice::from_raw_parts(ctx.menu.candidates, n);
                for c in arr {
                    if !c.text.is_null() {
                        texts.push(CStr::from_ptr(c.text).to_string_lossy().into_owned());
                    }
                }
            }
            rime_free_context(&mut ctx);
            eprintln!("HOST-PATH candidates (n={n}, total={total}): {texts:?}");
            assert!(
                !texts.is_empty(),
                "宿主路径下应产出候选；若为空，多半是 build_dir 解析到了程序目录（那里没有 .bin）"
            );
            assert!(
                texts.iter().any(|t| t.contains("你好")),
                "nihao 应能在候选里拿到「你好」，实得 {texts:?}"
            );
        }
    }
}
