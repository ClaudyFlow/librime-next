//! `src/rime/lever/`（1,968 行）：部署任务（`rime deploy` 干的事）。依赖 config + dict。
//!
//! | C++ 文件 | 行数 | Rust 模块 | 状态 |
//! |---|---|---|---|
//! | `deployer.h`(79) + `deployer.cc`(154) | 233 | [`deployer`] | ✅ 任务调度器 |
//! | `resource.cc` 里 lever 用到的那部分 | ~100 | [`paths`] | ✅ 路径解析（`config/resource.rs` 只公开了构造，故按同一规则重写；等 resource 层公开后可删） |
//! | `installation.yaml` / `user.yaml` / `*.custom.yaml` 读写 | — | [`tree`] | ✅ 数据文件读写层（用 config 的公开入口拼出来的最小实现） |
//! | `deployment_tasks.cc` | 749 | [`deployment_tasks`] | ◐ 8/11 任务落地（installation/workspace/schema/config_file/cleanup_trash/prebuild_all/backup_config/detect_modifications）；剩 user_dict_upgrade / user_dict_sync / clean_old_log_files |
//! | `user_dict_manager.cc` | 226 | [`user_dict_manager`] | ◐ 接口层在，等 `dict` 的 `UserDb` |
//! | `customizer.cc` / `custom_settings.cc` / `switcher_settings.cc` | 370 | — | ❌ 未移植 |
//! | `levers_module.cc` / `levers_stdbool.cc` | 50 | — | ❌ 未移植（组件注册） |
//!
//! ## 现在能做什么、不能做什么
//!
//! * **能**：建一个 `Deployer`、注册任务、按名字调度并统计结果（`deployer`）；
//!   按 Weasel-Next 的分目录规则解析资源路径（`paths`）；读/改/写 `installation.yaml` 这类文件（`tree`）；
//!   **跑 `deploy` 主链的 config 侧**：installation_update → workspace_update
//!   （`schema/schema.yaml` 点名清单 → 逐方案 `config::compile` → dependencies 递归 → schema_list 写回
//!   `staging/default.yaml`）→ cleanup_trash（`deployment_tasks`）。
//! * **不能**：`.bin`（prism/table）编译与 userdb 同步 —— 卡在 `dict` 层缺口
//!   （`UserDb` / `SymDeleteCollector` / `NearSearchCorrector`），已留 trait 接缝。
//!
//! 2026-10-24：这三个模块由子代理写出后**没有接进编译**（`lever.rs` 还是空壳、没有 `pub mod`），
//! 现已接上并纳入 `cargo test`。
//! 2026-10-25：deployment_tasks 的 8 个任务本体落地，集成测试 `test/lever_deploy.rs`
//! 用真实数据目录跑通 `WorkspaceUpdate`（12 个方案全部产出 `staging/<id>/<id>.schema.yaml`）。
//! `deploy` 主链四步通三步，只剩 `user_dict_upgrade`（等 `dict` 的 `UserDb`）；
//! `clean_old_log_files` 依赖 glog 日志目录机制，Rust 侧无对应物。

#![allow(dead_code)]

pub mod admit;
// pub mod compile_graph; // 并行部署:编译错未修完,下个班次继续
pub mod deployer;
pub mod deployment_tasks;
pub mod paths;
pub mod scan;
pub mod schema_list;
pub mod tree;
pub mod user_dict_manager;
