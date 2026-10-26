//! librime `src/rime/algo/calculus.h`(112 行) + `calculus.cc`(372 行) 的 Rust 移植。
//!
//! 拼写演算：把一条"规则文本"（`xlit/...`、`xform/...`、`derive/...` …）解析成一个
//! `Calculation`，再逐个作用到 `Spelling` 上。
//!
//! ## 正则子模块：`fancy-regex`
//!
//! C++ 用 `boost::regex`（`calculus.cc:7`、`encoder.cc:99`）做 `xform` / `erase` /
//! `exclude_patterns`。2026-10-24 先把 1,082 行手写子集引擎换成 `regex` crate；
//! 2026-10-25 再换成 `fancy-regex` —— 它是 `regex` 的超集，补上了真实方案在用的两样写法：
//!
//! * **模式内反向引用** `\1`：`xform/([aoe])\1(\w)/$1$2/`（`double_pinyin_ziranma` / `_flypy`）；
//! * **环视** `(?=` `(?!`：`recognizer/patterns/uppercase` 的 `^(?![`;])…`（16 份宇浩系方案）。
//!
//! 不含 fancy 语法的模式由 fancy-regex 内部**委托给 `regex` crate**，不会因为换库变慢。
//!
//! ## 与 boost 仍存的两处手工翻译（别删）
//!
//! * `\l` / `\u`（boost 的小写/大写字母类）→ `[a-z]` / `[A-Z]`：`translate_boost_classes`；
//! * 替换串 `$1z` → `${1}z`（否则 `z` 被当成组名的一部分）：`translate_boost_replacement`。
//!
//! ## 出错策略：宁可报错，也不静默算错
//!
//! * **编译期**：真·非法模式返回 [`RuleError`]（规则原文 + 字节位置 + 原因）。已经**没有**
//!   "子集不支持"这一档 —— fancy-regex 认得的写法一律放行。
//! * **运行期**：fancy-regex 可能因回溯超限返回 `RuntimeError`，本层一律**保守退化**：
//!   `is_full_match` 当"没匹配"、`replace_all` 返回**原文**（等价于这条规则被跳过），
//!   绝不让 panic 穿过输入法进程。
//!
//! ## C++ 侧的古怪点（照抄）
//!
//! 1. `Calculus::Parse` 用**第一个非小写字母的字符**当分隔符，不是固定的 `/`
//!    （`calculus.cc:33-38`）——所以 `"xlit abc XYZ"`（空格分隔）也能解析，测试里就这么用。
//! 2. `boost::split` 保留空 token，所以 `"reorder "` 得到 `["reorder", ""]`、
//!    `"xform/^/X/"` 得到 4 个参数（末尾空串）。`reorder ` 因此解析失败（order 为空）。
//! 3. `xlit` 的两串**长度必须相等**，否则解析失败（`calculus.cc:59-68` 那个
//!    `while ((cl=next),(cr=next), cl && cr)` 逗号表达式）。
//! 4. `derive/../..//correction|abbrev|fuzz` 的第 4 个参数是 tag，认不出时**退化成普通
//!    derive**（`calculus.cc:190`），不是报错。
//! 5. `reorder` 的 `last_key_index` **跨分隔符分组不重置**（`calculus.cc:287`、`303-306`），
//!    只有 `needs_reorder` 在新组开头被清零。
//!
//! ## 两套入口
//!
//! * [`Calculus::parse`] → `Option`，与 C++ 的 `return NULL` 逐位对齐（测试与对拍用）；
//! * [`Calculus::try_parse`] → `Result<_, RuleError>`，带诊断信息（CLI / 部署日志用）。
//!
//! 注意 C++ 的 `Transformation::Parse` 直接 `pattern_.assign(left)`，遇到 boost 不支持
//! 的模式会**抛 `regex_error` 且没人接**（`calculus.cc:113`）；这里退化成 `None` / `Err`，
//! 对拍时行为覆盖了"合法规则"这一侧，非法规则这一侧是**有意**比 C++ 温和。
#![allow(dead_code)]

use std::collections::HashMap;
use std::fmt;

use super::spelling::{Spelling, SpellingType};

/// `calculus.cc:14-16`：三条罚分（自然对数值）。
///
/// 数值**逐位照抄** C++（`-log(0.5)` / `-log(0.01)`）；不用 `f64::consts::LN_2` 改写，
/// 免得以后对拍时还要解释"位模式为什么一样"。
#[allow(clippy::approx_constant)]
pub const K_ABBREVIATION_PENALTY: f64 = -0.6931471805599453; // log(0.5)
#[allow(clippy::approx_constant)]
pub const K_FUZZY_SPELLING_PENALTY: f64 = -0.6931471805599453; // log(0.5)
pub const K_CORRECTION_PENALTY: f64 = -4.605170185988091; // log(0.01)

/// 规则编译失败：**带规则原文**，能指出错在模式还是替换串、以及字节位置。
///
/// 只由 [`Calculus::try_parse`] 产出；[`Calculus::parse`] 会把它压成 `None`
/// （C++ 的 `NULL` 语义）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleError {
    /// 规则原文（例如 `xform/(?!a)/b/`）。
    definition: String,
    /// 出错片段的原文（模式或替换串）；参数类错误为 `None`。
    fragment: Option<String>,
    /// 在 `fragment` 里的**字节**偏移（从 0 起）；参数类错误为 `None`。
    position: Option<usize>,
    /// 中文原因。
    reason: String,
}

impl RuleError {
    /// 参数/结构类错误（没有具体片段）。
    fn invalid(reason: impl Into<String>) -> Self {
        Self {
            definition: String::new(),
            fragment: None,
            position: None,
            reason: reason.into(),
        }
    }

    /// 正则编译错误 → 规则错误（位置是模式里的字节偏移）。
    fn from_regex(error: regex::RegexError) -> Self {
        Self {
            definition: String::new(),
            fragment: Some(error.pattern),
            position: error.position,
            reason: error.reason,
        }
    }
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.definition.is_empty() {
            f.write_str("规则")?;
        } else {
            write!(f, "规则 \"{}\"", self.definition)?;
        }
        match (&self.fragment, self.position) {
            (Some(fragment), Some(position)) => {
                write!(
                    f,
                    ": 片段 \"{fragment}\" 位置 {position} —— {}",
                    self.reason
                )
            }
            (Some(fragment), None) => write!(f, ": 片段 \"{fragment}\" —— {}", self.reason),
            _ => write!(f, ": {}", self.reason),
        }
    }
}

impl std::error::Error for RuleError {}

/// 一条拼写演算规则。对应 `calculus.h:18-26`。
pub trait Calculation: Send + Sync {
    /// 把规则作用到拼写上；返回"是否真的改了"。
    fn apply(&mut self, spelling: &mut Spelling) -> bool;
    /// 结果是否**可以新增**一个拼写（`derive` 系列 true，`erase` false）。
    fn addition(&self) -> bool {
        true
    }
    /// 原拼写是否**保留**（`erase` true —— 它删完还要保留"空"？见 `algebra.cc:111-117`；
    /// `derive` 系列 false，因为原串由 `addition` 分支负责）。
    fn deletion(&self) -> bool {
        true
    }
    /// 调试用名字。
    fn name(&self) -> &'static str;
}

/// 规则工厂：`(args) -> Calculation`。对应 `calculus.h:21` 的 `using Factory`。
///
/// 与 C++ 的差异：C++ 用返回 `NULL` 表示"这条规则不成立"；这里用 `Err(RuleError)`
/// 把"为什么不成立"（哪一段、第几个字节、什么语法）带出来，调用方要么当 `None`
/// （[`Calculus::parse`]），要么把诊断打出去（[`Calculus::try_parse`]）。
pub type Factory = fn(&[String]) -> Result<Box<dyn Calculation>, RuleError>;

/// `Calculus`：规则文本 → `Calculation`。`calculus.cc:18-46`。
pub struct Calculus {
    factories: HashMap<String, Factory>,
}

impl Default for Calculus {
    fn default() -> Self {
        Self::new()
    }
}

impl Calculus {
    /// `calculus.cc:18-26`：注册 7 个内置令牌。
    pub fn new() -> Self {
        let mut c = Self {
            factories: HashMap::new(),
        };
        c.register("xlit", parse_transliteration);
        c.register("xform", parse_transformation);
        c.register("erase", parse_erasion);
        c.register("derive", parse_derivation);
        c.register("fuzz", parse_fuzzing);
        c.register("abbrev", parse_abbreviation);
        c.register("reorder", parse_reorder);
        c
    }

    /// `calculus.cc:28-30`。
    fn register(&mut self, token: &str, factory: Factory) {
        self.factories.insert(token.to_string(), factory);
    }

    /// `calculus.cc:32-46`：解析一条规则文本。`None` 等价 C++ 返回 `NULL`。
    pub(super) fn parse(&self, definition: &str) -> Option<Box<dyn Calculation>> {
        self.try_parse(definition).ok()
    }

    /// 同 [`Calculus::parse`]，但失败时给出**可读的诊断**（规则原文 + 出错片段 + 位置）。
    ///
    /// 这是"不允许静默降级"的落点：不支持的语法不是返回 `None` 就算了，而是带着
    /// 证据返回；`Projection::from_rules` 会把这段诊断原样拼进错误信息。
    pub fn try_parse(&self, definition: &str) -> Result<Box<dyn Calculation>, RuleError> {
        // `calculus.cc:33`：`find_first_not_of("zyxwvutsrqponmlkjihgfedcba")`
        // —— 那个集合正好是 a..z，所以等价于"第一个非小写字母"的字节。
        let Some(sep) = definition.bytes().position(|b| !b.is_ascii_lowercase()) else {
            return Err(RuleError::invalid(
                "整条规则里没有分隔符（全是小写字母），不符合 `token/参数/参数/` 的形状",
            ));
        };
        // `sep` 必落在字符边界上：它之前的字节全是 ASCII 小写字母，
        // 所以 `definition[sep..]` 与 `.chars().next()` 都安全。
        let Some(sep_char) = definition[sep..].chars().next() else {
            return Err(RuleError::invalid("规则分隔符后没有内容"));
        };
        // `calculus.cc:37-38`：boost::split（保留空 token）
        let args: Vec<String> = definition.split(sep_char).map(|s| s.to_string()).collect();
        let Some(token) = args.first() else {
            return Err(RuleError::invalid("规则是空的"));
        };
        // `calculus.cc:41-43`
        let Some(factory) = self.factories.get(token.as_str()).copied() else {
            return Err(RuleError::invalid(format!(
                "未知规则令牌 `{token}`（内置：xlit/xform/erase/derive/fuzz/abbrev/reorder）"
            )));
        };
        factory(&args).map_err(|mut error| {
            error.definition = definition.to_string();
            error
        })
    }
}

/// 编译一条模式，失败时把 [`regex::RegexError`] 转成 [`RuleError`]。
fn compile_pattern(pattern: &str) -> Result<regex::Regex, RuleError> {
    regex::Regex::new(pattern).map_err(RuleError::from_regex)
}

/// 检查替换串：本子集只支持 `$1`、`${1}`、`$&`、`\x` 转义。
///
/// `${...}` 里必须是数字 —— 其余写法（`${name}`、`${1` 未闭合）boost 会当格式错误，
/// 这里同样明确报错，不当普通字符吐出去。
fn check_replacement(replacement: &str) -> Result<(), RuleError> {
    let bytes = replacement.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            // `\` 转义下一个字符（末尾单个 `\` 原样输出，与 boost 的宽松处理一致）
            b'\\' => i += 2,
            b'$' if bytes.get(i + 1) == Some(&b'{') => {
                let mut j = i + 2;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j == i + 2 || bytes.get(j) != Some(&b'}') {
                    return Err(RuleError {
                        definition: String::new(),
                        fragment: Some(replacement.to_string()),
                        position: Some(i),
                        reason: "替换串里的 `${...}` 只支持 `${数字}`".to_string(),
                    });
                }
                i = j + 1;
            }
            _ => i += 1,
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// xlit/zyx/abc/  —— 逐字符替换（`calculus.cc:50-107`）
// ---------------------------------------------------------------------------

/// `calculus.h:51-60`。注意 C++ 存 `map<uint32_t,uint32_t>`（码点），这里用 `char`。
pub struct Transliteration {
    char_map: HashMap<char, char>,
}

fn parse_transliteration(args: &[String]) -> Result<Box<dyn Calculation>, RuleError> {
    // `calculus.cc:51-52`
    if args.len() < 3 {
        return Err(RuleError::invalid(format!(
            "`xlit` 需要 2 个参数（左串/右串），只给了 {}",
            args.len().saturating_sub(1)
        )));
    }
    let left: Vec<char> = args[1].chars().collect();
    let right: Vec<char> = args[2].chars().collect();
    // `calculus.cc:59-67`：两串必须**同时**走完
    if left.len() != right.len() {
        return Err(RuleError::invalid(format!(
            "`xlit` 两侧长度必须相等（左 {} 个字符 / 右 {} 个字符）",
            left.len(),
            right.len()
        )));
    }
    let char_map: HashMap<char, char> = left.into_iter().zip(right).collect();
    Ok(Box::new(Transliteration { char_map }))
}

impl Calculation for Transliteration {
    fn name(&self) -> &'static str {
        "xlit"
    }

    /// `calculus.cc:71-107`：逐字符查表；一个都没命中就返回 false。
    fn apply(&mut self, spelling: &mut Spelling) -> bool {
        if spelling.str.is_empty() {
            return false;
        }
        let mut modified = false;
        let mut result = String::with_capacity(spelling.str.len());
        for (index, c) in spelling.str.char_indices() {
            match self.char_map.get(&c) {
                Some(&replacement) => {
                    if !modified {
                        // 直到第一次命中才把前缀拷进 result（C++ 也是这么省的）
                        modified = true;
                        result.push_str(&spelling.str[..index]);
                    }
                    result.push(replacement);
                }
                None => {
                    if modified {
                        result.push(c);
                    }
                }
            }
        }
        if modified {
            spelling.str = result;
        }
        modified
    }
}

// ---------------------------------------------------------------------------
// xform/x/y/  —— 正则替换（`calculus.cc:111-132`）
// ---------------------------------------------------------------------------

/// `calculus.h:63-71`
pub struct Transformation {
    pattern: regex::Regex,
    replacement: String,
}

fn parse_transformation(args: &[String]) -> Result<Box<dyn Calculation>, RuleError> {
    // `calculus.cc:112-117`
    if args.len() < 3 || args[1].is_empty() {
        return Err(RuleError::invalid(
            "`xform` 需要 2 个参数（模式/替换串），且模式不能为空",
        ));
    }
    let pattern = compile_pattern(&args[1])?;
    check_replacement(&args[2])?;
    Ok(Box::new(Transformation {
        pattern,
        replacement: args[2].clone(),
    }))
}

impl Calculation for Transformation {
    fn name(&self) -> &'static str {
        "xform"
    }

    /// `calculus.cc:124-132`：`boost::regex_replace` —— **替换所有**匹配。
    fn apply(&mut self, spelling: &mut Spelling) -> bool {
        if spelling.str.is_empty() {
            return false;
        }
        let result = self.pattern.replace_all(&spelling.str, &self.replacement);
        if result == spelling.str {
            return false;
        }
        spelling.str = result;
        true
    }
}

// ---------------------------------------------------------------------------
// erase/x/  —— 整串匹配就清空（`calculus.cc:136-154`）
// ---------------------------------------------------------------------------

/// `calculus.h:74-82`
pub struct Erasion {
    pattern: regex::Regex,
}

fn parse_erasion(args: &[String]) -> Result<Box<dyn Calculation>, RuleError> {
    // `calculus.cc:137-141`
    if args.len() < 2 || args[1].is_empty() {
        return Err(RuleError::invalid("`erase` 需要 1 个非空模式"));
    }
    Ok(Box::new(Erasion {
        pattern: compile_pattern(&args[1])?,
    }))
}

impl Calculation for Erasion {
    fn name(&self) -> &'static str {
        "erase"
    }

    /// `calculus.cc:80-82`：`addition()` 恒 false —— 删除规则不产生新拼写。
    fn addition(&self) -> bool {
        false
    }

    /// `calculus.cc:147-154`：用 `regex_match`（**整串**匹配），命中就把串清空。
    fn apply(&mut self, spelling: &mut Spelling) -> bool {
        if spelling.str.is_empty() {
            return false;
        }
        if !self.pattern.is_full_match(&spelling.str) {
            return false;
        }
        spelling.str.clear();
        true
    }
}

// ---------------------------------------------------------------------------
// derive/x/X/  —— 派生（`calculus.cc:158-197`）
// ---------------------------------------------------------------------------

/// `calculus.h:85-90`：派生规则**保留**原拼写（`deletion()` = false）。
pub struct Derivation {
    pattern: regex::Regex,
    replacement: String,
}

fn make_derivation_like(
    args: &[String],
    ctor: fn(regex::Regex, String) -> Box<dyn Calculation>,
) -> Result<Box<dyn Calculation>, RuleError> {
    if args.len() < 3 || args[1].is_empty() {
        return Err(RuleError::invalid(
            "派生类规则需要 2 个参数（模式/替换串），且模式不能为空",
        ));
    }
    let pattern = compile_pattern(&args[1])?;
    check_replacement(&args[2])?;
    Ok(ctor(pattern, args[2].clone()))
}

fn parse_derivation(args: &[String]) -> Result<Box<dyn Calculation>, RuleError> {
    // `calculus.cc:158-166`
    if args.len() < 3 || args[1].is_empty() {
        return Err(RuleError::invalid(
            "`derive` 需要 2 个参数（模式/替换串），且模式不能为空",
        ));
    }
    // `calculus.cc:167-191`：第 4 个参数是 tag
    if args.len() > 3 {
        match args[3].as_str() {
            "correction" => {
                return make_derivation_like(args, |pattern, replacement| {
                    Box::new(Correction {
                        inner: Derivation {
                            pattern,
                            replacement,
                        },
                    })
                });
            }
            "abbrev" => {
                return make_derivation_like(args, |pattern, replacement| {
                    Box::new(Abbreviation {
                        inner: Derivation {
                            pattern,
                            replacement,
                        },
                    })
                });
            }
            "fuzz" => {
                return make_derivation_like(args, |pattern, replacement| {
                    Box::new(Fuzzing {
                        inner: Derivation {
                            pattern,
                            replacement,
                        },
                    })
                });
            }
            // `calculus.cc:190`：tag 無法識別, 作爲普通 derive 處理
            _ => {}
        }
    }
    let pattern = compile_pattern(&args[1])?;
    check_replacement(&args[2])?;
    Ok(Box::new(Derivation {
        pattern,
        replacement: args[2].clone(),
    }))
}

impl Derivation {
    fn transform(&mut self, spelling: &mut Spelling) -> bool {
        if spelling.str.is_empty() {
            return false;
        }
        let result = self.pattern.replace_all(&spelling.str, &self.replacement);
        if result == spelling.str {
            return false;
        }
        spelling.str = result;
        true
    }
}

impl Calculation for Derivation {
    fn name(&self) -> &'static str {
        "derive"
    }

    fn apply(&mut self, spelling: &mut Spelling) -> bool {
        self.transform(spelling)
    }

    /// `calculus.h:88-89`：`deletion()` 恒 false。
    fn deletion(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// fuzz/zyx/zx/  —— 模糊音（`calculus.cc:201-221`）
// ---------------------------------------------------------------------------

/// `calculus.h:93-97`
pub struct Fuzzing {
    inner: Derivation,
}

fn parse_fuzzing(args: &[String]) -> Result<Box<dyn Calculation>, RuleError> {
    // `calculus.cc:201-207`
    if args.len() < 3 || args[1].is_empty() {
        return Err(RuleError::invalid(
            "`fuzz` 需要 2 个参数（模式/替换串），且模式不能为空",
        ));
    }
    let pattern = compile_pattern(&args[1])?;
    check_replacement(&args[2])?;
    Ok(Box::new(Fuzzing {
        inner: Derivation {
            pattern,
            replacement: args[2].clone(),
        },
    }))
}

impl Calculation for Fuzzing {
    fn name(&self) -> &'static str {
        "fuzz"
    }

    /// `calculus.cc:214-221`
    fn apply(&mut self, spelling: &mut Spelling) -> bool {
        let result = self.inner.transform(spelling);
        if result {
            spelling.properties.r#type = SpellingType::Fuzzy;
            spelling.properties.credibility += K_FUZZY_SPELLING_PENALTY;
        }
        result
    }

    fn deletion(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// abbrev/zyx/z/  —— 简拼（`calculus.cc:225-245`）
// ---------------------------------------------------------------------------

/// `calculus.h:100-104`
pub struct Abbreviation {
    inner: Derivation,
}

fn parse_abbreviation(args: &[String]) -> Result<Box<dyn Calculation>, RuleError> {
    // `calculus.cc:225-231`
    if args.len() < 3 || args[1].is_empty() {
        return Err(RuleError::invalid(
            "`abbrev` 需要 2 个参数（模式/替换串），且模式不能为空",
        ));
    }
    let pattern = compile_pattern(&args[1])?;
    check_replacement(&args[2])?;
    Ok(Box::new(Abbreviation {
        inner: Derivation {
            pattern,
            replacement: args[2].clone(),
        },
    }))
}

impl Calculation for Abbreviation {
    fn name(&self) -> &'static str {
        "abbrev"
    }

    /// `calculus.cc:238-245`
    fn apply(&mut self, spelling: &mut Spelling) -> bool {
        let result = self.inner.transform(spelling);
        if result {
            spelling.properties.r#type = SpellingType::Abbreviation;
            spelling.properties.credibility += K_ABBREVIATION_PENALTY;
        }
        result
    }

    fn deletion(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// derive/x/X/correction  —— 纠错（`calculus.cc:249-256`）
// ---------------------------------------------------------------------------

/// `calculus.h:107-111`：没有独立的 `Parse`，只能由 `derive/../../correction` 造出来。
pub struct Correction {
    inner: Derivation,
}

impl Calculation for Correction {
    fn name(&self) -> &'static str {
        "correction"
    }

    /// `calculus.cc:249-256`
    fn apply(&mut self, spelling: &mut Spelling) -> bool {
        let result = self.inner.transform(spelling);
        if result {
            spelling.properties.is_correction = true;
            spelling.properties.credibility += K_CORRECTION_PENALTY;
        }
        result
    }

    fn deletion(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// reorder/zyx/dedup  —— 并击键序重排（`calculus.cc:260-370`）
// ---------------------------------------------------------------------------

/// `calculus.h:114-122`
pub struct Reorder {
    order: Vec<char>,
    dedup: bool,
}

fn parse_reorder(args: &[String]) -> Result<Box<dyn Calculation>, RuleError> {
    // `calculus.cc:260-272`
    if args.len() < 2 || args[1].is_empty() {
        return Err(RuleError::invalid("`reorder` 需要 1 个非空的键序串"));
    }
    let dedup = args.len() > 2 && args[2] == "dedup";
    Ok(Box::new(Reorder {
        order: args[1].chars().collect(),
        dedup,
    }))
}

impl Calculation for Reorder {
    fn name(&self) -> &'static str {
        "reorder"
    }

    /// `calculus.cc:274-370`：把"键序组"内的字符按 `order` 的顺序重排，
    /// 非键序字符当分隔符原样保留，并且**只有真的乱序/重复才算修改**。
    fn apply(&mut self, spelling: &mut Spelling) -> bool {
        if spelling.str.is_empty() {
            return false;
        }
        let chars: Vec<char> = spelling.str.chars().collect();
        let mut result: Vec<char> = Vec::with_capacity(chars.len());
        let mut modified = false;

        // `calculus.cc:284-287`
        let mut key_count: Vec<usize> = vec![0; self.order.len()];
        let mut group_start: Option<usize> = None; // 字符下标（含）
        // ⚠️ 下面这两个**跨组不重置**（C++ 的 clear_group 只清 key_count / group_start，
        // 见 `calculus.cc:303-306`）。`last_key_index` 会带进下一组做比较。
        let mut last_key_index: usize = 0;
        let mut needs_reorder = false;

        // `settle_group`（`calculus.cc:308-326`）+ `flush_group`（`289-300`）
        // + `clear_group`（`303-306`）的合并实现。
        macro_rules! settle_group {
            ($group_end:expr) => {
                if let Some(start) = group_start {
                    if needs_reorder {
                        if !modified {
                            modified = true;
                            // 把组前的原样前缀先拷过来
                            result.extend_from_slice(&chars[..start]);
                        }
                        // flush_group：按 order 的顺序吐字符；dedup 时每种键只吐一次
                        for (k, &count) in key_count.iter().enumerate() {
                            if count == 0 {
                                continue;
                            }
                            let n = if self.dedup { 1 } else { count };
                            for _ in 0..n {
                                result.push(self.order[k]);
                            }
                        }
                    } else if modified {
                        // 组内没有乱序，原样拷贝该组
                        result.extend_from_slice(&chars[start..$group_end]);
                    }
                    // clear_group（`calculus.cc:303-306`）
                    key_count.fill(0);
                    group_start = None;
                }
            };
        }

        // `calculus.cc:328-360`：主循环
        let mut p = 0usize;
        while p < chars.len() {
            let here = p;
            let c = chars[p];
            p += 1;

            if let Some(key_index) = self.order.iter().position(|&k| k == c) {
                // 屬於待重排的鍵組（`calculus.cc:334-350`）
                if group_start.is_none() {
                    group_start = Some(here);
                    needs_reorder = false;
                } else if key_index < last_key_index {
                    // 鍵序不是單調遞增 → 需要重排
                    needs_reorder = true;
                }
                last_key_index = key_index;

                key_count[key_index] += 1;
                if self.dedup && key_count[key_index] > 1 {
                    needs_reorder = true;
                }
                continue;
            }

            // 遇到分隔字符：结算上一组（`calculus.cc:352-359`）
            settle_group!(here);
            // 追加当前分隔字符
            if modified {
                result.push(c);
            }
        }
        // `calculus.cc:362-363`：结算末尾的键组
        settle_group!(chars.len());
        // 末尾这次 clear_group 之后没人再读 group_start，显式消费掉它
        let _ = group_start;

        if modified {
            spelling.str = result.into_iter().collect();
        }
        modified
    }
}

/// 正则引擎 —— 薄包装 `fancy-regex`（2026-10-25；此前依次是手写子集引擎、`regex` crate）。
///
/// ## 为什么是 `fancy-regex`
///
/// `algebra` 的规则直接决定拼写，错了是**静默**改变候选，所以引擎语义必须与 librime 的
/// `boost::regex` 对齐。`regex` crate 保证线性时间但**不支持反向引用与环视**，而真实方案里
/// 两样都在用（见文件头注释）；`fancy-regex` 是它的超集，正好补上，普通模式仍走 `regex` 的
/// 高速路径。
///
/// ## 与 boost 的两处差异（有意保留的手工翻译）
///
/// * `\l` / `\u` → `[a-z]` / `[A-Z]`（boost 扩展类，fancy-regex 也不认）；
/// * 替换串 `$1z` → `${1}z`（`$` 后紧跟数字会吞掉后面的字母，boost 不会）。
///
/// ## API 与原实现一致
///
/// 调用方只用 `Regex::new` / `is_full_match` / `replace_all`，所以调用点一行未改。
pub mod regex {
    /// 编译失败的错误（保留原来的三个字段形状）。
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct RegexError {
        /// 出错的模式原文。
        pub pattern: String,
        /// 出错位置（字节偏移）；`regex` crate 不总是给，取不到就是 `None`。
        pub position: Option<usize>,
        /// 人话原因。
        pub reason: String,
    }

    impl std::fmt::Display for RegexError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self.position {
                Some(position) => write!(
                    formatter,
                    "invalid regex `{}` at byte {}: {}",
                    self.pattern, position, self.reason
                ),
                None => write!(formatter, "invalid regex `{}`: {}", self.pattern, self.reason),
            }
        }
    }

    impl std::error::Error for RegexError {}

    /// 编译好的正则（薄包装 `regex::Regex`）。
    #[derive(Debug, Clone)]
    pub struct Regex {
        inner: ::fancy_regex::Regex,
    }

    impl Regex {
        /// 编译一条模式。
        pub fn new(pattern: &str) -> Result<Self, RegexError> {
            let translated = translate_boost_classes(pattern);
            match ::fancy_regex::Regex::new(&translated) {
                Ok(inner) => Ok(Self { inner }),
                Err(error) => Err(RegexError {
                    pattern: pattern.to_string(),
                    position: fancy_error_position(&error),
                    reason: fancy_error_reason(&error),
                }),
            }
        }

        /// 整个串必须被匹配（原手写引擎的同名方法）。
        pub fn is_full_match(&self, text: &str) -> bool {
            match self.inner.find(text) {
                Ok(Some(found)) => found.start() == 0 && found.end() == text.len(),
                // 运行期错误（回溯超限等）：保守当"没匹配"，与 C++ 侧规则被跳过一致。
                Ok(None) | Err(_) => false,
            }
        }

        /// 替换**全部**匹配（对应 `boost::regex_replace` 的默认行为）。
        pub fn replace_all(&self, text: &str, replacement: &str) -> String {
            // boost 的 `$1z` 在 regex crate 里会被读成"组名 1z" ⇒ 必须先翻成 `${1}z`。
            let translated = translate_boost_replacement(replacement);
            // fancy-regex 的 `replace_all` 在运行期错误上会 panic；用 `try_replacen` 退成原文。
            self.inner
                .try_replacen(text, 0, &translated)
                .map(|result| result.into_owned())
                .unwrap_or_else(|_| text.to_string())
        }

        /// 底层引用（新代码需要 `captures`/`find_iter` 时用）。
        pub fn raw(&self) -> &::fancy_regex::Regex {
            &self.inner
        }
    }

    /// `boost::regex` 的两个字符类在 `regex` crate 里没有，做一次等价替换。
    /// 替换串翻译：boost 风格 → `regex` crate 风格。
    ///
    /// 规则：`$N`（数字）→ `${N}`；`$&` → `${0}`；已经写成 `${N}` 的原样保留；
    /// 其它 `$` 原样（Rime 的替换串里没有 `$` 当字面量的用法）。
    fn translate_boost_replacement(replacement: &str) -> String {
        if !replacement.contains('$') {
            return replacement.to_string();
        }
        let chars: Vec<char> = replacement.chars().collect();
        let mut out = String::with_capacity(replacement.len() + 8);
        let mut index = 0;
        while index < chars.len() {
            let ch = chars[index];
            if ch != '$' || index + 1 >= chars.len() {
                out.push(ch);
                index += 1;
                continue;
            }
            let next = chars[index + 1];
            if next == '&' {
                out.push_str("${0}");
                index += 2;
            } else if next == '{' {
                out.push('$'); // 已经是 ${N} 形式，原样
                index += 1;
            } else if next.is_ascii_digit() {
                let mut digits = String::new();
                let mut cursor = index + 1;
                while cursor < chars.len() && chars[cursor].is_ascii_digit() {
                    digits.push(chars[cursor]);
                    cursor += 1;
                }
                out.push_str("${");
                out.push_str(&digits);
                out.push('}');
                index = cursor;
            } else {
                out.push('$');
                index += 1;
            }
        }
        out
    }

    /// 从 fancy-regex 的错误里取字节位置（解析错误带位置，其余没有）。
    fn fancy_error_position(error: &::fancy_regex::Error) -> Option<usize> {
        match error {
            ::fancy_regex::Error::ParseError(position, _) => Some(*position),
            _ => None,
        }
    }

    /// fancy-regex 错误的可读原因（解析 / 编译 / 运行期分开说）。
    fn fancy_error_reason(error: &::fancy_regex::Error) -> String {
        match error {
            ::fancy_regex::Error::ParseError(_, parse) => format!("正则语法错误：{parse}"),
            ::fancy_regex::Error::CompileError(compile) => format!("正则编译失败：{compile}"),
            ::fancy_regex::Error::RuntimeError(runtime) => format!("正则运行期错误：{runtime}"),
            _ => format!("正则错误：{error}"),
        }
    }

    fn translate_boost_classes(pattern: &str) -> String {
        if !pattern.contains(r"\l") && !pattern.contains(r"\u") {
            return pattern.to_string();
        }
        let mut out = String::with_capacity(pattern.len() + 8);
        let mut chars = pattern.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                out.push(ch);
                continue;
            }
            match chars.peek() {
                Some('l') => {
                    chars.next();
                    out.push_str("[a-z]");
                }
                Some('u') => {
                    chars.next();
                    out.push_str("[A-Z]");
                }
                Some(&next) => {
                    out.push('\\');
                    out.push(next);
                    chars.next();
                }
                None => out.push('\\'),
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn full_match_is_anchored_to_whole_string() {
            let re = Regex::new("ab").expect("compile");
            assert!(re.is_full_match("ab"));
            assert!(!re.is_full_match("xab"), "前缀不该算整串匹配");
            assert!(!re.is_full_match("abx"), "后缀不该算整串匹配");
        }

        #[test]
        fn replace_all_replaces_every_occurrence() {
            let re = Regex::new("a").expect("compile");
            assert_eq!(re.replace_all("banana", "o"), "bonono");
        }

        #[test]
        fn boost_character_classes_are_translated() {
            let re = Regex::new(r"^\l\u$").expect("compile");
            assert!(re.is_full_match("aZ"), r"\l\u 应等于 [a-z][A-Z]");
            assert!(!re.is_full_match("Za"));
        }

        #[test]
        fn invalid_pattern_reports_the_original_text() {
            let error = Regex::new("(unclosed").expect_err("必须报错");
            assert_eq!(error.pattern, "(unclosed");
            assert!(!error.reason.is_empty());
        }

        #[test]
        fn lookahead_is_supported_now() {
            // 真实方案 `recognizer/patterns/uppercase` 的写法：首字符不能是 ` 或 ;。
            let re = Regex::new(r"^(?![`;]).*[A-Z][-_+.'0-9A-Za-z]*$").expect("环视必须能编译");
            assert!(re.is_full_match("ABC"));
            assert!(!re.is_full_match("`ABC"), "反引号开头必须被环视挡住");
            assert!(!re.is_full_match(";ABC"));
        }

        #[test]
        fn backreference_is_supported_now() {
            // 真实规则 `xform/([aoe])\1(\w)/$1$2/`（双拼自然码 / 小鹤在用）。
            let re = Regex::new(r"([aoe])\1(\w)").expect("反向引用必须能编译");
            assert_eq!(re.replace_all("aa1", "$1$2"), "a1");
            assert_eq!(re.replace_all("abc", "$1$2"), "abc", "不回引的串不该变");
        }

        #[test]
        fn lazy_quantifier_is_supported_now() {
            // 手写引擎不支持懒惰量词，`regex` crate 支持。
            let re = Regex::new("^a+?b$").expect("compile");
            assert!(re.is_full_match("aaab"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_one(formula: &str, spelling: &str) -> Option<(String, Spelling)> {
        let calc = Calculus::new();
        let mut c = calc.parse(formula)?;
        let mut s = Spelling::new(spelling);
        let modified = c.apply(&mut s);
        assert_eq!(modified, s.str != spelling, "Apply 返回值要和实际改动一致");
        Some((s.str.clone(), s))
    }

    // ---- 以下 8 条对应 librime `test/calculus_test.cc` 的断言 ----

    #[test]
    fn calculus_transliteration() {
        // calculus_test.cc:15-23
        let calc = Calculus::new();
        let mut c = calc
            .parse("xlit abcdefghijklmnopqrstuvwxyz ABCDEFGHIJKLMNOPQRSTUVWXYZ")
            .expect("xlit 应能解析（用空格当分隔符）");
        let mut s = Spelling::new("abracadabra");
        assert!(c.apply(&mut s));
        assert_eq!(s.str, "ABRACADABRA");
    }

    #[test]
    fn calculus_transformation() {
        // calculus_test.cc:25-35
        let calc = Calculus::new();
        let mut c = calc.parse("xform/^([zcs])h(.*)$/$1$2/").expect("xform");
        let mut s = Spelling::new("shang");
        assert!(c.apply(&mut s));
        assert_eq!(s.str, "sang");
        s.str = "bang".to_string();
        assert!(!c.apply(&mut s));
    }

    #[test]
    fn calculus_erasion() {
        // calculus_test.cc:37-49
        let calc = Calculus::new();
        let mut c = calc.parse("erase/^[czs]h[aoe]ng?$/").expect("erase");
        assert!(!c.addition());
        assert!(c.deletion());
        let mut s = Spelling::new("shang");
        assert!(c.apply(&mut s));
        assert_eq!(s.str, "");
        s.str = "bang".to_string();
        assert!(!c.apply(&mut s));
    }

    #[test]
    fn calculus_derivation() {
        // calculus_test.cc:51-63
        let calc = Calculus::new();
        let mut c = calc.parse("derive/^([zcs])h/$1/").expect("derive");
        assert!(c.addition());
        assert!(!c.deletion());
        let mut s = Spelling::new("shang");
        assert!(c.apply(&mut s));
        assert_eq!(s.str, "sang");
        s.str = "bang".to_string();
        assert!(!c.apply(&mut s));
    }

    #[test]
    fn calculus_abbreviation() {
        // calculus_test.cc:65-76
        let calc = Calculus::new();
        let mut c = calc.parse("abbrev/^([zcs]h).*$/$1/").expect("abbrev");
        assert!(c.addition());
        assert!(!c.deletion());
        let mut s = Spelling::new("shang");
        assert!(c.apply(&mut s));
        assert_eq!(s.str, "sh");
        assert_eq!(s.properties.r#type, SpellingType::Abbreviation);
        assert_eq!(s.properties.credibility, 0.5f64.ln());
    }

    // ---- Reorder：calculus_test.cc:78-218（宫保拼音并击键序） ----

    const COMBO_PINYIN_KEY_ORDER: &str = "SCZHLFGDBKTPIUÜANREO";

    fn reorder(formula: &str) -> Box<dyn Calculation> {
        Calculus::new()
            .parse(formula)
            .unwrap_or_else(|| panic!("{formula} 应能解析"))
    }

    #[test]
    fn reorder_basic_inversion() {
        // calculus_test.cc:79-99
        let mut r = reorder(&format!("reorder {COMBO_PINYIN_KEY_ORDER}"));
        let mut s = Spelling::new("FZ");
        assert!(r.apply(&mut s));
        assert_eq!(s.str, "ZF");

        let mut s = Spelling::new("FZURO");
        assert!(r.apply(&mut s));
        assert_eq!(s.str, "ZFURO");

        let mut s = Spelling::new("UZRF");
        assert!(r.apply(&mut s));
        assert_eq!(s.str, "ZFUR");
    }

    #[test]
    fn reorder_already_ordered() {
        // calculus_test.cc:102-116
        let mut r = reorder(&format!("reorder {COMBO_PINYIN_KEY_ORDER}"));
        let mut s = Spelling::new("ZFURO");
        assert!(!r.apply(&mut s));
        assert_eq!(s.str, "ZFURO");
        let mut s = Spelling::new("ZF");
        assert!(!r.apply(&mut s));
        assert_eq!(s.str, "ZF");
    }

    #[test]
    fn reorder_with_delimiter() {
        // calculus_test.cc:119-143
        let mut r = reorder(&format!("reorder {COMBO_PINYIN_KEY_ORDER}"));
        let mut s = Spelling::new("FZURO'UN");
        assert!(r.apply(&mut s));
        assert_eq!(s.str, "ZFURO'UN");

        let mut s = Spelling::new("FZ'NU");
        assert!(r.apply(&mut s));
        assert_eq!(s.str, "ZF'UN");

        let mut s = Spelling::new("'FZ");
        assert!(r.apply(&mut s));
        assert_eq!(s.str, "'ZF");

        let mut s = Spelling::new("FZ'");
        assert!(r.apply(&mut s));
        assert_eq!(s.str, "ZF'");
    }

    #[test]
    fn reorder_deduplication() {
        // calculus_test.cc:146-172
        let mut plain = reorder(&format!("reorder {COMBO_PINYIN_KEY_ORDER}"));
        let mut dedup = reorder(&format!("reorder {COMBO_PINYIN_KEY_ORDER} dedup"));

        let mut s = Spelling::new("ZZFFUU");
        assert!(!plain.apply(&mut s));
        assert_eq!(s.str, "ZZFFUU");

        let mut s = Spelling::new("ZZFFUU");
        assert!(dedup.apply(&mut s));
        assert_eq!(s.str, "ZFU");

        let mut s = Spelling::new("FFZZUU");
        assert!(dedup.apply(&mut s));
        assert_eq!(s.str, "ZFU");
    }

    #[test]
    fn reorder_non_order_characters() {
        // calculus_test.cc:175-190
        let mut r = reorder("reorder zyx");
        let mut s = Spelling::new("xzyaxzy");
        assert!(r.apply(&mut s));
        assert_eq!(s.str, "zyxazyx");

        let mut s = Spelling::new("xzyAxzyB");
        assert!(r.apply(&mut s));
        assert_eq!(s.str, "zyxAzyxB");
    }

    #[test]
    fn reorder_non_order_characters_with_dedup() {
        // calculus_test.cc:193-203
        let mut r = reorder("reorder zyx dedup");
        let mut s = Spelling::new("xzxyaxzy");
        assert!(r.apply(&mut s));
        assert_eq!(s.str, "zyxazyx");
    }

    #[test]
    fn reorder_parse_validation() {
        // calculus_test.cc:206-218
        let calc = Calculus::new();
        assert!(calc.parse("reorder").is_none());
        assert!(calc.parse("reorder ").is_none());
        assert!(calc.parse("reorder zyx").is_some());
        assert!(calc.parse("reorder zyx dedup").is_some());
    }

    // ---- 额外的语义钉子（C++ 里没有测试，但移植时必须确认） ----

    /// `try_parse` 的诊断：规则原文 + 出错片段 + 字节位置 + 具体语法名。
    ///
    /// 这是"不允许静默算错"的证据链：`parse` 只告诉你"不成立"，
    /// `try_parse` 告诉你"哪条规则、哪一段、第几个字节、为什么不支持"。
    fn err_of(calc: &Calculus, rule: &str) -> RuleError {
        match calc.try_parse(rule) {
            Ok(_) => panic!("{rule} 应当报错"),
            Err(error) => error,
        }
    }

    #[test]
    fn try_parse_diagnoses_invalid_patterns() {
        let calc = Calculus::new();
        // 2026-10-25：环视已由 fancy-regex 支持，反例换成**真正**的语法错误。
        assert!(
            calc.try_parse("xform/(?!a)/b/").is_ok(),
            "环视必须能编译（fancy-regex）"
        );
        let err = err_of(&calc, "xform/(unclosed/b/");
        assert_eq!(err.definition, "xform/(unclosed/b/", "要带规则原文");
        assert_eq!(err.fragment.as_deref(), Some("(unclosed"), "要指出错的模式");
        assert!(err.position.is_some(), "要指出模式里的字节位置：{err}");
        assert!(!err.reason.is_empty(), "{err}");
        // Display 里三样齐全
        let text = err.to_string();
        assert!(text.contains("xform/(unclosed/b/"), "{text}");
        assert!(text.contains("(unclosed"), "{text}");
        assert!(text.contains("位置 "), "{text}");

        // 参数类错误也有中文说明（不是干巴巴的 None）
        let err = err_of(&calc, "xlit/abc/ab/");
        assert!(err.reason.contains("长度必须相等"), "{}", err.reason);
        let err = err_of(&calc, "nosuch/x/y/");
        assert!(err.reason.contains("未知规则令牌"), "{}", err.reason);
        let err = err_of(&calc, "reorder ");
        assert!(err.reason.contains("reorder"), "{}", err.reason);

        // 替换串的 `${...}`：只认 `${数字}`
        let err = err_of(&calc, "xform/a/${name}/");
        assert!(err.reason.contains("${...}"), "{}", err.reason);
        assert_eq!(err.position, Some(0));
        assert_eq!(err.fragment.as_deref(), Some("${name}"));
        assert!(calc.try_parse("xform/a/${1}/").is_ok());
        assert!(calc.try_parse("xform/a/$1/").is_ok());
        assert!(calc.try_parse("xform/a/$&/").is_ok());

        // C++ 兼容入口：所有失败都还是 None（与 `return NULL` 对齐）
        assert!(calc.parse("xform/(?!a)/b/").is_some());
        assert!(calc.parse("nosuch/x/y/").is_none());
    }

    #[test]
    fn parse_requires_known_token_and_args() {
        let calc = Calculus::new();
        assert!(calc.parse("nosuch/x/y/").is_none());
        // ⚠️ `xform/x/` 只有 2 个参数才算失败；`xform/only-one-arg/` 的第三段是空串，
        // C++ 里 args.size() == 3 一样能过（`calculus.cc:112-113`）
        assert!(calc.parse("xform/only-one-arg/").is_some());
        assert!(calc.parse("xform/").is_none());
        assert!(calc.parse("xform//y/").is_none()); // 空 pattern
        assert!(calc.parse("erase/").is_none()); // 只有 token
        assert!(calc.parse("xlit/abc/ab/").is_none()); // 长度不等
        assert!(calc.parse("xform/(bad/y/").is_none()); // 正则语法错
        // tag 认不出来时退化成普通 derive，仍然能解析
        assert!(calc.parse("derive/^a$/b/nonsense").is_some());
        assert_eq!(
            calc.parse("derive/^a$/b/nonsense").unwrap().name(),
            "derive"
        );
        assert_eq!(calc.parse("derive/^a$/b/abbrev").unwrap().name(), "abbrev");
        assert_eq!(
            calc.parse("derive/^a$/b/correction").unwrap().name(),
            "correction"
        );
        assert_eq!(calc.parse("derive/^a$/b/fuzz").unwrap().name(), "fuzz");
    }

    #[test]
    fn correction_sets_flag_and_penalty() {
        let calc = Calculus::new();
        let mut c = calc.parse("derive/^([jqxy])u$/$1v/correction").unwrap();
        let mut s = Spelling::new("ju");
        assert!(c.apply(&mut s));
        assert_eq!(s.str, "jv");
        assert!(s.properties.is_correction);
        assert_eq!(s.properties.credibility, K_CORRECTION_PENALTY);
        assert!(!c.deletion());
    }

    #[test]
    fn fuzz_sets_type_and_penalty() {
        let calc = Calculus::new();
        let mut c = calc.parse("fuzz/^z$/zh/").unwrap();
        let mut s = Spelling::new("z");
        assert!(c.apply(&mut s));
        assert_eq!(s.str, "zh");
        assert_eq!(s.properties.r#type, SpellingType::Fuzzy);
        assert_eq!(s.properties.credibility, K_FUZZY_SPELLING_PENALTY);
        assert!(!c.deletion());
    }

    #[test]
    fn xform_replaces_all_occurrences() {
        let calc = Calculus::new();
        let mut c = calc.parse("xform/([nl])v/$1ü/").unwrap();
        let mut s = Spelling::new("nv");
        assert!(c.apply(&mut s));
        assert_eq!(s.str, "nü");
        // 多次出现的都要换（boost::regex_replace 默认替换全部）
        let mut c = calc.parse("xform/a/X/").unwrap();
        let mut s = Spelling::new("banana");
        assert!(c.apply(&mut s));
        assert_eq!(s.str, "bXnXnX");
    }

    #[test]
    fn yuling_derive_rule_on_aa() {
        // 真实规则：`%APPDATA%\Rime-Next\schema\yuling\yuling.schema.yaml:159-161`
        //   derive/^([a-y])([a-y])/$1z/    # 第二码用 z 通配（第二码限定为 a-y）
        let calc = Calculus::new();
        let mut c = calc.parse("derive/^([a-y])([a-y])/$1z/").unwrap();
        let mut s = Spelling::new("aa");
        assert!(c.apply(&mut s));
        assert_eq!(s.str, "az");
        assert!(!c.deletion());
        // z 开头的码不匹配（第二码限定 a-y）
        let mut s = Spelling::new("za");
        assert!(!c.apply(&mut s));
    }

    #[test]
    fn empty_spelling_is_never_modified() {
        let calc = Calculus::new();
        for formula in [
            "xlit/ab/cd/",
            "xform/a/b/",
            "erase/a/",
            "derive/a/b/",
            "fuzz/a/b/",
            "abbrev/a/b/",
            "reorder zyx",
        ] {
            let mut c = calc.parse(formula).unwrap_or_else(|| panic!("{formula}"));
            let mut s = Spelling::new("");
            assert!(!c.apply(&mut s), "{formula} 不该改动空串");
        }
    }
    #[test]
    fn backreference_xform_rule_now_takes_effect() {
        // 真实规则：`double_pinyin_ziranma` / `double_pinyin_flypy` 的 `speller/algebra`
        //   xform/([aoe])\1(\w)/$1$2/
        // 换 fancy-regex 之前这条**编译失败被静默跳过** ⇒ 拼写与 librime 不一致；
        // 现在必须真的生效：`aa1` → `a1`。
        let calc = Calculus::new();
        let mut c = calc
            .try_parse(r"xform/([aoe])\1(\w)/$1$2/")
            .expect("反向引用规则必须能解析");
        let mut s = Spelling::new("aa1");
        assert!(c.apply(&mut s), "这条规则应当改动 aa1");
        assert_eq!(s.str, "a1");

        // 不回引的串不该被改动
        let mut s = Spelling::new("abc");
        assert!(!c.apply(&mut s), "abc 不该被这条规则改动");
        assert_eq!(s.str, "abc");
    }

}
