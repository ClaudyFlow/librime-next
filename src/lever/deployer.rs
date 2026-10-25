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
//! * `Messenger` 的通知回调换成 [`Deployer::messages`] 里的事件记录（`("deploy","start")` 等）。
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
pub struct Deployer {
    pub shared_data_dir: PathBuf,
    pub user_data_dir: PathBuf,
    pub prebuilt_data_dir: PathBuf,
    pub staging_dir: PathBuf,
    pub sync_dir: PathBuf,
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
    pub user_db: Option<Box<dyn UserDbFactory + Send + Sync>>,
    /// `UserDb::Require("legacy_userdb")` 的替身。`None` = 没有可升级的旧词典。
    pub legacy_user_db: Option<Box<dyn UserDbFactory + Send + Sync>>,
    /// 本任务建的临时目录前缀（见 [`Deployer::new_in`]）—— 只给测试用，C++ 没有。
    temp_root: Option<PathBuf>,
    pending_tasks: VecDeque<DeploymentTask>,
    maintenance_mode: bool,
    messages: Vec<(String, String)>,
}

impl Default for Deployer {
    fn default() -> Self {
        Self::new()
    }
}

impl Deployer {
    /// `Deployer::Deployer()`：全部默认值照抄。
    pub fn new() -> Self {
        Self {
            shared_data_dir: PathBuf::from("."),
            user_data_dir: PathBuf::from("."),
            prebuilt_data_dir: PathBuf::from("build"),
            staging_dir: PathBuf::from("build"),
            sync_dir: PathBuf::from("sync"),
            user_id: "unknown".to_string(),
            log_dir: PathBuf::new(),
            distribution_name: String::new(),
            distribution_code_name: String::new(),
            distribution_version: String::new(),
            app_name: String::new(),
            backup_config_files: true,
            user_db: None,
            legacy_user_db: None,
            temp_root: None,
            pending_tasks: VecDeque::new(),
            maintenance_mode: false,
            messages: Vec::new(),
        }
    }

    /// **Rust 侧新增**：把四个目录一次设好（`setup.cc` 的 `SetupDeployer` 干的事）。
    ///
    /// `prebuilt_data_dir` 缺省是 `<shared>/build`、`staging_dir` 缺省是 `<user>/build`
    /// （`setup.cc:73-80`）；`sync_dir` 在 `installation_update` 里会被
    /// `installation.yaml` 的 `sync_dir` 覆盖，缺省 `<user>/sync`。
    pub fn with_data_dirs(
        shared_data_dir: impl Into<PathBuf>,
        user_data_dir: impl Into<PathBuf>,
    ) -> Self {
        let shared: PathBuf = shared_data_dir.into();
        let user: PathBuf = user_data_dir.into();
        let mut deployer = Self::new();
        deployer.prebuilt_data_dir = shared.join("build");
        deployer.staging_dir = user.join("build");
        deployer.sync_dir = user.join("sync");
        deployer.shared_data_dir = shared;
        deployer.user_data_dir = user;
        deployer
    }

    /// **Rust 侧新增**（测试用）：目录全在 `root` 下的沙箱布局。
    pub fn new_in(root: &Path) -> Self {
        let mut deployer = Self::with_data_dirs(root.join("shared"), root.join("user"));
        deployer.prebuilt_data_dir = root.join("prebuilt");
        deployer.temp_root = Some(root.to_path_buf());
        deployer
    }

    /// 布局视图（路径解析规则都在 [`super::paths::Layout`]）。
    pub fn layout(&self) -> super::paths::Layout {
        super::paths::Layout::new(
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
        self.pending_tasks.push_back(task);
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

    /// `NextTask()`。
    pub fn next_task(&mut self) -> Option<DeploymentTask> {
        self.pending_tasks.pop_front()
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

    /// `Deployer::Run()`：清空队列，边跑边收新任务，最后统计并记通知事件。
    ///
    /// 返回值 = `failure == 0`（与 C++ 一致）。
    pub fn run(&mut self) -> bool {
        log_info("running deployment tasks:");
        self.messages.push(("deploy".to_string(), "start".to_string()));
        let mut success = 0usize;
        let mut failure = 0usize;
        loop {
            while let Some(task) = self.next_task() {
                // C++ 的 try/catch：任务里抛异常只算一次 failure，循环继续。
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    task.run(self)
                }));
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
            self.messages.push((
                "deploy".to_string(),
                if failure == 0 { "success" } else { "failure" }.to_string(),
            ));
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

    /// `RimeDeployWorkspace()` —— 也就是 C API 的 `deploy`，Weasel-Next `--deploy` 走这条。
    ///
    /// ```text
    /// installation_update && workspace_update && user_dict_upgrade && cleanup_trash
    /// ```
    /// `&&` 是**短路**的：前一步失败，后面的步骤根本不会跑（C++ 的 `&&` 链）。
    pub fn deploy_workspace(&mut self) -> bool {
        self.run_task(DeploymentTask::InstallationUpdate)
            && self.run_task(DeploymentTask::WorkspaceUpdate)
            && self.run_task(DeploymentTask::UserDictUpgrade)
            && self.run_task(DeploymentTask::CleanupTrash)
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
