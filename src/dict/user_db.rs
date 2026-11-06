//! librime `dict/user_db.h`(142) + `user_db.cc`(270) 的移植：用户词典的**值格式**与工具集。
//!
//! # 值格式（`user_db.cc:18-56`）
//!
//! userdb 的键是 `"<码串> <TAB> <词条>"`，值是三个字段的空格分隔串：
//!
//! ```text
//! c=<commits> d=<dee> t=<tick>
//! ```
//!
//! | 字段 | 含义 |
//! |---|---|
//! | `c` | 累计选词次数。**负数 = 已被用户删除**（`CreateDictEntry` 见到负值直接丢弃） |
//! | `d` | 衰减后的词频权重（`algo::dynamics::formula_d` 迭代出来的），上限 10000 |
//! | `t` | 最后一次改动的 tick（`UserDictionary::tick_` 的快照），用来算衰减量 |
//!
//! **`pack` 的浮点写法与 C++ 一致**：C++ 用 `ostream <<` 的默认格式
//! （`%g` + 精度 6，例如 `d=3.50902`、`d=1`），Rust 的 `Display` 会写出
//! `3.5090199999999997` 这种往返精确值。两种写法对方的解析器都吃得下，
//! 但落盘字节和 `.userdb.txt` 快照的 diff 会全是噪声 —— 所以这里复刻 `%g`，
//! 见 [`format_g6`]。
//!
//! # 快照格式（`.userdb.txt`）
//!
//! 用户词典的"可读快照"是 TSV：`<码串（尾随一个空格）>\t<词条>\t<值>`。
//! 键的第一列**必须**以空格结尾（`userdb_entry_parser` 会补，兼容旧版 bug 写出的
//! 少空格键），否则拆键拆不开。
//!
//! # 工具集
//!
//! | C++ | Rust |
//! |---|---|
//! | `UserDb::snapshot_extension()` | [`snapshot_extension`] |
//! | `UserDbHelper` | [`UserDbHelper`] |
//! | `UserDbMerger` | [`UserDbMerger`] |
//! | `UserDbImporter` | [`UserDbImporter`] |
//!
//! `UserDb` 本身在 C++ 里是个**空壳类**（只有 `Component` 和 `snapshot_extension`），
//! 真正干活的是模板 `UserDbWrapper<BaseDb>`。Rust 没有模板特化那套，等价物是
//! `dict::level_db::LevelDb::new_user_db()`（= `UserDbWrapper<LevelDb>`）。

use std::path::Path;

use super::db::{Db, DbSink, DbSource};
use super::tsv::{Tsv, TsvReader, TsvSink, TsvSource, TsvWriter};
use crate::algo::dynamics::formula_d;

/// `UserDb::snapshot_extension()`：可读快照的后缀（`user_db.cc:58`）。
pub const SNAPSHOT_EXTENSION: &str = ".userdb.txt";

/// `struct UserDbState`：一条用户词典记录（evo 起是**五个**字段）。
///
/// | 字段 | 含义 |
/// |---|---|
/// | `c` | 累计选词次数。**负数 = 已被用户删除**（`CreateDictEntry` 见到负值直接丢弃） |
/// | `d` | 衰减后的词频权重（`algo::dynamics::formula_d` 迭代出来的），上限 10000 |
/// | `t` | 最后一次改动的 tick（`UserDictionary::tick_` 的快照），用来算衰减量 |
/// | `p` | **手排位置**（1 基，0 = 没手排过）。用户把候选拖到固定位置时写 |
/// | `m` | **手排名次戳**（`manual_stamp`）。两处手排冲突时"新的覆盖旧的" |
///
/// 上游 librime 只有前三字段（`UserDbValue`）。evo 把 `p`/`m` **追加**在后面而不是
/// 换格式：旧库照样能读（缺的字段按 0 处理），新库被旧版读也只是丢掉手排信息。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UserDbValue {
    pub commits: i32,
    pub dee: f64,
    pub tick: u64,
    /// 手排位置（1 基）；0 = 自动排序。
    pub position: i32,
    /// 手排时间戳（用于合并时定胜负；0 = 没有手排过）。
    pub manual_stamp: u64,
}

/// `d` 字段的上限（`user_db.cc:42` 的 `std::min(10000.0, ...)`）。
const K_MAX_DEE: f64 = 10000.0;

/// C++ `ostream <<` 的默认浮点格式：`%g`，精度 6。
///
/// 先按 `%e`（精度 5）定出**四舍五入后**的十进制指数 `X`，再按 `%g` 的两条规则选写法：
///
/// * `-4 <= X < 6` → 定点 `%f`，小数位 `6-1-X`；
/// * 否则 → 科学计数 `%e`，指数带符号补两位（`e+06` / `e-05`）；
///
/// 最后去掉小数部分末尾的 `0`（连同孤立的小数点）。
///
/// 这些字符串直接来自真实 userdb：`c=21 d=3.50902 t=3402`、`c=1 d=1 t=1258`。
fn format_g6(value: f64) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    if !value.is_finite() {
        // C++ 写 "inf" / "nan"；Rust 的 Display 也是，但大小写不同，这里对齐 C++
        return if value.is_nan() {
            "nan".to_string()
        } else if value > 0.0 {
            "inf".to_string()
        } else {
            "-inf".to_string()
        };
    }
    let scientific = format!("{value:.5e}");
    let (mantissa, exponent) = scientific.split_once('e').expect("LowerExp 必带 e");
    let exponent: i32 = exponent.parse().expect("指数必是整数");
    if !(-4..6).contains(&exponent) {
        let mantissa = trim_fraction_zeros(mantissa);
        let sign = if exponent < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exponent.abs());
    }
    let decimals = (5 - exponent).max(0) as usize;
    trim_fraction_zeros(&format!("{value:.decimals$}"))
}

/// 去掉小数部分末尾的 `0`，以及只剩小数点时的那个点。
fn trim_fraction_zeros(text: &str) -> String {
    if !text.contains('.') {
        return text.to_string();
    }
    let trimmed = text.trim_end_matches('0');
    trimmed.trim_end_matches('.').to_string()
}

impl UserDbValue {
    pub fn new(commits: i32, dee: f64, tick: u64) -> Self {
        Self {
            commits,
            dee,
            tick,
            position: 0,
            manual_stamp: 0,
        }
    }

    /// 手排过（位置 > 0 且有时间戳）。
    pub fn has_manual_position(&self) -> bool {
        self.manual_stamp != 0 && self.position > 0
    }

    /// `SetManualVersion(position, version)`：写手排位置 + 戳。
    pub fn set_manual_version(&mut self, position: i32, version: u64) {
        self.position = position.max(0);
        self.manual_stamp = version;
    }

    /// 清手排（位置归 0，但**保留戳**——清空本身也要参与合并的"新覆盖旧"判定）。
    pub fn reset_manual_position(&mut self, version: u64) {
        self.position = 0;
        self.manual_stamp = version;
    }

    /// `ResetAutoLearning`：只抹掉自动学习（`c`/`d`），保留手排与 tick。
    pub fn reset_auto_learning(&mut self) {
        self.commits = -1;
        self.dee = 0.0;
    }

    /// `UserDbValue::Pack()`。
    ///
    /// **格式（librime-evo）**：五个字段、**逗号**分隔、无键名：
    ///
    /// ```text
    /// 之前（上游 librime）: c=21 d=3.50902 t=3402
    /// 之后（五字段 CSV）  : 21,3.50902,3402,2,6fn3k
    /// ```
    ///
    /// 末尾的 `m` 是 36 进制短码（[`crate::dict::manual_stamp`]）。
    /// 手排字段没设也照样写出 `,0,0`：同步时要能表达"我把它清空了"，
    /// 省掉字段只会被读成 0，读不出"是谁清的"。
    pub fn pack(&self) -> String {
        format!(
            "{},{},{},{},{}",
            self.commits,
            format_g6(self.dee),
            self.tick,
            self.position,
            crate::dict::manual_stamp::format(self.manual_stamp)
        )
    }

    /// `UserDbValue::Unpack(value)`：两种格式都能读。
    ///
    /// * **旧格式**（上游三字段 `c=1 d=2 t=3`）→ 手排字段按 0；
    /// * **新格式**（五字段 CSV）→ 严格校验，任何字段非法都返回 `None`
    ///   （evo 的 `UserDbState::Unpack` 就是这么判的；宽松解析会让一条坏记录
    ///   污染整个词的排序）。
    ///
    /// 与 C++ 的细节差异：`d` 解析失败取 `0.0`（C++ 只对 `out_of_range` 这么做）。
    /// 实际落盘数据里不会出现非数字，这里取更宽松的一支。
    pub fn unpack(value: &str) -> Option<Self> {
        if let Some(five) = Self::unpack_compact(value) {
            return Some(five);
        }
        if Self::looks_compact(value) {
            // 看起来是五字段格式但解析失败 —— 不能退回键值解析（那会读出全 0）
            return None;
        }
        let mut result = Self::default();
        for token in value.split(' ') {
            let Some((key, raw)) = token.split_once('=') else {
                continue;
            };
            match key {
                "c" => result.commits = raw.parse::<i32>().ok()?,
                "d" => {
                    result.dee = raw
                        .parse::<f64>()
                        .map(|v| v.min(K_MAX_DEE))
                        .unwrap_or(0.0);
                }
                "t" => result.tick = raw.parse::<u64>().ok()?,
                // 兼容"半个升级"的库：键值形式带上了 p / m
                "p" => result.position = raw.parse::<i32>().unwrap_or(0).max(0),
                "m" => result.manual_stamp = crate::dict::manual_stamp::parse(raw).unwrap_or(0),
                _ => {}
            }
        }
        Some(result)
    }

    /// 五字段 CSV 的解析（evo `UserDbState::Unpack`）。
    fn unpack_compact(value: &str) -> Option<Self> {
        // 长度上限照抄 C++（120），挡住整段乱码被逐字段解析
        if value.is_empty() || value.len() > 120 || !value.contains(',') {
            return None;
        }
        let fields: Vec<&str> = value.split(',').collect();
        if fields.len() != 5 {
            return None;
        }
        let commits = fields[0].parse::<i32>().ok()?;
        let dee = fields[1].parse::<f64>().ok()?;
        if !dee.is_finite() || dee < 0.0 || dee > K_MAX_DEE {
            return None;
        }
        let tick = fields[2].parse::<u64>().ok()?;
        let position = fields[3].parse::<i32>().ok()?;
        if position < 0 {
            return None;
        }
        let manual_stamp = crate::dict::manual_stamp::parse(fields[4])?;
        // 有位置必须有版本（否则合并时无从比较）
        if manual_stamp == 0 && position != 0 {
            return None;
        }
        Some(Self {
            commits,
            dee,
            tick,
            position,
            manual_stamp,
        })
    }

    /// 是否"看起来是"五字段格式（用于区分"坏的五字段"和"三字段键值"）。
    fn looks_compact(value: &str) -> bool {
        value.contains(',') && !value.contains('=')
    }

    /// 是否为上游三字段格式（键值 `c= d= t=`，不含逗号）。
    pub fn is_legacy_format(value: &str) -> bool {
        !value.contains(',')
    }
}

/// `userdb_entry_parser`：TSV 行 → `(键, 值)`。
///
/// 键的第一列是码串，**必须以一个空格收尾**（`user_db.cc:76-79` 说旧版 bug 会漏写，
/// 这里补上）。第二列是词条。
pub fn userdb_entry_parser(row: &Tsv) -> Option<(String, String)> {
    if row.len() < 2 || row[0].is_empty() || row[1].is_empty() {
        return None;
    }
    let mut code = row[0].clone();
    if !code.ends_with(' ') {
        code.push(' ');
    }
    let key = format!("{code}\t{}", row[1]);
    let value = row.get(2).cloned().unwrap_or_default();
    Some((key, value))
}

/// `userdb_entry_formatter`：`(键, 值)` → TSV 行。
///
/// 键必须恰好是两段（码串 + 词条），否则这条写不出去。
pub fn userdb_entry_formatter(key: &str, value: &str) -> Option<Tsv> {
    let row = crate::algo::strings::split(key, "\t", crate::algo::strings::SplitBehavior::KeepToken);
    if row.len() != 2 || row[0].is_empty() || row[1].is_empty() {
        return None;
    }
    let mut row = row;
    row.push(value.to_string());
    Some(row)
}

/// 纯文本用户词典的格式描述（`user_db.cc:98-102` 的 `plain_userdb_format`）。
pub const PLAIN_FILE_DESCRIPTION: &str = "Rime user dictionary";

/// `UserDbHelper`：挂在一个 [`Db`] 上的零散动作。
pub struct UserDbHelper<'a> {
    db: &'a dyn Db,
}

impl<'a> UserDbHelper<'a> {
    pub fn new(db: &'a dyn Db) -> Self {
        Self { db }
    }

    /// `UserDbHelper::UpdateUserInfo()`：记下是谁在用这份词典（`/user_id`）。
    pub fn update_user_info(&self, user_id: &str) -> bool {
        self.db.meta_update("/user_id", user_id)
    }

    /// `UserDbHelper::IsUniformFormat(file_path)`：是不是可读快照（`*.userdb.txt`）。
    pub fn is_uniform_format(file_path: &Path) -> bool {
        file_path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(SNAPSHOT_EXTENSION))
    }

    /// `UserDbHelper::UniformBackup(snapshot_file)`：把 db 摊成 `.userdb.txt`。
    pub fn uniform_backup(&self, snapshot_file: &Path) -> bool {
        let Some(mut source) = DbSource::new(self.db) else {
            return false;
        };
        let writer = TsvWriter::new(
            snapshot_file,
            Box::new(|key: &str, value: &str| userdb_entry_formatter(key, value)),
        );
        let mut writer = writer;
        writer.file_description = PLAIN_FILE_DESCRIPTION.to_string();
        match writer.write(&mut source) {
            Ok(_) => true,
            Err(e) => {
                eprintln!("[user_db] failed to write snapshot {snapshot_file:?}: {e}");
                false
            }
        }
    }

    /// `UserDbHelper::UniformRestore(snapshot_file)`：从 `.userdb.txt` 灌回去。
    pub fn uniform_restore(&self, snapshot_file: &Path) -> bool {
        let mut sink = DbSink::new(self.db);
        match TsvReader::new(snapshot_file).read(&mut sink, userdb_entry_parser) {
            Ok(_) => true,
            Err(e) => {
                eprintln!("[user_db] failed to read snapshot {snapshot_file:?}: {e}");
                false
            }
        }
    }

    /// `UserDbHelper::IsUserDb()`：`/db_type == "userdb"`。
    pub fn is_user_db(&self) -> bool {
        self.db.meta_fetch("/db_type").as_deref() == Some("userdb")
    }

    /// `UserDbHelper::GetDbName()`：`/db_name` 砍掉 `.userdb` 及其后缀。
    ///
    /// 例：`"luna_pinyin.userdb.kct.snapshot"` → `"luna_pinyin"`。
    pub fn get_db_name(&self) -> Option<String> {
        let mut name = self.db.meta_fetch("/db_name")?;
        if let Some(pos) = name.rfind(".userdb") {
            name.truncate(pos);
        }
        Some(name)
    }

    /// `UserDbHelper::GetUserId()`：缺省 `"unknown"`。
    pub fn get_user_id(&self) -> String {
        self.db
            .meta_fetch("/user_id")
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// `UserDbHelper::GetRimeVersion()`。
    pub fn get_rime_version(&self) -> Option<String> {
        self.db.meta_fetch("/rime_version")
    }
}

/// `get_tick_count(db)`：读 `/tick`，读不到就当 1（`user_db.cc:176-185`）。
fn get_tick_count(db: &dyn Db) -> u64 {
    db.meta_fetch("/tick")
        .and_then(|tick| tick.parse::<u64>().ok())
        .unwrap_or(1)
}

/// `UserDbMerger`：把别人的 userdb 合并进来（同步功能的核心）。
///
/// 合并规则（`user_db.cc:208-228`）：两边的 `dee` 各自按"自己的 tick 到现在"衰减一次，
/// 取最大值；`commits` 取绝对值大的；`tick` 统一成两边的大者。
pub struct UserDbMerger<'a> {
    db: &'a dyn Db,
    our_tick: u64,
    their_tick: u64,
    max_tick: u64,
    merged_entries: usize,
}

impl<'a> UserDbMerger<'a> {
    pub fn new(db: &'a dyn Db) -> Self {
        let our_tick = get_tick_count(db);
        Self {
            db,
            our_tick,
            their_tick: 0,
            max_tick: our_tick,
            merged_entries: 0,
        }
    }

    /// `UserDbMerger::CloseMerge()`：把合并后的 tick 与 user_id 落盘。
    pub fn close_merge(&mut self, user_id: &str) -> bool {
        if self.merged_entries == 0 {
            return false;
        }
        let ok = self.db.meta_update("/tick", &self.max_tick.to_string())
            && self.db.meta_update("/user_id", user_id);
        self.merged_entries = 0;
        ok
    }

    /// 已合并条数（测试与日志用；C++ 里是 protected）。
    pub fn merged_entries(&self) -> usize {
        self.merged_entries
    }

    /// 合并后的 tick（测试与日志用）。
    pub fn max_tick(&self) -> u64 {
        self.max_tick
    }
}

impl TsvSink for UserDbMerger<'_> {
    fn meta_put(&mut self, key: &str, value: &str) -> bool {
        if key == "/tick" {
            if let Ok(tick) = value.parse::<u64>() {
                self.their_tick = tick;
                self.max_tick = self.our_tick.max(self.their_tick);
            }
        }
        true
    }

    fn put(&mut self, key: &str, value: &str) -> bool {
        let Some(mut theirs) = UserDbValue::unpack(value) else {
            return false;
        };
        if theirs.tick < self.their_tick {
            theirs.dee = formula_d(0.0, self.their_tick as f64, theirs.dee, theirs.tick as f64);
        }
        let mut ours = self
            .db
            .fetch(key)
            .and_then(|value| UserDbValue::unpack(&value))
            .unwrap_or_default();
        if ours.tick < self.our_tick {
            ours.dee = formula_d(0.0, self.our_tick as f64, ours.dee, ours.tick as f64);
        }
        if ours.commits.abs() < theirs.commits.abs() {
            ours.commits = theirs.commits;
        }
        ours.dee = ours.dee.max(theirs.dee);
        ours.tick = self.max_tick;
        let ok = self.db.update(key, &ours.pack());
        if ok {
            self.merged_entries += 1;
        }
        ok
    }
}

/// `UserDbImporter`：从快照导入（不衰减、不合并 tick）。
///
/// 规则（`user_db.cc:252-268`）：`commits > 0` 时取 `max`；`commits < 0` 表示"标记删除"，
/// 把结果压成 `-1` 或原负数中更小的那个。
pub struct UserDbImporter<'a> {
    db: &'a dyn Db,
    imported_entries: usize,
}

impl<'a> UserDbImporter<'a> {
    pub fn new(db: &'a dyn Db) -> Self {
        Self {
            db,
            imported_entries: 0,
        }
    }

    pub fn imported_entries(&self) -> usize {
        self.imported_entries
    }
}

impl TsvSink for UserDbImporter<'_> {
    fn meta_put(&mut self, _key: &str, _value: &str) -> bool {
        true
    }

    fn put(&mut self, key: &str, value: &str) -> bool {
        let Some(incoming) = UserDbValue::unpack(value) else {
            return false;
        };
        let mut ours = self
            .db
            .fetch(key)
            .and_then(|value| UserDbValue::unpack(&value))
            .unwrap_or_default();
        if incoming.commits > 0 {
            ours.commits = ours.commits.max(incoming.commits);
            ours.dee = ours.dee.max(incoming.dee);
        } else if incoming.commits < 0 {
            ours.commits = incoming.commits.min(-ours.commits.abs());
        }
        let ok = self.db.update(key, &ours.pack());
        if ok {
            self.imported_entries += 1;
        }
        ok
    }
}

/// 快照读侧的便捷入口：把 `.userdb.txt` 灌进一个 `TsvSink`。
///
/// C++ 没有这个函数（调用方自己 `TsvReader reader(file, parser); reader >> sink;`），
/// 这里包一层是因为 Rust 的 `TsvReader::read` 还要传 parser，两处调用点都想少写一遍。
pub fn read_snapshot(
    snapshot_file: &Path,
    sink: &mut dyn TsvSink,
) -> std::io::Result<usize> {
    TsvReader::new(snapshot_file).read(sink, userdb_entry_parser)
}

/// 让 `DbSource` 能直接喂给 [`TsvWriter`]。
impl TsvSource for DbSource {
    fn meta_get(&mut self) -> Option<(String, String)> {
        DbSource::meta_get(self)
    }

    fn get(&mut self) -> Option<(String, String)> {
        DbSource::get(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_round_trip() {
        let value = UserDbValue::new(3, 12.5, 42);
        // 五字段 CSV（librime-evo）：手排字段没设也照常写出 ",0,0"
        assert_eq!(value.pack(), "3,12.5,42,0,0");
        assert_eq!(UserDbValue::unpack("3,12.5,42,0,0"), Some(value));
    }

    /// 旧库（上游三字段）照样读得出来，手排字段按 0 处理。
    #[test]
    fn legacy_three_field_values_still_parse() {
        let value = UserDbValue::unpack("c=21 d=3.50902 t=3402").unwrap();
        assert_eq!(value.commits, 21);
        assert!((value.dee - 3.50902).abs() < 1e-9);
        assert_eq!(value.tick, 3402);
        assert_eq!(value.position, 0, "三字段没有手排信息");
        assert_eq!(value.manual_stamp, 0);
        assert!(UserDbValue::is_legacy_format("c=21 d=3.50902 t=3402"));
        assert!(!UserDbValue::is_legacy_format("21,3.50902,3402,2,6fn3k"));
    }

    /// 手排记录：位置与 36 进制短码都读得回来。
    #[test]
    fn manual_fields_round_trip() {
        let raw = "5,2.5,900,2,6fn3k";
        let value = UserDbValue::unpack(raw).unwrap();
        assert_eq!(value.position, 2);
        assert_eq!(
            value.manual_stamp,
            crate::dict::manual_stamp::parse("6fn3k").unwrap()
        );
        assert!(value.has_manual_position());
        assert_eq!(value.pack(), raw);
    }

    /// 坏的新格式**不能**退化成键值解析（否则读出一堆 0 把词顶到后面）。
    #[test]
    fn broken_compact_format_is_rejected() {
        // 有逗号没有等号 ⇒ 想写新格式但字段数不对
        assert_eq!(UserDbValue::unpack("1,2,3"), None);
        // 位置非零却没有版本戳
        assert_eq!(UserDbValue::unpack("1,2,3,2,0"), None);
        // 版本戳不是 36 进制短码
        assert_eq!(UserDbValue::unpack("1,2,3,0,ZZZ"), None);
        // 词频超上限
        assert_eq!(UserDbValue::unpack("1,99999,3,0,0"), None);
    }

    /// `format_g6` 必须逐字节对齐 C++ `ostream <<`（`%g`，精度 6）。
    ///
    /// 期望值取自真实 userdb 里 C++ 写出来的字符串。
    #[test]
    fn format_g6_matches_cpp_ostream() {
        // 真实数据里出现过的三种
        assert_eq!(format_g6(3.50902), "3.50902");
        assert_eq!(format_g6(1.88443), "1.88443");
        assert_eq!(format_g6(1.0), "1");
        // 整数 / 小数 / 截断
        assert_eq!(format_g6(0.0), "0");
        assert_eq!(format_g6(12.5), "12.5");
        assert_eq!(format_g6(10000.0), "10000");
        assert_eq!(format_g6(0.1 + 0.2), "0.3");
        // 6 位有效数字上限
        assert_eq!(format_g6(123456.0), "123456");
        // 越界走科学计数（C++ 指数补两位带符号）
        assert_eq!(format_g6(1234567.0), "1.23457e+06");
        assert_eq!(format_g6(0.00001), "1e-05");
    }

    #[test]
    fn unpack_tolerates_missing_and_extra_fields() {
        // C++ 对缺失字段保持默认值，对不认识的键直接跳过
        assert_eq!(
            UserDbValue::unpack("c=5"),
            Some(UserDbValue::new(5, 0.0, 0))
        );
        assert_eq!(
            UserDbValue::unpack("c=5 x=9 t=7"),
            Some(UserDbValue::new(5, 0.0, 7))
        );
        // 没有 '=' 的 token 忽略
        assert_eq!(
            UserDbValue::unpack("garbage c=1 t=2"),
            Some(UserDbValue::new(1, 0.0, 2))
        );
        // 空串 = 全默认
        assert_eq!(UserDbValue::unpack(""), Some(UserDbValue::default()));
    }

    #[test]
    fn unpack_clamps_dee_to_ten_thousand() {
        // user_db.cc:42 的 min(10000.0, ...)
        let value = UserDbValue::unpack("c=1 d=99999 t=1").unwrap();
        assert_eq!(value.dee, 10000.0);
        // 解析不出数字就当 0
        let value = UserDbValue::unpack("c=1 d=abc t=1").unwrap();
        assert_eq!(value.dee, 0.0);
    }

    #[test]
    fn unpack_rejects_bad_commits_or_tick() {
        assert_eq!(UserDbValue::unpack("c=abc"), None);
        assert_eq!(UserDbValue::unpack("t=-1"), None);
    }

    #[test]
    fn entry_parser_appends_missing_trailing_space() {
        // 老版本 bug 写出的键少了尾随空格，parser 要补
        let row = vec!["nihao".to_string(), "你好".to_string()];
        assert_eq!(
            userdb_entry_parser(&row),
            Some(("nihao \t你好".to_string(), String::new()))
        );

        let row = vec![
            "nihao ".to_string(),
            "你好".to_string(),
            "c=1 d=0 t=1".to_string(),
        ];
        assert_eq!(
            userdb_entry_parser(&row),
            Some(("nihao \t你好".to_string(), "c=1 d=0 t=1".to_string()))
        );

        // 少列 / 空列都拒
        assert_eq!(userdb_entry_parser(&vec!["only".to_string()]), None);
        assert_eq!(
            userdb_entry_parser(&vec![String::new(), "你好".to_string()]),
            None
        );
    }

    #[test]
    fn entry_formatter_requires_exactly_two_key_segments() {
        assert_eq!(
            userdb_entry_formatter("nihao \t你好", "c=1 d=0 t=1"),
            Some(vec![
                "nihao ".to_string(),
                "你好".to_string(),
                "c=1 d=0 t=1".to_string(),
            ])
        );
        // 键里没有 TAB → 只有一段 → 写不出去
        assert_eq!(userdb_entry_formatter("nihao", "c=1"), None);
        // 键里两个 TAB → 三段 → 写不出去
        assert_eq!(userdb_entry_formatter("a\tb\tc", "v"), None);
    }

    #[test]
    fn entry_parser_and_formatter_round_trip() {
        let row = vec![
            "zhong guo ".to_string(),
            "中國".to_string(),
            "c=2 d=5.5 t=9".to_string(),
        ];
        let (key, value) = userdb_entry_parser(&row).unwrap();
        assert_eq!(userdb_entry_formatter(&key, &value), Some(row));
    }

    #[test]
    fn snapshot_extension_matches_cpp() {
        assert_eq!(SNAPSHOT_EXTENSION, ".userdb.txt");
        assert!(UserDbHelper::is_uniform_format(Path::new(
            "/x/luna_pinyin.userdb.txt"
        )));
        assert!(!UserDbHelper::is_uniform_format(Path::new(
            "/x/luna_pinyin.userdb"
        )));
    }
}
