//! 输入串分段（Segmentor 链路）。

pub mod abc_segmentor;
pub mod affix_segmentor;
pub mod ascii_segmentor;
pub mod auxiliary_matcher;
pub mod auxiliary_suffix_segmentor;
pub mod fallback_segmentor;
pub mod matcher;
pub mod segmentor;
// NOTE: 早期的合集草稿 `segmentors.rs`（trait 签名与正式版不一致）已于 2026-11-16 删除，
// 内容被 `segmentor.rs`（trait）+ `abc/ascii/fallback_segmentor.rs` 取代，见 git 历史。

// 同名叶子模块 `segmentor` 的导出项在 `gear/segmentor/segmentor.rs`，再导出一层。
pub use segmentor::{Segmentor, SegmentorView};
