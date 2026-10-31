//! `src/rime_api.h` + `rime_api.cc`：对外 ABI 边界。
//!
//! 这是 `librime-next` 作为 **rime.dll 替代品** 的对外契约。结构体（`RimeApi` / `RimeTraits` /
//! `RimeContext` / …）的字段顺序与 `src/rime/rime_api.h` **逐字对齐**（含 `data_size` 偏移约定），
//! 这样现有前端（如 Weasel）无需改头即可链接。
//!
//! 本文件只做「C 边界 ↔ 内部 `Session`」的翻译；真正的组字逻辑在 `engine` 模块。

#![allow(non_snake_case, dead_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::os::raw::c_double;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

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
    /// 部署器（C++ `Service::instance().deployer()`）。`deployer_initialize` 建它，
    /// `deploy` / `prebuild` / `deploy_schema` … 用它。
    deployer: Option<Deployer>,
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
    /// `simplifier`（`opencc/`）与 `script_translator`（`model/*.gram`）靠它找外部数据 ——
    /// 两者不一定在同一个根下：出厂布局里 `model/` 跟用户目录走、`opencc/` 在共享目录。
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
    pub free_candidate_preview: Option<unsafe extern "C" fn(*mut RimeCandidatePreview)>,
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
    global().lock().unwrap().sessions.clear();
}

unsafe extern "C" fn rime_set_notification_handler(
    _handler: RimeNotificationHandler,
    _ctx: *mut c_void,
) {
    // MVP：通知回调暂不转发
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
    match g.deployer.as_mut() {
        Some(d) => d.start_maintenance(full_check != 0) as Bool,
        None => 0,
    }
}

/// `RimeIsMaintenanceMode()`。
unsafe extern "C" fn rime_is_maintenance_mode() -> Bool {
    let g = global().lock().unwrap();
    match g.deployer.as_ref() {
        Some(d) => d.is_maintenance_mode() as Bool,
        None => 0,
    }
}

/// `RimeJoinMaintenanceThread()` —— 我们的部署是同步执行的，没有工作线程可 join。
unsafe extern "C" fn rime_join_maintenance_thread() {}

unsafe extern "C" fn rime_create_session() -> RimeSessionId {
    let mut g = global().lock().unwrap();
    let id = g.next_id;
    g.next_id += 1;
    let build_dir = g.build_dir();
    let roots = g.data_roots();
    let default_schema = default_schema_id(&g);
    let session = Session::create_with_roots(&build_dir, &default_schema, &roots);
    g.sessions.insert(id, session);
    id
}

/// `<user>/schema/schema.yaml` 的相对路径（点名制方案清单，取代已废弃的 `schema.toml`）。
fn crate_schema_manifest_rel() -> std::path::PathBuf {
    std::path::PathBuf::from(crate::lever::paths::SCHEMA_DIR)
        .join(crate::lever::paths::SCHEMA_MANIFEST)
}

fn default_schema_id(g: &Global) -> String {
    // 取 schema/schema.yaml（点名制）/ default.yaml 里第一个方案；退化到 "luna_pinyin"
    let candidates = [
        g.shared_data_dir.clone(),
        g.user_data_dir.clone(),
    ];
    for dir in candidates.iter().filter(|d| !d.is_empty()) {
        let manifest = crate_schema_manifest_rel();
        for name in [manifest.as_path(), Path::new("default.yaml"), Path::new("schema/default.yaml")] {
            let p = PathBuf::from(dir).join(name);
            if let Ok(text) = std::fs::read_to_string(&p) {
                if let Some(id) = extract_first_schema_id(&text) {
                    return id;
                }
            }
        }
    }
    "luna_pinyin".to_string()
}

/// 从 `schema/schema.yaml` 的 `active:` / `schemas:`，或 `default.yaml` 的 `schema_list` 取方案 id。
///
/// 顺序：**`active:` 优先**（那是"当前方案"，新会话一建出来就该是它），
/// 没有 `active` 才退到清单第一条（首次运行、还没定过当前方案时）。
///
/// ⚠️ 2026-10-29：原来只看清单第一条，而 `schema.yaml` 的清单是按 id 排序的
/// （第一条是 `cangjie.5`），于是每个新会话先落在**仓颉**上；靠 Engine 侧再
/// `select_schema(active)` 纠正 —— 而那条路当时又是坏的（见 `Engine/src/librime/api.rs`
/// 漏装 `select_schema`），结果"设置里选的方案不生效"。两道修一起才对。
fn extract_first_schema_id(text: &str) -> Option<String> {
    // ① `active: <id>`（schema.yaml 的当前方案）
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("active:") {
            let id = rest.trim().trim_matches(['"', '\''].as_ref()).trim();
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    // ② 清单第一条：`schema_list:` 后的 `- schema: xxx` 或 `schemas:` 后的 `- xxx`
    let mut lines = text.lines().peekable();
    let mut in_list = false;
    for line in lines.by_ref() {
        let trimmed = line.trim();
        if trimmed.starts_with("schema_list:") || trimmed.starts_with("schemas:") {
            in_list = true;
            continue;
        }
        if in_list {
            if trimmed.starts_with('-') {
                // `- schema: xxx` 或 `- xxx`
                let body = trimmed.trim_start_matches('-').trim();
                if let Some(rest) = body.strip_prefix("schema:") {
                    return Some(rest.trim().to_string());
                }
                if !body.is_empty() && !body.contains(':') {
                    return Some(body.to_string());
                }
            } else if !trimmed.starts_with('-') && !trimmed.is_empty() {
                break;
            }
        }
    }
    None
}

unsafe extern "C" fn rime_find_session(id: RimeSessionId) -> Bool {
    global().lock().unwrap().sessions.contains_key(&id) as Bool
}

unsafe extern "C" fn rime_destroy_session(id: RimeSessionId) -> Bool {
    global().lock().unwrap().sessions.remove(&id).is_some() as Bool
}

unsafe extern "C" fn rime_cleanup_stale_sessions() {}
unsafe extern "C" fn rime_cleanup_all_sessions() {
    global().lock().unwrap().sessions.clear();
}

unsafe extern "C" fn rime_process_key(id: RimeSessionId, keycode: c_int, mask: c_int) -> Bool {
    let mut g = global().lock().unwrap();
    if let Some(s) = g.sessions.get_mut(&id) {
        s.process_key(keycode as u32, mask as u32) as Bool
    } else {
        0
    }
}

unsafe extern "C" fn rime_commit_composition(id: RimeSessionId) -> Bool {
    let mut g = global().lock().unwrap();
    if let Some(s) = g.sessions.get_mut(&id) {
        s.commit_composition() as Bool
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
    let g = global().lock().unwrap();
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
    (*ctx).menu.select_keys = cstr_dup("123456789");
    (*ctx).commit_text_preview = std::ptr::null_mut();
    (*ctx).select_labels = std::ptr::null_mut();
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
    (*status).is_ascii_mode = 0;
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
    let mut g = global().lock().unwrap();
    let build_dir = g.build_dir();
    let roots = g.data_roots();
    if let Some(s) = g.sessions.get_mut(&id) {
        let fresh = Session::create_with_roots(&build_dir, &sid, &roots);
        // 方案文件缺失/编译产物不全时 `Session` 的 engine 是 None。这里**不能**直接换掉
        // 原会话：换了之后这个会话对任何按键都返回"没吃掉、没候选"，用户看到的就是
        // "输入法突然哑了"（2026-10-29 的教训：选择必须是原子的"成功才换"）。
        if fresh.engine.is_none() {
            return 0;
        }
        *s = fresh;
        1
    } else {
        0
    }
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
    // 本实现里 `get_context` 始终渲染第 0 页（`Menu::page(page_size, 0)`），
    // 因此「当前页内下标」与「全局下标」一致，直接转发。
    rime_select_candidate(id, index)
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

unsafe extern "C" fn rime_delete_candidate(_id: RimeSessionId, _index: usize) -> Bool {
    0
}
unsafe extern "C" fn rime_delete_candidate_on_current_page(_id: RimeSessionId, _index: usize) -> Bool {
    0
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
        match KeyEvent::parse(token) {
            Some(key) => {
                let mut g = global().lock().unwrap();
                if let Some(s) = g.sessions.get_mut(&id) {
                    if !s.process_key(key.keycode, key.modifier) {
                        ok = false;
                    }
                }
            }
            None => ok = false,
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
        get_candidate_preview: None,
        free_candidate_preview: None,
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
        std::fs::create_dir_all(sandbox.join("build")).unwrap();
        // 拷 build 产物
        let src_build = format!("{real_user}/build");
        copy_dir_contents(std::path::Path::new(&src_build), &sandbox.join("build"));
        // 拷 schema 源(reload 用)
        let src_schema = format!("{real_user}/schema");
        copy_dir_contents(std::path::Path::new(&src_schema), &sandbox.join("schema"));
        let shared = CString::new("C:/Program Files/Rime-Next/Weasel-Next-0.18.4/data").unwrap();
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
            let ok = rime_get_context(id, &mut ctx);
            assert!(ok != 0, "应能取得上下文");
            let n = ctx.menu.num_candidates as usize;
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
            eprintln!("HOST-PATH candidates (n={n}): {texts:?}");
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
