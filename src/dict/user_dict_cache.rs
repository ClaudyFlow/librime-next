//! 用户词典的**内存有序镜像** —— librime PR #1196（`perf(user_dict): in-memory
//! sorted cache for user dictionary queries`）在 LRN 上的落地。
//!
//! ## 为什么要有这一层
//!
//! userdb 的键是 `"<码串> <TAB> <词条>"`，LevelDB 按字节序排键，于是"同一个码串下的
//! 所有词条"天然连续 —— `DfsLookup` 就靠这个性质做前缀扫。但**每按一个键都要走一遍
//! LevelDB**：`Db::query` 开游标 → `seek` → `current/advance`，而且 `LevelDb` 内部是一把
//! 全局 `Mutex`（`level_db.rs` 的 `state`），整条查询路径都在锁里。
//!
//! PR #1196 给的实测：Lookup 从 1.89ms 降到 1.0~1.5us（约 1275~1863 倍），写入基本无损，
//! Load 慢 10%，峰值内存 15.7MB vs 16.1MB。用户词典的量级（几千~几万条）全量驻留内存
//! 完全划得来。
//!
//! ## 为什么用 `BTreeMap` 而不是 PR 里的 per-code sorted vector
//!
//! PR 的做法是"每个码串一个有序 vector + 二分"。Rust 标准库的 `BTreeMap` 本身就是
//! **按键字节序有序**的结构，范围查询 `range(prefix..)` 等价于 LevelDB 的前缀扫，
//! 复杂度同样是 `O(log n + k)`，而且不需要自己维护"每个码串一份 vector"的二级索引。
//! 键直接存**完整的 db key**（`"ni hao \t你好"`），于是：
//!
//! * **PR 评审挑出来的坑 1（code 丢失）不存在**：PR 的缓存条目只填了
//!   `text/commit_count/weight/quality_len`，漏了 `e->code = state->code`，导致缓存路径
//!   返回的用户词 `code` 是空的，再次学习时 `TranslateCodeToString(entry.code)` 还原不出
//!   编码、词频再也涨不上去。这里缓存的是**原始 key/value**，`CreateDictEntry` 照旧从
//!   key 里切出码串，`code` 天然正确 —— 缓存只替掉 IO，不改语义。
//! * **PR 评审挑出来的坑 2（`new_entry_prefix` 不一致）不存在**：PR 把带前缀的 key 插进
//!   db、却把不带前缀的 key 写进缓存，于是"刚学完能看见、重载后消失"。这里缓存的 key
//!   就是**最终写进 db 的那个 key**（含前缀），两边同一个键空间，不会出现分叉。
//!
//! ## 一致性
//!
//! * 打开（`Load`）之后**懒建**：第一次查询时才全量扫一遍 db。启动路径不受影响。
//! * 写路径双写：db 写成功才更新缓存（`update_entry_with_prefix`）。
//! * 缓存被别处借着（有活跃游标）时用 `Arc::get_mut` 会失败，这时直接丢弃缓存，
//!   下次查询重建 —— 宁可重建，也不留一份写不进去的脏镜像。
//! * 元数据（`/tick`、`/db_name`）**不进缓存**，仍走 `Db::meta_fetch/meta_update`。
//! * 开关：`RIME_USER_DICT_CACHE_ENABLED`（`0`/`off`/`false`/`no` 关闭，缺省开启）。
//!   出问题时可以一键退回 IO 路径，不用改代码。

use std::collections::{BTreeMap, Bound};
use std::sync::Arc;

use super::db::{Db, DbAccessor};

/// 共享的用户词典镜像：`db key` → `db value`（`UserDbValue::pack()` 出来的串）。
pub type CacheMap = Arc<BTreeMap<String, String>>;

/// 未提交写入的覆盖层（PR #1196 的 `pending_`）：`db key` → `db value`。
///
/// 学词时先写 db、再把新值记在这里；查询游标同时看 `map` 与 `pending`，
/// `pending` 同键覆盖 `map`。下次查询（`ensure_cache`）会把 `pending` 折叠回 `map`
/// 并清空——**永不出现"写不进去被迫丢弃整份镜像"** 的情况（这正是原 PR 用
/// `pending_` 而非直接改共享镜像的原因）。
pub type PendingMap = Arc<BTreeMap<String, String>>;

/// 元数据键（`/tick` 之类）。元数据仍由 `Db` 自己管，不进缓存。
fn is_metadata_key(key: &str) -> bool {
    key.is_empty() || key.starts_with('/')
}

/// 是否启用内存缓存（`RIME_USER_DICT_CACHE_ENABLED`）。
///
/// 只读一次环境变量，之后是静态布尔 —— 查询在热路径上，不能每次 `env::var`。
pub fn cache_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        match std::env::var("RIME_USER_DICT_CACHE_ENABLED") {
            Ok(value) => !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "off" | "false" | "no" | "disabled"
            ),
            // 缺省开启：这就是 PR 要的收益
            Err(_) => true,
        }
    })
}

/// 全量镜像一个 db 的数据区。
///
/// 用 [`Db::query_all`]（内部已 `Jump(" ")` 跳过元数据区起点）从头扫到尾；
/// 元数据键 (`/xxx`) 的字节序在 `" "` 之后，仍会被扫到，所以这里显式跳过。
/// 返回 `None` = db 没打开（调用方应退回 IO 路径）。
pub fn build_cache(db: &dyn Db) -> Option<CacheMap> {
    if !db.loaded() {
        return None;
    }
    let mut accessor = db.query_all()?;
    let mut entries = BTreeMap::new();
    while let Some((key, value)) = accessor.get_next_record() {
        if is_metadata_key(&key) {
            continue;
        }
        entries.insert(key, value);
    }
    Some(Arc::new(entries))
}

/// 缓存上的**单向前缀游标** —— 行为逐条对齐 `LevelDbAccessor`：
///
/// * [`Self::jump`] 只做 `seek`，不挪前缀；
/// * [`Self::next_record`] 返回当前记录并前进；当前键不以 `prefix` 开头时返回 `None`
///   **且不前进**（调用方可以换前缀再来）；
/// * [`Self::exhausted`] 同理不前进。
///
/// 游标不持有 `&BTreeMap`（那样会把生命周期带进 `DfsState`），而是持有一份
/// `Arc` + 一个"下一个键"的游标串，每次 `range` 定位：`O(log n)`，不分配迭代器。
pub struct CacheScan {
    /// 已提交镜像：`db key` → `db value`。
    map: CacheMap,
    /// 未提交写入覆盖层（PR #1196 的 `pending_`）。
    pending: PendingMap,
    /// 构造时给定的前缀（对应 `LevelDbAccessor::prefix`）。
    prefix: String,
    /// `map` 流游标：`range(map_cursor..)` 的第一个键即"当前记录"。
    map_cursor: String,
    map_live: bool,
    /// `pending` 流游标：`range(pend_cursor..)` 的第一个键。
    pend_cursor: String,
    pend_live: bool,
}

impl CacheScan {
    /// `LevelDbAccessor::new(..., prefix)` + 注入 `pending_` 覆盖层。
    pub fn new(map: CacheMap, pending: PendingMap, prefix: &str) -> Self {
        Self {
            map,
            pending,
            prefix: prefix.to_string(),
            map_cursor: prefix.to_string(),
            map_live: true,
            pend_cursor: prefix.to_string(),
            pend_live: true,
        }
    }

    /// 严格大于 `key` 的最小键：`key + "\0"`（db key 不含 NUL，`\0` 比任何可打印字符都小，
    /// 恰好落在 `key` 与任何以 `key` 为前缀的更长键之间）。
    fn advance(cursor: &mut String, key: &str) {
        cursor.clear();
        cursor.push_str(key);
        cursor.push('\0');
    }

    /// `DbAccessor::Jump(key)`：只 seek，前缀不变；两流同时定位。
    pub fn jump(&mut self, key: &str) -> bool {
        self.map_cursor.clear();
        self.map_cursor.push_str(key);
        self.map_live = true;
        self.pend_cursor.clear();
        self.pend_cursor.push_str(key);
        self.pend_live = true;
        true
    }

    /// `DbAccessor::GetNextRecord`：合并两流，按字节序取最小键；
    /// pending 同键覆盖 map（两流都前进）；越过前缀则到头（不前进）。
    pub fn next_record(&mut self) -> Option<(String, String)> {
        let m = if self.map_live {
            self.map
                .range::<str, _>((Bound::Included(self.map_cursor.as_str()), Bound::Unbounded))
                .next()
                .map(|(k, v)| (k.clone(), v.clone()))
        } else {
            None
        };
        let p = if self.pend_live {
            self.pending
                .range::<str, _>((Bound::Included(self.pend_cursor.as_str()), Bound::Unbounded))
                .next()
                .map(|(k, v)| (k.clone(), v.clone()))
        } else {
            None
        };
        let (key, value, from_pending, both_same) = match (m, p) {
            (None, None) => {
                self.map_live = false;
                self.pend_live = false;
                return None;
            }
            (Some((mk, mv)), None) => (mk, mv, false, false),
            (None, Some((pk, pv))) => (pk, pv, true, false),
            (Some((mk, mv)), Some((pk, pv))) => {
                if pk.as_str() <= mk.as_str() {
                    let both_same = pk == mk;
                    (pk, pv, true, both_same)
                } else {
                    (mk, mv, false, false)
                }
            }
        };
        if !key.starts_with(&self.prefix) {
            self.map_live = false;
            self.pend_live = false;
            return None;
        }
        if from_pending {
            Self::advance(&mut self.pend_cursor, &key);
            if both_same {
                Self::advance(&mut self.map_cursor, &key);
            }
        } else {
            Self::advance(&mut self.map_cursor, &key);
        }
        Some((key, value))
    }

    /// `DbAccessor::exhausted()`：两流都越过前缀才到头。
    pub fn exhausted(&mut self) -> bool {
        let map_done = match self
            .map
            .range::<str, _>((Bound::Included(self.map_cursor.as_str()), Bound::Unbounded))
            .next()
        {
            Some((k, _)) => !k.starts_with(&self.prefix),
            None => true,
        };
        let pend_done = match self
            .pending
            .range::<str, _>((Bound::Included(self.pend_cursor.as_str()), Bound::Unbounded))
            .next()
        {
            Some((k, _)) => !k.starts_with(&self.prefix),
            None => true,
        };
        map_done && pend_done
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
}

/// `DfsLookup` / `LookupWords` 真正用的游标：有缓存就走内存，否则退回 LevelDB。
///
/// 之所以做成 `enum` 而不是 `Box<dyn ...>`：`DbAccessor` 的 `get_next_record` 返回
/// `Option<(String, String)>`，缓存侧也能同样签名，两条路对调用方完全等价。
pub enum EntryCursor {
    /// IO 路径（`RIME_USER_DICT_CACHE_ENABLED=0`，或缓存还没建好）。
    Db(Box<dyn DbAccessor>),
    /// 内存路径。
    Cache(CacheScan),
}

impl EntryCursor {
    /// `DbAccessor::Jump`。
    pub fn jump(&mut self, key: &str) -> bool {
        match self {
            Self::Db(accessor) => accessor.jump(key),
            Self::Cache(scan) => scan.jump(key),
        }
    }

    /// `DbAccessor::GetNextRecord`。
    pub fn next_record(&mut self) -> Option<(String, String)> {
        match self {
            Self::Db(accessor) => accessor.get_next_record(),
            Self::Cache(scan) => scan.next_record(),
        }
    }

    /// `DbAccessor::exhausted()`。
    pub fn exhausted(&mut self) -> bool {
        match self {
            Self::Db(accessor) => accessor.exhausted(),
            Self::Cache(scan) => scan.exhausted(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_of(pairs: &[(&str, &str)]) -> CacheMap {
        Arc::new(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn scan_stops_at_prefix_boundary_without_advancing() {
        let map = map_of(&[
            ("ni \t你", "c=1"),
            ("ni hao \t你好", "c=2"),
            ("ni hao ma \t你好吗", "c=3"),
            ("zhong \t中", "c=4"),
        ]);
        // prefix = "ni hao"：只能看到两条，第三条是 zhong，越界
        let mut scan = CacheScan::new(map, Arc::new(BTreeMap::new()), "ni hao");
        let mut seen = Vec::new();
        while let Some((key, _)) = scan.next_record() {
            seen.push(key);
        }
        assert_eq!(seen, vec!["ni hao \t你好", "ni hao ma \t你好吗"]);

        // 前缀还是 "ni hao"，越界后不前进：换起点再扫还是同样两条
        scan.jump("ni hao ma");
        let mut seen = Vec::new();
        while let Some((key, _)) = scan.next_record() {
            seen.push(key);
        }
        assert_eq!(seen, vec!["ni hao ma \t你好吗"], "从 ma 起只剩一条");
    }

    #[test]
    fn jump_seeks_without_moving_prefix() {
        // 对齐 `LevelDbAccessor::Jump`：只 seek，**不换前缀**。
        // 所以从 "a" 出发 jump 到 "b" 之后，第一条记录不以 "a" 开头 → 立刻到头。
        let map = map_of(&[("a \t阿", "c=1"), ("b \t八", "c=2")]);
        let mut scan = CacheScan::new(map.clone(), Arc::new(BTreeMap::new()), "a");
        assert!(scan.jump("b"));
        assert!(scan.next_record().is_none());
        // 但游标真的挪过去了：换成匹配的前缀再从同一位置继续就能扫到
        let mut scan = CacheScan::new(map, Arc::new(BTreeMap::new()), "");
        assert!(scan.jump("b"));
        assert_eq!(scan.next_record().map(|(k, _)| k), Some("b \t八".to_string()));
        assert!(scan.next_record().is_none(), "b 之后没有别的键了");
    }

    #[test]
    fn empty_prefix_scans_everything() {
        let map = map_of(&[("a \t阿", "c=1"), ("b \t八", "c=2"), ("/tick", "9")]);
        let mut scan = CacheScan::new(map.clone(), Arc::new(BTreeMap::new()), "");
        let mut seen = Vec::new();
        while let Some((key, _)) = scan.next_record() {
            seen.push(key);
        }
        assert_eq!(seen, vec!["/tick", "a \t阿", "b \t八"], "字节序：'/' 在前");
    }

    #[test]
    fn exhausted_matches_leveldb_semantics() {
        let map = map_of(&[("a \t阿", "c=1")]);
        let mut scan = CacheScan::new(map, Arc::new(BTreeMap::new()), "a");
        assert!(!scan.exhausted());
        scan.next_record();
        assert!(scan.exhausted());
        // exhausted 不前进：再来一次仍是 exhausted
        assert!(scan.exhausted());
    }

    #[test]
    fn metadata_keys_are_filtered() {
        assert!(is_metadata_key("/tick"));
        assert!(is_metadata_key(""));
        assert!(!is_metadata_key("ni hao \t你好"));
    }

    #[test]
    fn enabled_flag_defaults_to_true() {
        // 单元测里环境变量多半没设；只要不是显式关闭就是开
        let flag = cache_enabled();
        let expect = match std::env::var("RIME_USER_DICT_CACHE_ENABLED") {
            Ok(value) => !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "off" | "false" | "no" | "disabled"
            ),
            Err(_) => true,
        };
        assert_eq!(flag, expect);
    }
}
