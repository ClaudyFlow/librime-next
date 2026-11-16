//! `src/rime/dict/dict_compiler.{h,cc}`（66 + 426 行）的 Rust 移植：
//! 词典编译装配层 —— 把 `dict_settings` / `entry_collector` / `vocabulary` /
//! `table` / `prism` / `reverse_lookup_dictionary` 串成 C++ 的 `Compile` 主链。
//!
//! # Compile 主链（`dict_compiler.cc:122-252`，逐步对照）
//!
//! ```text
//! 1. classified_dict_source(dict_name)        找 <id>.dict.yaml（用户→共享，零扁平回退）
//! 2. LoadDictHeader + GetTables               import_tables 逐个解析成 dict_files
//! 3. compute_dict_file_checksum               源文件 CRC（含预置词库文件）
//! 4. Checksum(schema_file)                    schema 文件 CRC
//! 5. 存在性 + 校验和比对                      决定 rebuild_table（+ reverse_db）
//!    （reverse_db 过期也逼重建表）
//! 6. rebuild_table → EntryCollector → BuildTable(0)   （主表 .table.bin + .reverse.bin）
//! 7. packs（v6）→ 合并 `<dict>.packs.table.bin`  （pack 用自己的**局部**音节表）
//! 8. BuildPrism                               （.prism.bin，吃 speller/algebra）
//!    - 有 packs：v6，音节表 = 主表 ∪ pack（全局 id），校验和只算码集
//!    - 无 packs：v5，沿用主表 dict 校验和（避免现网 prism 全量重编）
//! ```
//!
//! # 与 C++ 的接口差异
//!
//! * C++ `DictCompiler(Dictionary*)`：prism/table 对象由 `DictionaryComponent`
//!   按 deployed resolver 造好带进来（含 prebuilt 回退）。Rust 侧自己算目标路径
//!   （`relocate_target`，与 C++ 补丁同规则：系列链 = id 全部点号分段），
//!   prebuilt 回退在 [`Self::deployed_path`] 里做。
//! * `ReverseLookupDictionaryComponent`（DbPool）未移植，只到 `ReverseDb`。
//! * `kDump`（导出 .txt 中转文件）未实现：Rust 侧调试用测试直接查结构。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::algo::algebra::{Projection, Script};
use crate::algo::utilities::{self, ChecksumComputer};
use crate::gear::dict::dict_settings::DictSettings;
use crate::gear::dict::entry_collector::{EntryCollector, RawDictEntry};
use crate::gear::dict::preset_vocabulary::resolve_dict_file_path;
use crate::gear::dict::prism::Prism;
use crate::gear::dict::reverse_lookup_dictionary::ReverseDb;
use crate::gear::dict::table::Table;
use crate::gear::dict::vocabulary::{Code, ShortDictEntry, Syllabary, Vocabulary};
use crate::lever::paths::DICT_DIR;

/// `DictCompiler::Options`（`dict_compiler.h` 的枚举）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompileOptions {
    /// `kRebuildTable`：强制重建表。
    pub rebuild_table: bool,
    /// `kRebuildPrism`：强制重建 prism。
    pub rebuild_prism: bool,
}

impl CompileOptions {
    /// `kRebuild` = kRebuildTable | kRebuildPrism（`verbose_` 时设）。
    pub fn rebuild_all() -> Self {
        Self {
            rebuild_table: true,
            rebuild_prism: true,
        }
    }
}

/// 阶段 B：`build/dictionaries.<dict>.algebra` 的解析结果。
///
/// * `algebra` 是 **列表** → 覆盖主 prism 的 `speller/algebra`（evo 的 list 形态）；
/// * `algebra` 是 **映射** → 每个分组编一份额外 prism（组名即 prism 后缀）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BuildDictionarySpec {
    /// 主 prism 的显式 algebra；`None` = 沿用 `/speller/algebra`。
    pub primary_algebra: Option<Vec<String>>,
    /// `<dict>.<group>` → 规则。空 = 只产主 prism。
    pub extra_prisms: BTreeMap<String, Vec<String>>,
}

/// 词库的 build 声明节点。
///
/// librime 惯例是 `build/dictionaries/<dict>`；Rime-Next 的新数据改成了
/// `build/dict/<dict>`。两种都认，前者优先（存在时不再看后者）。
pub fn build_dict_node<'a>(
    config: &'a serde_json::Value,
    dict_name: &str,
) -> Option<&'a serde_json::Value> {
    config
        .pointer(&format!("/build/dictionaries/{dict_name}"))
        .or_else(|| config.pointer(&format!("/build/dict/{dict_name}")))
}

/// 顶层「输入类型」声明节点：**两键都认**，优先带 `options` 的那个，其次复数
/// `input_types`，最后单数 `input_type`。
///
/// 2026-11-13：方案数据（源仓 `Rime-Next/schema/pinyin/pinyin.schema.yaml` 与
/// 部署产物 `build/pinyin/pinyin.schema.yaml`）写成单数 `input_type:`，而引擎
/// 原实现只读复数 `/input_types` —— 两边不一致时 `collect_input_type_options`
/// 返回空、拼音在 `Ctrl+`` 菜单里根本不会成为分组根（子项不出现）。
///
/// 为什么按「有没有 options」挑而不是固定键名：用户覆盖（`pinyin.custom.yaml`）
/// 可能把 `enabled` 补到另一个键名下，那种节点只有 enabled、没有 options（顺序
/// 与中文文案在 options 里），挑中它会丢文案；active 方案的真实声明总是带 options。
pub fn input_types_node(config: &serde_json::Value) -> Option<&serde_json::Value> {
    let plural = config
        .pointer("/input_types")
        .filter(|value| value.is_object());
    let singular = config
        .pointer("/input_type")
        .filter(|value| value.is_object());
    if plural.is_some_and(|node| node.get("options").is_some()) {
        return plural;
    }
    if singular.is_some_and(|node| node.get("options").is_some()) {
        return singular;
    }
    plural.or(singular)
}

/// 顶层 `input_types.enabled`：编译期组白名单。
///
/// 语义是"列了哪几个就只编译/部署哪几个组的棱镜"。缺失（或根本不是列表）时返回
/// `None`，调用方必须走原有行为（`build/.../algebra` 里声明的组全编译）。
/// 显式写成空列表时返回 `Some([])` —— 即"一个组都不编"，这是"列几个编几个"的
/// 字面语义，配错了只会少产物，不会悄悄把没声明的组编进来。
pub fn input_types_enabled(config: &serde_json::Value) -> Option<Vec<String>> {
    let items = input_types_node(config)?.get("enabled")?.as_array()?;
    Some(
        items
            .iter()
            .filter_map(|item| item.as_str())
            .map(str::to_string)
            .collect(),
    )
}

/// 顶层 `input_types.reset`：默认选中的输入类型（option 名）。
///
/// 只在方案显式写了 `input_types` 时才有意义；缺失时返回 `None`，保持原有
/// "所有输入类型开关默认关闭"的行为。
pub fn input_types_reset(config: &serde_json::Value) -> Option<String> {
    input_types_node(config)?
        .get("reset")
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

/// 解析 `build/dictionaries.<dict_name>.algebra`（阶段 B）。
///
/// 语法与 evo 对齐：`algebra` 既可以是规则列表（主 prism），也可以是
/// `组名: [规则…]` 的映射（额外 prism）。`__patch`/`__include` 在配置编译期
/// 已经展平，所以这里拿到的是最终结构。
///
/// 容错策略：
/// * 整个节点缺失 / `algebra` 缺失 → 空结果（不触发任何额外产物）；
/// * 本词库节点不是映射，或 `algebra` 既不是列表也不是映射 → `Err`（配置写坏了）；
/// * 单个分组不是"字符串列表" → 记日志并跳过该组，其余组照常；
/// * 空列表 → 视作缺省（避免把主 prism 变成"无拼写变换"）。
pub fn parse_build_dictionaries(
    config: &serde_json::Value,
    dict_name: &str,
) -> Result<BuildDictionarySpec, String> {
    let mut spec = BuildDictionarySpec::default();
    let Some(entry) = build_dict_node(config, dict_name) else {
        return Ok(spec);
    };
    if entry.is_null() {
        return Ok(spec);
    }
    let node = entry.as_object().ok_or_else(|| {
        format!("build/dictionaries/{dict_name} 必须是映射，实际是 {}", json_type_name(entry))
    })?;
    let Some(algebra) = node.get("algebra") else {
        return Ok(spec);
    };
    match algebra {
        serde_json::Value::Array(_) => {
            let rules = parse_algebra_rules(algebra).map_err(|e| {
                format!("build/dictionaries/{dict_name}/algebra 非法: {e}")
            })?;
            if !rules.is_empty() {
                spec.primary_algebra = Some(rules);
            }
        }
        serde_json::Value::Object(groups) => {
            for (group, rules_value) in groups {
                if group.is_empty()
                    || group == dict_name
                    || group.starts_with("__")
                    || group.contains('/')
                    || group.contains('\\')
                    || group.contains("..")
                {
                    log_error(&format!(
                        "build/dictionaries/{dict_name}/algebra: 非法分组名 '{group}'，已跳过"
                    ));
                    continue;
                }
                match parse_algebra_rules(rules_value) {
                    Ok(rules) if !rules.is_empty() => {
                        spec.extra_prisms
                            .insert(format!("{dict_name}.{group}"), rules);
                    }
                    Ok(_) => log_error(&format!(
                        "build/dictionaries/{dict_name}/algebra.{group}: 规则为空，已跳过"
                    )),
                    Err(e) => log_error(&format!(
                        "build/dictionaries/{dict_name}/algebra.{group}: {e}，已跳过"
                    )),
                }
            }
        }
        other => {
            return Err(format!(
                "build/dictionaries/{dict_name}/algebra 必须是列表或映射，实际是 {}",
                json_type_name(other)
            ));
        }
    }
    // 顶层 `input_types.enabled` 是**编译白名单**：只保留列出的组，未列的组连
    // `<dict>.<组>.prism.bin` 都不生成。没写 `input_types` 时完全不动，行为不变。
    if let Some(enabled) = input_types_enabled(config) {
        let declared: Vec<String> = spec
            .extra_prisms
            .keys()
            .filter_map(|qualified| {
                qualified
                    .strip_prefix(&format!("{dict_name}."))
                    .map(str::to_string)
            })
            .collect();
        for name in &enabled {
            // 主棱镜（`<dict>.prism.bin`）的名字就是词库名，它永远存在，
            // 不在 `extra_prisms` 里，不能报“没有同名组”。
            if name == dict_name {
                continue;
            }
            if !declared.iter().any(|group| group == name) {
                log_error(&format!(
                    "input_types.enabled: '{name}' 在 build 的 algebra 里没有同名组（已声明：{}），该名字不会产生棱镜",
                    declared.join(", ")
                ));
            }
        }
        spec.extra_prisms.retain(|qualified, _| {
            let group = qualified
                .strip_prefix(&format!("{dict_name}."))
                .unwrap_or(qualified);
            enabled.iter().any(|name| name == group)
        });
    }
    Ok(spec)
}

fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "list",
        serde_json::Value::Object(_) => "map",
    }
}

fn parse_algebra_rules(value: &serde_json::Value) -> Result<Vec<String>, String> {
    let items = value
        .as_array()
        .ok_or_else(|| format!("规则必须是列表，实际是 {}", json_type_name(value)))?;
    let mut rules = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let rule = item
            .as_str()
            .ok_or_else(|| format!("第 {index} 条规则不是字符串"))?;
        rules.push(rule.to_string());
    }
    Ok(rules)
}

/// 阶段 1 的 pack 单元：一个 pack 自己的源文件与字典设置。
///
/// 所有 pack 最终合成一张 `<dict>.packs.table.bin`，但每个 pack 仍**独立校验**、
/// 独立收集（各自的 `columns` / 编码器 / 预置词库），再统一按主表音节 id 编码。
struct PackSource {
    /// 在 `DictCompiler::packs` 里的下标（清理旧的一包一表用）。
    index: usize,
    name: String,
    settings: DictSettings,
    dict_files: Vec<PathBuf>,
}

/// 词典编译器（`DictCompiler`）。
pub struct DictCompiler {
    dict_name: String,
    /// `translator/prism` 指定的 prism 名（缺省 = dict_name）。
    prism_name: String,
    /// `translator/packs`：附加表名（`import_tables` 语义在 dict 设置里）。
    packs: Vec<String>,
    user_data_dir: PathBuf,
    shared_data_dir: PathBuf,
    staging_dir: PathBuf,
    prebuilt_data_dir: PathBuf,
    options: CompileOptions,
    /// 额外 prism（阶段 A 的编程入口；阶段 B 由 `build/dictionaries.<name>.algebra` 填充）：
    /// `prism 名 → spelling algebra 规则`。空 = 只产主 prism，产物与升级前一致。
    extra_prisms: BTreeMap<String, Vec<String>>,
    /// 主 prism 的显式 algebra（`build/dictionaries.<dict>.algebra` 的列表形态）。
    /// `None` = 沿用 `/speller/algebra`，产物与升级前逐位一致。
    primary_algebra: Option<Vec<String>>,
}

impl DictCompiler {
    pub fn new(
        dict_name: impl Into<String>,
        prism_name: impl Into<String>,
        packs: Vec<String>,
        user_data_dir: impl Into<PathBuf>,
        shared_data_dir: impl Into<PathBuf>,
        staging_dir: impl Into<PathBuf>,
        prebuilt_data_dir: impl Into<PathBuf>,
    ) -> Self {
        let dict_name = dict_name.into();
        let prism_name = prism_name.into();
        let prism_name = if prism_name.is_empty() {
            dict_name.clone()
        } else {
            prism_name
        };
        Self {
            dict_name,
            prism_name,
            packs,
            user_data_dir: user_data_dir.into(),
            shared_data_dir: shared_data_dir.into(),
            staging_dir: staging_dir.into(),
            prebuilt_data_dir: prebuilt_data_dir.into(),
            options: CompileOptions::default(),
            extra_prisms: BTreeMap::new(),
            primary_algebra: None,
        }
    }

    pub fn set_options(&mut self, options: CompileOptions) {
        self.options = options;
    }

    /// 注册额外的 prism（同一份表、同一 union 音节表，只换拼写 algebra）。
    ///
    /// 产物落在 `<dict>.<prism 名>.prism.bin`；主 prism 仍是 `<prism_name>.prism.bin`，
    /// 不受影响。阶段 B 会用 `build/dictionaries.<name>.algebra` 调这里。
    pub fn set_extra_prisms(&mut self, prisms: BTreeMap<String, Vec<String>>) {
        self.extra_prisms = prisms;
    }

    /// 显式覆盖主 prism 的 algebra（`build/dictionaries.<dict>.algebra` 列表形态）。
    ///
    /// 只对"主 prism 名 == 词库名"的方案生效：名字不同说明该 prism 可能被多个
    /// 方案共用，覆盖会改变别人的产物。
    pub fn set_primary_algebra(&mut self, rules: Option<Vec<String>>) {
        self.primary_algebra = rules.filter(|rules| !rules.is_empty());
    }

    pub fn dict_name(&self) -> &str {
        &self.dict_name
    }

    /// `DictCompiler::Compile(schema_file)`：主编译入口。
    ///
    /// `schema_file` = **编译后**的 schema（`staging/<系列…>/<id>.schema.yaml`，
    /// C++ 由 `SchemaUpdate` 用 deployed resolver 解出来传进来）；
    /// `speller/algebra` 和 schema 文件 CRC 都从它读。无词典源文件时复用已有
    /// `.table.bin`（C++ 的 `build_table_from_source = false` 分支）。
    pub fn compile(&mut self, schema_file: Option<&Path>) -> Result<(), String> {
        log_info(&format!("compiling dictionary for {schema_file:?}"));
        // 1-2. 词典源 + import_tables
        let mut build_table_from_source = true;
        let mut settings = DictSettings::default();
        let dict_file = classified_dict_source(&self.user_data_dir, &self.shared_data_dir, &self.dict_name);
        if let Some(dict_file) = &dict_file {
            let (loaded, _rest) = DictSettings::load_dict_header_from_file(dict_file)
                .map_err(|e| format!("read dict source: {e}"))?
                .ok_or_else(|| format!("failed to load settings from '{}'.", dict_file.display()))?;
            settings = loaded;
        } else {
            log_error(&format!(
                "source file '{}' does not exist.",
                self.dict_name
            ));
            build_table_from_source = false;
        }
        let mut dict_files = Vec::new();
        for table in expand_import_tables(
            &self.user_data_dir,
            &self.shared_data_dir,
            dict_file.as_deref(),
            &settings,
        )? {
            // C++ `get_dict_files_from_settings`：import_tables 里任何一个源文件
            // 缺失都直接失败（返回 false → Compile 中止），不看 build_table_from_source。
            let name = if table.is_empty() { self.dict_name.clone() } else { table };
            let Some(file) = classified_dict_source(&self.user_data_dir, &self.shared_data_dir, &name)
            else {
                return Err(format!("source file '{name}.dict.yaml' does not exist."));
            };
            dict_files.push(file);
        }
        // 3. 源文件 CRC
        let mut dict_file_checksum =
            compute_dict_file_checksum(0, &dict_files, &settings, &self.vocab_roots());
        // 4. schema 文件 CRC
        let schema_file_checksum = match schema_file {
            Some(path) if !path.as_os_str().is_empty() => utilities::checksum(path),
            _ => 0,
        };
        // 5. 重建决策
        let table_path = self.table_target(0);
        let mut rebuild_table = false;
        if let Some(table) = open_table_quiet(&table_path) {
            if build_table_from_source {
                rebuild_table = table.dict_file_checksum() != dict_file_checksum;
            } else {
                dict_file_checksum = table.dict_file_checksum();
                log_info(&format!("reuse existing table: {}", table_path.display()));
            }
        } else if build_table_from_source {
            rebuild_table = true;
        } else {
            return Err(format!(
                "neither {}.dict.yaml nor {}.table.bin exists.",
                self.dict_name, self.dict_name
            ));
        }
        let prism_path = self.prism_target();
        // reverse db 不在/过期 → 连表一起重建（C++:172-181）
        let reverse_path = self.reverse_target();
        let reverse_current = ReverseDb::open(&reverse_path)
            .ok()
            .and_then(|mut db| db.load().then_some(db))
            .is_some_and(|db| db.dict_file_checksum() == dict_file_checksum);
        if !reverse_current {
            rebuild_table = true;
        }
        if build_table_from_source && self.options.rebuild_table {
            rebuild_table = true;
        }
        log_info(&format!(
            "{}[{} file(s)] ({dict_file_checksum}); schema ({schema_file_checksum})",
            dict_file
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            dict_files.len()
        ));
        // 6. 主表
        let mut syllabary: Option<Syllabary> = None;
        if rebuild_table {
            let mut collector = EntryCollector::new();
            collector.set_data_roots(self.source_roots());
            syllabary = Some(self.build_table(0, &mut collector, &settings, &dict_files, dict_file_checksum)?);
        } else {
            syllabary = open_table_quiet(&table_path).and_then(|table| {
                let syllabary = table.syllabary();
                if syllabary.is_some() {
                    log_info(&format!("loaded syllabary from {}", table_path.display()));
                }
                syllabary
            });
        }
        // 7. packs（v6）：pack 表用自己的**局部**音节表，所以必须先编 pack 表，
        //    才能拿到"主表 ∪ pack"的 union 音节表去建 prism。
        let mut pack_syllabary: Option<Syllabary> = None;
        if !self.packs.is_empty() {
            let pack_sources = self.prepare_packs();
            if pack_sources.is_empty() {
                // 所有 pack 源都不在：不动旧产物（含 prism），加载侧仍可回退。
                log_error("no pack sources available; keeping existing pack tables and prism");
                return Ok(());
            }
            let packs_checksum = compute_packs_checksum(&pack_sources, &self.vocab_roots());
            let target_path = self.packs_target();
            // 指纹只认 pack 源（不再以主表 checksum 起算），因此主表改词/改权不会连带
            // 重编 pack 表。pack 表内部是**自己的**音节 id，与主表音节表变化无关。
            let reusable = open_table_quiet(&target_path)
                .is_some_and(|table| table.dict_file_checksum() == packs_checksum);
            let merged_ready = if reusable {
                log_info(&format!("merged packs table is up-to-date: {}", target_path.display()));
                true
            } else {
                match self.build_packs_table(&pack_sources, packs_checksum) {
                    Ok(()) => true,
                    Err(e) => {
                        log_error(&format!("failed to build merged packs table: {e}"));
                        false
                    }
                }
            };
            if merged_ready {
                pack_syllabary = open_table_quiet(&target_path)
                    .and_then(|table| table.syllabary())
                    .inspect(|_| {
                        log_info(&format!("loaded pack syllabary from {}", target_path.display()))
                    });
                if pack_syllabary.is_none() {
                    log_error("cannot read the syllabary from the merged packs table");
                    return Ok(());
                }
                // 旧的一包一表已经（或已被上次部署）并入合并表，留着只会让排障时
                // 误以为加载侧还在读它们 —— 部署链不会自动清这些孤儿产物。
                self.cleanup_pack_tables(&pack_sources);
            } else {
                log_error("cannot build prism without the merged packs table");
                return Ok(());
            }
        }
        // 8. prism：v6 只在含 packs 的方案写；不带 packs 的继续 v5 + 旧 dict 校验和，
        //    否则现网 30 个 prism 会在首次部署时全量重编。
        let has_packs = !self.packs.is_empty() && pack_syllabary.is_some();
        let mut union_syllabary = syllabary.clone().unwrap_or_default();
        if let Some(pack_syllabary) = pack_syllabary {
            for syllable in pack_syllabary.iter() {
                union_syllabary.insert(syllable.clone());
            }
        }
        let prism_checksum = if has_packs {
            // 只算码集：词条文本/权重变动不重编 prism。
            compute_syllabary_checksum(&union_syllabary, has_packs)
        } else {
            dict_file_checksum
        };
        // 只有主 prism 名 == 词库名时才允许用 build/dictionaries 的列表覆盖，
        // 避免改到被其他方案共用的 prism。
        let primary_rules = self
            .primary_algebra
            .as_deref()
            .filter(|_| self.prism_name == self.dict_name);
        let primary_schema_checksum = match primary_rules {
            Some(rules) => schema_file_checksum ^ prism_rules_checksum(rules),
            None => schema_file_checksum,
        };
        let rebuild_prism = self.options.rebuild_prism
            || match open_prism_quiet(&prism_path) {
                Some(prism) => {
                    prism.dict_file_checksum() != prism_checksum
                        || prism.schema_file_checksum() != primary_schema_checksum
                        // 带 packs 的必须是 v6；不带 packs 的必须退回 v5（旧产物可能是 v6）。
                        || prism.has_packs() != has_packs
                }
                None => true,
            };
        if rebuild_prism {
            self.build_prism(
                schema_file,
                primary_rules,
                &union_syllabary,
                prism_checksum,
                primary_schema_checksum,
                has_packs,
            )?;
        }
        // 8b. 额外 prism（阶段 A；阶段 B 由 `build/dictionaries.<name>.algebra` 填充）。
        //     与主 prism 共用同一 union 音节表 → 同一套全局音节 id，只是键集不同。
        for (name, rules) in &self.extra_prisms {
            let target_path = self.extra_prism_target(name);
            // algebra 规则参与校验和：改分组规则必须重编这一份 prism。
            let extra_schema_checksum = schema_file_checksum ^ prism_rules_checksum(rules);
            let rebuild = self.options.rebuild_prism
                || match open_prism_quiet(&target_path) {
                    Some(prism) => {
                        prism.dict_file_checksum() != prism_checksum
                            || prism.schema_file_checksum() != extra_schema_checksum
                            || prism.has_packs() != has_packs
                    }
                    None => true,
                };
            if rebuild {
                self.build_prism_with_rules(
                    &target_path,
                    &union_syllabary,
                    rules,
                    prism_checksum,
                    extra_schema_checksum,
                    has_packs,
                )?;
            }
        }
        Ok(())
    }

    /// `DictCompiler::BuildTable`（`dict_compiler.cc:273-335`）。
    ///
    /// 返回收集到的音节表（C++ 由调用方 `std::move(collector.syllabary)`）。
    fn build_table(
        &self,
        table_index: usize,
        collector: &mut EntryCollector,
        settings: &DictSettings,
        dict_files: &[PathBuf],
        dict_file_checksum: u32,
    ) -> Result<Syllabary, String> {
        let target_path = self.table_target(table_index);
        log_info(&format!("building table: {}", target_path.display()));
        collector.configure(settings);
        collector.collect(dict_files);
        let syllabary = collector.take_syllabary();
        let entries = collector.take_entries();
        let num_entries = entries.len();
        let vocabulary = vocabulary_from_entries(
            &entries,
            &syllabary,
            settings.sort_order() != "original",
        );
        Table::compile(&target_path, &syllabary, &vocabulary, num_entries, dict_file_checksum)
            .map_err(|e| format!("build table {}: {e}", target_path.display()))?;
        // 主表顺带建 reverse db（C++:329-333）
        if table_index == 0 {
            let stems = collector.take_stems();
            let reverse_path = self.reverse_target();
            ReverseDb::compile(&reverse_path, Some(settings), &syllabary, &vocabulary, &stems, dict_file_checksum)
                .map_err(|e| format!("build reverse db {}: {e}", reverse_path.display()))?;
        }
        Ok(syllabary)
    }

    /// 合并 pack 表：所有 pack 仍**各自**收集（各自的 `columns` / 编码器 / 预置词库
    /// 一次 `configure`），词条与音节表汇总后按**pack 自己的局部音节 id** 编码进一张表。
    ///
    /// v6：prism 的全局 id 由 union 音节表决定，pack 表只认自己的局部 id，
    /// 加载时由 `SyllableIdMap` 做局部 ↔ 全局换算。因此 pack 独有新音节不再被丢弃
    /// （阶段 1 用主表固定音节表，这类码会被 `vocabulary_from_entries` 丢掉）。
    fn build_packs_table(
        &self,
        packs: &[PackSource],
        packs_checksum: u32,
    ) -> Result<(), String> {
        let target_path = self.packs_target();
        log_info(&format!(
            "building merged packs table ({} pack(s)): {}",
            packs.len(),
            target_path.display()
        ));
        let mut entries: Vec<RawDictEntry> = Vec::new();
        let mut syllabary = Syllabary::new();
        let mut sort_homophones = false;
        for pack in packs {
            let mut collector = EntryCollector::new();
            collector.set_data_roots(self.source_roots());
            collector.configure(&pack.settings);
            collector.collect(&pack.dict_files);
            let pack_syllables = collector.take_syllabary();
            for syllable in pack_syllables.iter() {
                syllabary.insert(syllable.clone());
            }
            entries.extend(collector.take_entries());
            sort_homophones |= pack.settings.sort_order() != "original";
        }
        let vocabulary = vocabulary_from_entries(&entries, &syllabary, sort_homophones);
        Table::compile(
            &target_path,
            &syllabary,
            &vocabulary,
            entries.len(),
            packs_checksum,
        )
        .map_err(|e| format!("build packs table {}: {e}", target_path.display()))?;
        Ok(())
    }

    /// 把 `translator/packs` 逐个解析成可编译单元；源缺失的 pack 跳过并记日志。
    ///
    /// 旧行为「源不在就复用现成的一包一表」在阶段 1 不再成立：合并表建立后旧的
    /// `<pack>.table.bin` 会被清掉，所以这里只认源文件。若**全部** pack 都缺源，
    /// 调用方不会动旧产物，加载侧仍能回退。
    fn prepare_packs(&self) -> Vec<PackSource> {
        let mut prepared = Vec::new();
        for (index, pack_name) in self.packs.iter().enumerate() {
            let Some(pack_file) =
                classified_dict_source(&self.user_data_dir, &self.shared_data_dir, pack_name)
            else {
                log_info(&format!(
                    "pack source file '{pack_name}' does not exist; skipped"
                ));
                continue;
            };
            let pack_settings = match DictSettings::load_dict_header_from_file(&pack_file) {
                Ok(Some((settings, _))) => settings,
                Ok(None) => {
                    log_error(&format!(
                        "failed to load settings from '{}'.",
                        pack_file.display()
                    ));
                    continue;
                }
                Err(e) => {
                    log_error(&format!("read pack source '{}': {e}", pack_file.display()));
                    continue;
                }
            };
            let mut pack_files = Vec::new();
            let mut missing = None;
            let tables = match expand_import_tables(
                &self.user_data_dir,
                &self.shared_data_dir,
                Some(&pack_file),
                &pack_settings,
            ) {
                Ok(tables) => tables,
                Err(e) => {
                    log_error(&format!("pack '{pack_name}': {e}"));
                    continue;
                }
            };
            for table in tables {
                let name = if table.is_empty() { pack_name.clone() } else { table };
                match classified_dict_source(&self.user_data_dir, &self.shared_data_dir, &name) {
                    Some(file) => pack_files.push(file),
                    None => {
                        missing = Some(name);
                        break;
                    }
                }
            }
            if let Some(name) = missing {
                log_error(&format!("source file '{name}.dict.yaml' does not exist."));
                continue;
            }
            prepared.push(PackSource {
                index,
                name: pack_name.clone(),
                settings: pack_settings,
                dict_files: pack_files,
            });
        }
        prepared
    }

    /// 清掉被合并表取代的旧 `<pack>.table.bin`（部署链不会自动清这些孤儿产物）。
    ///
    /// 只清**本次参与合并**的 pack：源缺失、没进合并表的 pack 保留旧产物，
    /// 让加载侧还能回退。
    fn cleanup_pack_tables(&self, packs: &[PackSource]) {
        for pack in packs {
            let old = self.table_target(pack.index + 1);
            if !old.is_file() {
                continue;
            }
            match std::fs::remove_file(&old) {
                Ok(()) => log_info(&format!("removed obsolete pack table: {}", old.display())),
                Err(e) => log_error(&format!(
                    "failed to remove obsolete pack table {}: {e}",
                    old.display()
                )),
            }
        }
    }

    /// `DictCompiler::BuildPrism`（`dict_compiler.cc:353-424`）。
    ///
    /// `syllabary` 是 **union** 音节表（无 packs 时就是主表音节表），下标即全局音节 id；
    /// `has_packs == true` 时写 v6（metadata 追加音节表 + `has_packs`），否则写 v5。
    fn build_prism(
        &self,
        schema_file: Option<&Path>,
        rules_override: Option<&[String]>,
        syllabary: &Syllabary,
        dict_file_checksum: u32,
        schema_file_checksum: u32,
        has_packs: bool,
    ) -> Result<(), String> {
        log_info("building prism...");
        let target_path = self.prism_target();
        let mut rules: Vec<String> = rules_override.map(<[String]>::to_vec).unwrap_or_default();
        if rules.is_empty() {
            if let Some(schema_file) = schema_file.filter(|p| !p.as_os_str().is_empty()) {
                let value = crate::load_file(schema_file).map_err(|e| {
                    format!("error loading prism definition from {schema_file:?}: {e}")
                })?;
                rules = value
                    .pointer("/speller/algebra")
                    .and_then(|list| list.as_array())
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
            }
        }
        self.build_prism_with_rules(
            &target_path,
            syllabary,
            &rules,
            dict_file_checksum,
            schema_file_checksum,
            has_packs,
        )
    }

    /// 用一份 **显式** algebra 规则建 prism（主 prism 与额外 prism 共用这条路径）。
    ///
    /// `rules` 为空 = 不做拼写变换（键就是音节本身）。
    fn build_prism_with_rules(
        &self,
        target_path: &Path,
        syllabary: &Syllabary,
        rules: &[String],
        dict_file_checksum: u32,
        schema_file_checksum: u32,
        has_packs: bool,
    ) -> Result<(), String> {
        if syllabary.is_empty() {
            return Err(format!(
                "cannot get syllabary from primary table {}",
                self.table_target(0).display()
            ));
        }
        // speller/algebra → Script（C++:367-407）
        let mut script = Script::new();
        if !rules.is_empty() {
            let mut projection = Projection::from_rules(rules)
                .map_err(|e| format!("error loading prism definition: {e}"))?;
            for syllable in syllabary.iter() {
                script.add_syllable(syllable);
            }
            if !projection.apply_script(&mut script) {
                script = Script::new(); // C++：script.clear()
            }
        }
        let script_ref = if script.is_empty() { None } else { Some(&script) };
        Prism::compile_with_packs(
            target_path,
            syllabary,
            script_ref,
            dict_file_checksum,
            schema_file_checksum,
            has_packs,
        )
        .map_err(|e| format!("build prism {}: {e}", target_path.display()))?;
        Ok(())
    }

    // ---------------------------------------------------------------- 路径

    /// 源数据根（用户优先、共享回退）。
    fn source_roots(&self) -> Vec<PathBuf> {
        vec![self.user_data_dir.clone(), self.shared_data_dir.clone()]
    }

    /// 预置词库的搜索根（同源数据根）。
    fn vocab_roots(&self) -> Vec<PathBuf> {
        self.source_roots()
    }

    /// `classified_dict_source`（`dict_compiler.cc:56-86`）：词典源只认分类路径。
    /// id 含 '/' 时按显式相对路径；裸 id 按点号分段进 `dict/<系列…>/`。
    fn classified_dict_source(&self, dict_name: &str) -> Option<PathBuf> {
        classified_dict_source(&self.user_data_dir, &self.shared_data_dir, dict_name)
    }

    /// 主表/附加表目标：`<staging>/<系列…>/<name>.table.bin`（relocate_target）。
    fn table_target(&self, table_index: usize) -> PathBuf {
        let name = if table_index == 0 {
            &self.dict_name
        } else {
            &self.packs[table_index - 1]
        };
        relocate_target(&self.staging_dir, &format!("{name}.table.bin"))
    }

    /// 合并 pack 表目标：与主表同目录，文件名 `<prism>.packs.table.bin`。
    ///
    /// ⚠️ 2026-11-11 修：原按 `dict_name` 命名，导致 `qiaoma`（5 packs）与
    /// `qiaoma_jf`（6 packs）共用 `dictionary: qiaoma` 时抢同一个
    /// `build/qiaoma/qiaoma.packs.table.bin`，checksum 永远对不上，每轮部署都
    /// 重编两次 10 万级词条（实测两个方案合计 ~40s，占整次部署 49s 的大头）。
    /// 改按 `prism_name` 命名后各写各的，第二次部署即 `up-to-date`。
    ///
    /// 目录仍按 `dict_name` 分段（与加载侧 `bin_resource_path` 一致），
    /// 只有文件名带 prism 区分；`prism_name == dict_name` 时产物名与旧版逐位相同。
    fn packs_target(&self) -> PathBuf {
        self.staging_dir
            .join(self.dict_name.replace('.', "/"))
            .join(format!("{}.packs.table.bin", self.prism_name))
    }

    /// prism 目标：`<staging>/<系列…>/<prism_name>.prism.bin`。
    fn prism_target(&self) -> PathBuf {
        relocate_target(&self.staging_dir, &format!("{}.prism.bin", self.prism_name))
    }

    /// 额外 prism 目标：`<staging>/<系列…>/<dict>.<name>.prism.bin`。
    fn extra_prism_target(&self, name: &str) -> PathBuf {
        // 与 `<dict>.packs.table.bin` 同目录（不要把 `demo.double` 当成系列路径）。
        // 阶段 B 的 key 是完整名 `<dict>.<group>`；阶段 A 的裸组名 `double` 仍然兼容。
        let qualified = if name.starts_with(&format!("{}.", self.dict_name)) {
            name.to_string()
        } else {
            format!("{}.{}", self.dict_name, name)
        };
        self.staging_dir
            .join(self.dict_name.replace('.', "/"))
            .join(format!("{qualified}.prism.bin"))
    }

    /// reverse db 目标：`<staging>/<系列…>/<dict_name>.reverse.bin`。
    fn reverse_target(&self) -> PathBuf {
        relocate_target(&self.staging_dir, &format!("{}.reverse.bin", self.dict_name))
    }
}

/// `classified_dict_source`（`dict_compiler.cc:56-86`，C++ 的 static 函数）：
/// 先用户目录后共享目录；找不到返回 `None`。
///
/// **词库路径放宽到"前缀"**（2026-10-28 用户定）：方案要求目录 = id 全部分段，
/// 词库不要求 —— 目录只要是 `dict/` + **id 分段的某个前缀**即可
/// （`cangjie.5.base` 可以住在 `dict/cangjie/5/`、`dict/cangjie/`，甚至 `dict/`）。
/// 理由：`cangjie.5.base` / `.stem` / `.extended` 是被 `import_tables` 吃掉的片段，
/// 为它们各建一层文件夹没有收益。所以查找顺序是**精确优先、再逐级退前缀**：
/// `dict/cangjie/5/base/…` → `dict/cangjie/5/…` → `dict/cangjie/…` → `dict/…`。
///
/// [Weasel-Next 宽容项 2026-10-26] 前缀都失手后，再扫两个数据根的
/// `dict/<一级子目录>/`（兄弟词库目录）。场景：用户目录里留着**旧版无前缀**
/// `import_tables`（`- yustar.quick`）的词典头，而带前缀写法的新版在共享目录
/// `dict/yuhao/` 下——旧副本按优先级胜出后，系列规则找不到文件，真机 deploy 因此 18/2。
/// 只扫一级目录、只认完整文件名。
pub fn classified_dict_source(
    user_data_dir: &Path,
    shared_data_dir: &Path,
    dict_name: &str,
) -> Option<PathBuf> {
    let file_name = format!("{dict_name}.dict.yaml");
    let explicit = dict_name.contains('/') || dict_name.contains('\\');
    for root in [user_data_dir, shared_data_dir] {
        if root.as_os_str().is_empty() {
            continue;
        }
        if explicit {
            let path = root.join(&file_name);
            if path.is_file() {
                return Some(path);
            }
            continue;
        }
        // 精确在前，逐级退前缀（前缀层级从深到浅）
        let segments: Vec<&str> = dict_name.split('.').filter(|s| !s.is_empty()).collect();
        for depth in (0..=segments.len()).rev() {
            let mut nested = root.join(DICT_DIR);
            for segment in &segments[..depth] {
                nested = nested.join(segment);
            }
            let candidate = nested.join(&file_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    // 宽容搜索：裸 id 在兄弟词库目录里找（见函数头注释）。
    if !explicit {
        for root in [user_data_dir, shared_data_dir] {
            let dict_dir = root.join(DICT_DIR);
            let Ok(entries) = std::fs::read_dir(&dict_dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let candidate = entry.path().join(&file_name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// `import_tables` 的通配展开（Weasel-Next 扩展，2026-11-10）。
///
/// 返回**要编译的表名列表**（身份在前，其后精确名按书写顺序，最后是各通配的展开项）。
/// 调用方仍然把每个名字交给 [`classified_dict_source`] 解析成文件 —— 展开只产出名字，
/// 不另开一条取文件的分支，因此用户目录/共享目录的优先级原样保留。
///
/// 语法边界：只认 `*`（匹配任意长度字符序列，含点号），`?` / `[]` / `{a,b}` 一律按
/// 字面字符处理。不含 `*` 的条目完全走 `GetTables()` 的老路径，行为不变。
///
/// 扫描规则：
/// * 扫描目录 = 词典源文件所在**那一层**（`dict/pinyin/`，不递归子目录：子目录是别的
///   系列，`dict/pinyin/luna/` 里的表不属于 `pinyin.*`），再带上另一个数据根的同相对
///   目录（用户目录与共享目录混放时两边都能展开，同一名字仍由
///   `classified_dict_source` 按"用户优先"落到唯一文件）；
/// * 排除身份自身（否则 `pinyin*` 会把 `pinyin.dict.yaml` 导进来，自引用递归）；
/// * 每个通配的匹配结果按名字字典序排序，且已被精确条目 / 前面的通配覆盖的名字不重复追加。
pub fn expand_import_tables(
    user_data_dir: &Path,
    shared_data_dir: &Path,
    source_file: Option<&Path>,
    settings: &DictSettings,
) -> Result<Vec<String>, String> {
    let tables = settings.get_tables();
    if tables.is_empty() {
        return Ok(tables);
    }
    let identity = settings.identity();
    let mut names: Vec<String> = Vec::with_capacity(tables.len());
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut patterns: Vec<&str> = Vec::new();
    for table in &tables {
        if table == &identity {
            if seen.insert(table.clone()) {
                names.push(table.clone());
            }
        } else if table.contains('*') {
            patterns.push(table.as_str());
        } else if seen.insert(table.clone()) {
            names.push(table.clone());
        }
    }
    if patterns.is_empty() {
        return Ok(names);
    }
    let scan_dirs = dict_scan_dirs(user_data_dir, shared_data_dir, source_file);
    let found = scan_dict_names(&scan_dirs);
    for pattern in patterns {
        let matched: Vec<&String> = found
            .iter()
            .filter(|name| name.as_str() != identity && wildcard_match(pattern, name))
            .collect();
        if matched.is_empty() {
            let dirs = scan_dirs
                .iter()
                .map(|dir| dir.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let files = found.iter().cloned().collect::<Vec<_>>().join(", ");
            return Err(format!(
                "import_tables 模式 '{pattern}' 没有匹配到任何词库（扫描目录: {dirs}；\
                 目录下找到的 .dict.yaml: {}）",
                if files.is_empty() { "（无）" } else { &files }
            ));
        }
        for name in matched {
            if seen.insert(name.clone()) {
                names.push(name.clone());
            }
        }
    }
    Ok(names)
}

/// 通配展开的扫描目录：词典源所在系列目录在前，另一个数据根的同相对目录在后。
///
/// 源文件不在任何一个已知根下（测试/非常规调用）时只扫它自己的父目录。
fn dict_scan_dirs(
    user_data_dir: &Path,
    shared_data_dir: &Path,
    source_file: Option<&Path>,
) -> Vec<PathBuf> {
    let Some(parent) = source_file.and_then(Path::parent) else {
        return Vec::new();
    };
    for (root, other) in [
        (user_data_dir, shared_data_dir),
        (shared_data_dir, user_data_dir),
    ] {
        if root.as_os_str().is_empty() {
            continue;
        }
        let Ok(relative) = parent.strip_prefix(root) else {
            continue;
        };
        let mut dirs = vec![root.join(relative)];
        if !other.as_os_str().is_empty() {
            let mirror = other.join(relative);
            if !dirs.contains(&mirror) {
                dirs.push(mirror);
            }
        }
        return dirs;
    }
    vec![parent.to_path_buf()]
}

/// 收集**目录第一层**的词库名（`<name>.dict.yaml` 去掉后缀），目录不在就跳过。
///
/// 不递归：`dict/pinyin/luna/pinyin.luna.dict.yaml` 那种子目录是**另一个系列**的
/// 系列目录，不属于 `dict/pinyin/` 这一层；递归会把旧系列表一并吞进 `pinyin.*`。
fn scan_dict_names(dirs: &[PathBuf]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                continue;
            }
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if let Some(name) = file_name.strip_suffix(".dict.yaml")
                && !name.is_empty()
            {
                names.insert(name.to_string());
            }
        }
    }
    names
}

/// `*` 通配匹配：`*` 匹配任意长度字符序列，其余字符按字面比较。
///
/// `?` / `[]` 不支持（当作普通字符）—— 词库名里这些字符合法，按 glob 解释会误伤。
fn wildcard_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_resume = 0usize;
    while ni < name.len() {
        if pi < pattern.len() && pattern[pi] == name[ni] {
            pi += 1;
            ni += 1;
        } else if pi < pattern.len() && pattern[pi] == '*' {
            star = Some(pi);
            star_resume = ni;
            pi += 1;
        } else if let Some(star_pos) = star {
            pi = star_pos + 1;
            star_resume += 1;
            ni = star_resume;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
}

/// `relocate_target`（`dict_compiler.cc:254-291`，2026-10-25 系列化修正版）：
/// 产物文件名 → `<staging>/<系列…>/<文件名>`。系列链 = id 的**全部**点号
/// （与读侧 `.bin` 分类恒等：`resource.cc:92-93`）。
pub fn relocate_target(staging_dir: &Path, file_name: &str) -> PathBuf {
    const TYPE_SUFFIXES: [&str; 3] = [".prism.bin", ".table.bin", ".reverse.bin"];
    let mut id = file_name.to_string();
    for suffix in TYPE_SUFFIXES {
        if let Some(stripped) = id.strip_suffix(suffix) {
            id = stripped.to_string();
            break;
        }
    }
    let mut nested = staging_dir.to_path_buf();
    if !id.is_empty() {
        for segment in id.split('.') {
            if !segment.is_empty() {
                nested = nested.join(segment);
            }
        }
    }
    nested.join(file_name)
}

/// `compute_dict_file_checksum`（`dict_compiler.cc:106-120`）。
fn compute_dict_file_checksum(
    initial_checksum: u32,
    dict_files: &[PathBuf],
    settings: &DictSettings,
    vocab_roots: &[PathBuf],
) -> u32 {
    if dict_files.is_empty() {
        return initial_checksum;
    }
    let mut computer = ChecksumComputer::new(initial_checksum);
    for file in dict_files {
        computer.process_file(file);
    }
    if settings.use_preset_vocabulary() {
        let vocab_file = resolve_dict_file_path(vocab_roots, &settings.vocabulary());
        computer.process_file(&vocab_file);
    }
    computer.checksum()
}

/// 合并 pack 表指纹：**只吃 pack 源**，不再以主表 checksum 起算。
///
/// 因此主表改词/改权时 pack 表不重编（收益：主表大改也只付一次主表编译成本）。
/// pack 名参与 CRC —— 否则 `[A, B]` 改成 `[A]` 时剩余内容哈希恰好相同，
/// 合并表会漏掉"少了一个 pack"这次变更。
///
/// 开头的 `packs-v2` 是**编译格式版本标记**：阶段 1 的合并表按主表音节 id 编码，
/// 指纹恰好相同会被误复用；掺一位让它们全部重编成 v6 的"pack 局部 id"。
fn compute_packs_checksum(packs: &[PackSource], vocab_roots: &[PathBuf]) -> u32 {
    let mut computer = ChecksumComputer::new(0);
    computer.process_bytes(b"packs-v2");
    for pack in packs {
        computer.process_bytes(pack.name.as_bytes());
        for file in &pack.dict_files {
            computer.process_file(file);
        }
        if pack.settings.use_preset_vocabulary() {
            let vocab_file = resolve_dict_file_path(vocab_roots, &pack.settings.vocabulary());
            computer.process_file(&vocab_file);
        }
    }
    computer.checksum()
}

/// prism v6 的失效指纹：**只算码集**（union 音节表 + `has_packs`），
/// 不掺词条文本/权重 —— 那两者只影响 `.table.bin`。
///
/// 逐条以 `字符串 + NUL` 参与 CRC（与 evo 的 `process_bytes(code.c_str(), size + 1)` 等价），
/// 顺序固定来自 `Syllabary`（`BTreeSet`）的排序，因此可重现。
fn compute_syllabary_checksum(syllabary: &Syllabary, has_packs: bool) -> u32 {
    let mut computer = ChecksumComputer::new(0);
    for syllable in syllabary.iter() {
        computer.process_bytes(syllable.as_bytes());
        computer.process_bytes(b"\0");
    }
    computer.process_bytes(if has_packs { b"1" } else { b"0" });
    computer.checksum()
}

/// 额外 prism 的 algebra 规则指纹：规则任何一条改动都要重编那一份 prism。
///
/// 主 prism 的规则来自 schema 文件（其 CRC 已经参与校验和）；额外 prism 的规则
/// 来自 `build/dictionaries.<name>.algebra`，没有文件可算，所以在这里串成稳定字节流。
fn prism_rules_checksum(rules: &[String]) -> u32 {
    let mut computer = ChecksumComputer::new(0);
    for rule in rules {
        computer.process_bytes(rule.as_bytes());
        computer.process_bytes(b"\x1f");
    }
    computer.checksum()
}

/// 词条 → `Vocabulary`（`dict_compiler.cc:291-321` 的逐条编码 + `SortHomophones`）。
///
/// 主表与合并 pack 表共用：`syllabary` 是各自表的**局部**音节表，
/// 所以 pack 独有新音节也能正常编码（v6）。
fn vocabulary_from_entries(
    entries: &[RawDictEntry],
    syllabary: &Syllabary,
    sort_homophones: bool,
) -> Vocabulary {
    let syllable_to_id = syllabary.syllable_to_id();
    let mut vocabulary = Vocabulary::new();
    for entry in entries {
        let mut code = Code::new();
        let mut complete = true;
        for syllable in entry.raw_code.iter() {
            match syllable_to_id.get(syllable.as_str()) {
                Some(&id) => code.push(id),
                None => {
                    complete = false;
                    break;
                }
            }
        }
        if !complete {
            log_error("Error locating entries in vocabulary.");
            continue;
        }
        // C++：`weight = log(r->weight > 0 ? r->weight : DBL_EPSILON)`
        let weight = if entry.weight > 0.0 {
            entry.weight.ln()
        } else {
            f64::EPSILON.ln()
        };
        if let Some(list) = vocabulary.locate_entries(&code) {
            list.0.push(ShortDictEntry::new(entry.text.clone(), code, weight));
        } else {
            log_error("Error locating entries in vocabulary.");
        }
    }
    if sort_homophones {
        vocabulary.sort_homophones();
    }
    vocabulary
}

/// `Table::Exists() && Table::Load()` 的 Rust 对应：文件不在或打不开都算
/// "不存在"（C++ 的 Exists 检查就是看文件在不在）。
fn open_table_quiet(path: &Path) -> Option<Table> {
    if path.is_file() {
        Table::open(path).ok()
    } else {
        None
    }
}

/// `Prism::Exists() && Prism::Load()` 的 Rust 对应。
fn open_prism_quiet(path: &Path) -> Option<Prism> {
    if path.is_file() {
        Prism::open(path).ok()
    } else {
        None
    }
}

fn log_info(message: &str) {
    println!("[dict_compiler] {message}");
}

fn log_error(message: &str) {
    eprintln!("[dict_compiler] {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("librime-dict-compiler-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 写一个最小词库存根（只有 YAML 头，正文随意）。
    fn write_dict_source(path: &Path, header: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, format!("{header}...\n")).unwrap();
    }

    fn settings_of(header: &str) -> DictSettings {
        // `LoadDictHeader` 要求 `name` + `version` 齐全，测试只关心 `id` / `import_tables`
        let text = format!("name: t\n{header}...\n");
        DictSettings::load_dict_header(&text).expect("dict header").0
    }

    #[test]
    fn relocate_target_uses_full_series_chain() {
        let staging = Path::new("C:/Rime-Next/build");
        // 无点号：一层
        assert_eq!(
            relocate_target(staging, "luna_pinyin.table.bin"),
            staging.join("luna_pinyin/luna_pinyin.table.bin")
        );
        // 带点号：**全部**点号分级（2026-10-25 修正：原先只取第一个点号前的 stem）
        assert_eq!(
            relocate_target(staging, "yuling.five.prism.bin"),
            staging.join("yuling/five/yuling.five.prism.bin")
        );
        assert_eq!(
            relocate_target(staging, "a.b.c.reverse.bin"),
            staging.join("a/b/c/a.b.c.reverse.bin")
        );
    }

    /// 同一词库、不同 prism、不同 packs 集合 → 合并表必须各写各的。
    /// 这是「为什么部署还是一分钟」的根因回归：qiaoma(5 packs) / qiaoma_jf(6 packs)
    /// 若共用 qiaoma.packs.table.bin，checksum 永远不匹配，每轮都重编两次十万级词条。
    #[test]
    fn packs_target_is_per_prism_to_avoid_mutual_overwrite() {
        let staging = Path::new("C:/Rime-Next/build");
        let common_packs = || {
            vec![
                "qiaoma.base".to_string(),
                "qiaoma.ext".to_string(),
                "qiaoma.user".to_string(),
                "qiaoma.associational".to_string(),
                "qiaoma_en".to_string(),
            ]
        };
        let mut base_packs = common_packs();
        let mut jf_packs = common_packs();
        jf_packs.push("qiaoma.essay".to_string());
        let base = DictCompiler::new("qiaoma", "qiaoma", base_packs, ".", ".", staging, ".");
        let jf = DictCompiler::new("qiaoma", "qiaoma_jf", jf_packs, ".", ".", staging, ".");
        let base_path = base.packs_target();
        let jf_path = jf.packs_target();
        println!(
            "qiaoma    ({} packs) -> {}",
            base.packs.len(),
            base_path.display()
        );
        println!(
            "qiaoma_jf ({} packs) -> {}",
            jf.packs.len(),
            jf_path.display()
        );
        assert_eq!(base_path, staging.join("qiaoma/qiaoma.packs.table.bin"));
        assert_eq!(jf_path, staging.join("qiaoma/qiaoma_jf.packs.table.bin"));
        assert_ne!(base_path, jf_path, "两个 prism 绝不能写同一个 packs 表");
    }

    #[test]
    fn classified_dict_source_prefers_user_and_never_falls_back_to_flat() {
        let root = temp_dir("source");
        let user = root.join("user");
        let shared = root.join("shared");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        // 都不在 → None
        assert_eq!(
            classified_dict_source(&user, &shared, "luna_pinyin"),
            None
        );
        // 扁平存在也**不认**（零扁平回退）
        std::fs::write(shared.join("luna_pinyin.dict.yaml"), "x").unwrap();
        assert_eq!(classified_dict_source(&user, &shared, "luna_pinyin"), None);
        // 分类路径才认（词库只进 `dict/`）
        std::fs::create_dir_all(shared.join("dict/luna_pinyin")).unwrap();
        std::fs::write(shared.join("dict/luna_pinyin/luna_pinyin.dict.yaml"), "x").unwrap();
        assert_eq!(
            classified_dict_source(&user, &shared, "luna_pinyin"),
            Some(shared.join("dict/luna_pinyin/luna_pinyin.dict.yaml"))
        );
        // 用户目录优先
        std::fs::create_dir_all(user.join("dict/yuling/five")).unwrap();
        std::fs::write(user.join("dict/yuling/five/yuling.five.dict.yaml"), "x").unwrap();
        assert_eq!(
            classified_dict_source(&user, &shared, "yuling.five"),
            Some(user.join("dict/yuling/five/yuling.five.dict.yaml"))
        );
        // 显式相对路径：不套系列规则
        std::fs::create_dir_all(shared.join("dict/yuhao")).unwrap();
        std::fs::write(shared.join("dict/yuhao/yuling.quick.dict.yaml"), "x").unwrap();
        assert_eq!(
            classified_dict_source(&user, &shared, "dict/yuhao/yuling.quick"),
            Some(shared.join("dict/yuhao/yuling.quick.dict.yaml"))
        );
        std::fs::remove_dir_all(&root).ok();
    }

    // ------------------------------------------------------------ import_tables 通配

    /// 不含 `*` 的条目：结果与 `GetTables()` 逐项一致（顺序也不动），不扫描目录。
    #[test]
    fn import_tables_without_wildcard_keeps_exact_behavior() {
        let root = temp_dir("import-exact");
        let user = root.join("user");
        let shared = root.join("shared");
        std::fs::create_dir_all(&user).unwrap();
        let header = "id: pinyin\nversion: \"1\"\nimport_tables:\n  - pinyin.zi\n  - pinyin.jichu\n";
        let source = shared.join("dict/pinyin/pinyin.dict.yaml");
        write_dict_source(&source, header);
        let settings = settings_of(header);
        assert_eq!(
            expand_import_tables(&user, &shared, Some(&source), &settings).unwrap(),
            vec!["pinyin", "pinyin.zi", "pinyin.jichu"]
        );
        // 精确名不存在时仍交给原导入路径报错（这里不替它判存在性）
        let missing = settings_of("id: pinyin\nversion: \"1\"\nimport_tables:\n  - pinyin.nope\n");
        assert_eq!(
            expand_import_tables(&user, &shared, Some(&source), &missing).unwrap(),
            vec!["pinyin", "pinyin.nope"]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// `pinyin.*` → 目录（含系列子目录）里的 `pinyin.<后缀>` 全部展开，字典序；排除自身。
    #[test]
    fn wildcard_expands_sorted_and_skips_self() {
        let root = temp_dir("import-wildcard");
        let user = root.join("user");
        let shared = root.join("shared");
        std::fs::create_dir_all(&user).unwrap();
        let dir = shared.join("dict/pinyin");
        write_dict_source(
            &dir.join("pinyin.dict.yaml"),
            "id: pinyin\nversion: \"1\"\nimport_tables:\n  - pinyin.*\n",
        );
        for name in ["pinyin.zi", "pinyin.jichu", "pinyin.diming"] {
            write_dict_source(&dir.join(format!("{name}.dict.yaml")), &format!("id: {name}\nversion: \"1\""));
        }
        // 系列子目录**不算**（`dict/pinyin/luna/` 是另一个系列，不归 `pinyin.*`）
        write_dict_source(
            &dir.join("luna/pinyin.luna.dict.yaml"),
            "id: pinyin.luna\nversion: \"1\"",
        );
        // 不匹配的邻居不进来
        write_dict_source(&dir.join("pinyin2.dict.yaml"), "id: pinyin2\nversion: \"1\"");
        write_dict_source(&dir.join("other.dict.yaml"), "id: other\nversion: \"1\"");

        let source = dir.join("pinyin.dict.yaml");
        let settings = settings_of("id: pinyin\nversion: \"1\"\nimport_tables:\n  - pinyin.*\n");
        assert_eq!(
            expand_import_tables(&user, &shared, Some(&source), &settings).unwrap(),
            vec!["pinyin", "pinyin.diming", "pinyin.jichu", "pinyin.zi"]
        );

        // 宽通配 `pinyin*` 也匹配自身，但必须被排除（只留身份那一项）
        let wide = settings_of("id: pinyin\nversion: \"1\"\nimport_tables:\n  - pinyin*\n");
        let names = expand_import_tables(&user, &shared, Some(&source), &wide).unwrap();
        assert_eq!(names.iter().filter(|name| name.as_str() == "pinyin").count(), 1);
        assert_eq!(names.first().map(String::as_str), Some("pinyin"));
        std::fs::remove_dir_all(&root).ok();
    }

    /// 精确条目在前且不被通配重复追加。
    #[test]
    fn exact_entries_win_over_wildcard() {
        let root = temp_dir("import-exact-wildcard");
        let user = root.join("user");
        let shared = root.join("shared");
        std::fs::create_dir_all(&user).unwrap();
        let dir = shared.join("dict/pinyin");
        for name in ["pinyin.zi", "pinyin.jichu"] {
            write_dict_source(&dir.join(format!("{name}.dict.yaml")), &format!("id: {name}\nversion: \"1\""));
        }
        let header = "id: pinyin\nversion: \"1\"\nimport_tables:\n  - pinyin.zi\n  - pinyin.*\n";
        let source = dir.join("pinyin.dict.yaml");
        write_dict_source(&source, header);
        assert_eq!(
            expand_import_tables(&user, &shared, Some(&source), &settings_of(header)).unwrap(),
            vec!["pinyin", "pinyin.zi", "pinyin.jichu"]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 一个都匹配不到 → 明确报错（模式 + 扫描目录 + 实际找到的文件）。
    #[test]
    fn wildcard_without_match_is_an_error() {
        let root = temp_dir("import-wildcard-miss");
        let user = root.join("user");
        let shared = root.join("shared");
        std::fs::create_dir_all(&user).unwrap();
        let dir = shared.join("dict/pinyin");
        write_dict_source(&dir.join("pinyin.zi.dict.yaml"), "id: pinyin.zi\nversion: \"1\"");
        let header = "id: pinyin\nversion: \"1\"\nimport_tables:\n  - pinyin.zzz.*\n";
        let source = dir.join("pinyin.dict.yaml");
        write_dict_source(&source, header);
        let error = expand_import_tables(&user, &shared, Some(&source), &settings_of(header))
            .expect_err("no match must fail");
        assert!(error.contains("pinyin.zzz.*"), "报错含模式: {error}");
        assert!(
            error.contains(&dir.display().to_string()),
            "报错含扫描目录: {error}"
        );
        assert!(error.contains("pinyin.zi"), "报错含实际找到的文件: {error}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// 用户目录与共享目录混放：两边都展开，同名文件仍由 `classified_dict_source` 按用户优先落地。
    #[test]
    fn wildcard_scans_both_roots_with_user_priority() {
        let root = temp_dir("import-wildcard-roots");
        let user = root.join("user");
        let shared = root.join("shared");
        write_dict_source(
            &user.join("dict/pinyin/pinyin.dict.yaml"),
            "id: pinyin\nversion: \"1\"\nimport_tables:\n  - pinyin.*\n",
        );
        write_dict_source(&user.join("dict/pinyin/pinyin.zi.dict.yaml"), "id: pinyin.zi\nversion: \"1\"");
        write_dict_source(
            &shared.join("dict/pinyin/pinyin.jichu.dict.yaml"),
            "id: pinyin.jichu\nversion: \"1\"",
        );
        write_dict_source(&shared.join("dict/pinyin/pinyin.zi.dict.yaml"), "id: pinyin.zi\nversion: \"1\"");
        let source = classified_dict_source(&user, &shared, "pinyin").expect("用户目录的源优先");
        assert_eq!(source, user.join("dict/pinyin/pinyin.dict.yaml"));
        let header = "id: pinyin\nversion: \"1\"\nimport_tables:\n  - pinyin.*\n";
        assert_eq!(
            expand_import_tables(&user, &shared, Some(&source), &settings_of(header)).unwrap(),
            vec!["pinyin", "pinyin.jichu", "pinyin.zi"]
        );
        // 覆盖关系不变：同名 `pinyin.zi` 仍是用户目录那份，共享独有的 jichu 能补上
        assert_eq!(
            classified_dict_source(&user, &shared, "pinyin.zi"),
            Some(user.join("dict/pinyin/pinyin.zi.dict.yaml"))
        );
        assert_eq!(
            classified_dict_source(&user, &shared, "pinyin.jichu"),
            Some(shared.join("dict/pinyin/pinyin.jichu.dict.yaml"))
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 通配只支持 `*`：`?` / `[]` 是字面字符。
    #[test]
    fn wildcard_only_supports_star() {
        assert!(wildcard_match("pinyin.*", "pinyin.zi"));
        assert!(wildcard_match("pinyin*", "pinyin"));
        assert!(wildcard_match("*", "anything"));
        assert!(!wildcard_match("pinyin.*", "pinyin"));
        assert!(!wildcard_match("pinyin.??", "pinyin.zi"));
        assert!(!wildcard_match("pinyin.[ab]", "pinyin.a"));
    }

    /// 同级存在（甚至同名的）子目录时，通配**不进入子目录**：只认本层 `*.dict.yaml`。
    ///
    /// 真实场景：`dict/pinyin/` 下还有 `luna/`、`terra/` 两个系列子目录，
    /// `pinyin.*` 不能把里面的 `pinyin.luna` / `pinyin.terra` 一起吃进来。
    #[test]
    fn wildcard_does_not_enter_sibling_subdirectories() {
        let root = temp_dir("import-wildcard-subdir");
        let user = root.join("user");
        let shared = root.join("shared");
        std::fs::create_dir_all(&user).unwrap();
        let dir = shared.join("dict/pinyin");
        let header = "id: pinyin\nversion: \"1\"\nimport_tables:\n  - pinyin.*\n";
        let source = dir.join("pinyin.dict.yaml");
        write_dict_source(&source, header);
        write_dict_source(&dir.join("pinyin.zi.dict.yaml"), "id: pinyin.zi\nversion: \"1\"");
        // 子目录里的表：普通名字的、以及与条目同名的子目录，都不参与展开
        write_dict_source(
            &dir.join("luna/pinyin.luna.dict.yaml"),
            "id: pinyin.luna\nversion: \"1\"",
        );
        write_dict_source(
            &dir.join("terra/pinyin.terra.dict.yaml"),
            "id: pinyin.terra\nversion: \"1\"",
        );
        write_dict_source(
            &dir.join("pinyin.zi/pinyin.deep.dict.yaml"),
            "id: pinyin.deep\nversion: \"1\"",
        );
        assert_eq!(
            expand_import_tables(&user, &shared, Some(&source), &settings_of(header)).unwrap(),
            vec!["pinyin", "pinyin.zi"]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn compile_end_to_end_produces_all_three_bins() {
        // 端到端：小词典 → .table.bin + .prism.bin + .reverse.bin 全部落位
        let root = temp_dir("e2e");
        let user = root.join("user");
        let shared = root.join("shared");
        let staging = root.join("build");
        // 源：dict/luna_pinyin/luna_pinyin.dict.yaml（单字 + 一个无码词组）
        let dict_dir = shared.join("dict/luna_pinyin");
        std::fs::create_dir_all(&dict_dir).unwrap();
        std::fs::write(
            dict_dir.join("luna_pinyin.dict.yaml"),
            "\
---
name: luna_pinyin
version: \"1.0\"
sort: by_weight
...
你\tni
好\thao
你好\t
",
        )
        .unwrap();
        // schema 源（给 speller/algebra 用；编译后的产物由调用方写 staging）
        let schema_dir = shared.join("schema/luna_pinyin");
        std::fs::create_dir_all(&schema_dir).unwrap();
        let schema_src = schema_dir.join("luna_pinyin.schema.yaml");
        std::fs::write(
            &schema_src,
            "\
schema:
  schema_id: luna_pinyin
translator:
  dictionary: luna_pinyin
",
        )
        .unwrap();

        let mut compiler = DictCompiler::new(
            "luna_pinyin",
            "",
            Vec::new(),
            &user,
            &shared,
            &staging,
            shared.join("prebuilt"),
        );
        compiler.compile(Some(&schema_src)).expect("compile");

        // 三个产物全部就位（系列路径）
        let table = staging.join("luna_pinyin/luna_pinyin.table.bin");
        let prism = staging.join("luna_pinyin/luna_pinyin.prism.bin");
        let reverse = staging.join("luna_pinyin/luna_pinyin.reverse.bin");
        assert!(table.is_file(), "table.bin 未生成: {}", table.display());
        assert!(prism.is_file(), "prism.bin 未生成: {}", prism.display());
        assert!(reverse.is_file(), "reverse.bin 未生成: {}", reverse.display());

        // 表能读回来：音节表 ni/hao，3 个词条
        let loaded = Table::open(&table).expect("open table");
        assert_eq!(loaded.syllabary_len(), 2, "ni / hao 两个音节");
        // prism 能读回来：has_key("ni")
        let prism_loaded = Prism::open(&prism).expect("open prism");
        assert!(prism_loaded.has_key("ni"), "prism 应含 ni");
        assert!(prism_loaded.has_key("hao"), "prism 应含 hao");
        assert!(!prism_loaded.has_key("xx"), "prism 不应含 xx");
        // reverse db 能读回来：你 → ni
        let mut reverse_db = ReverseDb::open(&reverse).expect("open reverse");
        assert!(reverse_db.load());
        assert_eq!(reverse_db.lookup("你").as_deref(), Some("ni"));
        assert_eq!(reverse_db.lookup("好").as_deref(), Some("hao"));

        // 第二次编译：checksum 全对 → 不重建（但产物仍在）
        compiler.compile(Some(&schema_src)).expect("recompile");
        assert!(table.is_file() && prism.is_file() && reverse.is_file());

        std::fs::remove_dir_all(&root).ok();
    }

    /// 阶段 A：一份表 → 主 prism（全拼）+ 额外 prism（双拼），同一套全局音节 id。
    #[test]
    fn extra_prism_is_built_from_same_syllabary() {
        let root = temp_dir("extra-prism");
        let user = root.join("user");
        let shared = root.join("shared");
        let staging = root.join("build");
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
        let mut extras = BTreeMap::new();
        extras.insert(
            "double".to_string(),
            vec![
                "xform/^zhong$/vs/".to_string(),
                "xform/^guo$/go/".to_string(),
            ],
        );
        compiler.set_extra_prisms(extras);
        compiler.compile(Some(&schema_src)).expect("compile");

        let primary = Prism::open(&staging.join("demo/demo.prism.bin")).expect("primary prism");
        let extra =
            Prism::open(&staging.join("demo/demo.double.prism.bin")).expect("extra prism");
        // 主 prism 不被额外分组改动（只检查双拼独有键，避免依赖 trie 的负例行为）。
        assert!(primary.has_key("zhong") && primary.has_key("guo"));
        assert!(!primary.has_key("vs"));
        // 额外 prism 只换键集，音节 id 与主 prism 相同。
        let zhong = primary.get_value("zhong").expect("zhong");
        let guo = primary.get_value("guo").expect("guo");
        assert_eq!(
            primary.spelling_syllable_id(zhong),
            extra.spelling_syllable_id(extra.get_value("vs").expect("vs"))
        );
        assert_eq!(
            primary.spelling_syllable_id(guo),
            extra.spelling_syllable_id(extra.get_value("go").expect("go"))
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// 阶段 B：无 `build/dictionaries` 或只提到别的词库时，解析必须是空操作。
    #[test]
    fn build_dictionaries_absent_or_other_dict_is_noop() {
        let plain = serde_json::json!({"schema": {"schema_id": "demo"}});
        assert_eq!(
            parse_build_dictionaries(&plain, "demo").unwrap(),
            BuildDictionarySpec::default()
        );
        let other = serde_json::json!({
            "build": {"dictionaries": {"other": {"algebra": {"double": ["xform/^a$/b/"]}}}}
        });
        assert_eq!(
            parse_build_dictionaries(&other, "demo").unwrap(),
            BuildDictionarySpec::default()
        );
    }

    /// 阶段 B：`algebra` 映射 → 每个合法分组一份额外 prism（完整名 = dict.group）。
    #[test]
    fn build_dictionaries_map_groups_become_extra_prisms() {
        let config = serde_json::json!({
            "build": {"dictionaries": {"demo": {"algebra": {
                "double": ["xform/^zhong$/vs/", "xform/^guo$/go/"],
                "broken": "not-a-list",
                "": ["xform/^a$/b/"],
                "__patch": ["xform/^a$/b/"]
            }}}}
        });
        let spec = parse_build_dictionaries(&config, "demo").unwrap();
        assert_eq!(spec.primary_algebra, None);
        assert_eq!(spec.extra_prisms.len(), 1, "非法分组应被跳过");
        assert_eq!(spec.extra_prisms["demo.double"].len(), 2);
    }

    /// 阶段 B：`algebra` 列表 → 主 prism 规则覆盖；空列表/形态错误的行为要明确。
    #[test]
    fn build_dictionaries_list_sets_primary_algebra() {
        let config = serde_json::json!({
            "build": {"dictionaries": {"demo": {"algebra": ["xform/^zhong$/vs/"]}}}
        });
        let spec = parse_build_dictionaries(&config, "demo").unwrap();
        assert_eq!(
            spec.primary_algebra.as_deref(),
            Some(&["xform/^zhong$/vs/".to_string()][..])
        );
        assert!(spec.extra_prisms.is_empty());

        // 空列表 = 缺省（不能把主 prism 变成"无拼写变换"）。
        let empty = serde_json::json!({
            "build": {"dictionaries": {"demo": {"algebra": []}}}
        });
        assert_eq!(
            parse_build_dictionaries(&empty, "demo").unwrap().primary_algebra,
            None
        );

        // 顶层形态错误 → 该词库配置算写坏了。
        let bad = serde_json::json!({
            "build": {"dictionaries": {"demo": {"algebra": "oops"}}}
        });
        assert!(parse_build_dictionaries(&bad, "demo").is_err());
    }

    /// 阶段 B：列表形态确实替换主 prism 的键集；主 prism 名 != 词库名时忽略。
    #[test]
    fn build_dictionaries_list_overrides_primary_prism_only_when_name_matches() {
        let root = temp_dir("primary-override");
        let user = root.join("user");
        let shared = root.join("shared");
        let staging = root.join("build");
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

        let rules = vec![
            "xform/^zhong$/vs/".to_string(),
            "xform/^guo$/go/".to_string(),
        ];
        let mut compiler = DictCompiler::new(
            "demo",
            "",
            Vec::new(),
            &user,
            &shared,
            &staging,
            shared.join("prebuilt"),
        );
        compiler.set_primary_algebra(Some(rules.clone()));
        compiler.compile(Some(&schema_src)).expect("compile");
        let primary = Prism::open(&staging.join("demo/demo.prism.bin")).expect("primary prism");
        assert!(primary.has_key("vs") && primary.has_key("go"));
        assert!(!primary.has_key("zhong"), "列表覆盖应替换主 prism 的键集");

        // 主 prism 名与词库名不同：覆盖必须被忽略（prism 可能被别的方案共用）。
        let shared_staging = root.join("build-shared");
        let mut shared_compiler = DictCompiler::new(
            "demo",
            "demo.shared",
            Vec::new(),
            &user,
            &shared,
            &shared_staging,
            shared.join("prebuilt"),
        );
        shared_compiler.set_primary_algebra(Some(rules));
        shared_compiler.compile(Some(&schema_src)).expect("compile shared");
        let prism = Prism::open(&shared_staging.join("demo/shared/demo.shared.prism.bin"))
            .expect("shared prism");
        assert!(prism.has_key("zhong") && !prism.has_key("vs"));

        std::fs::remove_dir_all(&root).ok();
    }

    /// 阶段 B 回归钉：不写 `build/dictionaries` 时，两次编译的三个产物逐位一致，
    /// 且不会多出 `<dict>.*.prism.bin`。
    #[test]
    fn no_build_dictionaries_products_are_byte_identical() {
        let root = temp_dir("no-multi");
        let user = root.join("user");
        let shared = root.join("shared");
        let dict_dir = shared.join("dict/demo");
        std::fs::create_dir_all(&dict_dir).unwrap();
        std::fs::write(
            dict_dir.join("demo.dict.yaml"),
            "---\nname: demo\nversion: \"1.0\"\nsort: by_weight\ncolumns:\n  - text\n  - code\n...\n中国\tzhong guo\n你好\tni hao\n",
        )
        .unwrap();
        let schema_src = shared.join("schema/demo/demo.schema.yaml");
        std::fs::create_dir_all(schema_src.parent().unwrap()).unwrap();
        std::fs::write(
            &schema_src,
            "schema:\n  schema_id: demo\ntranslator:\n  dictionary: demo\n",
        )
        .unwrap();

        let staging_a = root.join("build-a");
        let staging_b = root.join("build-b");
        for staging in [&staging_a, &staging_b] {
            let mut compiler = DictCompiler::new(
                "demo",
                "",
                Vec::new(),
                &user,
                &shared,
                staging,
                shared.join("prebuilt"),
            );
            compiler.compile(Some(&schema_src)).expect("compile");
        }
        for name in [
            "demo/demo.table.bin",
            "demo/demo.prism.bin",
            "demo/demo.reverse.bin",
        ] {
            let a = std::fs::read(staging_a.join(name)).expect("read a");
            let b = std::fs::read(staging_b.join(name)).expect("read b");
            assert_eq!(a, b, "{name} 不应因阶段 B 的解析而不同");
        }
        assert!(
            !staging_a.join("demo/demo.double.prism.bin").exists(),
            "无配置时不应产生额外 prism"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// 阶段 B：`__patch` 在配置编译期展平，解析器拿到的是最终分组。
    #[test]
    fn build_dictionaries_patch_is_flattened_before_parse() {
        let root = temp_dir("patch-flatten");
        let shared = root.join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("default.yaml"), "config_version: \"1\"\n").unwrap();
        std::fs::write(
            shared.join("algebra.yaml"),
            "build:\n  dictionaries:\n    demo:\n      algebra:\n        double:\n          - xform/^zhong$/vs/\n",
        )
        .unwrap();
        let schema_src = shared.join("schema/demo/demo.schema.yaml");
        std::fs::create_dir_all(schema_src.parent().unwrap()).unwrap();
        std::fs::write(
            &schema_src,
            "schema:\n  schema_id: demo\nbuild:\n  __patch: algebra.yaml:/build\n",
        )
        .unwrap();

        let compiled = crate::config::compile(&schema_src, &shared.join("default.yaml"), &[])
            .expect("compile schema");
        let compiled_path = root.join("compiled.yaml");
        std::fs::write(&compiled_path, crate::config::to_yaml_string(&compiled).unwrap()).unwrap();
        let value = crate::load_file(&compiled_path).unwrap();
        let spec = parse_build_dictionaries(&value, "demo").unwrap();
        assert_eq!(
            spec.extra_prisms["demo.double"],
            vec!["xform/^zhong$/vs/".to_string()]
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// 真实数据诊断：同一词典连续编译两次，第二次必须**复用**（mtime 不变）。
    /// 真实数据已按 `dict/` + `schema/` 拆家（2026-10-28）：明月拼音的 id 是
    /// `pinyin.luna`，词库在 `dict/pinyin/luna/`、方案在 `schema/pinyin/luna/`。
    /// 仓库共享目录不在时跳过（CI / 换机器）。
    #[test]
    fn real_luna_pinyin_second_compile_reuses_table() {
        let shared = Path::new("C:/code/Weasel-Next/Rime-Next");
        let schema = shared.join("schema/pinyin/luna/pinyin.luna.schema.yaml");
        let dict = shared.join("dict/pinyin/luna/pinyin.luna.dict.yaml");
        if !schema.is_file() || !dict.is_file() {
            eprintln!("skip: no real shared data dir");
            return;
        }
        let root = temp_dir("real-reuse");
        let user = root.join("user");
        let staging = root.join("build");
        std::fs::create_dir_all(&user).unwrap();
        let mut compiler = DictCompiler::new(
            "pinyin.luna",
            "pinyin.luna",
            Vec::new(),
            &user,
            shared,
            &staging,
            shared.join("prebuilt"),
        );
        compiler.compile(Some(&schema)).expect("first compile");
        let table = staging.join("pinyin/luna/pinyin.luna.table.bin");
        let before = std::fs::metadata(&table).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        compiler.compile(Some(&schema)).expect("second compile");
        let after = std::fs::metadata(&table).unwrap().modified().unwrap();
        assert_eq!(before, after, "checksum 没变，第二次不该重建表");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn compile_rebuilds_when_source_changes() {
        let root = temp_dir("rebuild");
        let user = root.join("user");
        let shared = root.join("shared");
        let staging = root.join("build");
        let dict_dir = shared.join("dict/luna_pinyin");
        std::fs::create_dir_all(&dict_dir).unwrap();
        let dict_file = dict_dir.join("luna_pinyin.dict.yaml");
        std::fs::write(&dict_file, "---\nname: luna_pinyin\nversion: \"1.0\"\n...\n你\tni\n").unwrap();
        let schema_src = shared.join("schema/luna_pinyin/luna_pinyin.schema.yaml");
        std::fs::create_dir_all(schema_src.parent().unwrap()).unwrap();
        std::fs::write(&schema_src, "schema:\n  schema_id: luna_pinyin\ntranslator:\n  dictionary: luna_pinyin\n").unwrap();
        let mut compiler = DictCompiler::new(
            "luna_pinyin",
            "",
            Vec::new(),
            &user,
            &shared,
            &staging,
            shared.join("prebuilt"),
        );
        compiler.compile(Some(&schema_src)).expect("first compile");
        let table = staging.join("luna_pinyin/luna_pinyin.table.bin");
        let before = std::fs::metadata(&table).unwrap().modified().unwrap();
        // 改源文件 → checksum 变 → 重建
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&dict_file, "---\nname: luna_pinyin\nversion: \"1.0\"\n...\n你\tni\n好\thao\n").unwrap();
        compiler.compile(Some(&schema_src)).expect("second compile");
        let after = std::fs::metadata(&table).unwrap().modified().unwrap();
        assert!(after > before, "源变了表必须重建");
        let loaded = Table::open(&table).expect("open table");
        assert_eq!(loaded.syllabary_len(), 2, "重建后应有 ni/hao 两个音节");
        std::fs::remove_dir_all(&root).ok();
    }

    // ------------------------------------------------- 阶段 1：packs 合并单表

    /// 阶段 1 fixture：主表 `demo`（甲 ab / 卯 cd）+ pack `demo.one`（乙 ab）+
    /// pack `demo.two`（丙 cd / **戊 zz**，`zz` 不在主表音节表里）。
    fn packs_fixture(root: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let user = root.join("user");
        let shared = root.join("shared");
        let staging = user.join("build");
        for dir in [&user, &shared, &staging] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let dict_dir = shared.join("dict/demo");
        std::fs::create_dir_all(&dict_dir).unwrap();
        std::fs::write(
            dict_dir.join("demo.dict.yaml"),
            "---\nname: demo\nversion: \"1.0\"\nsort: by_weight\ncolumns:\n  - text\n  - code\n...\n甲\tab\n卯\tcd\n",
        )
        .unwrap();
        std::fs::write(
            dict_dir.join("demo.one.dict.yaml"),
            "---\nname: demo.one\nversion: \"1.0\"\nsort: by_weight\ncolumns:\n  - text\n  - code\n  - weight\n...\n乙\tab\t1000\n",
        )
        .unwrap();
        std::fs::write(
            dict_dir.join("demo.two.dict.yaml"),
            "---\nname: demo.two\nversion: \"1.0\"\nsort: by_weight\ncolumns:\n  - text\n  - code\n  - weight\n...\n丙\tcd\t500\n戊\tzz\t100\n",
        )
        .unwrap();
        let schema_src = shared.join("schema/demo/demo.schema.yaml");
        std::fs::create_dir_all(schema_src.parent().unwrap()).unwrap();
        std::fs::write(
            &schema_src,
            "schema:\n  schema_id: demo\ntranslator:\n  dictionary: demo\n",
        )
        .unwrap();
        (user, shared, staging, schema_src)
    }

    fn merged_pack_texts(staging: &Path) -> Vec<String> {
        let table =
            Table::open(staging.join("demo/demo.packs.table.bin")).expect("open merged packs");
        let mut texts: Vec<String> = table
            .entries_sorted_by_weight()
            .into_iter()
            .map(|(_, entry)| table.entry_text(&entry))
            .collect();
        texts.sort();
        texts
    }

    #[test]
    fn packs_are_merged_into_one_table_and_old_tables_removed() {
        let root = temp_dir("packs-merge");
        let (user, shared, staging, schema_src) = packs_fixture(&root);
        // 先按旧方式编出一包一表（模拟升级前已经存在的 build 目录）
        for name in ["demo", "demo.one", "demo.two"] {
            let mut legacy = DictCompiler::new(
                name,
                "",
                Vec::new(),
                &user,
                &shared,
                &staging,
                shared.join("prebuilt"),
            );
            legacy
                .compile(Some(&schema_src))
                .unwrap_or_else(|e| panic!("legacy {name}: {e}"));
        }
        let old_one = staging.join("demo/one/demo.one.table.bin");
        let old_two = staging.join("demo/two/demo.two.table.bin");
        assert!(old_one.is_file() && old_two.is_file(), "旧布局前提");

        let mut compiler = DictCompiler::new(
            "demo",
            "",
            vec!["demo.one".to_string(), "demo.two".to_string()],
            &user,
            &shared,
            &staging,
            shared.join("prebuilt"),
        );
        compiler.compile(Some(&schema_src)).expect("compile merges packs");

        let merged = staging.join("demo/demo.packs.table.bin");
        assert!(merged.is_file(), "合并 pack 表应落位: {}", merged.display());
        assert!(
            !old_one.is_file() && !old_two.is_file(),
            "旧的一包一表应被清掉"
        );
        assert_eq!(
            merged_pack_texts(&staging),
            vec!["丙".to_string(), "乙".to_string(), "戊".to_string()],
            "两个 pack 的词条都在合并表里（含 pack 独有新音节 zz 的词条）"
        );
        // 阶段 2：pack 表按**自己的局部音节表**编码，`zz` 不再被丢弃；
        // prism v6 的 union 音节表也纳入 `zz`，所以它真的能被打出来。
        assert!(merged_pack_texts(&staging).contains(&"戊".to_string()));
        let prism = Prism::open(staging.join("demo/demo.prism.bin")).expect("open prism");
        assert!(prism.has_packs(), "含 packs 的方案应写 v6 prism");
        assert!(prism.has_key("zz"), "union 音节表应包含 pack 独有新音节");
        let syllabary = prism.syllabary().expect("v6 应带音节表");
        assert!(syllabary.iter().any(|s| s == "zz"), "prism 音节表应含 zz");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn pack_fingerprint_does_not_follow_primary_dict() {
        let root = temp_dir("packs-fingerprint");
        let (user, shared, staging, schema_src) = packs_fixture(&root);
        let mut compiler = DictCompiler::new(
            "demo",
            "",
            vec!["demo.one".to_string(), "demo.two".to_string()],
            &user,
            &shared,
            &staging,
            shared.join("prebuilt"),
        );
        compiler.compile(Some(&schema_src)).expect("first compile");
        let merged = staging.join("demo/demo.packs.table.bin");
        let before = std::fs::metadata(&merged).unwrap().modified().unwrap();

        // 主表加词（音节表不变）→ 主表重编，但 pack 指纹只看 pack 源
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(
            shared.join("dict/demo/demo.dict.yaml"),
            "---\nname: demo\nversion: \"1.0\"\nsort: by_weight\ncolumns:\n  - text\n  - code\n...\n甲\tab\n卯\tcd\n巳\tab\n",
        )
        .unwrap();
        compiler.compile(Some(&schema_src)).expect("second compile");
        let after = std::fs::metadata(&merged).unwrap().modified().unwrap();
        assert_eq!(before, after, "主表改词不该连带重编 pack 表");

        std::fs::remove_dir_all(&root).ok();
    }

    /// 阶段 2：prism v6 的失效指纹**只算码集** —— 只改 pack 词条的权重时，
    /// pack 表重编（指纹吃了源文件），但 prism 不该重编。
    #[test]
    fn changing_pack_weight_rebuilds_table_but_not_prism() {
        let root = temp_dir("packs-prism-checksum");
        let (user, shared, staging, schema_src) = packs_fixture(&root);
        let mut compiler = DictCompiler::new(
            "demo",
            "",
            vec!["demo.one".to_string(), "demo.two".to_string()],
            &user,
            &shared,
            &staging,
            shared.join("prebuilt"),
        );
        compiler.compile(Some(&schema_src)).expect("first compile");
        let prism = staging.join("demo/demo.prism.bin");
        let merged = staging.join("demo/demo.packs.table.bin");
        let prism_before = std::fs::metadata(&prism).unwrap().modified().unwrap();
        let merged_before = std::fs::metadata(&merged).unwrap().modified().unwrap();

        // 只改权重（音节表不变）→ pack 表重建、prism 复用
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(
            shared.join("dict/demo/demo.two.dict.yaml"),
            "---\nname: demo.two\nversion: \"1.0\"\nsort: by_weight\ncolumns:\n  - text\n  - code\n  - weight\n...\n丙\tcd\t900\n戊\tzz\t100\n",
        )
        .unwrap();
        compiler.compile(Some(&schema_src)).expect("second compile");
        assert!(
            std::fs::metadata(&merged).unwrap().modified().unwrap() > merged_before,
            "pack 源变了，合并表必须重编"
        );
        assert_eq!(
            std::fs::metadata(&prism).unwrap().modified().unwrap(),
            prism_before,
            "码集没变，prism v6 不该重编"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// 阶段 2 兼容：不带 packs 的方案继续写 **v5** prism（metadata 仍是 320 字节、
    /// 没有音节表），旧代码照常能读；重新编译也不会因为"没有音节表"而错判为过期。
    #[test]
    fn schema_without_packs_keeps_v5_prism() {
        let root = temp_dir("prism-v5");
        let (user, shared, staging, schema_src) = packs_fixture(&root);
        let mut compiler =
            DictCompiler::new("demo", "", Vec::new(), &user, &shared, &staging, shared.join("prebuilt"));
        compiler.compile(Some(&schema_src)).expect("compile v5");

        let prism_path = staging.join("demo/demo.prism.bin");
        let prism = Prism::open(&prism_path).expect("open prism");
        assert!(!prism.has_packs(), "不带 packs 不该写 v6 标记");
        assert!(!prism.has_syllabary(), "v5 没有外置音节表");
        assert!(prism.syllabary().is_none(), "v5 的 syllable_by_id 走主表");
        assert!(prism.has_key("ab") && prism.has_key("cd"), "v5 键集照常");

        // 再编一次：v5 沿用旧 dict checksum，不应误判过期（mtime 不变）
        let before = std::fs::metadata(&prism_path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        compiler.compile(Some(&schema_src)).expect("recompile v5");
        assert_eq!(
            std::fs::metadata(&prism_path).unwrap().modified().unwrap(),
            before,
            "v5 校验和没变，prism 不该重编"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    fn enabled_fixture(enabled: Option<&[&str]>) -> serde_json::Value {
        let mut config = serde_json::json!({
            "build": { "dict": { "pinyin": { "algebra": {
                "zrm": ["derive/^([jqxy])u$/$1v/"],
                "flypy": ["derive/^([jqxy])u$/$1v/"]
            } } } }
        });
        if let Some(enabled) = enabled {
            config["input_types"] = serde_json::json!({ "enabled": enabled });
        }
        config
    }

    fn enabled_groups(spec: &BuildDictionarySpec) -> Vec<String> {
        let mut groups: Vec<String> = spec.extra_prisms.keys().cloned().collect();
        groups.sort();
        groups
    }

    #[test]
    fn input_types_enabled_is_compile_whitelist() {
        let spec = parse_build_dictionaries(&enabled_fixture(Some(&["zrm"])), "pinyin").unwrap();
        assert_eq!(enabled_groups(&spec), vec!["pinyin.zrm"]);

        let spec = parse_build_dictionaries(&enabled_fixture(Some(&["flypy", "zrm"])), "pinyin").unwrap();
        assert_eq!(enabled_groups(&spec), vec!["pinyin.flypy", "pinyin.zrm"]);
    }

    #[test]
    fn input_types_absent_or_empty_falls_back() {
        // 不写 input_types：回退到"algebra 里声明的全编译"。
        let spec = parse_build_dictionaries(&enabled_fixture(None), "pinyin").unwrap();
        assert_eq!(enabled_groups(&spec), vec!["pinyin.flypy", "pinyin.zrm"]);

        // 显式空列表：字面语义是"一个组都不编"。
        let spec = parse_build_dictionaries(&enabled_fixture(Some(&[])), "pinyin").unwrap();
        assert!(enabled_groups(&spec).is_empty());
    }
}
