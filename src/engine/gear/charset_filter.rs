//! `gear/charset_filter.cc`：把"扩展区汉字"挡在候选外面。
//!
//! 出厂 `wubi86` / `cangjie5` / `cangjie5_express` 三个方案都开了
//! `enable_charset_filter: true`。不开这个开关，`wqvbwq` 之类的输入里会冒出
//! 「𠛈」（U+206C8，扩展 B 区）这类候选 —— 字库里有，但用户一辈子不会打。
//!
//! # 判据只有一条
//!
//! 候选文本里**任何一个**字符落在下面这些区间，就丢掉整条候选（[`contains_extended_cjk`]）：
//!
//! | 区间 | 名称 |
//! |---|---|
//! | U+3400..=U+4DBF | 扩展 A |
//! | U+20000..=U+2A6DF | 扩展 B |
//! | U+2A700..=U+2B73F | 扩展 C |
//! | U+2B740..=U+2B81F | 扩展 D |
//! | U+2B820..=U+2CEAF | 扩展 E |
//! | U+2CEB0..=U+2EBEF | 扩展 F |
//! | U+30000..=U+3134F | 扩展 G |
//! | U+31350..=U+323AF | 扩展 H |
//! | U+2EBF0..=U+2EE5F | 扩展 I |
//! | U+323B0..=U+3347F | 扩展 J |
//! | U+F900..=U+FAFF | 兼容汉字 |
//! | U+2F800..=U+2FA1F | 兼容汉字补充 |
//!
//! C++ 里 `U+3300..U+33FF`（兼容符号）与 `U+FE30..U+FE4F`（兼容形式）两行是**注释掉**的，
//! 照抄不启用 —— 那些是标点/竖排符号，不是"生僻到不该出候选"的字。
//!
//! # 两条入口
//!
//! 1. **组件**：[`CharsetFilter`]（`charset_filter` / `cjk_minifier`）按段标签过滤候选列表。
//!    出厂方案引用数 0，但组件注册表要齐。
//! 2. **内联**：`table_translator` 的 `enable_charset_filter` 选项直接用它过滤词条 ——
//!    三个出厂方案走的都是这条路，不是组件那条。
//!
//! 两条路共用同一个谓词，开关是 `extended_charset`（用户可以在菜单里打开"显示生僻字"，
//! 打开后过滤器整个失效）。

use crate::engine::candidate::Candidate;
use crate::engine::context::Context;
use crate::engine::gear::filter::Filter;
use crate::engine::segmentation::Segment;

/// `is_extended_cjk`（`charset_filter.cc:19-45`）。
pub fn is_extended_cjk(ch: char) -> bool {
    let code = ch as u32;
    matches!(code,
        0x3400..=0x4DBF      // 扩展 A
        | 0x20000..=0x2A6DF  // 扩展 B
        | 0x2A700..=0x2B73F  // 扩展 C
        | 0x2B740..=0x2B81F  // 扩展 D
        | 0x2B820..=0x2CEAF  // 扩展 E
        | 0x2CEB0..=0x2EBEF  // 扩展 F
        | 0x30000..=0x3134F  // 扩展 G
        | 0x31350..=0x323AF  // 扩展 H
        | 0x2EBF0..=0x2EE5F  // 扩展 I
        | 0x323B0..=0x3347F  // 扩展 J
        | 0xF900..=0xFAFF    // 兼容汉字
        | 0x2F800..=0x2FA1F  // 兼容汉字补充
    )
}

/// `contains_extended_cjk`（`charset_filter.cc:47-60`）：文本里只要有一个字符超纲就算。
pub fn contains_extended_cjk(text: &str) -> bool {
    text.chars().any(is_extended_cjk)
}

/// `CharsetFilter::FilterText`：`true` = 保留这条候选。
pub fn filter_text(text: &str) -> bool {
    !contains_extended_cjk(text)
}

/// `CharsetFilter::FilterDictEntry`：词条版（`table_translator` 装到迭代器上的那个）。
pub fn filter_dict_entry(entry: &crate::dict::vocabulary::DictEntry) -> bool {
    filter_text(&entry.text)
}

/// `class CharsetFilter : public Filter, public TagMatching`。
pub struct CharsetFilter {
    /// `filter/tags`：空 = 对所有段生效（`TagMatching::TagsMatch`）。
    tags: Vec<String>,
}

impl CharsetFilter {
    /// `CharsetFilter(const Ticket&)`：`TagMatching` 只读 `tags`，没有别的配置项。
    pub fn load(config: &serde_json::Value, name_space: &str) -> Self {
        let tags = config
            .pointer(&format!("/{name_space}/tags"))
            .and_then(serde_json::Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Self { tags }
    }

    pub fn new(tags: Vec<String>) -> Self {
        Self { tags }
    }
}

impl Filter for CharsetFilter {
    fn applies_to_segment(&self, _ctx: &Context, segment: &Segment) -> bool {
        self.tags.is_empty() || segment.tags.iter().any(|tag| self.tags.contains(tag))
    }

    fn apply(&mut self, ctx: &Context, candidates: &mut Vec<Candidate>) {
        // 主命名空间 + 用户开了 `extended_charset` ⇒ 整个过滤器失效。
        // 非默认命名空间的用法 C++ 直接报错（"charset parameter is unsupported
        // by basic charset_filter"），这里按"不生效"处理。
        if ctx.get_option("extended_charset") {
            return;
        }
        candidates.retain(|candidate| filter_text(&candidate.text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_blocks_are_detected() {
        // 常用字与标点都不该命中
        assert!(!is_extended_cjk('中'));
        assert!(!is_extended_cjk('。'));
        assert!(!is_extended_cjk('Ａ'));
        // 扩展 A / B / 兼容汉字
        assert!(is_extended_cjk('\u{3400}'), "扩展 A 起点");
        assert!(is_extended_cjk('\u{206c8}'), "扩展 B（𠛈）");
        assert!(is_extended_cjk('\u{F900}'), "兼容汉字起点");
        assert!(is_extended_cjk('\u{2F800}'), "兼容汉字补充起点");
        // C++ 注释掉的区间不启用
        assert!(!is_extended_cjk('\u{3300}'), "兼容符号不该被判为生僻");
    }

    #[test]
    fn any_extended_char_rejects_the_whole_text() {
        assert!(filter_text("你好"));
        assert!(!filter_text("\u{206c8}"));
        assert!(!filter_text("你\u{206c8}好"), "有一个超纲就整条丢");
        assert!(filter_text(""));
    }

    fn cand(text: &str) -> Candidate {
        Candidate::new("table", 0, text.len(), text.to_string(), 1.0)
    }

    #[test]
    fn filter_drops_rare_candidates_unless_extended_charset() {
        let mut candidates = vec![cand("你好"), cand("\u{206c8}"), cand("好")];
        let mut filter = CharsetFilter::new(Vec::new());
        filter.apply(&Context::new(), &mut candidates);
        let texts: Vec<&str> = candidates.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["你好", "好"]);

        // 打开 `extended_charset` ⇒ 一条都不丢
        let mut context = Context::new();
        context.set_option("extended_charset", true);
        let mut candidates = vec![cand("你好"), cand("\u{206c8}")];
        filter.apply(&context, &mut candidates);
        assert_eq!(candidates.len(), 2, "开了开关就不过滤");
    }

    #[test]
    fn tags_gate_which_segments_are_filtered() {
        let mut filter = CharsetFilter::new(vec!["abc".to_string()]);
        let mut segment = Segment::new(0, 1);
        segment.tags.push("abc".to_string());
        assert!(filter.applies_to_segment(&Context::new(), &segment));

        let mut other = Segment::new(0, 1);
        other.tags.push("punct".to_string());
        assert!(!filter.applies_to_segment(&Context::new(), &other));

        // 没配 tags ⇒ 对所有段生效
        let all = CharsetFilter::new(Vec::new());
        assert!(all.applies_to_segment(&Context::new(), &other));
    }
}
