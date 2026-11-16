//! `gear`：一次按键的核心链路（处理器 / 翻译器 / 选择器 / 段 / 过滤器 / 神经 / 字典）。
//!
//! 按角色拆分到子模块：
//! * `schema`     —— 方案（编译后的 Config 树）+ 方案开关 + 方案级列表翻译器
//! * `processor`  —— 按键处理器（ascii / speller / editor / navigator / selector / ...）
//! * `segmentor`  —— 输入串分段
//! * `translator` —— 翻译器
//! * `filter`     —— 候选过滤器
//! * `neural`     —— 神经网络（预测 / 重排 / 学习头）
//! * `dict`       —— 数据层（词典 / prism / userdb / 编译）
//!
//! 跨角色特性 `lua` / `poet` / `shape` / `recognizer` / `registry` 留在根。

pub mod schema;
pub mod processor;
pub mod segmentor;
pub mod translator;
pub mod filter;
pub mod neural;
pub mod dict;

pub mod lua;
pub mod poet;
pub mod shape;
pub mod recognizer;
pub mod registry;
