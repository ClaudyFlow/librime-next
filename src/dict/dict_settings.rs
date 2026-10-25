//! `src/rime/dict/dict_settings.{h,cc}`（25 + 119 行）的 Rust 移植：词典 YAML 头。
//!
//! # 什么是"词典 YAML 头"
//!
//! `.dict.yaml` 分两段：YAML 头（`name` / `version` / `sort` / `import_tables` /
//! `columns` / `encoder` …）和 `...` 之后的词条表。`LoadDictHeader` 只吃第一段，
//! 读完把流的剩余部分（词条表）留给调用方 —— 所以这里的入口是：
//!
//! ```text
//! let (settings, rest) = DictSettings::load_dict_header(text)?;  // rest = 词条表
//! ```
//!
//! # 与 C++ 的接口差异
//!
//! * C++ `LoadDictHeader(istream&)`：流式读，读完头流停在词条表开头。Rust 没有
//!   "读一半的流"这种便宜结构，直接收 `&str` 返回 `(Self, &str)`（零拷贝切片）。
//! * C++ `DictSettings : public Config`：整个 config 树都在。Rust 侧只保留
//!   [`Self::config`] 暴露头部的 `serde_json::Value`（encoder 的
//!   `load_settings` 要的就是这棵树）。
//!
//! # 取值语义（`config_types.cc`，逐条对拍过）
//!
//! | C++ | 这里 |
//! |---|---|
//! | `IsNull()`（缺 key 也是 null） | `Value::is_null()` / `get(key).is_none()` |
//! | `IsValue()`（标量才算） | 字符串 / 数字 / 布尔 |
//! | `ToString()`（null → ""） | 标量一律转字符串（C++ 的 ConfigValue 本就是字符串） |
//! | `ToBool()` | 只认小写 `true` / `false`（`GetBool`） |
//! | `ToInt()` | `0x` 十六进制或十进制前缀（`GetInt`） |
//! | `ToDouble()` | `std::stod` **前缀**解析（`GetDouble`） |

use std::path::Path;

use serde_json::Value;

/// 默认词库名（`dict_settings.cc:60` 的 `kDefaultVocabulary`）。
const K_DEFAULT_VOCABULARY: &str = "essay";

/// 词典 YAML 头（`DictSettings : public Config` 的等价物）。
#[derive(Debug, Clone, Default)]
pub struct DictSettings {
    config: Value,
}

impl DictSettings {
    /// `DictSettings::LoadDictHeader(istream&)`：从文本开头读 YAML 头。
    ///
    /// 返回 `(设置, 词条表文本)`。以下情况返回 `None`（C++ 对应 `return false`）：
    /// * 头部 YAML 解析失败；
    /// * 缺 `name` 或 `version`（`incomplete dict header`）。
    ///
    /// 没有 `...` 行时 C++ 会把整个文本当头部然后解析失败（词条表不是 YAML），
    /// 这里同样：解析失败 → `None`。
    pub fn load_dict_header(text: &str) -> Option<(Self, &str)> {
        // 逐行收进 header，遇到 "..." 停下（C++ 把 "..." 行本身也写进 header）
        let mut header = String::new();
        let mut rest = "";
        let mut offset = 0usize;
        for line in text.split_inclusive('\n') {
            let trimmed = line.trim_end_matches(['\n', '\r']);
            header.push_str(line);
            offset += line.len();
            if trimmed == "..." {
                rest = &text[offset..];
                break;
            }
        }
        let config = parse_yaml_header(&header)?;
        // `(*this)["name"].IsNull() || (*this)["version"].IsNull()` → 不完整
        if config.get("name").is_none_or(Value::is_null)
            || config.get("version").is_none_or(Value::is_null)
        {
            return None;
        }
        Some((Self { config }, rest))
    }

    /// 读整个文件再分头（`Collect(dict_file)` 的用法）。
    pub fn load_dict_header_from_file(path: &Path) -> std::io::Result<Option<(Self, String)>> {
        let text = std::fs::read_to_string(path)?;
        Ok(Self::load_dict_header(&text).map(|(settings, rest)| (settings, rest.to_string())))
    }

    /// `DictSettings::empty()`：`(*this)["name"].IsNull()`。
    pub fn is_empty(&self) -> bool {
        self.config.get("name").is_none_or(Value::is_null)
    }

    /// `dict_name()`：`name` 的字符串值（null → ""）。
    pub fn dict_name(&self) -> String {
        scalar_str(self.config.get("name")).unwrap_or_default()
    }

    /// `dict_version()`：`version` 的字符串值。
    pub fn dict_version(&self) -> String {
        scalar_str(self.config.get("version")).unwrap_or_default()
    }

    /// `sort_order()`：`sort` 的字符串值（缺省 ""，调用方与 "original" 比较）。
    pub fn sort_order(&self) -> String {
        scalar_str(self.config.get("sort")).unwrap_or_default()
    }

    /// `use_preset_vocabulary()`：`use_preset_vocabulary` 为真**或** `vocabulary`
    /// 是标量（`dict_settings.cc:55-58`）。
    pub fn use_preset_vocabulary(&self) -> bool {
        self.to_bool("use_preset_vocabulary")
            || self
                .config
                .get("vocabulary")
                .is_some_and(|v| !v.is_null() && !v.is_array() && !v.is_object())
    }

    /// `vocabulary()`：`vocabulary` 字符串，空则回退 `"essay"`。
    pub fn vocabulary(&self) -> String {
        let value = scalar_str(self.config.get("vocabulary")).unwrap_or_default();
        if value.is_empty() {
            K_DEFAULT_VOCABULARY.to_string()
        } else {
            value
        }
    }

    /// `use_rule_based_encoder()`：`encoder/rules` 是列表。
    pub fn use_rule_based_encoder(&self) -> bool {
        self.config
            .get("encoder")
            .and_then(|e| e.get("rules"))
            .is_some_and(Value::is_array)
    }

    /// `max_phrase_length()`：`ToInt()` 语义，缺省 0。
    pub fn max_phrase_length(&self) -> i32 {
        self.to_int("max_phrase_length")
    }

    /// `min_phrase_weight()`：`ToDouble()` 语义，缺省 0.0。
    pub fn min_phrase_weight(&self) -> f64 {
        self.to_double("min_phrase_weight")
    }

    /// `GetTables()`：`[name] + import_tables`，跳过"导自己"的项
    /// （`dict_settings.cc:79-96`）。空头返回空表。
    pub fn get_tables(&self) -> Vec<String> {
        if self.is_empty() {
            return Vec::new();
        }
        let mut tables = vec![self.dict_name()];
        if let Some(imports) = self.config.get("import_tables").and_then(Value::as_array) {
            for item in imports {
                let Some(table) = scalar_str(Some(item)) else {
                    continue; // `Is<ConfigValue>`：非标量跳过
                };
                if table == self.dict_name() {
                    continue; // cannot import from itself
                }
                tables.push(table);
            }
        }
        tables
    }

    /// `GetColumnIndex(label)`：`columns` 列表里找标量项；没有 `columns` 时按
    /// 默认列序 text=0 / code=1 / weight=2，其余 -1（`dict_settings.cc:98-117`）。
    pub fn get_column_index(&self, column_label: &str) -> i32 {
        if self.config.get("columns").is_none() {
            return match column_label {
                "text" => 0,
                "code" => 1,
                "weight" => 2,
                _ => -1,
            };
        }
        let Some(columns) = self.config.get("columns").and_then(Value::as_array) else {
            return -1;
        };
        for (index, item) in columns.iter().enumerate() {
            if scalar_str(Some(item)).as_deref() == Some(column_label) {
                return index as i32;
            }
        }
        -1
    }

    /// 头部配置树（encoder 的 `load_settings` / 规则编码器判定用）。
    pub fn config(&self) -> &Value {
        &self.config
    }

    // ---------------------------------------------------------------- 取值助手

    fn to_bool(&self, key: &str) -> bool {
        matches!(
            scalar_str(self.config.get(key)).as_deref().map(str::to_ascii_lowercase).as_deref(),
            Some("true")
        )
    }

    fn to_int(&self, key: &str) -> i32 {
        let Some(value) = scalar_str(self.config.get(key)) else {
            return 0;
        };
        parse_int_prefix(&value).unwrap_or(0)
    }

    fn to_double(&self, key: &str) -> f64 {
        let Some(value) = scalar_str(self.config.get(key)) else {
            return 0.0;
        };
        parse_double_prefix(&value).unwrap_or(0.0)
    }
}

/// 解析 YAML 头部文本（多文档取第一个；与 `config::load::load_file` 同套路：
/// 0 个文档 → `Null`）。
fn parse_yaml_header(header: &str) -> Option<Value> {
    let mut documents = serde_norway::Deserializer::from_str(header);
    let Some(document) = documents.next() else {
        return Some(Value::Null);
    };
    let value: serde_norway::Value = serde::Deserialize::deserialize(document).ok()?;
    serde_json::to_value(&value).ok()
}

/// `ConfigItem::ToString()`：标量才给字符串（null / 列表 / 映射 → None）。
/// C++ 的 ConfigValue 里一切都是字符串，这里把数字/布尔也转成字符串。
fn scalar_str(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(if *b { "true" } else { "false" }.to_string()),
        _ => None,
    }
}

/// `ConfigValue::GetInt`：`0x` 十六进制或十进制前缀（`config_types.cc:49-68`）。
fn parse_int_prefix(value: &str) -> Option<i32> {
    let trimmed = value.trim();
    if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
        return i64::from_str_radix(hex, 16).ok().map(|v| v as i32);
    }
    // 十进制：允许前导空白和符号（std::stoi 语义）
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digits_start {
        return None;
    }
    trimmed[..i].parse::<i32>().ok()
}

/// `ConfigValue::GetDouble`：`std::stod` 前缀解析（`config_types.cc:70-79`）。
/// `entry_collector` 的百分数权重也用它（C++ 同样是 stod）。
pub fn parse_double_prefix(value: &str) -> Option<f64> {
    let trimmed = value.trim();
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let mut digits = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    // 指数部分（stod 也认）
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        let exp_start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp_start {
            i = j;
        }
    }
    trimmed[..i].parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# Rime dictionary
---
name: luna_pinyin
version: \"1.0\"
sort: by_weight
use_preset_vocabulary: true
max_phrase_length: 8
min_phrase_weight: 0.05
import_tables:
  - luna_pinyin.english
columns:
  - text
  - code
  - weight
...
	luna	 Luna
";

    #[test]
    fn load_dict_header_splits_at_yaml_doc_end() {
        let (settings, rest) = DictSettings::load_dict_header(SAMPLE).expect("header");
        assert_eq!(settings.dict_name(), "luna_pinyin");
        assert_eq!(settings.dict_version(), "1.0");
        assert_eq!(settings.sort_order(), "by_weight");
        // 词条表原样返回（前导制表符是 C++ 切列前的状态）
        assert_eq!(rest, "\tluna\t Luna\n");
    }

    #[test]
    fn incomplete_header_is_rejected() {
        // 缺 version → None（C++: incomplete dict header）
        assert!(DictSettings::load_dict_header("name: x\n...\n").is_none());
        // 缺 name → None
        assert!(DictSettings::load_dict_header("version: \"1\"\n...\n").is_none());
    }

    #[test]
    fn preset_vocabulary_flags() {
        let (settings, _) = DictSettings::load_dict_header(SAMPLE).expect("header");
        assert!(settings.use_preset_vocabulary());
        assert_eq!(settings.vocabulary(), "essay", "未指定 vocabulary 时回退 essay");
        // 只写 vocabulary 标量也算启用
        let (s2, _) =
            DictSettings::load_dict_header("name: d\nversion: \"1\"\nvocabulary: my_vocab\n...\n")
                .expect("header");
        assert!(s2.use_preset_vocabulary());
        assert_eq!(s2.vocabulary(), "my_vocab");
        // 都没写 → 不启用
        let (s3, _) =
            DictSettings::load_dict_header("name: d\nversion: \"1\"\n...\n").expect("header");
        assert!(!s3.use_preset_vocabulary());
    }

    #[test]
    fn numeric_accessors_follow_cpp_semantics() {
        let (settings, _) = DictSettings::load_dict_header(SAMPLE).expect("header");
        assert_eq!(settings.max_phrase_length(), 8);
        assert!((settings.min_phrase_weight() - 0.05).abs() < 1e-9);
        // 缺省值
        let (bare, _) =
            DictSettings::load_dict_header("name: d\nversion: \"1\"\n...\n").expect("header");
        assert_eq!(bare.max_phrase_length(), 0);
        assert_eq!(bare.min_phrase_weight(), 0.0);
        // stod 前缀语义："1.5xyz" → 1.5
        let (s, _) = DictSettings::load_dict_header(
            "name: d\nversion: \"1\"\nmin_phrase_weight: 1.5xyz\n...\n",
        )
        .expect("header");
        assert!((s.min_phrase_weight() - 1.5).abs() < 1e-9);
    }

    #[test]
    fn get_tables_excludes_self_import() {
        let (settings, _) = DictSettings::load_dict_header(SAMPLE).expect("header");
        assert_eq!(settings.get_tables(), vec!["luna_pinyin", "luna_pinyin.english"]);
        // 导自己被跳过
        let text = "name: d\nversion: \"1\"\nimport_tables:\n  - d\n  - e\n...\n";
        let (settings, _) = DictSettings::load_dict_header(text).expect("header");
        assert_eq!(settings.get_tables(), vec!["d", "e"]);
    }

    #[test]
    fn column_index_defaults_and_custom() {
        let (settings, _) = DictSettings::load_dict_header(SAMPLE).expect("header");
        // 自定义列序：text=0 code=1 weight=2
        assert_eq!(settings.get_column_index("text"), 0);
        assert_eq!(settings.get_column_index("code"), 1);
        assert_eq!(settings.get_column_index("weight"), 2);
        assert_eq!(settings.get_column_index("stem"), -1);
        // 没有 columns 时走默认列序
        let text = "name: d\nversion: \"1\"\n...\n";
        let (bare, _) = DictSettings::load_dict_header(text).expect("header");
        assert_eq!(bare.get_column_index("text"), 0);
        assert_eq!(bare.get_column_index("weight"), 2);
        assert_eq!(bare.get_column_index("nope"), -1);
    }

    #[test]
    fn rule_based_encoder_detection() {
        let (settings, _) = DictSettings::load_dict_header(SAMPLE).expect("header");
        assert!(!settings.use_rule_based_encoder());
        let text = "name: d\nversion: \"1\"\nencoder:\n  rules:\n    - formula: \"AaAb\"\n...\n";
        let (settings, _) = DictSettings::load_dict_header(text).expect("header");
        assert!(settings.use_rule_based_encoder());
    }
}
