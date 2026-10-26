//! `gear/reverse_lookup_translator.cc`：反查翻译器。
//!
//! 出厂 18 个方案在用，是"输入法里怎么打出不会念的字"的标准答案：打 `` ` ``
//! 进反查模式，再打笔画码 `hspnz`，出「你」，候选右侧注释显示它的拼音 `ni`。
//!
//! # 它同时用两份词典，方向相反
//!
//! | 角色 | 配置键 | 实际文件 | 拿去干什么 |
//! |---|---|---|---|
//! | 正查 | `reverse_lookup/dictionary`（`stroke`） | `build/stroke.table.bin` | 码 → 字（`hspnz` → 「你」） |
//! | 反查 | `reverse_lookup/target`（默认 `translator`）→ `translator/dictionary`（`luna_pinyin`） | `build/luna_pinyin.reverse.bin` | 字 → 码（「你」 → `ni`），填候选注释 |
//!
//! 反查表是编译期从词表建的（`ReverseDb::build`），键是**词面**、值是**码串**，
//! 正好和主词典相反。所以"反查"这个名字是从用户视角起的：用户想从字形查到读音。
//!
//! # 前缀/后缀是给 recognizer 用的
//!
//! 方案里 `prefix: "`"`、`suffix: "'"`，同时 `recognizer/patterns` 里有
//! ``reverse_lookup: "`[a-z]*'?$"``。recognizer 先按这个 pattern 把 `` `hspnz' ``
//! 切成一个独立段并打 `reverse_lookup` 标签，翻译器再削掉前缀后缀拿到 `hspnz`。
//! 所以这两个字段**不能只当装饰**：削不掉就查不到。
//!
//! # 与 C++ 的两处接口差异
//!
//! * **不做 lazy init。** C++ 在首次 `Query` 时 `Initialize()` 才去 `Load()` 词典
//!   （因为引擎构造时可能还没部署完）。LRN 的组件一律在 `registry` 里构造时就打开，
//!   与 `table_translator` / `script_translator` 保持一致。
//! * **排序靠 `quality` 近似。** C++ 的 `ReverseLookupTranslation::Compare` 是对
//!   `Translation` 流做两两比较（"排在所有非补全、非整句候选之后"）。LRN 的菜单是
//!   急切模型，只能用一个常数 `quality` 表达。实际方案里反查段由 recognizer 单独切出、
//!   段内只有本翻译器认领，所以这个近似不会改变可见行为。

use std::path::Path;

use serde_json::Value;

use crate::algo::syllabifier::{Syllabifier, SyllableGraph};
use crate::dict::dict_entry_iterator::DictEntryIterator;
use crate::dict::dictionary::Dictionary;
use crate::dict::reverse_lookup_dictionary::ReverseDb;
use crate::dict::vocabulary::DictEntry;
use crate::engine::candidate::Candidate;
use crate::engine::gear::translator::Translator;
use crate::engine::gear::translator_options::TranslatorOptions;
use crate::engine::segmentation::Segment;

/// 反查模式默认挂的段标签（`reverse_lookup/tag` 可改）。
const K_DEFAULT_TAG: &str = "reverse_lookup";
/// 反查表默认取自哪个命名空间的 `dictionary`（`reverse_lookup/target` 可改）。
const K_DEFAULT_TARGET: &str = "translator";
/// 补全模式下的首屏额度（C++ `LookupWords(&iter, code, true, 100, nullptr)`）。
const K_INITIAL_SEARCH_LIMIT: usize = 100;

pub struct ReverseLookupTranslator {
    /// 认领哪些段（`segment.HasTag(tag_)`）。
    tag: String,
    /// 正查词典：码 → 字。
    dict: Dictionary,
    /// 反查表：字 → 码。文件缺失时退化成"不填注释"（`None`），不是致命错误。
    rev_dict: Option<ReverseDb>,
    options: TranslatorOptions,
    prefix: String,
    suffix: String,
    /// 打了反查前缀之后在输入区显示的提示（`〔筆畫〕`）。
    tips: String,
}

impl ReverseLookupTranslator {
    /// `ReverseLookupTranslator::Initialize()`（`reverse_lookup_translator.cc:76-118`）。
    ///
    /// `name_space` 就是组件创建时的命名空间（`Ticket::name_space`）。C++ 里
    /// `if (ticket.name_space == "translator") name_space_ = "reverse_lookup";`
    /// —— 因为 `- reverse_lookup_translator` 这种不带 alias 的写法，命名空间默认
    /// 是组件类型名 `translator`，而反查的配置块一律叫 `reverse_lookup:`。
    pub fn load(config: &Value, name_space: &str, build_dir: &Path) -> Option<Self> {
        let ns = if name_space == "translator" {
            "reverse_lookup"
        } else {
            name_space
        };
        let get = |key: &str| {
            config
                .pointer(&format!("/{ns}/{key}"))
                .and_then(Value::as_str)
        };

        let dict_name = get("dictionary")?;
        let prism_name = get("prism").unwrap_or(dict_name);
        let dict = Dictionary::open(build_dir, dict_name, prism_name)?;

        let mut options = TranslatorOptions::from_config(config, ns);
        // C++ 的写法值得单独解释：`GetBool(...)` 返回 **false 表示键不存在**，
        // 于是 `if (!GetBool(...)) set_enable_completion(false);` —— 读不到就**强制关**，
        // 覆盖掉 TranslatorOptions 里的默认 true。反查默认不做前缀补全。
        options.enable_completion = config
            .pointer(&format!("/{ns}/enable_completion"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        options.strict_spelling = config
            .pointer(&format!("/{ns}/strict_spelling"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let tag = get("tag").unwrap_or(K_DEFAULT_TAG).to_string();
        let prefix = get("prefix").unwrap_or("").to_string();
        let suffix = get("suffix").unwrap_or("").to_string();
        let tips = get("tips").unwrap_or("").to_string();

        // 反查表的目标命名空间：默认读 `translator/dictionary`，也就是"主词典的
        // 反查表"。找不到就用正查词典名兜底（stroke 方案里两者同名）。
        let target = get("target").unwrap_or(K_DEFAULT_TARGET);
        let rev_name = config
            .pointer(&format!("/{target}/dictionary"))
            .and_then(Value::as_str)
            .unwrap_or(dict_name);
        // `open` 只把文件映进来，`load` 才解析两个 trie —— 少这一步 `lookup` 恒返回
        // `None`，表现为"候选出得来、注释永远是空的"（C++ 侧对应
        // `ReverseLookupDictionary::Load()` 里的 `db_->Load()`）。
        let rev_dict = ReverseDb::open(build_dir.join(format!("{rev_name}.reverse.bin")))
            .ok()
            .and_then(|mut db| db.load().then_some(db));

        Some(Self {
            tag,
            dict,
            rev_dict,
            options,
            prefix,
            suffix,
            tips,
        })
    }

    /// 正查：`code` → 词条。补全模式走前缀扫，否则建音节图取**最远**那一桶。
    ///
    /// 为什么非补全模式要建图而不是直接 `lookup_words`：反查码（笔画 `hspnz`）是
    /// 多音节串，C++ 要确认整串能被切成合法音节且**吃满**（`consumed == code.length()`），
    /// 否则说明输入打错了，宁可不给候选。`lookup_words(code, false, ...)` 的语义不同 ——
    /// 它按"码串在音节边界处"匹配，会把半截码也算命中。
    fn lookup_entries(&self, code: &str) -> Vec<DictEntry> {
        if code.is_empty() {
            return Vec::new();
        }
        if self.options.enable_completion {
            let mut iter = DictEntryIterator::new();
            self.dict.lookup_words(
                &mut iter,
                code,
                true,
                K_INITIAL_SEARCH_LIMIT,
                Some(&self.options.blacklist),
            );
            return iter.collect_entries();
        }
        let mut graph = SyllableGraph::default();
        let mut syllabifier = Syllabifier::with_options(
            &self.options.delimiters,
            true,
            self.options.strict_spelling,
            None,
        );
        let consumed = syllabifier.build_syllable_graph(code, &self.dict.prism, &mut graph);
        if consumed <= 0 || consumed as usize != code.len() {
            return Vec::new();
        }
        let collector = self.dict.lookup_from_graph(&graph, 0, None, false);
        // `collector->rbegin()`：`end_pos` 最大的那一桶 = 吃掉最多输入的那条路径
        match collector.iter().next_back() {
            Some((end, bucket)) if *end == consumed as usize => bucket.clone(),
            _ => Vec::new(),
        }
    }
}

impl Translator for ReverseLookupTranslator {
    fn query(
        &mut self,
        input: &str,
        segment: &mut Segment,
        _history: &crate::engine::commit_history::CommitHistory,
    ) -> Vec<Candidate> {
        if !segment.tags.iter().any(|t| *t == self.tag) {
            return Vec::new();
        }

        // C++ 的 `preedit` 是**完整 input**（含前缀），然后套 preedit_format
        // （`TableTranslation` 构造里 `options_->preedit_formatter().Apply(&preedit_)`）。
        // luna_pinyin 配的是 `xlit/hspnz/一丨丿丶乙/`，所以输入区显示 `` `一丨丿丶乙 ``。
        let mut preedit = input.to_string();
        self.options.format_preedit(&mut preedit);

        let mut start = 0;
        if !self.prefix.is_empty() && input.starts_with(self.prefix.as_str()) {
            start = self.prefix.len();
        }
        let mut code = &input[start..];
        if !self.suffix.is_empty() && code.ends_with(self.suffix.as_str()) {
            code = &code[..code.len() - self.suffix.len()];
        }

        if start > 0 {
            // C++ 在这里 `const_cast<Segment*>(&segment)->prompt = tips_;`
            // —— "usually translators do not modify the segment directly"，
            // 但反查就是要让输入区显示〔筆畫〕，属于明知故犯的例外。
            segment.prompt = self.tips.clone();
        }

        // `start < input.length()`：只打了前缀（`` ` ``）时不出候选。
        if start >= input.len() {
            return Vec::new();
        }

        let entries = self.lookup_entries(code);
        if entries.is_empty() {
            return Vec::new();
        }

        let end = input.len();
        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            // 反查表：字 → 码。命中就用码当注释，没命中退回词条自带的注释
            // （C++：`!tips.empty() ? tips : entry->comment`，注意只对 tips 套
            // `comment_format`，`entry->comment` 原样透出）。
            let mut tips = self
                .rev_dict
                .as_ref()
                .and_then(|rev| rev.lookup(&entry.text))
                .unwrap_or_default();
            if !tips.is_empty() {
                self.options.format_comment(&mut tips);
            }
            let comment = if tips.is_empty() {
                entry.comment.clone()
            } else {
                tips
            };
            // `SimpleCandidate` 不带质量分（默认 0）；C++ 靠 `Compare` 决定与别的
            // 翻译流谁先出，这里只留 `initial_quality` 当可配置偏移。
            let quality = self.options.initial_quality;
            out.push(
                Candidate::new("reverse_lookup", 0, end, entry.text, quality)
                    .with_comment(comment)
                    .with_preedit(preedit.clone()),
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn build_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("test/e2e/build")
    }

    fn segment_with(tag: &str) -> Segment {
        let mut seg = Segment::new(0, 0);
        seg.tags.push(tag.to_string());
        seg
    }

    fn luna_config() -> Value {
        json!({
            "reverse_lookup": {
                "dictionary": "stroke",
                "enable_completion": true,
                "prefix": "`",
                "suffix": "'",
                "tips": "〔筆畫〕",
                "preedit_format": ["xlit/hspnz/一丨丿丶乙/"]
            },
            "translator": { "dictionary": "luna_pinyin" }
        })
    }

    #[test]
    fn loads_stroke_dictionary_and_luna_reverse_table() {
        let t = ReverseLookupTranslator::load(&luna_config(), "translator", &build_dir())
            .expect("应能加载");
        assert_eq!(t.tag, "reverse_lookup");
        assert_eq!(t.prefix, "`");
        assert_eq!(t.suffix, "'");
        assert!(t.options.enable_completion, "方案里显式写了 true");
        assert!(t.rev_dict.is_some(), "应读到 luna_pinyin.reverse.bin");
    }

    /// 配置里没写 `enable_completion` → C++ 会**强制关**（覆盖默认 true）。
    #[test]
    fn enable_completion_defaults_to_false_not_true() {
        let config = json!({
            "reverse_lookup": { "dictionary": "stroke" },
            "translator": { "dictionary": "luna_pinyin" }
        });
        let t = ReverseLookupTranslator::load(&config, "translator", &build_dir()).unwrap();
        assert!(!t.options.enable_completion, "反查默认不做前缀补全");
    }

    #[test]
    fn does_not_claim_segments_without_the_tag() {
        let mut t = ReverseLookupTranslator::load(&luna_config(), "translator", &build_dir())
            .expect("应能加载");
        let mut seg = segment_with("abc");
        assert!(t.query("`hspnz", &mut seg, &crate::engine::commit_history::CommitHistory::new()).is_empty(), "没反查标签就不认领");
    }

    /// 端到端核心断言：`` `h' `` → 笔画「一」（`h` = 橫），注释是它的拼音 `yi`。
    #[test]
    fn reverse_lookup_finds_word_by_stroke_code() {
        let mut t = ReverseLookupTranslator::load(&luna_config(), "translator", &build_dir())
            .expect("应能加载");
        let mut seg = segment_with("reverse_lookup");
        let cands = t.query("`h'", &mut seg, &crate::engine::commit_history::CommitHistory::new());
        assert!(!cands.is_empty(), "`h' 应出候选");
        let yi = cands.iter().find(|c| c.text == "一").unwrap_or_else(|| {
            panic!(
                "应含「一」，实际：{:?}",
                cands.iter().map(|c| &c.text).take(12).collect::<Vec<_>>()
            )
        });
        assert_eq!(yi.kind, "reverse_lookup");
        assert_eq!(yi.comment, "yi", "「一」的注释应是从反查表取的拼音");
        // prompt 与 preedit 都按 C++ 的规矩来
        assert_eq!(seg.prompt, "〔筆畫〕", "打前缀后要设提示");
        // C++ 的 `preedit` 是**完整 input**（连前缀后缀一起），只把码字过一遍 xlit
        assert_eq!(yi.preedit, "`一'", "preedit 是完整 input 经 xlit 后的结果");
        assert_eq!((yi.start, yi.end), (0, 3), "坐标相对本段（runtime 再加 seg.start）");
    }

    /// 不带后缀也能查（suffix 只在输入真带后缀时削）。
    #[test]
    fn suffix_is_optional() {
        let mut t = ReverseLookupTranslator::load(&luna_config(), "translator", &build_dir())
            .expect("应能加载");
        let mut seg = segment_with("reverse_lookup");
        let cands = t.query("`h", &mut seg, &crate::engine::commit_history::CommitHistory::new());
        assert!(cands.iter().any(|c| c.text == "一"), "`h 不带后缀也要能出「一」");
    }

    #[test]
    fn bare_prefix_yields_nothing() {
        let mut t = ReverseLookupTranslator::load(&luna_config(), "translator", &build_dir())
            .expect("应能加载");
        let mut seg = segment_with("reverse_lookup");
        assert!(t.query("`", &mut seg, &crate::engine::commit_history::CommitHistory::new()).is_empty(), "只打前缀不该出候选");
        assert_eq!(seg.prompt, "〔筆畫〕", "但提示照设");
    }
}
