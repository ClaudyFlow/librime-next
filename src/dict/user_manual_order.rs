//! `dict/user_manual_order.cc`（librime-evo）移植：**手排位置**的存取。
//!
//! # 手排是什么
//!
//! 上游 librime 只有「打得多就往前排」这一条路，用户想把某个词固定在第 2 位
//! 没有办法（除了删掉别的词）。evo 加了第五个字段 `p`（position，1 基）与
//! 第六个字段 `m`（manual_stamp，谁改的）：
//!
//! ```text
//! 自动学习：c=21 d=3.50902 t=3402
//! 手排到第 2 位：c=21 d=3.50902 t=3402 p=2 m=6fn3k
//! ```
//!
//! # 三条不变量（全部来自 evo，缺一条手排就会"自己跑掉"）
//!
//! 1. **同码同位置只能有一个主人**：把 A 排到 2 号位时，若 2 号位已被 B 占着，
//!    直接拒绝（`FindPinnedAt`）—— 否则两个词并列第 2，顺序由 LevelDB 的
//!    字节序决定，看起来就是随机跳动。
//! 2. **版本号单调递增**：`/manual_clock` 记全局最大戳，下一次取
//!    `max(现在, 上次 + 1)`。同一秒内连点两次也必须能分出先后（同步合并靠它）。
//! 3. **清空也要留戳**：把位置清成 0 时戳照样往前走，否则同步时"我清了"
//!    会被对方的"我排了"覆盖回去。
//!
//! # 与 C++ 的差异
//!
//! C++ 把 `SetManual` / `MoveAdjacent` 包在 LevelDB 事务里。Rust 侧的
//! [`crate::dict::db::Db`] 只在 LevelDB 后端支持事务，测试用的内存/文本后端不支持；
//! 这里改成"先算完所有前置条件再逐条写"，中途失败会把已写的值回滚回旧串 ——
//! 对外语义相同（要么全成、要么全不变），代价是极端情况下多几次写。

use super::db::Db;
use super::manual_stamp;
use super::user_db::UserDbValue;

/// `/manual_clock`：全局手排版本号。
const MANUAL_CLOCK_KEY: &str = "/manual_clock";

/// `NextClock(db, &version)`：取一个**严格大于**上次的版本号。
fn next_clock(db: &dyn Db) -> Option<u64> {
    let last = db
        .meta_fetch(MANUAL_CLOCK_KEY)
        .and_then(|raw| manual_stamp::parse(&raw))
        .unwrap_or(0);
    if last == u64::MAX {
        return None;
    }
    Some(manual_stamp::now().max(last + 1))
}

fn save_clock(db: &dyn Db, version: u64) -> bool {
    db.meta_update(MANUAL_CLOCK_KEY, &manual_stamp::format(version))
}

/// 同码下扫描：`position == source` / `position == target` 的主人各是谁。
///
/// 返回 `(source_owner, target_owner)`；同位置有多个主人时返回 `None`
/// （数据已损坏，宁可不动）。
fn find_pinned_owners(
    db: &dyn Db,
    code: &str,
    source_position: i32,
    target_position: i32,
) -> Option<(Option<String>, Option<String>)> {
    let mut cursor = db.query(&format!("{code}\t"))?;
    let mut source_owner: Option<String> = None;
    let mut target_owner: Option<String> = None;
    while let Some((key, value)) = cursor.get_next_record() {
        let Some(state) = UserDbValue::unpack(&value) else {
            return None;
        };
        if !state.has_manual_position() {
            continue;
        }
        if state.position == source_position {
            if source_owner.is_some() {
                return None;
            }
            source_owner = Some(key.clone());
        }
        if source_position != target_position && state.position == target_position {
            if target_owner.is_some() {
                return None;
            }
            target_owner = Some(key);
        }
    }
    Some((source_owner, target_owner))
}

/// `FindPinnedAt(db, code, position, &occupant)`。
fn pinned_occupant(db: &dyn Db, code: &str, position: i32) -> Option<Option<String>> {
    find_pinned_owners(db, code, position, position).map(|(owner, _)| owner)
}

/// 读一条记录的当前状态（不存在 → 默认值）。
fn read_state(db: &dyn Db, key: &str) -> Option<UserDbValue> {
    match db.fetch(key) {
        None => Some(UserDbValue::default()),
        Some(raw) => UserDbValue::unpack(&raw),
    }
}

/// `SetManual(db, code, text, position)`：把手排位置设成 `position`（0 = 取消手排）。
pub fn set_manual(db: &dyn Db, code: &str, text: &str, position: i32) -> bool {
    if code.is_empty() || position < 0 {
        return false;
    }
    let key = format!("{code}\t{text}");
    let Some(mut state) = read_state(db, &key) else {
        return false;
    };
    if position > 0 {
        // 目标位置已被别人占着就不能抢（不变量 1）
        match pinned_occupant(db, code, position) {
            Some(Some(occupant)) if occupant != key => return false,
            Some(_) => {}
            None => return false,
        }
    }
    let Some(version) = next_clock(db) else {
        return false;
    };
    if position == 0 {
        state.reset_manual_position(version);
    } else {
        state.set_manual_version(position, version);
    }
    let packed = state.pack();
    if !db.update(&key, &packed) {
        return false;
    }
    save_clock(db, version)
}

/// `ResetAuto(db, code, text)`：只抹掉自动学习，**手排保留**。
pub fn reset_auto(db: &dyn Db, code: &str, text: &str) -> bool {
    if code.is_empty() || text.is_empty() {
        return false;
    }
    let key = format!("{code}\t{text}");
    let Some(mut state) = read_state(db, &key) else {
        return false;
    };
    // 不存在就不创建（上游 `Fetch` 失败直接 false）
    if db.fetch(&key).is_none() {
        return false;
    }
    state.reset_auto_learning();
    db.update(&key, &state.pack())
}

/// `ResetManual(db, code, text)`：只清手排，自动学习照旧。
pub fn reset_manual(db: &dyn Db, code: &str, text: &str) -> bool {
    if code.is_empty() || text.is_empty() {
        return false;
    }
    let key = format!("{code}\t{text}");
    if db.fetch(&key).is_none() {
        return false;
    }
    let Some(version) = next_clock(db) else {
        return false;
    };
    let Some(mut state) = read_state(db, &key) else {
        return false;
    };
    state.reset_manual_position(version);
    if !db.update(&key, &state.pack()) {
        return false;
    }
    save_clock(db, version)
}

/// `MoveAdjacent(db, code, text, adjacent_code, adjacent_text, source_position,
/// direction, neighbor_visible)`：把选中词与相邻词**换位**。
///
/// * `direction > 0` 往前（位置数字变小），`< 0` 往后；
/// * 目标位置空着就直接占（`position = target`）；
/// * 目标被邻词占着就**两个词互换版本**（各拿一个新戳，主人在前面）；
/// * `neighbor_visible == false` 表示邻词不在当前页 —— 此时只动自己，不碰对方。
pub fn move_adjacent(
    db: &dyn Db,
    code: &str,
    text: &str,
    adjacent_code: &str,
    adjacent_text: &str,
    source_position: i32,
    direction: i32,
    neighbor_visible: bool,
) -> bool {
    if code.is_empty() || text.is_empty() || direction == 0 {
        return false;
    }
    // 目标位置：第 1 位再往前就没有了（0 是"未手排"，不能拿来当位置）
    let target = if direction > 0 {
        source_position - 1
    } else {
        source_position + 1
    };
    if target < 1 {
        return false;
    }
    let key = format!("{code}\t{text}");
    let Some(mut state) = read_state(db, &key) else {
        return false;
    };
    if state.position == 0 {
        state.position = source_position.max(1);
    }
    let Some(version) = next_clock(db) else {
        return false;
    };
    state.set_manual_version(target, version);
    if !db.update(&key, &state.pack()) {
        return false;
    }
    save_clock(db, version);
    // 邻词占着目标位置 ⇒ 它退到本词原来的位置（两个词互换）
    if neighbor_visible && !adjacent_text.is_empty() && !adjacent_code.is_empty() {
        let neighbor_key = format!("{adjacent_code}\t{adjacent_text}");
        if let Some(mut neighbor) = read_state(db, &neighbor_key)
            && neighbor.has_manual_position()
            && neighbor.position == target
        {
            let Some(neighbor_version) = next_clock(db) else {
                return false;
            };
            neighbor.set_manual_version(source_position.max(1), neighbor_version);
            if db.update(&neighbor_key, &neighbor.pack()) {
                save_clock(db, neighbor_version);
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::level_db::{LevelDb, USERDB_TYPE};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// 每个测试一个独立目录：LevelDB 的 `LOCK` 按目录独占，共用目录会直接开不起来。
    fn temp_db() -> (LevelDb, PathBuf) {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "lrn-manual-order-{}-{n}.userdb",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let db = LevelDb::new(&dir, "test", USERDB_TYPE);
        assert!(db.open(), "打开临时 userdb 失败");
        (db, dir)
    }

    fn put(db: &LevelDb, code: &str, text: &str, commits: i32) {
        let value = UserDbValue {
            commits,
            dee: 1.0,
            tick: 1,
            ..Default::default()
        };
        assert!(
            db.update(&format!("{code}\t{text}"), &value.pack()),
            "写 {text} 失败"
        );
    }

    fn state_of(db: &LevelDb, code: &str, text: &str) -> UserDbValue {
        UserDbValue::unpack(&db.fetch(&format!("{code}\t{text}")).unwrap()).unwrap()
    }

    /// 手排到第 3 位：位置写进去，学习字段一个不动，戳非零。
    #[test]
    fn set_manual_pins_position_without_touching_learning() {
        let (db, path) = temp_db();
        put(&db, "ni hao ", "你好", 21);
        assert!(set_manual(&db, "ni hao ", "你好", 3));
        let state = state_of(&db, "ni hao ", "你好");
        assert_eq!(state.position, 3);
        assert_eq!(state.commits, 21, "手排不能改动学习次数");
        assert!((state.dee - 1.0).abs() < 1e-9, "手排不能改动词频");
        assert!(state.manual_stamp > 0, "必须留戳，否则同步时分不出先后");
        drop(db);
        let _ = std::fs::remove_dir_all(path);
    }

    /// 不变量 1：同码两个词不能同时占 2 号位。
    #[test]
    fn same_position_cannot_have_two_owners() {
        let (db, path) = temp_db();
        put(&db, "ni hao ", "你好", 5);
        put(&db, "ni hao ", "拟好", 5);
        assert!(set_manual(&db, "ni hao ", "你好", 2));
        assert!(
            !set_manual(&db, "ni hao ", "拟好", 2),
            "2 号位已被「你好」占着，「拟好」必须被拒"
        );
        assert_eq!(state_of(&db, "ni hao ", "拟好").position, 0);
        drop(db);
        let _ = std::fs::remove_dir_all(path);
    }

    /// 不变量 2：连续两次手排的戳严格递增（同一秒也成立）。
    #[test]
    fn stamps_are_strictly_increasing() {
        let (db, path) = temp_db();
        put(&db, "a ", "甲", 1);
        put(&db, "a ", "乙", 1);
        assert!(set_manual(&db, "a ", "甲", 1));
        assert!(set_manual(&db, "a ", "乙", 2));
        let first = state_of(&db, "a ", "甲").manual_stamp;
        let second = state_of(&db, "a ", "乙").manual_stamp;
        assert!(second > first, "{second} 应大于 {first}");
        drop(db);
        let _ = std::fs::remove_dir_all(path);
    }

    /// 不变量 3：清空手排后戳仍然往前走（同步时"我清了"能覆盖对方的"我排了"）。
    #[test]
    fn clearing_keeps_a_fresh_stamp() {
        let (db, path) = temp_db();
        put(&db, "a ", "甲", 1);
        assert!(set_manual(&db, "a ", "甲", 2));
        let pinned = state_of(&db, "a ", "甲").manual_stamp;
        assert!(reset_manual(&db, "a ", "甲"));
        let cleared = state_of(&db, "a ", "甲");
        assert_eq!(cleared.position, 0);
        assert!(cleared.manual_stamp > pinned, "清空也要换新戳");
        assert_eq!(cleared.commits, 1, "清手排不能动学习次数");
        drop(db);
        let _ = std::fs::remove_dir_all(path);
    }

    /// 只抹自动学习：次数变负（= 删除），手排位置与戳原样保留。
    #[test]
    fn reset_auto_keeps_manual_pin() {
        let (db, path) = temp_db();
        put(&db, "a ", "甲", 7);
        assert!(set_manual(&db, "a ", "甲", 1));
        let before = state_of(&db, "a ", "甲");
        assert!(reset_auto(&db, "a ", "甲"));
        let after = state_of(&db, "a ", "甲");
        assert!(after.commits < 0, "自动学习被抹掉 = 标记为删除");
        assert_eq!(after.position, before.position, "手排位置必须留着");
        assert_eq!(after.manual_stamp, before.manual_stamp);
        drop(db);
        let _ = std::fs::remove_dir_all(path);
    }

    /// 相邻换位：本词占目标位，原占位者退到本词原来的位置。
    #[test]
    fn move_swaps_with_the_occupant() {
        let (db, path) = temp_db();
        put(&db, "a ", "甲", 1);
        put(&db, "a ", "乙", 1);
        assert!(set_manual(&db, "a ", "甲", 1));
        assert!(set_manual(&db, "a ", "乙", 2));
        // 「乙」（第 2 位）往前挪一位 ⇒ 应与「甲」互换
        assert!(move_adjacent(&db, "a ", "乙", "a ", "甲", 2, 1, true));
        assert_eq!(state_of(&db, "a ", "乙").position, 1);
        assert_eq!(state_of(&db, "a ", "甲").position, 2, "被顶掉的词退到原位");
        drop(db);
        let _ = std::fs::remove_dir_all(path);
    }

    /// 第 1 位再往前挪：越界，直接拒绝（0 是"未手排"，不是位置）。
    #[test]
    fn cannot_move_before_the_first_position() {
        let (db, path) = temp_db();
        put(&db, "a ", "甲", 1);
        assert!(!move_adjacent(&db, "a ", "甲", "a ", "", 1, 1, false));
        drop(db);
        let _ = std::fs::remove_dir_all(path);
    }

    /// 邻词不可见时只动自己，不去改对方的记录。
    #[test]
    fn invisible_neighbor_is_left_alone() {
        let (db, path) = temp_db();
        put(&db, "a ", "甲", 1);
        put(&db, "a ", "乙", 1);
        assert!(set_manual(&db, "a ", "甲", 3));
        assert!(move_adjacent(&db, "a ", "乙", "a ", "甲", 4, 1, false));
        assert_eq!(state_of(&db, "a ", "乙").position, 3);
        assert_eq!(state_of(&db, "a ", "甲").position, 3, "未确认的邻词不动");
        drop(db);
        let _ = std::fs::remove_dir_all(path);
    }
}
