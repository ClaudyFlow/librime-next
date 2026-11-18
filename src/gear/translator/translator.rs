//! `src/rime/translator.h` + `gear/script_translator.cc`：`Translator` trait + 拼音翻译器。

use crate::algo::syllabifier::{SyllableGraph, Syllabifier};
use crate::gear::dict::corrector::EditDistanceCorrector;
use crate::gear::dict::dictionary::{Dictionary, K_BUCKET_CAP};
use crate::gear::dict::prism_index::PrismEntry;
use crate::gear::dict::user_dictionary::UserDictionary;
use crate::gear::dict::vocabulary::{Code, DictEntry, SyllableId};
use crate::engine::candidate::Candidate;
use crate::engine::commit_history::CommitHistory;
use crate::gear::poet::{Poet, Sentence, WordGraph};
use crate::gear::translator::translation::Translation as TranslationTrait;
use crate::gear::translator::translator_options::TranslatorOptions;
use crate::engine::segmentation::Segment;

/// 候选质量的统一尺度，对齐 C++ `ScriptTranslation::Evaluate` 的结构序：
///
/// ```text
/// quality = 码长(音节数) × 1e12 + 层级 × 1e9 + weight(对数尺度)
/// 层级:整句 2 > 用户词典 1 > 系统词表 0(C++ PrepareCandidate 的分支顺序)
/// ```
///
/// C++ 里"整句/用户词优先"靠 `PrepareCandidate` 的分支顺序、"长码优先"靠
/// `DictEntryCollector` 按 code_length 分桶 + `rbegin()` 最长先消费
/// （`script_translator.cc:479`）——都是**结构序**，与数值无关。LRN 的菜单是
/// 单一 `quality` 降序，所以把两级结构序都编码进分数：码长是主台阶
/// （1e12 远超任何 weight 摆动），同码长内层级定"学过的最优先"。
const QUALITY_SYLLABLE_STRIDE: f64 = 1.0e12;
const QUALITY_TIER_SENTENCE: f64 = 2.0e9;
const QUALITY_TIER_USER: f64 = 1.0e9;

/// 整句里是否混进了「原码原样回显」的成分（无 grammar 时的形码假整句守卫）。
///
/// `Sentence::components()` 给出逐段 `(end_pos, text, score)`（正序），但不含起点；
/// 这里用「上一段的 end」重建每段起点（第一段从 0 起），再看该段 `text` 是否
/// **逐字节等于**输入中 `[start, end)` 的原码。相等 = 这个词没被翻译、只是把
/// 编码原样搬进整句 —— 形码英文词库（`os`/`js`/`rsa`/`dime`）叠上无语法的 DP
/// 就会拼出 `os昱日jsrsadime道止` 这种伪整句（见 `best_sentence` 的守卫注释）。
///
/// 判据刻意保守：
/// - 只比「段 text == 该段输入原码」，正常翻译出的词（汉字/词组）永不命中；
/// - 拼音整句 `nihaoshijie` → ["你","好时节"] 全是汉字，绝不受影响；
/// - 纯英文方案若整段就是一个英文词条，会被 `poet` 的
///   `start==0 && end==total_length` 规则跳过，根本到不了这里；
/// - UTF-8 边界安全：只在 `input.get(start..end)` 成功（合法 char 边界）时比较。
fn sentence_has_raw_code(sentence: &Sentence, input: &str) -> bool {
    let mut start = 0usize;
    for (end, text, _) in sentence.components() {
        if input.get(start..*end) == Some(text.as_str()) {
            return true;
        }
        start = *end;
    }
    false
}

/// 翻译器（对照 C++ `Translator::Query(input, segment)`）。
///
/// 传整段 `Segment` 而不是只传 `start` + `tags`：C++ 的 `Query` 拿的是
/// `const Segment*`，`reverse_lookup_translator` 会 `const_cast` 往里写
/// `prompt`（"usually translators do not modify the segment directly; prompt text
/// is best set by a processor or a segmentor"，但它自己就是例外：打了反查前缀
/// `` ` `` 之后要在输入区显示〔笔画〕）。Rust 侧直接把 `&mut Segment` 交出去，
/// 比留个 `prompt` 旁路干净。
pub trait Translator {
    /// 每次翻译前同步一次运行期状态（C++ 里 translator 持 `Engine*`，随时能读 option）。
    ///
    /// lrn 的 `query` 只拿得到 `input` + `segment` + `history`，拿不到 `Context`，
    /// 所以需要读 option 的实现（目前只有 `punct_translator` 要 `full_shape`）
    /// 在这里刷新。默认实现什么都不做。
    fn sync_context(&mut self, _ctx: &crate::engine::context::Context) {}

    /// 本翻译器的候选是**兜底**性质：只有同一段的正经候选全空时才补上。
    ///
    /// C++ 靠 `Translation::Compare` 让翻译器之间互相窥视（`echo_translator` 的
    /// `EchoTranslation::Compare` 会检查 `other->exhausted()`），LRN 的翻译器各自
    /// 返回 `Vec` 后统一塞进 `Menu` 排序，没有这种协商通道。用一个显式标记让
    /// runtime 延后合并，语义与 C++ 等价。默认 `false`。
    fn is_fallback(&self) -> bool {
        false
    }

    /// 给定一段输入串，产出候选列表（位置**相对本段**，候选各带覆盖到的 `end`）。
    ///
    /// `history` 是本会话的提交历史（C++ 里通过 `engine_->context()` 拿到）。
    /// 只有 `history_translator` 会真的读它；其余实现忽略。
    fn query(
        &mut self,
        input: &str,
        segment: &mut Segment,
        history: &CommitHistory,
    ) -> Option<Box<dyn TranslationTrait>>;

    /// `Translator::Memorize(commit_entry)`：一次提交里选中的词条交回来学。
    ///
    /// C++ 在 `Context::commit()` 里把 `CommitEntry::elements` 播给每个 translator；
    /// 只有带用户词典的实现会真的写库，默认实现什么都不做。
    ///
    /// `history` 是本会话的提交历史：`table_translator` 的 `encode_commit_history`
    /// 要拿它把"连续几个词"拼成短语去编码（C++ 里同样是查 `engine_->context()`）。
    fn memorize(&mut self, _elements: &[DictEntry], _history: &CommitHistory) -> bool {
        false
    }
}

/// 拼音翻译器：把输入按音节图切分，逐切分查 `.table.bin` 得词条。
///
/// 词级：枚举整段输入的所有音节切分，对每个切分 `query_phrases(Code)`，
/// 词条 `end` = 该切分覆盖到的字节位置。这样「你」(单音节切分, end=2) 与「你好」
/// (双音节切分, end=4) 同台出现，逐音节确认行为自然成立。
pub struct ScriptTranslator {
    pub dict: Dictionary,
    pub options: TranslatorOptions,
    /// 组句器（`poet.cc`）。没有 grammar 时内部走动态规划，等价 C++ 的
    /// `Poet::MakeSentenceWithStrategy<DynamicProgramming>`。
    pub poet: Poet,
    /// 用户词典（`Translator::user_dict_`）。`None` = 方案没开 `enable_user_dict`
    /// 或者拿不到词典名。
    pub user_dict: Option<UserDictionary>,
    /// 纠错器（`translator/enable_correction: true` 时懒建）。
    ///
    /// 音节表来自 [`Dictionary::table`]，一次建好；每次 `build_graph` 只需要
    /// [`EditDistanceCorrector::clone`]（共享同一份音节表与查询缓存）。
    pub correction: Option<EditDistanceCorrector>,
    /// 完整输入串（含 `segment.end` **之后**的辅助码尾巴）。
    ///
    /// `query()` 只拿得到段内子串，而后置辅助码 `nihao`m` 的 `` `m `` 全在段外 ——
    /// 所以由 [`Translator::sync_context`] 每轮把它抄进来。
    pub full_input: String,
    /// 后置辅助码触发符（`translator/aux_suffix`）；空 = 不启用。
    pub aux_trigger: String,
    /// 直接辅助码开关（`translator/auxiliary_mode`，默认 true）。
    pub auxiliary_mode: bool,
    /// 辅助码查表器（缓存音节表与字形反查结果）。
    pub aux_matcher: crate::gear::segmentor::auxiliary_matcher::AuxiliaryMatcher,
    /// 辅助码反查词典（`.reverse.bin` 里带 `\x1faux` 键时才有用，作为回退）。
    pub aux_reverse: Option<std::sync::Arc<crate::gear::dict::reverse_lookup_dictionary::ReverseDb>>,
    /// 独立辅助码字典（`build/auxiliary` 编出的 `<group>.aux.bin`，`translator/aux` 指定组名）。
    pub aux_db: Option<std::sync::Arc<crate::gear::dict::auxiliary_db::AuxiliaryDb>>,
    /// input type → prism 名（未来由 `translator/prisms` 配置填充）。
    ///
    /// 空表 = 单 prism 行为，与升级前完全一致。
    pub prism_selections: std::collections::BTreeMap<String, String>,
    /// 当前输入类型（空 = 不做选择）。由方案层/界面在会话里设置。
    pub input_type: String,
    /// 主 prism 名（构造时从 `Dictionary` 记下）。
    ///
    /// 不能现取 `dict.prism_name()` —— 那返回的是**当前生效**的 prism：切到双拼后
    /// 再取就是双拼名，"全部开关关闭 → 回主 prism"会变成空操作（2026-11-09 E2E 实测：
    /// 从注音切回全拼，prism 仍停在注音，`zhongguo` 被按注音键位解析成别的音节）。
    pub primary_prism: String,
}

impl ScriptTranslator {
    pub fn new(dict: Dictionary) -> Self {
        Self::with_options(dict, TranslatorOptions::default())
    }

    pub fn with_options(dict: Dictionary, options: TranslatorOptions) -> Self {
        let primary_prism = dict.prism_name().to_string();
        Self {
            dict,
            options,
            poet: Poet::new(None),
            user_dict: None,
            correction: None,
            full_input: String::new(),
            aux_trigger: String::new(),
            auxiliary_mode: true,
            aux_matcher: crate::gear::segmentor::auxiliary_matcher::AuxiliaryMatcher::new(),
            aux_reverse: None,
            aux_db: None,
            prism_selections: std::collections::BTreeMap::new(),
            input_type: String::new(),
            primary_prism,
        }
    }

    /// 注册一条 `input type → prism` 选择（Stage B 的 `translator/prisms` 解析落点）。
    pub fn set_prism_selection(&mut self, input_type: &str, prism_name: &str) {
        self.prism_selections
            .insert(input_type.to_string(), prism_name.to_string());
    }

    /// 阶段 B：解析 `translator/prisms` 与 `translator/input_types`。
    ///
    /// 兼容 evo 的写法：`prisms` 是 `[{name, option}]`，`option` 在这里直接当作
    /// input type 名（evo 用它读 context switch，见 [`Self::sync_context`]）。
    /// 另外支持更短的映射写法：
    ///
    /// ```yaml
    /// translator:
    ///   prisms:
    ///     - { name: pinyin.multi.ziranma, option: ziranma }
    ///   # 或者
    ///   input_types:
    ///     full: pinyin.multi
    ///     ziranma: pinyin.multi.ziranma
    ///   # 或者
    ///   prisms:
    ///     full: pinyin.multi
    ///     ziranma: pinyin.multi.ziranma
    /// ```
    ///
    /// 也兼容字符串引用 `prisms: input_types`：展开顶层 `/input_types/options[].option`
    /// 里的名字，再按 `index`（部署期 `prisms.list`）把名字映射到同名组的棱镜。
    ///
    /// `prism`（单数、主 prism）可以与 `prisms` 共存：前者是默认 prism，后者只是
    /// 额外的输入类型映射。这样旧方案加 `prisms` 时不会因为多了 `prism` 被整体禁用。
    ///
    /// **自动发现**：`prisms` 与 `input_types` 都不存在时，若 `index` 有内容，就把
    /// 索引里的组名全部注册为 `组名 → <dict>.<组名>`。索引缺失/过期时 `index` 为
    /// `None`，回到"只有主 prism"的旧行为。
    ///
    /// **优先级**：同时打开多个可选开关时，[`Self::sync_context`] 按
    /// `prism_selections` 的 BTreeMap 字典序取第一个命中的 option（与手写清单时
    /// 完全一致）；互斥 switch 正常只会有一个打开，这条规则只用于异常组合的确定性。
    pub fn apply_prism_config(
        &mut self,
        config: &serde_json::Value,
        name_space: &str,
        index: Option<&[PrismEntry]>,
    ) -> Result<(), String> {
        // 与 `TranslatorOptions::from_config` 保持同一套命名空间规则。
        let prefix = if name_space == "simp" || name_space == "trad" {
            format!("/translator/{name_space}")
        } else {
            format!("/{name_space}")
        };
        let prisms = config.pointer(&format!("{prefix}/prisms"));
        let input_types = config.pointer(&format!("{prefix}/input_types"));
        if prisms.is_none() && input_types.is_none() {
            // 没有手写名单：按部署索引自动认。空索引 / 旧部署（None）都保持单 prism。
            if let Some(entries) = index {
                for entry in entries {
                    self.set_prism_selection(&entry.group, &entry.prism);
                }
            }
            return Ok(());
        }

        let mut selections: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        if let Some(prisms) = prisms {
            if let Some(reference) = prisms.as_str() {
                parse_prism_reference(
                    reference,
                    config,
                    &prefix,
                    index,
                    &self.primary_prism,
                    &mut selections,
                )?;
            } else {
                parse_prisms(prisms, &prefix, &mut selections)?;
            }
        }
        if let Some(input_types) = input_types {
            parse_input_types(input_types, &prefix, &mut selections)?;
        }
        for (input_type, prism_name) in selections {
            self.set_prism_selection(&input_type, &prism_name);
        }
        Ok(())
    }

    /// 切换当前输入类型；命中选择表时立即切 prism。
    ///
    /// 返回 prism 切换是否成功（未命中选择表视为成功，保持当前 prism）。
    pub fn set_input_type(&mut self, input_type: &str) -> bool {
        self.input_type = input_type.to_string();
        self.apply_prism_selection()
    }

    /// query 入口的选择动作：把 `input_type` 映射成 prism。
    fn apply_prism_selection(&mut self) -> bool {
        let Some(name) = self.prism_selections.get(&self.input_type).cloned() else {
            return true;
        };
        self.dict.set_prism(&name)
    }

    /// 按输入类型切换 prism（多 prism / `input_types` 的运行时接入点）。
    ///
    /// **选择点在 translator 的 query 入口，不在 `Dictionary` 的查询入口。**
    /// 理由：input type 是方案级概念（"这个方案怎么把按键切成音节"），
    /// `Dictionary` 只是词表 + 音节 id 映射的容器，同一份词典可以被不同 input type
    /// 复用；而 prism 决定 `build_graph` 里 `common_prefix_search` 的键集，
    /// 必须在构图**之前**切好。后续 `translator/prisms` 解析完成后，
    /// [`Translator::query`] 开头按当前 input type 调这里即可。
    pub fn select_prism(&mut self, prism_name: &str) -> bool {
        self.dict.set_prism(prism_name)
    }

    /// 配好后置辅助码（`translator/aux_suffix` + `translator/aux`）。
    ///
    /// 数据来源两个：独立 aux 字典（主）与 `.reverse.bin` 的 `\x1faux` 键（回退）。
    pub fn with_auxiliary(
        mut self,
        trigger: &str,
        reverse: Option<std::sync::Arc<crate::gear::dict::reverse_lookup_dictionary::ReverseDb>>,
        db: Option<std::sync::Arc<crate::gear::dict::auxiliary_db::AuxiliaryDb>>,
    ) -> Self {
        self.aux_trigger = trigger.to_string();
        self.aux_reverse = reverse;
        self.aux_db = db;
        self
    }

    /// `auxiliary_enabled()`：两个数据来源有一个 + 开关没关。
    pub fn auxiliary_enabled(&self) -> bool {
        self.auxiliary_mode && (self.aux_db.is_some() || self.aux_reverse.is_some())
    }

    /// 从段 + 完整输入算出后置辅助码串（`ScriptTranslator::Query` 开头那段）。
    ///
    /// 条件三个都要满足：
    /// 1. 段上带 `auxiliary_suffix` 标签（由 `auxiliary_suffix_segmentor` 打）；
    /// 2. 段末尾紧跟触发符；
    /// 3. 触发符之后至少还有一个字符。
    pub fn auxiliary_suffix(&self, segment: &Segment) -> Option<String> {
        if !self.auxiliary_enabled() || self.aux_trigger.is_empty() {
            return None;
        }
        if !segment.has_tag("auxiliary_suffix") {
            return None;
        }
        let full = &self.full_input;
        let after_marker = segment.end + self.aux_trigger.len();
        if full.len() <= after_marker {
            return None;
        }
        if !full.get(segment.end..)?.starts_with(&self.aux_trigger) {
            return None;
        }
        Some(full[after_marker..].to_string())
    }

    /// 把一条流按后置辅助码过滤。
    ///
    /// 辅助码是**输入约束**，不是候选层：命中的留下、没命中的整条丢。
    /// 用 `FilteredTranslation` 包装而不是预先 drain，是为了保住懒流 ——
    /// 一屏只要 5 条，没必要为过滤把几百条候选全物化。
    fn apply_aux_filter(
        &self,
        inner: Box<dyn crate::gear::translator::translation::Translation>,
        aux: &str,
    ) -> Box<dyn crate::gear::translator::translation::Translation> {
        let reverse = self.aux_reverse.clone();
        let db = self.aux_db.clone();
        if reverse.is_none() && db.is_none() {
            return inner;
        }
        // 闭包自己持一份 matcher：反查结果（字形 → 辅助码）在整条流上复用，
        // 每次 peek 都新建一份会把同一批字反复查几十次。
        let mut matcher = crate::gear::segmentor::auxiliary_matcher::AuxiliaryMatcher::new();
        let input = aux.to_string();
        Box::new(crate::gear::translator::translation::FilteredTranslation::new(
            inner,
            move |candidate: &mut Candidate| {
                matcher.matches(db.as_deref(), reverse.as_deref(), &candidate.text, &input)
            },
        ))
    }

    /// 挂上用户词典（`Translator::user_dict_`）。
    pub fn with_user_dict(mut self, user_dict: UserDictionary) -> Self {
        self.user_dict = Some(user_dict);
        self
    }

    /// 换上带语法模型的组句器（`Poet` 里 `grammar` 非空即走束搜索）。
    pub fn with_poet(mut self, poet: Poet) -> Self {
        self.poet = poet;
        self
    }

    /// 建纠错器（`translator/enable_correction: true` 时）。
    ///
    /// 幂等：没开开关、或已经建过就直接返回。音节表从 `Table` 拿 —— prism 那边
    /// 不暴露 syllabary（`dict/prism.h` 只有查词方法），而 `Table` 有。
    /// 拿不到音节表（词典没打开）时保持 `None`，纠错静默失效。
    pub fn ensure_correction(&mut self) {
        if !self.options.enable_correction || self.correction.is_some() {
            return;
        }
        // 音节表优先取 prism 的（v6 是 union 表，含 pack 独有新音节；v5 没有）。
        let Some(syllabary) = self
            .dict
            .prism
            .syllabary_set()
            .or_else(|| self.dict.table.syllabary())
        else {
            return;
        };
        // ⚠️ id 必须是 **prism 的拼写 id**，不能是 table 的音节 id：
        // `syllabifier` 拿到 `tolerance_search` 的结果后直接拿它去
        // `prism.query_spelling(id)` 查属性（`syllabifier.cc:97-104`），
        // 所以这里逐条用 `prism.get_value` 把音节串翻成 prism 自己的 id。
        // v6 下 prism 的拼写 id 就是**全局**音节 id（与音节图一致）。
        let mut pairs: Vec<(String, SyllableId)> = Vec::new();
        for spelling in syllabary.iter() {
            if let Some(spelling_id) = self.dict.prism.get_value(spelling) {
                pairs.push((spelling.clone(), spelling_id));
            }
        }
        if pairs.is_empty() {
            return;
        }
        self.correction = Some(EditDistanceCorrector::with_syllabary(
            self.options.correction_tolerance,
            pairs,
        ));
    }

    /// 构造候选的 preedit 串（对照 C++ `ScriptSyllabifier::GetPreeditString`）。
    ///
    /// 用 prism 把候选覆盖的输入重新切分成音节，用分隔符连接，
    /// 再套 `preedit_formatter`。切分失败时返回空串（候选 preedit 留空）。
    fn build_preedit(&self, input: &str, end: usize) -> String {
        self.syllabify(input, end)
            .map(|(preedit, _)| preedit)
            .unwrap_or_default()
    }

    /// 一次切分同时产出 **preedit 串**与**音节顶点**。
    ///
    /// 顶点是**相对本段**的字节偏移、升序、首 = 0、末 = `end`
    /// （C++ `Spans::vertices()` 的形态；`partial_commit` 靠它把
    /// 「前 N 个音节」翻成输入上的切点）。
    ///
    /// 切分与 preedit 本来就是同一件事的两种投影，分开算会得到两套边界
    /// （preedit 用最短切分，spans 又另走一遍最长切分 → 屏幕上看到的分隔
    /// 和 Ctrl+数字切的位置对不上），所以这里**只有一条** DFS。
    fn syllabify(&self, input: &str, end: usize) -> Option<(String, Vec<usize>)> {
        let sub = &input[..end.min(input.len())];
        let n = sub.len();
        if n == 0 {
            return None;
        }
        // 每个起始位置能起的合法音节（只关心跨度，不关心音节 id）。
        let mut match_at: Vec<Vec<usize>> = vec![Vec::new(); n + 1];
        for start in 0..n {
            let suffix = &sub[start..];
            for m in self.dict.prism.common_prefix_search(suffix) {
                match_at[start].push(start + m.length);
            }
        }
        // DFS 找一条能精确覆盖到 `end` 的最短切分。
        let mut best: Vec<usize> = Vec::new();
        fn dfs(
            start: usize,
            end: usize,
            match_at: &[Vec<usize>],
            current: &mut Vec<usize>,
            best: &mut Vec<usize>,
        ) {
            if start == end {
                if best.is_empty() || current.len() < best.len() {
                    *best = current.clone();
                }
                return;
            }
            if start > end || current.len() > 12 {
                return;
            }
            for &next_pos in &match_at[start] {
                current.push(next_pos);
                dfs(next_pos, end, match_at, current, best);
                current.pop();
            }
        }
        dfs(0, n, &match_at, &mut Vec::new(), &mut best);
        if best.is_empty() {
            return None;
        }
        // 用分隔符连接各音节。
        let delimiter = self.options.delimiters.chars().next().unwrap_or(' ');
        let mut out = String::new();
        let mut prev = 0;
        for &pos in &best {
            if !out.is_empty() && !out.ends_with(delimiter) {
                out.push(delimiter);
            }
            out.push_str(&sub[prev..pos]);
            prev = pos;
        }
        // 顶点：0 → 各音节末位。`best` 已经是升序闭区间终点。
        let mut vertices = Vec::with_capacity(best.len() + 1);
        vertices.push(0);
        vertices.extend(best.iter().copied());
        Some((out, vertices))
    }

    /// 相对顶点 → 绝对顶点（补上段起点）。
    fn absolute_spans(base: usize, vertices: &[usize]) -> Vec<usize> {
        vertices.iter().map(|v| base + v).collect()
    }

    /// 整句候选（`ScriptTranslation::Evaluate` 的 sentence 分支，
    /// `script_translator.cc:444-514`）。
    ///
    /// C++ 的三个前置条件全部照抄：
    ///
    /// 1. `has_at_least_two_syllables` —— `syllable_graph.edges.size() >= 2`
    ///    （只有一个起点的输入谈不上"组句"）；
    /// 2. 没有可靠词级候选 —— 若整段输入能命中一个 `IsExactMatch` 的词条
    ///    （比如 `nihao` → 「你好」），就不出整句；
    /// 3. 组句成功。
    ///
    /// # quality 换算（与 C++ 的差异）
    ///
    /// C++ 的候选顺序是**结构性**的：`PrepareCandidate` 先走 `kSentence` 分支，
    /// 整句天然排在词级前面，与 `quality` 无关。LRN 的
    /// [`crate::engine::menu`] 统一按 `quality` 降序排序，所以这里给整句叠一个
    /// 远大于词级量级（词级是 `音节数 * 1000 + weight`）的基数：
    /// 既保证整句排最前，又保留多条整句之间的相对顺序。
    ///
    /// 2026-10-31:去掉无人调用的 `sentence_candidates` 包装(query 内联构造),
    /// 它的候选坐标还是段内相对系(起点写死 0),affix 段会错位 —— 留着是坑。
    /// [`Self::best_sentence`] 保留(条件判定仍然用它)。

    /// 按 C++ 的前置条件算出一条最优整句；不满足条件时 `None`。
    fn best_sentence(&mut self, input: &str) -> Option<Sentence> {
        let (graph, consumed) = self.build_graph(input)?;
        // 条件 1：至少两个起点（`script_translator.cc:497`）
        if graph.edges.len() < 2 {
            return None;
        }
        let word_graph = self.prepare_word_graph(&graph, consumed);
        // 条件 2：有可靠词级候选就不组句（`script_translator.cc:502-508`）
        if self.has_reliable_phrase(&word_graph, consumed) || word_graph.is_empty() {
            return None;
        }
        // 条件 3：组句（`max_sentences_` 默认 1，出厂方案没配过）
        let sentence = self.poet.make_sentence(&word_graph, consumed, "")?;
        // 无语法模型时的"原码回显"守卫（2026-11-10，用户 4 张截图）：
        // 形码方案（巧码）的英文词库把**编码字母本身**收成词条（qiaoma_en 的
        // `os`/`js`/`rsa`/`dime` 等），octagram 移除后 DP 只按 weight 拼句，
        // 于是把这些未翻译的原码段和真词拼成 `os昱日jsrsadime道止` 这种假整句，
        // 再吃 QUALITY_TIER_SENTENCE 强排首位 = 用户看到的"乱码"。
        // C++ 原版靠 grammar 把混排句压到最低；本引擎 grammar 恒 None，必须显式拦。
        //
        // 判据：整句任一成分的 text 恰好等于它覆盖的那段输入原码（逐字节相等），
        // 即"原样回显、没翻译"，整句作废，让词级候选（昏/昱日/…）上位。
        // 不误伤拼音：`nihaoshijie` → ["你","好时节"] 每段都是汉字，永不等于 ASCII 拼音。
        if !self.poet.has_grammar() && sentence_has_raw_code(&sentence, input) {
            return None;
        }
        Some(sentence)
    }

    /// 构建音节图；`consumed` 是音节图能解释到的位置（`interpreted_length`）。
    fn build_graph(&mut self, input: &str) -> Option<(SyllableGraph, usize)> {
        let mut graph = SyllableGraph::default();
        let mut syllabifier = Syllabifier::with_options(
            &self.options.delimiters,
            self.options.enable_completion,
            false,
            None,
        );
        // 纠错：`enable_correction` 打开 + 纠错器已建（音节表来自 Table）。
        // 挂上后纠出来的拼写带 `is_correction` + `credibility = log(0.01)`，
        // 因此只会在真候选**后面**出现（Xime 说的"只追加、绝不替换"）。
        if let Some(corrector) = self.correction.as_ref() {
            syllabifier.enable_correction(Box::new(corrector.clone()));
        } else if self.options.enable_near_correction {
            // R-04：邻键纠错旁路（trie BFS，只做"替换 1 次"，长度 ≥3）。
            // 无状态、无音节表依赖，所以不需要 `ensure_correction` 那套缓存。
            syllabifier.enable_correction(Box::new(
                crate::gear::dict::near_corrector::NearSearchCorrector::new(),
            ));
        }
        let mut consumed = syllabifier.build_syllable_graph(input, &self.dict.prism, &mut graph);
        if consumed <= 0 {
            return None;
        }
        // direct aux：构图期把"基音节 + 辅助码"合成一条带 `allowed_chars` 的边。
        // 与 evo `syllabifier.cc:173-198` 的 `auxiliary_lookup_` 注入等价。
        // 只处理以 0 起点的整段输入（`AuxiliaryMatcher::lookup` 的输入就是整串）。
        if self.auxiliary_enabled() {
            if let Some(db) = self.aux_db.clone() {
                let matches = self
                    .aux_matcher
                    .lookup(&self.dict, db.as_ref(), input);
                for m in matches {
                    graph.add_aux_edge(
                        0,
                        m.length,
                        m.syllable_id,
                        m.spelling_type,
                        m.credibility,
                        m.allowed_chars,
                    );
                    if m.length > graph.interpreted_length {
                        graph.interpreted_length = m.length;
                        consumed = m.length as i32;
                    }
                }
            }
        }
        Some((graph, consumed as usize))
    }

    /// 覆盖 `[0, end]` 的一条音节路径 → `Code`（用户词典的学词键）。
    ///
    /// 整句候选的 `entry` 原本**没有 code**，于是 `Memorize` 写不进用户词典
    /// （`update_entry` 见空 code 就跳过）——用户上屏的整句永远学不会，下次打同样
    /// 一串还是整句候选压在前面，刚打过的词"又回到后面"（2026-11-01 实机报告）。
    ///
    /// 走法：每个起点取**跨度最大**的边（长音节优先，与 `Transpose` 的序一致），
    /// 直到走满 `end`。切分点优先用 `stops`（整句各成分的 end_pos，来自 poet），
    /// 这样学进去的切法与屏幕上看到的分词一致。
    fn code_for(&mut self, input: &str, end: usize, stops: &[usize]) -> Option<Code> {
        if end == 0 {
            return None;
        }
        let (graph, _consumed) = self.build_graph(input)?;
        let mut ids: Vec<crate::algo::syllabifier::SyllableId> = Vec::new();
        let mut pos = 0usize;
        while pos < end {
            // 优先落在成分边界上（poet 的分词点），否则取最长音节
            let target = stops
                .iter()
                .copied()
                .find(|s| *s > pos && *s <= end && graph.edges.get(&pos).map_or(false, |m| m.contains_key(s)))
                .or_else(|| {
                    graph.edges.get(&pos).and_then(|m| {
                        m.keys()
                            .filter(|e| **e <= end)
                            .max()
                            .copied()
                    })
                })?;
            let syllable_id = graph
                .edges
                .get(&pos)
                .and_then(|m| m.get(&target))
                .and_then(|sp| sp.keys().next().copied())?;
            ids.push(syllable_id);
            pos = target;
        }
        Some(Code::from_slice(&ids))
    }

    /// `PrepareForMakingSentence`（`script_translator.cc:705-722`）：
    /// 图里**每个起点**都查一次词表，合成 `WordGraph`。
    fn prepare_word_graph(
        &mut self,
        graph: &SyllableGraph,
        _consumed: usize,
    ) -> WordGraph {
        let starts: Vec<usize> = graph.edges.keys().copied().collect();
        let mut word_graph = WordGraph::new();
        for start in starts {
            let mut collector = self.dict.lookup_from_graph(
                graph,
                start,
                Some(&self.options.blacklist),
                self.options.enable_completion,
                Some(K_BUCKET_CAP),
            );
            // 整句词图也要过 aux（evo `PrepareForMakingSentence` 用的
            // `Dictionary::Lookup` 是带 `auxiliary_path` 的下推查询）：
            // 否则打了辅助码，整句仍能拿同音字拼出一句，排在过滤后的词级候选前面。
            let filter_aux = graph.has_auxiliary() && aux_stream_filter_enabled();
            if filter_aux {
                collector.retain(|end, bucket| {
                    bucket.retain(|entry| aux_allows(graph, start, &entry.code, &entry.text, *end));
                    !bucket.is_empty()
                });
            }
            // 用户词典词条并入词图(2026-11-01,"神经网络算用户词"):
            // 学过的词参与整句组装——用户词典权重随提交次数增长,
            // 包含它的整句得分随之上升,打的遍数多了整句自然登顶。
            if let Some(user_dict) = self.user_dict.as_mut()
                && user_dict.loaded()
                && let Some(user_collector) =
                    user_dict.lookup(&self.dict, graph, start, 0, 0, 0.0)
            {
                for (end, iterator) in user_collector {
                    let entries = iterator.into_entries().0;
                    let bucket = collector.entry(end).or_default();
                    for entry in entries {
                        // evo `UserDictionary::DfsState::auxiliary_path` +
                        // `RecruitEntry`（`user_dictionary.cc:88-89`）：用户词同样受约束。
                        if filter_aux && !aux_allows(graph, start, &entry.code, &entry.text, end) {
                            continue;
                        }
                        if !self.options.blacklist.contains(&entry.text)
                            && !bucket.iter().any(|e| e.text == entry.text)
                        {
                            bucket.push(entry);
                        }
                    }
                }
            }
            if !collector.is_empty() {
                word_graph.insert(start, collector);
            }
        }
        word_graph
    }

    /// `has_exact_match_phrase`（`script_translator.cc:478-482`）：
    /// `end_pos == consumed` 且首条词条是精确匹配。
    fn has_reliable_phrase(&self, word_graph: &WordGraph, consumed: usize) -> bool {
        word_graph
            .get(&0)
            .and_then(|edges| edges.get(&consumed))
            .and_then(|bucket| bucket.first())
            .is_some_and(|entry| entry.is_exact_match())
    }

    /// 取组句结果（多条），给 e2e 断言与调试用。
    pub fn make_sentences(&mut self, input: &str) -> Vec<Sentence> {
        let Some((graph, consumed)) = self.build_graph(input) else {
            return Vec::new();
        };
        let word_graph = self.prepare_word_graph(&graph, consumed);
        self.poet.make_sentences(&word_graph, consumed, "", 5, 0.1)
    }

    /// 用户词典候选（`script_translator.cc` 里 `user_dict_->Lookup(...)` 那段）。
    ///
    /// C++ 把用户词典词条塞进同一个 `ScriptTranslation`，靠 `credibility` 加成
    /// 顶到词表词条前面；LRN 用 [`USER_DICT_QUALITY_BASE`] 表达同一件事。
    ///
    /// `predict_word_from_depth` 传 0（`script_translator` 出厂方案不开预测），
    /// 与 C++ 的 `user_dict_->Lookup(syllable_graph, start, 0, 0)` 一致。
        fn user_dict_candidates(
        &mut self,
        input: &str,
        seg_start: usize,
        ngram_bonus: &dyn Fn(&str) -> f64,
    ) -> Vec<Candidate> {
        if self.user_dict.is_none() {
            return Vec::new();
        }
        // 先把音节图算出来：`build_graph` 借 `&self`，不能和 `user_dict` 的可变借用共存
        let Some((graph, _consumed)) = self.build_graph(input) else {
            return Vec::new();
        };
        let Some(user_dict) = self.user_dict.as_mut() else {
            return Vec::new();
        };
        if !user_dict.loaded() {
            return Vec::new();
        }
        let Some(collector) = user_dict.lookup(&self.dict, &graph, 0, 0, 0, 0.0) else {
            return Vec::new();
        };
        // evo `UserDictionary::DfsState::auxiliary_path` + `RecruitEntry`
        // （`user_dictionary.cc:88-89`）：用户词/已学词与系统词表过同一条 aux 线，
        // 不能出现「系统词被 aux 过滤、用户词照旧全出」。
        let filter_aux = graph.has_auxiliary() && aux_stream_filter_enabled();
        let mut out = Vec::new();
        for (end, iterator) in collector {
            for entry in iterator.into_entries().0 {
                if self.options.blacklist.contains(&entry.text) {
                    continue;
                }
                // 同上（translation.rs 的 peek）：删过的词直接消失，不靠压分。
                if crate::engine::user_ngram::is_blocked_global(&entry.text) {
                    continue;
                }
                if filter_aux && !aux_allows(&graph, 0, &entry.code, &entry.text, end) {
                    continue;
                }
                let quality =
                    entry.code.len() as f64 * QUALITY_SYLLABLE_STRIDE + QUALITY_TIER_USER + entry.weight;
                // 收集器键是段内相对 end;候选跨度补成绝对坐标。
                let mut candidate = Candidate::new(
                    "script",
                    seg_start,
                    seg_start + end,
                    entry.text.clone(),
                    quality + ngram_bonus(&entry.text),
                )
                .with_entry(entry);
                if let Some((preedit, vertices)) = self.syllabify(input, end) {
                    candidate = candidate
                        .with_preedit(preedit)
                        .with_spans(Self::absolute_spans(seg_start, &vertices));
                }
                out.push(candidate);
            }
        }
        // 2026-11-18（juzhong → 居中㕶）：luna 代数把 zhong 派生出 zho，
        // DFS 会沿短边 zho(end=5) 与全拼边 zhong(end=7) 招募**同一条**用户词
        // 「居中」；collector 按 end 升序出列，短边先占据首候选。选中短边后
        // 剩余裸尾巴 ng 进入下一场，候选「嗯; 唔; 㕶;」→ 上屏「居中㕶」。
        // 同一文本只保留覆盖最长的一条（并列取质量高者），让整段输入一次
        // 吃完；其余文本与跨段候选不受影响。
        out.sort_by(|a, b| {
            a.text.cmp(&b.text)
                .then_with(|| b.end.cmp(&a.end))
                .then_with(|| {
                    b.quality
                        .partial_cmp(&a.quality)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });
        out.dedup_by(|a, b| a.text == b.text);
        // 用户词典候选按质量降序(2026-11-01):lookup 顺序≠质量序,
        // 不排序会把高频单字顶在多字词组前面(实机:打过十几遍的词组
        // 被两个 1e12 的单字压住)。整句让位判据也依赖这里的顺序。
        //
        // 2026-11-05（librime-evo 手排）：**手排位置压过质量**。
        // 用户把「拟好」钉在第 2 位，就该排第 2 —— 不管它被学过多少次。
        // 组内仍按质量降序，所以没手排的词相对次序不变。
        out.sort_by(|a, b| {
            let pinned = |c: &Candidate| {
                c.entry
                    .as_ref()
                    .map(|e| e.manual_position > 0)
                    .unwrap_or(false)
            };
            match (pinned(a), pinned(b)) {
                (true, true) => {
                    let pa = a.entry.as_ref().map(|e| e.manual_position).unwrap_or(0);
                    let pb = b.entry.as_ref().map(|e| e.manual_position).unwrap_or(0);
                    pa.cmp(&pb).then_with(|| {
                        b.quality
                            .partial_cmp(&a.quality)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                }
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                (false, false) => b
                    .quality
                    .partial_cmp(&a.quality)
                    .unwrap_or(std::cmp::Ordering::Equal),
            }
        });
        out
    }
}

/// 解析 `prisms`：evo 的 `[{name, option|input_type}]` 或 `{input_type: prism}`。
fn parse_prisms(
    value: &serde_json::Value,
    prefix: &str,
    selections: &mut std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    match value {
        serde_json::Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                let object = item.as_object().ok_or_else(|| {
                    format!("{prefix}/prisms[{index}] 必须是映射")
                })?;
                let prism = object
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("{prefix}/prisms[{index}] 缺少 name"))?;
                let input_type = object
                    .get("option")
                    .or_else(|| object.get("input_type"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("{prefix}/prisms[{index}] 缺少 option/input_type"))?;
                insert_selection(selections, input_type, prism, prefix)?;
            }
        }
        serde_json::Value::Object(map) => {
            for (input_type, value) in map {
                let prism = prism_name_in(value).ok_or_else(|| {
                    format!("{prefix}/prisms.{input_type} 必须是 prism 名字符串或 {{prism: …}}")
                })?;
                insert_selection(selections, input_type, prism, prefix)?;
            }
        }
        _ => return Err(format!("{prefix}/prisms 必须是列表或映射")),
    }
    Ok(())
}

/// 解析字符串引用 `prisms: input_types`。
///
/// 把顶层 `/input_types/options[].option` 里列出的开关名逐个映射到部署索引里的
/// 同名组。主棱镜名（`prism` 单数字段）隐式可用，直接跳过；其余名字必须**恰好**
/// 命中一个组，0 个或 2 个以上都报错并写明名字与命中数量。
fn parse_prism_reference(
    reference: &str,
    config: &serde_json::Value,
    prefix: &str,
    index: Option<&[PrismEntry]>,
    primary_prism: &str,
    selections: &mut std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    if reference.trim() != "input_types" && reference.trim() != "input_type" {
        return Err(format!(
            "{prefix}/prisms: 不支持的字符串引用 '{reference}'（只支持 input_types/input_type）"
        ));
    }
    // 2026-11-13：数据迁移期单数 `input_type` 与复数 `input_types` 都认。
    let node = crate::gear::dict::dict_compiler::input_types_node(config).ok_or_else(|| {
        format!("{prefix}/prisms: 引用了顶层 input_types，但方案里没有该字段")
    })?;
    // 写了 `input_types.enabled` 时，未启用的名字本来就没编产物，**跳过而不报错**
    // （白名单是编译期事实，不是名字写错；名字写错由部署期日志报）。
    let enabled = crate::gear::dict::dict_compiler::input_types_enabled(config);
    let names: Vec<String> = input_type_option_names(node)
        .into_iter()
        .filter(|name| {
            enabled
                .as_ref()
                .is_none_or(|enabled| enabled.iter().any(|item| item == name))
        })
        .collect();
    let Some(entries) = index else {
        return Err(format!(
            "{prefix}/prisms: input_types 引用需要部署索引 {}，但当前部署目录里没有（或已过期）",
            crate::gear::dict::prism_index::INDEX_FILE
        ));
    };
    for name in names {
        // 主棱镜不需要映射：没有同名 option 时本来就落回默认行为。
        if name == primary_prism {
            continue;
        }
        let matches: Vec<&PrismEntry> = entries
            .iter()
            .filter(|entry| entry.group == name)
            .collect();
        match matches.len() {
            0 => {
                let available = entries
                    .iter()
                    .map(|entry| entry.group.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!(
                    "{prefix}/prisms: input_types 里的名字 '{name}' 在部署索引里对应 0 个棱镜组（索引共 {} 组：{available}）",
                    entries.len()
                ));
            }
            1 => insert_selection(selections, &name, &matches[0].prism, prefix)?,
            count => {
                return Err(format!(
                    "{prefix}/prisms: input_types 里的名字 '{name}' 在部署索引里对应 {count} 个棱镜组，无法确定用哪个"
                ));
            }
        }
    }
    Ok(())
}

/// 顶层 `/input_types` 的开关名清单：`options[].option`（也接受 `input_type`），
/// 或直接写成 `{名字: …}` 的映射（排除 enabled/reset/options 三个控制键）。
pub(crate) fn input_type_option_names(node: &serde_json::Value) -> Vec<String> {
    if let Some(options) = node.get("options").and_then(|value| value.as_array()) {
        let names: Vec<String> = options
            .iter()
            .filter_map(|item| {
                item.get("option")
                    .or_else(|| item.get("input_type"))
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            })
            .collect();
        if !names.is_empty() {
            return names;
        }
    }
    node.as_object()
        .map(|map| {
            map.keys()
                .filter(|key| !matches!(key.as_str(), "enabled" | "reset" | "options"))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// 解析 `input_types`：`{input_type: prism}` 或 `{input_type: {prism: …}}`。
fn parse_input_types(
    value: &serde_json::Value,
    prefix: &str,
    selections: &mut std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    let map = value
        .as_object()
        .ok_or_else(|| format!("{prefix}/input_types 必须是映射"))?;
    for (input_type, value) in map {
        let prism = prism_name_in(value).ok_or_else(|| {
            format!("{prefix}/input_types.{input_type} 必须是 prism 名字符串或 {{prism: …}}")
        })?;
        insert_selection(selections, input_type, prism, prefix)?;
    }
    Ok(())
}

fn prism_name_in(value: &serde_json::Value) -> Option<&str> {
    value.as_str().or_else(|| {
        value
            .as_object()
            .and_then(|object| object.get("prism").or_else(|| object.get("name")))
            .and_then(|v| v.as_str())
    })
}

fn insert_selection(
    selections: &mut std::collections::BTreeMap<String, String>,
    input_type: &str,
    prism: &str,
    prefix: &str,
) -> Result<(), String> {
    if input_type.is_empty() || prism.is_empty() {
        return Err(format!("{prefix}: input type 与 prism 名都不能为空"));
    }
    if let Some(existing) = selections.get(input_type) {
        if existing != prism {
            return Err(format!(
                "{prefix}: input type '{input_type}' 同时映射到 '{existing}' 与 '{prism}'"
            ));
        }
    }
    selections.insert(input_type.to_string(), prism.to_string());
    Ok(())
}

impl Translator for ScriptTranslator {
    /// 抄一份完整输入：后置辅助码的尾巴在 `segment.end` 之后，`query` 拿不到。
    fn sync_context(&mut self, ctx: &crate::engine::context::Context) {
        if ctx.input != self.full_input {
            self.full_input.clear();
            self.full_input.push_str(&ctx.input);
        }
        self.auxiliary_mode = !ctx.get_option("auxiliary_off");
        // 阶段 B：evo 的 `prisms[].option` 是 context switch。命中任一开关就切
        // input type；全部关闭时回到主 prism，避免停在最后一个双拼键位。
        if !self.prism_selections.is_empty() {
            match self
                .prism_selections
                .keys()
                .find(|name| ctx.get_option(name))
                .cloned()
            {
                Some(input_type) => self.input_type = input_type,
                None if self.prism_selections.contains_key(&self.input_type) => {
                    self.input_type.clear();
                    let primary = self.primary_prism.clone();
                    self.dict.set_prism(&primary);
                }
                None => {}
            }
        }
    }

    fn query(
        &mut self,
        input: &str,
        segment: &mut Segment,
        history: &CommitHistory,
    ) -> Option<Box<dyn TranslationTrait>> {
        if !self.options.has_any_tag(&segment.tags) {
            return None;
        }
        // 多 prism：按当前 input type 切 prism（空选择表 / 同名时是 O(log n) 空操作），
        // 必须在 `build_graph` 之前 —— prism 决定 `common_prefix_search` 的键集。
        self.apply_prism_selection();
        // 纠错器懒建一次（`enable_correction` 没开时什么都不做）
        self.ensure_correction();

        // ---- 层 1(先算):用户词典——命中整段就不出整句 ------------------
        // C++ `IsExactMatch` 的判据含用户词典词条:打过十几遍的词组再打,
        // 整句必须让位,否则学的永远上不了首选(用户实机报告的根因)。
        let full_len = input.len();
        let user = self.user_dict_candidates(input, segment.start, &|_| 0.0);
        let user_covers_all = user.iter().any(|c| c.end - c.start == full_len);

        // ---- 层 0:整句(poet 组句,最多一条) ------------------------------
        let mut tiers: Vec<Box<dyn TranslationTrait>> = Vec::new();
        if !user_covers_all
            && let Some(sentence) = self.best_sentence(input)
        {
            let consumed = sentence.end_pos;
            let preedit = self.build_preedit(input, consumed);
            // 学词键：整句覆盖 [0, consumed] 的音节路径；切分点用 poet 的成分边界。
            let stops: Vec<usize> = sentence
                .components()
                .iter()
                .map(|(end_pos, _, _)| *end_pos)
                .collect();
            let sentence_code = self.code_for(input, consumed, &stops);
            let mut candidate = Candidate::new(
                "script",
                segment.start,
                segment.start + consumed,
                sentence.text.clone(),
                sentence.segments.len() as f64 * QUALITY_SYLLABLE_STRIDE
                    + QUALITY_TIER_SENTENCE
                    + sentence.weight
                    + self.options.initial_quality,
            )
            .with_entry(crate::gear::dict::vocabulary::DictEntry {
                text: sentence.text.clone(),
                code: sentence_code.unwrap_or_default(),
                ..Default::default()
            })
            .with_components(
                sentence
                    .components()
                    .iter()
                    .map(|(_, text, _)| text.clone())
                    .collect(),
            );
            // 整句的音节顶点：**poet 的分词边界**就是权威切点
            // （C++ 整句候选的 `Spans` 由 `SentenceSyllabifier` 用同一份
            //  components 生成）。没有成分时退化成「整段一个音节」。
            let mut spans = vec![segment.start];
            spans.extend(stops.iter().map(|pos| segment.start + pos));
            if spans.len() < 2 {
                spans.push(segment.start + consumed);
            }
            candidate = candidate.with_spans(spans);
            if !preedit.is_empty() {
                candidate = candidate.with_preedit(preedit);
            }
            tiers.push(Box::new(crate::gear::translator::translation::VecTranslation::new(
                vec![candidate],
            )));
        }

        // 个人 n-gram 加成(青简):上文最后两个"词样"记录 → 词级候选加分。
        // 只加不减、没学过=0,不改变候选集合;整句候选(层 0)不吃这个。
        let (ngram_before, ngram_prev) =
            crate::engine::user_ngram::last_two_words(history);
        let ngram_bonus = |text: &str| -> f64 {
            crate::engine::user_ngram::global().lock().ok().map_or(0.0, |ngram| {
                ngram.bonus(ngram_before.as_deref(), ngram_prev.as_deref(), text, crate::engine::user_ngram::DEFAULT_WEIGHT)
            })
        };

        // ---- 层 1:用户词典 ------------------------------------------------
        let user = self.user_dict_candidates(input, segment.start, &ngram_bonus);
        if !user.is_empty() {
            tiers.push(Box::new(crate::gear::translator::translation::VecTranslation::new(
                user,
            )));
        }

        // ---- 层 2:词表懒迭代(C++ ScriptTranslation 的 kSysPhrase 分支) --
        let full_len = input.len();
        // 词图/收集器都是**段内相对坐标**(0 基);候选的绝对跨度由
        // `segment.start` 在流内补上。之前这里传绝对 start 给
        // `lookup_from_graph`,段起点非 0(affix/内联段)时查空。
        let graph = self.build_graph(input).map(|(g, _)| g).unwrap_or_default();
        // 词级候选流不设桶上限：同码词要能全部翻到，且全拼一个无调拼写会
        // 展开成多个带调音节 id（`guo` → `guo`+`guo1..guo5`），共用一个桶
        // 时顶部 32 条会挤掉 terra 独有的生僻字。上限只留给束搜索。
        let mut collector = self.dict.lookup_from_graph(&graph, 0, None, false, None);
        // direct aux 收敛（evo `dictionary.cc:73-79` 的
        // `SkipNonMatchingAuxiliary` → `AuxiliaryPath::Matches`）：
        // 候选每个音节深度若落在 aux 边上，该深度的字必须在 `allowed_chars` 里。
        // 普通方案没有 aux 边，`has_auxiliary()` 为 false，整趟过滤跳过。
        if graph.has_auxiliary() {
            collector.retain(|end, bucket| {
                bucket.retain(|entry| aux_allows(&graph, 0, &entry.code, &entry.text, *end));
                !bucket.is_empty()
            });
        }
        // preedit 预计算:懒流拿不到 Projection(非 Clone),对**去重后的 end**
        // 各算一次格式化串(一屏候选 end 至多几个),流内查表。键 = 绝对 end。
        // 同一趟顺手记下音节顶点:每个词表候选都要靠它支持 Ctrl+数字按需提交,
        // 而流内再切一次会把这项工作从「O(不同 end 数)」放大成「O(候选数)」。
        let ends: Vec<usize> = collector.keys().copied().collect();
        let mut preedit_by_end = std::collections::HashMap::with_capacity(ends.len());
        let mut spans_by_end = std::collections::HashMap::with_capacity(ends.len());
        for end in ends {
            if let Some((mut preedit, vertices)) = self.syllabify(input, end) {
                self.options.format_preedit(&mut preedit);
                preedit_by_end.insert(segment.start + end, preedit);
                spans_by_end.insert(
                    segment.start + end,
                    Self::absolute_spans(segment.start, &vertices),
                );
            }
        }
        tiers.push(Box::new(crate::gear::translator::translation::ScriptWordsTranslation::new(
            collector,
            segment.start,
            full_len,
            QUALITY_SYLLABLE_STRIDE,
            self.options.initial_quality,
            None,
            preedit_by_end,
            spans_by_end,
            Some((ngram_before.clone(), ngram_prev.clone())),
        )));

        // 后置辅助码（`nihao`m`）：整串是**输入约束**，命中的候选才留。
        // C++ 在 `ScriptTranslation::PrepareCandidate` 里逐条过滤；这里在
        // 交出流之前统一包一层，三层的语义一致。
        let aux = self.auxiliary_suffix(segment);
        if let Some(aux) = aux {
            let filtered: Vec<Box<dyn crate::gear::translator::translation::Translation>> = tiers
                .into_iter()
                .map(|tier| self.apply_aux_filter(tier, &aux))
                .collect();
            tiers = filtered;
        }
        // R-12：个性化（词频衰减 + 最近选择置顶）。开关关闭时**完全不碰**，
        // 连快照都不取 —— 候选与基线逐位一致。
        let mut result: Box<dyn crate::gear::translator::translation::Translation> =
            Box::new(crate::gear::translator::translation::TieredTranslation::new(tiers));
        if self.options.enable_personalization
            && let Some(view) = crate::engine::personalization::snapshot_global()
        {
            // "置顶"靠**结构序**：把最近选择提到最前面。
            // 注意：不能重放 TSV 里存的音节码 —— 那是记录时方案的 id，切方案后
            // 会拼出本方案不存在的候选、并把外方案的码写进用户词典。这里只提升
            // **当前方案候选流里文本相同**的那一条（码/质量原样保留），找不到就不置顶。
            result = crate::engine::personalization::promote_pinned_text(result, &view, input);
            result = Box::new(crate::engine::personalization::PreferenceTranslation::new(
                result, view, input,
            ));
        }
        Some(result)
    }

    /// `ScriptTranslator::Memorize`：把这次提交里选中的词条逐条
    /// `UpdateEntry(entry, 1)` 写进用户词典(懒化重构时曾随 stash 丢失,2026-10-31 恢复)。
    fn memorize(&mut self, elements: &[DictEntry], _history: &CommitHistory) -> bool {
        // R-12：先记个人化（不依赖用户词典是否可用；`code` 为空的整句分词成分跳过）。
        if self.options.enable_personalization && !self.full_input.is_empty() {
            for entry in elements.iter().filter(|entry| !entry.code.is_empty()) {
                crate::engine::personalization::record_choice_global(
                    &self.full_input,
                    &entry.text,
                    &entry.code,
                );
            }
        }
        let Some(user_dict) = self.user_dict.as_mut() else {
            return false;
        };
        if !user_dict.loaded() || user_dict.readonly() {
            return false;
        }
        let mut written = false;
        for entry in elements {
            if user_dict.update_entry(&self.dict, entry, 1) {
                written = true;
            }
        }
        written
    }
}

/// 回退开关：`WEASEL_AUX_FILTER=off`（或 `0` / `false`）整体关掉本轮新接的
/// 「候选流 aux 过滤」——用户词典层与整句词图都不再按 aux 收敛。
///
/// 关掉后行为与接线前完全一致（系统词表的 direct aux 过滤是既有行为，不受此开关影响，
/// 它仍由 `graph.has_auxiliary()` 决定）。`auxiliary_off` / `auxiliary_mode=false`
/// 则是更上一层的总闸：连 aux 边都不注入，自然也不会过滤。
fn aux_stream_filter_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        !matches!(
            std::env::var("WEASEL_AUX_FILTER").as_deref(),
            Ok("off") | Ok("0") | Ok("false")
        )
    })
}

/// direct aux 过滤：候选 `text` 沿 `code` 走音节图时，凡是落在带
/// `auxiliary_chars` 的边上，对应深度的字必须命中允许集。
///
/// 语义等价于 evo `AuxiliaryPath::Matches(text)`（`syllabifier.cc:366-380`）：
/// 区别只是我们的过滤发生在 collector 出桶之后，而不是 accessor 下推
/// （`table.rs` / `dictionary.rs` 的既有函数体本轮不动）。
///
/// 无 aux 的步照常通过；路径必须恰好走到 `end`（段内相对字节位置）。
fn aux_allows(
    graph: &SyllableGraph,
    start: usize,
    code: &[SyllableId],
    text: &str,
    end: usize,
) -> bool {
    let characters: Vec<char> = text.chars().collect();
    fn dfs(
        graph: &SyllableGraph,
        pos: usize,
        depth: usize,
        code: &[SyllableId],
        characters: &[char],
        end: usize,
    ) -> bool {
        if depth == code.len() {
            return pos == end;
        }
        if pos > end {
            return false;
        }
        let Some(index) = graph.indices().get(&pos) else {
            return false;
        };
        let Some(spellings) = index.get(&code[depth]) else {
            return false;
        };
        for props in spellings {
            let next = props.end_pos();
            if next > end || next <= pos {
                continue;
            }
            // aux 边：候选在该深度的字必须在允许集里（`AuxiliaryPath::Matches`）。
            if let Some(allowed) = props.auxiliary_chars() {
                match characters.get(depth) {
                    Some(character) if allowed.contains(character) => {}
                    _ => continue,
                }
            }
            if dfs(graph, next, depth + 1, code, characters, end) {
                return true;
            }
        }
        false
    }
    dfs(graph, start, 0, code, &characters, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实词典夹具（`luna_pinyin.table.bin` + `luna_pinyin_simp.prism.bin`），
    /// 缺夹具时静默跳过 —— 与其它真实数据测试同风格。
    fn real_dict() -> Option<Dictionary> {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/e2e/build");
        Dictionary::open(&dir, "luna_pinyin", "luna_pinyin_simp")
    }

    /// direct aux 最小闭环：`zhong`（音） + `b`（辅助码）→ 收敛到「中」。
    ///
    /// 证明三件事：
    /// 1. 构图期真的注入了 aux 边（`edges[0][6][zhong]` 带 `allowed_chars = {中}`）；
    /// 2. 词典查询能带出跨"基音节 + 辅助码"的候选；
    /// 3. 过滤把同音字（种 / 重）挡在满覆盖桶之外。
    ///
    /// 装配方式：`DictCompiler` 现场编一份最小词库，`AuxiliaryDb::compile`
    /// 现场编一份 aux 字典 —— 与部署产物同格式。
    #[test]
    fn direct_auxiliary_code_narrows_to_the_character() {
        use crate::algo::SpellingType;
        use crate::gear::dict::auxiliary_db::{AuxiliaryCharacter, AuxiliaryCode, AuxiliaryDb};
        use crate::gear::dict::dict_compiler::DictCompiler;
        use crate::engine::commit_history::CommitHistory;
        use crate::engine::segmentation::Segment;
        use std::collections::{BTreeMap, BTreeSet};

        let root = std::env::temp_dir().join(format!(
            "librime-next-direct-aux-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let user = root.join("user");
        let shared = root.join("shared");
        let staging = user.join("build");
        for dir in [&user, &shared, &staging] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // 词库：三个同音字共用 `zhong`，辅助码才有收敛空间。
        let dict_dir = shared.join("dict/demo");
        std::fs::create_dir_all(&dict_dir).unwrap();
        std::fs::write(
            dict_dir.join("demo.dict.yaml"),
            "---\nname: demo\nversion: \"1.0\"\nsort: by_weight\ncolumns:\n  - text\n  - code\n...\n中\tzhong\n种\tzhong\n重\tzhong\n",
        )
        .unwrap();
        // 方案：`translator.aux` 指组名，`build.auxiliary` 是部署期语法（本测试
        // 直接编 aux 产物，字段保留以反映真实 schema 形态）。
        let schema_dir = shared.join("schema/demo");
        std::fs::create_dir_all(&schema_dir).unwrap();
        let schema_src = schema_dir.join("demo.schema.yaml");
        std::fs::write(
            &schema_src,
            "schema:\n  schema_id: demo\ntranslator:\n  dictionary: demo\n  aux: demo_aux\nbuild:\n  auxiliary:\n    dictionary: demo_aux\n",
        )
        .unwrap();

        let mut compiler = DictCompiler::new(
            "demo",
            "",
            Vec::new(),
            &user,
            &shared,
            &staging,
            shared.join("prebuilt"),
        );
        compiler
            .compile(Some(&schema_src))
            .unwrap_or_else(|e| panic!("compile demo: {e}"));

        // aux 字典：「中」的精确辅助码 `b`（与 AuxiliaryCompiler 产物同格式）。
        let by_character = BTreeMap::from([(
            "中".to_string(),
            vec![AuxiliaryCode {
                code: "b".to_string(),
                spelling_type: SpellingType::Normal,
                credibility: 0.0,
            }],
        )]);
        let by_code = BTreeMap::from([(
            "b".to_string(),
            vec![AuxiliaryCharacter {
                text: "中".to_string(),
                spelling_type: SpellingType::Normal,
                credibility: 0.0,
            }],
        )]);
        let aux_db = AuxiliaryDb::compile(&staging.join("demo_aux.aux.bin"), &by_character, &by_code, 1)
            .expect("compile aux db");

        let dict = Dictionary::open(&staging, "demo", "demo").expect("open demo dict");
        let mut options = TranslatorOptions::default();
        options.tags = vec!["abc".to_string()];
        let mut translator = ScriptTranslator::with_options(dict, options).with_auxiliary(
            "",
            None,
            Some(std::sync::Arc::new(aux_db)),
        );

        // 1) 图里必须有 aux 边：`[0, 6)` → 基音节 zhong，允许字集 {中}。
        let (graph, _) = translator.build_graph("zhongb").expect("build graph");
        assert!(graph.has_auxiliary(), "direct aux 边应已注入音节图");
        let zhong = syllable_id(&translator.dict, "zhong").expect("zhong 应是合法音节");
        let allowed = graph
            .edges
            .get(&0)
            .and_then(|ends| ends.get(&6))
            .and_then(|spellings| spellings.get(&zhong))
            .and_then(|props| props.auxiliary_chars())
            .cloned();
        assert_eq!(allowed, Some(BTreeSet::from(['中'])), "aux 边允许字集应为「中」");

        // 2) + 3) 查询：满覆盖（end = 6）候选只能剩「中」。
        translator.full_input = "zhongb".to_string();
        let mut segment = Segment::new(0, 6);
        segment.tags = vec!["abc".to_string()];
        let mut translation = translator
            .query("zhongb", &mut segment, &CommitHistory::new())
            .expect("query 应有结果");
        let mut full_span = Vec::new();
        while let Some(candidate) = translation.peek() {
            if candidate.end == 6 {
                full_span.push(candidate.text.clone());
            }
            translation.next();
        }
        assert_eq!(full_span, vec!["中".to_string()], "基础码 + 辅助码应收敛到「中」");

        std::fs::remove_dir_all(&root).ok();
    }

    /// 用户词典层同样受 direct aux 约束（evo `user_dictionary.cc:88-89`）。
    ///
    /// 场景：`zhongb` 的辅助码 `b` 只允许「中」；用户又恰好把同音的「种」学过很多遍。
    /// 接线前用户词层不看 aux，「种」会从用户词典桶里照旧冒出来（甚至因权重更高排前面）；
    /// 接线后必须与系统词表一致地被挡掉。
    #[test]
    fn user_dictionary_respects_direct_auxiliary() {
        use crate::algo::SpellingType;
        use crate::gear::dict::auxiliary_db::{AuxiliaryCharacter, AuxiliaryCode, AuxiliaryDb};
        use crate::gear::dict::level_db::LevelDb;
        use std::collections::BTreeMap;

        let Some(dict) = real_dict() else {
            return;
        };
        let root = std::env::temp_dir().join(format!(
            "librime-next-user-aux-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("user")).unwrap();

        let zhong = syllable_id(&dict, "zhong").expect("zhong 应是合法音节");
        let db = LevelDb::shared_user_db(&root.join("user"), "luna_pinyin");
        let mut user_dict = UserDictionary::new("luna_pinyin", db);
        assert!(user_dict.load(), "userdb 应能打开");
        let entry = DictEntry {
            text: "种".to_string(),
            code: Code::from_slice(&[zhong]),
            weight: 100.0,
            ..Default::default()
        };
        assert!(
            user_dict.update_entry(&dict, &entry, 20),
            "「种」应能写进 userdb"
        );

        let by_character = BTreeMap::from([(
            "中".to_string(),
            vec![AuxiliaryCode {
                code: "b".to_string(),
                spelling_type: SpellingType::Normal,
                credibility: 0.0,
            }],
        )]);
        let by_code = BTreeMap::from([(
            "b".to_string(),
            vec![AuxiliaryCharacter {
                text: "中".to_string(),
                spelling_type: SpellingType::Normal,
                credibility: 0.0,
            }],
        )]);
        let aux_db = AuxiliaryDb::compile(root.join("luna.aux.bin"), &by_character, &by_code, 1)
            .expect("compile aux db");

        let mut options = TranslatorOptions::default();
        options.tags = vec!["abc".to_string()];
        let mut translator = ScriptTranslator::with_options(dict, options)
            .with_user_dict(user_dict)
            .with_auxiliary("", None, Some(std::sync::Arc::new(aux_db)));

        // 不带辅助码：用户词「种」确实在（证明 userdb 生效，下面的断言不是空转）。
        let plain = query_candidates(&mut translator, "zhong");
        assert!(
            plain
                .iter()
                .any(|c| c.text == "种" && c.entry.as_ref().is_some_and(|e| e.commit_count >= 1)),
            "不带辅助码时用户词「种」应出现"
        );

        // 原始 userdb 查询确实会在满覆盖桶（end = 6）招到「种」，
        // 下面的查询结果里它必须消失 —— 否则断言是空转的。
        let (graph, _) = translator.build_graph("zhongb").expect("build graph");
        assert!(graph.has_auxiliary());
        let raw = translator
            .user_dict
            .as_mut()
            .expect("应有用户词典")
            .lookup(&translator.dict, &graph, 0, 0, 0, 0.0)
            .expect("userdb 应能查到「种」");
        let recruited_at_full_span = raw
            .into_iter()
            .filter(|(end, _)| *end == 6)
            .flat_map(|(_, iterator)| iterator.into_entries().0)
            .any(|entry| entry.text == "种");
        assert!(
            recruited_at_full_span,
            "接线前的 userdb 查询应把「种」招进 end=6（用户词层确实参与）"
        );

        // 带辅助码：满覆盖桶里不能有「种」；「中」必须还在。
        let narrowed = query_candidates(&mut translator, "zhongb");
        assert!(
            !narrowed
                .iter()
                .any(|c| c.end == 6 && c.text == "种"),
            "用户词「种」不应从 end=6 漏出来（用户词层也必须过 aux）"
        );
        assert!(
            narrowed.iter().any(|c| c.end == 6 && c.text == "中"),
            "「中」应仍在满覆盖桶里"
        );

        drop(translator);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn correction_config_defaults_off_and_reads_from_schema() {
        assert!(
            !TranslatorOptions::default().enable_correction,
            "纠错默认关（与上游一致：要方案作者主动开）"
        );
        let config: serde_json::Value = serde_json::from_str(
            r#"{"translator": {"enable_correction": true, "correction_tolerance": 2}}"#,
        )
        .unwrap();
        let options = TranslatorOptions::from_config(&config, "translator");
        assert!(options.enable_correction);
        assert_eq!(options.correction_tolerance, 2);
        // 容差有个上限，配得再大也不让噪声无限膨胀
        let config: serde_json::Value =
            serde_json::from_str(r#"{"translator": {"correction_tolerance": 99}}"#).unwrap();
        assert!(TranslatorOptions::from_config(&config, "translator").correction_tolerance <= 3);
    }

    /// 端到端：把「nihao」打成「nihap」（`o` 敲到邻键 `p`）时，开了纠错整串能被
    /// 音节图解释完；关着就只能解释到一半。
    #[test]
    fn correction_saves_a_mistyped_syllable() {
        let Some(dict) = real_dict() else {
            return;
        };
        let input = "nihap";
        // `Dictionary` 不 `Clone`，两遍各自打开一次
        let Some(dict) = real_dict() else {
            return;
        };
        let hao = syllable_id(&dict, "hao");
        let mut plain = ScriptTranslator::new(dict);
        assert!(
            !edges_have(&plain.build_graph(input).unwrap().0, 2, 5, hao),
            "关着纠错时「hap」不该被解释成「hao」"
        );

        let Some(dict) = real_dict() else {
            return;
        };
        let mut options = TranslatorOptions::default();
        options.enable_correction = true;
        let mut translator = ScriptTranslator::with_options(dict, options);
        translator.ensure_correction();
        let Some(corrector) = translator.correction.as_ref() else {
            panic!("开了 enable_correction 就该建出纠错器（音节表来自 Table）");
        };
        assert!(corrector.syllabary_len() > 0, "音节表不该为空");
        // id 用 **table 的音节 id**：图里 `spellings` 的键是 `query_spelling` 出来的
        // syllable_id，而不是 prism 的 spelling_id（`tolerance_search` 返回的那个）。
        let hao = syllable_id(&translator.dict, "hao");
        let (g, _) = translator.build_graph(input).unwrap();
        assert!(
            edges_have(&g, 2, 5, hao),
            "「nihap」的 `hap` 应被纠成 `hao`（p 是 o 的邻键）"
        );
        // 反过来：不是邻键的替换不该被当成误触
        let mao = syllable_id(&translator.dict, "mao");
        if let Some(mao) = mao {
            let (g, _) = translator.build_graph("nihaz").unwrap();
            assert!(!edges_have(&g, 2, 5, Some(mao)), "「z」不是「o」的邻键，不该纠");
        }
    }

    #[test]
    fn near_correction_config_defaults_off_and_reads_from_schema() {
        assert!(
            !TranslatorOptions::default().enable_near_correction,
            "R-04 邻键纠错旁路默认关"
        );
        let config: serde_json::Value =
            serde_json::from_str(r#"{"translator": {"enable_near_correction": true}}"#).unwrap();
        assert!(
            TranslatorOptions::from_config(&config, "translator").enable_near_correction,
            "方案里写 enable_near_correction: true 应能打开"
        );
    }

    /// 开关关闭时，音节图与候选流必须和基线**逐位一致**（R-04 的"行为不变"证据）。
    #[test]
    fn near_correction_default_off_matches_baseline() {
        let Some(dict) = real_dict() else {
            return;
        };
        let Some(dict_off) = real_dict() else {
            return;
        };
        let input = "nihap";
        let mut baseline = ScriptTranslator::new(dict);
        baseline.options.tags = vec!["abc".to_string()];
        let (baseline_graph, baseline_consumed) =
            baseline.build_graph(input).expect("基线应能建图");
        let mut options = TranslatorOptions::from_config(&serde_json::json!({}), "translator");
        options.tags = vec!["abc".to_string()];
        assert!(!options.enable_near_correction);
        let mut off = ScriptTranslator::with_options(dict_off, options);
        let (off_graph, off_consumed) = off.build_graph(input).expect("开关关闭应能建图");
        assert_eq!(off_consumed, baseline_consumed, "consumed 必须一致");
        assert_eq!(
            format!("{off_graph:?}"),
            format!("{baseline_graph:?}"),
            "开关关闭时音节图必须与基线逐位一致"
        );

        // 候选流（文本 + quality 位模式 + 区间）也逐项一致。
        let stream = |translator: &mut ScriptTranslator| {
            let mut segment = Segment::new(0, input.len());
            segment.tags = vec!["abc".to_string()];
            let mut history = CommitHistory::new();
            let mut translation = translator
                .query(input, &mut segment, &mut history)
                .expect("query 应有结果");
            let mut out = Vec::new();
            while let Some(candidate) = translation.peek() {
                out.push((
                    candidate.text.clone(),
                    candidate.quality.to_bits(),
                    candidate.start,
                    candidate.end,
                ));
                translation.next();
            }
            out
        };
        assert_eq!(
            stream(&mut off),
            stream(&mut baseline),
            "开关关闭时候选顺序/分数必须与基线一致"
        );
    }

    /// 端到端：`nihap` 的 `hap` 在 trie 上按邻键纠成 `hao`（p 是 o 的邻键），
    /// 且纠错边带惩罚；打对时不产生纠错边。
    #[test]
    fn near_correction_bridges_mistyped_syllable() {
        let Some(dict) = real_dict() else {
            return;
        };
        let mut options = TranslatorOptions::default();
        options.enable_near_correction = true;
        let mut translator = ScriptTranslator::with_options(dict, options);
        let hao = syllable_id(&translator.dict, "hao");
        let (bad, _) = translator.build_graph("nihap").expect("「nihap」应能建图");
        assert!(
            edges_have(&bad, 2, 5, hao),
            "邻键纠错应把「hap」纠成「hao」"
        );
        assert!(has_correction_edge(&bad), "纠错边应带惩罚（排在精确匹配之后）");
        let (good, _) = translator.build_graph("nihao").expect("「nihao」应能建图");
        assert!(!has_correction_edge(&good), "打对时不该出现纠错边");
        // 非邻键的替换不算误触。
        if let Some(mao) = syllable_id(&translator.dict, "mao") {
            let (other, _) = translator.build_graph("nihaz").expect("「nihaz」应能建图");
            assert!(
                !edges_have(&other, 2, 5, Some(mao)),
                "「z」不是「o」的邻键，不该纠成「mao」"
            );
        }
    }

    #[test]
    fn personalization_config_defaults_on_and_reads_from_schema() {
        assert!(
            TranslatorOptions::default().enable_personalization,
            "R-12 个性化默认开（空表时是 no-op）"
        );
        let config: serde_json::Value =
            serde_json::from_str(r#"{"translator": {"enable_personalization": true}}"#).unwrap();
        assert!(
            TranslatorOptions::from_config(&config, "translator").enable_personalization,
            "方案里写 enable_personalization: true 应能打开"
        );
    }

    /// 开关关闭时，候选流必须和基线**逐项一致**（R-12 的"行为不变"证据）。
    #[test]
    fn personalization_default_off_matches_baseline() {
        let Some(dict) = real_dict() else {
            return;
        };
        let Some(dict_off) = real_dict() else {
            return;
        };
        let input = "nihao";
        let mut baseline = ScriptTranslator::new(dict);
        baseline.options.tags = vec!["abc".to_string()];
        // 显式关掉才走基线；默认值已改成 true（空表 no-op）。
        let mut options = TranslatorOptions::from_config(
            &serde_json::json!({"translator": {"enable_personalization": false}}),
            "translator",
        );
        options.tags = vec!["abc".to_string()];
        assert!(!options.enable_personalization);
        let mut off = ScriptTranslator::with_options(dict_off, options);
        let stream = |translator: &mut ScriptTranslator| {
            let _ = translator.build_graph(input);
            let mut segment = Segment::new(0, input.len());
            segment.tags = vec!["abc".to_string()];
            let mut history = CommitHistory::new();
            let mut translation = translator
                .query(input, &mut segment, &mut history)
                .expect("query 应有结果");
            let mut out = Vec::new();
            while let Some(candidate) = translation.peek() {
                out.push((
                    candidate.text.clone(),
                    candidate.quality.to_bits(),
                    candidate.start,
                    candidate.end,
                ));
                translation.next();
            }
            out
        };
        assert_eq!(
            stream(&mut off),
            stream(&mut baseline),
            "个性化关闭时候选顺序/分数必须与基线一致"
        );
    }

    /// 阶段 A：translator 按 input type 在两个 prism 间切换；
    /// 全拼 `zhongguo` 与双拼 `vsgo` 都切成同一对音节（zhong / guo）。
    #[test]
    fn input_type_selects_prism_for_full_and_double_pinyin() {
        use crate::gear::dict::dict_compiler::DictCompiler;
        use crate::gear::dict::prism::Prism;

        let root = std::env::temp_dir().join("librime-translator-multi-prism");
        let _ = std::fs::remove_dir_all(&root);
        let user = root.join("user");
        let shared = root.join("shared");
        let staging = root.join("build");
        for dir in [&user, &shared, &staging] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let dict_dir = shared.join("dict/demo");
        std::fs::create_dir_all(&dict_dir).unwrap();
        std::fs::write(
            dict_dir.join("demo.dict.yaml"),
            "---\nname: demo\nversion: \"1.0\"\nsort: by_weight\ncolumns:\n  - text\n  - code\n...\n中国\tzhong guo\n",
        )
        .unwrap();
        let schema_src = shared.join("schema/demo/demo.schema.yaml");
        std::fs::create_dir_all(schema_src.parent().unwrap()).unwrap();
        std::fs::write(
            &schema_src,
            "schema:\n  schema_id: demo\ntranslator:\n  dictionary: demo\n",
        )
        .unwrap();

        let mut compiler = DictCompiler::new(
            "demo",
            "",
            Vec::new(),
            &user,
            &shared,
            &staging,
            shared.join("prebuilt"),
        );
        let mut extras = std::collections::BTreeMap::new();
        extras.insert(
            "double".to_string(),
            vec![
                "xform/^zhong$/vs/".to_string(),
                "xform/^guo$/go/".to_string(),
            ],
        );
        compiler.set_extra_prisms(extras);
        compiler.compile(Some(&schema_src)).expect("compile");

        let mut dict = Dictionary::open(&staging, "demo", "demo").expect("open dict");
        let zhong = syllable_id(&dict, "zhong");
        let guo = syllable_id(&dict, "guo");
        let extra =
            Prism::open(&staging.join("demo/demo.double.prism.bin")).expect("extra prism");
        assert!(dict.add_prism("demo.double", extra));

        let mut translator = ScriptTranslator::new(dict);
        translator.set_prism_selection("full", "demo");
        translator.set_prism_selection("double", "demo.double");

        assert!(translator.set_input_type("double"));
        let (graph, consumed) = translator.build_graph("vsgo").expect("双拼应能建图");
        assert_eq!(consumed, 4);
        assert!(edges_have(&graph, 0, 2, zhong), "vs → zhong");
        assert!(edges_have(&graph, 2, 4, guo), "go → guo");

        assert!(translator.set_input_type("full"));
        let (graph, consumed) = translator.build_graph("zhongguo").expect("全拼应能建图");
        assert_eq!(consumed, 8);
        assert!(edges_have(&graph, 0, 5, zhong), "zhong → zhong");
        assert!(edges_have(&graph, 5, 8, guo), "guo → guo");

        std::fs::remove_dir_all(&root).ok();
    }

    /// `Table` 里某个拼写对应的**音节 id**。
    fn syllable_id(dict: &Dictionary, spelling: &str) -> Option<SyllableId> {
        dict.table
            .syllabary()?
            .syllable_to_id()
            .into_iter()
            .find(|(text, _)| *text == spelling)
            .map(|(_, id)| id)
    }

    /// 音节图里 `start → end` 这条边上有没有 `syllable` 这个音节。
    fn edges_have(
        graph: &SyllableGraph,
        start: usize,
        end: usize,
        syllable: Option<SyllableId>,
    ) -> bool {
        let Some(syllable) = syllable else {
            return false;
        };
        graph
            .edges
            .get(&start)
            .and_then(|ends| ends.get(&end))
            .is_some_and(|spellings| spellings.contains_key(&syllable))
    }

    /// 纠错不能抢位：`syllabifier` 给纠出来的拼写打 `is_correction` +
    /// `credibility = log(0.01)`，所以「打错才有的候选」一定比「打对就有的」低。
    #[test]
    fn corrected_spellings_are_penalized() {
        let Some(dict) = real_dict() else {
            return;
        };
        let mut options = TranslatorOptions::default();
        options.enable_correction = true;
        let mut translator = ScriptTranslator::with_options(dict, options);
        translator.ensure_correction();
        // 打对的：整串可解释，且没有一条边是 correction
        let (good, _) = translator.build_graph("nihao").expect("「nihao」应能建图");
        assert!(
            !has_correction_edge(&good),
            "打对时不该出现纠错边"
        );
        // 打错的：纠错边存在，且 credibility 是惩罚值
        let (bad, _) = translator.build_graph("nihap").expect("「nihap」应能建图");
        assert!(has_correction_edge(&bad), "打错时应出现纠错边");
    }

    /// 判据用 `credibility`：纠错的拼写被罚 `log(0.01)`（`is_correction` 是
    /// `algo` 模块内可见的字段，这里只能看分数）。
    fn has_correction_edge(graph: &SyllableGraph) -> bool {
        graph
            .edges
            .values()
            .flat_map(|ends| ends.values())
            .flat_map(|spellings| spellings.values())
            .any(|props| {
                props.credibility <= crate::algo::syllabifier::K_CORRECTION_CREDIBILITY
            })
    }

    // ---- 阶段 B：配置语法 ----

    /// `translator/prisms` 的 evo 形态、简写形态、变体命名空间与冲突降级。
    #[test]
    fn prism_config_parses_evo_and_shorthand_forms() {
        let Some(dict) = real_dict() else {
            return;
        };
        let mut translator = ScriptTranslator::new(dict);
        // `prism`（主 prism）与 `prisms` 共存：旧方案加多 prism 时不应被整体禁用。
        let config = serde_json::json!({
            "translator": {
                "prism": "luna_pinyin",
                "prisms": [
                    {"name": "luna_pinyin.ziranma", "option": "ziranma"},
                    {"name": "luna_pinyin.flypy", "input_type": "flypy"}
                ],
                "input_types": {"full": "luna_pinyin"}
            }
        });
        translator
            .apply_prism_config(&config, "translator", None)
            .expect("合法配置应解析成功");
        assert_eq!(translator.prism_selections.len(), 3);
        assert_eq!(
            translator.prism_selections["ziranma"],
            "luna_pinyin.ziranma"
        );
        assert_eq!(translator.prism_selections["flypy"], "luna_pinyin.flypy");
        assert_eq!(translator.prism_selections["full"], "luna_pinyin");

        // 变体命名空间：`simp` 读 `/translator/simp`。
        let Some(dict) = real_dict() else {
            return;
        };
        let mut variant = ScriptTranslator::new(dict);
        let variant_config = serde_json::json!({
            "translator": {"simp": {"input_types": {"full": "luna_pinyin_simp"}}}
        });
        variant
            .apply_prism_config(&variant_config, "simp", None)
            .expect("变体配置应解析成功");
        assert_eq!(
            variant.prism_selections["full"],
            "luna_pinyin_simp"
        );

        // 同一 input type 映射到不同 prism → 报错（不静默覆盖）。
        let duplicate = serde_json::json!({
            "translator": {
                "prisms": {"double": "a"},
                "input_types": {"double": "b"}
            }
        });
        let Some(dict) = real_dict() else {
            return;
        };
        let mut duplicate_translator = ScriptTranslator::new(dict);
        assert!(
            duplicate_translator
                .apply_prism_config(&duplicate, "translator", None)
                .is_err()
        );
    }

    // ---- 阶段 B：部署索引自动认 ----

    fn prism_index_fixture() -> Vec<PrismEntry> {
        vec![
            PrismEntry {
                group: "flypy".to_string(),
                prism: "luna_pinyin.flypy".to_string(),
            },
            PrismEntry {
                group: "zrm".to_string(),
                prism: "luna_pinyin.zrm".to_string(),
            },
        ]
    }

    /// 没有 `prisms` / `input_types` 时按索引注册全部名字；索引缺失退回旧行为；
    /// 显式名单优先于索引。
    #[test]
    fn prisms_auto_discovered_from_deploy_index() {
        let Some(dict) = real_dict() else {
            return;
        };
        let index = prism_index_fixture();
        let bare = serde_json::json!({"translator": {"dict": "luna_pinyin"}});

        let mut auto = ScriptTranslator::new(dict);
        auto.apply_prism_config(&bare, "translator", Some(&index))
            .expect("索引自动注册应成功");
        assert_eq!(auto.prism_selections.len(), 2);
        assert_eq!(auto.prism_selections["zrm"], "luna_pinyin.zrm");
        assert_eq!(auto.prism_selections["flypy"], "luna_pinyin.flypy");

        // 旧部署：没有索引 → 只有主 prism，不报错。
        let mut legacy = ScriptTranslator::new(real_dict().expect("fixture"));
        legacy
            .apply_prism_config(&bare, "translator", None)
            .expect("无索引应回退旧行为");
        assert!(legacy.prism_selections.is_empty());

        // 手写名单优先：索引里的 flypy 不应被额外注册（这里只写了 zrm 一条）。
        let mut explicit = ScriptTranslator::new(real_dict().expect("fixture"));
        explicit
            .apply_prism_config(
                &serde_json::json!({
                    "translator": {"prisms": [{"name": "luna_pinyin.zrm", "option": "zrm"}]}
                }),
                "translator",
                Some(&index),
            )
            .expect("显式名单应解析成功");
        assert_eq!(explicit.prism_selections.len(), 1);
        assert_eq!(explicit.prism_selections["zrm"], "luna_pinyin.zrm");
    }

    /// 字符串引用 `prisms: input_types`：展开顶层 `options[].option`，按索引同名组
    /// 映射；主棱镜名跳过；对不上任何组时错误要带名字与 0 个组的计数。
    #[test]
    fn prisms_string_reference_expands_input_types() {
        let index = prism_index_fixture();
        let Some(dict) = real_dict() else {
            return;
        };
        let config = serde_json::json!({
            "translator": {"prisms": "input_types"},
            "input_types": {"options": [
                {"option": "luna_pinyin_simp"}, // 主棱镜，隐式可用 → 跳过
                {"option": "zrm"},
                {"option": "flypy"}
            ]}
        });
        let mut translator = ScriptTranslator::new(dict);
        translator
            .apply_prism_config(&config, "translator", Some(&index))
            .expect("字符串引用应展开成功");
        assert_eq!(translator.prism_selections.len(), 2);
        assert_eq!(translator.prism_selections["zrm"], "luna_pinyin.zrm");

        // 名字对应不到任何组：错误里要有名字与命中数。
        let missing = serde_json::json!({
            "translator": {"prisms": "input_types"},
            "input_types": {"options": [{"option": "missing_type"}]}
        });
        let error = ScriptTranslator::new(real_dict().expect("fixture"))
            .apply_prism_config(&missing, "translator", Some(&index))
            .expect_err("对不上组应报错");
        assert!(error.contains("missing_type"), "错误应包含名字: {error}");
        assert!(error.contains("0 个棱镜组"), "错误应包含命中数: {error}");

        // 旧部署没有索引：明确说明缺 prisms.list，仍保持单 prism。
        let no_index = ScriptTranslator::new(real_dict().expect("fixture"))
            .apply_prism_config(&config, "translator", None)
            .expect_err("字符串引用无索引应报错");
        assert!(no_index.contains("prisms.list"), "错误应提示索引文件: {no_index}");
    }

    // ---- 阶段 C：真实 userdb 双向落盘 ----

    /// 现场编一份「全拼主 prism + 双拼额外 prism」的词库。
    ///
    /// 表里放两个同码词：`重过` 权重高（系统层默认排前），`中国` 权重低。
    /// 学完 `中国` 后它必须靠用户词层排到 `重过` 前面。
    fn prepare_multi_prism_dict(tag: &str) -> Option<std::path::PathBuf> {
        use crate::gear::dict::dict_compiler::DictCompiler;

        let root = std::env::temp_dir().join(format!(
            "librime-learn-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let user = root.join("user");
        let shared = root.join("shared");
        let staging = root.join("build");
        for dir in [&user, &shared, &staging] {
            std::fs::create_dir_all(dir).ok()?;
        }
        let dict_dir = shared.join("dict/demo");
        std::fs::create_dir_all(&dict_dir).ok()?;
        std::fs::write(
            dict_dir.join("demo.dict.yaml"),
            "---\nname: demo\nversion: \"1.0\"\nsort: by_weight\ncolumns:\n  - text\n  - code\n  - weight\n...\n重过\tzhong guo\t100\n中国\tzhong guo\t1\n",
        )
        .ok()?;
        let schema_src = shared.join("schema/demo/demo.schema.yaml");
        std::fs::create_dir_all(schema_src.parent()?).ok()?;
        std::fs::write(
            &schema_src,
            "schema:\n  schema_id: demo\ntranslator:\n  dictionary: demo\n",
        )
        .ok()?;

        let mut compiler = DictCompiler::new(
            "demo",
            "",
            Vec::new(),
            &user,
            &shared,
            &staging,
            shared.join("prebuilt"),
        );
        let mut extras = std::collections::BTreeMap::new();
        extras.insert(
            "double".to_string(),
            vec![
                "xform/^zhong$/vs/".to_string(),
                "xform/^guo$/go/".to_string(),
            ],
        );
        compiler.set_extra_prisms(extras);
        compiler.compile(Some(&schema_src)).ok()?;
        Some(root)
    }

    /// 打开词库、额外 prism 与真实 LevelDB 用户词典。
    fn open_multi_prism_translator(root: &std::path::Path) -> ScriptTranslator {
        use crate::gear::dict::level_db::LevelDb;
        use crate::gear::dict::prism::Prism;

        let staging = root.join("build");
        let mut dict = Dictionary::open(&staging, "demo", "demo").expect("open demo dict");
        let extra = Prism::open(&staging.join("demo/demo.double.prism.bin"))
            .expect("open extra prism");
        assert!(dict.add_prism("demo.double", extra));
        let db = LevelDb::shared_user_db(&root.join("user"), "demo");
        let mut user_dict = UserDictionary::new("demo", db);
        assert!(user_dict.load(), "userdb 应能打开");
        let mut translator = ScriptTranslator::new(dict).with_user_dict(user_dict);
        // 与其它 translator 测试一致：`abc` 标签让 `query` 认领该段。
        translator.options.tags = vec!["abc".to_string()];
        translator.set_prism_selection("full", "demo");
        translator.set_prism_selection("double", "demo.double");
        translator
    }

    fn query_candidates(translator: &mut ScriptTranslator, input: &str) -> Vec<Candidate> {
        let mut segment = Segment::new(0, input.len());
        segment.tags = vec!["abc".to_string()];
        let mut history = CommitHistory::new();
        let mut translation = translator
            .query(input, &mut segment, &mut history)
            .expect("query 应有结果");
        let mut out = Vec::new();
        while let Some(candidate) = translation.peek() {
            out.push(candidate.clone());
            translation.next();
        }
        out
    }

    /// 所有输入类型开关都关掉时必须回到**主 prism**。
    ///
    /// 回归：曾经用 `dict.prism_name()` 当"主 prism"，而它返回的是当前生效的
    /// prism —— 切到双拼后再关开关仍是双拼，`zhongguo` 会被按双拼键位解析。
    #[test]
    fn clearing_all_input_type_options_restores_primary_prism() {
        let Some(root) = prepare_multi_prism_dict("reset-to-primary") else {
            return;
        };
        let mut translator = open_multi_prism_translator(&root);
        let mut ctx = crate::engine::context::Context::new();

        assert!(translator.set_input_type("double"));
        assert_eq!(translator.dict.prism_name(), "demo.double");

        // 所有输入类型开关关闭 → sync_context 把 prism 复位到主 prism。
        ctx.set_option("double", false);
        translator.sync_context(&mut ctx);
        assert_eq!(translator.dict.prism_name(), "demo");

        let candidates = query_candidates(&mut translator, "zhongguo");
        assert!(
            candidates.iter().any(|c| c.text == "中国"),
            "回到主 prism 后 zhongguo 必须能出「中国」，实际 {:?}",
            candidates.iter().map(|c| c.text.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn learned_word_under_double_prism_is_visible_under_full_pinyin() {
        let Some(root) = prepare_multi_prism_dict("double-to-full") else {
            return;
        };
        let mut translator = open_multi_prism_translator(&root);

        assert!(translator.set_input_type("double"));
        let candidates = query_candidates(&mut translator, "vsgo");
        let learned_index = candidates
            .iter()
            .position(|c| c.text == "中国")
            .expect("双拼应出「中国」");
        let other_index = candidates
            .iter()
            .position(|c| c.text == "重过")
            .expect("双拼应出「重过」");
        assert!(
            learned_index > other_index,
            "学习前系统层应把高权重的「重过」排在「中国」前"
        );
        assert_eq!(
            candidates[learned_index]
                .entry
                .as_ref()
                .map(|e| e.commit_count),
            Some(0),
            "学习前候选来自系统表，commits 应为 0"
        );
        let entry = candidates[learned_index]
            .entry
            .clone()
            .expect("候选应带词条");
        assert!(
            translator.memorize(&[entry], &CommitHistory::new()),
            "双拼下学词应真实写入 userdb"
        );

        assert!(translator.set_input_type("full"));
        let candidates = query_candidates(&mut translator, "zhongguo");
        let learned = candidates
            .iter()
            .position(|c| c.text == "中国")
            .expect("切全拼后应可见");
        let other = candidates
            .iter()
            .position(|c| c.text == "重过")
            .expect("「重过」应可见");
        assert!(
            learned < other,
            "双拼学过的词切全拼后应排在同码其他候选之前"
        );
        assert!(
            candidates[learned]
                .entry
                .as_ref()
                .is_some_and(|e| e.commit_count >= 1),
            "排前的原因应是用户词层（commits >= 1）"
        );

        drop(translator);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn learned_word_under_full_pinyin_is_visible_under_double_prism() {
        let Some(root) = prepare_multi_prism_dict("full-to-double") else {
            return;
        };
        let mut translator = open_multi_prism_translator(&root);

        assert!(translator.set_input_type("full"));
        let candidates = query_candidates(&mut translator, "zhongguo");
        let learned_index = candidates
            .iter()
            .position(|c| c.text == "中国")
            .expect("全拼应出「中国」");
        let other_index = candidates
            .iter()
            .position(|c| c.text == "重过")
            .expect("全拼应出「重过」");
        assert!(
            learned_index > other_index,
            "学习前系统层应把高权重的「重过」排在「中国」前"
        );
        let entry = candidates[learned_index]
            .entry
            .clone()
            .expect("候选应带词条");
        assert!(
            translator.memorize(&[entry], &CommitHistory::new()),
            "全拼下学词应真实写入 userdb"
        );

        assert!(translator.set_input_type("double"));
        let candidates = query_candidates(&mut translator, "vsgo");
        let learned = candidates
            .iter()
            .position(|c| c.text == "中国")
            .expect("切双拼后应可见");
        let other = candidates
            .iter()
            .position(|c| c.text == "重过")
            .expect("「重过」应可见");
        assert!(
            learned < other,
            "全拼学过的词切双拼后应排在同码其他候选之前"
        );
        assert!(
            candidates[learned]
                .entry
                .as_ref()
                .is_some_and(|e| e.commit_count >= 1),
            "排前的原因应是用户词层（commits >= 1）"
        );

        drop(translator);
        std::fs::remove_dir_all(&root).ok();
    }

    // ------------------------------------------------------------------
    // aux 每键开销（默认不跑；`WEASEL_AUX_PERF=1` 才执行）
    // ------------------------------------------------------------------

    /// 「开 aux / 关 aux」的每键开销，口径固定，接线前后各跑一次做对比。
    ///
    /// * 词库：真实 `luna_pinyin.table.bin`（13 MB）；
    /// * aux 字典：从真实词库的单字现场编（676 个两字母码桶，一码多字）；
    /// * 用户词典：隔离临时目录里的真实 LevelDB，预置若干已学词；
    /// * 一个「键」= 一次 `query`（真实输入法每敲一键查一次），输入取常用词的
    ///   每个前缀，并额外取「前缀 + 单个辅助码字母」的形态；
    /// * 每轮采样 3 遍，报 p50 / p90 / p99 / 均值 / 标准差，以及每键均摊。
    ///
    /// 跑法（release + 单独 target 目录）：
    /// `cargo test --release --lib aux_per_key_overhead -- --nocapture`
    #[test]
    fn aux_per_key_overhead() {
        if std::env::var("WEASEL_AUX_PERF").ok().as_deref() != Some("1") {
            return;
        }
        use crate::gear::dict::auxiliary_db::{AuxiliaryCharacter, AuxiliaryCode, AuxiliaryDb};
        use crate::gear::dict::level_db::LevelDb;
        use std::collections::{BTreeMap, BTreeSet};
        use std::sync::Arc;
        use std::time::Instant;

        let Some(dict) = real_dict() else {
            return;
        };
        let root = std::env::temp_dir().join(format!("librime-aux-perf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建临时目录");

        // ---- 1) 从真实词库抽单字，编 aux 字典 -------------------------
        let mut characters: BTreeSet<char> = BTreeSet::new();
        for table in dict.tables() {
            table.walk_entries(|_code, entry| {
                let text = table.entry_text(entry);
                let mut chars = text.chars();
                if let (Some(character), None) = (chars.next(), chars.next()) {
                    characters.insert(character);
                }
            });
            if characters.len() >= 8000 {
                break;
            }
        }
        let letters: Vec<char> = ('a'..='z').collect();
        let mut by_character: BTreeMap<String, Vec<AuxiliaryCode>> = BTreeMap::new();
        let mut by_code: BTreeMap<String, Vec<AuxiliaryCharacter>> = BTreeMap::new();
        for (index, character) in characters.iter().take(8000).enumerate() {
            let code = format!("{}{}", letters[index % 26], letters[(index / 26) % 26]);
            by_character
                .entry(character.to_string())
                .or_default()
                .push(AuxiliaryCode {
                    code: code.clone(),
                    spelling_type: crate::algo::SpellingType::Normal,
                    credibility: 0.0,
                });
            by_code
                .entry(code)
                .or_default()
                .push(AuxiliaryCharacter {
                    text: character.to_string(),
                    spelling_type: crate::algo::SpellingType::Normal,
                    credibility: 0.0,
                });
        }
        let aux_db = AuxiliaryDb::compile(root.join("perf.aux.bin"), &by_character, &by_code, 1)
            .expect("compile aux db");

        // ---- 2) 隔离的 LevelDB 用户词 --------------------------------
        let db = LevelDb::shared_user_db(&root.join("user"), "luna_pinyin");
        let mut user_dict = UserDictionary::new("luna_pinyin", db);
        assert!(user_dict.load(), "userdb 应能打开");
        let learned: [(&str, &[&str]); 8] = [
            ("中国", &["zhong", "guo"]),
            ("我们", &["wo", "men"]),
            ("北京", &["bei", "jing"]),
            ("世界", &["shi", "jie"]),
            ("学习", &["xue", "xi"]),
            ("输入法", &["shu", "ru", "fa"]),
            ("朋友", &["peng", "you"]),
            ("工作", &["gong", "zuo"]),
        ];
        for (text, spellings) in learned {
            let code: Option<Vec<_>> = spellings.iter().map(|s| syllable_id(&dict, s)).collect();
            if let Some(code) = code {
                let entry = DictEntry {
                    text: text.to_string(),
                    code: Code::from_slice(&code),
                    weight: 1.0,
                    ..Default::default()
                };
                user_dict.update_entry(&dict, &entry, 1);
            }
        }

        let mut translator = ScriptTranslator::new(dict)
            .with_user_dict(user_dict)
            .with_auxiliary("", None, Some(Arc::new(aux_db)));
        translator.options.tags = vec!["abc".to_string()];

        // ---- 3) 输入序列：每个前缀都是一次按键查询 --------------------
        let words = [
            "zhongguo", "women", "beijing", "shijie", "xuexi", "shurufa", "pengyou",
            "shijian", "gongzuo", "shenghuo", "diannao", "shouji", "yinyue", "dianshi",
            "haizi", "laoshi", "xuesheng", "mingzi", "wenti", "jintian",
        ];
        let mut keys: Vec<String> = Vec::new();
        for word in words {
            for end in 1..=word.len() {
                keys.push(word[..end].to_string());
                keys.push(format!("{}b", &word[..end]));
            }
        }

        // 负载折算：固定算术负载跑 7 次，最快一次≈空载，中位数/最快 = 折算系数。
        // 有别的构建/测试并行时墙钟会被整体拉长，绝对值必须先除这个系数再看。
        fn calibrate() -> (f64, f64) {
            let mut best = f64::MAX;
            let mut samples = Vec::with_capacity(7);
            for _ in 0..7 {
                let started = Instant::now();
                let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
                for _ in 0..20_000_000u64 {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    std::hint::black_box(state);
                }
                let elapsed = started.elapsed().as_nanos() as f64 / 1000.0;
                best = best.min(elapsed);
                samples.push(elapsed);
            }
            samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let median = samples[samples.len() / 2];
            (median, median / best)
        }

        let history = CommitHistory::new();
        let mut measure = |translator: &mut ScriptTranslator, aux: bool, samples: &mut Vec<u128>| {
            translator.auxiliary_mode = aux;
            for key in &keys {
                let mut segment = Segment::new(0, key.len());
                segment.tags = vec!["abc".to_string()];
                let started = Instant::now();
                if let Some(mut translation) = translator.query(key, &mut segment, &history) {
                    let mut shown = 0;
                    while shown < 10 && translation.peek().is_some() {
                        translation.next();
                        shown += 1;
                    }
                }
                samples.push(started.elapsed().as_nanos());
            }
        };

        // 热身：让 aux 音节表 / 用户词缓存进入稳态。
        for aux in [false, true] {
            translator.auxiliary_mode = aux;
            for key in &keys {
                let mut segment = Segment::new(0, key.len());
                segment.tags = vec!["abc".to_string()];
                let _ = translator.query(key, &mut segment, &history);
            }
        }
        let (_, factor_before) = calibrate();
        let mut off: Vec<u128> = Vec::with_capacity(keys.len() * 9);
        let mut on: Vec<u128> = Vec::with_capacity(keys.len() * 9);
        let mut paired_deltas: Vec<f64> = Vec::with_capacity(9);
        // 交替采样，抵消机器负载漂移（off/on 共享同一个时间窗）。
        // 每轮记一次配对差（同一时间窗内），最后看配对差的中位数，
        // 比拿两次长时间跑的均值直接相减稳得多。
        for _ in 0..9 {
            let mut round_off: Vec<u128> = Vec::with_capacity(keys.len());
            let mut round_on: Vec<u128> = Vec::with_capacity(keys.len());
            measure(&mut translator, false, &mut round_off);
            measure(&mut translator, true, &mut round_on);
            // 用每轮的中位数做配对（spikes 会把均值拉飞，中位数才代表"典型每键"）。
            let median = |values: &mut Vec<u128>| {
                values.sort_unstable();
                values[values.len() / 2] as f64 / 1000.0
            };
            paired_deltas.push(median(&mut round_on) - median(&mut round_off));
            off.extend_from_slice(&round_off);
            on.extend_from_slice(&round_on);
        }
        let (_, factor_after) = calibrate();
        let factor = (factor_before + factor_after) / 2.0;

        fn stats(samples: &mut [u128]) -> (f64, f64, f64, f64, f64) {
            samples.sort_unstable();
            let n = samples.len();
            let pick = |q: f64| {
                let index = ((n - 1) as f64 * q).round() as usize;
                samples[index] as f64 / 1000.0
            };
            let mean = samples.iter().map(|v| *v as f64).sum::<f64>() / n as f64 / 1000.0;
            let variance = samples
                .iter()
                .map(|v| {
                    let delta = *v as f64 / 1000.0 - mean;
                    delta * delta
                })
                .sum::<f64>()
                / n as f64;
            (pick(0.5), pick(0.9), pick(0.99), mean, variance.sqrt())
        }

        println!(
            "[aux-perf] aux chars={} codes={} keys={} load-factor={:.2}",
            characters.len().min(8000),
            by_code.len(),
            keys.len(),
            factor
        );
        let mut off = off;
        let mut on = on;
        let (off_p50, off_p90, off_p99, off_mean, off_sd) = stats(&mut off);
        let (on_p50, on_p90, on_p99, on_mean, on_sd) = stats(&mut on);
        let per_key = |samples: &[u128]| {
            samples.iter().map(|v| *v as f64).sum::<f64>() / samples.len() as f64 / 1000.0
        };
        let normalized = |value: f64| value / factor;
        println!(
            "[aux-perf] off per-key {:.3} µs ({:.3} 折算) | p50 {:.3} p90 {:.3} p99 {:.3} mean {:.3} sd {:.3} (µs)",
            per_key(&off),
            normalized(per_key(&off)),
            off_p50,
            off_p90,
            off_p99,
            off_mean,
            off_sd
        );
        println!(
            "[aux-perf] on  per-key {:.3} µs ({:.3} 折算) | p50 {:.3} p90 {:.3} p99 {:.3} mean {:.3} sd {:.3} (µs)",
            per_key(&on),
            normalized(per_key(&on)),
            on_p50,
            on_p90,
            on_p99,
            on_mean,
            on_sd
        );
        paired_deltas.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let paired_median = paired_deltas[paired_deltas.len() / 2];
        println!(
            "[aux-perf] delta per-key {:+.3} µs ({:+.1}%) / 折算后 {:+.3} µs",
            per_key(&on) - per_key(&off),
            (per_key(&on) - per_key(&off)) / per_key(&off) * 100.0,
            normalized(per_key(&on) - per_key(&off))
        );
        println!(
            "[aux-perf] 配对差(9 轮) 中位 {:+.3} µs | 最小 {:+.3} | 最大 {:+.3} | 折算后中位 {:+.3} µs",
            paired_median,
            paired_deltas[0],
            paired_deltas[paired_deltas.len() - 1],
            paired_median / factor
        );

        drop(translator);
        std::fs::remove_dir_all(&root).ok();
    }

    /// 形码假整句守卫（用户 4 张截图，`ossqsrjsrsadimeenesz`）。
    /// 用探针实测的 `components` 逐字构造，确保判据对真实数据生效。
    #[test]
    fn sentence_has_raw_code_detects_shape_code_garbage() {
        // 探针实测：候选[0] `os昱日jsrsadime道止`，components =
        // [(2,"os"), (6,"昱日"), (8,"js"), (11,"rsa"), (15,"dime"), (20,"道止")]
        // os/js/rsa/dime 都是"原码原样回显"，必须判为垃圾整句。
        let garbage = Sentence {
            text: "os昱日jsrsadime道止".into(),
            end_pos: 20,
            weight: 0.0,
            segments: vec![
                (2, "os".into(), 0.0),
                (6, "昱日".into(), 0.0),
                (8, "js".into(), 0.0),
                (11, "rsa".into(), 0.0),
                (15, "dime".into(), 0.0),
                (20, "道止".into(), 0.0),
            ],
        };
        assert!(
            sentence_has_raw_code(&garbage, "ossqsrjsrsadimeenesz"),
            "形码原码回显段应被识别为假整句"
        );
    }

    /// 拼音整句（同为 grammar=None）绝不能被误伤 —— 每段都是汉字，永不等于 ASCII 拼音。
    #[test]
    fn sentence_has_raw_code_spares_pinyin_sentence() {
        // 探针实测 `nihaoshijie` → 候选[0] `你好时节`，components = [(2,"你"),(11,"好时节")]
        let pinyin = Sentence {
            text: "你好时节".into(),
            end_pos: 11,
            weight: 0.0,
            segments: vec![(2, "你".into(), 0.0), (11, "好时节".into(), 0.0)],
        };
        assert!(
            !sentence_has_raw_code(&pinyin, "nihaoshijie"),
            "拼音整句全是汉字，不应命中原码守卫"
        );
    }

    /// 边界：纯汉字整句 / 空输入 / 单段整句 / 多字节 char 边界安全。
    #[test]
    fn sentence_has_raw_code_edge_cases() {
        // 纯汉字整句（每段是词，不等于拼音/编码）
        let han = Sentence {
            text: "中华人民共和国".into(),
            end_pos: 6,
            weight: 0.0,
            segments: vec![(6, "中华人民共和国".into(), 0.0)],
        };
        assert!(!sentence_has_raw_code(&han, "zhonghuarenmingongheguo"));

        // 空 segments
        let empty = Sentence {
            text: String::new(),
            end_pos: 0,
            weight: 0.0,
            segments: vec![],
        };
        assert!(!sentence_has_raw_code(&empty, ""));

        // 单段：text 等于整段输入原码（如纯英文词条，poet 会跳过这种边，但守卫也应识别）
        let raw_only = Sentence {
            text: "hello".into(),
            end_pos: 5,
            weight: 0.0,
            segments: vec![(5, "hello".into(), 0.0)],
        };
        assert!(sentence_has_raw_code(&raw_only, "hello"));

        // 多字节输入：input.get(start..end) 在非法 char 边界返回 None，不应 panic。
        let multi = Sentence {
            text: "好".into(),
            end_pos: 3, // "你".len() = 3，合法边界
            weight: 0.0,
            segments: vec![(3, "好".into(), 0.0)],
        };
        assert!(!sentence_has_raw_code(&multi, "你好")); // "你好"[0..3]="你" ≠ "好"

        let bad_boundary = Sentence {
            text: "好".into(),
            end_pos: 1, // 落在 "你" 的多字节中间 → get 返回 None，跳过该段
            weight: 0.0,
            segments: vec![(1, "?".into(), 0.0)],
        };
        assert!(!sentence_has_raw_code(&bad_boundary, "你好")); // 不 panic
    }

}
