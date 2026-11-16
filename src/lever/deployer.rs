//! `src/rime/deployer.{h,cc}`（79 + 154 行）的移植 —— **任务调度器**。
//!
//! ## C++ 里它怎么工作
//!
//! ```text
//! DeploymentTask::Require(name)   ← Registry 里的组件（levers_module.cc 注册的 11 个名字）
//! RunTask(name, arg)              ← 立刻创建 + 立刻 Run（同步）
//! ScheduleTask(name, arg)         ← 塞进 pending_tasks_ 队列，等 StartWork/Run 消费
//! Run()                           ← do { 消费队列 } while (队列又被塞进新任务)
//!                                  统计 "N tasks ran: X success, Y failure"，
//!                                  并通过 Messenger::message_sink_ 发
//!                                  ("deploy","start") / ("deploy","success"/"failure")
//! StartWork(maintenance)          ← 队列为空返回 false；否则起工作线程跑 Run()（RIME_NO_THREADING 时同步跑）
//! ```
//!
//! ## Rust 侧的对齐与差异
//!
//! * **任务不是组件工厂，是枚举**：[`DeploymentTask`] 的每个变体对应 C++ 的一个类，
//!   参数（`TaskInitializer` 那个 `std::any`）变成变体字段。`RunTask(name)` 那种
//!   "按名字创建 + 默认参数"由 [`DeploymentTask::from_name`] 承担（名字表 = `levers_module.cc`）。
//! * **没有工作线程**：`StartWork` 在 C++ 里 `std::async` 起线程，Rust 里
//!   [`Deployer::start_work`] **同步**跑完再返回（返回值语义不变：队列空 → false）。
//!   要异步由调用方 `std::thread::spawn`；这样 `Deployer` 不必是 `Send + Sync`，
//!   测试也不用等线程。
//! * **异常处理**：C++ 在 `Run()` 里 `catch (std::exception)` 记一次 failure。Rust 对应
//!   [`std::panic::catch_unwind`]，失败即 +1 failure，循环继续。
//! * `IsWorking` / `JoinWorkThread` 没有意义了（同步执行），保留 [`Deployer::is_working`]
//!   说明"永远 false"。
//! * `Messenger` 的通知回调换成 [`crate::signal`] 总线的 `deploy` 信号（2026-11-14\n//!   「librime 全部信号化」）；[`Deployer::messages`] 仍保留同一份事件记录供测试/对照读。
//!
//! ## 三条入口序列（`rime_api_impl.h`，**顺序就是契约**）
//!
//! | C API | 函数 | 序列 |
//! |---|---|---|
//! | `deploy` | [`Deployer::deploy_workspace`] | installation_update → workspace_update → user_dict_upgrade → cleanup_trash（**短路**：任一步 false 就停） |
//! | `start_maintenance(full)` | [`Deployer::start_maintenance`] | clean_old_log_files → installation_update → [detect_modifications] → 排队 workspace_update / user_dict_upgrade / cleanup_trash → Run |
//! | `sync_user_data` | [`Deployer::sync_user_data`] | 排队 installation_update / backup_config_files / user_dict_sync → Run |
//! | `prebuild_all_schemas` | [`Deployer::prebuild_all_schemas`] | prebuild_all_schemas |
//! | `deploy_schema(path)` | [`Deployer::deploy_schema`] | schema_update(path) |
//! | `deploy_config_file(f,k)` | [`Deployer::deploy_config_file`] | config_file_update(f,k) |
//!
//! Weasel-Next 的 `Weasel-Next-Engine.exe --deploy` 走的是 **`deploy`**（= `deploy_workspace`），
//! 不是 `start_maintenance`（见 `Engine/src/librime/mod.rs` 的 `Librime::deploy`：
//! `deployer_initialize` + `deploy`）。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use super::deployment_tasks::DeploymentTask;
use super::tree::{log_info, log_warn};
use super::user_dict_manager::UserDbFactory;

/// `Deployer`：一次部署的**全部可变状态**（目录、身份、待办队列、可注入的跨层服务）。
///
/// 字段逐个对应 `deployer.h:32-46`。默认值与 C++ 构造函数一致
/// （`shared_data_dir="."`、`user_data_dir="."`、`prebuilt/staging="build"`、
/// `sync_dir="sync"`、`user_id="unknown"`、`backup_config_files=true`）。
/// 排队中的部署任务:任务本体 + 到达时刻(HRRN 的等待时间起点)。
#[derive(Clone)]
pub struct ScheduledTask {
    pub task: DeploymentTask,
    pub arrival: std::time::Instant,
}

#[derive(Clone)]
pub struct Deployer {
    /// 程序文件夹（`PROGRAM_DIR`，安装目录）：宿主经 API 注入，是共享数据目录的父目录。
    /// 只含共享数据目录（`shared_data_dir`），禁止任何设置文件、禁止任何 `build`。
    pub program_dir: PathBuf,
    /// 共享数据目录（`SHARE_DIR`，即 Weasel 设的 `Rime-Next` 数据目录）。宿主经 API 注入。
    pub shared_data_dir: PathBuf,
    /// 用户文件夹（`USER_DIR`，如 `%APPDATA%\Rime-Next`）。宿主经 API 注入。
    pub user_data_dir: PathBuf,
    pub prebuilt_data_dir: PathBuf,
    pub staging_dir: PathBuf,
    pub sync_dir: PathBuf,
    /// 部署台账 `build/deploy_records.yaml` 的输出根覆盖（测试专用）。
    /// `None` = 跟随 `user_data_dir`（生产行为零变化）；`Some(root)` = 写
    /// `root/build/deploy_records.yaml`，用于把离线对照测试的假记录与真实
    /// `%APPDATA%/Rime-Next` 台账隔离。
    records_root_override: Option<PathBuf>,
    pub user_id: String,
    /// 日志目录（C++ `RimeTraits::log_dir`）。空 = 日志走 stderr，
    /// 此时 `clean_old_log_files` 直接成功返回（与 C++ 的 `FLAGS_log_dir.empty()` 分支一致）。
    pub log_dir: PathBuf,
    pub distribution_name: String,
    pub distribution_code_name: String,
    pub distribution_version: String,
    pub app_name: String,
    pub backup_config_files: bool,
    /// `UserDb::Require("userdb")` 的替身。`None` = 该组件没注册
    /// （C++ 里 `UserDictManager` 拿到的 `user_db_component_` 是空指针）。
    /// `UserDb::Require("legacy_userdb")` 的替身。`None` = 没有可升级的旧词典。
    /// 本任务建的临时目录前缀（见 [`Deployer::new_in`]）—— 只给测试用，C++ 没有。
    temp_root: Option<PathBuf>,
    /// HRRN 调度(用户 2026-11-07):任务带到达时刻,按最高响应比出队,
    /// 避免短任务被长任务(如全量词库编译)堵死。
    pending_tasks: Vec<ScheduledTask>,
    /// 各任务类型的最近一次耗时(毫秒),来自历史执行的起止记录。
    task_stats: std::collections::HashMap<String, u64>,
    maintenance_mode: bool,
    messages: Vec<(String, String)>,
}

impl Default for Deployer {
    fn default() -> Self {
        Self::new()
    }
}

/// HRRN 响应比：(已等待 + 预计耗时) / 预计耗时。
///
/// 单列成函数是为了能拿具体数字做回归：短任务追得快，长任务等久了也会反超。
#[inline]
fn response_ratio(wait_ms: u64, est_ms: u64) -> f64 {
    let est_ms = est_ms.max(1);
    (wait_ms + est_ms) as f64 / est_ms as f64
}

/// 编译侧台账交接文件：`<user>/build/deploy_records.yaml`。
///
/// 用户要求（2026-11-11）：在已有的 `user.yaml` 哈希段（`weasel_next.folders`）后面
/// 追加「方案名 / 哈希 / 到达 / 结束」，`build/` 里也留一份对应文件。
/// 引擎侧只写这份交接文件；Server 部署成功后由 `common::deploy_manifest` 读它，
/// 合并进 `user.yaml` 并回写完整版到 `build/`。不再另起 CSV 台账。
pub const DEPLOY_RECORDS_FILE: &str = "build/deploy_records.yaml";

/// 墙钟时间（Unix 毫秒）。HRRN 队列里的 `Instant` 是单调时钟、不能落盘，
/// 台账要用可序列化的墙钟，跨重启也能复盘。
pub fn wall_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Unix 毫秒 → ISO8601 UTC（与 IME 日志同一时区，便于对照）。
/// 用 Howard Hinnant 的 civil_from_days，不为格式化时间引入 chrono 依赖。
pub fn iso_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let sod = secs % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    format!(
        "{year:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60,
        ms % 1000
    )
}

impl Deployer {
    /// `Deployer::Deployer()`：全部默认值照抄。
    pub fn new() -> Self {
        Self {
            program_dir: PathBuf::new(),
            shared_data_dir: PathBuf::from("."),
            user_data_dir: PathBuf::from("."),
            // 出厂预编译取消（程序文件夹禁止 build），默认即空。
            prebuilt_data_dir: PathBuf::new(),
            // 相对 "build" 仅测试占位；真实部署由 `layout()` 钉到 `用户文件夹/build`。
            staging_dir: PathBuf::from("build"),
            sync_dir: PathBuf::from("sync"),
            records_root_override: None,
            user_id: "unknown".to_string(),
            log_dir: PathBuf::new(),
            distribution_name: String::new(),
            distribution_code_name: String::new(),
            distribution_version: String::new(),
            app_name: String::new(),
            backup_config_files: true,
            temp_root: None,
            pending_tasks: Vec::new(),
            task_stats: std::collections::HashMap::new(),
            maintenance_mode: false,
            messages: Vec::new(),
        }
    }

    /// **Rust 侧新增**：把三个根目录 + 派生的 staging/sync 一次设好
    /// （`setup.cc` 的 `SetupDeployer` 干的事）。
    ///
    /// `program_dir`（`PROGRAM_DIR`）由 `shared_data_dir` 的父目录推出 —— 宿主经 API 注入的是
    /// `shared_data_dir`（即 `Rime-Next` 数据目录），其所在安装目录就是程序文件夹；也可用
    /// [`Deployer::set_program_dir`] 显式覆盖。
    /// `prebuilt_data_dir` 缺省是**空**（2026-10-25 硬规则：程序文件夹禁止任何 build，
    /// 出厂预编译取消）、`staging_dir` 缺省是 `<user>/build`
    /// （`setup.cc:73-80`）；`sync_dir` 在 `installation_update` 里会被
    /// `installation.yaml` 的 `sync_dir` 覆盖，缺省 `<user>/sync`。
    pub fn with_data_dirs(
        shared_data_dir: impl Into<PathBuf>,
        user_data_dir: impl Into<PathBuf>,
    ) -> Self {
        let shared: PathBuf = shared_data_dir.into();
        let user: PathBuf = user_data_dir.into();
        let mut deployer = Self::new();
        // 2026-10-25：prebuilt 不再指向 `<shared>/build`，避免程序文件夹出现 build。
        deployer.prebuilt_data_dir = PathBuf::new();
        deployer.staging_dir = user.join("build");
        deployer.sync_dir = user.join("sync");
        deployer.shared_data_dir = shared.clone();
        deployer.user_data_dir = user;
        // 程序文件夹 = 共享数据目录的父目录（安装目录）；无父目录时退回共享数据目录自身。
        deployer.program_dir = shared
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| shared.clone());
        deployer
    }

    // ---- 三个根的 get/set（宿主经 API 注入实际路径）----

    /// set `PROGRAM_DIR`：程序文件夹（安装目录）。宿主可在注入 `shared_data_dir` 后覆盖，
    /// 例如宿主知道自己装在哪、不依赖 `shared_data_dir.parent()` 的推断。
    pub fn set_program_dir(&mut self, dir: impl Into<PathBuf>) {
        self.program_dir = dir.into();
    }

    /// set `SHARE_DIR`：共享数据目录（即 `Rime-Next` 数据目录）。
    pub fn set_shared_data_dir(&mut self, dir: impl Into<PathBuf>) {
        self.shared_data_dir = dir.into();
    }

    /// set `USER_DIR`：用户文件夹。
    pub fn set_user_data_dir(&mut self, dir: impl Into<PathBuf>) {
        self.user_data_dir = dir.into();
    }

    /// 测试专用：把部署台账 `build/deploy_records.yaml` 重定向到独立根，
    /// 避免离线对照测试把假记录追加进真实 `%APPDATA%/Rime-Next` 台账。
    /// 传 `None` 恢复生产行为（跟随 `user_data_dir`）。
    pub fn set_records_root(&mut self, root: Option<impl Into<PathBuf>>) {
        self.records_root_override = root.map(Into::into);
    }

    /// 测试台账/统计隔离根（纯只读）。`None` = 跟随 `user_data_dir`。
    /// 并行部署的 HRRN 耗时账本 `var/deploy_hrrn_est.json` 也跟随此根，
    /// 避免离线对照测试把假耗时写进真实 `%APPDATA%/Rime-Next`。
    pub fn records_root_override_for_test(&self) -> Option<&Path> {
        self.records_root_override.as_deref()
    }

    /// get `PROGRAM_DIR`。
    pub fn program_dir(&self) -> &Path {
        &self.program_dir
    }

    /// **Rust 侧新增**（测试用）：目录全在 `root` 下的沙箱布局。
    pub fn new_in(root: &Path) -> Self {
        let mut deployer = Self::with_data_dirs(root.join("shared"), root.join("user"));
        deployer.prebuilt_data_dir = root.join("prebuilt");
        deployer.temp_root = Some(root.to_path_buf());
        deployer
    }

    /// 布局视图（路径解析规则都在 [`super::paths::Layout`]）。
    ///
    /// **字段即单一事实源**：`staging_dir` / `prebuilt_data_dir` 直接取自本结构体的
    /// 字段，`layout()` 不二次改写。2026-10-25 硬规则（build 只存在于用户文件夹、
    /// 出厂预编译取消）在**注入侧**执行，共有三个入口保证：
    /// * [`Self::with_data_dirs`]：`staging = <user>/build`、`prebuilt = 空`；
    /// * `c_api::rime_deployer_initialize`：同样强制，无视 `RimeTraits` 传入值；
    /// * `c_api` 的 `set_shared_data_dir` / `set_user_data_dir` 同步重算 staging。
    /// 这样 `layout()` 对字段忠实（测试可直接覆盖 `staging_dir` 指向沙箱），
    /// 而真实宿主链路永远拿到用户文件夹下的 build。
    ///
    /// `program_dir` / `shared_data_dir` / `user_data_dir` 都是**宿主（Weasel）通过
    /// API 经 get/set 注入**的根路径（lrn 持有为状态、不写死值）。
    pub fn layout(&self) -> super::paths::Layout {
        super::paths::Layout::new(
            &self.program_dir,
            &self.shared_data_dir,
            &self.user_data_dir,
            &self.staging_dir,
            &self.prebuilt_data_dir,
        )
    }

    /// `user_data_sync_dir()`：`sync_dir / user_id`。
    pub fn user_data_sync_dir(&self) -> PathBuf {
        self.sync_dir.join(&self.user_id)
    }

    /// 测试沙箱根（C++ 没有这个字段）。
    pub fn temp_root(&self) -> Option<&Path> {
        self.temp_root.as_deref()
    }

    // ---------------------------------------------------------------------
    // 队列（deployer.cc:42-77）
    // ---------------------------------------------------------------------

    /// `ScheduleTask(task)`。
    pub fn schedule(&mut self, task: DeploymentTask) {
        self.pending_tasks.push(ScheduledTask {
            task,
            arrival: std::time::Instant::now(),
        });
    }

    /// `ScheduleTask(name, arg)`：名字不认识 → false（C++ 记 `unknown deployment task`）。
    pub fn schedule_named(&mut self, name: &str) -> bool {
        match DeploymentTask::from_name(name) {
            Some(task) => {
                self.schedule(task);
                true
            }
            None => {
                log_warn(&format!("unknown deployment task: {name}"));
                false
            }
        }
    }

    /// `NextTask()`——HRRN(最高响应比优先):
    /// 响应比 = (已等待 + 预计耗时) / 预计耗时。预计耗时取该类任务
    /// 上一次执行的起止差(没记录过的按 1 秒基线,同基线间按先来后到)。
    /// 短任务响应比追得快、长任务等待久了响应比也会涨——都不饿死。
    pub fn next_task(&mut self) -> Option<DeploymentTask> {
        let now = std::time::Instant::now();
        let mut best: Option<(usize, f64)> = None;
        for (idx, scheduled) in self.pending_tasks.iter().enumerate() {
            let key = scheduled.task.name();
            let est_ms = *self.task_stats.get(key).unwrap_or(&1000);
            let wait_ms = now
                .saturating_duration_since(scheduled.arrival)
                .as_millis() as u64;
            let ratio = response_ratio(wait_ms, est_ms);
            if best.map_or(true, |(_, b)| ratio > b) {
                best = Some((idx, ratio));
            }
        }
        let (idx, _) = best?;
        Some(self.pending_tasks.swap_remove(idx).task)
    }

    /// 记录一次执行的起止耗时(毫秒),并持久化到用户目录 var/deploy_stats.json。
    fn record_task_stat(&mut self, key: &str, elapsed_ms: u64) {
        self.task_stats.insert(key.to_string(), elapsed_ms);
        self.persist_task_stats();
    }

    /// 统计文件固定在 `<user_data_dir>/var/deploy_stats.json`。
    ///
    /// ⚠️ 2026-11-11 修：原实现走 `user_data_sync_dir().parent()`，落点是
    /// `<user>/sync/var/`，既和文档/汇报写的 `var/` 不符，又会随 `sync_dir` 被
    /// `installation.yaml` 改写而漂移；实测 live 里这个文件从未生成过。
    fn stats_path(&self) -> PathBuf {
        self.user_data_dir.join("var").join("deploy_stats.json")
    }

    fn persist_task_stats(&self) {
        let Ok(text) = serde_json::to_string(&self.task_stats) else { return };
        let path = self.stats_path();
        if std::fs::create_dir_all(path.parent().unwrap_or(&self.user_data_dir)).is_ok() {
            let _ = std::fs::write(path, text);
        }
    }

    fn load_task_stats(&mut self) {
        let path = self.stats_path();
        let Ok(text) = std::fs::read_to_string(path) else { return };
        if let Ok(map) = serde_json::from_str::<std::collections::HashMap<String, u64>>(&text) {
            self.task_stats = map;
        }
    }

    /// `HasPendingTasks()`。
    pub fn has_pending_tasks(&self) -> bool {
        !self.pending_tasks.is_empty()
    }

    /// 队列长度（Rust 侧新增，测试/日志用）。
    pub fn pending_count(&self) -> usize {
        self.pending_tasks.len()
    }

    // ---------------------------------------------------------------------
    // 执行（deployer.cc:28-40 / 79-104）
    // ---------------------------------------------------------------------

    /// `RunTask(task)`：立刻跑一个任务。
    pub fn run_task(&mut self, task: DeploymentTask) -> bool {
        task.run(self)
    }

    /// `RunTask(name, arg)`：按注册名跑（参数用默认值）。
    pub fn run_named(&mut self, name: &str) -> bool {
        match DeploymentTask::from_name(name) {
            Some(task) => task.run(self),
            None => {
                log_warn(&format!("unknown deployment task: {name}"));
                false
            }
        }
    }

    /// `Deployer::Run()`：清空队列，边跑边收新任务，最后统计并**发通知信号**。
    ///
    /// 返回值 = `failure == 0`（与 C++ 一致）。`messages` 仍保留为事件记录（既有
    /// 测试/离线对照读它），但对外通知统一走 [`crate::signal`] 总线，
    /// 与 C++ `Deployer` → `Messenger::message_sink_` → `Service::Notify` 同款。
    pub fn run(&mut self) -> bool {
        log_info("running deployment tasks:");
        self.messages.push(("deploy".to_string(), "start".to_string()));
        crate::signal::emit_deploy(crate::signal::DeployPhase::Start);
        let mut success = 0usize;
        let mut failure = 0usize;
        self.load_task_stats();
        loop {
            while let Some(task) = self.next_task() {
                // 起止记录(用户 2026-11-07):起始/结束时间 → 该类任务的预计耗时。
                let key = task.name().to_string();
                let started = std::time::Instant::now();
                // C++ 的 try/catch：任务里抛异常只算一次 failure，循环继续。
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    task.run(self)
                }));
                self.record_task_stat(&key, started.elapsed().as_millis() as u64);
                match outcome {
                    Ok(true) => success += 1,
                    Ok(false) => failure += 1,
                    Err(_) => {
                        failure += 1;
                        log_warn("Error deploying: task panicked");
                    }
                }
            }
            log_info(&format!(
                "{} tasks ran: {} success, {} failure.",
                success + failure,
                success,
                failure
            ));
            let phase = if failure == 0 {
                crate::signal::DeployPhase::Success
            } else {
                crate::signal::DeployPhase::Failure
            };
            self.messages.push((
                "deploy".to_string(),
                if failure == 0 { "success" } else { "failure" }.to_string(),
            ));
            crate::signal::emit_deploy(phase);
            // 发通知的过程中可能又有任务被塞进来（C++ 的 do/while + HasPendingTasks）
            if !self.has_pending_tasks() {
                break;
            }
        }
        failure == 0
    }

    /// `StartWork(maintenance_mode)`：队列为空 → false；否则跑 [`Deployer::run`]。
    ///
    /// **与 C++ 的差异**：同步执行（C++ 起工作线程）。返回值语义不变。
    pub fn start_work(&mut self, maintenance_mode: bool) -> bool {
        self.maintenance_mode = maintenance_mode;
        if self.pending_tasks.is_empty() {
            return false;
        }
        self.run()
    }

    /// `StartMaintenance()`。
    pub fn start_maintenance_work(&mut self) -> bool {
        self.start_work(true)
    }

    /// `IsWorking()`：同步执行下永远是 false（任务已经跑完了）。
    pub fn is_working(&self) -> bool {
        false
    }

    /// `IsMaintenanceMode()`：C++ 是 `maintenance_mode_ && IsWorking()`，同步执行下恒 false。
    pub fn is_maintenance_mode(&self) -> bool {
        false
    }

    /// `JoinWorkThread()`：没有线程可等（同步执行）。
    pub fn join_work_thread(&self) {}

    /// `JoinMaintenanceThread()`。
    pub fn join_maintenance_thread(&self) {}

    /// 通知事件记录（替代 `Messenger::message_sink_`）。
    pub fn messages(&self) -> &[(String, String)] {
        &self.messages
    }

    // ---------------------------------------------------------------------
    // 序列（rime_api_impl.h:65-145）
    // ---------------------------------------------------------------------

    /// 往 `<user>/build/deploy_records.yaml` 追加一条 `方案名/哈希/到达/结束`。
    ///
    /// 哈希沿用引擎编译期已经算过的源文件 checksum（`lever::tree::checksum`，8 位
    /// hex），不另算 SHA256、不新增哈希体系。到达/结束是墙钟（ISO8601 UTC），
    /// `@deploy` 是整次部署的汇总。Server 部署成功后读走它，写进 `user.yaml` 的
    /// `weasel_next` 哈希段，并回写一份完整版到 `build/`。
    pub fn append_deploy_record(
        &self,
        schema: &str,
        hash: u32,
        arrival_ms: u64,
        finished_ms: u64,
    ) {
        let records_root = self
            .records_root_override
            .as_ref()
            .unwrap_or(&self.user_data_dir);
        let path = records_root.join(DEPLOY_RECORDS_FILE);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let line = format!(
            "    - schema: \"{schema}\"\n      hash: \"{hash:08x}\"\n      arrived_at: \"{}\"\n      finished_at: \"{}\"\n",
            iso_utc(arrival_ms),
            iso_utc(finished_ms)
        );
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        if !text.contains("weasel_next:") {
            text.push_str("# >>> weasel_next 部署记录（自动生成，请勿手改）>>>\n");
            text.push_str("weasel_next:\n  records:\n");
        }
        // 结束标记每次刷到末尾，保证下一条追加在它之前
        if let Some(position) = text.find("# <<< weasel_next") {
            text.truncate(position);
        }
        text.push_str(&line);
        text.push_str("# <<< weasel_next 部署记录 <<<\n");
        let _ = std::fs::write(&path, text);
    }

    /// `RimeDeployWorkspace()` —— 也就是 C API 的 `deploy`，Weasel-Next `--deploy` 走这条。
    ///
    /// ```text
    /// installation_update && workspace_update && user_dict_upgrade && cleanup_trash
    /// ```
    /// `&&` 是**短路**的：前一步失败，后面的步骤根本不会跑（C++ 的 `&&` 链）。
    /// 2026-11-11：整次部署的到达/结束也写一条 `@deploy` 汇总进 build 交接文件，
    /// Server 部署成功后把它并进 `user.yaml` 的 `weasel_next` 哈希段。
    pub fn deploy_workspace(&mut self) -> bool {
        let arrival_ms = wall_now_ms();
        let ok = self.run_task(DeploymentTask::InstallationUpdate)
            && self.run_task(DeploymentTask::WorkspaceUpdate)
            && self.run_task(DeploymentTask::UserDictUpgrade)
            && self.run_task(DeploymentTask::CleanupTrash);
        let finished_ms = wall_now_ms();
        let hash = crate::lever::tree::checksum(
            &self.user_data_dir.join("schema").join("schema.yaml"),
        );
        self.append_deploy_record("@deploy", hash, arrival_ms, finished_ms);
        ok
    }

    /// `RimeStartMaintenance(full_check)`。
    ///
    /// * `clean_old_log_files` 与 `installation_update` 是**同步**跑的，失败即整体失败；
    /// * `full_check == false` 时先跑 `detect_modifications`：
    ///   它返回 true 表示"检测到改动、需要更新"，false 表示"没改动"→ 整个维护**不启动**
    ///   （这就是 `RimeStartMaintenanceOnWorkspaceChange` 的语义）；
    /// * 之后三步进队列，由 [`Deployer::start_maintenance_work`] 消费。
    pub fn start_maintenance(&mut self, full_check: bool) -> bool {
        self.run_task(DeploymentTask::CleanOldLogFiles);
        if !self.run_task(DeploymentTask::InstallationUpdate) {
            return false;
        }
        if !full_check {
            let data_dirs = vec![self.user_data_dir.clone(), self.shared_data_dir.clone()];
            if !self.run_task(DeploymentTask::DetectModifications { data_dirs }) {
                return false;
            }
            log_info("changes detected; starting maintenance.");
        }
        self.schedule(DeploymentTask::WorkspaceUpdate);
        self.schedule(DeploymentTask::UserDictUpgrade);
        self.schedule(DeploymentTask::CleanupTrash);
        self.start_maintenance_work();
        true
    }

    /// `RimeSyncUserData()`：队列 = installation_update / backup_config_files / user_dict_sync。
    pub fn sync_user_data(&mut self) -> bool {
        self.schedule(DeploymentTask::InstallationUpdate);
        self.schedule(DeploymentTask::BackupConfigFiles);
        self.schedule(DeploymentTask::UserDictSync);
        self.start_maintenance_work()
    }

    /// `RimePrebuildAllSchemas()`。
    pub fn prebuild_all_schemas(&mut self) -> bool {
        self.run_task(DeploymentTask::PrebuildAllSchemas)
    }

    /// `RimeDeploySchema(schema_file)`。
    pub fn deploy_schema(&mut self, source_path: impl Into<PathBuf>) -> bool {
        self.run_task(DeploymentTask::SchemaUpdate {
            source_path: source_path.into(),
            verbose: false,
        })
    }

    /// `RimeDeployConfigFile(file_name, version_key)`。
    pub fn deploy_config_file(&mut self, file_name: &str, version_key: &str) -> bool {
        self.run_task(DeploymentTask::ConfigFileUpdate {
            file_name: file_name.to_string(),
            version_key: version_key.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(root: &Path) -> Deployer {
        Deployer::with_data_dirs(root.join("shared"), root.join("user"))
    }

    /// 短任务不被长任务堵死：同样的等待时间下，预计耗时短的任务响应比更高，先出队。
    #[test]
    fn hrrn_short_task_beats_long_one() {
        let mut d = sandbox(Path::new("."));
        let long = DeploymentTask::WorkspaceUpdate;
        let short = DeploymentTask::CleanupTrash;
        let long_name = long.name().to_string();
        let short_name = short.name().to_string();
        d.task_stats.insert(long_name.clone(), 11_000);
        d.task_stats.insert(short_name.clone(), 100);
        d.schedule(long); // 长任务先到（FIFO 会先跑它）
        d.schedule(short);
        let past = std::time::Instant::now() - std::time::Duration::from_millis(50);
        for s in &mut d.pending_tasks {
            s.arrival = past;
        }
        assert_eq!(d.next_task().unwrap().name(), short_name);
        assert_eq!(d.next_task().unwrap().name(), long_name);
        assert!(d.next_task().is_none());
    }

    /// 长任务不会饿死：等得够久，响应比反超刚到达的短任务。
    #[test]
    fn hrrn_long_task_not_starved() {
        let mut d = sandbox(Path::new("."));
        let long = DeploymentTask::WorkspaceUpdate;
        let short = DeploymentTask::CleanupTrash;
        let long_name = long.name().to_string();
        d.task_stats.insert(long_name.clone(), 10_000);
        d.task_stats.insert(short.name().to_string(), 100);
        d.schedule(long);
        d.schedule(short);
        let now = std::time::Instant::now();
        // 长任务已等 60s：(60000+10000)/10000 = 7；短任务刚到：(0+100)/100 = 1
        d.pending_tasks[0].arrival = now - std::time::Duration::from_secs(60);
        d.pending_tasks[1].arrival = now;
        assert_eq!(d.next_task().unwrap().name(), long_name);
    }

    /// 响应比公式的具体输入 → 输出（用户要求测试给数字，不只要「通过」）。
    #[test]
    fn hrrn_response_ratio_numbers() {
        let cases = [
            (0u64, 100u64, 1.0f64),      // 刚到达的短任务
            (50, 100, 1.5),              // 等 50ms 的短任务追得最快
            (50, 11_000, 1.004_545_454_545_454_6), // 等同样久的长任务几乎没涨
            (60_000, 10_000, 7.0),       // 长任务等 60s 后反超
            (60_000, 100, 601.0),        // 短任务等 60s 会暴涨
        ];
        for (wait, est, expected) in cases {
            let got = response_ratio(wait, est);
            println!("wait={wait}ms est={est}ms -> R={got:.6}");
            assert!((got - expected).abs() < 1e-9, "wait={wait} est={est}");
        }
    }

    /// 4 个任务同时排队：按响应比出队，且顺序与 FIFO 不同。
    #[test]
    fn hrrn_trace_four_tasks() {
        let mut d = sandbox(Path::new("."));
        // (任务, 预计耗时, 已等待)——数值就是本用例的输入
        let input: Vec<(DeploymentTask, u64, u64)> = vec![
            (DeploymentTask::WorkspaceUpdate, 10_000, 60_000),
            (DeploymentTask::CleanupTrash, 100, 0),
            (DeploymentTask::CleanOldLogFiles, 500, 2_000),
            (DeploymentTask::BackupConfigFiles, 2_000, 5_000),
        ];
        println!("任务           预计耗时   已等待     响应比");
        for (task, est, wait) in &input {
            println!(
                "{:<15} {:>7}ms {:>7}ms  R={:.3}",
                task.name(),
                est,
                wait,
                response_ratio(*wait, *est)
            );
        }
        let now = std::time::Instant::now();
        for (task, est, wait) in input {
            let key = task.name().to_string();
            d.task_stats.insert(key, est);
            d.schedule(task);
            d.pending_tasks.last_mut().unwrap().arrival =
                now - std::time::Duration::from_millis(wait);
        }
        let order: Vec<String> = std::iter::from_fn(|| d.next_task())
            .map(|task| task.name().to_string())
            .collect();
        println!("实际出队顺序：{order:?}");
        assert_eq!(
            order,
            vec![
                "workspace_update",     // R = 7.000
                "clean_old_log_files",  // R = 5.000
                "backup_config_files",  // R = 3.500
                "cleanup_trash",        // R = 1.000
            ],
            "必须按最高响应比，不能是 FIFO"
        );
    }

    /// 真正走一遍 `run()` 队列：每个任务的耗时都要落到 `<user>/var/deploy_stats.json`。
    #[test]
    fn hrrn_run_persists_real_timings() {
        let root = std::env::temp_dir().join(format!("hrrn-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut d = sandbox(&root);
        d.schedule(DeploymentTask::CleanupTrash);
        d.schedule(DeploymentTask::BackupConfigFiles);
        let _ = d.run(); // 任务本身成功与否不影响「统计必须落盘」
        let path = d.stats_path();
        assert!(path.is_file(), "run() 必须真的写出统计文件：{}", path.display());
        let text = std::fs::read_to_string(&path).unwrap();
        println!("deploy_stats.json = {text}");
        assert!(text.contains("cleanup_trash"), "短任务耗时要有记录");
        assert!(text.contains("backup_config_files"), "长任务耗时要有记录");
        let mut d2 = sandbox(&root);
        d2.load_task_stats();
        assert_eq!(d2.task_stats.len(), 2, "重启后要能读回两类任务的历史耗时");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 统计文件落在 `<user>/var/deploy_stats.json`，且写入后能读回。
    #[test]
    fn stats_path_and_roundtrip() {
        let root = std::env::temp_dir().join(format!("hrrn-stats-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut d = sandbox(&root);
        assert_eq!(
            d.stats_path(),
            root.join("user").join("var").join("deploy_stats.json")
        );
        d.task_stats.insert("cleanup_trash".to_string(), 123);
        d.persist_task_stats();
        assert!(d.stats_path().is_file(), "统计文件必须真的落盘");
        let mut d2 = sandbox(&root);
        d2.load_task_stats();
        assert_eq!(d2.task_stats.get("cleanup_trash"), Some(&123));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 墙钟格式化：epoch 与跨日毫秒都要正确（UTC，不带时区偏移的 `Z`）。
    #[test]
    fn iso_utc_formats_wall_clock() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_utc(86_400_123), "1970-01-02T00:00:00.123Z");
        assert_eq!(iso_utc(1_796_000_000_000), "2026-11-30T00:53:20.000Z");
    }

    /// 编译台账落在 `<user>/build/deploy_records.yaml`，四条字段齐全、可追加，
    /// 交给 Server 合并进 user.yaml 的 weasel_next 哈希段。
    #[test]
    fn deploy_records_land_in_build_for_server_to_merge() {
        let root = std::env::temp_dir().join(format!("deploy-records-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let d = sandbox(&root);
        d.append_deploy_record("pinyin", 0x1234_abcd, 0, 86_400_123);
        d.append_deploy_record("@deploy", 0x0000_00ff, 86_400_123, 86_500_000);
        let path = root.join("user").join(DEPLOY_RECORDS_FILE);
        let text = std::fs::read_to_string(&path).unwrap();
        println!("{text}");
        assert!(text.contains("weasel_next:"), "交接文件带 weasel_next 段");
        assert!(text.contains("  records:"), "方案记录列表");
        assert_eq!(text.matches("- schema:").count(), 2, "两条记录");
        let lines: Vec<&str> = text.lines().collect();
        let at = lines
            .iter()
            .position(|line| line.trim() == "- schema: \"pinyin\"")
            .unwrap();
        assert!(lines[at + 1].contains("hash: \"1234abcd\""), "有哈希");
        assert!(
            lines[at + 2].contains("arrived_at: \"1970-01-01T00:00:00.000Z\""),
            "有到达时间"
        );
        assert!(
            lines[at + 3].contains("finished_at: \"1970-01-02T00:00:00.123Z\""),
            "有结束时间"
        );
        assert!(text.contains("- schema: \"@deploy\""), "整次部署有汇总行");
        let _ = std::fs::remove_dir_all(&root);
    }
}
