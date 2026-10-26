//! `gear/reverse_lookup_filter.cc`：给候选追加"反查码"注释。
//!
//! 与 [`super::reverse_lookup_translator`] 的分工：那个是"码 → 字"（输入笔画出汉字），
//! 这个是"字 → 码"（候选是汉字，把它的码写在右边）。出厂方案里引用数为 0，
//! 属于按需组件 —— 五笔/仓颉方案常自己配一个，用来提示笔画码或拼音。
//!
//! # 三个开关决定注释怎么落地
//!
//! | `overwrite_comment` | `append_comment` | 已有注释的候选 |
//! |---|---|---|
//! | false | false | **跳过**（默认：不碰别人的注释） |
//! | true | * | 直接覆盖 |
//! | false | true | 追加，空格分隔 |
//!
//! 没注释的候选一律直接写入，与两个开关无关。
//!
//! # 与 C++ 的两处差异
//!
//! * C++ 只处理 `As<Phrase>(GetGenuineCandidate(cand))` —— 因为 `SimpleCandidate`
//!   没有 `set_comment`。LRN 的 [`Candidate`] 是统一结构，注释字段直接可写，
//!   所以对所有候选生效。可见行为一致（`SimpleCandidate` 的那些本来也没注释）。
//! * 反查表在构造时就打开（C++ 是首次 `Apply` 时 `Initialize()`），理由同翻译器。

use serde_json::Value;

use crate::algo::algebra::Projection;
use crate::dict::reverse_lookup_dictionary::ReverseDb;
use crate::engine::candidate::Candidate;
use crate::engine::context::Context;
use crate::engine::gear::filter::Filter;
use crate::engine::segmentation::Segment;

pub struct ReverseLookupFilter {
    /// 反查表：字 → 码。文件缺失时整个过滤器退化为空操作。
    rev_dict: Option<ReverseDb>,
    overwrite_comment: bool,
    append_comment: bool,
    comment_formatter: Projection,
    /// `reverse_lookup/tags`：只对这些标签的段生效；空 = 全部生效
    /// （`TagMatching::TagsMatch`：`if (tags_.empty()) return true;`）。
    tags: Vec<String>,
}

impl ReverseLookupFilter {
    /// `ReverseLookupFilter::Initialize()`（`reverse_lookup_filter.cc:43-59`）。
    ///
    /// `name_space` 与翻译器同样的规矩：默认命名空间是 `filter` 时改读 `reverse_lookup`。
    ///
    /// 注意这里读的是 **`reverse_lookup/dictionary`**（= `stroke`），而翻译器读的是
    /// `target`（默认 `translator`，= `luna_pinyin`）。不是笔误：过滤器要显示的是
    /// "这个字在反查**输入法**里怎么打"，所以取反查词典自己的反查表。
    pub fn load(config: &Value, name_space: &str, build_dir: &std::path::Path) -> Self {
        let ns = if name_space == "filter" {
            "reverse_lookup"
        } else {
            name_space
        };
        let rev_dict = config
            .pointer(&format!("/{ns}/dictionary"))
            .and_then(Value::as_str)
            .and_then(|name| {
                // 只 `open` 不 `load` 的话两个 trie 都是空的，`lookup` 恒返回 `None`
                // （C++ 对应 `ReverseLookupDictionary::Load()` 里的 `db_->Load()`）。
                ReverseDb::open(crate::dict::dictionary::bin_resource_path(
                    build_dir,
                    name,
                    ".reverse",
                ))
                    .ok()
                    .and_then(|mut db| db.load().then_some(db))
            });
        let get_bool = |key: &str| {
            config
                .pointer(&format!("/{ns}/{key}"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        let tags = config
            .pointer(&format!("/{ns}/tags"))
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            rev_dict,
            overwrite_comment: get_bool("overwrite_comment"),
            append_comment: get_bool("append_comment"),
            comment_formatter: Projection::from_config_path(
                config,
                &format!("{ns}/comment_format"),
            )
            .unwrap_or_default(),
            tags,
        }
    }
}

impl Filter for ReverseLookupFilter {
    /// `ReverseLookupFilter::AppliesToSegment` → `TagMatching::TagsMatch`：
    /// `tags` 为空时对每段都生效。
    fn applies_to_segment(&self, _ctx: &Context, segment: &Segment) -> bool {
        if self.tags.is_empty() {
            return true;
        }
        self.tags.iter().any(|tag| segment.tags.contains(tag))
    }

    fn apply(&mut self, _ctx: &Context, candidates: &mut Vec<Candidate>) {
        let Some(rev_dict) = self.rev_dict.as_ref() else {
            return;
        };
        for cand in candidates.iter_mut() {
            // 有注释且两个开关都没开 → 不动别人的注释
            if !cand.comment.is_empty() && !(self.overwrite_comment || self.append_comment) {
                continue;
            }
            let Some(mut codes) = rev_dict.lookup(&cand.text) else {
                continue;
            };
            self.comment_formatter.apply_str(&mut codes);
            if codes.is_empty() {
                continue;
            }
            if self.overwrite_comment || cand.comment.is_empty() {
                cand.comment = codes;
            } else {
                cand.comment = format!("{} {}", cand.comment, codes);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::{Path, PathBuf};

    fn build_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("test/e2e/build")
    }

    /// 用 `luna_pinyin` 的反查表：`test/e2e/build/` 里只有它带 `.reverse.bin`
    /// （`stroke` 是"码 → 字"的词典，编译时不产反查表，它没有可输出的"码"）。
    fn luna_config() -> Value {
        json!({ "reverse_lookup": { "dictionary": "luna_pinyin" } })
    }

    fn cand(text: &str, comment: &str) -> Candidate {
        Candidate::new("script", 0, text.len(), text.to_string(), 1.0).with_comment(comment)
    }

    fn seg_with(tag: &str) -> Segment {
        let mut seg = Segment::new(0, 0);
        seg.tags.push(tag.to_string());
        seg
    }

    #[test]
    fn fills_comment_for_candidates_without_one() {
        let mut f = ReverseLookupFilter::load(&luna_config(), "filter", &build_dir());
        assert!(f.rev_dict.is_some(), "应读到 luna_pinyin.reverse.bin");
        let mut list = vec![cand("你", "")];
        f.apply(&Context::new(), &mut list);
        assert_eq!(list[0].comment, "ni", "「你」的注释应是拼音");
    }

    /// 默认两个开关都关 → 已有注释的候选原样保留。
    #[test]
    fn keeps_existing_comment_by_default() {
        let mut f = ReverseLookupFilter::load(&luna_config(), "filter", &build_dir());
        let mut list = vec![cand("你", "ni")];
        f.apply(&Context::new(), &mut list);
        assert_eq!(list[0].comment, "ni");
    }

    #[test]
    fn append_comment_joins_with_space() {
        let mut config = luna_config();
        config["reverse_lookup"]["append_comment"] = json!(true);
        let mut f = ReverseLookupFilter::load(&config, "filter", &build_dir());
        let mut list = vec![cand("你", "xx")];
        f.apply(&Context::new(), &mut list);
        assert_eq!(list[0].comment, "xx ni", "应追加、空格分隔，实际 {:?}", list[0].comment);
    }

    #[test]
    fn overwrite_comment_replaces_it() {
        let mut config = luna_config();
        config["reverse_lookup"]["overwrite_comment"] = json!(true);
        let mut f = ReverseLookupFilter::load(&config, "filter", &build_dir());
        let mut list = vec![cand("你", "xx")];
        f.apply(&Context::new(), &mut list);
        assert_eq!(list[0].comment, "ni", "应覆盖掉原来的注释");
    }

    #[test]
    fn tags_gate_which_segments_it_applies_to() {
        let mut config = luna_config();
        config["reverse_lookup"]["tags"] = json!(["reverse_lookup"]);
        let mut f = ReverseLookupFilter::load(&config, "filter", &build_dir());
        assert!(f.applies_to_segment(&Context::new(), &seg_with("reverse_lookup")));
        assert!(!f.applies_to_segment(&Context::new(), &seg_with("abc")));
        // 没配 tags → 全都生效
        let any = ReverseLookupFilter::load(&luna_config(), "filter", &build_dir());
        assert!(any.applies_to_segment(&Context::new(), &seg_with("abc")));
    }

    #[test]
    fn missing_reverse_table_degrades_to_noop() {
        let config = json!({ "reverse_lookup": { "dictionary": "no_such_dict" } });
        let mut f = ReverseLookupFilter::load(&config, "filter", &build_dir());
        let mut list = vec![cand("你", "")];
        f.apply(&Context::new(), &mut list);
        assert_eq!(list[0].comment, "", "没有反查表就不该动注释");
    }
}
