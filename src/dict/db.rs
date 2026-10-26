//! librime `dict/db.h`(119) + `db.cc`(60) + `db_utils.h`(75) + `db_utils.cc`(49) 的移植。
//!
//! # 这一层是什么
//!
//! Rime 的用户词典不是"一个文件"，而是 **"元数据 kv + 数据 kv + 有序游标"** 三件套。
//! 元数据键带前导 `\x01`（[`K_META_CHARACTER`]），数据键就是业务键
//! （userdb 的键是 `"<码串> <TAB> <词条>"`）。游标必须支持
//! **按前缀 forward scan**，因为 `UserDictionary::DfsLookup` 靠它做一遍扫完的遍历。
//!
//! Rust 侧把 C++ 的三个角色拆成三个 trait：
//!
//! | C++ | Rust |
//! |---|---|
//! | `class Db`（纯虚） | [`Db`] |
//! | `class DbAccessor`（纯虚） | [`DbAccessor`] |
//! | `class Transactional` / `class Recoverable` | 同名 trait，由 [`Db::as_transactional`] / [`Db::as_recoverable`] 取出 |
//!
//! # 与 C++ 的接口差异（都是"接缝"，不是语义）
//!
//! * **`&mut self` → `&self`**。C++ 的 `Db::Update` 是普通成员函数，谁都能改；
//!   Rust 侧所有方法都吃 `&self`，可变状态关在实现内部的 `Mutex` 里。这样
//!   `Arc<dyn Db>` 能直接共享给多个 `UserDictionary`（C++ 靠 `an<Db>` + 裸指针）。
//! * **出参 → 返回值**。`GetNextRecord(string* key, string* value)` →
//!   [`DbAccessor::get_next_record`] 返回 `Option<(String, String)>`；
//!   `Fetch(key, string* value)` → [`Db::fetch`] 返回 `Option<String>`。
//! * **`Sink` / `Source`**。C++ 的 `Sink` 是模板基类，`TsvSink` 就是它的一个子类。
//!   Rust 侧不另立炉灶，直接复用 [`crate::dict::tsv::TsvSink`] 当 `Sink`；
//!   `Source` 侧只有一种实现（从 [`Db`] 拉），所以做成具体结构
//!   [`DbSource`]（对应 C++ 的 `DbSource`）。
//! * **`DbComponentBase`**。C++ 靠 `Service::CreateResourceResolver({"db","",""})`
//!   算路径；Rust 侧路径由调用方（`lever::paths::Layout::user_db_dir()`）给定，
//!   这里只提供 [`db_file_path`] 拼接。**不猜目录**，目录是部署层的契约。

use std::path::{Path, PathBuf};

use super::tsv::TsvSink;

/// 元数据键的前导字符（`level_db.cc:17` 的 `kMetaCharacter`）。
///
/// 选 `\x01` 的理由：它比所有可打印 ASCII 都小，所以元数据自然排在数据前面，
/// `QueryAll` 只要从 `" "` 起扫就能跳过它们。
pub const K_META_CHARACTER: &str = "\x01";

/// `rime_api.h` 的 `RIME_VERSION`。写进 db 元数据 `/rime_version`。
///
/// 直接复用 `config` 层那个常量（`1.17.0`），不另立一份 —— 两处不一致会让
/// `__build_info.rime_version` 与 userdb 里的 `/rime_version` 对不上。
pub use crate::config::RIME_VERSION;

/// `class DbAccessor`：有序游标。
///
/// 语义逐条照 `level_db.cc:113-138`：
///
/// * [`Self::reset`] 回到 `prefix` 处的第一条（不是从头！C++ 是 `Jump(prefix_)`）；
/// * [`Self::jump`] 定位到第一个 `>= key` 的键；
/// * [`Self::get_next_record`] 返回**当前位置**的记录再前进；一旦当前键不再以
///   `prefix` 开头就返回 `None`，**且不前进**（这样调用方可以从容换前缀）；
/// * [`Self::exhausted`] 同理不前进。
pub trait DbAccessor {
    /// `DbAccessor::Reset()`。
    fn reset(&mut self) -> bool;
    /// `DbAccessor::Jump(key)`。
    fn jump(&mut self, key: &str) -> bool;
    /// `DbAccessor::GetNextRecord`。返回 `None` = 到头或越过前缀。
    fn get_next_record(&mut self) -> Option<(String, String)>;
    /// `DbAccessor::exhausted()`。
    fn exhausted(&mut self) -> bool;
}

/// `class Transactional`：写事务。
///
/// 用户词典一次输入会话里会写多条（学词 + 记频率 + 改 tick），成批提交省 IO。
/// 方法吃 `&self` 是为了让 [`Db::as_transactional`] 能从 `&dyn Db` 取出来。
pub trait Transactional {
    fn begin_transaction(&self) -> bool;
    fn abort_transaction(&self) -> bool;
    fn commit_transaction(&self) -> bool;
    fn in_transaction(&self) -> bool;
}

/// `class Recoverable`：损坏的 db 尝试修复。
pub trait Recoverable {
    fn recover(&self) -> bool;
}

/// `class Db`：元数据 + 数据 + 游标工厂。
///
/// 需要 `Send + Sync` 是因为 [`crate::c_api`] 的 `Global` 关在一把
/// `static Mutex` 里（`Session` 必须 `Send`）。实现只能靠内部 `Mutex`
/// 保护真实的存储句柄，见 `dict::level_db`。
pub trait Db: Send + Sync {
    /// `Db::name()`。
    fn name(&self) -> &str;
    /// `Db::file_path()`。
    fn file_path(&self) -> &Path;
    /// `Db::loaded()`。
    fn loaded(&self) -> bool;
    /// `Db::readonly()`。
    fn readonly(&self) -> bool;
    /// `Db::disabled()`。被禁用的 db 不打开、不查询（`UserDictionary::Load` 直接 false）。
    fn disabled(&self) -> bool;
    /// `Db::disable()`。
    fn disable(&self);
    /// `Db::enable()`。
    fn enable(&self);

    /// `Db::Exists()`。目录型 db（LevelDB）也存在，所以统一用 `Path::exists`。
    fn exists(&self) -> bool {
        self.file_path().exists()
    }
    /// `Db::Remove()`。`false` = 已打开（C++ 拒绝删除打开中的 db）。
    fn remove(&self) -> bool;
    /// `Db::Open()`。
    fn open(&self) -> bool;
    /// `Db::OpenReadOnly()`。
    fn open_readonly(&self) -> bool;
    /// `Db::Close()`。
    fn close(&self) -> bool;

    /// `Db::Backup(snapshot_file)`。
    fn backup(&self, snapshot_file: &Path) -> bool;
    /// `Db::Restore(snapshot_file)`。
    fn restore(&self, snapshot_file: &Path) -> bool;

    /// `Db::CreateMetadata()`：写 `/db_name` 与 `/rime_version`。
    ///
    /// 默认实现够用（`db.cc:54-59`），子类只在前/后追加自己的键
    /// （`LevelDb` 追加 `/db_type`）。
    fn create_metadata(&self) -> bool {
        self.meta_update("/db_name", self.name())
            && self.meta_update("/rime_version", RIME_VERSION)
    }
    /// `Db::MetaFetch(key)`。
    fn meta_fetch(&self, key: &str) -> Option<String>;
    /// `Db::MetaUpdate(key, value)`。
    fn meta_update(&self, key: &str, value: &str) -> bool;

    /// `Db::QueryMetadata()`。
    fn query_metadata(&self) -> Option<Box<dyn DbAccessor>>;
    /// `Db::QueryAll()`：跳过元数据，从 `" "` 起扫。
    fn query_all(&self) -> Option<Box<dyn DbAccessor>>;
    /// `Db::Query(key)`：定位到第一个 `>= key` 且**以此开头**的键。
    fn query(&self, key: &str) -> Option<Box<dyn DbAccessor>>;
    /// `Db::Fetch(key)`。
    fn fetch(&self, key: &str) -> Option<String>;
    /// `Db::Update(key, value)`。
    fn update(&self, key: &str, value: &str) -> bool;
    /// `Db::Erase(key)`。
    fn erase(&self, key: &str) -> bool;

    /// `As<Transactional>(db)`：不支持事务的实现返回 `None`。
    fn as_transactional(&self) -> Option<&dyn Transactional> {
        None
    }
    /// `Is<Recoverable>(db)`。
    fn as_recoverable(&self) -> Option<&dyn Recoverable> {
        None
    }
}

/// `DbComponentBase::DbFilePath(name, extension)`。
///
/// C++ 走资源解析器（`{"db", "", ""}`，根 = 用户文件夹）；Weasel-Next 把
/// 用户词典统一放在 `<用户文件夹>/userdb/` 下（见
/// [`crate::lever::paths::Layout::user_db_dir`]），所以这里只做拼接。
pub fn db_file_path(db_dir: &Path, name: &str, extension: &str) -> PathBuf {
    db_dir.join(format!("{name}{extension}"))
}

/// `class DbSource`：把一个 [`Db`] 摊成"元数据流 + 数据流"。
///
/// C++ 版本同时持两个 accessor（`db_utils.cc:38-39`），这里一样。
pub struct DbSource {
    metadata: Box<dyn DbAccessor>,
    data: Box<dyn DbAccessor>,
}

impl DbSource {
    /// `DbSource::DbSource(db)`。db 未打开时两个游标都拿不到，返回 `None`。
    pub fn new(db: &dyn Db) -> Option<Self> {
        Some(Self {
            metadata: db.query_metadata()?,
            data: db.query_all()?,
        })
    }

    /// `DbSource::MetaGet`。
    pub fn meta_get(&mut self) -> Option<(String, String)> {
        self.metadata.get_next_record()
    }

    /// `DbSource::Get`。
    pub fn get(&mut self) -> Option<(String, String)> {
        self.data.get_next_record()
    }

    /// `Source::Dump(sink)`（`db_utils.cc:12-26`）：先元数据再数据，
    /// 返回**成功写进 sink 的条数**（不是读到的条数）。
    pub fn dump(&mut self, sink: &mut dyn TsvSink) -> usize {
        let mut num_entries = 0usize;
        while let Some((key, value)) = self.meta_get() {
            if sink.meta_put(&key, &value) {
                num_entries += 1;
            }
        }
        while let Some((key, value)) = self.get() {
            if sink.put(&key, &value) {
                num_entries += 1;
            }
        }
        num_entries
    }
}

/// `class DbSink`：把 sink 收到的键值写回 [`Db`]。
///
/// `meta_put` 走 [`Db::meta_update`]、`put` 走 [`Db::update`]——和
/// `db_utils.cc:30-36` 一样，键是**已经去掉** `\x01` 的裸元数据键。
pub struct DbSink<'a> {
    db: &'a dyn Db,
}

impl<'a> DbSink<'a> {
    pub fn new(db: &'a dyn Db) -> Self {
        Self { db }
    }
}

impl TsvSink for DbSink<'_> {
    fn meta_put(&mut self, key: &str, value: &str) -> bool {
        self.db.meta_update(key, value)
    }

    fn put(&mut self, key: &str, value: &str) -> bool {
        self.db.update(key, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个只活在内存里的 [`Db`]，用来验证 `DbSource` / `DbSink` / `dump` 的语义。
    ///
    /// 容器**保持有序**：真实 LevelDB 也是按字节序返回的，
    /// 无序容器会让 dump 的顺序测试失去意义。
    struct MemDb {
        name: String,
        path: PathBuf,
        meta: std::sync::Mutex<Vec<(String, String)>>,
        data: std::sync::Mutex<Vec<(String, String)>>,
        disabled: std::sync::atomic::AtomicBool,
    }

    impl MemDb {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                path: PathBuf::from(format!("/mem/{name}")),
                meta: Default::default(),
                data: Default::default(),
                disabled: Default::default(),
            }
        }
    }

    struct MemAccessor {
        rows: Vec<(String, String)>,
        prefix: String,
        index: usize,
    }

    impl MemAccessor {
        fn new(rows: &[(String, String)], prefix: &str, start: &str) -> Self {
            let mut accessor = Self {
                rows: rows.to_vec(),
                prefix: prefix.to_string(),
                index: 0,
            };
            accessor.jump(start);
            accessor
        }
    }

    impl DbAccessor for MemAccessor {
        fn reset(&mut self) -> bool {
            let prefix = self.prefix.clone();
            self.jump(&prefix)
        }

        fn jump(&mut self, key: &str) -> bool {
            self.index = self
                .rows
                .iter()
                .position(|(k, _)| k.as_str() >= key)
                .unwrap_or(self.rows.len());
            true
        }

        fn get_next_record(&mut self) -> Option<(String, String)> {
            let (key, value) = self.rows.get(self.index)?;
            if !key.starts_with(&self.prefix) {
                return None;
            }
            let record = (key.clone(), value.clone());
            self.index += 1;
            Some(record)
        }

        fn exhausted(&mut self) -> bool {
            match self.rows.get(self.index) {
                Some((key, _)) => !key.starts_with(&self.prefix),
                None => true,
            }
        }
    }

    impl Db for MemDb {
        fn name(&self) -> &str {
            &self.name
        }
        fn file_path(&self) -> &Path {
            &self.path
        }
        fn loaded(&self) -> bool {
            true
        }
        fn readonly(&self) -> bool {
            false
        }
        fn disabled(&self) -> bool {
            self.disabled.load(std::sync::atomic::Ordering::Relaxed)
        }
        fn disable(&self) {
            self.disabled
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        fn enable(&self) {
            self.disabled
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
        fn remove(&self) -> bool {
            false
        }
        fn open(&self) -> bool {
            true
        }
        fn open_readonly(&self) -> bool {
            true
        }
        fn close(&self) -> bool {
            true
        }
        fn backup(&self, _snapshot_file: &Path) -> bool {
            false
        }
        fn restore(&self, _snapshot_file: &Path) -> bool {
            false
        }
        fn meta_fetch(&self, key: &str) -> Option<String> {
            self.meta
                .lock()
                .unwrap()
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        }
        fn meta_update(&self, key: &str, value: &str) -> bool {
            let mut meta = self.meta.lock().unwrap();
            match meta.iter_mut().find(|(k, _)| k == key) {
                Some(slot) => slot.1 = value.to_string(),
                None => meta.push((key.to_string(), value.to_string())),
            }
            meta.sort();
            true
        }
        fn query_metadata(&self) -> Option<Box<dyn DbAccessor>> {
            Some(Box::new(MemAccessor::new(&self.meta.lock().unwrap(), "", "")))
        }
        fn query_all(&self) -> Option<Box<dyn DbAccessor>> {
            Some(Box::new(MemAccessor::new(&self.data.lock().unwrap(), "", " ")))
        }
        fn query(&self, key: &str) -> Option<Box<dyn DbAccessor>> {
            Some(Box::new(MemAccessor::new(&self.data.lock().unwrap(), key, key)))
        }
        fn fetch(&self, key: &str) -> Option<String> {
            self.data
                .lock()
                .unwrap()
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        }
        fn update(&self, key: &str, value: &str) -> bool {
            let mut data = self.data.lock().unwrap();
            match data.iter_mut().find(|(k, _)| k == key) {
                Some(slot) => slot.1 = value.to_string(),
                None => data.push((key.to_string(), value.to_string())),
            }
            data.sort();
            true
        }
        fn erase(&self, key: &str) -> bool {
            let mut data = self.data.lock().unwrap();
            data.retain(|(k, _)| k != key);
            true
        }
    }

    /// 记录 `dump` 调用顺序的 sink：`(通道, 键)`。
    #[derive(Default)]
    struct Recorder {
        events: Vec<(String, String)>,
    }

    impl TsvSink for Recorder {
        fn meta_put(&mut self, key: &str, _value: &str) -> bool {
            self.events.push(("meta".to_string(), key.to_string()));
            true
        }

        fn put(&mut self, key: &str, _value: &str) -> bool {
            self.events.push(("data".to_string(), key.to_string()));
            true
        }
    }

    #[test]
    fn create_metadata_writes_db_name_and_rime_version() {
        let db = MemDb::new("luna_pinyin.userdb");
        assert!(db.create_metadata());
        assert_eq!(
            db.meta_fetch("/db_name").as_deref(),
            Some("luna_pinyin.userdb")
        );
        assert_eq!(db.meta_fetch("/rime_version").as_deref(), Some(RIME_VERSION));
    }

    #[test]
    fn db_source_dumps_metadata_before_data() {
        let db = MemDb::new("t.userdb");
        db.create_metadata();
        db.update("a \t阿", "c=1 d=0 t=1");
        db.update("b \t巴", "c=2 d=0 t=2");

        let mut source = DbSource::new(&db).unwrap();
        let mut sink = Recorder::default();
        let count = source.dump(&mut sink);

        // 2 条元数据 + 2 条数据
        assert_eq!(count, 4);
        assert_eq!(
            sink.events,
            [
                ("meta".to_string(), "/db_name".to_string()),
                ("meta".to_string(), "/rime_version".to_string()),
                ("data".to_string(), "a \t阿".to_string()),
                ("data".to_string(), "b \t巴".to_string()),
            ]
        );
    }

    #[test]
    fn db_sink_routes_meta_and_data_to_the_right_channels() {
        let db = MemDb::new("t.userdb");
        let mut sink = DbSink::new(&db);
        assert!(sink.meta_put("/tick", "7"));
        assert!(sink.put("a \t阿", "c=1 d=0 t=7"));

        assert_eq!(db.meta_fetch("/tick").as_deref(), Some("7"));
        assert_eq!(db.fetch("a \t阿").as_deref(), Some("c=1 d=0 t=7"));
        // 关键：两条通道不能串
        assert_eq!(db.meta_fetch("a \t阿"), None);
        assert_eq!(db.fetch("/tick"), None);
    }

    #[test]
    fn query_all_skips_metadata() {
        let db = MemDb::new("t.userdb");
        db.create_metadata();
        db.update("a \t阿", "c=1 d=0 t=1");

        let mut all = db.query_all().unwrap();
        assert_eq!(all.get_next_record().unwrap().0, "a \t阿");
        assert!(all.get_next_record().is_none());
        assert!(all.exhausted());
    }

    #[test]
    fn accessor_stops_without_advancing_when_prefix_ends() {
        // 前缀扫的关键语义：越界那一次不吞记录，也不前进。
        let db = MemDb::new("t.userdb");
        db.update("a \t阿", "1");
        db.update("ab \t阿布", "2");
        db.update("b \t巴", "3");

        let mut accessor = db.query("a").unwrap();
        assert_eq!(accessor.get_next_record().unwrap().0, "a \t阿");
        assert_eq!(accessor.get_next_record().unwrap().0, "ab \t阿布");
        assert!(accessor.get_next_record().is_none());
        // 再调一次仍然 None，而且没有把 "b \t巴" 吞掉
        assert!(accessor.get_next_record().is_none());
        assert!(accessor.exhausted());
    }

    #[test]
    fn db_file_path_is_name_plus_extension() {
        let path = db_file_path(Path::new("/home/u/userdb"), "luna_pinyin", ".userdb");
        assert_eq!(path, PathBuf::from("/home/u/userdb/luna_pinyin.userdb"));
    }
}
