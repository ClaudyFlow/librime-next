//! librime `src/rime/algo/encoder.h`(118 行) + `encoder.cc`(336 行) 的 Rust 移植。
//!
//! 造词编码器：把"词 → 每个字的编码 → 按公式取字符 → 拼成一个码"。
//!
//! * [`TableEncoder`]：按 `encoder/rules` 里的公式（`AaAzBaBz`）取码，带 `exclude_patterns`
//!   与 `tail_anchor`（`'` 之类的尾锚，`encoder.cc:30-44` 的注释就是样例配置）；
//! * [`ScriptEncoder`]：按音节切分做 DFS（最长词优先），码之间用空格连。
//!
//! ## 依赖：配置树而不是 `Config*`
//!
//! C++ 的 `LoadSettings(Config*)` 只读三处：`encoder/rules`、`encoder/exclude_patterns`、
//! `encoder/tail_anchor`（`encoder.cc:55-102`）。这里吃 `serde_json::Value`（与
//! `src/config.rs` 同一套树），并用 [`value_as_i32`] 兼容"标量一律存字符串"的模型
//! （`ConfigValue::GetInt` 就是解析字符串，见 `config/config_types.cc`）。
//!
//! ## 照抄的古怪点
//!
//! 1. `ParseFormula` 里字母的含义分大小写两段且**不连续**：
//!    `A`..`T` → 0..19，`U`..`Z` → **-6..-1**（`(*it >= 'U') ? (*it - 'Z' - 1) : (*it - 'A')`，
//!    `encoder.cc:120`）；小写 `a`..`t` → 0..19，`u`..`z` → **-6..-1**（`encoder.cc:126`）。
//! 2. 没写 `length_equal` 也没写 `length_in_range` 的规则 → `min = max = 0`，永远匹配不上
//!    （`encoder.cc:64`）。
//! 3. `max_phrase_length_` 只由**成功解析的**规则撑起来，没有规则就是 0 —— 于是
//!    `EncodePhrase` 对任何非空词都直接 false（`encoder.cc:239-240`）。
//! 4. `DfsEncode` 的 `limit`（默认 32）是**全局**预算：每造出一个词减一，
//!    减到 0 就整棵树收工（`encoder.cc:252-255`、`284-286`）。
//! 5. `ScriptEncoder::DfsEncode` 用 `phrase.substr(start_pos, k)` 按**字节**截，
//!    k 从字节长度倒数到 1（`encoder.cc:318`）—— 多字节字符会截出半个字。
//!    这里跳过非字符边界的 k（那些键永远查不到，观察结果相同）。
#![allow(dead_code)]

use serde_json::Value;

use super::calculus::regex::Regex;
use super::strings::{SplitBehavior, join, split};

/// `encoder.cc:15`：DFS 预算。
pub const K_ENCODER_DFS_LIMIT: i32 = 32;
/// `encoder.cc:16`：短语最大字数。
pub const K_MAX_PHRASE_LENGTH: i32 = 32;

/// `encoder.h:16-20`：一串音节码（`["abc", "def"]`）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawCode(Vec<String>);

impl RawCode {
    fn new() -> Self {
        Self::default()
    }

    /// `encoder.cc:18-20`：`strings::join(*this, " ")`
    fn to_string(&self) -> String {
        join(self.0.iter(), " ")
    }

    /// `encoder.cc:22-25`：`strings::split(code_str, " ", SkipToken)`
    fn from_string(code_str: &str) -> Self {
        Self(split(code_str, " ", SplitBehavior::SkipToken))
    }

    fn push(&mut self, code: String) {
        self.0.push(code);
    }

    fn pop(&mut self) -> Option<String> {
        self.0.pop()
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::ops::Deref for RawCode {
    type Target = Vec<String>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for RawCode {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// `encoder.h:22-32`：造词结果的接收方。
pub trait PhraseCollector {
    /// `encoder.h:27-29`
    fn create_entry(&mut self, phrase: &str, code_str: &str, value: &str);
    /// `encoder.h:30-31`：返回该词的可选编码；返回 false 表示查不到。
    fn translate_word(&mut self, word: &str, code: &mut Vec<String>) -> bool;
}

/// `encoder.h:36-49`
pub trait Encoder {
    /// `encoder.h:41`：基类默认 false。
    fn load_settings(&mut self, config: &Value) -> bool {
        let _ = config;
        false
    }

    /// `encoder.h:43`
    fn encode_phrase(&mut self, phrase: &str, value: &str) -> bool;
}

/// `encoder.h:54-57`：`Aa` = 第 0 个字的第 0 个码；`Az` = 第 0 个字的第 -1 个码。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodeCoords {
    char_index: i32,
    code_index: i32,
}

/// `encoder.h:59-63`
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableEncodingRule {
    min_word_length: i32,
    max_word_length: i32,
    coords: Vec<CodeCoords>,
}

/// 配置树取标量：`Item` 里的数字可能存成字符串（C++ 的 `ConfigValue` 就是字符串）。
fn value_as_i32(value: &Value) -> Option<i32> {
    match value {
        Value::Number(n) => n.as_i64().map(|v| v as i32),
        Value::String(s) => s.trim().parse::<i32>().ok(),
        _ => None,
    }
}

/// 配置树取字符串标量。
fn value_as_str(value: &Value) -> Option<&str> {
    value.as_str()
}

fn is_scalar(value: &Value) -> bool {
    !value.is_array() && !value.is_object() && !value.is_null()
}

/// 在 `haystack` 里从 `from` 开始找 `needles` 中任一字节的位置（C++ `find_first_of`）。
fn find_first_of_from(haystack: &[u8], needles: &[u8], from: usize) -> Option<usize> {
    if needles.is_empty() || from >= haystack.len() {
        return None;
    }
    haystack[from..]
        .iter()
        .position(|b| needles.contains(b))
        .map(|p| from + p)
}

/// `encoder.h:66-99`：按公式取码。
///
/// # 为什么这里没有 `collector` 字段
///
/// C++ 的 `TableEncoder` 存一个 `PhraseCollector* collector_`，构造时
/// `new TableEncoder(this)` 指向自己那边（`entry_collector` / `unity_table_encoder`
/// 都是这种自引用写法）。Rust 侧把这个字段拆走了，原因有两条：
///
/// 1. **自引用写不出来**：`UnityTableEncoder` 既是编码器又是收集方，字段互指会打架；
/// 2. **`Box<dyn PhraseCollector>` 不是 `Send`/`Sync`**（`entry_collector` 那边用
///    `Rc<RefCell<..>>`），而引擎要求组件 `Send + Sync`。留着这个字段，
///    `TableEncoder` 一进 `TableTranslator` 就编不过。
///
/// 于是收集方变成**每次调用的参数**（[`Self::encode_phrase_with`]）；
/// 编译期那条需要"绑定一个收集方"的路径用 [`BoundTableEncoder`] 包装。
/// `TableEncoder` 本身只剩纯数据，因此天然 `Send + Sync`。
pub struct TableEncoder {
    loaded: bool,
    encoding_rules: Vec<TableEncodingRule>,
    /// C++ 是 `vector<boost::regex>`
    exclude_patterns: Vec<Regex>,
    tail_anchor: String,
    /// 只用来快速拒掉过长的词
    max_phrase_length: i32,
}

impl Default for TableEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl TableEncoder {
    /// `encoder.cc:27-28`
    pub fn new() -> Self {
        Self {
            loaded: false,
            encoding_rules: Vec::new(),
            exclude_patterns: Vec::new(),
            tail_anchor: String::new(),
            max_phrase_length: 0,
        }
    }

    /// `TableEncoder::loaded()`：`encoder/rules` 里至少有一条规则解析成功。
    pub fn loaded(&self) -> bool {
        self.loaded
    }

    fn encoding_rules(&self) -> &[TableEncodingRule] {
        &self.encoding_rules
    }

    fn tail_anchor(&self) -> &str {
        &self.tail_anchor
    }

    fn max_phrase_length(&self) -> i32 {
        self.max_phrase_length
    }

    /// `encoder.cc:45-106`：读 `encoder/*` 设置。返回 `loaded_`。
    pub fn load_settings(&mut self, config: &Value) -> bool {
        self.loaded = false;
        self.max_phrase_length = 0;
        self.encoding_rules.clear();
        self.exclude_patterns.clear();
        self.tail_anchor.clear();

        // C++：`if (!config) return false;`
        if config.is_null() {
            return false;
        }

        // `encoder.cc:55-93`
        if let Some(rules) = config
            .get("encoder")
            .and_then(|e| e.get("rules"))
            .and_then(|r| r.as_array())
        {
            for item in rules {
                let Some(rule) = item.as_object() else {
                    continue;
                };
                if !rule.contains_key("formula") {
                    continue;
                }
                let Some(formula) = rule.get("formula").and_then(|v| v.as_str()) else {
                    continue;
                };
                let mut r = TableEncodingRule::default();
                if !Self::parse_formula(formula, &mut r) {
                    continue;
                }
                r.min_word_length = 0;
                r.max_word_length = 0;
                // `encoder.cc:65-74`：`length_equal` 优先
                let length_equal = rule.get("length_equal").filter(|v| is_scalar(v));
                if let Some(value) = length_equal {
                    let Some(length) = value_as_i32(value) else {
                        continue; // "invalid length"
                    };
                    r.min_word_length = length;
                    r.max_word_length = length;
                    if self.max_phrase_length < length {
                        self.max_phrase_length = length;
                    }
                } else if let Some(range) = rule.get("length_in_range").and_then(|v| v.as_array()) {
                    // `encoder.cc:75-87`
                    if range.len() != 2 {
                        continue;
                    }
                    let (Some(min), Some(max)) = (value_as_i32(&range[0]), value_as_i32(&range[1]))
                    else {
                        continue; // "invalid range."
                    };
                    if min > max {
                        continue;
                    }
                    r.min_word_length = min;
                    r.max_word_length = max;
                    if self.max_phrase_length < max {
                        self.max_phrase_length = max;
                    }
                }
                self.encoding_rules.push(r);
            }
            // `encoder.cc:90-92`
            if self.max_phrase_length > K_MAX_PHRASE_LENGTH {
                self.max_phrase_length = K_MAX_PHRASE_LENGTH;
            }
        }

        // `encoder.cc:94-101`
        if let Some(excludes) = config
            .get("encoder")
            .and_then(|e| e.get("exclude_patterns"))
            .and_then(|v| v.as_array())
        {
            for item in excludes {
                if !is_scalar(item) {
                    continue;
                }
                let Some(pattern) = value_as_str(item) else {
                    continue;
                };
                // C++ 直接 `boost::regex(pattern)`，非法模式会抛异常（没人接）——
                // 这里选择跳过，行为更稳，且不影响任何合法配置。
                if let Ok(re) = Regex::new(pattern) {
                    self.exclude_patterns.push(re);
                }
            }
        }

        // `encoder.cc:102`
        if let Some(anchor) = config
            .get("encoder")
            .and_then(|e| e.get("tail_anchor"))
            .and_then(|v| v.as_str())
        {
            self.tail_anchor = anchor.to_string();
        }

        // `encoder.cc:104-105`
        self.loaded = !self.encoding_rules.is_empty();
        self.loaded
    }

    /// `encoder.cc:108-131`：解析 `"AaAzBaBz"` 这类公式。
    fn parse_formula(formula: &str, rule: &mut TableEncodingRule) -> bool {
        let bytes = formula.as_bytes();
        if bytes.len() % 2 != 0 {
            return false; // "bad formula"
        }
        let mut i = 0usize;
        while i < bytes.len() {
            let mut c = CodeCoords::default();
            let upper = bytes[i];
            if !(b'A'..=b'Z').contains(&upper) {
                return false; // "invalid character index"
            }
            // `encoder.cc:120`：U..Z → -6..-1
            c.char_index = if upper >= b'U' {
                upper as i32 - b'Z' as i32 - 1
            } else {
                upper as i32 - b'A' as i32
            };
            i += 1;
            let lower = bytes[i];
            if !(b'a'..=b'z').contains(&lower) {
                return false; // "invalid code index"
            }
            // `encoder.cc:126`
            c.code_index = if lower >= b'u' {
                lower as i32 - b'z' as i32 - 1
            } else {
                lower as i32 - b'a' as i32
            };
            i += 1;
            rule.coords.push(c);
        }
        true
    }

    /// `encoder.cc:133-139`：命中任一排除模式（**整串**匹配）就算排除。
    fn is_code_excluded(&self, code: &str) -> bool {
        self.exclude_patterns.iter().any(|p| p.is_full_match(code))
    }

    /// `encoder.cc:141-198`：按规则表取码；`result` 在命中时被覆盖。
    fn encode(&self, code: &RawCode, result: &mut String) -> bool {
        let num_syllables = code.len() as i32;
        for rule in &self.encoding_rules {
            if num_syllables < rule.min_word_length || num_syllables > rule.max_word_length {
                continue;
            }
            let mut out: Vec<u8> = Vec::new();
            // `CodeCoords previous = {0, 0}; CodeCoords encoded = {0, 0};`
            let mut previous = CodeCoords::default();
            let mut encoded = CodeCoords::default();
            for current in &rule.coords {
                let mut c = *current;
                if c.char_index < 0 {
                    c.char_index += num_syllables;
                }
                if c.char_index >= num_syllables {
                    continue; // 'abc def' ~ 'Ca'
                }
                if c.char_index < 0 {
                    continue; // 'abc def' ~ 'Xa'
                }
                if current.char_index < 0 && c.char_index < encoded.char_index {
                    continue; // 'abc def' ~ '(AaBa)Ya'
                }
                let mut start_index = 0;
                if c.char_index == encoded.char_index {
                    start_index = encoded.code_index + 1;
                }
                let syllable = &code[c.char_index as usize];
                c.code_index = self.calculate_code_index(syllable, c.code_index, start_index);
                if c.code_index >= syllable.len() as i32 {
                    continue; // 'abc def' ~ 'Ad'
                }
                if c.code_index < 0 {
                    continue; // 'abc def' ~ 'Ax'
                }
                if (current.char_index < 0 || current.code_index < 0)
                    && c.char_index == encoded.char_index
                    && c.code_index <= encoded.code_index
                    && (current.char_index != previous.char_index
                        || current.code_index != previous.code_index)
                {
                    continue; // 'abc def' ~ '(AaBb)By'
                }
                // `*result += code[c.char_index][c.code_index];`
                let byte = syllable.as_bytes()[c.code_index as usize];
                out.push(byte);
                previous = *current;
                encoded = c;
            }
            if out.is_empty() {
                continue;
            }
            *result = String::from_utf8_lossy(&out).into_owned();
            return true;
        }
        false
    }

    /// `encoder.cc:200-234`：把"虚拟码位"换算成 `code` 里的**字节**下标。
    ///
    /// * `index` 为负时从尾部数，尾锚（`tail_anchor`）不算编码字符；
    /// * `start` 仅在 `index` 为负时使用：从 `start` 之后找第一个尾锚来定位 -1。
    fn calculate_code_index(&self, code: &str, index: i32, start: i32) -> i32 {
        let bytes = code.as_bytes();
        let n = bytes.len() as i32;
        let anchors = self.tail_anchor.as_bytes();
        let mut k: i32 = 0;
        if index < 0 {
            k = n - 1;
            // `code.find_first_of(tail_anchor_, start + 1)`
            let from = if start + 1 < 0 {
                usize::MAX
            } else {
                (start + 1) as usize
            };
            if let Some(tail) = find_first_of_from(bytes, anchors, from) {
                k = tail as i32 - 1;
            }
            let mut index = index;
            // while (++index < 0) { while (--k >= 0 && is_anchor(code[k])) {} }
            loop {
                index += 1;
                if index >= 0 {
                    break;
                }
                loop {
                    k -= 1;
                    if k < 0 {
                        break;
                    }
                    if !anchors.contains(&bytes[k as usize]) {
                        break;
                    }
                }
            }
        } else {
            let mut index = index;
            // while (index-- > 0) { while (++k < n && is_anchor(code[k])) {} }
            loop {
                if index <= 0 {
                    break;
                }
                index -= 1;
                loop {
                    k += 1;
                    if k >= n {
                        break;
                    }
                    if !anchors.contains(&bytes[k as usize]) {
                        break;
                    }
                }
            }
        }
        k
    }

    /// `encoder.cc:247-290`：逐字 DFS，把 `code` 的所有组合都试一遍。
    ///
    /// `collector` 是**显式入参**而不是 `self.collector`：C++ 用 `collector_(this)`
    /// 让 `UnityTableEncoder` 既是编码器又是收集方（自引用）。Rust 不能这么写，
    /// 于是把"收集方"提到每次调用的参数上 —— 见
    /// [`Self::encode_phrase_with`]。
    fn dfs_encode(
        &self,
        collector: &mut dyn PhraseCollector,
        phrase: &str,
        value: &str,
        start_pos: usize,
        code: &mut RawCode,
        limit: &mut i32,
    ) -> bool {
        if start_pos == phrase.len() {
            *limit -= 1;
            let mut encoded = String::new();
            if self.encode(code, &mut encoded) {
                collector.create_entry(phrase, &encoded, value);
                return true;
            }
            return false;
        }
        // `const char* word_end = word_start; utf8::unchecked::next(word_end);`
        let word_end = match phrase[start_pos..].chars().next() {
            Some(c) => start_pos + c.len_utf8(),
            None => phrase.len(),
        };
        let word = &phrase[start_pos..word_end];
        let mut ret = false;
        let mut translations: Vec<String> = Vec::new();
        if !collector.translate_word(word, &mut translations) {
            translations.clear();
        }
        for x in translations {
            if self.is_code_excluded(&x) {
                continue;
            }
            code.push(x);
            let ok = self.dfs_encode(collector, phrase, value, word_end, code, limit);
            ret = ret || ok;
            code.pop();
            if *limit <= 0 {
                return ret;
            }
        }
        ret
    }

    /// `EncodePhrase` 的"自带收集方"版本（`encoder.cc:236-245` 的语义 + 显式 collector）。
    ///
    /// `self` 只需要 `&`：编码规则、tail anchor、exclude patterns 都是只读的。
    /// 这让 [`crate::engine::gear::unity_table_encoder::UnityTableEncoder`] 能把
    /// 自己的两个字段（反查表 + 用户词典）同时借进来当收集方。
    pub fn encode_phrase_with(
        &self,
        collector: &mut dyn PhraseCollector,
        phrase: &str,
        value: &str,
    ) -> bool {
        let phrase_length = phrase.chars().count() as i32;
        if phrase_length > self.max_phrase_length {
            return false;
        }
        let mut code = RawCode::new();
        let mut limit = K_ENCODER_DFS_LIMIT;
        self.dfs_encode(collector, phrase, value, 0, &mut code, &mut limit)
    }
}

/// `TableEncoder` + 一个绑定好的收集方 = C++ 的 `new TableEncoder(this)`。
///
/// 只有词典编译期（[`crate::dict::entry_collector`]）需要这种"绑死收集方"的形态；
/// 引擎侧的 [`crate::engine::gear::unity_table_encoder::UnityTableEncoder`]
/// 走 [`TableEncoder::encode_phrase_with`]，每次调用现拼收集方。
///
/// 这个类型**不是** `Send`/`Sync`（收集方往往是 `Rc<RefCell<..>>`），
/// 但也只在单线程的编译流程里用 —— 拆开就是为了让 `TableEncoder` 本身能进引擎。
pub struct BoundTableEncoder {
    encoder: TableEncoder,
    collector: Box<dyn PhraseCollector>,
}

impl BoundTableEncoder {
    /// `new TableEncoder(this)`（`encoder.cc:27-28`）。
    pub fn new(collector: Box<dyn PhraseCollector>) -> Self {
        Self {
            encoder: TableEncoder::new(),
            collector,
        }
    }

    /// 内层编码器的只读视图（规则数量之类）。
    pub fn encoder(&self) -> &TableEncoder {
        &self.encoder
    }
}

impl Encoder for BoundTableEncoder {
    fn load_settings(&mut self, config: &Value) -> bool {
        self.encoder.load_settings(config)
    }

    /// `encoder.cc:236-245`
    fn encode_phrase(&mut self, phrase: &str, value: &str) -> bool {
        self.encoder
            .encode_phrase_with(self.collector.as_mut(), phrase, value)
    }
}

/// `encoder.h:102-114`：按音节切分编码（最长词优先）。
pub struct ScriptEncoder {
    collector: Option<Box<dyn PhraseCollector>>,
}

impl ScriptEncoder {
    /// `encoder.cc:31-32`：`new ScriptEncoder(this)`。
    /// `dict::entry_collector` 造词时用（collector 经 `PhraseCollector` 句柄传入）。
    pub fn new(collector: Box<dyn PhraseCollector>) -> Self {
        Self {
            collector: Some(collector),
        }
    }

    /// `encoder.cc:305-334`
    fn dfs_encode(
        &mut self,
        phrase: &str,
        value: &str,
        start_pos: usize,
        code: &mut RawCode,
        limit: &mut i32,
    ) -> bool {
        if start_pos == phrase.len() {
            *limit -= 1;
            if let Some(collector) = self.collector.as_mut() {
                collector.create_entry(phrase, &code.to_string(), value);
            }
            return true;
        }
        let mut ret = false;
        // `for (size_t k = phrase.length() - start_pos; k > 0; --k)`
        for k in (1..=phrase.len() - start_pos).rev() {
            // C++ 按字节截；非字符边界的 k 在 C++ 里也查不到任何词，跳过等价
            let Some(word) = phrase.get(start_pos..start_pos + k) else {
                continue;
            };
            let mut translations: Vec<String> = Vec::new();
            if let Some(collector) = self.collector.as_mut() {
                if !collector.translate_word(word, &mut translations) {
                    translations.clear();
                }
            }
            for x in translations {
                code.push(x);
                let ok = self.dfs_encode(phrase, value, start_pos + k, code, limit);
                ret = ret || ok;
                code.pop();
                if *limit <= 0 {
                    return ret;
                }
            }
        }
        ret
    }
}

impl Encoder for ScriptEncoder {
    /// `encoder.cc:294-303`
    fn encode_phrase(&mut self, phrase: &str, value: &str) -> bool {
        let phrase_length = phrase.chars().count() as i32;
        if phrase_length > K_MAX_PHRASE_LENGTH {
            return false;
        }
        let mut code = RawCode::new();
        let mut limit = K_ENCODER_DFS_LIMIT;
        self.dfs_encode(phrase, value, 0, &mut code, &mut limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    /// 记下 `CreateEntry` 的调用；`TranslateWord` 查一张表。
    #[derive(Default, Clone)]
    struct Recorder {
        entries: Rc<RefCell<Vec<(String, String, String)>>>,
        words: Rc<BTreeMap<String, Vec<String>>>,
    }

    impl PhraseCollector for Recorder {
        fn create_entry(&mut self, phrase: &str, code_str: &str, value: &str) {
            self.entries.borrow_mut().push((
                phrase.to_string(),
                code_str.to_string(),
                value.to_string(),
            ));
        }

        fn translate_word(&mut self, word: &str, code: &mut Vec<String>) -> bool {
            match self.words.get(word) {
                Some(v) => {
                    code.extend(v.iter().cloned());
                    true
                }
                None => false,
            }
        }
    }

    fn rules_config(rules: Value) -> Value {
        serde_json::json!({ "encoder": { "rules": rules } })
    }

    // ---- 以下 4 条对应 librime `test/encoder_test.cc` 的全部断言 ----

    #[test]
    fn encoder_settings() {
        // encoder_test.cc:12-48
        let config = rules_config(serde_json::json!([
            { "length_equal": 2, "formula": "AaAzBaBz" },
            { "length_equal": 3, "formula": "AaBaCaBz" },
            { "length_in_range": [4, 9], "formula": "AaBaCaZz" }
        ]));
        let mut encoder = TableEncoder::new();
        assert!(encoder.load_settings(&config));
        assert!(encoder.loaded());
        let rules = encoder.encoding_rules();
        assert_eq!(3, rules.len());
        assert_eq!(2, rules[0].min_word_length);
        assert_eq!(2, rules[0].max_word_length);
        assert_eq!(4, rules[2].min_word_length);
        assert_eq!(9, rules[2].max_word_length);
        assert_eq!(4, rules[0].coords.len());
        assert_eq!(
            (0, 0),
            (rules[0].coords[0].char_index, rules[0].coords[0].code_index)
        );
        assert_eq!(
            (0, -1),
            (rules[0].coords[1].char_index, rules[0].coords[1].code_index)
        );
        assert_eq!(
            (1, 0),
            (rules[0].coords[2].char_index, rules[0].coords[2].code_index)
        );
        assert_eq!(
            (1, -1),
            (rules[0].coords[3].char_index, rules[0].coords[3].code_index)
        );
        assert_eq!(4, rules[2].coords.len());
        assert_eq!(
            (0, 0),
            (rules[2].coords[0].char_index, rules[2].coords[0].code_index)
        );
        assert_eq!(
            (1, 0),
            (rules[2].coords[1].char_index, rules[2].coords[1].code_index)
        );
        assert_eq!(
            (2, 0),
            (rules[2].coords[2].char_index, rules[2].coords[2].code_index)
        );
        assert_eq!(
            (-1, -1),
            (rules[2].coords[3].char_index, rules[2].coords[3].code_index)
        );
    }

    #[test]
    fn encoder_exclude_patterns() {
        // encoder_test.cc:50-58
        let config = serde_json::json!({ "encoder": { "exclude_patterns": ["^x.*$"] } });
        let mut encoder = TableEncoder::new();
        // 只有 exclude_patterns、没有 rules → loaded = false，但模式已经生效
        assert!(!encoder.load_settings(&config));
        assert!(encoder.is_code_excluded("x"));
        assert!(encoder.is_code_excluded("xyz"));
        assert!(!encoder.is_code_excluded("XYZ"));
    }

    #[test]
    fn encode_cases() {
        // encoder_test.cc:60-105（7 个 case 逐条照抄）
        let mut encoder = TableEncoder::new();
        let c0 = RawCode::from_string("abc def");
        let c1 = RawCode::from_string("abc def ghi");
        assert_eq!(c0.0, vec!["abc", "def"]);
        assert_eq!(c1.to_string(), "abc def ghi");
        let mut result = String::new();

        // case 1
        let mut config = rules_config(serde_json::json!([
            { "length_equal": 2, "formula": "AaAbBaBb" }
        ]));
        assert!(encoder.load_settings(&config));
        assert!(encoder.encode(&c0, &mut result));
        assert_eq!("abde", result);

        // case 2：追加第二条规则
        config = rules_config(serde_json::json!([
            { "length_equal": 2, "formula": "AaAbBaBb" },
            { "length_in_range": [3, 5], "formula": "AaAzBaBzCaCz" }
        ]));
        assert!(encoder.load_settings(&config));
        assert!(encoder.encode(&c1, &mut result));
        assert_eq!("acdfgi", result);

        // case 3
        config = rules_config(serde_json::json!([
            { "length_equal": 2, "formula": "AaAzBaBzCaCz" },
            { "length_in_range": [3, 5], "formula": "AaAzBaBzCaCz" }
        ]));
        assert!(encoder.load_settings(&config));
        assert!(encoder.encode(&c0, &mut result));
        assert_eq!("acdf", result);

        // case 4
        config = rules_config(serde_json::json!([
            { "length_equal": 2, "formula": "AaAbZyZz" },
            { "length_in_range": [3, 5], "formula": "AaAzBaBzCaCz" }
        ]));
        assert!(encoder.load_settings(&config));
        assert!(encoder.encode(&c0, &mut result));
        assert_eq!("abef", result);

        // case 5
        config = rules_config(serde_json::json!([
            { "length_equal": 2, "formula": "AaAaBbBbZzZz" },
            { "length_in_range": [3, 5], "formula": "AaAzBaBzCaCz" }
        ]));
        assert!(encoder.load_settings(&config));
        assert!(encoder.encode(&c0, &mut result));
        assert_eq!("aaeeff", result);

        // case 6
        config = rules_config(serde_json::json!([
            { "length_equal": 2, "formula": "AaAaBbBbZzZz" },
            { "length_in_range": [3, 5], "formula": "AzAzByByZaZa" }
        ]));
        assert!(encoder.load_settings(&config));
        assert!(encoder.encode(&c1, &mut result));
        assert_eq!("cceegg", result);

        // case 7
        config = rules_config(serde_json::json!([
            { "length_equal": 2, "formula": "AaBaYaZaZz" },
            { "length_in_range": [3, 5], "formula": "AzAzByByZaZa" }
        ]));
        assert!(encoder.load_settings(&config));
        assert!(encoder.encode(&c0, &mut result));
        assert_eq!("adf", result);
    }

    #[test]
    fn tail_anchor_cases() {
        // encoder_test.cc:107-130
        let mut encoder = TableEncoder::new();
        let mut config = serde_json::json!({
            "encoder": {
                "rules": [{ "length_equal": 3, "formula": "AaAzBaBzCaCz" }],
                "tail_anchor": "'"
            }
        });
        let c = RawCode::from_string("zyx'wvu'tsr qpo'nmlk'jih gfedcba");
        let mut result = String::new();

        // case 1
        assert!(encoder.load_settings(&config));
        assert_eq!(encoder.tail_anchor(), "'");
        assert!(encoder.encode(&c, &mut result));
        assert_eq!("zxqoga", result);

        // case 2
        config["encoder"]["rules"][0]["formula"] = serde_json::json!("AaAbAcAzBwBxByBz");
        assert!(encoder.load_settings(&config));
        assert!(encoder.encode(&c, &mut result));
        assert_eq!("zyxuqpo", result);

        // case 3
        config["encoder"]["rules"][0]["formula"] = serde_json::json!("AaAbAcAdAzBaBxByBz");
        assert!(encoder.load_settings(&config));
        assert!(encoder.encode(&c, &mut result));
        assert_eq!("zyxwuqpo", result);
    }

    // ---- 额外的语义钉子 ----

    #[test]
    fn formula_letters_map_to_discontinuous_indices() {
        // `encoder.cc:120,126`：U..Z → -6..-1，u..z → -6..-1
        let mut rule = TableEncodingRule::default();
        assert!(TableEncoder::parse_formula("AaUuZz", &mut rule));
        assert_eq!(
            rule.coords,
            vec![
                CodeCoords {
                    char_index: 0,
                    code_index: 0
                },
                CodeCoords {
                    char_index: -6,
                    code_index: -6
                },
                CodeCoords {
                    char_index: -1,
                    code_index: -1
                },
            ]
        );
        // 奇数长度 / 非字母 → false
        assert!(!TableEncoder::parse_formula(
            "AaA",
            &mut TableEncodingRule::default()
        ));
        assert!(!TableEncoder::parse_formula(
            "A1",
            &mut TableEncodingRule::default()
        ));
        assert!(!TableEncoder::parse_formula(
            "1a",
            &mut TableEncodingRule::default()
        ));
    }

    #[test]
    fn rule_without_length_never_matches() {
        // `encoder.cc:64`：min = max = 0
        let config = rules_config(serde_json::json!([{ "formula": "AaBa" }]));
        let mut encoder = TableEncoder::new();
        assert!(encoder.load_settings(&config));
        assert_eq!(0, encoder.max_phrase_length());
        let c0 = RawCode::from_string("abc def");
        let mut result = String::new();
        assert!(!encoder.encode(&c0, &mut result));
        // EncodePhrase 对任何非空词也直接 false（max_phrase_length 是 0）
        assert!(!encoder.encode_phrase_with(&mut Recorder::default(), "ab", "value"));
    }

    #[test]
    fn bad_length_is_skipped() {
        // `length_equal` 不是整数 → 整条规则被丢
        let config = rules_config(serde_json::json!([
            { "length_equal": "abc", "formula": "AaBa" },
            { "length_equal": 2, "formula": "AaBa" }
        ]));
        let mut encoder = TableEncoder::new();
        assert!(encoder.load_settings(&config));
        assert_eq!(1, encoder.encoding_rules().len());
        // range 反了也丢
        let config = rules_config(serde_json::json!([
            { "length_in_range": [9, 4], "formula": "AaBa" }
        ]));
        let mut encoder = TableEncoder::new();
        assert!(!encoder.load_settings(&config));
    }

    #[test]
    fn table_encoder_phrase_dfs_and_limit() {
        // 造词：每个字查表拿码，DFS 出所有组合
        let mut words = BTreeMap::new();
        words.insert("中".to_string(), vec!["z".to_string(), "zo".to_string()]);
        words.insert("国".to_string(), vec!["g".to_string(), "go".to_string()]);
        let recorder = Recorder {
            entries: Rc::new(RefCell::new(Vec::new())),
            words: Rc::new(words),
        };
        let entries = recorder.entries.clone();
        let mut encoder = BoundTableEncoder::new(Box::new(recorder));
        // AaAzBaBz：每个字取"首码 + 尾码"
        let config = rules_config(serde_json::json!([
            { "length_equal": 2, "formula": "AaAzBaBz" }
        ]));
        assert!(encoder.load_settings(&config));
        assert!(Encoder::encode_phrase(&mut encoder, "中国", "value"));
        let got = entries.borrow().clone();
        assert_eq!(got.len(), 4, "中2码 × 国2码 = 4 种组合: {got:?}");
        assert_eq!(got[0].0, "中国");
        assert_eq!(got[0].2, "value");
        // 中=z/zo 国=g/go → zg / zgo / zog / zogo
        // ⚠️ "Az" 紧跟在 "Aa" 之后时会被 `code_index <= encoded.code_index` 那条
        // `continue` 挡掉（`encoder.cc:178-186`），所以尾码不会重复取自同一下标。
        assert!(got.iter().any(|(_, code, _)| code == "zg"), "{got:?}");
        assert!(got.iter().any(|(_, code, _)| code == "zog"), "{got:?}");
        assert!(got.iter().any(|(_, code, _)| code == "zogo"), "{got:?}");
    }

    #[test]
    fn exclude_patterns_filter_translations() {
        let mut words = BTreeMap::new();
        words.insert("中".to_string(), vec!["x".to_string(), "z".to_string()]);
        words.insert("国".to_string(), vec!["g".to_string()]);
        let recorder = Recorder {
            entries: Rc::new(RefCell::new(Vec::new())),
            words: Rc::new(words),
        };
        let entries = recorder.entries.clone();
        let mut encoder = BoundTableEncoder::new(Box::new(recorder));
        let config = serde_json::json!({
            "encoder": {
                "rules": [{ "length_equal": 2, "formula": "AaBa" }],
                "exclude_patterns": ["^x.*$"]
            }
        });
        assert!(encoder.load_settings(&config));
        assert!(Encoder::encode_phrase(&mut encoder, "中国", "value"));
        let got = entries.borrow().clone();
        assert!(got.iter().all(|(_, code, _)| code != "xg"), "{got:?}");
        assert!(got.iter().any(|(_, code, _)| code == "zg"));
    }

    #[test]
    fn script_encoder_prefers_longest_word() {
        // ScriptEncoder：最长词优先（encoder.cc:318）
        let mut words = BTreeMap::new();
        words.insert("中国".to_string(), vec!["zhong guo".to_string()]);
        words.insert("中".to_string(), vec!["zhong".to_string()]);
        words.insert("国".to_string(), vec!["guo".to_string()]);
        let recorder = Recorder {
            entries: Rc::new(RefCell::new(Vec::new())),
            words: Rc::new(words),
        };
        let entries = recorder.entries.clone();
        let mut encoder = ScriptEncoder::new(Box::new(recorder));
        assert!(ScriptEncoder::encode_phrase(&mut encoder, "中国", "v"));
        let got = entries.borrow().clone();
        // 最长优先 → 先出整词
        assert_eq!(got[0].0, "中国");
        assert_eq!(got[0].1, "zhong guo");
        // 也能逐字造
        assert!(got.iter().any(|(_, code, _)| code == "zhong guo"));
        // 超过 32 字直接拒
        let long: String = "中".repeat(33);
        assert!(!ScriptEncoder::encode_phrase(&mut encoder, &long, "v"));
    }

    #[test]
    fn dfs_limit_is_global_budget() {
        // `limit` 预算：每造出一词减一，减到 0 收工（encoder.cc:253-255）
        let mut words = BTreeMap::new();
        for w in ["a", "b", "c"] {
            words.insert(w.to_string(), vec![w.to_string(), format!("{w}{w}")]);
        }
        let recorder = Recorder {
            entries: Rc::new(RefCell::new(Vec::new())),
            words: Rc::new(words),
        };
        let entries = recorder.entries.clone();
        let mut encoder = ScriptEncoder::new(Box::new(recorder));
        assert!(ScriptEncoder::encode_phrase(&mut encoder, "abc", "v"));
        let count = entries.borrow().len();
        assert!(count > 0);
        assert!(count <= K_ENCODER_DFS_LIMIT as usize, "count = {count}");
    }

    #[test]
    fn raw_code_round_trip() {
        let code = RawCode::from_string("  abc   def  ");
        assert_eq!(code.0, vec!["abc", "def"]);
        assert_eq!(code.to_string(), "abc def");
        // 空串 → 空码（SkipToken）
        assert!(RawCode::from_string("").is_empty());
    }
}
