//! evo `DictCompiler::CompileAuxiliary`（`dict_compiler.cc:512-676`）的 Rust 移植。
//!
//! 编译一个**独立**的辅助码字典产物：`<group>.aux.bin`。它不依赖 `build/dictionaries`
//! 那套单方案多 prism 的重构，只吃两个节点：
//!
//! ```yaml
//! build:
//!   auxiliary:
//!     dictionary: aux            # 纯 `字<TAB>码` 数据表（可带 Rime 字典头）
//!     algebra:
//!       zhijie:                  # 组名 → 产物 `<staging>/zhijie.aux.bin`
//!         - "aux|^([a-z])([a-z])$|$1|"
//!         - "aux|^([a-z])([a-z])$|$1$2|abbrev"
//! ```
//!
//! 也支持单组简写（`algebra` 直接是列表 → 组名 `auto`）。
//!
//! # 增量
//!
//! 每组 checksum = **源文件 CRC32** 再逐条喂入该组规则（含结尾 NUL，照
//! `dict_compiler.cc:647-655`）。目标文件已存在且 checksum 相同就跳过 —— 改一组
//! 不影响别组重编。
//!
//! # 数据源约束（`dict_compiler.cc:514-545`）
//!
//! * 有 YAML 头就按头的 `columns` 找 `text` / `code`，无头默认 `0` / `1`；
//! * 只收**单字**（一个 UTF-8 码点后必须到行尾）；
//! * 码只允许 ASCII 字母/数字/`/`（`IsAsciiPrismKey`）；
//! * 同一 `(字, 码)` 去重；同一字可有多码、同一码可对多字。
//!
//! # 与 C++ 的接缝
//!
//! C++ 从 `build/auxiliary/algebra` 的 `ConfigList` 读规则；这里是 `serde_json::Value`
//! （与 `lever` 其他配置读取一致）。规则文本语义完全相同，仍是 [`Projection`]。
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::algo::utilities::checksum as file_checksum;
use crate::algo::{ChecksumComputer, Projection, Script, SpellingType};
use crate::gear::dict::auxiliary_db::{AuxiliaryCharacter, AuxiliaryCode, AuxiliaryDb};
use crate::gear::dict::dict_settings::DictSettings;

/// 一组辅助码规则。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuxiliaryGroup {
    /// 组名，同时是产物名（`<group>.aux.bin`）
    pub name: String,
    /// `aux|...` 规则原文
    pub rules: Vec<String>,
}

/// `build/auxiliary` 的解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuxiliaryConfig {
    /// `build/auxiliary/dictionary`（已补 `.yaml`）
    pub dictionary: String,
    pub groups: Vec<AuxiliaryGroup>,
}

/// 单组编译结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuxGroupResult {
    pub name: String,
    pub path: PathBuf,
    /// 是否真的重编（`false` = checksum 命中跳过）
    pub recompiled: bool,
    /// 字典里的**字数**（不是 `(字, 码)` 对数；同一个字可能有多码）
    pub entries: usize,
}

/// 解析 `build/auxiliary`。**没有该节点**返回 `Ok(None)`；格式错误返回 `Err`。
pub fn parse_auxiliary_config(config: &Value) -> Result<Option<AuxiliaryConfig>, String> {
    let Some(node) = config.pointer("/build/auxiliary") else {
        return Ok(None);
    };
    if node.is_null() {
        return Ok(None);
    }
    // `dict_compiler.cc:599-602`
    let Some(object) = node.as_object() else {
        return Err("build/auxiliary must be a map".to_string());
    };

    let groups = match object.get("algebra") {
        None => Vec::new(),
        Some(Value::Null) => Vec::new(),
        // `dict_compiler.cc:604-605`：单组简写
        Some(Value::Array(items)) => vec![AuxiliaryGroup {
            name: "auto".to_string(),
            rules: parse_rules("auto", items)?,
        }],
        // `dict_compiler.cc:606-615`：具名多组
        Some(Value::Object(named)) => {
            let mut groups = Vec::with_capacity(named.len());
            for (name, value) in named {
                let Some(items) = value.as_array() else {
                    return Err(format!("build/auxiliary/algebra group '{name}' is not a list"));
                };
                groups.push(AuxiliaryGroup {
                    name: name.clone(),
                    rules: parse_rules(name, items)?,
                });
            }
            groups
        }
        Some(_) => {
            return Err("build/auxiliary/algebra must be a list or a map of lists".to_string());
        }
    };

    // `dict_compiler.cc:620-621`：没有任何组 = 不需要编译
    if groups.is_empty() {
        return Ok(None);
    }

    // `dict_compiler.cc:623-631`
    let Some(source_name) = object.get("dictionary").and_then(Value::as_str) else {
        return Err("build/auxiliary/dictionary is required for auxiliary algebra".to_string());
    };
    if source_name.is_empty() {
        return Err("build/auxiliary/dictionary is required for auxiliary algebra".to_string());
    }
    let dictionary = if source_name.to_ascii_lowercase().ends_with(".yaml") {
        source_name.to_string()
    } else {
        format!("{source_name}.yaml")
    };

    Ok(Some(AuxiliaryConfig { dictionary, groups }))
}

fn parse_rules(group: &str, items: &[Value]) -> Result<Vec<String>, String> {
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("build/auxiliary/algebra group '{group}' contains a non-string rule"))
        })
        .collect()
}

/// 辅助码源数据：`字 → {码}`。
pub type AuxiliarySources = BTreeMap<String, BTreeSet<String>>;

/// 读一份辅助码数据表（对应 `ReadAuxiliarySources` + `LoadDictHeader(in, true)`）。
///
/// 头部缺失或 `columns` 里找不到 `text`/`code` 时返回空表（C++ 是 `continue` 跳过）。
/// 注意：C++ `LoadDictHeader` 失败也会整体 `continue`，所以坏头 = 空表，不是错误。
pub fn read_auxiliary_sources(text: &str) -> AuxiliarySources {
    // 首行判定：`---` 或 `name:` 才算有头（`dict_settings.cc:22-41`）。
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'));

    let (text_col, code_col, data) = match first {
        Some(line) if line == "---" || line.starts_with("name:") => {
            let Some((settings, rest)) = DictSettings::load_dict_header(text) else {
                return AuxiliarySources::new();
            };
            (
                settings.get_column_index("text"),
                settings.get_column_index("code"),
                rest,
            )
        }
        _ => (0, 1, text),
    };
    // `dict_compiler.cc:521-524`
    if text_col < 0 || code_col < 0 {
        return AuxiliarySources::new();
    }
    let text_col = text_col as usize;
    let code_col = code_col as usize;

    let mut sources = AuxiliarySources::new();
    for line in data.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let row: Vec<&str> = line.split('\t').collect();
        if row.len() <= text_col.max(code_col) {
            continue;
        }
        let word = row[text_col];
        let code = row[code_col];
        if word.is_empty() || code.is_empty() {
            continue;
        }
        // `utf8::next` 后必须到行尾 = 恰好一个码点
        if word.chars().count() != 1 {
            continue;
        }
        if !is_ascii_prism_key(code) {
            continue;
        }
        sources.entry(word.to_string()).or_default().insert(code.to_string());
    }
    sources
}

/// `IsAsciiPrismKey`（`dict_compiler.cc:503-510`）
fn is_ascii_prism_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'/')
}

/// `BuildAuxiliaryDatabase` 的前半：把源数据按规则投影成两张表。
///
/// 返回 `(字 → 码记录, 码 → 字记录)`，两张表都用 [`BTreeMap`]（= C++ `std::map`）。
pub fn build_entries(
    sources: &AuxiliarySources,
    rules: &[String],
) -> Result<
    (
        BTreeMap<String, Vec<AuxiliaryCode>>,
        BTreeMap<String, Vec<AuxiliaryCharacter>>,
    ),
    String,
> {
    let mut projection = Projection::from_rules(rules)?;
    // `(字, 码) → (type, credibility)`：同键时取"更精确"的那条（type 小优先，
    // 同 type 取 credibility 大者），对应 `dict_compiler.cc:567-572`。
    let mut entries: BTreeMap<(String, String), (SpellingType, f64)> = BTreeMap::new();

    for (word, codes) in sources {
        for raw_code in codes {
            let mut projected = Script::new();
            projected.add_syllable(raw_code);
            projection.apply_script(&mut projected);
            for (derived, spellings) in projected.iter() {
                // `dict_compiler.cc:561-562`
                if !is_ascii_prism_key(derived) {
                    continue;
                }
                for spelling in spellings {
                    if !spelling.properties.is_auxiliary() {
                        continue;
                    }
                    let candidate = (spelling.properties.r#type, spelling.properties.credibility);
                    let key = (word.clone(), derived.clone());
                    let replace = match entries.get(&key) {
                        None => true,
                        Some(current) => {
                            candidate.0 < current.0
                                || (candidate.0 == current.0 && candidate.1 > current.1)
                        }
                    };
                    if replace {
                        entries.insert(key, candidate);
                    }
                }
            }
        }
    }

    // `dict_compiler.cc:578-586`：按 (字, 码) 字典序拆成两张表
    let mut by_character: BTreeMap<String, Vec<AuxiliaryCode>> = BTreeMap::new();
    let mut by_code: BTreeMap<String, Vec<AuxiliaryCharacter>> = BTreeMap::new();
    for ((word, code), (spelling_type, credibility)) in entries {
        by_character
            .entry(word.clone())
            .or_default()
            .push(AuxiliaryCode {
                code: code.clone(),
                spelling_type,
                credibility,
            });
        by_code
            .entry(code)
            .or_default()
            .push(AuxiliaryCharacter {
                text: word,
                spelling_type,
                credibility,
            });
    }
    Ok((by_character, by_code))
}

/// `DictCompiler::CompileAuxiliary` 的完整流程。
///
/// * `config`：编译后的方案树（`staging/<id>/<id>.schema.yaml` 读出来的 `Value`）；
/// * `force`：`true` 时忽略 checksum 一律重编（部署的 verbose 语义）。
pub fn compile_auxiliary(
    config: &Value,
    user_data_dir: &Path,
    shared_data_dir: &Path,
    staging_dir: &Path,
    force: bool,
) -> Result<Vec<AuxGroupResult>, String> {
    let Some(auxiliary) = parse_auxiliary_config(config)? else {
        return Ok(Vec::new());
    };

    // `dict_compiler.cc:632-638`：source_file resolver（root = 用户目录，回退共享目录）
    let source_path = resolve_source(
        &auxiliary.dictionary,
        user_data_dir,
        shared_data_dir,
    )
    .ok_or_else(|| format!("auxiliary source '{}' does not exist", auxiliary.dictionary))?;
    let source_checksum = file_checksum(&source_path);

    // C++ 是懒加载：所有组都命中 checksum 时一次都不读源文件。
    let mut sources: Option<AuxiliarySources> = None;
    let mut results = Vec::with_capacity(auxiliary.groups.len());

    for group in &auxiliary.groups {
        // `dict_compiler.cc:646-656`
        let mut computer = ChecksumComputer::new(source_checksum);
        for rule in &group.rules {
            computer.process_bytes(rule.as_bytes());
            computer.process_bytes(&[0]);
        }
        let group_checksum = computer.checksum();

        let target = staging_dir.join(format!("{}.aux.bin", group.name));
        if !force && AuxiliaryDb::exists(&target) {
            if let Ok(db) = AuxiliaryDb::open(&target) {
                if db.checksum() == group_checksum {
                    results.push(AuxGroupResult {
                        name: group.name.clone(),
                        path: target,
                        recompiled: false,
                        entries: db.num_character_entries(),
                    });
                    continue;
                }
            }
        }

        if sources.is_none() {
            let text = std::fs::read_to_string(&source_path)
                .map_err(|error| format!("failed to read '{}': {error}", source_path.display()))?;
            sources = Some(read_auxiliary_sources(&text));
        }
        let sources = sources.as_ref().expect("源数据已加载");

        let (by_character, by_code) = build_entries(sources, &group.rules)?;
        let entries = by_character.len();
        AuxiliaryDb::compile(&target, &by_character, &by_code, group_checksum)
            .map_err(|error| format!("failed to compile auxiliary group '{}': {error}", group.name))?;
        results.push(AuxGroupResult {
            name: group.name.clone(),
            path: target,
            recompiled: true,
            entries,
        });
    }
    Ok(results)
}

fn resolve_source(name: &str, user_data_dir: &Path, shared_data_dir: &Path) -> Option<PathBuf> {
    let user = user_data_dir.join(name);
    if user.exists() {
        return Some(user);
    }
    let shared = shared_data_dir.join(name);
    if shared.exists() {
        return Some(shared);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 万象 `build.auxiliary.algebra.zhijie` 的三条规则。
    fn zhijie_rules() -> Vec<String> {
        vec![
            "aux|^([a-z])([a-z])$|$1|".to_string(),
            "aux|^([a-z])([a-z])$|$1$2|abbrev".to_string(),
            "aux|^([a-z])([a-z])$|$1$2/|".to_string(),
        ]
    }

    #[test]
    fn parses_named_groups_and_single_group() {
        let config = json!({
            "build": {
                "auxiliary": {
                    "dictionary": "aux",
                    "algebra": {
                        "zhijie": zhijie_rules(),
                        "jianjie": ["aux|^([a-z])([a-z])$|$1|"]
                    }
                }
            }
        });
        let parsed = parse_auxiliary_config(&config).expect("解析").expect("有配置");
        assert_eq!(parsed.dictionary, "aux.yaml");
        assert_eq!(parsed.groups.len(), 2);
        assert!(parsed.groups.iter().any(|group| group.name == "zhijie"));

        let single = json!({
            "build": { "auxiliary": { "dictionary": "aux.yaml", "algebra": ["aux|a|b|"] } }
        });
        let parsed = parse_auxiliary_config(&single).expect("解析").expect("有配置");
        assert_eq!(parsed.groups[0].name, "auto");
        assert_eq!(parsed.dictionary, "aux.yaml");
    }

    #[test]
    fn missing_or_empty_config_is_not_an_error() {
        assert_eq!(parse_auxiliary_config(&json!({})).unwrap(), None);
        assert_eq!(
            parse_auxiliary_config(&json!({"build": {"auxiliary": {"dictionary": "aux"}}})).unwrap(),
            None
        );
        assert!(parse_auxiliary_config(&json!({"build": {"auxiliary": []}})).is_err());
    }

    #[test]
    fn reads_headerless_source_and_skips_bad_rows() {
        let text = "中\tbs\n国\tg\n多字\tab\n中\tx1!\n# 注释\n\n";
        let sources = read_auxiliary_sources(text);
        assert_eq!(sources["中"].len(), 1);
        assert!(sources["中"].contains("bs"));
        assert!(sources["国"].contains("g"));
        assert!(!sources.contains_key("多字"), "只收单字");
        assert!(!sources.contains_key("x1!"), "码必须是 ASCII 字母数字/斜杠");
    }

    #[test]
    fn reads_headed_source_with_columns() {
        let text = "---\nname: aux\nversion: \"1.0\"\ncolumns:\n  - code\n  - text\n...\nbs\t中\n";
        let sources = read_auxiliary_sources(text);
        assert!(sources["中"].contains("bs"));
    }

    #[test]
    fn projects_aux_rules_into_two_tables() {
        let text = "中\tbs\n";
        let sources = read_auxiliary_sources(text);
        let (by_character, by_code) = build_entries(&sources, &zhijie_rules()).expect("投影");

        // `bs` 原样被声明为 aux（第二条规则），`b` 是第一位，`bs/` 是补斜杠
        let codes: Vec<&str> = by_character["中"]
            .iter()
            .map(|record| record.code.as_str())
            .collect();
        assert_eq!(codes, vec!["b", "bs", "bs/"]);

        // `bs` 同时被 `$1$2|abbrev` 标成简拼，但 `$1$2|` 是 Normal；
        // 去重规则取 type 小者 → Normal 胜出。
        let bs = by_code["bs"].first().expect("有记录");
        assert_eq!(bs.spelling_type, SpellingType::Normal);
        assert_eq!(bs.credibility, 0.0);

        // `b` 只来自第一条规则，是 Normal
        assert_eq!(by_code["b"][0].spelling_type, SpellingType::Normal);
        // `bs/` 来自第三条规则，也是 Normal
        assert_eq!(by_code["bs/"][0].spelling_type, SpellingType::Normal);
    }

    /// `abbrev` 标记落在**新键**上时生效：`bs` → `b`。
    #[test]
    fn abbreviation_tag_applies_to_new_key() {
        let sources = read_auxiliary_sources("中\tbs\n");
        let rules = vec!["aux|^([a-z])([a-z])$|$1|abbrev".to_string()];
        let (_, by_code) = build_entries(&sources, &rules).expect("投影");
        assert_eq!(by_code["b"][0].spelling_type, SpellingType::Abbreviation);
        assert!(by_code["b"][0].credibility < 0.0);
    }

    /// 替换串与原串相同时（`$1$2`），派生键和保留的原串**同串**，走
    /// `SpellingProperties::Update`：类型取小 → Normal 胜出、可信度取大 → 0.0。
    /// 这是 evo/上游的既有语义（`algebra.cc:109-142` + `dict_compiler.cc:547-590`），
    /// 不是 bug —— `abbrev` 只有在缩短/改写成新键时才看得出效果。
    #[test]
    fn unchanged_replacement_falls_back_to_normal() {
        let sources = read_auxiliary_sources("中\tbs\n");
        let rules = vec!["aux|^([a-z])([a-z])$|$1$2|abbrev".to_string()];
        let (_, by_code) = build_entries(&sources, &rules).expect("投影");
        assert_eq!(by_code["bs"][0].spelling_type, SpellingType::Normal);
        assert_eq!(by_code["bs"][0].credibility, 0.0);
    }
}
