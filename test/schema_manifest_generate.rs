//! 部署主链会**先造清单**：`<user>/schema/schema.yaml` 缺失时，由程序文件夹识别生成。
//!
//! 这是「首装 → 部署 → 能打字」这条链的第一环，端到端验一遍：
//! * 清单缺失 → 部署生成（只收过准入四关的）；
//! * 清单已有 → 部署一个字都不动（用户的选择是权威）；
//! * `default.yaml` 的 `schema_list` 不再当兜底。

use std::path::{Path, PathBuf};

use librime_next::lever::deployer::Deployer;
use librime_next::lever::deployment_tasks::load_schema_manifest;

fn temp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lrn-manifest-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// 合规方案：标准三行头 + `schema.id` 单数 + 摆放正确。
fn write_schema(root: &Path, id: &str, name: &str) {
    let dir = root.join("schema").join(id.replace('.', "/"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{id}.schema.yaml")),
        format!(
            "# Rime schema\n# encode: utf-8\n# newline: lf\n---\nschema:\n  id: {id}\n  name: \
             {name}\n...\n"
        ),
    )
    .unwrap();
}

/// 不合格方案：写的是 librime 原生 `schema_id`（方言不纯）→ 必须被抛弃。
fn write_native_id_schema(root: &Path, id: &str) {
    let dir = root.join("schema").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{id}.schema.yaml")),
        format!(
            "# Rime schema\n# encode: utf-8\n# newline: lf\n---\nschema:\n  schema_id: {id}\n  \
             name: 坏方案\n...\n"
        ),
    )
    .unwrap();
}

fn write_default_yaml(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        root.join("default.yaml"),
        "# Rime default\nconfig_version: \"1\"\n",
    )
    .unwrap();
}

fn manifest_path(user: &Path) -> PathBuf {
    user.join("schema").join("schema.yaml")
}

/// 首装：清单不存在 → 部署顺手生成，只点名过四关的方案。
#[test]
fn deploy_generates_manifest_when_missing() {
    let root = temp("missing");
    let shared = root.join("shared");
    let user = root.join("user");
    write_default_yaml(&shared);
    write_schema(&shared, "pinyin", "全拼");
    write_schema(&shared, "wubi86", "五笔86");
    write_native_id_schema(&shared, "broken");

    assert!(!manifest_path(&user).exists(), "首装时清单不该存在");

    // 编译那一段可能因为没词典而失败，但清单必须在此之前就生成好
    let mut deployer = Deployer::with_data_dirs(&shared, &user);
    let _ = deployer.deploy_workspace();

    let manifest = load_schema_manifest(&user);
    assert_eq!(
        manifest.schemas,
        vec!["pinyin".to_string(), "wubi86".to_string()],
        "只点名过准入检查的方案；`schema_id` 复数写法的 broken 不许进"
    );
    assert_eq!(manifest.active.as_deref(), Some("pinyin"), "active 优先标准样板 pinyin");
    assert!(manifest_path(&user).is_file(), "清单要落盘");

    let _ = std::fs::remove_dir_all(&root);
}

/// 已有清单：部署不许覆盖（用户勾了什么就是什么）。
#[test]
fn deploy_keeps_existing_manifest() {
    use librime_next::lever::admit;
    use librime_next::lever::deployment_tasks::SchemaManifest;

    let root = temp("existing");
    let shared = root.join("shared");
    let user = root.join("user");
    write_default_yaml(&shared);
    write_schema(&shared, "pinyin", "全拼");
    write_schema(&shared, "wubi86", "五笔86");

    // 用户只留了 wubi86
    admit::save_schema_manifest(
        &user,
        &SchemaManifest {
            active: Some("wubi86".into()),
            schemas: vec!["wubi86".into()],
        },
    )
    .unwrap();

    let mut deployer = Deployer::with_data_dirs(&shared, &user);
    let _ = deployer.deploy_workspace();

    let manifest = load_schema_manifest(&user);
    assert_eq!(manifest.schemas, vec!["wubi86".to_string()], "用户的选择不许被部署改写");
    assert_eq!(manifest.active.as_deref(), Some("wubi86"));

    let _ = std::fs::remove_dir_all(&root);
}

/// 程序文件夹一个合规方案都没有 → 不生成空清单（那等于把用户的选择清空）。
#[test]
fn deploy_writes_nothing_when_nothing_admitted() {
    let root = temp("nothing");
    let shared = root.join("shared");
    let user = root.join("user");
    write_default_yaml(&shared);
    write_native_id_schema(&shared, "broken");

    let mut deployer = Deployer::with_data_dirs(&shared, &user);
    let _ = deployer.deploy_workspace();

    assert!(!manifest_path(&user).exists(), "没有合规方案时不写空清单");
    assert!(load_schema_manifest(&user).is_empty());

    let _ = std::fs::remove_dir_all(&root);
}
