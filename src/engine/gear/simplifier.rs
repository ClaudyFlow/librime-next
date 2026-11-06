//! `gear/simplifier.cc`：把候选文本用 OpenCC 转换（简繁 / 繁简 / 地域变体 /
//! 自定义映射，如拼音注释、部件拆分、emoji）。
//!
//! # 与 C++ 的逐项对齐
//!
//! | 配置键 | 语义 |
//! |---|---|
//! | `opencc_config` | 默认 `t2s.json`；相对路径在 `<root>/opencc/` 下找 |
//! | `option_name` | 默认 `simplification`（**不是**"配了才生效"，C++ 恒有一道闸） |
//! | `tags` | 空 = 对所有段生效；非空 = 段标签有交集才生效（`TagMatching`） |
//! | `tips` / `tip` | `all` / `char`（仅单字）/ 其它 = `none` |
//! | `show_in_comment` | 转换结果放**注释**栏，候选文本保持原样 |
//! | `inherit_comment` | 默认 `true`；tips 为空时注释继承原候选 |
//! | `comment_format` | 对注释跑拼写演算（`xform/…/…/`） |
//! | `excluded_types` | 这些 `type()` 的候选不转换 |
//! | `random` | 多变体里随机取一个（C++ `RandomConvertText`） |
//!
//! # 一条候选可能变多条
//!
//! `Opencc::ConvertWord` 精确命中时返回**全部**变体（s2t 的「里」→ 里/裏），
//! C++ 会为每个变体各产一条候选。所以这里的滤镜是 1→多，不能用
//! 「原样替换 text」的简化实现（早期版本正是这么做的，结果只有默认变体）。

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::algo::algebra::Projection;
use crate::dict::opencc::Opencc;
use crate::engine::candidate::Candidate;
use crate::engine::context::Context;
use crate::engine::gear::filter::Filter;
use crate::engine::gear::translation::Translation;
use crate::engine::segmentation::Segment;

/// `tips` 的取值（C++ `Simplifier::TipsLevel`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TipsLevel {
    None,
    Char,
    All,
}

/// C++ `quote_left` / `quote_right`（`〔` / `〕`）。
const QUOTE_LEFT: &str = "\u{3014}";
const QUOTE_RIGHT: &str = "\u{3015}";

/// 滤镜的全部配置（构造后不变，可廉价共享给包装流）。
struct SimplifierConfig {
    opencc: Opencc,
    tips: TipsLevel,
    option_name: String,
    show_in_comment: bool,
    inherit_comment: bool,
    /// `comment_format` 的拼写演算。`apply_str` 要 `&mut`，用锁包住。
    comment_formatter: Mutex<Projection>,
    tags: Vec<String>,
    excluded_types: HashSet<String>,
    random: bool,
}

/// 简繁转换过滤器（对照 C++ `Simplifier`）。
pub struct Simplifier {
    config: Arc<SimplifierConfig>,
}

impl Simplifier {
    /// 从配置树构造。
    ///
    /// `opencc_dir` 是 `<某个数据根>/opencc`；调用方按「用户目录优先、共享目录其次」
    /// 逐个试，第一个能加载成功的胜出（C++ `SimplifierComponent::Create` 同序）。
    /// `name_space` 已由 `Ticket` 归一到配置节点名（无别名时是 `simplifier`）。
    pub fn from_config(
        config: &serde_json::Value,
        name_space: &str,
        opencc_dir: &Path,
    ) -> Option<Self> {
        let node = config
            .pointer(&format!("/{name_space}"))
            .or_else(|| config.pointer("/simplifier"));
        let config_name = node
            .and_then(|n| n.get("opencc_config"))
            .and_then(|v| v.as_str())
            .unwrap_or("t2s.json");
        let opencc = Opencc::load(opencc_dir.join(config_name)).ok()?;

        let tips = node
            .and_then(|n| n.get("tips").or_else(|| n.get("tip")))
            .and_then(|v| v.as_str())
            .map(|s| match s {
                "all" => TipsLevel::All,
                "char" => TipsLevel::Char,
                _ => TipsLevel::None,
            })
            .unwrap_or(TipsLevel::None);

        let option_name = node
            .and_then(|n| n.get("option_name"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| "simplification".to_string());

        let show_in_comment = node
            .and_then(|n| n.get("show_in_comment"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let inherit_comment = node
            .and_then(|n| n.get("inherit_comment"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let random = node
            .and_then(|n| n.get("random"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let comment_formatter = node
            .and_then(|n| n.get("comment_format"))
            .and_then(|v| v.as_array())
            .map(|items| {
                let rules: Vec<String> = items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect();
                Projection::from_rules(&rules).unwrap_or_default()
            })
            .unwrap_or_default();

        let tags = node
            .and_then(|n| n.get("tags"))
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let excluded_types = node
            .and_then(|n| n.get("excluded_types"))
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        Some(Self {
            config: Arc::new(SimplifierConfig {
                opencc,
                tips,
                option_name,
                show_in_comment,
                inherit_comment,
                comment_formatter: Mutex::new(comment_formatter),
                tags,
                excluded_types,
                random,
            }),
        })
    }

    /// 便捷构造（测试用）：直接给 opencc 目录 + 配置名，其余取默认。
    pub fn new(opencc_dir: &Path, config_name: &str, option_name: Option<String>) -> Option<Self> {
        let config = serde_json::json!({
            "simplifier": {
                "opencc_config": config_name,
                "option_name": option_name,
            }
        });
        Self::from_config(&config, "simplifier", opencc_dir)
    }

    /// 承载的 opencc 配置名（诊断用）。
    pub fn config_name(&self) -> &str {
        self.config.opencc.config_name()
    }
}

impl Filter for Simplifier {
    /// C++ `AppliesToSegment` 只做 `TagsMatch` —— 开关闸在 `Apply` 里。
    fn applies_to_segment(&self, _ctx: &Context, segment: &Segment) -> bool {
        if self.config.tags.is_empty() {
            return true;
        }
        self.config.tags.iter().any(|tag| segment.has_tag(tag))
    }

    fn apply(
        &mut self,
        ctx: &Context,
        translation: Box<dyn Translation>,
        _candidates: &mut Vec<Candidate>,
    ) -> Box<dyn Translation> {
        // C++ `Simplifier::Apply`：开关关 ⇒ 原样放行（双变体方案靠这道闸，
        // 否则繁体模式里 trad 翻译器产出的繁体会被 t2s 打回简体）。
        if !ctx.get_option(&self.config.option_name) {
            return translation;
        }
        Box::new(SimplifiedTranslation {
            inner: translation,
            config: self.config.clone(),
            buffer: Vec::new(),
            pos: 0,
            filled: false,
        })
    }
}

/// 逐条转换的包装流（C++ `SimplifiedTranslation`：`PrefetchTranslation` 的 1→多版）。
struct SimplifiedTranslation {
    inner: Box<dyn Translation>,
    config: Arc<SimplifierConfig>,
    buffer: Vec<Candidate>,
    pos: usize,
    filled: bool,
}

impl SimplifiedTranslation {
    /// 一次补齐：把内层全量取出，逐条转换后放进 buffer。
    ///
    /// C++ 是"按需补一条"，这里一次性跑完 —— 候选总量本来就在菜单容量内，
    /// 省掉跨条目的状态机比省这点延迟更值。
    fn fill(&mut self) {
        if self.filled {
            return;
        }
        self.filled = true;
        let mut out = Vec::new();
        while let Some(candidate) = self.inner.peek() {
            self.inner.next();
            self.convert_into(&candidate, &mut out);
        }
        self.buffer = out;
    }

    /// C++ `Simplifier::Convert` + `PushBack`。
    fn convert_into(&self, original: &Candidate, out: &mut Vec<Candidate>) {
        if self.config.excluded_types.contains(&original.kind) {
            out.push(original.clone());
            return;
        }
        let text = &original.text;
        if self.config.random {
            if let Some(converted) = self.random_convert(text) {
                out.push(self.push_back(original, &converted));
                return;
            }
            out.push(original.clone());
            return;
        }
        if let Some(forms) = self.config.opencc.convert_word(text) {
            for form in forms {
                if &form == text {
                    out.push(original.clone());
                } else {
                    out.push(self.push_back(original, &form));
                }
            }
            return;
        }
        // 没有精确命中：退回整段最长匹配（C++ `ConvertText`，没变化就不产新候选）
        if let Some(converted) = self.config.opencc.convert_text(text) {
            if &converted != text {
                out.push(self.push_back(original, &converted));
                return;
            }
        }
        out.push(original.clone());
    }

    /// C++ `RandomConvertText` 的确定性近似：整段最长匹配，没变化返回 `None`。
    ///
    /// 随机只影响"多变体时挑哪个"，不影响正确性；社区方案里 `random` 极少用
    /// （巧码 10 个 opencc 配置一个都没开），这里不做真随机以免候选不可复现。
    fn random_convert(&self, text: &str) -> Option<String> {
        let converted = self.config.opencc.convert_text(text)?;
        (converted != text).then_some(converted)
    }

    /// C++ `PushBack`：按 `show_in_comment` / `tips` 生成影子候选。
    fn push_back(&self, original: &Candidate, converted: &str) -> Candidate {
        let length = original.text.chars().count();
        let show_tips = match self.config.tips {
            TipsLevel::All => true,
            TipsLevel::Char => length == 1,
            TipsLevel::None => false,
        };
        let (text, tips) = if self.config.show_in_comment {
            // 转换结果进注释；原文本不动
            let mut tips = String::new();
            if show_tips {
                tips = converted.to_string();
                if let Ok(mut formatter) = self.config.comment_formatter.lock() {
                    formatter.apply_str(&mut tips);
                }
            }
            (original.text.clone(), tips)
        } else {
            // 转换结果替换文本；原文按 tips 进注释
            let mut tips = String::new();
            if show_tips {
                tips = original.text.clone();
                let modified = self
                    .config
                    .comment_formatter
                    .lock()
                    .map(|mut f| f.apply_str(&mut tips))
                    .unwrap_or(false);
                if !modified {
                    tips = format!("{QUOTE_LEFT}{}{QUOTE_RIGHT}", original.text);
                }
            }
            (converted.to_string(), tips)
        };
        let comment = if tips.is_empty() {
            if self.config.inherit_comment {
                original.comment.clone()
            } else {
                String::new()
            }
        } else {
            tips
        };
        let mut candidate = original.clone();
        candidate.kind = "simplified".to_string();
        candidate.text = text;
        candidate.comment = comment;
        candidate
    }
}

impl Translation for SimplifiedTranslation {
    fn peek(&mut self) -> Option<Candidate> {
        self.fill();
        self.buffer.get(self.pos).cloned()
    }

    fn next(&mut self) {
        self.fill();
        if self.pos < self.buffer.len() {
            self.pos += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::gear::translation::{drain, VecTranslation};

    fn opencc_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("Rime-Next")
            .join(crate::lever::paths::OPENCC_DIR)
    }

    fn community_opencc_dir() -> Option<std::path::PathBuf> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(".tmp_qiaoma")
            .join("shared")
            .join(crate::lever::paths::OPENCC_DIR);
        dir.is_dir().then_some(dir)
    }

    fn cand(text: &str) -> Candidate {
        Candidate::new("script", 0, text.len(), text.to_string(), 1.0)
    }

    /// 懒化后滤镜是"包装流"（不再原地转换列表）⇒ 测试把包装后的流物化再断言。
    fn converted(s: &mut Simplifier, ctx: &Context, items: Vec<Candidate>) -> Vec<Candidate> {
        let mut materialized = Vec::new();
        let stream = s.apply(ctx, Box::new(VecTranslation::new(items)), &mut materialized);
        drain(stream)
    }

    /// 打开一个开关的上下文（`option_name` 默认 `simplification`）。
    fn ctx_with(option: &str) -> Context {
        let mut ctx = Context::new();
        ctx.set_option(option, true);
        ctx
    }

    #[test]
    fn always_on_converts_text() {
        let mut s = Simplifier::new(&opencc_dir(), "s2t.json", None).expect("加载 s2t");
        let out = converted(&mut s, &ctx_with("simplification"), vec![cand("汉语")]);
        assert_eq!(out[0].text, "漢語");
        assert_eq!(out[0].kind, "simplified", "影子候选的 type 标成 simplified");
    }

    #[test]
    fn t2s_converts_text() {
        let mut s = Simplifier::new(&opencc_dir(), "t2s.json", None).expect("加载 t2s");
        let out = converted(&mut s, &ctx_with("simplification"), vec![cand("漢語")]);
        assert_eq!(out[0].text, "汉语");
    }

    #[test]
    fn gated_by_option() {
        let mut s =
            Simplifier::new(&opencc_dir(), "t2s.json", Some("zh_simp".to_string())).expect("加载 t2s");

        // 开关关着 → 原样放行
        let off = converted(&mut s, &Context::new(), vec![cand("漢語")]);
        assert_eq!(off[0].text, "漢語", "zh_simp 关着应原样");

        // 开关开着 → 转换
        let on = converted(&mut s, &ctx_with("zh_simp"), vec![cand("漢語")]);
        assert_eq!(on[0].text, "汉语");
    }

    #[test]
    fn default_option_name_is_simplification() {
        // C++ 里 `option_name` 缺省是 "simplification"，缺省选项关着 ⇒ 不转换
        let mut s = Simplifier::new(&opencc_dir(), "s2t.json", None).expect("加载 s2t");
        let out = converted(&mut s, &Context::new(), vec![cand("汉语")]);
        assert_eq!(out[0].text, "汉语", "simplification 关着不转换");
    }

    #[test]
    fn multi_variant_expands_candidates() {
        // s2t 的「里」有 里/裏 两个变体 ⇒ 两条候选
        let mut s = Simplifier::new(&opencc_dir(), "s2t.json", None).expect("加载 s2t");
        let out = converted(&mut s, &ctx_with("simplification"), vec![cand("里")]);
        assert!(
            out.len() >= 2,
            "应变出多条: {:?}",
            out.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
        assert!(
            out.iter().any(|c| c.text == "裏"),
            "{:?}",
            out.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn tips_char_puts_original_in_comment() {
        let config = serde_json::json!({
            "simplifier": { "opencc_config": "s2t.json", "tips": "char" }
        });
        let mut s = Simplifier::from_config(&config, "simplifier", &opencc_dir()).expect("加载 s2t");
        let out = converted(&mut s, &ctx_with("simplification"), vec![cand("汉")]);
        assert_eq!(out[0].text, "漢");
        assert_eq!(out[0].comment, "〔汉〕", "单字 tips 用〔〕括原文");
        // 多字词 tips=char 不提示
        let out = converted(&mut s, &ctx_with("simplification"), vec![cand("汉语")]);
        assert_eq!(out[0].comment, "");
    }

    #[test]
    fn show_in_comment_keeps_text() {
        let config = serde_json::json!({
            "simplifier": {
                "opencc_config": "s2t.json",
                "tips": "all",
                "show_in_comment": true,
                "comment_format": ["xform/^/<<", "xform/$/>>"]
            }
        });
        let mut s = Simplifier::from_config(&config, "simplifier", &opencc_dir()).expect("加载 s2t");
        let out = converted(&mut s, &ctx_with("simplification"), vec![cand("汉语")]);
        assert_eq!(out[0].text, "汉语", "show_in_comment 不动文本");
        assert_eq!(out[0].comment, "<<漢語>>", "注释走 comment_format");

        // 关掉简化 → 原样
        let off = converted(&mut s, &Context::new(), vec![cand("汉语")]);
        assert_eq!(off[0].text, "汉语");
        assert_eq!(off[0].comment, "");
    }

    /// 巧码的自定义 opencc（`type: text`）—— 修之前这条链整条被跳过。
    #[test]
    fn community_text_opencc_annotates() {
        let Some(dir) = community_opencc_dir() else {
            eprintln!("跳过:没有社区方案副本 (.tmp_qiaoma)");
            return;
        };
        // 单字拼音注释：pinyin.json + show_in_comment + tips: char
        let config = serde_json::json!({
            "pinyin_switch": {
                "opencc_config": "pinyin.json",
                "option_name": "pinyin_segestion",
                "show_in_comment": true,
                "comment_format": ["xform/'/ /"],
                "tips": "char"
            }
        });
        let mut s = Simplifier::from_config(&config, "pinyin_switch", &dir).expect("加载 pinyin");
        let out = converted(&mut s, &ctx_with("pinyin_segestion"), vec![cand("㐀")]);
        assert_eq!(out[0].text, "㐀", "拼音只进注释");
        assert_eq!(out[0].comment, "qiū");

        // 部件拆分：qiao_cf.json（tips: all + show_in_comment）
        let config = serde_json::json!({
            "chaifen": {
                "opencc_config": "qiao_cf.json",
                "option_name": "chaifen",
                "show_in_comment": true,
                "tips": "all"
            }
        });
        let mut s = Simplifier::from_config(&config, "chaifen", &dir).expect("加载 qiao_cf");
        let out = converted(&mut s, &ctx_with("chaifen"), vec![cand("⺏")]);
        assert!(out[0].comment.contains('八'), "拆分注释: {}", out[0].comment);
    }

    #[test]
    fn excluded_types_are_untouched() {
        let config = serde_json::json!({
            "simplifier": { "opencc_config": "s2t.json", "excluded_types": ["punct"] }
        });
        let mut s = Simplifier::from_config(&config, "simplifier", &opencc_dir()).expect("加载 s2t");
        let punct = Candidate::new("punct", 0, 1, "汉".to_string(), 1.0);
        let out = converted(&mut s, &ctx_with("simplification"), vec![punct]);
        assert_eq!(out[0].text, "汉", "排除的 type 不转换");
    }
}
