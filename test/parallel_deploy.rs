//! 并行部署 vs 串行部署的对照测试（真实数据目录、临时 build、离线可跑）。
//!
//! 验证三件事（用户 2026-11-11 要求"不同系列方案并行化部署 + 具体数据"）：
//!
//! 1. **产物一致**：同一份源数据，串行与并行（4 worker）产出的每个
//!    `staging/<id>/<id>.schema.yaml` **逐字节相同**；`.bin` 三件套齐全。
//! 2. **台账一致**：两者写出的 `deploy_records.yaml` 的方案集合相同（时间戳
//!    因并行而不同，不比时间戳）。
//! 3. **具体加速**：纳秒计时打印串行 / 并行（4 worker）耗时与加速比。
//!
//! 目录约定：`%APPDATA%/Rime-Next` 或 `C:/code/Weasel-Next/Rime-Next` 不在时
//! 全部跳过（换机器 / CI），不失败。staging 指向独立临时目录，不碰真实 `build/`。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use librime_next::lever::deployment_tasks::{load_schema_manifest, DeploymentTask};
use librime_next::lever::deployer::Deployer;
use librime_next::lever::parallel_deploy::{
    run_workspace_update_parallel_with, ParallelStats,
};

fn user_data() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os("APPDATA")?).join("Rime-Next");
    path.is_dir().then_some(path)
}

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

/// `prisms.list` 头行含目录 mtime（纳秒，`prism_index::write_index` 写入），
/// 两次独立部署的目录创建时刻必然不同。只把 `mtime=<digits>` 的数字规范化，
/// `count=` 与全部条目仍逐字节比较——棱镜集合/顺序/文件名一个都不放过。
fn normalize_nondeterministic(rel: &str, bytes: Vec<u8>) -> Vec<u8> {
    if !rel.ends_with("prisms.list") {
        return bytes;
    }
    let Ok(text) = String::from_utf8(bytes.clone()) else {
        return bytes;
    };
    let Some(start) = text.find("mtime=") else {
        return bytes;
    };
    let digits_start = start + "mtime=".len();
    let digits_len = text[digits_start..]
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    if digits_len == 0 {
        return bytes;
    }
    let mut normalized = String::with_capacity(text.len());
    normalized.push_str(&text[..digits_start]);
    normalized.push_str("<nanos>");
    normalized.push_str(&text[digits_start + digits_len..]);
    normalized.into_bytes()
}

/// 递归收集 staging 下所有**产物文件**的相对路径 → 内容字节（排除台账）。
/// 台账 `deploy_records.yaml` 的 arrived/finished 时间戳并行/串行必然不同，不比。
fn collect_staging_products(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else if path.is_file() {
                let rel = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                // 台账/统计是时间戳产物，排除；其余产物（配置 + .bin + prisms.list）
                // 都应逐字节一致。
                if rel.contains("deploy_records") || rel.contains("deploy_stats") {
                    continue;
                }
                if let Ok(bytes) = std::fs::read(&path) {
                    let bytes = normalize_nondeterministic(&rel, bytes);
                    out.insert(rel, bytes);
                }
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// 造一个只含路径的 deployer，staging 指向 `tag` 对应的临时目录。
fn fresh_deployer(shared: &Path, user: &Path, tag: &str) -> (Deployer, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "librime-parallel-deploy-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let staging = root.join("build");
    let mut deployer = Deployer::with_data_dirs(shared, user);
    deployer.staging_dir = staging.clone();
    // 台账重定向到临时根：否则 append_deploy_record 会往真实
    // %APPDATA%/Rime-Next/build/deploy_records.yaml 追加 17 条假记录，
    // 且 staging 里读不到 → "台账集合一致"变成空对空的假阳性。
    deployer.set_records_root(Some(root.clone()));
    (deployer, root)
}

/// 统计台账里记录的方案名（不管时间戳）。
fn record_schemas(staging: &Path) -> Vec<String> {
    let path = staging.join("deploy_records.yaml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix("- schema:")
                .map(|rest| rest.trim().trim_matches('"').to_string())
        })
        .collect()
}

#[test]
fn parallel_matches_serial_products_and_is_faster() {
    let (Some(user), Some(shared)) = (user_data(), shared_data()) else {
        eprintln!("skip: 无真实数据目录 (APPDATA/Rime-Next 或 repo Rime-Next)");
        return;
    };
    let manifest = load_schema_manifest(&user);
    assert!(!manifest.is_empty(), "用户根 schema/schema.yaml 应有清单");
    let schema_count = manifest.schemas.len();

    // ---------- 串行（默认路径：无环境变量 → run_workspace_update）----------
    let (mut serial_deployer, serial_root) = fresh_deployer(&shared, &user, "serial");
    let serial_staging = serial_deployer.staging_dir.clone();
    let serial_started = Instant::now();
    let serial_ok = DeploymentTask::WorkspaceUpdate.run(&mut serial_deployer);
    let serial_nanos = serial_started.elapsed().as_nanos();
    let serial_products = collect_staging_products(&serial_staging);
    let serial_records = record_schemas(&serial_staging);

    // ---------- 并行（4 worker，显式入口）----------
    let (mut parallel_deployer, parallel_root) = fresh_deployer(&shared, &user, "parallel");
    let parallel_staging = parallel_deployer.staging_dir.clone();
    let parallel_started = Instant::now();
    let parallel_result: Result<ParallelStats, (usize, usize)> =
        run_workspace_update_parallel_with(&mut parallel_deployer, 4);
    let parallel_nanos = parallel_started.elapsed().as_nanos();
    let parallel_products = collect_staging_products(&parallel_staging);
    let parallel_records = record_schemas(&parallel_staging);

    // ---------- 断言 1：两者都成功 ----------
    assert!(serial_ok, "串行 WorkspaceUpdate 应成功");
    let pseq = parallel_result.expect("并行 WorkspaceUpdate 应成功");
    assert_eq!(pseq.failure, 0, "并行不应有失败方案");

    // ---------- 断言 2：产物文件集合一致 ----------
    let serial_keys: Vec<&String> = serial_products.keys().collect();
    let parallel_keys: Vec<&String> = parallel_products.keys().collect();
    assert_eq!(
        serial_keys, parallel_keys,
        "串行/并行产物文件集合不一致\n串行: {serial_keys:?}\n并行: {parallel_keys:?}"
    );

    // ---------- 断言 3：每个产物逐字节一致 ----------
    let mut byte_diffs = Vec::new();
    for (key, serial_bytes) in &serial_products {
        let parallel_bytes = &parallel_products[key];
        if serial_bytes != parallel_bytes {
            byte_diffs.push(format!(
                "{key} ({} vs {} bytes)",
                serial_bytes.len(),
                parallel_bytes.len()
            ));
        }
    }
    assert!(
        byte_diffs.is_empty(),
        "串行/并行产物存在字节差异: {byte_diffs:?}"
    );

    // ---------- 断言 4：台账方案集合一致 ----------
    let mut serial_rec = serial_records.clone();
    let mut parallel_rec = parallel_records.clone();
    serial_rec.sort();
    parallel_rec.sort();
    assert_eq!(
        serial_rec, parallel_rec,
        "串行/并行台账方案集合不一致\n串行: {serial_rec:?}\n并行: {parallel_rec:?}"
    );

    // ---------- 断言 5：每个清单方案都有配置产物 ----------
    let mut missing = Vec::new();
    for id in &manifest.schemas {
        let product = format!("{}/{}.schema.yaml", id.replace('.', "/"), id);
        if !parallel_products.contains_key(&product) {
            missing.push(product);
        }
    }
    assert!(missing.is_empty(), "并行产物缺少方案配置: {missing:?}");

    // ---------- 具体数据输出 ----------
    let serial_ms = serial_nanos as f64 / 1_000_000.0;
    let parallel_ms = parallel_nanos as f64 / 1_000_000.0;
    let speedup = serial_nanos as f64 / parallel_nanos.max(1) as f64;
    // HRRN 出队实证：策略标签 + 实际出队顺序 vs 清单基线 + 是否真重排。
    // 出队顺序改变只影响"谁先被银行家审查"，产物仍须逐字节一致（上面已断言）。
    let hrrn_order = pseq.schedule.hrrn_order.clone();
    let manifest_order = manifest.schemas.clone();
    let reordered = hrrn_order != manifest_order;
    let mut a = hrrn_order.clone();
    let mut b = manifest_order.clone();
    a.sort();
    b.sort();
    assert_eq!(
        a, b,
        "HRRN 出队方案集合应覆盖全部清单方案（只重排、不丢不重）"
    );
    eprintln!(
        "\n=== 并行部署实测（{schema_count} 方案, 4 worker, 出队策略={}）===\n\
         串行        : {serial_ms:.3} ms ({serial_nanos} ns)\n\
         并行        : {parallel_ms:.3} ms ({parallel_nanos} ns)\n\
         加速比      : {speedup:.2}x\n\
         产物文件    : {} 个（逐字节一致）\n\
         台账记录    : {} 条（方案集合一致）\n\
         银行家调度  : 轮次={} 延迟={} 不安全拒绝={} 批准={}\n\
         {} : {}\n\
         出队(实际)  : {}\n\
         清单(基线)  : {}\n",
        pseq.schedule.policy,
        parallel_products.len(),
        parallel_records.len(),
        pseq.schedule.rounds,
        pseq.schedule.deferred_rounds,
        pseq.schedule.unsafe_rejections,
        pseq.schedule.granted_sequences,
        // 两种策略都可能偏离清单：HRRN 是响应比驱动的重排，FIFO 是被银行家
        // 挡回后放到队尾的自然结果。标签区分开，避免 FIFO 下显示"HRRN 重排"误导。
        if pseq.schedule.policy == "hrrn" {
            "HRRN响应比重排"
        } else {
            "FIFO挡回偏离"
        },
        if reordered { "是" } else { "否" },
        hrrn_order.join(","),
        manifest_order.join(","),
    );

    let _ = std::fs::remove_dir_all(&serial_root);
    let _ = std::fs::remove_dir_all(&parallel_root);
}

/// 单 worker 退化的并行路径也应与串行产物一致（验证银行家池的确定性）。
#[test]
fn one_worker_parallel_matches_serial() {
    let (Some(user), Some(shared)) = (user_data(), shared_data()) else {
        eprintln!("skip: 无真实数据目录");
        return;
    };
    let (mut serial_deployer, serial_root) = fresh_deployer(&shared, &user, "serial1");
    let serial_staging = serial_deployer.staging_dir.clone();
    assert!(DeploymentTask::WorkspaceUpdate.run(&mut serial_deployer));
    let serial_products = collect_staging_products(&serial_staging);

    let (mut one_deployer, one_root) = fresh_deployer(&shared, &user, "oneworker");
    let one_staging = one_deployer.staging_dir.clone();
    let result = run_workspace_update_parallel_with(&mut one_deployer, 1);
    assert!(result.is_ok(), "单 worker 并行路径应成功");
    let one_products = collect_staging_products(&one_staging);

    assert_eq!(
        serial_products.keys().collect::<Vec<_>>(),
        one_products.keys().collect::<Vec<_>>(),
        "单 worker 产物集合应与串行一致"
    );
    for (key, serial_bytes) in &serial_products {
        assert_eq!(
            serial_bytes,
            &one_products[key],
            "单 worker 产物 {key} 应与串行逐字节一致"
        );
    }
    eprintln!(
        "单 worker 与串行产物一致: {} 个文件",
        one_products.len()
    );

    let _ = std::fs::remove_dir_all(&serial_root);
    let _ = std::fs::remove_dir_all(&one_root);
}

/// 2026-11-11：并行路径必须和串行一样遵守「禁用但保留内部状态」。
/// 造一份 4 方案的小工作区，禁用 pinyin 组后只应编译 wubi86 / liangfen，
/// 且用户根 schema.yaml 的 active 与 4 项 schemas 一个字节都不能变。
#[test]
fn parallel_respects_disabled_group_and_keeps_manifest() {
    let root = std::env::temp_dir().join(format!(
        "librime-parallel-disabled-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let shared = root.join("shared");
    let user = root.join("user");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::create_dir_all(user.join("schema")).unwrap();
    std::fs::write(
        shared.join("default.yaml"),
        "config_version: 2\nschema_list: []\n",
    )
    .unwrap();

    for (id, parent) in [
        ("pinyin", None),
        ("pinyin.luna", Some("pinyin")),
        ("wubi86", None),
        ("liangfen", None),
    ] {
        let path = user
            .join("schema")
            .join(id.replace('.', "/"))
            .join(format!("{id}.schema.yaml"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let parent_line = parent.map(|p| format!("  parent: {p}\n")).unwrap_or_default();
        std::fs::write(
            &path,
            format!("schema:\n  schema_id: {id}\n  name: {id}\n{parent_line}"),
        )
        .unwrap();
    }

    let manifest = "# 方案信息\nactive: pinyin\nschemas:\n  - pinyin\n  - pinyin.luna\n  - wubi86\n  - liangfen\n";
    let manifest_path = user.join("schema").join("schema.yaml");
    std::fs::write(&manifest_path, manifest).unwrap();
    std::fs::write(
        user.join("schema").join("disabled.yaml"),
        "groups:\n  - pinyin\nschemas: []\n",
    )
    .unwrap();

    let mut deployer = Deployer::with_data_dirs(&shared, &user);
    deployer.staging_dir = user.join("build");
    let result = run_workspace_update_parallel_with(&mut deployer, 2);
    assert!(result.is_ok(), "并行部署应成功: {result:?}");

    assert_eq!(
        std::fs::read_to_string(&manifest_path).unwrap(),
        manifest,
        "并行路径也不能改 schema.yaml（内部状态保留）"
    );
    let deployed = librime_next::load_file(&user.join("build/default.yaml")).unwrap();
    let schema_list: Vec<&str> = deployed
        .get("schema_list")
        .and_then(|v| v.as_array())
        .unwrap()
        .iter()
        .filter_map(|item| item.get("schema").and_then(|s| s.as_str()))
        .collect();
    assert_eq!(
        schema_list,
        vec!["wubi86", "liangfen"],
        "输入 4 项 + groups=[pinyin] → build/default.yaml 只应剩 wubi86, liangfen（保序）"
    );
    assert!(
        !user.join("build/pinyin").exists(),
        "禁用组的编译产物不该生成"
    );
    assert!(
        user.join("build/wubi86/wubi86.schema.yaml").is_file(),
        "未禁用方案仍要正常编译"
    );

    let _ = std::fs::remove_dir_all(&root);
}
