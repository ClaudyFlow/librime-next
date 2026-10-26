//! librime `dict/level_db.h`(78) + `level_db.cc`(338) 的移植：LevelDB 后端。
//!
//! # 为什么用 `rusty-leveldb`
//!
//! 用户词典是**已经写在磁盘上**的 LevelDB 目录（`%APPDATA%/Rime-Next/userdb/*.userdb`，
//! 出厂实测 4 个在用）。换引擎时这些文件必须被原地读出来 —— 所以不能自造格式，
//! 只能接一个真正的 LevelDB。`rusty-leveldb` 是 LevelDB 存储格式的纯 Rust 实现
//! （CURRENT / MANIFEST / .ldb / .log 全套），读写都可与 C++ 版互通。
//!
//! 压缩：C++ 版 leveldb 默认写 Snappy 块（块类型 1）。`rusty-leveldb` 的
//! `CompressorList::default()` 恰好同时注册了 `NoneCompressor`(0) 与
//! `SnappyCompressor`(1)，所以**读**老库不需要额外配置。**写**用的是 id 0（不压缩），
//! 块自带类型标记，C++ 版照样读得懂 —— 字节不同、语义相同。
//!
//! # 结构：游标关进 `Mutex`
//!
//! C++ 的 `LevelDbAccessor` 持有一个 `leveldb::Iterator*`，裸指针想活多久活多久。
//! `rusty_leveldb::DBIterator` 内部含 `Rc<RefCell<..>>`（`Shared<T> = Rc<RefCell<T>>`），
//! 既不是 `Send`，也不能跨线程随便转移。而 [`Db`] 要求 `Send + Sync`
//! （`c_api::Global` 在 `static Mutex` 里）。
//!
//! 解法：**所有句柄（`DB` + 全部活跃游标）都放进同一个 `Mutex<LevelDbState>`**，
//! accessor 只持 `Arc<Mutex<LevelDbState>>` + 一个槽位号。这样
//!
//! * `Rc` 的引用计数永远只在持锁时动，不会跨线程竞态；
//! * 哪个线程拿锁哪个线程用，`unsafe impl Send/Sync` 的前提是被遵守的；
//! * 多游标并存（`DbSource` 就要两个：元数据 + 数据）靠槽位数组支持，
//!   行为和 C++ 的"每个 accessor 一个 iterator"一致。
//!
//! # 与 C++ 的接口差异
//!
//! * **`RepairDB` / `DestroyDB` 没有对应物**。`rusty-leveldb` 没导出这两个函数，
//!   所以 [`Db::remove`] 直接 `remove_dir_all`，[`Recoverable::recover`] 退化成
//!   "重开一次"（`DB::open` 本身会重放 `.log`，这是唯一能做的恢复）。
//! * **`WriteBatch` 语义**。C++ 的 `CommitTransaction` 是一次原子 `Write`；
//!   这里同样是 `WriteBatch` + 一次 `DB::write`（`rusty-leveldb` 提供
//!   `WriteBatch::default()`，`new()` 是 `pub(crate)`）。
//! * **`reuse_logs`**。`rusty-leveldb` 默认 `true`（复用 `.log` 而不是每次 open 都
//!   转成 `.ldb`），C++ 默认 `false`。只影响落盘时机，不影响可见数据。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusty_leveldb::{Options as LdbOptions, WriteBatch, DB};

use super::db::{Db, DbAccessor, Recoverable, Transactional, K_META_CHARACTER};
use super::user_db::UserDbHelper;

/// `.userdb` 的扩展名（`level_db.cc:329-331` 的 `UserDbComponent<LevelDb>::extension`）。
pub const USERDB_EXTENSION: &str = ".userdb";
/// 写进元数据 `/db_type` 的值（`level_db.cc:334-336`）。
pub const USERDB_TYPE: &str = "userdb";

/// 一个活跃的操作，事务期间先攒着，提交时一次写。
enum Op {
    Put(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
}

/// [`LevelDb`] 的全部可变状态：句柄 + 事务缓冲 + 游标槽位。
struct LevelDbState {
    db: Option<DB>,
    pending: Option<Vec<Op>>,
    loaded: bool,
    readonly: bool,
    disabled: bool,
    cursors: Vec<Option<rusty_leveldb::DBIterator>>,
}

impl LevelDbState {
    fn new() -> Self {
        Self {
            db: None,
            pending: None,
            loaded: false,
            readonly: false,
            disabled: false,
            cursors: Vec::new(),
        }
    }

    fn in_transaction(&self) -> bool {
        self.pending.is_some()
    }

    fn fetch(&mut self, key: &str) -> Option<String> {
        let db = self.db.as_mut()?;
        let bytes = db.get(key.as_bytes())?;
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn update(&mut self, key: &str, value: &str) -> bool {
        if !self.loaded || self.readonly {
            return false;
        }
        if let Some(pending) = self.pending.as_mut() {
            pending.push(Op::Put(key.as_bytes().to_vec(), value.as_bytes().to_vec()));
            return true;
        }
        let Some(db) = self.db.as_mut() else {
            return false;
        };
        db.put(key.as_bytes(), value.as_bytes()).is_ok()
    }

    fn erase(&mut self, key: &str) -> bool {
        if !self.loaded || self.readonly {
            return false;
        }
        if let Some(pending) = self.pending.as_mut() {
            pending.push(Op::Delete(key.as_bytes().to_vec()));
            return true;
        }
        let Some(db) = self.db.as_mut() else {
            return false;
        };
        db.delete(key.as_bytes()).is_ok()
    }

    fn meta_fetch(&mut self, key: &str) -> Option<String> {
        self.fetch(&(K_META_CHARACTER.to_string() + key))
    }

    fn meta_update(&mut self, key: &str, value: &str) -> bool {
        let meta_key = K_META_CHARACTER.to_string() + key;
        self.update(&meta_key, value)
    }

    /// 分配一个游标槽位，返回槽位号。
    fn alloc_cursor(&mut self, iter: rusty_leveldb::DBIterator) -> usize {
        if let Some(index) = self.cursors.iter().position(Option::is_none) {
            self.cursors[index] = Some(iter);
            index
        } else {
            self.cursors.push(Some(iter));
            self.cursors.len() - 1
        }
    }

    fn cursor(&mut self, slot: usize) -> Option<&mut rusty_leveldb::DBIterator> {
        self.cursors.get_mut(slot).and_then(|c| c.as_mut())
    }
}

/// `class LevelDb : public Db, public Recoverable, public Transactional`。
pub struct LevelDb {
    name: String,
    file_path: PathBuf,
    db_type: String,
    state: Arc<Mutex<LevelDbState>>,
}

// `LevelDbState` 里装着 `rusty_leveldb::DB`，后者用 `Rc<RefCell<..>>` 做内部共享，
// 因此不是自动 `Send`/`Sync`。但所有访问路径都先锁 `state`（见模块文档），
// 引用计数不会跨线程并发改动 —— 这是本条 unsafe 成立的唯一前提。
unsafe impl Send for LevelDb {}
unsafe impl Sync for LevelDb {}

impl LevelDb {
    /// `LevelDb::LevelDb(file_path, db_name, db_type)`。
    pub fn new(file_path: impl Into<PathBuf>, name: &str, db_type: &str) -> Self {
        Self {
            name: name.to_string(),
            file_path: file_path.into(),
            db_type: db_type.to_string(),
            state: Arc::new(Mutex::new(LevelDbState::new())),
        }
    }

    /// `<db_dir>/<name>.userdb` 的用户词典后端（C++ `UserDbWrapper<LevelDb>`）。
    pub fn new_user_db(db_dir: &Path, name: &str) -> Self {
        Self::new(
            super::db::db_file_path(db_dir, name, USERDB_EXTENSION),
            name,
            USERDB_TYPE,
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LevelDbState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `LevelDbAccessor` 的工厂：建游标、定位到 `key`、登记槽位。
    fn open_cursor(&self, prefix: &str) -> Option<usize> {
        let mut state = self.lock();
        let db = state.db.as_mut()?;
        let mut iter = db.new_iter().ok()?;
        // C++ 是 `Seek(key)`；LevelDB 的 Seek("") 本来就在第一条，但显式区分更稳。
        if prefix.is_empty() {
            rusty_leveldb::LdbIterator::seek_to_first(&mut iter);
        } else {
            rusty_leveldb::LdbIterator::seek(&mut iter, prefix.as_bytes());
        }
        Some(state.alloc_cursor(iter))
    }
}

impl Db for LevelDb {
    fn name(&self) -> &str {
        &self.name
    }

    fn file_path(&self) -> &Path {
        &self.file_path
    }

    fn loaded(&self) -> bool {
        self.lock().loaded
    }

    fn readonly(&self) -> bool {
        self.lock().readonly
    }

    fn disabled(&self) -> bool {
        self.lock().disabled
    }

    fn disable(&self) {
        self.lock().disabled = true;
    }

    fn enable(&self) {
        self.lock().disabled = false;
    }

    /// `LevelDb::Remove()`：`DestroyDB` 没有对应物，直接删目录。
    fn remove(&self) -> bool {
        let mut state = self.lock();
        if state.loaded {
            eprintln!("[level_db] attempt to remove opened db '{}'.", self.name);
            return false;
        }
        match std::fs::remove_dir_all(&self.file_path) {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
            Err(e) => {
                eprintln!("[level_db] error removing db '{}': {e}", self.name);
                false
            }
        }
    }

    /// `LevelDb::Open()`：打开后若缺 `/db_name` 就补元数据。
    fn open(&self) -> bool {
        self.open_impl(false)
    }

    fn open_readonly(&self) -> bool {
        self.open_impl(true)
    }

    fn close(&self) -> bool {
        let mut state = self.lock();
        if !state.loaded {
            return false;
        }
        state.db = None;
        state.cursors.clear();
        state.pending = None;
        state.loaded = false;
        state.readonly = false;
        true
    }

    fn backup(&self, snapshot_file: &Path) -> bool {
        if !self.loaded() {
            return false;
        }
        UserDbHelper::new(self).uniform_backup(snapshot_file)
    }

    fn restore(&self, snapshot_file: &Path) -> bool {
        if !self.loaded() || self.readonly() {
            return false;
        }
        UserDbHelper::new(self).uniform_restore(snapshot_file)
    }

    /// `LevelDb::CreateMetadata()` = 基类的两键 + `/db_type`（`level_db.cc:291-293`）。
    ///
    /// 三个键在同一把锁里写完：分成"基类一次、子类一次"会锁两次，
    /// 中间可能被别人看见半套元数据。
    fn create_metadata(&self) -> bool {
        let mut state = self.lock();
        state.meta_update("/db_name", &self.name)
            && state.meta_update("/rime_version", super::db::RIME_VERSION)
            && state.meta_update("/db_type", &self.db_type)
    }

    fn meta_fetch(&self, key: &str) -> Option<String> {
        self.lock().meta_fetch(key)
    }

    fn meta_update(&self, key: &str, value: &str) -> bool {
        self.lock().meta_update(key, value)
    }

    fn query_metadata(&self) -> Option<Box<dyn DbAccessor>> {
        self.query(K_META_CHARACTER)
    }

    /// `LevelDb::QueryAll()`：`Query("")` 之后 `Jump(" ")` 跳过元数据。
    fn query_all(&self) -> Option<Box<dyn DbAccessor>> {
        let mut accessor = LevelDbAccessor::new(self.state.clone(), self.open_cursor("")?, "");
        accessor.jump(" ");
        Some(Box::new(accessor))
    }

    fn query(&self, key: &str) -> Option<Box<dyn DbAccessor>> {
        let slot = self.open_cursor(key)?;
        Some(Box::new(LevelDbAccessor::new(
            self.state.clone(),
            slot,
            key,
        )))
    }

    fn fetch(&self, key: &str) -> Option<String> {
        self.lock().fetch(key)
    }

    fn update(&self, key: &str, value: &str) -> bool {
        self.lock().update(key, value)
    }

    fn erase(&self, key: &str) -> bool {
        self.lock().erase(key)
    }

    fn as_transactional(&self) -> Option<&dyn Transactional> {
        Some(self)
    }

    fn as_recoverable(&self) -> Option<&dyn Recoverable> {
        Some(self)
    }
}

impl LevelDb {
    fn open_impl(&self, readonly: bool) -> bool {
        let mut state = self.lock();
        if state.loaded {
            return false;
        }
        let options = LdbOptions {
            create_if_missing: !readonly,
            ..Default::default()
        };
        state.readonly = readonly;
        state.db = DB::open(&self.file_path, options).ok();
        state.loaded = state.db.is_some();
        if !state.loaded {
            eprintln!(
                "[level_db] error opening db '{}' (readonly={readonly}).",
                self.name
            );
            return false;
        }
        if !readonly && state.meta_fetch("/db_name").is_none() {
            // 等价于 `Db::CreateMetadata()`，但在同一把锁里完成，避免重复加锁。
            let ok = state.meta_update("/db_name", &self.name)
                && state.meta_update("/rime_version", super::db::RIME_VERSION)
                && state.meta_update("/db_type", &self.db_type);
            if !ok {
                eprintln!("[level_db] error creating metadata for '{}'.", self.name);
                state.db = None;
                state.loaded = false;
                state.readonly = false;
                return false;
            }
        }
        true
    }
}

impl Transactional for LevelDb {
    fn begin_transaction(&self) -> bool {
        let mut state = self.lock();
        if !state.loaded {
            return false;
        }
        state.pending = Some(Vec::new());
        true
    }

    fn abort_transaction(&self) -> bool {
        let mut state = self.lock();
        if !state.loaded || !state.in_transaction() {
            return false;
        }
        state.pending = None;
        true
    }

    /// `LevelDb::CommitTransaction()`：攒下的操作一次性写进一个 `WriteBatch`。
    fn commit_transaction(&self) -> bool {
        let mut state = self.lock();
        if !state.loaded || !state.in_transaction() {
            return false;
        }
        let ops = state.pending.take().unwrap_or_default();
        let Some(db) = state.db.as_mut() else {
            return false;
        };
        if ops.is_empty() {
            return true;
        }
        let mut batch = WriteBatch::default();
        for op in ops {
            match op {
                Op::Put(key, value) => batch.put(&key, &value),
                Op::Delete(key) => batch.delete(&key),
            }
        }
        db.write(batch, false).is_ok()
    }

    fn in_transaction(&self) -> bool {
        self.lock().in_transaction()
    }
}

impl Recoverable for LevelDb {
    /// `LevelDb::Recover()`。`RepairDB` 在 `rusty-leveldb` 里没有对应物，
    /// 能做的是"关掉再开一次"—— `DB::open` 会重放 `.log`，这就是它的全部恢复能力。
    fn recover(&self) -> bool {
        let loaded = self.loaded();
        if loaded {
            self.close();
        }
        let ok = self.open();
        if !ok {
            eprintln!("[level_db] db recovery failed: '{}'.", self.name);
        }
        ok
    }
}

/// `class LevelDbAccessor`：只持槽位号，真游标留在 `Mutex` 里。
struct LevelDbAccessor {
    state: Arc<Mutex<LevelDbState>>,
    slot: usize,
    prefix: String,
    is_metadata_query: bool,
}

impl LevelDbAccessor {
    fn new(state: Arc<Mutex<LevelDbState>>, slot: usize, prefix: &str) -> Self {
        // `LevelDbAccessor::LevelDbAccessor`：`is_metadata_query_ = (prefix == kMetaCharacter)`
        let is_metadata_query = prefix == K_META_CHARACTER;
        let mut accessor = Self {
            state,
            slot,
            prefix: prefix.to_string(),
            is_metadata_query,
        };
        accessor.reset();
        accessor
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LevelDbState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl DbAccessor for LevelDbAccessor {
    fn reset(&mut self) -> bool {
        let prefix = self.prefix.clone();
        self.jump(&prefix)
    }

    fn jump(&mut self, key: &str) -> bool {
        let slot = self.slot;
        let mut state = self.lock();
        let Some(iter) = state.cursor(slot) else {
            return false;
        };
        if key.is_empty() {
            rusty_leveldb::LdbIterator::seek_to_first(iter);
        } else {
            rusty_leveldb::LdbIterator::seek(iter, key.as_bytes());
        }
        true
    }

    fn get_next_record(&mut self) -> Option<(String, String)> {
        let slot = self.slot;
        let prefix = self.prefix.clone();
        let is_metadata_query = self.is_metadata_query;
        let mut state = self.lock();
        let iter = state.cursor(slot)?;
        let (key_bytes, value_bytes) = rusty_leveldb::LdbIterator::current(iter)?;
        let mut key = String::from_utf8_lossy(&key_bytes).into_owned();
        if !key.starts_with(&prefix) {
            return None;
        }
        let value = String::from_utf8_lossy(&value_bytes).into_owned();
        rusty_leveldb::LdbIterator::advance(iter);
        if is_metadata_query {
            // 去掉元数据前导字符 `\x01`
            key.remove(0);
        }
        Some((key, value))
    }

    fn exhausted(&mut self) -> bool {
        let slot = self.slot;
        let prefix = self.prefix.clone();
        let mut state = self.lock();
        let Some(iter) = state.cursor(slot) else {
            return true;
        };
        match rusty_leveldb::LdbIterator::current(iter) {
            Some((key_bytes, _)) => !key_bytes.starts_with(prefix.as_bytes()),
            None => true,
        }
    }
}

impl Drop for LevelDbAccessor {
    fn drop(&mut self) {
        let slot = self.slot;
        let mut state = self.lock();
        if let Some(cursor) = state.cursors.get_mut(slot) {
            *cursor = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时目录（不依赖 tempfile crate）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "lrn_leveldb_{tag}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn open_creates_metadata_with_db_type() {
        let dir = TempDir::new("meta");
        let db = LevelDb::new_user_db(dir.path(), "luna_pinyin");
        assert!(db.open());
        assert_eq!(db.meta_fetch("/db_name").as_deref(), Some("luna_pinyin"));
        assert_eq!(
            db.meta_fetch("/rim_version").as_deref(),
            None,
            "键名打错必须查不出来"
        );
        assert_eq!(db.meta_fetch("/rime_version").as_deref(), Some("1.17.0"));
        assert_eq!(db.meta_fetch("/db_type").as_deref(), Some("userdb"));
        assert!(db.close());
        assert!(!db.loaded());
    }

    #[test]
    fn metadata_keys_are_invisible_to_data_queries() {
        let dir = TempDir::new("channels");
        let db = LevelDb::new_user_db(dir.path(), "t");
        assert!(db.open());
        db.update("a \t阿", "c=1 d=0 t=1");
        db.update("b \t巴", "c=2 d=0 t=2");

        let mut all = db.query_all().unwrap();
        assert_eq!(all.get_next_record().unwrap(), ("a \t阿".into(), "c=1 d=0 t=1".into()));
        assert_eq!(all.get_next_record().unwrap(), ("b \t巴".into(), "c=2 d=0 t=2".into()));
        assert!(all.get_next_record().is_none());

        let mut meta = db.query_metadata().unwrap();
        let key = meta.get_next_record().unwrap().0;
        // 取出来的是去掉了 `\x01` 的键
        assert!(key.starts_with('/'), "元数据键应以 / 开头，实际 {key:?}");
        assert!(db.close());
    }

    #[test]
    fn prefix_scan_stops_at_prefix_boundary() {
        let dir = TempDir::new("prefix");
        let db = LevelDb::new_user_db(dir.path(), "t");
        assert!(db.open());
        for (key, value) in [
            ("a \t阿", "1"),
            ("ab \t阿布", "2"),
            ("b \t巴", "3"),
        ] {
            db.update(key, value);
        }

        let mut accessor = db.query("a").unwrap();
        assert_eq!(accessor.get_next_record().unwrap().0, "a \t阿");
        assert_eq!(accessor.get_next_record().unwrap().0, "ab \t阿布");
        assert!(accessor.get_next_record().is_none());
        assert!(accessor.exhausted());
        // 越过前缀后不前进：再调仍是 None
        assert!(accessor.get_next_record().is_none());
        drop(accessor);

        // jump 能重新定位（注意：游标仍然只认自己的前缀，跳到别的前缀就取不到记录）
        let mut accessor = db.query("a").unwrap();
        assert!(accessor.jump("b \t巴"));
        assert!(
            accessor.get_next_record().is_none(),
            "前缀不是 a 的键不该被这个游标取到"
        );
        drop(accessor);

        // 新开一个前缀为 "b" 的游标就能取到
        let mut accessor = db.query("b").unwrap();
        assert_eq!(accessor.get_next_record().unwrap().0, "b \t巴");
        assert!(db.close());
    }

    #[test]
    fn transaction_defers_writes_until_commit() {
        let dir = TempDir::new("tx");
        let db = LevelDb::new_user_db(dir.path(), "t");
        assert!(db.open());
        let tx = db.as_transactional().unwrap();

        assert!(tx.begin_transaction());
        assert!(db.update("a \t阿", "c=1 d=0 t=1"));
        assert_eq!(db.fetch("a \t阿"), None, "事务未提交前不应可见");

        assert!(tx.commit_transaction());
        assert_eq!(db.fetch("a \t阿").as_deref(), Some("c=1 d=0 t=1"));

        // abort 丢弃
        assert!(tx.begin_transaction());
        assert!(db.update("b \t巴", "c=9 d=0 t=9"));
        assert!(tx.abort_transaction());
        assert_eq!(db.fetch("b \t巴"), None);
        assert!(db.close());
    }

    #[test]
    fn data_survives_close_and_reopen() {
        let dir = TempDir::new("persist");
        let name = "persist";
        {
            let db = LevelDb::new_user_db(dir.path(), name);
            assert!(db.open());
            db.meta_update("/tick", "42");
            db.update("nihao \t你好", "c=3 d=12.5 t=42");
            assert!(db.close());
        }
        {
            let db = LevelDb::new_user_db(dir.path(), name);
            assert!(db.open());
            assert_eq!(db.meta_fetch("/tick").as_deref(), Some("42"));
            assert_eq!(db.fetch("nihao \t你好").as_deref(), Some("c=3 d=12.5 t=42"));
            assert!(db.close());
        }
    }

    #[test]
    fn readonly_open_cannot_create_missing_db() {
        let dir = TempDir::new("ro");
        let db = LevelDb::new_user_db(dir.path(), "absent");
        assert!(!db.open_readonly());
        assert!(!db.loaded());
    }

    #[test]
    fn erase_removes_entry() {
        let dir = TempDir::new("erase");
        let db = LevelDb::new_user_db(dir.path(), "t");
        assert!(db.open());
        db.update("a \t阿", "1");
        assert!(db.erase("a \t阿"));
        assert_eq!(db.fetch("a \t阿"), None);
        assert!(db.close());
    }

    /// 递归拷目录（不引第三方 crate）。
    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_dir(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).unwrap();
            }
        }
    }

    /// **互通验证**：读一台真实机器上由 C++ 版 librime 写出来的 `.userdb`。
    ///
    /// 做法是**先拷到临时目录**再打开 —— 用户词典是活数据，测试绝不能碰原件。
    /// 本机没有该文件时静默跳过（与 `gram_db` 的 `open_real_gram_file` 同风格）。
    #[test]
    fn reads_real_cpp_written_userdb() {
        let Some(appdata) = std::env::var_os("APPDATA") else {
            return;
        };
        let source = PathBuf::from(appdata)
            .join("Rime-Next")
            .join("userdb")
            .join("luna_pinyin.userdb");
        if !source.is_dir() {
            return;
        }
        let dir = TempDir::new("real");
        copy_dir(&source, &dir.path().join("luna_pinyin.userdb"));

        let db = LevelDb::new_user_db(dir.path(), "luna_pinyin");
        assert!(db.open_readonly(), "应能只读打开 C++ 版写出的 userdb");

        // 元数据：C++ 版写的三件套
        assert_eq!(db.meta_fetch("/db_name").as_deref(), Some("luna_pinyin"));
        assert_eq!(db.meta_fetch("/db_type").as_deref(), Some("userdb"));
        assert!(
            db.meta_fetch("/tick")
                .and_then(|t| t.parse::<u64>().ok())
                .is_some(),
            "/tick 应存在且可解析"
        );

        // 数据：键 = "码串 + TAB + 词条"，值 = "c=.. d=.. t=.."
        let mut all = db.query_all().unwrap();
        let mut total = 0usize;
        let mut samples = Vec::new();
        while let Some((key, value)) = all.get_next_record() {
            assert!(key.contains('\t'), "userdb 键必须含 TAB，实际 {key:?}");
            assert!(
                super::super::user_db::UserDbValue::unpack(&value).is_some(),
                "值必须能解成 UserDbValue，实际 {value:?}"
            );
            if samples.len() < 3 {
                samples.push((key, value));
            }
            total += 1;
        }
        assert!(total > 0, "真实 userdb 不该是空的");
        eprintln!("[level_db] 读到 {total} 条用户词典记录，样例：{samples:?}");

        // 前缀扫：用第一条的码串当前缀，至少能找回归属它的那条
        let (first_key, _) = &samples[0];
        let code = first_key.split('\t').next().unwrap();
        let mut accessor = db.query(code).unwrap();
        let mut hits = 0;
        while accessor.get_next_record().is_some() {
            hits += 1;
        }
        assert!(hits > 0, "前缀 {code:?} 应至少命中一条");
        assert!(db.close());
    }
}
