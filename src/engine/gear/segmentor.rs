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
//!
//! # 为什么 `proceed` 还要收一份提交历史
//!
//! C++ 的 segmentor 持有 `Engine*`，`PunctSegmentor::Proceed` 就是这么读
//! `engine_->context()->commit_history()` 的 —— 它要靠"上一条提交是不是数字"来决定
//! 把末尾的 `.` / `:` 打成 `punct` 还是 `punct_number` 标签（`3.14` 与 `3。`）。
//!
//! lrn 的 segmentor 列表在 `calculate_segmentation` 里被 `mem::take` 出来遍历，
//! 此时拿不到 `&Engine`，所以改成**显式传参**。`parity` 上等价于 C++ 的那次
//! `context()->commit_history()`。

use crate::engine::commit_history::CommitHistory;
use crate::engine::segmentation::Segmentation;

/// 分段器能看到的那一点上下文（C++ 里 segmentor 持有 `Engine*`，直接读选项）。
///
/// 目前只有两个开关：
/// * `ascii_mode` —— `ascii_segmentor` 用；
/// * `full_shape` —— `punct_segmentor` 用（决定查 `punctuator/half_shape` 还是
///   `punctuator/full_shape`，进而决定"这个字符算不算已知标点"）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SegmentorView {
    pub ascii_mode: bool,
    pub full_shape: bool,
}

pub trait Segmentor {
    /// `Proceed(Segmentation*)`。`history` 对应 C++ 的 `context()->commit_history()`。
    fn proceed(
        &self,
        segmentation: &mut Segmentation,
        view: SegmentorView,
        history: &CommitHistory,
    ) -> bool;
}
