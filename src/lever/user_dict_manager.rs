//! `src/rime/lever/user_dict_manager.cc`（226 行）的**接缝**（trait 部分）。
//!
//! ## 为什么只有 trait
//!
//! `UserDictManager` 的操作全落在用户词典文件上（`*.userdb`），而 `userdb` 的读写
//! 属于 `dict` 层（`dict/user_db.rs` / `dict/level_db.rs`），那一层还在移植中。
//! `deployer` 只需要一个**能存进去的 trait 对象**（`Deployer::user_db` /
//! `Deployer::legacy_user_db`），现在就给它这个形状，等 `dict` 落地再实现。
//!
//! ## C++ 的对应关系
//!
//! ```text
//! UserDictManager(deployer)
//!   ├─ user_dict_sync()      → 遍历 userdb、合并/备份
//!   ├─ upgrade_user_dicts()  → 老格式 .userdb → 新格式
//!   └─ backup_user_dicts()   → 拷到 sync/<user_id>/
//! ```

/// 用户词典工厂的接缝：C++ 的 `UserDictComponent`。
///
/// 只保留 `UserDictManager` 真正用到的那几个动作 —— 目前还没被调用，
/// 所以这里是**空实现也能编译**的最小形状；等 `dict` 层能读 `.userdb` 再补。
///
/// `Send + Sync`：`Deployer` 会被 C API 放进 `Mutex<Global>` 背后的 `static`，
/// 不加这两个 bound 就无法满足 `Sync`（与 `Engine` 的 `dyn Translator + Send + Sync` 同理）。
pub trait UserDbFactory: Send + Sync {
    /// 打开一个用户词典（C++ `UserDictComponent::Create(name)`）。
    fn create(&mut self, name: &str) -> bool;

    /// 列出磁盘上已有的用户词典名（C++ `UserDictManager::GetUserDictList`）。
    fn list(&mut self) -> Vec<String>;

    /// 备份到 `sync/<user_id>/`（C++ `BackupUserDicts`）。
    fn backup(&mut self, destination: &std::path::Path) -> bool;

    /// 升级旧格式（C++ `UpgradeUserDicts`）。
    fn upgrade(&mut self) -> bool;
}

/// 内存桩：测试与"没有真实词典"的路径用它，避免 `Deployer` 里全是 `Option<None>` 特判。
#[derive(Debug, Default)]
pub struct InMemoryUserDb {
    created: Vec<String>,
}

impl UserDbFactory for InMemoryUserDb {
    fn create(&mut self, name: &str) -> bool {
        self.created.push(name.to_string());
        true
    }

    fn list(&mut self) -> Vec<String> {
        self.created.clone()
    }

    fn backup(&mut self, _destination: &std::path::Path) -> bool {
        // 桩：真的备份要等 dict 层能读 userdb
        false
    }

    fn upgrade(&mut self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_stub_tracks_created_dicts() {
        let mut db = InMemoryUserDb::default();
        assert!(db.create("yuling"));
        assert!(db.create("luna_pinyin"));
        assert_eq!(db.list(), vec!["yuling".to_string(), "luna_pinyin".to_string()]);
    }

    #[test]
    fn stub_is_honest_about_unimplemented_actions() {
        let mut db = InMemoryUserDb::default();
        // 备份/升级依赖 dict 层，桩必须返回 false 而不是假装成功
        assert!(!db.backup(std::path::Path::new("C:\\nonexistent")));
        assert!(!db.upgrade());
    }
}
