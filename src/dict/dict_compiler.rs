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
//! 5. 存在性 + 校验和比对                      决定 rebuild_table / rebuild_prism
//!    （primary_table / prism / reverse_db 三处；reverse_db 过期也逼重建表）
//! 6. rebuild_table → EntryCollector → BuildTable(0)   （主表 .table.bin + .reverse.bin）
//! 7. rebuild_prism → BuildPrism               （.prism.bin，吃 speller/algebra）
//! 8. packs 循环（table_index ≥ 1）             import_tables 的附加表
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

use std::path::{Path, PathBuf};

use crate::algo::algebra::{Projection, Script};
use crate::algo::utilities::{self, ChecksumComputer};
use crate::dict::dict_settings::DictSettings;
use crate::dict::entry_collector::EntryCollector;
use crate::dict::preset_vocabulary::resolve_dict_file_path;
use crate::dict::prism::Prism;
use crate::dict::reverse_lookup_dictionary::ReverseDb;
use crate::dict::table::Table;
use crate::dict::vocabulary::{Code, ShortDictEntry, Syllabary, Vocabulary};

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
        }
    }

    pub fn set_options(&mut self, options: CompileOptions) {
        self.options = options;
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
        for table in settings.get_tables() {
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
        let mut rebuild_prism = match open_prism_quiet(&prism_path) {
            Some(prism) => {
                prism.dict_file_checksum() != dict_file_checksum
                    || prism.schema_file_checksum() != schema_file_checksum
            }
            None => true,
        };
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
        if self.options.rebuild_prism {
            rebuild_prism = true;
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
        } else if !self.packs.is_empty() {
            syllabary = open_table_quiet(&table_path)
                .and_then(|table| table.syllabary())
                .inspect(|_| log_info(&format!("loaded syllabary from {}", table_path.display())));
        }
        // 7. prism
        if rebuild_prism {
            self.build_prism(schema_file, dict_file_checksum, schema_file_checksum)?;
        }
        // 8. packs
        for (index, pack_name) in self.packs.clone().iter().enumerate() {
            let table_index = index + 1;
            let mut collector = match syllabary.take() {
                Some(fixed) => EntryCollector::with_fixed_syllabary(fixed),
                None => EntryCollector::new(),
            };
            collector.set_data_roots(self.source_roots());
            let Some(pack_file) =
                classified_dict_source(&self.user_data_dir, &self.shared_data_dir, pack_name)
            else {
                // C++：源不在就看有没有现成表，都没有 → 记错误并跳过该 pack
                if open_table_quiet(&self.table_target(table_index)).is_none() {
                    log_error(&format!(
                        "neither pack source file '{pack_name}' nor a prebuilt table exists"
                    ));
                } else {
                    log_info(&format!(
                        "pack source file '{pack_name}' does not exist, using prebuilt table"
                    ));
                }
                continue;
            };
            let Some((pack_settings, _)) = DictSettings::load_dict_header_from_file(&pack_file)
                .map_err(|e| format!("read pack source: {e}"))?
            else {
                log_error(&format!("failed to load settings from '{}'.", pack_file.display()));
                continue;
            };
            let mut pack_files = Vec::new();
            let mut missing = false;
            for table in pack_settings.get_tables() {
                let name = if table.is_empty() { pack_name.clone() } else { table };
                match classified_dict_source(&self.user_data_dir, &self.shared_data_dir, &name) {
                    Some(file) => pack_files.push(file),
                    None => {
                        missing = true;
                        break;
                    }
                }
            }
            if missing {
                continue;
            }
            let pack_checksum =
                compute_dict_file_checksum(dict_file_checksum, &pack_files, &pack_settings, &self.vocab_roots());
            let rebuild_pack = open_table_quiet(&self.table_target(table_index))
                .is_none_or(|table| table.dict_file_checksum() != pack_checksum);
            if rebuild_pack {
                log_info(&format!("rebuilding pack '{pack_name}'"));
                match self.build_table(table_index, &mut collector, &pack_settings, &pack_files, pack_checksum) {
                    Ok(next_syllabary) => syllabary = Some(next_syllabary),
                    Err(e) => log_error(&format!("failed to build pack: {pack_name}: {e}")),
                }
            } else {
                log_info(&format!("pack '{pack_name}' reuses up-to-date table"));
                syllabary = open_table_quiet(&self.table_target(table_index))
                    .and_then(|table| table.syllabary());
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
        // 词条 → vocabulary（C++:291-321）
        let syllable_to_id = syllabary.syllable_to_id();
        let entries = collector.take_entries();
        let num_entries = entries.len();
        let mut vocabulary = Vocabulary::new();
        for entry in &entries {
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
        if settings.sort_order() != "original" {
            vocabulary.sort_homophones();
        }
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

    /// `DictCompiler::BuildPrism`（`dict_compiler.cc:353-424`）。
    fn build_prism(
        &self,
        schema_file: Option<&Path>,
        dict_file_checksum: u32,
        schema_file_checksum: u32,
    ) -> Result<(), String> {
        log_info("building prism...");
        let target_path = self.prism_target();
        // 音节表从主表读（主表可能没重建）
        let Some(syllabary) = open_table_quiet(&self.table_target(0))
            .and_then(|table| table.syllabary())
            .filter(|s| !s.is_empty())
        else {
            return Err(format!(
                "cannot get syllabary from primary table {}",
                self.table_target(0).display()
            ));
        };
        // speller/algebra → Script（C++:367-407）
        let mut script = Script::new();
        if let Some(schema_file) = schema_file.filter(|p| !p.as_os_str().is_empty()) {
            let value = crate::load_file(schema_file)
                .map_err(|e| format!("error loading prism definition from {schema_file:?}: {e}"))?;
            let rules: Vec<String> = value
                .pointer("/speller/algebra")
                .and_then(|list| list.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if !rules.is_empty() {
                let mut projection = Projection::from_rules(&rules)
                    .map_err(|e| format!("error loading prism definition: {e}"))?;
                for syllable in syllabary.iter() {
                    script.add_syllable(syllable);
                }
                if !projection.apply_script(&mut script) {
                    script = Script::new(); // C++：script.clear()
                }
            }
        }
        let script_ref = if script.is_empty() { None } else { Some(&script) };
        Prism::compile(&target_path, &syllabary, script_ref, dict_file_checksum, schema_file_checksum)
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
    /// id 含 '/' 时按显式相对路径；裸 id 按点号分段进 `schema/<系列…>/`。
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

    /// prism 目标：`<staging>/<系列…>/<prism_name>.prism.bin`。
    fn prism_target(&self) -> PathBuf {
        relocate_target(&self.staging_dir, &format!("{}.prism.bin", self.prism_name))
    }

    /// reverse db 目标：`<staging>/<系列…>/<dict_name>.reverse.bin`。
    fn reverse_target(&self) -> PathBuf {
        relocate_target(&self.staging_dir, &format!("{}.reverse.bin", self.dict_name))
    }
}

/// `classified_dict_source`（`dict_compiler.cc:56-86`，C++ 的 static 函数）：
/// 先用户目录后共享目录，**没有扁平回退**；找不到返回 `None`。
///
/// [Weasel-Next 宽容项 2026-10-26] 裸 id 的系列路径失手后，再扫两个数据根的
/// `schema/<一级子目录>/`（兄弟方案目录）。场景：用户目录里留着**旧版无前缀**
/// `import_tables`（`- yustar.quick`）的词典头，而带前缀写法的新版在共享目录
/// `schema/yuhao/` 下——旧副本按优先级胜出后，系列规则
/// `schema/yustar/quick/…` 找不到文件，真机 deploy 因此 18/2。
/// 只扫一级目录、只认完整文件名，不会把扁平文件捞回来。
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
        let mut nested = root.to_path_buf();
        if !explicit {
            nested = nested.join("schema");
            for segment in dict_name.split('.') {
                if !segment.is_empty() {
                    nested = nested.join(segment);
                }
            }
        }
        nested = nested.join(&file_name);
        if nested.is_file() {
            return Some(nested);
        }
    }
    // 宽容搜索：裸 id 在兄弟方案目录里找（见函数头注释）。
    if !explicit {
        for root in [user_data_dir, shared_data_dir] {
            let schema_dir = root.join("schema");
            let Ok(entries) = std::fs::read_dir(&schema_dir) else {
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
        // 分类路径才认
        std::fs::create_dir_all(shared.join("schema/luna_pinyin")).unwrap();
        std::fs::write(shared.join("schema/luna_pinyin/luna_pinyin.dict.yaml"), "x").unwrap();
        assert_eq!(
            classified_dict_source(&user, &shared, "luna_pinyin"),
            Some(shared.join("schema/luna_pinyin/luna_pinyin.dict.yaml"))
        );
        // 用户目录优先
        std::fs::create_dir_all(user.join("schema/yuling/five")).unwrap();
        std::fs::write(user.join("schema/yuling/five/yuling.five.dict.yaml"), "x").unwrap();
        assert_eq!(
            classified_dict_source(&user, &shared, "yuling.five"),
            Some(user.join("schema/yuling/five/yuling.five.dict.yaml"))
        );
        // 显式相对路径：不套系列规则
        std::fs::create_dir_all(shared.join("schema/yuhao")).unwrap();
        std::fs::write(shared.join("schema/yuhao/yuling.quick.dict.yaml"), "x").unwrap();
        assert_eq!(
            classified_dict_source(&user, &shared, "schema/yuhao/yuling.quick"),
            Some(shared.join("schema/yuhao/yuling.quick.dict.yaml"))
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
        // 源：schema/luna_pinyin/luna_pinyin.dict.yaml（单字 + 一个无码词组）
        let dict_dir = shared.join("schema/luna_pinyin");
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
        let schema_src = shared.join("schema/luna_pinyin/luna_pinyin.schema.yaml");
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

    /// 真实数据诊断：同一词典连续编译两次，第二次必须**复用**（mtime 不变）。
    /// 仓库共享目录不在时跳过（CI / 换机器）。
    #[test]
    fn real_luna_pinyin_second_compile_reuses_table() {
        let shared = Path::new("C:/code/Weasel-Next/Rime-Next");
        let schema = shared.join("schema/luna_pinyin/luna_pinyin.schema.yaml");
        if !schema.is_file() {
            eprintln!("skip: no real shared data dir");
            return;
        }
        let root = temp_dir("real-reuse");
        let user = root.join("user");
        let staging = root.join("build");
        std::fs::create_dir_all(&user).unwrap();
        let mut compiler = DictCompiler::new(
            "luna_pinyin",
            "luna_pinyin",
            Vec::new(),
            &user,
            shared,
            &staging,
            shared.join("prebuilt"),
        );
        compiler.compile(Some(&schema)).expect("first compile");
        let table = staging.join("luna_pinyin/luna_pinyin.table.bin");
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
        let dict_dir = shared.join("schema/luna_pinyin");
        std::fs::create_dir_all(&dict_dir).unwrap();
        let dict_file = dict_dir.join("luna_pinyin.dict.yaml");
        std::fs::write(&dict_file, "---\nname: luna_pinyin\nversion: \"1.0\"\n...\n你\tni\n").unwrap();
        let schema_src = shared.join("schema/luna_pinyin/luna_pinyin.schema.yaml");
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
}
