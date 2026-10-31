//! `history_translator`（对照 C++ `gear/history_translator.h` / `.cc`）。
//!
//! 出厂 12 个方案用它做"打一个字母就把最近上屏的词列出来"：`yujoy` / `yuling`
//! 系列都配成 `history: { input: z, size: 9, initial_quality: 1 }` —— 打 `z`
//! 就出最近 9 条。
//!
//! ## 生效条件（两个都必须满足）
//!
//! 1. 本段带 `history/tag` 标签（默认 `abc`）；
//! 2. 当前段的输入串**恰好等于** `history/input`（默认空 ⇒ 永不触发）。
//!
//! 第二个条件容易写错成"以 input 开头"：C++ 是 `input_ != input` 直接返回，
//! 打 `zh` 时 `input_("z") != "zh"` ⇒ **不出历史**。这是有意的 —— 历史只在
//! 精确键入那个触发键时出现，多打一个字母就该让位给真正的翻译器。
//!
//! ## 候选从哪来
//!
//! 从 [`crate::engine::commit_history::CommitHistory`] 倒序取，**跳过 `thru`**
//! （那些是没被翻译的裸按键，如 `3` `.`，当候选毫无意义）。候选的 `kind` 就
//! 直接沿用记录里的 `kind`（`table` / `script` / `raw` …），所以菜单上能看出
//! 这个词当初是怎么来的。
//!
//! `size` 是**上限**，默认 1；`initial_quality` 决定它和同段其他候选的相对位置，
//! 出厂普遍写成 1（很低），意思是"排在正常候选后面，只在没有别的候选时露头"。

use crate::engine::gear::translation::{Translation as TranslationTrait, VecTranslation};
use crate::engine::candidate::Candidate;
use crate::engine::commit_history::{CommitHistory, KIND_THRU};
use crate::engine::gear::translator::Translator;
use crate::engine::segmentation::Segment;
use serde_json::Value;

/// C++ 构造函数里的默认值（`history_translator.cc:22`）。
const DEFAULT_TAG: &str = "abc";
const DEFAULT_SIZE: i32 = 1;
const DEFAULT_INITIAL_QUALITY: f64 = 1000.0;

pub struct HistoryTranslator {
    /// 配置命名空间（`history`）。
    pub namespace: String,
    /// `history/tag`：本段必须带这个标签才认领。
    pub tag: String,
    /// `history/input`：触发键（空 = 永不触发）。
    pub input: String,
    /// `history/size`：最多给几条。
    pub size: i32,
    /// `history/initial_quality`：候选质量分。
    pub initial_quality: f64,
}

impl HistoryTranslator {
    /// 按默认值构造（`tag=abc`、`size=1`、`input=""`、`quality=1000`）。
    pub fn new() -> Self {
        Self {
            namespace: "history".to_string(),
            tag: DEFAULT_TAG.to_string(),
            input: String::new(),
            size: DEFAULT_SIZE,
            initial_quality: DEFAULT_INITIAL_QUALITY,
        }
    }

    /// `HistoryTranslator::HistoryTranslator(ticket)`。
    ///
    /// 命名空间的改写规则照抄：`ticket.name_space == "translator"` 时改成
    /// `"history"`（`history_translator.cc:23-25`）。所以方案里写
    /// `history_translator` 或 `history_translator@history` 都读同一个 `history:` 段。
    pub fn load(config: &Value, name_space: &str) -> Self {
        let mut t = Self::new();
        if name_space == "translator" {
            t.namespace = "history".to_string();
        } else {
            t.namespace = name_space.to_string();
        }
        let ptr = |key: &str| format!("/{}/{key}", t.namespace);
        if let Some(v) = config.pointer(&ptr("tag")).and_then(Value::as_str) {
            t.tag = v.to_string();
        }
        if let Some(v) = config.pointer(&ptr("input")).and_then(Value::as_str) {
            t.input = v.to_string();
        }
        if let Some(v) = config.pointer(&ptr("size")).and_then(Value::as_i64) {
            t.size = v as i32;
        }
        if let Some(v) = config.pointer(&ptr("initial_quality")).and_then(Value::as_f64) {
            t.initial_quality = v;
        }
        t
    }
}

impl Default for HistoryTranslator {
    fn default() -> Self {
        Self::new()
    }
}

impl Translator for HistoryTranslator {
    fn query(
        &mut self,
        input: &str,
        segment: &mut Segment,
        history: &CommitHistory,
    ) -> Option<Box<dyn TranslationTrait>> {
        if !segment.has_tag(&self.tag) {
            return None;
        }
        // `input_` 为空说明方案没配触发键，这个组件就是摆设
        if self.input.is_empty() || self.input != input {
            return None;
        }
        if history.is_empty() {
            return None;
        }
        if self.size <= 0 {
            return None;
        }
        let mut out = Vec::new();
        let mut count = 0i32;
        for record in history.iter_rev() {
            // `thru` 是穿过去的裸按键，不是"词"
            if record.kind == KIND_THRU {
                continue;
            }
            out.push(
                Candidate::new(
                    &record.kind,
                    0,
                    input.len(),
                    record.text.clone(),
                    self.initial_quality,
                ),
            );
            count += 1;
            if self.size == count {
                break;
            }
        }
        Some(Box::new(crate::engine::gear::translation::VecTranslation::new(out)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::commit_history::CommitRecord;
    use crate::engine::gear::translation::drain_opt;
    use serde_json::json;

    fn segment_with(tag: &str) -> Segment {
        let mut seg = Segment::new(0, 1);
        seg.tags.push(tag.to_string());
        seg
    }

    fn history_of(records: &[(&str, &str)]) -> CommitHistory {
        let mut h = CommitHistory::new();
        for (kind, text) in records {
            h.push(CommitRecord::new(*kind, *text));
        }
        h
    }

    #[test]
    fn reads_config_values() {
        let config = json!({ "history": { "input": "z", "size": 9, "initial_quality": 1, "tag": "xyz" } });
        let t = HistoryTranslator::load(&config, "translator");
        assert_eq!(t.namespace, "history", "translator 命名空间要改写成 history");
        assert_eq!(t.input, "z");
        assert_eq!(t.size, 9);
        assert_eq!(t.initial_quality, 1.0);
        assert_eq!(t.tag, "xyz");
    }

    #[test]
    fn defaults_match_cpp() {
        let t = HistoryTranslator::load(&json!({}), "translator");
        assert_eq!(t.tag, "abc");
        assert_eq!(t.size, 1);
        assert_eq!(t.initial_quality, 1000.0);
        assert!(t.input.is_empty(), "没配 input 就是空");
    }

    #[test]
    fn custom_namespace_is_kept() {
        let config = json!({ "myhist": { "input": "q" } });
        let t = HistoryTranslator::load(&config, "myhist");
        assert_eq!(t.namespace, "myhist");
        assert_eq!(t.input, "q");
    }

    #[test]
    fn requires_exact_input_match() {
        let mut t = HistoryTranslator::load(&json!({ "history": { "input": "z", "size": 9 } }), "translator");
        let h = history_of(&[("table", "你好")]);
        assert!(drain_opt(t.query("z", &mut segment_with("abc"), &h)).len() == 1);
        assert!(
            drain_opt(t.query("zh", &mut segment_with("abc"), &h)).is_empty(),
            "打 z 之后再多打 h 就不该再出历史"
        );
        assert!(drain_opt(t.query("", &mut segment_with("abc"), &h)).is_empty());
    }

    #[test]
    fn requires_tag() {
        let mut t = HistoryTranslator::load(&json!({ "history": { "input": "z" } }), "translator");
        let h = history_of(&[("table", "你好")]);
        assert!(
            drain_opt(t.query("z", &mut segment_with("reverse_lookup"), &h)).is_empty(),
            "没有 abc 标签的段不认领"
        );
    }

    #[test]
    fn lists_recent_first_and_respects_size() {
        let mut t = HistoryTranslator::load(
            &json!({ "history": { "input": "z", "size": 2, "initial_quality": 7.5 } }),
            "translator",
        );
        let h = history_of(&[("table", "第一"), ("script", "第二"), ("table", "第三")]);
        let cands = drain_opt(t.query("z", &mut segment_with("abc"), &h));
        assert_eq!(cands.len(), 2, "size=2 只给两条");
        assert_eq!(cands[0].text, "第三", "最新的排前面");
        assert_eq!(cands[1].text, "第二");
        assert_eq!(cands[0].kind, "table", "候选类型沿用记录的 kind");
        assert_eq!(cands[0].quality, 7.5);
        assert_eq!((cands[0].start, cands[0].end), (0, 1), "坐标相对本段");
    }

    #[test]
    fn skips_thru_records() {
        let mut t = HistoryTranslator::load(&json!({ "history": { "input": "z", "size": 9 } }), "translator");
        let h = history_of(&[("table", "词"), ("thru", "3"), ("thru", "4")]);
        let cands = drain_opt(t.query("z", &mut segment_with("abc"), &h));
        assert_eq!(cands.len(), 1, "thru 不算候选");
        assert_eq!(cands[0].text, "词");
    }

    #[test]
    fn empty_history_yields_nothing() {
        let mut t = HistoryTranslator::load(&json!({ "history": { "input": "z" } }), "translator");
        assert!(drain_opt(t.query("z", &mut segment_with("abc"), &CommitHistory::new())).is_empty());
    }
}
