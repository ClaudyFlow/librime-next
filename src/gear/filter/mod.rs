//! 候选过滤器（Filter 链路）。

pub mod charset_filter;
pub mod filter;
pub mod partial_commit;
pub mod punctuator;
pub mod reverse_lookup_filter;
pub mod rescore;
pub mod simplifier;

// 同名叶子模块 `filter` 的导出项在 `gear/filter/filter.rs`，再导出一层。
pub use filter::{Filter, SingleCharFilter, Uniquifier};
