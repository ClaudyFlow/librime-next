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

pub trait Segmentor {
    fn proceed(&self, segmentation: &mut Segmentation) -> bool;
}
