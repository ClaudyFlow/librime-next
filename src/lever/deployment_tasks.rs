//! `src/rime/lever/deployment_tasks.{h,cc}`（749 行）的移植 —— **只做到"枚举 + 名字表 + 接缝"**。
//!
//! ## 为什么先是空实现
//!
//! `deployer` 已经能调度任务、统计成功/失败、发通知事件；它需要的只是一个**任务枚举**
//! 和 `from_name`（C++ 用 Registry 按名字创建，Rust 侧用枚举更直白）。
//! 但 11 个任务的**本体**（每个 `Run()`）还没移植 —— 那些是真正动文件的部分：
//! 编词典、写 `build/`、备份配置、清 trash。**在它们落地前，`run()` 必须诚实地说"没实现"，
//! 不能假装成功**（否则部署会静默什么都不做还报成功）。
//!
//! ## 2026-10-25：前八个任务落地

//!
//! | 任务 | 状态 | 说明 |
//! |---|---|---|
//! | `installation_update` | ✅ | 维护 `installation.yaml`（id/sync_dir/backup_config_files/发行版版本） |
//! | `config_file_update` | ✅ | **编译**配置到 staging（`ConfigNeedsUpdate` mtime 比对 + trash 旧副本） |
//! | `schema_update` | ✅ 含 .bin | schema_id 取源文件字段 → ConfigFileUpdate 编译 → `compile_dictionary`（DictionaryComponent 配置语义 + `dict::dict_compiler` 产 table/prism/reverse 三件套） |
//! | `workspace_update` | ✅ 主循环 | `schema/schema.yaml` 清单（点名制）→ 逐方案编译 → dependencies 递归 → `schema_list` 写回 `staging/default.yaml` |
//! | `cleanup_trash` | ✅ | 用户目录根的部署垃圾移进 `trash/` |
//! | `prebuild_all_schemas` | ✅ | 递归枚举共享目录方案源文件全量预编译（与 C++ `classified_schema_sources` 的目录链校验逐条对齐，系列成员不漏） |
//! | `backup_config_files` | ✅ | 用户目录根 yaml/txt 备份进 `<sync>/<user_id>/`（CRC32 比对 + customized copy 跳过） |
//! | `detect_modifications` | ✅ | yaml 最大 mtime vs `user.yaml:var/last_build_time`（`start_maintenance(false)` 的门） |
//!
//! `deploy` 主链 = installation_update → workspace_update → user_dict_upgrade → cleanup_trash：
//! **四步里三步已通**，只剩 `user_dict_upgrade`（等 `dict` 的 `UserDb`）。
//! `sync_user_data` 链 = installation_update → backup_config_files → user_dict_sync：前两步已通。
//! 剩余未移植：user_dict_upgrade / user_dict_sync / clean_old_log_files
//! （后者依赖 glog 的日志目录机制，Rust 侧无对应物，**仍然诚实返回 false**）。

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{self, Item};
use crate::load_file;

use super::deployer::Deployer;
use crate::lever::tree::{log_error, log_info, log_warn};

/// 一个部署任务。变体对应 C++ 的一个类；`from_name` 对应 `levers_module.cc` 的注册表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentTask {
    /// `InstallationUpdate`：写/更新 `installation.yaml`。
    InstallationUpdate,
    /// `WorkspaceUpdate`：给用户目录补齐出厂文件（`default.yaml` 等）。
    WorkspaceUpdate,
    /// `UserDictUpgrade`：用户词典格式升级。
    UserDictUpgrade,
    /// `CleanupTrash`：清 `trash/`。
    CleanupTrash,
    /// `CleanOldLogFiles`：清旧日志。
    CleanOldLogFiles,
    /// `DetectModifications`：比较数据目录的 mtime 判断要不要维护。
    DetectModifications { data_dirs: Vec<PathBuf> },
    /// `BackupConfigFiles`：备份用户根下的 `*.yaml`（TOML 2026-10-26 起不认）。
    BackupConfigFiles,
    /// `UserDictSync`：用户词典同步。
    UserDictSync,
    /// `PrebuildAllSchemas`：把 `<共享目录>\*.schema.yaml` 全部预编译一遍。
    PrebuildAllSchemas,
    /// `SchemaUpdate`：编译单个方案。
    SchemaUpdate { source_path: PathBuf, verbose: bool },
    /// `ConfigFileUpdate`：把出厂 `<file_name>` 拷到用户目录（带版本键判断）。
    ConfigFileUpdate { file_name: String, version_key: String },
}

impl DeploymentTask {
    /// 全部注册名 —— 与 `levers_module.cc` 的注册表**逐字一致**（测试会钉住）。
    pub const NAMES: [&'static str; 11] = [
        "installation_update",
        "workspace_update",
        "user_dict_upgrade",
        "cleanup_trash",
        "clean_old_log_files",
        "detect_modifications",
        "backup_config_files",
        "user_dict_sync",
        "prebuild_all_schemas",
        "schema_update",
        "config_file_update",
    ];

    /// `Registry::Create(name)` 的替代。注意：C++ 里带参数的任务（`schema_update` 等）
    /// 也能按名字创建，参数用默认值 —— 这里同样给默认值。
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "installation_update" => Self::InstallationUpdate,
            "workspace_update" => Self::WorkspaceUpdate,
            "user_dict_upgrade" => Self::UserDictUpgrade,
            "cleanup_trash" => Self::CleanupTrash,
            "clean_old_log_files" => Self::CleanOldLogFiles,
            // C++ 的默认参数是"用户目录 + 共享目录"两个 data dir
            "detect_modifications" => Self::DetectModifications {
                data_dirs: Vec::new(),
            },
            "backup_config_files" => Self::BackupConfigFiles,
            "user_dict_sync" => Self::UserDictSync,
            "prebuild_all_schemas" => Self::PrebuildAllSchemas,
            "schema_update" => Self::SchemaUpdate {
                source_path: PathBuf::new(),
                verbose: false,
            },
            "config_file_update" => Self::ConfigFileUpdate {
                file_name: String::new(),
                version_key: String::new(),
            },
            _ => return None,
        })
    }

    /// 对应的注册名（日志用）。
    pub fn name(&self) -> &'static str {
        match self {
            Self::InstallationUpdate => "installation_update",
            Self::WorkspaceUpdate => "workspace_update",
            Self::UserDictUpgrade => "user_dict_upgrade",
            Self::CleanupTrash => "cleanup_trash",
            Self::CleanOldLogFiles => "clean_old_log_files",
            Self::DetectModifications { .. } => "detect_modifications",
            Self::BackupConfigFiles => "backup_config_files",
            Self::UserDictSync => "user_dict_sync",
            Self::PrebuildAllSchemas => "prebuild_all_schemas",
            Self::SchemaUpdate { .. } => "schema_update",
            Self::ConfigFileUpdate { .. } => "config_file_update",
        }
    }

    /// `DeploymentTask::Run(deployer)`。
    ///
    /// 2026-10-25：`config_file_update` / `schema_update` / `workspace_update` 已落地；
    /// 其余任务**仍然返回 false** —— 宁可显式失败，也不要在真机上"部署成功但什么都没做"。
    pub fn run(&self, deployer: &mut Deployer) -> bool {
        match self {
            Self::ConfigFileUpdate {
                file_name,
                version_key,
            } => run_config_file_update(deployer, file_name, version_key),
            Self::SchemaUpdate {
                source_path,
                verbose,
            } => run_schema_update(deployer, source_path, *verbose),
            Self::WorkspaceUpdate => run_workspace_update(deployer),
            Self::InstallationUpdate => run_installation_update(deployer),
            Self::CleanupTrash => run_cleanup_trash(deployer),
            Self::PrebuildAllSchemas => run_prebuild_all_schemas(deployer),
            Self::BackupConfigFiles => run_backup_config_files(deployer),
            Self::DetectModifications { data_dirs } => {
                run_detect_modifications(deployer, data_dirs)
            }
            Self::UserDictUpgrade => run_user_dict_upgrade(deployer),
            Self::UserDictSync => run_user_dict_sync(deployer),
            Self::CleanOldLogFiles => run_clean_old_log_files(deployer),
            _ => {
                log_warn(&format!(
                    "deployment task `{}` not implemented yet (lever 层任务本体待移植)",
                    self.name()
                ));
                false
            }
        }
    }
}

/// `InstallationUpdate::Run`（C++ deployment_tasks.cc:85-168）：维护 `installation.yaml`。
///
/// 读侧把用户目录里已有的 `installation_id` / `sync_dir` / `backup_config_files`
/// 同步回 `deployer`（C++ 的 `for now:` 注释原样保留）；写侧在"首次安装"或
/// "发行版/Rime 版本变了"时更新文件。
fn run_installation_update(deployer: &mut Deployer) -> bool {
    log_info("updating rime installation info.");
    if !deployer.user_data_dir.exists() {
        log_info(&format!(
            "creating user data dir: {}",
            deployer.user_data_dir.display()
        ));
        let _ = fs::create_dir_all(&deployer.user_data_dir);
    }
    let installation_info = deployer.user_data_dir.join("installation.yaml");
    let mut config: crate::lever::tree::Tree = serde_json::json!({});
    let mut installation_id = String::new();
    let mut last_distro_code_name = String::new();
    let mut last_distro_version = String::new();
    let mut last_rime_version = String::new();
    if let Some(tree) = crate::lever::tree::load(&installation_info) {
        config = tree;
        if let Some(id) = crate::lever::tree::get_string(&config, "installation_id") {
            log_info(&format!("installation info exists. installation id: {id}"));
            installation_id = id.clone();
            deployer.user_id = id; // for now:
        }
        if let Some(sync_dir) = crate::lever::tree::get_string(&config, "sync_dir") {
            deployer.sync_dir = PathBuf::from(sync_dir);
        } else {
            deployer.sync_dir = deployer.user_data_dir.join(crate::lever::paths::SYNC_DIR);
        }
        log_info(&format!("sync dir: {}", deployer.sync_dir.display()));
        if let Some(backup) = crate::lever::tree::get_bool(&config, "backup_config_files") {
            deployer.backup_config_files = backup;
        }
        if let Some(name) = crate::lever::tree::get_string(&config, "distribution_code_name") {
            log_info(&format!("previous distribution: {name}"));
            last_distro_code_name = name;
        }
        if let Some(version) = crate::lever::tree::get_string(&config, "distribution_version") {
            last_distro_version = version;
        }
        if let Some(version) = crate::lever::tree::get_string(&config, "rime_version") {
            log_info(&format!("previous Rime version: {version}"));
            last_rime_version = version;
        }
    }
    if !installation_id.is_empty()
        && last_distro_code_name == deployer.distribution_code_name
        && last_distro_version == deployer.distribution_version
        && last_rime_version == crate::config::RIME_VERSION
    {
        return true; // 没变化，不重写
    }
    log_info("creating installation info.");
    let time_str = crate::lever::tree::ctime(crate::lever::tree::now());
    if installation_id.is_empty() {
        installation_id = crate::lever::tree::generate_installation_id();
        log_info(&format!("generated installation id: {installation_id}"));
        deployer.user_id = installation_id.clone(); // for now:
        crate::lever::tree::set_scalar(&mut config, "installation_id", &installation_id);
        crate::lever::tree::set_scalar(&mut config, "install_time", &time_str);
    } else {
        crate::lever::tree::set_scalar(&mut config, "update_time", &time_str);
    }
    if !deployer.distribution_name.is_empty() {
        crate::lever::tree::set_scalar(&mut config, "distribution_name", &deployer.distribution_name);
        log_info(&format!("distribution: {}", deployer.distribution_name));
    }
    if !deployer.distribution_code_name.is_empty() {
        crate::lever::tree::set_scalar(
            &mut config,
            "distribution_code_name",
            &deployer.distribution_code_name,
        );
    }
    if !deployer.distribution_version.is_empty() {
        crate::lever::tree::set_scalar(
            &mut config,
            "distribution_version",
            &deployer.distribution_version,
        );
    }
    crate::lever::tree::set_scalar(&mut config, "rime_version", crate::config::RIME_VERSION);
    log_info(&format!("Rime version: {}", crate::config::RIME_VERSION));
    crate::lever::tree::save(&config, &installation_info)
}

/// `CleanupTrash::Run`（C++ deployment_tasks.cc:687-712）：用户目录根的部署垃圾
/// （`rime.log` / `*.bin` / `*.reverse.kct` / `*.userdb.kct.old` / `*.userdb.kct.snapshot`）
/// 移进 `trash/`。目录不存在返回 false；清不动的文件计 failure。
fn run_cleanup_trash(deployer: &Deployer) -> bool {
    log_info("clean up trash.");
    if !deployer.user_data_dir.exists() {
        return false;
    }
    let trash = deployer.user_data_dir.join("trash");
    let mut success = 0usize;
    let mut failure = 0usize;
    let mut moved_any = false;
    for entry in fs::read_dir(&deployer.user_data_dir)
        .into_iter()
        .flatten()
        .flatten()
    {
        if !entry.path().is_file() {
            continue;
        }
        let file_name = crate::lever::tree::file_name(&entry.path());
        let is_trash = file_name == "rime.log"
            || file_name.ends_with(".bin")
            || file_name.ends_with(".reverse.kct")
            || file_name.ends_with(".userdb.kct.old")
            || file_name.ends_with(".userdb.kct.snapshot");
        if !is_trash {
            continue;
        }
        if !moved_any && !crate::lever::tree::maybe_create_directory(&trash) {
            return false;
        }
        moved_any = true;
        let backup = trash.join(&file_name);
        if fs::rename(entry.path(), &backup).is_ok() {
            success += 1;
        } else {
            log_warn(&format!("error clean up file {}", entry.path().display()));
            failure += 1;
        }
    }
    if failure > 0 {
        log_info(&format!("cleaned up {success} files, {failure} failure."));
        false
    } else {
        true
    }
}

/// `PrebuildAllSchemas::Run`：把共享目录里**所有**方案源文件预编译一遍。
///
/// 枚举规则与 C++ 补丁（`classified_schema_sources`，`recursive_directory_iterator` +
/// 目录链校验）逐条对齐：递归扫 `schema/`，只认"目录链按点号拼接 == 文件名主干"的
/// `<id>.schema.yaml` —— folder==stem==ID 才认，摆错位置任何 ID 都解析不到，不等即跳过。
/// 排序只为结果确定。
fn run_prebuild_all_schemas(deployer: &mut Deployer) -> bool {
    let sources = classified_schema_sources(&deployer.shared_data_dir);
    if sources.is_empty() {
        log_warn(&format!(
            "no schema source files under {}",
            deployer.shared_data_dir.join("schema").display()
        ));
        return false;
    }
    let mut success = 0usize;
    let mut failure = 0usize;
    for source in &sources {
        let task = DeploymentTask::SchemaUpdate {
            source_path: source.clone(),
            verbose: false,
        };
        if task.run(deployer) {
            success += 1;
        } else {
            failure += 1;
        }
    }
    log_info(&format!(
        "prebuilt {success} schemas, {failure} failure (of {} source files).",
        sources.len()
    ));
    failure == 0
}

/// 递归枚举 `<root>/schema/` 下的方案源文件（系列分层版）。
///
/// 只认"目录链按点号拼接 == 文件名主干"的 `<id>.schema.yaml`：folder==stem==ID 才认，
/// 摆错位置（如 `<系列>/<其它名>.schema.yaml`）任何 ID 都解析不到，不等即跳过。
fn classified_schema_sources(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, chain: &[String], out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let mut children: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        children.sort();
        for path in children {
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let mut chain = chain.to_vec();
            chain.push(name.to_string());
            let id = chain.join(".");
            let candidate = path.join(format!("{id}.schema.yaml"));
            if candidate.is_file() {
                out.push(candidate);
            }
            walk(&path, &chain, out); // 更深层的系列成员
        }
    }
    let mut files = Vec::new();
    walk(&root.join("schema"), &[], &mut files);
    files.sort();
    files
}

/// `DetectModifications::Run`（C++ deployment_tasks.cc:47-83）：数据目录里 yaml 文件
/// （`user.yaml` 除外）的最大 mtime 与 `user.yaml` 的 `var/last_build_time` 比较。
/// 返回 true = "检测到改动，workspace 需要更新"（`start_maintenance(false)` 的门）；
/// 读文件信息出错时按 C++ 的保守策略返回 true。
fn run_detect_modifications(deployer: &Deployer, data_dirs: &[PathBuf]) -> bool {
    let dirs = if data_dirs.is_empty() {
        vec![
            deployer.user_data_dir.clone(),
            deployer.shared_data_dir.clone(),
        ]
    } else {
        data_dirs.to_vec()
    };
    let mut last_modified = 0i64;
    for dir in &dirs {
        // canonical 失败（目录不存在等）时退回原路径；C++ 出错直接 return true
        let Ok(resolved) = fs::canonicalize(dir) else {
            log_warn(&format!("Error reading file information: {}", dir.display()));
            return true;
        };
        last_modified = last_modified.max(crate::lever::tree::modified_time(&resolved).unwrap_or(0));
        if !resolved.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&resolved).into_iter().flatten().flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if crate::lever::tree::extension(&path) != ".yaml" {
                continue;
            }
            if crate::lever::tree::file_name(&path) == "user.yaml" {
                continue;
            }
            last_modified = last_modified.max(crate::lever::tree::modified_time(&path).unwrap_or(0));
        }
    }
    let last_build_time = crate::lever::tree::load(&deployer.user_data_dir.join("user.yaml"))
        .as_ref()
        .and_then(|tree| crate::lever::tree::get_int(tree, "var/last_build_time"))
        .unwrap_or(0) as i64;
    if last_modified > last_build_time {
        log_info("modifications detected. workspace needs update.");
        true
    } else {
        false
    }
}

/// `BackupConfigFiles::Run`（C++ deployment_tasks.cc:640-684）：把用户目录根的
/// `*.yaml` / `*.txt` 备份进 `<sync_dir>/<user_id>/`。已备份且 CRC32 相同的跳过；
/// 用户手工改过的副本（带 `customization` 键）不备份（同步时以本地为准）。
fn run_backup_config_files(deployer: &Deployer) -> bool {
    if !deployer.backup_config_files {
        log_info("skip backing up config files because it's disabled.");
        return true;
    }
    log_info("backing up config files.");
    if !deployer.user_data_dir.exists() {
        return false;
    }
    let backup_dir = deployer.user_data_sync_dir();
    if !crate::lever::tree::maybe_create_directory(&backup_dir) {
        return false;
    }
    let mut success = 0usize;
    let mut failure = 0usize;
    let mut latest = 0usize;
    let mut skipped = 0usize;
    for entry in fs::read_dir(&deployer.user_data_dir)
        .into_iter()
        .flatten()
        .flatten()
    {
        if !entry.path().is_file() {
            continue;
        }
        let is_yaml = crate::lever::tree::extension(&entry.path()) == ".yaml";
        let is_text = crate::lever::tree::extension(&entry.path()) == ".txt";
        if !is_yaml && !is_text {
            continue;
        }
        let backup = backup_dir.join(entry.file_name());
        if backup.is_file()
            && crate::lever::tree::checksum(&backup) == crate::lever::tree::checksum(&entry.path())
        {
            latest += 1; // already up-to-date
            continue;
        }
        if is_yaml && is_customized_copy(&entry.path()) {
            skipped += 1; // customized copy
            continue;
        }
        match fs::copy(entry.path(), &backup) {
            Ok(_) => success += 1,
            Err(err) => {
                log_warn(&format!("error backing up file {}: {err}", backup.display()));
                failure += 1;
            }
        }
    }
    log_info(&format!(
        "backed up {success} config files to {}, {failure} failed, {latest} up-to-date, {skipped} skipped.",
        backup_dir.display()
    ));
    failure == 0
}

/// `IsCustomizedCopy`（deployment_tasks.cc:626-638）：非 `.custom.yaml` 的 yaml
/// 且带 `customization` 键 → 用户改过的出厂副本。
fn is_customized_copy(path: &Path) -> bool {
    let name = crate::lever::tree::file_name(path);
    if name.ends_with(".yaml")
        && !name.ends_with(".custom.yaml")
        && let Some(tree) = crate::lever::tree::load(path)
    {
        return crate::lever::tree::get_string(&tree, "customization").is_some();
    }
    false
}

/// `UserDictUpgrade::Run`（C++ deployment_tasks.cc:604-619）。
///
/// C++ 的关键短路：**没有注册 `legacy_userdb` 组件就直接返回 true**
/// （注释原文 "nothing to upgrade"）。这条短路很要紧 —— 它让
/// `RimeDeployWorkspace()` 的 `&&` 链在"没装旧版词典"的机器上能走完。
fn run_user_dict_upgrade(deployer: &mut Deployer) -> bool {
    log_info("upgrading user dictionaries.");
    if deployer.legacy_user_db.is_none() {
        // C++ 同款短路：没有 legacy userdb 组件 → 没什么可升级的
        return true;
    }
    let Some(db) = deployer.legacy_user_db.as_mut() else {
        return true;
    };
    // C++ 是**逐个词典**升级（`manager.UpgradeUserDict(*it)`）；我们的
    // `UserDbFactory::upgrade()` 是聚合动作，所以调一次即可。
    db.upgrade()
}

/// `UserDictSync::Run` → `UserDictManager::SynchronizeAll()`（C++ user_dict_manager.cc:210-224）。
///
/// 取用户词典列表，逐个同步；有失败就记失败数，全部成功才返回 true。
/// 没有词典时 C++ 的循环体一次都不进 → 返回 true。
fn run_user_dict_sync(deployer: &mut Deployer) -> bool {
    let destination = deployer.user_data_sync_dir();
    let Some(db) = deployer.user_db.as_mut() else {
        // 没有 userdb 组件 → 没有可同步的词典（C++ 空列表时同样返回 true）
        return true;
    };
    let names = db.list();
    log_info(&format!("synchronizing {} user dicts.", names.len()));
    if names.is_empty() {
        return true;
    }
    if !crate::lever::tree::maybe_create_directory(&destination) {
        return false;
    }
    db.backup(&destination)
}

/// `CleanOldLogFiles::Run`（C++ deployment_tasks.cc:723-…）。
///
/// C++ 的第一道门：`FLAGS_logtostderr || FLAGS_log_dir.empty()` → 直接成功返回。
/// 我们的 `log_dir` 为空即对应这一支（日志走 stderr，没有要清的目录）。
fn run_clean_old_log_files(deployer: &Deployer) -> bool {
    if deployer.log_dir.as_os_str().is_empty() {
        return true;
    }
    if !deployer.log_dir.is_dir() {
        return true;
    }
    // 今天之前的 `.log` 文件删掉；今天的留着。
    let mut success = true;
    for entry in fs::read_dir(&deployer.log_dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if crate::lever::tree::extension(&path) != ".log" {
            continue;
        }
        let Some(days_ago) = file_days_since_modified(&path) else {
            continue;
        };
        if days_ago == 0 {
            continue; // 今天生成的
        }
        if fs::remove_file(&path).is_err() {
            log_warn(&format!("failed to remove old log file: {}", path.display()));
            success = false;
        }
    }
    success
}

/// 文件修改时间距今多少天（0 = 今天）。
fn file_days_since_modified(path: &Path) -> Option<u64> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let secs = mtime.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((now - secs) / 86_400)
}

/// `ConfigFileUpdate::Run`（C++ deployment_tasks.cc:482-503）：**编译**配置到 staging，
/// 不是"拷文件"。序列（与 C++ 逐条对齐）：
/// 1. `TrashDeprecatedUserCopy`：用户目录根的扁平旧副本（旧布局遗留）移进 `trash/`；
/// 2. 加载已部署产物，`ConfigNeedsUpdate` 比对 `__build_info/timestamps` 与源文件 mtime；
/// 3. 需要更新 → `config::compile` 编译源文件，产物写 `staged_output`（分类路径，
///    `SaveOutputPlugin` 的写侧契约）。
fn run_config_file_update(deployer: &mut Deployer, file_name: &str, version_key: &str) -> bool {
    // 资源 id = file_name 去掉 ".yaml"（C++ 的 config_source_file 类型后缀）
    let Some(resource_id) = file_name.strip_suffix(".yaml") else {
        log_warn(&format!("invalid config file name '{file_name}'"));
        return false;
    };
    let layout = deployer.layout();
    let source = layout.config_source(resource_id);
    if !source.is_file() {
        log_warn(&format!(
            "config source '{}' not found",
            source.display()
        ));
        return false;
    }
    trash_deprecated_user_copy(deployer, file_name, version_key);

    let output = layout.staged_output(resource_id);
    if output.is_file() && !config_needs_update(&output, &layout) {
        log_info(&format!("config file '{file_name}' is up to date"));
        return true;
    }
    if let Some(parent) = output.parent()
        && let Err(err) = fs::create_dir_all(parent)
    {
        log_warn(&format!("cannot create {}: {err}", parent.display()));
        return false;
    }
    // `default` 资源（编译时 `__include`/`__patch` 的基准，schema 靠它拿
    // `default/menu`、`default/navigator`、`default/selector`）。
    let defaults_path = default_config_path(deployer);
    // 共享目录必须始终在解析根里（C++ 的 ConfigCompiler 拿的 resolver 恒为
    // [user_data_dir, shared_data_dir] 双根）。defaults_path 指向用户副本时
    // （真机：default.yaml 在用户目录），compile 的根去重后只剩 user 单根，
    // `symbols`/`luna_pinyin_simp` 等只在共享目录的资源全部解析失败
    // （真机 2026-10-26：default 编译 11 处 "resource could not be loaded: symbols"）。
    match config::compile(&source, &defaults_path, &[deployer.shared_data_dir.clone()]) {
        Ok(compiled) => match write_compiled_schema(&compiled, &output) {
            Ok(()) => {
                log_info(&format!("updated config file '{file_name}' -> {}", output.display()));
                true
            }
            Err(err) => {
                log_warn(&format!("error writing '{}': {err}", output.display()));
                false
            }
        },
        Err(err) => {
            log_warn(&format!("error compiling config '{file_name}': {err}"));
            false
        }
    }
}

/// `ConfigNeedsUpdate`（deployment_tasks.cc:444-480）：已部署产物里记的
/// `__build_info/timestamps`（资源 id → 源文件 mtime）逐项与新源文件比对。
/// 任一项"源文件更新了 / 消失了"→ 需要重新编译；产物没有 build info → 需要。
fn config_needs_update(staged_path: &Path, layout: &super::paths::Layout) -> bool {
    let Ok(value) = load_file(staged_path) else {
        return true;
    };
    let Some(timestamps) = value
        .pointer("/__build_info/timestamps")
        .and_then(|node| node.as_object())
    else {
        log_info("missing build info");
        return true;
    };
    for (resource_id, recorded) in timestamps {
        let recorded_time = recorded.as_i64().unwrap_or(0);
        let source = layout.config_source(resource_id);
        match crate::lever::tree::modified_time(&source) {
            None => {
                if recorded_time != 0 {
                    log_info(&format!("source file no longer exists: {}", source.display()));
                    return true;
                }
            }
            Some(mtime) => {
                if mtime > recorded_time {
                    return true;
                }
            }
        }
    }
    false
}

/// `default` 资源的实际路径：**用户目录优先、共享目录回退**。
///
/// C++ 全程用 `Config::Require("config")->Create("default")`，它走的是
/// `Service::CreateResourceResolver({"config","",".yaml"})` —— 根 = 用户目录、
/// 回退根 = 共享目录（`service.cc:167-172`）。所以：
///
/// * 编译 schema 时的基准配置（`default/menu` 等）要按这个顺序找；
/// * 方案清单的回退来源（`default.yaml` 的 `schema_list`）也是这一份。
///
/// 原来两处都硬写成 `shared_data_dir/default.yaml`，用户改过
/// `%APPDATA%/Rime-Next/default.yaml` 时会被忽略。
fn default_config_path(deployer: &Deployer) -> std::path::PathBuf {
    let user_copy = deployer.user_data_dir.join("default.yaml");
    if user_copy.is_file() {
        user_copy
    } else {
        deployer.shared_data_dir.join("default.yaml")
    }
}

/// `TrashDeprecatedUserCopy`（deployment_tasks.cc:338-372，简化版）：
/// 用户目录根的扁平副本（`<user>/<file_name>`）在共享目录根也有同名**旧版**时移进 trash。
/// 新布局下共享目录根不再放文件，故通常直接返回 false；留着是为旧安装升级时清场。
fn trash_deprecated_user_copy(deployer: &Deployer, file_name: &str, version_key: &str) -> bool {
    let shared_copy = deployer.shared_data_dir.join(file_name);
    let user_copy = deployer.user_data_dir.join(file_name);
    if !shared_copy.is_file() || !user_copy.is_file() {
        return false;
    }
    let shared_version = load_file(&shared_copy)
        .ok()
        .and_then(|value| version_string(&value, version_key));
    let user_version = load_file(&user_copy)
        .ok()
        .and_then(|value| version_string(&value, version_key));
    let (Some(shared_version), Some(user_version)) = (shared_version, user_version) else {
        return false;
    };
    if compare_version_string(&shared_version, &user_version) > 0 {
        let trash = deployer.user_data_dir.join("trash");
        if !crate::lever::tree::maybe_create_directory(&trash) {
            return false;
        }
        if fs::rename(&user_copy, trash.join(file_name)).is_ok() {
            log_warn(&format!(
                "deprecated user copy of '{file_name}' is moved to {}",
                trash.display()
            ));
            return true;
        }
    }
    false
}

/// 读 yaml 的 `version_key` 字符串值（C++ 的 `GetString`）。
fn version_string(value: &serde_json::Value, version_key: &str) -> Option<String> {
    value.get(version_key)?.as_str().map(str::to_string)
}

/// `CompareVersionString`（点分数字逐段比，缺段按 0）。返回 >0 / =0 / <0。
fn compare_version_string(left: &str, right: &str) -> i32 {
    let parse = |text: &str| -> Vec<u64> {
        text.split('.')
            .map(|part| part.trim().parse::<u64>().unwrap_or(0))
            .collect()
    };
    let left = parse(left);
    let right = parse(right);
    for index in 0..left.len().max(right.len()) {
        let l = left.get(index).copied().unwrap_or(0);
        let r = right.get(index).copied().unwrap_or(0);
        if l != r {
            return if l > r { 1 } else { -1 };
        }
    }
    0
}

/// `SchemaUpdate::Run`（C++ deployment_tasks.cc:379-414）：
/// 1. 方案 id 取**文件名主干**——Rime-Next 标准里身份由清单确立
///    （`schema/schema.yaml` 列出的 id = 文件夹名 = 文件名 = 文件内 `schema/id`，
///    准入四步已验一致性），部署不再读内容键取身份；文件里显式写了
///    `schema/id` 时做一致性校验，不一致拒收（旧式 `schema_id:` 不读、
///    迁移由准入与数据迁移负责）；
/// 2. `ConfigFileUpdate(<id>.schema.yaml, schema/version)` —— 编译写 staging；
/// 3. reload 产物读 `translator/dictionary` —— 有词典才需要 `Dictionary::Create`
///    （`.bin` 编译，Rust 留 dict 层接缝）；没有词典的方案到此为止。
fn run_schema_update(deployer: &mut Deployer, source_path: &Path, verbose: bool) -> bool {
    if !source_path.is_file() {
        log_warn(&format!(
            "Error updating schema: nonexistent file '{}'.",
            source_path.display()
        ));
        return false;
    }
    // id = 文件名主干（清单归属；准入已保证与 `schema/id` 一致）
    let Some(schema_id) = source_path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".schema.yaml"))
        .filter(|id| !id.is_empty())
    else {
        log_warn(&format!(
            "invalid schema definition in '{}'.",
            source_path.display()
        ));
        return false;
    };
    // 一致性校验：显式写了 `schema/id` 就必须与文件名一致（load_file 已把
    // 方言单数 `id` 归一化出来；旧式 `schema_id:` 不再参与身份判定）
    if let Some(declared) = load_file(source_path)
        .ok()
        .and_then(|value| {
            value
                .pointer("/schema/id")
                .and_then(|id| id.as_str())
                .map(str::to_string)
        })
        .filter(|id| !id.is_empty())
        && declared != schema_id
    {
        log_warn(&format!(
            "schema id mismatch in '{}': declared '{declared}', file name says '{schema_id}'.",
            source_path.display()
        ));
        return false;
    }
    if verbose {
        log_info(&format!("schema: {schema_id} <- {}", source_path.display()));
    }
    // 1. 编译写 staging（ConfigFileUpdate 的语义就是"编译配置到 staging"）
    let config_file_update = DeploymentTask::ConfigFileUpdate {
        file_name: format!("{schema_id}.schema.yaml"),
        version_key: "schema/version".to_string(),
    };
    if !config_file_update.run(deployer) {
        return false;
    }
    // 2. reload 产物，看要不要编译词典（.bin 在 dict 层，留接缝）
    let layout = deployer.layout();
    let staged = layout.compiled_schema(&schema_id);
    // 要不要编译词典:主 `translator/dictionary`,或任何**变体节**(`simp:` /
    // `trad:` … 顶层带 `dict:`/`dictionary:` 的节,Rime-Next 双变体设计)。
    let has_dictionary = load_file(&staged)
        .ok()
        .and_then(|value| {
            let main = value
                .pointer("/translator/dictionary")
                .and_then(|name| name.as_str())
                .map(str::to_string)
                .is_some_and(|name| !name.is_empty());
            let variants = value.as_object().is_some_and(|map| {
                map.iter()
                    .any(|(key, node)| {
                        key != "translator"
                            && node
                                .get("dict")
                                .or_else(|| node.get("dictionary"))
                                .and_then(|v| v.as_str())
                                .is_some_and(|name| !name.is_empty())
                    })
            });
            Some(main || variants)
        })
        .unwrap_or(false);
    if has_dictionary {
        // Dictionary::Create + DictCompiler::Compile（C++ deployment_tasks.cc:402-429）
        compile_dictionary(deployer, &schema_id, verbose)
    } else {
        log_info(&format!("schema '{schema_id}': not requiring a dictionary"));
        true
    }
}

/// `DictionaryComponent::Create` + `DictCompiler::Compile`
/// （C++ `deployment_tasks.cc:402-429` + `dictionary.cc:425-451`）。
///
/// 配置语义：`translator/dictionary` 必填（空 = 不需要静态词典）；
/// `translator/prism` 缺省 = dict_name；`translator/packs` 是字符串列表。
/// `schema_file` 用 deployed resolver 解出的编译产物（staging 优先、prebuilt 回退），
/// 与 C++ `resolver->ResolvePath(schema_id)` 一致。
///
/// Rime-Next 双变体扩展（2026-10-27 设计）：除主 `translator/dictionary` 外，
/// 顶层任何带 `dict:`/`dictionary:` 键的节（`simp:` / `trad:` …）都是**一个变体
/// 词库**，逐个编译。变体词源缺失时**跳过不报错**——运行时该变体走 opencc
/// 兜底（`<变体>/fallback` + `<变体>/opencc`），这正是"一个方案挂两本词库、
/// 缺哪本 opencc 补"的部署侧配合。
fn compile_dictionary(deployer: &mut Deployer, schema_id: &str, verbose: bool) -> bool {
    let layout = deployer.layout();
    let compiled_schema = layout.compiled_schema(schema_id);
    let Ok(config) = load_file(&compiled_schema) else {
        log_error(&format!(
            "Error loading compiled schema '{}'.",
            compiled_schema.display()
        ));
        return false;
    };

    // 词典清单:主 `translator/dictionary` + 顶层变体节(`simp:` / `trad:` …)
    let mut entries: Vec<(String, String, String, bool)> = Vec::new(); // (节名, dict, prism, 可跳过)
    if let Some(dict_name) = config
        .pointer("/translator/dictionary")
        .and_then(|name| name.as_str())
        .map(str::to_string)
    {
        if dict_name.is_empty() {
            // not requiring a dictionary（C++ Create 直接返回 nullptr 前的早退）
            return true;
        }
        let prism_name = config
            .pointer("/translator/prism")
            .and_then(|name| name.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| dict_name.clone());
        entries.push(("translator".to_string(), dict_name, prism_name, false));
    }
    if let Some(map) = config.as_object() {
        for (key, node) in map {
            if key == "translator" {
                continue; // 主词典上面已处理
            }
            let Some(dict_name) = node
                .get("dict")
                .or_else(|| node.get("dictionary"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
            else {
                continue;
            };
            if dict_name.is_empty() {
                continue;
            }
            let prism_name = node
                .get("prism")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| dict_name.clone());
            entries.push((key.clone(), dict_name, prism_name, true));
        }
    }
    if entries.is_empty() {
        log_error(&format!(
            "translator/dictionary not specified in schema '{schema_id}'."
        ));
        return false;
    }

    if !crate::lever::tree::maybe_create_directory(&deployer.staging_dir) {
        return false;
    }
    let mut all_ok = true;
    for (section, dict_name, prism_name, skippable) in entries {
        // 变体的 packs 暂不扩展(设计里变体词库不带 packs)
        let packs: Vec<String> = if section == "translator" {
            config
                .pointer("/translator/packs")
                .and_then(|list| list.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut compiler = crate::dict::dict_compiler::DictCompiler::new(
            dict_name.clone(),
            prism_name.clone(),
            packs,
            &deployer.user_data_dir,
            &deployer.shared_data_dir,
            &deployer.staging_dir,
            &deployer.prebuilt_data_dir,
        );
        if verbose {
            // C++：verbose_ → kRebuild | kDump（kDump 未移植，只留 kRebuild）
            compiler.set_options(crate::dict::dict_compiler::CompileOptions::rebuild_all());
        }
        match compiler.compile(Some(&compiled_schema)) {
            Ok(()) => {
                log_info(&format!("dictionary '{dict_name}' is ready."));
            }
            Err(error) => {
                if skippable && dict_source_missing(&error) {
                    log_info(&format!(
                        "variant dictionary '{dict_name}' ({section}) has no source; \
                         opencc fallback at runtime."
                    ));
                    continue;
                }
                log_error(&format!(
                    "dictionary '{dict_name}' failed to compile: {error}"
                ));
                all_ok = false;
            }
        }
    }
    all_ok
}

/// 词典编译错误是否属于"词源文件不存在"(变体词库缺失的预期形态)。
fn dict_source_missing(error: &str) -> bool {
    // 两种形态:"source file 'X' does not exist." 与
    // "neither X.dict.yaml nor X.table.bin exists."
    error.contains("not exist") || error.contains("nor ") && error.contains("exists")
}

/// 编译产物落盘（目录自动建；YAML 序列化用 config 层的 emit）。
///
/// 对 `.schema.yaml` 产物按**标准写法**落盘：
/// 1. 头部注释固定为 Rime-Next 标准三行（[`SCHEMA_HEADER`]）；
/// 2. `schema.schema_id`（编译期 librime 复数键）反归一化为单数 `schema.id`
///    （见 [`revert_schema_id`]，与 `normalize_dialect` 的上下文限制一致）。
/// 其余配置（default.yaml 等）原样序列化：不加头、不动键。
fn write_compiled_schema(item: &Item, output: &std::path::Path) -> Result<(), String> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("create_dir_all: {err}"))?;
    }
    let mut yaml = config::to_yaml_string(item).map_err(|err| format!("to_yaml_string: {err}"))?;
    if output
        .file_name()
        .map_or(false, |name| name.to_string_lossy().ends_with(".schema.yaml"))
    {
        yaml = revert_schema_id(yaml);
        yaml = format!("{SCHEMA_HEADER}\n{yaml}");
    }
    fs::write(output, yaml).map_err(|err| format!("write: {err}"))
}

/// Rime-Next 方案文件的标准头注释（与 `pinyin.schema.yaml` 等手写源一致）。
const SCHEMA_HEADER: &str = "# Rime Schema\n# encode: utf-8\n# newline: lf";

/// 把编译产物里 librime 的复数键 `schema_id` 反归一化为标准单数 `id`。
///
/// 只作用于 `schema:` 顶层块（与 `normalize_dialect` 的上下文限制一致：
/// `id`→`schema_id` 仅在 `schema` 块内发生），其余键一律不动。
fn revert_schema_id(yaml: String) -> String {
    let mut in_schema = false;
    let mut out = String::with_capacity(yaml.len());
    for line in yaml.lines() {
        let stripped = line.trim_start();
        let indent = line.len() - stripped.len();
        if indent == 0 && stripped.trim_end() == "schema:" {
            in_schema = true;
        } else if indent == 0 && !stripped.is_empty() && !stripped.starts_with('#') {
            // 进入下一个顶层块，退出 schema 作用域
            in_schema = false;
        }
        if in_schema && stripped.starts_with("schema_id:") {
            if let Some(pos) = line.find("schema_id:") {
                out.push_str(&line[..pos]);
                out.push_str("id:");
                out.push_str(&line[pos + "schema_id:".len()..]);
                out.push('\n');
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// `WorkspaceUpdate::Run`（C++ deployment_tasks.cc:169-292 的移植）。
///
/// 序列：`config_file_update(default.yaml)` → schema 清单 → 逐方案编译 → dependencies
/// 递归 → 把清单写回 `staging/default.yaml` 的 `schema_list`。
/// `SymlinkingPrebuiltDictionaries`（链接出厂预编译词典）**未移植**，记日志跳过。
fn run_workspace_update(deployer: &mut Deployer) -> bool {
    log_info("updating workspace.");
    // 1. 出厂 default.yaml 补齐到用户目录
    let default_update = DeploymentTask::ConfigFileUpdate {
        file_name: "default.yaml".to_string(),
        version_key: "config_version".to_string(),
    };
    default_update.run(deployer); // C++ 同样忽略返回值

    // 2. SymlinkingPrebuiltDictionaries —— 未移植（预编译词典符号链接），跳过
    log_info("[Weasel-Next] SymlinkingPrebuiltDictionaries 未移植，跳过");

    // 3. 方案清单：`<user>/schema/schema.yaml`（点名制）
    //
    // 2026-10-28：清单**缺失就生成** —— 识别程序文件夹（shared）里过准入四关的方案写进去。
    // `default.yaml` 的 `schema_list` **不再当兜底**（用户拍板：直接生成，不过 default）：
    // default 那份是出厂预设，而清单是用户的选择，二者不该混着用。
    let outcome =
        super::schema_list::ensure_manifest(&deployer.user_data_dir, &deployer.shared_data_dir);
    if outcome.generated() {
        log_info(&format!(
            "[Weasel-Next] 方案清单缺失，已由程序文件夹识别生成：{} 项（active = {}）",
            outcome.manifest().schemas.len(),
            outcome.manifest().active.as_deref().unwrap_or("<无>")
        ));
    }
    let manifest = load_schema_manifest(&deployer.user_data_dir);
    let mut schema_ids = manifest.schemas.clone();
    if schema_ids.is_empty() {
        log_warn("schema list not defined; 程序文件夹里没识别到任何过准入检查的方案。");
        return false;
    }
    log_info(&format!(
        "[Weasel-Next] 方案清单：{} 项（schema/schema.yaml 为唯一权威）",
        schema_ids.len()
    ));

    // 4. 逐方案编译 + dependencies 递归（C++ 的 build_schema lambda）
    let layout = deployer.layout();
    let mut built: Vec<String> = Vec::new();
    let mut success = 0usize;
    let mut failure = 0usize;
    let mut queue: Vec<(String, bool)> = schema_ids.iter().map(|id| (id.clone(), false)).collect();
    while let Some((schema_id, as_dependency)) = queue.pop() {
        if built.contains(&schema_id) {
            continue;
        }
        built.push(schema_id.clone());
        let source = layout.schema_source(&schema_id);
        if !source.is_file() {
            if as_dependency {
                log_warn(&format!(
                    "missing input schema; skipped unsatisfied dependency: {schema_id}"
                ));
            } else {
                log_warn(&format!("missing input schema: {schema_id}"));
                failure += 1;
            }
            continue;
        }
        let task = DeploymentTask::SchemaUpdate {
            source_path: source,
            // C++ `SchemaUpdate` 构造器恒 verbose_=false（`deployment_tasks.cc:308`，
            // 连 initializer 都不收 verbose 参数）：workspace/prebuild 两条路都不强制
            // 重建，增量由 DictCompiler 的 checksum 比对决定。
            verbose: false,
        };
        if task.run(deployer) {
            success += 1;
        } else {
            failure += 1;
            continue;
        }
        // 编译成功后才读 dependencies（C++ 用 schema_component->Create 重读一遍，
        // 这里直接读 staging 产物，语义等价且少一次 IO）
        let staged = layout.compiled_schema(&schema_id);
        if let Ok(value) = load_file(&staged) {
            for dep in dependencies_from_json(&value) {
                if !built.contains(&dep) {
                    queue.push((dep, true));
                }
            }
        }
    }
    log_info(&format!(
        "finished updating schemas: {success} success, {failure} failure."
    ));

    // 5. schema_list 写回 staging/default.yaml（清单唯一落点是 schema/schema.yaml，
    //    但 librime 的菜单读 build/default.yaml 的 schema_list，所以两处都要有）
    if !schema_ids.is_empty()
        && let Some(mut value) = load_file(&default_config_path(deployer)).ok()
    {
            let list: Vec<serde_json::Value> = schema_ids
                .iter()
                .map(|id| serde_json::json!({ "schema": id }))
                .collect();
            if let Some(map) = value.as_object_mut() {
                map.insert("schema_list".to_string(), serde_json::Value::Array(list));
            }
            let output = deployer.staging_dir.join("default.yaml");
            if let Some(parent) = output.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match config::to_yaml_string(&config::data::convert_from_yaml(&value, None)) {
                Ok(yaml) => {
                    if fs::write(&output, yaml).is_ok() {
                        log_info("[Weasel-Next] schema_list 已按 schema.yaml 写回 staging/default.yaml");
                    }
                }
                Err(err) => log_warn(&format!("cannot serialize staging default.yaml: {err}")),
            }
        }
    failure == 0
}

/// 方案清单（**点名制**）：`<user_data_dir>/schema/schema.yaml`。
///
/// ```yaml
/// # 方案信息 —— 勾选清单 + 当前使用的方案
/// active: double_pinyin_ziranma
/// schemas:
///   - luna_pinyin_simp
///   - double_pinyin_ziranma
/// ```
///
/// 2026-10-26：**TOML 已彻底移除**。之前这里读 `schema/schema.toml`
/// （对应 C++ 补丁 `toml_lite.cc` 的 `LoadSchemaIdsFromToml`），用户拍板 toml 不好用、
/// 全仓库不认，于是改成 YAML，语义与字段一字未变，只是换了格式。
#[derive(Debug, Default, PartialEq)]
pub struct SchemaManifest {
    /// 当前在用方案（`active`），没有就是 `None`。
    pub active: Option<String>,
    /// 要部署的方案 id（**顺序 = 菜单顺序**）。
    pub schemas: Vec<String>,
}

impl SchemaManifest {
    pub fn is_empty(&self) -> bool {
        self.schemas.is_empty()
    }
}

/// 读 `<user_data_dir>/schema/schema.yaml`；文件不存在 / 解析失败都返回空清单
/// （调用方据此回退 `default.yaml` 的 `schema_list`）。
pub fn load_schema_manifest(user_data_dir: &std::path::Path) -> SchemaManifest {
    let path = user_data_dir
        .join(crate::lever::paths::SCHEMA_DIR)
        .join(crate::lever::paths::SCHEMA_MANIFEST);
    let Ok(value) = crate::load_file(&path) else {
        return SchemaManifest::default();
    };
    let active = value
        .get("active")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let schemas = value
        .get("schemas")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| match item {
                    // 容忍两种写法：裸 id，或 `schema: id`（与 default.yaml 的 schema_list 同构）
                    serde_json::Value::String(id) => Some(id.clone()),
                    other => other.get("schema").and_then(|s| s.as_str()).map(str::to_string),
                })
                .collect()
        })
        .unwrap_or_default();
    SchemaManifest { active, schemas }
}

/// 从 `default.yaml` 的配置树里取 `schema_list`。
///
/// 2026-10-28：部署**不再**拿它当清单兜底（清单缺失时直接由程序文件夹识别生成，
/// 见 `schema_list::ensure_manifest`）。留着是因为第 5 步还要把清单写回
/// `staging/default.yaml` 的 `schema_list`，以及外部工具可能读这份出厂预设。
fn schema_list_from_json(value: &serde_json::Value) -> Vec<String> {
    value
        .get("schema_list")
        .and_then(|list| list.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("schema").and_then(|s| s.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// 从已编译产物（JSON 视图）取 `schema/dependencies` 的 id 序列。
fn dependencies_from_json(value: &serde_json::Value) -> Vec<String> {
    value
        .pointer("/schema/dependencies")
        .and_then(|list| list.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// 沿 `path` 逐层下钻，取末端的字符串列表（读 `schema/dependencies` 用）。
///
/// `Item` 的取值器是 `pub(super)`，lever 侧用公开的 `as_map` + `borrow()` 组合走等价的路。
fn string_list_at(item: &Item, path: &[&str]) -> Vec<String> {
    let mut current = item.clone();
    for key in path {
        let Item::Map(map) = &current else {
            return Vec::new();
        };
        let Some(next) = map.borrow().get(*key).cloned() else {
            return Vec::new();
        };
        current = next;
    }
    let Item::List(list) = &current else {
        return Vec::new();
    };
    list.borrow()
        .iter()
        .filter_map(|entry| match entry {
            Item::Scalar(s) if !s.is_empty() => Some(s.clone()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_name_round_trips() {
        for name in DeploymentTask::NAMES {
            let task = DeploymentTask::from_name(name).unwrap_or_else(|| panic!("{name} 建不出来"));
            assert_eq!(task.name(), name, "名字表与 from_name 不一致: {name}");
        }
        assert_eq!(DeploymentTask::NAMES.len(), 11, "levers_module.cc 注册 11 个");
    }

    #[test]
    fn unknown_name_is_none() {
        // C++ 里不认识的名字记 `unknown deployment task` 并返回 false
        assert!(DeploymentTask::from_name("nope").is_none());
        assert!(DeploymentTask::from_name("").is_none());
    }

    #[test]
    fn all_eleven_tasks_are_now_implemented() {
        let mut deployer = Deployer::new();
        // 2026-10-25：11 个任务全部落地。C++ 的语义里，这三个在「没有对应组件」
        // 时都返回 **true**（nothing to upgrade / 空列表 / log_dir 为空），
        // 而不是失败 —— 这正是 `RimeDeployWorkspace()` 的 `&&` 链能走完的前提。
        assert!(
            DeploymentTask::UserDictUpgrade.run(&mut deployer),
            "没有 legacy userdb 组件时，C++ 语义是成功"
        );
        assert!(
            DeploymentTask::UserDictSync.run(&mut deployer),
            "没有 userdb 组件时，C++ 语义是成功"
        );
        assert!(
            DeploymentTask::CleanOldLogFiles.run(&mut deployer),
            "log_dir 为空时，C++ 语义是成功"
        );
    }

    #[test]
    fn schema_manifest_reads_yaml_point_list() {
        let root = std::env::temp_dir().join(format!("manifest-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("schema")).unwrap();
        std::fs::write(
            root.join("schema").join("schema.yaml"),
            r#"# 方案信息 —— 勾选清单 + 当前使用的方案
active: double_pinyin_ziranma
schemas:
  - luna_pinyin_simp
  - double_pinyin_ziranma
  - yuling.five
"#,
        )
        .unwrap();

        let manifest = load_schema_manifest(&root);
        assert_eq!(manifest.active.as_deref(), Some("double_pinyin_ziranma"));
        assert_eq!(
            manifest.schemas,
            vec![
                "luna_pinyin_simp".to_string(),
                "double_pinyin_ziranma".to_string(),
                "yuling.five".to_string()
            ]
        );

        // 空目录 → 空清单（调用方据此回退 default.yaml）
        let empty = std::env::temp_dir().join(format!("manifest-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();
        let manifest = load_schema_manifest(&empty);
        assert!(manifest.is_empty());
        assert_eq!(manifest.active, None);

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[test]
    fn toml_is_not_recognized_anymore() {
        // 2026-10-26：TOML 彻底移除。磁盘上就算有 schema.toml 也不该被读进来。
        let root = std::env::temp_dir().join(format!("manifest-toml-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("schema")).unwrap();
        std::fs::write(
            root.join("schema").join("schema.toml"),
            r#"active = "luna"
schemas = ["a", "b"]
"#,
        )
        .unwrap();
        assert!(load_schema_manifest(&root).is_empty(), "toml 不应被识别");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn config_file_update_compiles_to_staging_with_mtime_check() {
        let root = std::env::temp_dir().join(format!("cfu-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let shared = root.join("shared");
        let user = root.join("user");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        // 出厂 default.yaml（未归类配置：shared 根 → staging 根，都不套系列夹）
        std::fs::write(
            shared.join("default.yaml"),
            "config_version: 2\nschema_list: []\n",
        )
        .unwrap();

        let mut deployer = Deployer::with_data_dirs(&shared, &user);
        let task = DeploymentTask::ConfigFileUpdate {
            file_name: "default.yaml".to_string(),
            version_key: "config_version".to_string(),
        };
        // 首刷：编译产物落 staging/default.yaml（**不是**拷贝到用户目录根）
        assert!(task.run(&mut deployer));
        let staged = user.join("build/default.yaml");
        assert!(staged.is_file(), "产物应落 staging/default.yaml");
        assert!(
            !user.join("default.yaml").exists(),
            "用户目录根不该有副本（零扁平契约）"
        );

        // 再跑：源文件没动 → up to date，不重写
        let first = std::fs::read_to_string(&staged).unwrap();
        assert!(task.run(&mut deployer));
        assert_eq!(
            std::fs::read_to_string(&staged).unwrap(),
            first,
            "无变化不该重写产物"
        );

        // 源文件变新（mtime 推进 + 内容变化）→ 重新编译。
        // 注意：__build_info/timestamps 与 modified_time 都是**秒级**精度，
        // sleep 必须跨过秒边界才能触发（C++ 同样如此）。
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(
            shared.join("default.yaml"),
            "config_version: 3\nschema_list: []\n",
        )
        .unwrap();
        assert!(task.run(&mut deployer));
        let second = std::fs::read_to_string(&staged).unwrap();
        assert!(second.contains("3"), "源更新后产物应跟进: {second}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `default` 资源**用户目录优先、共享目录回退**（C++ 的
    /// `Config::Require("config")->Create("default")` 就是这么解析的）。
    /// 原来两处都硬写 `shared_data_dir/default.yaml`，用户改过的那份会被忽略。
    #[test]
    fn user_default_yaml_wins_over_shared_copy() {
        let root = std::env::temp_dir().join(format!("default-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let shared = root.join("shared");
        let user = root.join("user");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(
            shared.join("default.yaml"),
            "config_version: 2\nmenu:\n  page_size: 5\n",
        )
        .unwrap();
        let deployer = Deployer::with_data_dirs(&shared, &user);

        // 只有共享目录那份 ⇒ 回退到它
        assert_eq!(default_config_path(&deployer), shared.join("default.yaml"));

        // 用户目录也有一份 ⇒ 用户那份说了算
        std::fs::write(
            user.join("default.yaml"),
            "config_version: 2\nmenu:\n  page_size: 9\n",
        )
        .unwrap();
        assert_eq!(default_config_path(&deployer), user.join("default.yaml"));

        // 编译产物必须反映用户那份（page_size 9），否则就是又读回了共享目录
        let mut deployer = Deployer::with_data_dirs(&shared, &user);
        assert!(
            DeploymentTask::ConfigFileUpdate {
                file_name: "default.yaml".to_string(),
                version_key: "config_version".to_string(),
            }
            .run(&mut deployer)
        );
        let staged = std::fs::read_to_string(user.join("build/default.yaml")).unwrap();
        assert!(
            staged.contains("9"),
            "staging 应来自用户目录那份 default.yaml：{staged}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn string_list_at_walks_nested_maps() {        use crate::config::convert_from_yaml;
        let value: serde_json::Value = serde_json::json!({
            "schema": {
                "dependencies": ["dep_a", "dep_b", ""],
                "other": 1
            }
        });
        let item = convert_from_yaml(&value, None);
        assert_eq!(
            string_list_at(&item, &["schema", "dependencies"]),
            vec!["dep_a".to_string(), "dep_b".to_string()],
            "空字符串应被过滤"
        );
        assert!(string_list_at(&item, &["schema", "nope"]).is_empty());
        assert!(string_list_at(&item, &["nope", "x"]).is_empty());
        // 路径尽头是标量不是列表 → 空
        assert!(string_list_at(&item, &["schema", "other"]).is_empty());
    }

    #[test]
    fn installation_update_creates_then_keeps_id() {
        let root = std::env::temp_dir().join(format!("inst-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let shared = root.join("shared");
        let user = root.join("user");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        let mut deployer = Deployer::with_data_dirs(&shared, &user);
        deployer.distribution_code_name = "Weasel-Next".to_string();
        deployer.distribution_version = "0.1".to_string();
        let task = DeploymentTask::InstallationUpdate;

        // 首刷：生成 installation_id，user_id 被同步
        assert!(task.run(&mut deployer));
        let info = user.join("installation.yaml");
        assert!(info.is_file(), "installation.yaml 应被创建");
        let first_id = crate::lever::tree::get_string(&crate::lever::tree::load(&info).unwrap(), "installation_id")
            .expect("应有 installation_id");
        assert_eq!(deployer.user_id, first_id, "user_id 应同步为 installation_id");
        assert_eq!(
            deployer.sync_dir,
            user.join("sync"),
            "sync_dir 缺省是 <user>/sync"
        );

        // 再跑：id 不变、文件不被重写（写 update_time 但保留 id）
        let before = std::fs::read_to_string(&info).unwrap();
        assert!(task.run(&mut deployer));
        let after = std::fs::read_to_string(&info).unwrap();
        let second_id = crate::lever::tree::get_string(&crate::lever::tree::load(&info).unwrap(), "installation_id")
            .expect("id 应保留");
        assert_eq!(first_id, second_id, "重复部署不得换 id");
        assert!(after.contains("update_time") || after == before, "要么写 update_time 要么原样");

        // sync_dir 从文件读回（C++ 的 for now 语义）
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cleanup_trash_moves_debris_and_keeps_normal_files() {
        let root = std::env::temp_dir().join(format!("trash-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let shared = root.join("shared");
        let user = root.join("user");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        // 部署垃圾 + 正常文件
        std::fs::write(user.join("rime.log"), "log").unwrap();
        std::fs::write(user.join("luna_pinyin.table.bin"), "bin").unwrap();
        std::fs::write(user.join("a.userdb.kct.snapshot"), "snap").unwrap();
        std::fs::write(user.join("user.yaml"), "keep me").unwrap();
        std::fs::create_dir_all(user.join("schema")).unwrap();

        let mut deployer = Deployer::with_data_dirs(&shared, &user);
        assert!(DeploymentTask::CleanupTrash.run(&mut deployer));

        let trash = user.join("trash");
        assert!(trash.join("rime.log").is_file(), "rime.log 应进 trash");
        assert!(trash.join("luna_pinyin.table.bin").is_file(), ".bin 应进 trash");
        assert!(
            trash.join("a.userdb.kct.snapshot").is_file(),
            ".snapshot 应进 trash"
        );
        assert!(user.join("user.yaml").is_file(), "正常文件必须留在原地");
        assert!(user.join("schema").is_dir(), "目录必须留在原地");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prebuild_all_schemas_scans_series_layout_recursively() {
        let root = std::env::temp_dir().join(format!("prebuild-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let shared = root.join("shared");
        let user = root.join("user");
        // 最小 default.yaml（compile 的 defaults）
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("default.yaml"), "schema_list: []\n").unwrap();
        std::fs::create_dir_all(&user).unwrap();
        // 一层方案
        std::fs::create_dir_all(shared.join("schema/aaa")).unwrap();
        std::fs::write(
            shared.join("schema/aaa/aaa.schema.yaml"),
            "schema:\n  schema_id: aaa\n",
        )
        .unwrap();
        // 系列成员（两级）
        std::fs::create_dir_all(shared.join("schema/bbb/ccc")).unwrap();
        std::fs::write(
            shared.join("schema/bbb/ccc/bbb.ccc.schema.yaml"),
            "schema:\n  schema_id: bbb.ccc\n",
        )
        .unwrap();
        // 摆错的：文件夹名 != 文件名主干 → 任何 ID 都解析不到，必须跳过
        std::fs::create_dir_all(shared.join("schema/ddd")).unwrap();
        std::fs::write(
            shared.join("schema/ddd/wrong.schema.yaml"),
            "schema:\n  schema_id: ddd\n",
        )
        .unwrap();

        let mut deployer = Deployer::with_data_dirs(&shared, &user);
        assert!(DeploymentTask::PrebuildAllSchemas.run(&mut deployer));

        let staging = user.join("build");
        assert!(
            staging.join("aaa/aaa.schema.yaml").is_file(),
            "一层方案应有产物"
        );
        assert!(
            staging.join("bbb/ccc/bbb.ccc.schema.yaml").is_file(),
            "系列成员应有产物（递归扫描 + 系列分层写侧）"
        );
        assert!(
            !staging.join("ddd").exists(),
            "folder!=stem 的文件不该被编译"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_modifications_compares_yaml_mtime_with_last_build_time() {
        let root = std::env::temp_dir().join(format!("detect-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let shared = root.join("shared");
        let user = root.join("user");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        // user.yaml 记录 last_build_time = 现在
        let now = crate::lever::tree::now();
        std::fs::write(
            user.join("user.yaml"),
            format!("var:\n  last_build_time: {now}\n"),
        )
        .unwrap();
        let mut deployer = Deployer::with_data_dirs(&shared, &user);

        // 无改动：user.yaml 自身被排除，其余没有更新的 yaml → false
        let task = DeploymentTask::DetectModifications {
            data_dirs: vec![shared.clone(), user.clone()],
        };
        assert!(!task.run(&mut deployer), "没有改动应返回 false");

        // data_dirs 为空 → 默认扫用户+共享目录
        assert!(!DeploymentTask::DetectModifications {
            data_dirs: Vec::new(),
        }
        .run(&mut deployer));

        // 放一个比 last_build_time 新的 yaml（秒级精度，sleep 跨秒边界）
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(user.join("custom.yaml"), "k: 1\n").unwrap();
        assert!(
            task.run(&mut deployer),
            "检测到改动应返回 true（start_maintenance 的门）"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn backup_config_files_respects_crc_and_customization() {        let root = std::env::temp_dir().join(format!("backup-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let shared = root.join("shared");
        let user = root.join("user");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(user.join("a.yaml"), "k: 1\n").unwrap();
        std::fs::write(user.join("b.txt"), "text\n").unwrap();
        std::fs::write(user.join("c.userdb"), "binary-ish").unwrap(); // 不是 yaml/txt
        // 用户改过的出厂副本：带 customization 键 → 跳过
        std::fs::write(
            user.join("d.yaml"),
            "customization: 20261025\nk: 2\n",
        )
        .unwrap();

        let mut deployer = Deployer::with_data_dirs(&shared, &user);
        assert!(DeploymentTask::BackupConfigFiles.run(&mut deployer));

        let backup_dir = user.join("sync/unknown");
        assert!(backup_dir.join("a.yaml").is_file(), "yaml 应备份");
        assert!(backup_dir.join("b.txt").is_file(), "txt 应备份");
        assert!(!backup_dir.join("c.userdb").exists(), "非 yaml/txt 不备份");
        assert!(
            !backup_dir.join("d.yaml").exists(),
            "customized copy 不备份"
        );

        // 再跑一次：CRC 相同 → latest，不报错
        assert!(DeploymentTask::BackupConfigFiles.run(&mut deployer));
        let _ = std::fs::remove_dir_all(&root);
    }
}
