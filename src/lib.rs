//! `librime-next` —— librime 的 Rust 替代品。
//!
//! ## 为什么这样分模块
//!
//! 目录结构**刻意和 C++ 的 `librime/src/rime/` 一一对应**，这样两边可以逐文件对照着读，
//! 移植进度也能按目录说清楚（"哪个目录换完了"）。
//!
//! | C++ 目录 | 行数 | Rust 模块 | 状态 | 依赖 | 可验证方式 |
//! |---|---|---|---|---|---|
//! | `src/rime/config/` | 2,516 | [`config`] | **已移植** | 只有 YAML | 产物 `build/<方案>.schema.yaml` 可逐份对拍 |
//! | `src/rime/algo/` | 1,829 | [`algo`] | **已移植** | 无（纯算法） | C++ 测试逐条对拍 + 真实方案样例 |
//! | `src/rime/dict/` | 7,372 | [`dict`] | **已移植** | leveldb / marisa | `.bin` 读侧 + 编译链 231 测试全过 |
//! | `src/rime/lever/` | 1,968 | [`lever`] | **已移植** | config + dict | 部署任务序列对拍，440s 测试通过 |
//! | `src/rime/gear/` | 9,560 | [`engine::gear`] | **已移植** | engine + dict | 组件名 **35/35 全覆盖**（含并击输入两套实现）；出厂 124 个方案引用的组件 472/472 |
//! | `src/rime/`（顶层） | ~1,700 | [`engine`] | **已移植（MVP）** | 全部 | `RimeSetup→ProcessKey→GetContext→Select→Commit` 端到端 |
//! | `src/rime_api.{h,cc}` | — | [`c_api`] | **已移植（MVP）** | engine | `rime_get_api()` 导出 `RimeApi`，字段逐字对齐 |
//!
//! ## 为什么从 `config` 开始
//!
//! 换掉它**不需要碰 C++、不需要重编 rime.dll**：接缝是**文件**而不是函数。
//!
//! ```text
//! 输入：schema/<方案>/<方案>.schema.yaml（＋ default.yaml 与各预设）
//!   ↓  ← 这一层换成 Rust
//! 输出：build/<方案>/<方案>.schema.yaml
//!   ↓
//! 引擎照旧读产物、照旧用同一份 rime.dll
//! ```
//!
//! 而且 30 份现成的编译产物已经备份成夹具（`fixture/build-20261023/`），每一步都能对拍。

pub mod algo;
pub mod c_api;
pub mod config;
pub mod dict;
pub mod engine;
pub mod lever;

/// 读一份 YAML 成树。
///
/// **实现已归位到 config 层**（[`config::load::load_file`]）：块标量、Rime 词典的
/// `...` 约定都是配置层的私有知识，不属于 crate 顶层。这里只保留一层 re-export，
/// 免得外部调用点（`example/algo_real.rs` 等）跟着改。
pub use config::load::load_file;
