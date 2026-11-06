//! `gear/auxiliary_suffix_segmentor.cc`（librime-evo，amzxyz）移植：
//! **后置辅助码**的分段器。
//!
//! # 形态
//!
//! ```text
//! 打 nihao`m  →  音码段 [0,5) "nihao"  + 触发符 `  + 辅助码 "m"
//!               ↑ 打 abc + auxiliary_suffix 标签     ↑ 不是独立段，只是输入约束
//! ```
//!
//! 与「直接辅助码」（`shir` 里 `r` 直接进音节）不同，后置辅助码**不参与切分**：
//! 它只是"给前面的候选加一条过滤条件"，所以这里返回 `false` 把分段停住 ——
//! 后面的 segmentor 不必再管这段尾巴。
//!
//! # 为什么要 `InvalidateMainSegment`
//!
//! 用户先打 `nihao`（已经算出候选），再补 `` `m ``。此时「你好」那段是
//! `kGuess/kSelected` 状态、menu 也物化好了 —— 不把它打回 `kVoid` 清掉 menu，
//! 新加的 aux 约束就**不会**触发重算，屏幕上还是那 5 个不该出现的候选。
//!
//! # 触发符从哪来
//!
//! 1. `auxiliary_suffix_segmentor/trigger`（组件自己的命名空间）；
//! 2. 回退到 `translator/aux_suffix`（evo 为了让方案作者只写一处而加的回退）。
//!
//! 空串 = 关闭后置辅助码（默认就是空，要方案主动开）。

use crate::engine::commit_history::CommitHistory;
use crate::engine::gear::segmentor::{Segmentor, SegmentorView};
use crate::engine::segmentation::{SegStatus, Segment, Segmentation};

/// 打在音码段上的标签：`abc`（普通拼写）+ `auxiliary_suffix`（末尾有 aux 约束）。
const AUX_SUFFIX_TAG: &str = "auxiliary_suffix";
const ABC_TAG: &str = "abc";

pub struct AuxiliarySuffixSegmentor {
    /// 触发符；空串 = 不启用。
    trigger: String,
}

impl AuxiliarySuffixSegmentor {
    pub fn new(config: &serde_json::Value) -> Self {
        let trigger = config
            .pointer("/auxiliary_suffix_segmentor/trigger")
            .and_then(|v| v.as_str())
            .or_else(|| {
                config
                    .pointer("/translator/aux_suffix")
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("")
            .to_string();
        Self { trigger }
    }

    pub fn trigger(&self) -> &str {
        &self.trigger
    }

    /// `ClearAuxiliarySuffixState`：触发符消失（用户退格回去）时把标签摘掉，
    /// 让那一段按普通拼写重新翻译。
    fn clear_state(segmentation: &mut Segmentation) {
        for segment in &mut segmentation.segments {
            if segment.has_tag(AUX_SUFFIX_TAG) {
                segment.tags.retain(|t| t != AUX_SUFFIX_TAG);
                segment.status = SegStatus::Void;
                segment.menu = None;
                segment.selected_index = 0;
            }
        }
    }

    /// `InvalidateMainSegment(segmentation, marker)`：把**以触发符结尾**的段打回未算状态。
    fn invalidate_main_segment(segmentation: &mut Segmentation, marker: usize) {
        for segment in &mut segmentation.segments {
            if segment.start < marker && segment.end == marker {
                if !segment.has_tag(AUX_SUFFIX_TAG) {
                    segment.tags.push(AUX_SUFFIX_TAG.to_string());
                }
                segment.status = SegStatus::Void;
                segment.menu = None;
                segment.selected_index = 0;
            }
        }
    }
}

impl Segmentor for AuxiliarySuffixSegmentor {
    fn proceed(
        &self,
        segmentation: &mut Segmentation,
        _view: SegmentorView,
        _history: &CommitHistory,
    ) -> bool {
        if self.trigger.is_empty() {
            return true;
        }
        let input = segmentation.input.clone();
        let start = segmentation.current_start();
        let Some(marker) = input[start.min(input.len())..]
            .find(&self.trigger)
            .map(|offset| start + offset)
        else {
            Self::clear_state(segmentation);
            return true;
        };
        // 只有「以小写字母开头的编码 + 触发符」才算后置辅助码。
        // `marker == 0` 说明触发符在开头 —— 那是别的用法（反查、符号引导）。
        let first = input.chars().next();
        if marker == 0 || input.is_empty() || !first.map(|c| c.is_ascii_lowercase()).unwrap_or(false)
        {
            Self::clear_state(segmentation);
            return true;
        }

        // 已确认段会在末尾留一个空占位段；后缀属于**前一段**，
        // 不先摘掉占位段，前一段就回不到活动状态，菜单也不会重算。
        if marker == start
            && segmentation
                .back()
                .map(|s| s.start == s.end && s.start == marker)
                .unwrap_or(false)
        {
            segmentation.trim();
        }
        Self::invalidate_main_segment(segmentation, marker);
        if marker > start {
            let mut segment = Segment::new(start, marker);
            segment.tags.push(ABC_TAG.to_string());
            segment.tags.push(AUX_SUFFIX_TAG.to_string());
            segmentation.push_back(segment);
        }
        // 触发符之后还没有辅助码字符时，把它挡在 abc_segmentor 之外；
        // 有字符时也是一个**输入约束**，不是可翻译段 —— 两种情况都不让后面的
        // segmentor 继续（C++ 同样 `return false`）。
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segmentation_with(input: &str) -> Segmentation {
        let mut segmentation = Segmentation::default();
        segmentation.input = input.to_string();
        segmentation
    }

    fn segmentor() -> AuxiliarySuffixSegmentor {
        let config: serde_json::Value =
            serde_json::from_str(r#"{"translator":{"aux_suffix":"`"}}"#).unwrap();
        AuxiliarySuffixSegmentor::new(&config)
    }

    #[test]
    fn trigger_falls_back_to_translator_namespace() {
        assert_eq!(segmentor().trigger(), "`");
        let config: serde_json::Value = serde_json::from_str(
            r#"{"auxiliary_suffix_segmentor":{"trigger":";"},"translator":{"aux_suffix":"`"}}"#,
        )
        .unwrap();
        assert_eq!(
            AuxiliarySuffixSegmentor::new(&config).trigger(),
            ";",
            "组件自己的命名空间优先"
        );
    }

    #[test]
    fn empty_trigger_disables_the_segmentor() {
        let config: serde_json::Value = serde_json::from_str("{}").unwrap();
        let segmentor = AuxiliarySuffixSegmentor::new(&config);
        let mut segmentation = segmentation_with("nihao`m");
        assert!(segmentor.proceed(&mut segmentation, SegmentorView::default(), &CommitHistory::default()));
        assert!(segmentation.segments.is_empty(), "没开就不该动分段");
    }

    /// 主场景：`nihao`m` 切出 [0,5) 的音码段，并打上两个标签。
    #[test]
    fn suffix_splits_the_main_segment() {
        let segmentor = segmentor();
        let mut segmentation = segmentation_with("nihao`m");
        let proceed = segmentor.proceed(
            &mut segmentation,
            SegmentorView::default(),
            &CommitHistory::default(),
        );
        assert!(!proceed, "有 aux 后缀时不让后面的 segmentor 继续");
        assert_eq!(segmentation.segments.len(), 1);
        let segment = &segmentation.segments[0];
        assert_eq!((segment.start, segment.end), (0, 5), "5 = 'nihao'.len()");
        assert!(segment.has_tag("abc"));
        assert!(segment.has_tag(AUX_SUFFIX_TAG));
    }

    /// 触发符在开头（`` ` `` 打头）不算后置辅助码 —— 那是反查前缀的用法。
    #[test]
    fn leading_trigger_is_not_a_suffix() {
        let segmentor = segmentor();
        let mut segmentation = segmentation_with("`m");
        assert!(segmentor.proceed(
            &mut segmentation,
            SegmentorView::default(),
            &CommitHistory::default()
        ));
        assert!(segmentation.segments.is_empty());
    }

    /// 非小写字母开头（西文大写 / 数字）同样不接管。
    #[test]
    fn non_lowercase_input_is_left_alone() {
        let segmentor = segmentor();
        for input in ["Nihao`m", "123`m"] {
            let mut segmentation = segmentation_with(input);
            assert!(
                segmentor.proceed(
                    &mut segmentation,
                    SegmentorView::default(),
                    &CommitHistory::default()
                ),
                "{input} 不该被接管"
            );
        }
    }

    /// 退格把触发符删掉后，标签必须一起消失（否则那段永远不再重算）。
    #[test]
    fn removing_the_trigger_clears_the_tag() {
        let segmentor = segmentor();
        let mut segmentation = segmentation_with("nihao`m");
        segmentor.proceed(
            &mut segmentation,
            SegmentorView::default(),
            &CommitHistory::default(),
        );
        assert!(segmentation.segments[0].has_tag(AUX_SUFFIX_TAG));
        // 用户退格到 "nihao"
        segmentation.input = "nihao".to_string();
        assert!(segmentor.proceed(
            &mut segmentation,
            SegmentorView::default(),
            &CommitHistory::default()
        ));
        assert!(
            !segmentation.segments[0].has_tag(AUX_SUFFIX_TAG),
            "触发符没了，约束标签也要没"
        );
    }

    /// 已算好的段被打回 `Void` 且菜单清空 —— 这是"补打 aux 后候选立刻变少"的前提。
    #[test]
    fn existing_menu_is_invalidated() {
        let segmentor = segmentor();
        let mut segmentation = segmentation_with("nihao`m");
        let mut main = Segment::new(0, 5);
        main.status = SegStatus::Selected;
        main.menu = Some(crate::engine::menu::Menu::new());
        segmentation.push_back(main);
        segmentor.proceed(
            &mut segmentation,
            SegmentorView::default(),
            &CommitHistory::default(),
        );
        let main = &segmentation.segments[0];
        assert_eq!(main.status, SegStatus::Void, "必须打回重算");
        assert!(main.menu.is_none(), "旧菜单要清掉");
        assert!(main.has_tag(AUX_SUFFIX_TAG));
    }
}
