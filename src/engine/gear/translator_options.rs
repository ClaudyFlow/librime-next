//! `src/rime/gear/translator_commons.{h,cc}` 的 `TranslatorOptions`：
//! `script_translator` / `table_translator` 共享的那份配置。
//!
//! # C++ 里为什么要单独拎出这一层
//!
//! 两个翻译器的查表方式完全不同（拼音靠音节图切分组合，形码靠整码命中），
//! 但"认领哪些段、要不要补全、候选怎么格式化"是同一套，于是 librime 把它抽成
//! `TranslatorOptions`，两个翻译器各自继承一份。
//!
//! # 命名空间
//!
//! 所有键都在组件命名空间下，默认是 `translator`（见
//! [`crate::engine::gear::registry::Ticket::name_space`]），
//! 也就是方案文件里的 `translator:` 节点。

use std::collections::HashSet;

use serde_json::Value;

use crate::algo::algebra::Projection;

/// 翻译器的公共选项（对照 `translator_commons.h:143-183`）。
///
/// 不 derive `Debug`：`Projection` 字段未实现 `Debug`，这里也不需要。
#[derive(Default)]
pub struct TranslatorOptions {
    /// 码表分隔符（默认一个空格）。查表前要把输入**尾部**的分隔符削掉。
    pub delimiters: String,
    /// 本翻译器认领的段标签（恒非空，默认 `abc`）。
    pub tags: Vec<String>,
    /// `enable_completion`：输入没打完时是否也给候选（默认 true）。
    pub enable_completion: bool,
    /// `initial_quality`：叠加到每条候选质量分上的基数（默认 0）。
    pub initial_quality: f64,
    /// `preedit_format`：候选在输入区显示的文本（如笔画方案的 `xlit/hspnz/⼀⼁⼃⼂⼄/`）。
    pub preedit_formatter: Projection,
    /// `comment_format`：候选注释。
    pub comment_formatter: Projection,
    /// `dictionary_exclude`：命中即丢弃的词面。
    pub blacklist: HashSet<String>,
}

impl TranslatorOptions {
    /// C++ `TranslatorOptions::TranslatorOptions(const Ticket&)`。
    pub fn from_config(config: &Value, name_space: &str) -> Self {
        let mut options = Self {
            delimiters: String::new(),
            tags: vec!["abc".to_string()],
            enable_completion: true,
            initial_quality: 0.0,
            preedit_formatter: Projection::default(),
            comment_formatter: Projection::default(),
            blacklist: HashSet::new(),
        };

        // 分隔符：先本命名空间，回退 `speller/delimiter`。键是单数 `delimiter`。
        if let Some(v) = config
            .pointer(&format!("/{}/delimiter", name_space))
            .or_else(|| config.pointer("/speller/delimiter"))
            .and_then(Value::as_str)
        {
            options.delimiters = v.to_string();
        }
        if options.delimiters.is_empty() {
            options.delimiters = " ".to_string();
        }

        if let Some(v) = config
            .pointer(&format!("/{}/enable_completion", name_space))
            .and_then(Value::as_bool)
        {
            options.enable_completion = v;
        }
        if let Some(v) = config
            .pointer(&format!("/{}/initial_quality", name_space))
            .and_then(Value::as_f64)
        {
            options.initial_quality = v;
        }

        // tag / tags：配了 `tag` 就替换第一个；**没配** `tag` 就把默认的 `abc` 清空，
        // 完全交给 `tags` 列表（translator_commons.cc:145-158）。
        let tag = config
            .pointer(&format!("/{}/tag", name_space))
            .and_then(Value::as_str)
            .map(str::to_string);
        match tag {
            Some(t) => options.tags[0] = t,
            None => options.tags.clear(),
        }
        if let Some(list) = config
            .pointer(&format!("/{}/tags", name_space))
            .and_then(Value::as_array)
        {
            for item in list {
                if let Some(t) = item.as_str() {
                    options.tags.push(t.to_string());
                }
            }
        }
        if options.tags.is_empty() {
            options.tags.push("abc".to_string());
        }

        // 格式化规则：节点不存在是常态（C++ `Load` 拿空列表直接返回 false），不算错误。
        options.preedit_formatter =
            Projection::from_config_path(config, &format!("{}/preedit_format", name_space))
                .unwrap_or_default();
        options.comment_formatter =
            Projection::from_config_path(config, &format!("{}/comment_format", name_space))
                .unwrap_or_default();

        if let Some(list) = config
            .pointer(&format!("/{}/dictionary_exclude", name_space))
            .and_then(Value::as_array)
        {
            for item in list {
                if let Some(word) = item.as_str() {
                    options.blacklist.insert(word.to_string());
                }
            }
        }

        options
    }

    /// C++ `Segment::HasAnyTagIn(tags_)`：翻译器只认领带自己标签的段。
    pub fn has_any_tag(&self, tags: &[String]) -> bool {
        tags.iter().any(|t| self.tags.iter().any(|mine| mine == t))
    }

    /// C++ `boost::trim_right_if(code, boost::is_any_of(delimiters_))`。
    ///
    /// 形码输入常以分隔符结尾（五笔的 `z` 反查、笔画打完补空格），
    /// 不削掉就查不到整码。
    pub fn trim_delimiters<'a>(&self, code: &'a str) -> &'a str {
        code.trim_end_matches(|c| self.delimiters.contains(c))
    }

    /// 把 `preedit_format` 作用到候选的显示串上。
    pub fn format_preedit(&mut self, text: &mut String) {
        self.preedit_formatter.apply_str(text);
    }

    /// 把 `comment_format` 作用到候选注释上。
    pub fn format_comment(&mut self, text: &mut String) {
        self.comment_formatter.apply_str(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_match_c_plus_plus() {
        let o = TranslatorOptions::from_config(&json!({}), "translator");
        assert_eq!(o.delimiters, " ");
        assert_eq!(o.tags, vec!["abc".to_string()]);
        assert!(o.enable_completion, "C++ 默认 true");
        assert_eq!(o.initial_quality, 0.0);
        assert!(o.blacklist.is_empty());
    }

    #[test]
    fn delimiter_falls_back_to_speller() {
        let o = TranslatorOptions::from_config(
            &json!({"speller": {"delimiter": " '"}}),
            "translator",
        );
        assert_eq!(o.delimiters, " '");
        // 削尾部：连续分隔符全去掉，中间的保留
        assert_eq!(o.trim_delimiters("abc'  "), "abc");
        assert_eq!(o.trim_delimiters("a'b"), "a'b");
    }

    /// `tag` 替换默认标签；只给 `tags` 时默认标签要**消失**（不是追加）。
    #[test]
    fn tag_replaces_default_but_tags_does_not() {
        let o = TranslatorOptions::from_config(
            &json!({"translator": {"tag": "pinyin"}}),
            "translator",
        );
        assert_eq!(o.tags, vec!["pinyin".to_string()]);

        let o = TranslatorOptions::from_config(
            &json!({"translator": {"tags": ["a", "b"]}}),
            "translator",
        );
        assert_eq!(o.tags, vec!["a".to_string(), "b".to_string()]);

        let o = TranslatorOptions::from_config(
            &json!({"translator": {"tag": "x", "tags": ["y"]}}),
            "translator",
        );
        assert_eq!(o.tags, vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn has_any_tag_is_the_claim_check() {
        let o = TranslatorOptions::from_config(&json!({}), "translator");
        assert!(o.has_any_tag(&["abc".to_string()]));
        assert!(o.has_any_tag(&["reverse_lookup".to_string(), "abc".to_string()]));
        assert!(!o.has_any_tag(&["punct".to_string()]));
        assert!(!o.has_any_tag(&[]));
    }

    #[test]
    fn loads_blacklist_and_formatters() {
        let o = TranslatorOptions::from_config(
            &json!({"translator": {"dictionary_exclude": ["脏词"]}}),
            "translator",
        );
        assert!(o.blacklist.contains("脏词"));

        let mut o = TranslatorOptions::from_config(
            &json!({"translator": {"preedit_format": ["xlit/ab/AB/"]}}),
            "translator",
        );
        let mut s = "ab".to_string();
        o.format_preedit(&mut s);
        assert_eq!(s, "AB", "preedit_format 应作用在显示串上");
    }
}
