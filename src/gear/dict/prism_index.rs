//! 部署期棱镜索引：`<staging>/<dict 点号分段>/prisms.list`。
//!
//! 为什么需要它：多棱镜子方案的产物本来就在部署期按
//! `build/dictionaries/<dict>/algebra/<组名>` 一个个编出来
//! （`build/<dict>/<dict>.<组名>.prism.bin`），但运行时过去只能靠 schema 里
//! 手写的 `translator/prisms` 清单知道有哪些棱镜。索引把"实际产出的集合"
//! 落成一份文件，运行时直接读，**不再扫目录**，也不再需要第二份手写名单。
//!
//! 文件格式（一行一个组，按组名字典序；`\t` 分隔组名与文件名）：
//!
//! ```text
//! # rime prism index v1 count=2 mtime=639123456789012345
//! flypy	pinyin.flypy.prism.bin
//! zrm	pinyin.zrm.prism.bin
//! ```
//!
//! * `count` = 条目数，读侧必须与真实行数相等；
//! * `mtime` = 写入完成时该目录的 `modified()` 纳秒值。目录项增删会改变它、
//!   文件内容覆盖不会，正好对应"棱镜集合是否变了"。
//!
//! 读取是**纯降级**的：文件不存在、头不认识、行数不符、目录 mtime 对不上，
//! 一律返回 `None`，调用方回退到旧行为（只看显式 `prisms`），绝不报错、
//! 也不会因此少加载手写清单里的棱镜。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// 索引文件名（与棱镜同层）。
pub const INDEX_FILE: &str = "prisms.list";

const HEADER_PREFIX: &str = "# rime prism index v1";

/// 索引里的一条：组名（= 输入类型 option 名）与棱镜名（= 产物文件 stem）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrismEntry {
    /// 例：`zrm`
    pub group: String,
    /// 例：`pinyin.zrm`（对应 `pinyin.zrm.prism.bin`）
    pub prism: String,
}

/// 棱镜所在目录：`<build>/<dict 点号分段>`（与 `DictCompiler::extra_prism_target` 同规则）。
pub fn dict_dir(build_dir: &Path, dict_name: &str) -> PathBuf {
    build_dir.join(dict_name.replace('.', "/"))
}

/// 索引路径。
pub fn index_path(build_dir: &Path, dict_name: &str) -> PathBuf {
    dict_dir(build_dir, dict_name).join(INDEX_FILE)
}

/// 写入索引，返回实际登记的条目。
///
/// `groups` 是**声明集合**（裸组名，不含 `<dict>.` 前缀）；只登记
/// `<build>/<dict>/<dict>.<group>.prism.bin` 真实存在的组，因此声明里残留、
/// 但产物已被删除的组不会复活。按组名排序，保证同样的集合每次写出同样的文件。
pub fn write_index(
    build_dir: &Path,
    dict_name: &str,
    groups: &[String],
) -> Result<Vec<PrismEntry>, String> {
    let dir = dict_dir(build_dir, dict_name);
    let mut entries: Vec<PrismEntry> = groups
        .iter()
        .filter(|group| !group.is_empty())
        .map(|group| PrismEntry {
            group: group.clone(),
            prism: format!("{dict_name}.{group}"),
        })
        .filter(|entry| {
            dir.join(format!("{}.prism.bin", entry.prism)).is_file()
        })
        .collect();
    entries.sort_by(|a, b| a.group.cmp(&b.group));
    entries.dedup_by(|a, b| a.group == b.group);

    fs::create_dir_all(&dir).map_err(|err| format!("create_dir_all {}: {err}", dir.display()))?;
    let path = dir.join(INDEX_FILE);
    // 先写一次：若文件原本不存在，这一步会创建目录项并改变目录 mtime。
    // 再 stat 目录、把 mtime 写进头里，最后覆盖写一次（内容变更不改变目录 mtime）。
    fs::write(&path, render(&entries, 0))
        .map_err(|err| format!("write {}: {err}", path.display()))?;
    let mtime = dir_mtime_nanos(&dir)?;
    fs::write(&path, render(&entries, mtime))
        .map_err(|err| format!("write {}: {err}", path.display()))?;
    Ok(entries)
}

/// 删除本次**没有声明**的旧棱镜文件，返回删除个数。
///
/// 部署只会覆盖同名产物，不会清理上一次部署留下的文件；`input_types.enabled`
/// 缩小白名单后，旧组的 `<dict>.<组>.prism.bin` 必须一起消失，否则"删掉一个
/// 名字重部署后对应棱镜不再出现"不成立。主棱镜 `<dict>.prism.bin` 永远保留。
///
/// 必须在 [`write_index`] **之前**调用：删文件会改变目录 mtime，索引头里的
/// mtime 要反映最终状态。
pub fn prune_stale_prisms(build_dir: &Path, dict_name: &str, keep_groups: &[String]) -> usize {
    let dir = dict_dir(build_dir, dict_name);
    let Ok(read_dir) = fs::read_dir(&dir) else {
        return 0;
    };
    let prefix = format!("{dict_name}.");
    let mut removed = 0;
    for entry in read_dir.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".prism.bin") else {
            continue;
        };
        let Some(group) = stem.strip_prefix(&prefix) else {
            continue;
        };
        // 空组名 = 主棱镜（`<dict>.prism.bin`），不在白名单管理范围内。
        if group.is_empty() || keep_groups.iter().any(|keep| keep == group) {
            continue;
        }
        if fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 读取索引。任何异常形态都返回 `None`（调用方回退旧行为）。
pub fn read_index(build_dir: &Path, dict_name: &str) -> Option<Vec<PrismEntry>> {
    let dir = dict_dir(build_dir, dict_name);
    let text = fs::read_to_string(dir.join(INDEX_FILE)).ok()?;
    let mut lines = text.lines();
    let header = lines.next()?;
    let rest = header.strip_prefix(HEADER_PREFIX)?;
    let mut count: Option<usize> = None;
    let mut mtime: Option<u64> = None;
    for field in rest.split_whitespace() {
        if let Some(value) = field.strip_prefix("count=") {
            count = value.parse().ok();
        } else if let Some(value) = field.strip_prefix("mtime=") {
            mtime = value.parse().ok();
        }
    }
    let (count, mtime) = (count?, mtime?);

    let prefix = format!("{dict_name}.");
    let mut entries: Vec<PrismEntry> = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let (group, file) = line.split_once('\t')?;
        let prism = file.strip_suffix(".prism.bin")?;
        if group.is_empty() || prism.strip_prefix(&prefix)? != group {
            return None;
        }
        entries.push(PrismEntry {
            group: group.to_string(),
            prism: prism.to_string(),
        });
    }
    if entries.len() != count {
        return None;
    }
    if dir_mtime_nanos(&dir).ok()? != mtime {
        return None;
    }
    Some(entries)
}

fn render(entries: &[PrismEntry], mtime: u64) -> String {
    let mut out = format!(
        "{HEADER_PREFIX} count={} mtime={mtime}\n",
        entries.len()
    );
    for entry in entries {
        out.push_str(&format!("{}\t{}.prism.bin\n", entry.group, entry.prism));
    }
    out
}

fn dir_mtime_nanos(dir: &Path) -> Result<u64, String> {
    let modified = fs::metadata(dir)
        .and_then(|meta| meta.modified())
        .map_err(|err| format!("metadata {}: {err}", dir.display()))?;
    let duration = modified
        .duration_since(UNIX_EPOCH)
        .map_err(|err| format!("mtime {}: {err}", dir.display()))?;
    Ok(duration.as_nanos() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "librime-prism-index-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x").unwrap();
    }

    #[test]
    fn roundtrip_is_sorted_and_skips_missing_files() {
        let build = temp("roundtrip");
        let dir = dict_dir(&build, "pinyin");
        touch(&dir.join("pinyin.zrm.prism.bin"));
        touch(&dir.join("pinyin.flypy.prism.bin"));
        touch(&dir.join("pinyin.prism.bin")); // 主棱镜，不算组
        touch(&dir.join("sub/pinyin.sub.prism.bin")); // 子目录不参与本层

        let groups = vec![
            "zrm".to_string(),
            "flypy".to_string(),
            "missing".to_string(),
            "sub".to_string(),
        ];
        let written = write_index(&build, "pinyin", &groups).expect("write");
        assert_eq!(
            written,
            vec![
                PrismEntry {
                    group: "flypy".to_string(),
                    prism: "pinyin.flypy".to_string()
                },
                PrismEntry {
                    group: "zrm".to_string(),
                    prism: "pinyin.zrm".to_string()
                },
            ]
        );
        assert_eq!(read_index(&build, "pinyin"), Some(written));
        let _ = fs::remove_dir_all(&build);
    }

    #[test]
    fn empty_group_set_is_a_valid_index() {
        let build = temp("empty");
        let written = write_index(&build, "pinyin", &[]).expect("write");
        assert!(written.is_empty());
        assert_eq!(read_index(&build, "pinyin"), Some(Vec::new()));
        let _ = fs::remove_dir_all(&build);
    }

    #[test]
    fn missing_index_falls_back() {
        let build = temp("missing");
        fs::create_dir_all(dict_dir(&build, "pinyin")).unwrap();
        assert_eq!(read_index(&build, "pinyin"), None);
        let _ = fs::remove_dir_all(&build);
    }

    #[test]
    fn unknown_format_falls_back() {
        let build = temp("format");
        let dir = dict_dir(&build, "pinyin");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(INDEX_FILE), "not a prism index\nzrm\tpinyin.zrm.prism.bin\n").unwrap();
        assert_eq!(read_index(&build, "pinyin"), None);
        let _ = fs::remove_dir_all(&build);
    }

    #[test]
    fn wrong_entry_count_falls_back() {
        let build = temp("count");
        let dir = dict_dir(&build, "pinyin");
        fs::create_dir_all(&dir).unwrap();
        touch(&dir.join("pinyin.zrm.prism.bin"));
        write_index(&build, "pinyin", &["zrm".to_string()]).expect("write");
        let path = dir.join(INDEX_FILE);
        let text = fs::read_to_string(&path).unwrap();
        let mtime = text
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().find_map(|f| f.strip_prefix("mtime=")))
            .unwrap()
            .to_string();
        // 头说 2 条，实际只有 1 条。
        fs::write(
            &path,
            format!("# rime prism index v1 count=2 mtime={mtime}\nzrm\tpinyin.zrm.prism.bin\n"),
        )
        .unwrap();
        assert_eq!(read_index(&build, "pinyin"), None);
        let _ = fs::remove_dir_all(&build);
    }

    #[test]
    fn stale_mtime_falls_back() {
        let build = temp("stale");
        let dir = dict_dir(&build, "pinyin");
        touch(&dir.join("pinyin.zrm.prism.bin"));
        write_index(&build, "pinyin", &["zrm".to_string()]).expect("write");
        let path = dir.join(INDEX_FILE);
        let text = fs::read_to_string(&path).unwrap();
        let header = text.lines().next().unwrap();
        let count = header
            .split_whitespace()
            .find_map(|f| f.strip_prefix("count="))
            .unwrap();
        // 把目录 mtime 改成一个不可能的值：读侧必须判过期。
        fs::write(
            &path,
            format!("# rime prism index v1 count={count} mtime=1\nzrm\tpinyin.zrm.prism.bin\n"),
        )
        .unwrap();
        assert_eq!(read_index(&build, "pinyin"), None);
        let _ = fs::remove_dir_all(&build);
    }

    #[test]
    fn new_directory_entry_makes_index_stale() {
        let build = temp("newentry");
        let dir = dict_dir(&build, "pinyin");
        touch(&dir.join("pinyin.zrm.prism.bin"));
        write_index(&build, "pinyin", &["zrm".to_string()]).expect("write");
        assert!(read_index(&build, "pinyin").is_some());
        // 部署产物集合变了（新增一个目录项）→ 索引过期。
        fs::create_dir_all(dir.join("zrm")).unwrap();
        assert_eq!(read_index(&build, "pinyin"), None);
        let _ = fs::remove_dir_all(&build);
    }

    #[test]
    fn prune_removes_undeclared_group_prisms_only() {
        let build = temp("prune");
        let dir = dict_dir(&build, "pinyin");
        touch(&dir.join("pinyin.prism.bin"));
        touch(&dir.join("pinyin.zrm.prism.bin"));
        touch(&dir.join("pinyin.flypy.prism.bin"));
        touch(&dir.join("pinyin.table.bin"));
        touch(&dir.join("other.unknown.bin"));

        let removed = prune_stale_prisms(&build, "pinyin", &["zrm".to_string()]);

        assert_eq!(removed, 1);
        assert!(dir.join("pinyin.prism.bin").is_file(), "主棱镜必须保留");
        assert!(dir.join("pinyin.zrm.prism.bin").is_file(), "仍在白名单里的保留");
        assert!(!dir.join("pinyin.flypy.prism.bin").exists(), "不再声明的必须删掉");
        assert!(dir.join("pinyin.table.bin").is_file(), "非棱镜产物不动");
        assert!(dir.join("other.unknown.bin").is_file(), "别的词库不动");
        let _ = fs::remove_dir_all(&build);
    }
}
