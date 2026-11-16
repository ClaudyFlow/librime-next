//! 一次性用户词典合并工具（阶段 E：旧双拼库 → 统一方案 `pinyin.userdb`）。
//!
//! **不改引擎行为**：这里只调用 `dict::level_db` / `dict::user_db` 已有的读写 API，
//! 把若干 `.userdb` 的条目按「规范音节 + 词条文本」逐键合并进目标库。
//!
//! 入口脚本：`script/merge-userdb.ps1`（带 `-DryRun`，默认拒写 `%APPDATA%`）。
//!
//! ```text
//! cargo run -p librime-next --example userdb_merge -- \
//!   --source <a.userdb> [--source <b.userdb> ...] \
//!   --target <pinyin.userdb> \
//!   [--dry-run] [--export <merged.tsv>] [--prism <pinyin.prism.bin>] \
//!   [--allow-real]
//! ```
//!
//! 合并规则（同一 key 出现多次时）：
//! * `commits`：取**绝对值**大的那个（与 `UserDbMerger` 的规则一致；删除标记也按它）；
//! * `dee`：取 max；
//! * `tick`：取 max；
//! * 手排（`position` / `manual_stamp`）：`manual_stamp` 新的覆盖旧的，只有一边有就取有的一边。
//!
//! 这里**不做时间衰减**：一次性迁移在同一台机器上完成，两边的 tick 可直接比较；
//! 衰减留给定时的同步流程（`UserDbMerger`）去算。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use librime_next::gear::dict::db::{Db, DbSource};
use librime_next::gear::dict::level_db::{USERDB_TYPE, LevelDb};
use librime_next::gear::dict::prism::Prism;
use librime_next::gear::dict::user_db::UserDbValue;
use librime_next::gear::dict::vocabulary::Syllabary;

#[derive(Default, Clone)]
struct Merged {
    value: UserDbValue,
    sources: Vec<String>,
}

#[derive(Default)]
struct Stats {
    read: usize,
    valid: usize,
    deleted: usize,
    /// 失败原因 → 条数
    failures: BTreeMap<&'static str, usize>,
    /// 失败样例（每类最多 5 条，控制输出）
    samples: BTreeMap<&'static str, Vec<String>>,
}

impl Stats {
    fn fail(&mut self, reason: &'static str, key: &str) {
        *self.failures.entry(reason).or_insert(0) += 1;
        let samples = self.samples.entry(reason).or_default();
        if samples.len() < 5 {
            samples.push(key.replace('\t', "\\t"));
        }
    }

    fn summary(&self) -> String {
        let mut out = format!(
            "read={} valid={} deleted={}",
            self.read, self.valid, self.deleted
        );
        for (reason, count) in &self.failures {
            out.push_str(&format!("; {reason}={count}"));
        }
        out
    }
}

#[derive(Debug)]
struct Args {
    sources: Vec<PathBuf>,
    target: PathBuf,
    dry_run: bool,
    export: Option<PathBuf>,
    prism: Option<PathBuf>,
    allow_real: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: userdb_merge --source <dir> [--source <dir> ...] --target <dir>\n\
         \x20[--dry-run] [--export <merged.tsv>] [--prism <*.prism.bin>] [--allow-real]"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut args = Args {
        sources: Vec::new(),
        target: PathBuf::new(),
        dry_run: false,
        export: None,
        prism: None,
        allow_real: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--source" => args.sources.push(PathBuf::from(it.next().unwrap_or_else(|| usage()))),
            "--target" => args.target = PathBuf::from(it.next().unwrap_or_else(|| usage())),
            "--export" => args.export = Some(PathBuf::from(it.next().unwrap_or_else(|| usage()))),
            "--prism" => args.prism = Some(PathBuf::from(it.next().unwrap_or_else(|| usage()))),
            "--dry-run" => args.dry_run = true,
            "--allow-real" => args.allow_real = true,
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
    }
    if args.sources.is_empty() || args.target.as_os_str().is_empty() {
        usage();
    }
    args
}

fn db_name(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "userdb".to_string())
}

fn open_readonly(path: &Path) -> Option<LevelDb> {
    let db = LevelDb::new(path.to_path_buf(), &db_name(path), USERDB_TYPE);
    if db.open_readonly() {
        Some(db)
    } else {
        eprintln!("[merge] cannot open (readonly): {}", path.display());
        None
    }
}

/// 解析一条 userdb 记录。
///
/// key 形如 `"ni hao \t你好"`（音节串可能带尾随空格），value 是
/// [`UserDbValue`] 的两种格式之一（`c=.. d=.. t=..` 或五字段 CSV）。
fn parse_entry(
    key: &str,
    value: &str,
    syllabary: Option<&Syllabary>,
) -> Result<Option<UserDbValue>, &'static str> {
    let Some((code, word)) = key.split_once('\t') else {
        return Err("key_without_tab");
    };
    let syllables = code.trim();
    if syllables.is_empty() || word.trim().is_empty() {
        return Err("empty_field");
    }
    if let Some(set) = syllabary {
        for syllable in syllables.split_whitespace() {
            if !set.contains(syllable) {
                return Err("syllable_not_in_prism");
            }
        }
    }
    let value = UserDbValue::unpack(value).ok_or("bad_value")?;
    if value.commits < 0 {
        // 删除标记：不迁移（迁移只做"把旧词带过来"，不把旧删除扩散到新库）。
        return Ok(None);
    }
    Ok(Some(value))
}

fn merge_value(ours: &mut Merged, theirs: UserDbValue) {
    if ours.value.commits.abs() < theirs.commits.abs() {
        ours.value.commits = theirs.commits;
    }
    ours.value.dee = ours.value.dee.max(theirs.dee);
    ours.value.tick = ours.value.tick.max(theirs.tick);
    if theirs.manual_stamp > ours.value.manual_stamp {
        ours.value.position = theirs.position;
        ours.value.manual_stamp = theirs.manual_stamp;
    }
}

fn insert(
    map: &mut BTreeMap<String, Merged>,
    key: String,
    value: UserDbValue,
    source: &str,
) -> bool {
    match map.get_mut(&key) {
        Some(existing) => {
            merge_value(existing, value);
            if !existing.sources.iter().any(|s| s == source) {
                existing.sources.push(source.to_string());
            }
            false // 重复
        }
        None => {
            map.insert(
                key,
                Merged {
                    value,
                    sources: vec![source.to_string()],
                },
            );
            true // 新键
        }
    }
}

fn load_db(
    path: &Path,
    map: &mut BTreeMap<String, Merged>,
    stats: &mut Stats,
    syllabary: Option<&Syllabary>,
) -> (usize, usize) {
    let Some(db) = open_readonly(path) else {
        return (0, 0);
    };
    let Some(mut source) = DbSource::new(&db) else {
        eprintln!("[merge] no data cursor in {}", path.display());
        db.close();
        return (0, 0);
    };
    let label = db_name(path);
    let mut fresh = 0usize;
    let mut duplicate = 0usize;
    while let Some((key, value)) = source.get() {
        stats.read += 1;
        match parse_entry(&key, &value, syllabary) {
            Ok(Some(parsed)) => {
                stats.valid += 1;
                if insert(map, key, parsed, &label) {
                    fresh += 1;
                } else {
                    duplicate += 1;
                }
            }
            Ok(None) => stats.deleted += 1,
            Err(reason) => stats.fail(reason, &key),
        }
    }
    drop(source);
    db.close();
    (fresh, duplicate)
}

fn main() {
    let args = parse_args();

    // 防呆：默认拒写真实用户目录（必须显式 `--allow-real`）。
    if !args.dry_run && !args.allow_real {
        let target = args.target.to_string_lossy().to_lowercase();
        if target.contains("\\appdata\\roaming\\rime-next\\") {
            eprintln!(
                "[merge] refusing to write inside %APPDATA%\\Rime-Next without --allow-real: {}",
                args.target.display()
            );
            std::process::exit(3);
        }
    }

    let syllabary: Option<Syllabary> = match &args.prism {
        Some(path) => match Prism::open(path.clone()).and_then(|mut prism| {
            prism.load()?;
            Ok(prism)
        }) {
            Ok(prism) => {
                let set = prism.syllabary_set();
                if set.is_none() {
                    eprintln!(
                        "[merge] prism has no syllabary table (v5?); syllable validation disabled"
                    );
                }
                set
            }
            Err(error) => {
                eprintln!("[merge] cannot load prism {}: {error}", path.display());
                std::process::exit(2);
            }
        },
        None => None,
    };

    let mut merged: BTreeMap<String, Merged> = BTreeMap::new();

    // 目标库先读进来：迁移是"并入"，已有条目按同一套规则参与合并。
    let mut target_stats = Stats::default();
    load_db(
        &args.target,
        &mut merged,
        &mut target_stats,
        syllabary.as_ref(),
    );
    let target_before = merged.len();
    if target_stats.read > 0 {
        println!(
            "[merge] target existing: {}",
            target_stats.summary()
        );
    }

    for path in &args.sources {
        let mut stats = Stats::default();
        let (fresh, duplicate) = load_db(path, &mut merged, &mut stats, syllabary.as_ref());
        println!(
            "[merge] source {}: {} (new={fresh} duplicate={duplicate})",
            path.display(),
            stats.summary()
        );
    }

    let after = merged.len();
    let collisions: usize = merged.values().filter(|m| m.sources.len() > 1).count();
    println!("[merge] target {} before={target_before}", args.target.display());
    println!(
        "[merge] merged keys={after} collisions={collisions} (entries carried by >1 source)"
    );

    if let Some(path) = &args.export {
        let mut out = String::from(
            "syllables\tword\tcommits\tdee\ttick\tposition\tmanual_stamp\tsources\n",
        );
        for (key, entry) in &merged {
            let (code, word) = key.split_once('\t').unwrap_or((key.as_str(), ""));
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                code.trim(),
                word.trim(),
                entry.value.commits,
                entry.value.dee,
                entry.value.tick,
                entry.value.position,
                entry.value.manual_stamp,
                entry.sources.join(",")
            ));
        }
        match std::fs::write(path, out) {
            Ok(()) => println!("[merge] exported {} entries to {}", after, path.display()),
            Err(error) => eprintln!("[merge] export failed {}: {error}", path.display()),
        }
    }

    if args.dry_run {
        println!("[merge] dry-run: target untouched (would end with {after} entries)");
        return;
    }

    // 写入一个**全新的** LevelDB，再整目录替换目标。
    //
    // 为什么不直接往旧库 `put`：旧 `.userdb` 的 WAL 尾部可能是半条记录
    // （从正在运行的 Weasel 复制快照时尤其常见），rusty-leveldb 的 recovery
    // 读到断点就停，后续追加的记录永远读不回来 —— 演练里表现为
    // "live=1568 reopened=427"。写全新库既绕开损坏的日志，也天然不会和
    // 正在运行的实例抢同一个 LOCK。
    let mut tmp = args.target.clone();
    tmp.set_file_name(format!(
        "{}.merge-tmp",
        args.target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "userdb".to_string())
    ));
    if tmp.exists() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    let fresh = LevelDb::new(tmp.clone(), &db_name(&args.target), USERDB_TYPE);
    if !fresh.open() {
        eprintln!("[merge] cannot create staging db: {}", tmp.display());
        std::process::exit(4);
    }
    let max_tick = merged.values().map(|m| m.value.tick).max().unwrap_or(0);
    let mut written = 0usize;
    for (key, entry) in &merged {
        if fresh.update(key, &entry.value.pack()) {
            written += 1;
        } else {
            eprintln!("[merge] write failed: {}", key.replace('\t', "\\t"));
        }
    }
    // tick 抬到合并后的最大值，避免旧 tick 让新写入的权重立刻被衰减掉。
    if max_tick > 0 {
        fresh.meta_update("/tick", &max_tick.to_string());
    }
    drop(fresh);

    // 自检：重开新库再数一遍，数量对不上就中止，旧库原封不动。
    let mut reopened = BTreeMap::new();
    let mut reopened_stats = Stats::default();
    load_db(&tmp, &mut reopened, &mut reopened_stats, None);
    if reopened.len() != after {
        eprintln!(
            "[merge] self-check failed: staged={} expected={after}; target left untouched, staging kept at {}",
            reopened.len(),
            tmp.display()
        );
        std::process::exit(5);
    }

    // 备份并替换：旧库改名（不删），新库就位。
    if args.target.exists() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut backup = args.target.clone();
        backup.set_file_name(format!(
            "{}.bak-{stamp}",
            args.target
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "userdb".to_string())
        ));
        if let Err(error) = std::fs::rename(&args.target, &backup) {
            eprintln!(
                "[merge] cannot move old target aside ({}): {error}",
                backup.display()
            );
            std::process::exit(6);
        }
        println!("[merge] old target kept at {}", backup.display());
    }
    if let Err(error) = std::fs::rename(&tmp, &args.target) {
        eprintln!("[merge] cannot move staging into place: {error}");
        std::process::exit(6);
    }
    println!(
        "[merge] wrote {written}/{after} entries to {} (tick={max_tick}); self-check reopened={}",
        args.target.display(),
        reopened.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(commits: i32, dee: f64, tick: u64, position: i32, manual_stamp: u64) -> UserDbValue {
        UserDbValue {
            commits,
            dee,
            tick,
            position,
            manual_stamp,
        }
    }

    fn syllabary() -> Syllabary {
        let mut set = Syllabary::new();
        for syllable in ["ni", "hao", "zhong"] {
            set.insert(syllable);
        }
        set
    }

    #[test]
    fn rejects_keys_without_tab() {
        assert_eq!(
            parse_entry("nihao", "c=1 d=1 t=1", None).unwrap_err(),
            "key_without_tab"
        );
    }

    #[test]
    fn rejects_empty_fields() {
        assert_eq!(
            parse_entry("\t你好", "c=1 d=1 t=1", None).unwrap_err(),
            "empty_field"
        );
        assert_eq!(
            parse_entry("ni hao \t", "c=1 d=1 t=1", None).unwrap_err(),
            "empty_field"
        );
    }

    #[test]
    fn rejects_bad_value() {
        // 五字段形状但字段数/数值非法 → `unpack` 严格返回 None。
        assert_eq!(
            parse_entry("ni hao \t你好", "1,2,3", None).unwrap_err(),
            "bad_value"
        );
        assert_eq!(
            parse_entry("ni hao \t你好", "1,abc,3,0,0", None).unwrap_err(),
            "bad_value"
        );
    }

    #[test]
    fn rejects_syllables_outside_prism() {
        let set = syllabary();
        assert_eq!(
            parse_entry("ni nihao \t你好", "c=1 d=1 t=1", Some(&set)).unwrap_err(),
            "syllable_not_in_prism"
        );
        assert!(parse_entry("ni hao \t你好", "c=1 d=1 t=1", Some(&set)).is_ok());
    }

    #[test]
    fn negative_commits_is_deletion_marker_not_an_error() {
        assert_eq!(
            parse_entry("ni hao \t你好", "c=-2 d=1 t=1", None).unwrap(),
            None
        );
    }

    #[test]
    fn merge_takes_abs_commits_max_dee_tick_and_newer_manual() {
        let mut ours = Merged {
            value: value(3, 1.5, 100, 2, 7),
            sources: vec!["a".into()],
        };
        merge_value(&mut ours, value(-5, 2.5, 120, 0, 9));
        assert_eq!(ours.value.commits, -5, "commits 取绝对值大的（含删除标记）");
        assert_eq!(ours.value.dee, 2.5);
        assert_eq!(ours.value.tick, 120);
        assert_eq!(ours.value.position, 0, "更新的手排戳覆盖旧的");
        assert_eq!(ours.value.manual_stamp, 9);
    }
}
