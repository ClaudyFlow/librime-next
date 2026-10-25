//! `src/rime/segmentor.h`：分段器。
//!
//! 与 [`crate::engine::gear::processor::Processor`] 的区别：processor 消费**按键**，
//! segmentor 消费**已经成形的输入串**，把 `[start, end)` 区间切成若干段并打标签。
//!
//! C++ 的契约：
//! ```cpp
//! virtual bool Proceed(Segmentation* segmentation) = 0;
//! ```
//! 返回 `true` = "我处理完了，**下一个 segmentor 请继续这一轮**"；
//! 返回 `false` = "到此为止"（`CalculateSegmentation` 的 `for` 会 `break`）。

use crate::engine::segmentation::Segmentation;

/// 分段器能看到的那一点上下文（C++ 里 segmentor 持有 `Engine*`，直接读选项）。
///
/// lrn 的 segmentor 列表在 `calculate_segmentation` 里被 `mem::take` 出来遍历，
/// 此时拿不到 `&Engine`（会和 `&mut segmentation` 打架），所以把需要的开关
/// **挑出来**传进去 —— 目前只有 `ascii_mode` 一个。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SegmentorView {
    pub ascii_mode: bool,
}

pub trait Segmentor {
    fn proceed(&self, segmentation: &mut Segmentation, view: SegmentorView) -> bool;
}
