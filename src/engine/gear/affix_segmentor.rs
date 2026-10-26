//! `affix_segmentor`（对照 C++ `gear/affix_segmentor.h` / `.cc`）。
//!
//! 出厂 6 个方案用它做**用户造词**：
//!
//! ```yaml
//! engine:
//!   segmentors:
//!     - ...
//!     - affix_segmentor@zaoci   # 用户造詞
//! recognizer:
//!   patterns:
//!     zaoci: "^[a-z]*`[a-z`]*$"
//! zaoci:
//!   __include: translator
//!   prefix: "`"
//!   tips: "〔用户造詞〕"
//!   tag: zaoci
//! ```
//!
//! 它做一件很具体的事：输入里出现 `prefix`（上例是反引号）时，把这一段**劈成三段** ——
//!
//! ```text
//! 输入  nihao`shijie
//!       └─┬─┘└┬┘└──┬─┘
//!       段0  前缀  段1
//!         phony  zaoci(abc…)
//! ```
//!
//! * 前缀段打 `<tag>_prefix`，**加 `phony` 标签**（`phony` = 不提交原始输入，
//!   见 `Composition::get_commit_text`），并设 `prompt = tips`（输入区显示〔用户造詞〕）；
//! * 剩余部分打 `tag`（外加 `extra_tags`），交给同名 translator 去翻译；
//! * 有 `suffix` 且输入以它结尾时再劈一段 `<tag>_suffix`，同样 `phony`
//!   （`closing_tips` 优先于 `tips`）。
//!
//! ## 三个易漏的细节
//!
//! 1. **只打前缀**（`active_input.len() == prefix.len()`）时不劈段：直接给当前段
//!    去掉 `tag`、加上 `<tag>_prefix`、设 prompt，然后返回 `true` 让**本轮继续**
//!    （后续 segmentor 还会看这段）。
//! 2. **`partial` 继承**：上一段同时带 `partial` 与 `tag` 时，当前段要继承 `tag`，
//!    并且**去掉 `abc`** —— 否则剩余输入会被 abc_segmentor 抢走，翻译器就看不到它。
//! 3. 劈完返回 `false`（**exclusive**）：本轮到此为止，后面的 segmentor 不再参与。

use crate::engine::gear::segmentor::{Segmentor, SegmentorView};
use crate::engine::segmentation::{SegStatus, Segmentation, Segment};
use serde_json::Value;

/// 默认标签（C++ `AffixSegmentor::AffixSegmentor` 里 `tag_("abc")`）。
const DEFAULT_TAG: &str = "abc";

pub struct AffixSegmentor {
    /// `{ns}/tag`：劈出来的码段要带的标签。
    pub tag: String,
    /// `{ns}/prefix`：前缀串（空 = 这个组件不干活）。
    pub prefix: String,
    /// `{ns}/suffix`：后缀串（可空）。
    pub suffix: String,
    /// `{ns}/tips`：前缀段的提示文字。
    pub tips: String,
    /// `{ns}/closing_tips`：后缀段的提示文字（空则用 `tips`）。
    pub closing_tips: String,
    /// `{ns}/extra_tags`：码段额外要带的标签（`reverse_lookup` 之类）。
    pub extra_tags: Vec<String>,
}

impl AffixSegmentor {
    pub fn new() -> Self {
        Self {
            tag: DEFAULT_TAG.to_string(),
            prefix: String::new(),
            suffix: String::new(),
            tips: String::new(),
            closing_tips: String::new(),
            extra_tags: Vec::new(),
        }
    }

    /// `AffixSegmentor::AffixSegmentor(ticket)`。
    pub fn load(config: &Value, name_space: &str) -> Self {
        let mut s = Self::new();
        let ptr = |key: &str| format!("/{name_space}/{key}");
        if let Some(v) = config.pointer(&ptr("tag")).and_then(Value::as_str) {
            s.tag = v.to_string();
        }
        if let Some(v) = config.pointer(&ptr("prefix")).and_then(Value::as_str) {
            s.prefix = v.to_string();
        }
        if let Some(v) = config.pointer(&ptr("suffix")).and_then(Value::as_str) {
            s.suffix = v.to_string();
        }
        if let Some(v) = config.pointer(&ptr("tips")).and_then(Value::as_str) {
            s.tips = v.to_string();
        }
        if let Some(v) = config
            .pointer(&ptr("closing_tips"))
            .and_then(Value::as_str)
        {
            s.closing_tips = v.to_string();
        }
        if let Some(list) = config.pointer(&ptr("extra_tags")).and_then(Value::as_array) {
            for item in list {
                if let Some(tag) = item.as_str() {
                    if !s.extra_tags.iter().any(|t| t == tag) {
                        s.extra_tags.push(tag.to_string());
                    }
                }
            }
        }
        s
    }
}

impl Default for AffixSegmentor {
    fn default() -> Self {
        Self::new()
    }
}

impl Segmentor for AffixSegmentor {
    fn proceed(&self, segs: &mut Segmentation, _view: SegmentorView) -> bool {
        if segs.empty() {
            return true;
        }
        // ── 当前段没有本组件的标签：只在 `partial` 继承这一种情况下动手 ──
        if !segs.back().map_or(false, |s| s.has_tag(&self.tag)) {
            if segs.segments.len() >= 2 {
                let previous = &segs.segments[segs.segments.len() - 2];
                if previous.has_tag("partial") && previous.has_tag(&self.tag) {
                    // 部分选中的剩余输入继承标签；但**不能**留 `abc`，
                    // 否则 abc_segmentor 会继续认领它（`affix_segmentor.cc:43-48`）
                    let inherit_abc = previous.has_tag("abc");
                    if let Some(back) = segs.back_mut() {
                        if !back.tags.iter().any(|t| *t == self.tag) {
                            back.tags.push(self.tag.clone());
                        }
                        if !inherit_abc {
                            back.tags.retain(|t| t != "abc");
                        }
                    }
                }
            }
            return true;
        }

        let j = segs.current_start();
        let k = segs.current_end();
        let active_input = segs
            .input
            .get(j..k)
            .unwrap_or("")
            .to_string();

        // `prefix_` 为空 ⇒ 没配前缀，这个组件是摆设
        if self.prefix.is_empty() || !active_input.starts_with(&self.prefix) {
            return true;
        }

        // ── 只打了前缀：就地改标签 + 设提示，本轮继续 ──
        if active_input.len() == self.prefix.len() {
            let tag = self.tag.clone();
            let prefix_tag = format!("{tag}_prefix");
            let tips = self.tips.clone();
            if let Some(back) = segs.back_mut() {
                back.tags.retain(|t| *t != tag);
                back.prompt = tips;
                if !back.tags.iter().any(|t| *t == prefix_tag) {
                    back.tags.push(prefix_tag);
                }
            }
            return true;
        }

        // ── 前缀 + 码：劈段 ──
        let code_start = j + self.prefix.len();

        let mut prefix_segment = Segment::new(j, code_start);
        prefix_segment.status = SegStatus::Guess;
        prefix_segment.prompt = self.tips.clone();
        prefix_segment.tags.push(format!("{}_prefix", self.tag));
        // `phony` = 不提交原始输入（`Composition::get_commit_text`）
        prefix_segment.tags.push("phony".to_string());

        segs.pop_back();
        segs.forward();
        segs.add_segment(prefix_segment);

        let mut code_segment = Segment::new(code_start, k);
        code_segment.tags.push(self.tag.clone());
        for tag in &self.extra_tags {
            if !code_segment.tags.iter().any(|t| t == tag) {
                code_segment.tags.push(tag.clone());
            }
        }
        segs.forward();
        segs.add_segment(code_segment);

        // ── 有后缀就再劈一段 ──
        let mut code_end = k;
        if !self.suffix.is_empty() && active_input.ends_with(&self.suffix) {
            code_end = k - self.suffix.len();
            let back_start = segs.back().map_or(0, |s| s.start);
            if code_end == back_start {
                // 码段是空的：整段扔掉，只留前后缀
                segs.pop_back();
            } else if let Some(back) = segs.back_mut() {
                back.end = code_end;
            }
            let mut suffix_segment = Segment::new(code_end, code_end + self.suffix.len());
            suffix_segment.status = SegStatus::Guess;
            suffix_segment.prompt = if self.closing_tips.is_empty() {
                self.tips.clone()
            } else {
                self.closing_tips.clone()
            };
            suffix_segment.tags.push(format!("{}_suffix", self.tag));
            suffix_segment.tags.push("phony".to_string());
            segs.forward();
            segs.add_segment(suffix_segment);
        }

        // exclusive：本轮到此为止
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn segs_of(input: &str) -> Segmentation {
        let mut segs = Segmentation::default();
        segs.input = input.to_string();
        segs
    }

    fn zaoci() -> AffixSegmentor {
        AffixSegmentor::load(
            &json!({ "zaoci": { "prefix": "`", "tips": "〔用户造詞〕", "tag": "zaoci" } }),
            "zaoci",
        )
    }

    /// 段列表的 `(start, end, tags)` 摘要，方便断言。
    fn snapshot(segs: &Segmentation) -> Vec<(usize, usize, Vec<String>)> {
        segs.segments
            .iter()
            .map(|s| (s.start, s.end, s.tags.clone()))
            .collect()
    }

    #[test]
    fn reads_config() {
        let s = AffixSegmentor::load(
            &json!({ "n": { "prefix": "<", "suffix": ">", "tips": "a", "closing_tips": "b", "tag": "t", "extra_tags": ["x", "y"] } }),
            "n",
        );
        assert_eq!(s.prefix, "<");
        assert_eq!(s.suffix, ">");
        assert_eq!(s.tips, "a");
        assert_eq!(s.closing_tips, "b");
        assert_eq!(s.tag, "t");
        assert_eq!(s.extra_tags, vec!["x", "y"]);
    }

    #[test]
    fn default_tag_is_abc() {
        assert_eq!(AffixSegmentor::load(&json!({}), "x").tag, "abc");
    }

    #[test]
    fn empty_segmentation_is_a_noop() {
        let mut segs = segs_of("`a");
        assert!(zaoci().proceed(&mut segs, SegmentorView::default()));
        assert!(segs.empty());
    }

    #[test]
    fn segment_without_tag_is_left_alone() {
        let mut segs = segs_of("`a");
        segs.push_back(Segment::new(0, 2));
        segs.segments[0].tags.push("abc".to_string());
        assert!(zaoci().proceed(&mut segs, SegmentorView::default()));
        assert_eq!(snapshot(&segs), vec![(0, 2, vec!["abc".to_string()])]);
    }

    #[test]
    fn prefix_only_retags_current_segment() {
        let mut segs = segs_of("`");
        segs.push_back(Segment::new(0, 1));
        segs.segments[0].tags.push("zaoci".to_string());
        assert!(
            zaoci().proceed(&mut segs, SegmentorView::default()),
            "只打前缀要让本轮继续"
        );
        let seg = &segs.segments[0];
        assert!(!seg.has_tag("zaoci"), "原来那个标签要去掉");
        assert!(seg.has_tag("zaoci_prefix"));
        assert_eq!(seg.prompt, "〔用户造詞〕");
    }

    #[test]
    fn prefix_and_code_splits_into_two_segments() {
        let mut segs = segs_of("`nihao");
        segs.push_back(Segment::new(0, 6));
        segs.segments[0].tags.push("zaoci".to_string());
        assert!(
            !zaoci().proceed(&mut segs, SegmentorView::default()),
            "劈完要 exclusive"
        );
        let snap = snapshot(&segs);
        assert_eq!(snap.len(), 2, "前缀段 + 码段，实际 {snap:?}");
        assert_eq!((snap[0].0, snap[0].1), (0, 1), "前缀段是 [`] ");
        assert!(snap[0].2.contains(&"zaoci_prefix".to_string()));
        assert!(snap[0].2.contains(&"phony".to_string()), "前缀不能提交原文");
        assert_eq!((snap[1].0, snap[1].1), (1, 6), "码段是 [nihao]");
        assert!(snap[1].2.contains(&"zaoci".to_string()));
        assert_eq!(segs.segments[0].prompt, "〔用户造詞〕");
    }

    #[test]
    fn extra_tags_land_on_the_code_segment() {
        let s = AffixSegmentor::load(
            &json!({ "n": { "prefix": "`", "tag": "t", "extra_tags": ["reverse_lookup"] } }),
            "n",
        );
        let mut segs = segs_of("`ab");
        segs.push_back(Segment::new(0, 3));
        segs.segments[0].tags.push("t".to_string());
        s.proceed(&mut segs, SegmentorView::default());
        assert!(segs.segments[1].has_tag("reverse_lookup"));
    }

    #[test]
    fn suffix_is_split_off_with_closing_tips() {
        let s = AffixSegmentor::load(
            &json!({ "n": { "prefix": "`", "suffix": "'", "tag": "t", "tips": "open", "closing_tips": "close" } }),
            "n",
        );
        let mut segs = segs_of("`ab'");
        segs.push_back(Segment::new(0, 4));
        segs.segments[0].tags.push("t".to_string());
        s.proceed(&mut segs, SegmentorView::default());
        let snap = snapshot(&segs);
        assert_eq!(snap.len(), 3, "前缀 + 码 + 后缀，实际 {snap:?}");
        assert_eq!((snap[1].0, snap[1].1), (1, 3), "码段不含后缀");
        assert_eq!((snap[2].0, snap[2].1), (3, 4));
        assert!(snap[2].2.contains(&"t_suffix".to_string()));
        assert_eq!(segs.segments[2].prompt, "close", "closing_tips 优先");
    }

    #[test]
    fn empty_code_with_suffix_drops_the_code_segment() {
        let s = AffixSegmentor::load(
            &json!({ "n": { "prefix": "`", "suffix": "'", "tag": "t" } }),
            "n",
        );
        // 输入就是 `` `' ``：前缀 + 空码 + 后缀
        let mut segs = segs_of("`'");
        segs.push_back(Segment::new(0, 2));
        segs.segments[0].tags.push("t".to_string());
        s.proceed(&mut segs, SegmentorView::default());
        let snap = snapshot(&segs);
        assert_eq!(snap.len(), 2, "空码段要扔掉，实际 {snap:?}");
        assert!(snap[0].2.contains(&"t_prefix".to_string()));
        assert!(snap[1].2.contains(&"t_suffix".to_string()));
    }

    #[test]
    fn partial_previous_segment_makes_current_inherit_tag() {
        let mut segs = segs_of("ni`hao");
        let mut prev = Segment::new(0, 2);
        prev.tags.push("zaoci".to_string());
        prev.tags.push("partial".to_string());
        segs.push_back(prev);
        // 当前段是剩余输入，被 abc 认领了
        let mut cur = Segment::new(2, 6);
        cur.tags.push("abc".to_string());
        segs.push_back(cur);

        assert!(zaoci().proceed(&mut segs, SegmentorView::default()));
        let back = segs.back().unwrap();
        assert!(back.has_tag("zaoci"), "要继承 tag");
        assert!(!back.has_tag("abc"), "abc 必须去掉，否则轮不到翻译器");
    }

    #[test]
    fn prefix_absent_from_input_is_a_noop() {
        let mut segs = segs_of("nihao");
        segs.push_back(Segment::new(0, 5));
        segs.segments[0].tags.push("zaoci".to_string());
        assert!(zaoci().proceed(&mut segs, SegmentorView::default()));
        assert_eq!(snapshot(&segs).len(), 1, "没前缀就不劈");
    }
}
