//! lever 部署层集成测试：`WorkspaceUpdate` 在**真实数据目录**上跑一遍。
//!
//! 与 `key_order.rs` 同样的约定：`%APPDATA%/Rime-Next` 或共享目录不在（换机器 / CI）
//! 时全部跳过，不失败。staging 输出指向临时目录，不碰真实 `build/`。
//!
//! 验证点（写侧契约）：
//! * `schema/schema.toml` 的每个方案都产出 `staging/<id>/<id>.schema.yaml`（系列布局）；
//! * `staging/default.yaml` 的 `schema_list` 与 `schema.toml` 一致（写回逻辑）；
//! * dependencies 递归编译（被依赖方案的产物也在）；
//! * **`.bin` 三件套**（2026-10-25 接上 dict 层后新增）：编译产物里
//!   `translator/dictionary` 非空的方案，必须产出 `staging/<系列…>/<id>.{table,prism,reverse}.bin`。

use std::path::{Path, PathBuf};

use librime_next::lever::deployment_tasks::{DeploymentTask, load_schema_ids_from_toml};
use librime_next::lever::deployer::Deployer;
use librime_next::load_file;

/// 系列链拼接（与 `dict_compiler::relocate_target` / 读侧分类同规则：
/// id 的每个点号一级文件夹）。真实数据的 id 都无点号，这里仍然按通用规则算。
fn series_join(root: &Path, resource_id: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    for segment in resource_id.split('.') {
        if !segment.is_empty() {
            path = path.join(segment);
        }
    }
    path
}

fn user_data() -> Option<PathBuf> {
    let dir = std::env::var_os("APPDATA")?;
    let path = PathBuf::from(dir).join("Rime-Next");
    path.is_dir().then_some(path)
}

/// 共享数据目录优先取仓库里的源布局（`C:/code/Weasel-Next/Rime-Next`），
/// 它比装机版更新（本次系列文件夹迁移就发生在那里）。
fn shared_data() -> Option<PathBuf> {
    for candidate in [
        r"C:\code\Weasel-Next\Rime-Next",
        r"C:\Program Files\Weasel-Next\Rime-Next",
    ] {
        let path = PathBuf::from(candidate);
        if path.join("default.yaml").is_file() && path.join("schema").is_dir() {
            return Some(path);
        }
    }
    None
}

#[test]
fn workspace_update_writes_staging_per_schema() {
    let Some(user) = user_data() else {
        eprintln!("skip: no APPDATA/Rime-Next");
        return;
    };
    let Some(shared) = shared_data() else {
        eprintln!("skip: no shared data dir");
        return;
    };
    let schema_ids = load_schema_ids_from_toml(&user);
    assert!(!schema_ids.is_empty(), "schema.toml 应该有清单");

    // staging 指到临时目录：不碰真实 build/，也能反复跑。
    // ⚠️ 目录名必须是 `build` 才会被判定为 staging 根（真实场景就是 <user>/build），
    // 否则系列分层解析不生效（见 paths::is_staging_root 的契约）。
    let staging = std::env::temp_dir().join(format!("librime-next-deploy-test-{}/build", std::process::id()));
    let _ = std::fs::remove_dir_all(staging.parent().unwrap());

    let mut deployer = Deployer::with_data_dirs(&shared, &user);
    deployer.staging_dir = staging.clone();

    let ok = DeploymentTask::WorkspaceUpdate.run(&mut deployer);
    let default_yaml = std::fs::read_to_string(staging.join("default.yaml")).unwrap_or_default();

    // 清单里的每个方案都要有产物（写侧契约定死权）
    let mut missing = Vec::new();
    for id in &schema_ids {
        let product = staging.join(id).join(format!("{id}.schema.yaml"));
        if !product.is_file() {
            missing.push(id.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "staging 缺少产物: {missing:?}\nstaging={}",
        staging.display()
    );

    // schema_list 写回
    for id in &schema_ids {
        assert!(
            default_yaml.contains(id),
            "staging/default.yaml 的 schema_list 缺少 {id}"
        );
    }

    // dependencies 递归：yuling 依赖 yuhao_pinyin，产物应在（即便不在清单里也有）
    let dep_product = staging.join("yuhao_pinyin").join("yuhao_pinyin.schema.yaml");
    assert!(
        dep_product.is_file() || !schema_ids.iter().any(|id| id == "yuling"),
        "yuling 的依赖 yuhao_pinyin 未被递归编译"
    );

    // `.bin` 三件套：编译产物里 translator/dictionary 非空 → 表/反查按**词典名**、
    // prism 按 **prism 名**（缺省 = 词典名）落位。多个方案共享同一词典时表只建一次，
    // 位置是 `<系列…>/<词典名>.{table,reverse}.bin`，不是 schema id 名下。
    // 缺任何一件都说明 dict 编译链（entry_collector → table/prism/reverse db）断了。
    let mut missing_bins = Vec::new();
    for id in &schema_ids {
        let compiled = series_join(&staging, id).join(format!("{id}.schema.yaml"));
        let Ok(config) = load_file(&compiled) else {
            continue;
        };
        let Some(dict_name) = config
            .pointer("/translator/dictionary")
            .and_then(|name| name.as_str())
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        let prism_name = config
            .pointer("/translator/prism")
            .and_then(|name| name.as_str())
            .filter(|name| !name.is_empty())
            .unwrap_or(dict_name);
        for (name, suffix) in [(dict_name, "table"), (dict_name, "reverse"), (prism_name, "prism")] {
            let bin = series_join(&staging, name).join(format!("{name}.{suffix}.bin"));
            if !bin.is_file() {
                missing_bins.push(format!("{name}.{suffix}.bin"));
            }
        }
    }
    assert!(
        missing_bins.is_empty(),
        "有词典的方案缺少 .bin 产物: {missing_bins:?}"
    );
    assert!(ok, "WorkspaceUpdate 应报告成功（任一方案编译失败即为 false）");

    eprintln!("WorkspaceUpdate ok={ok} schemas={} staging={}", schema_ids.len(), staging.display());
    let _ = std::fs::remove_dir_all(staging.parent().unwrap());
}
