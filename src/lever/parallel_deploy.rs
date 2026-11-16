//! 不同系列方案的并行化部署 —— 用 [`super::banker::BankerPool`] 做资源准入。
//!
//! # 为什么串行慢（2026-11-11 五轮实测）
//!
//! `run_workspace_update` 的旧路径逐方案 `task.run(deployer)`：配置编译 + 词典编译
//! 串在一条线上。增量部署 15.231s 里约 10.6s 是"每方案 ~587ms 的配置层 gap"
//! （`config::compile` 的加载/展开/IO），词典段只占 2.3s。单个方案的配置编译
//! **彼此不共享写**（各写 `staging/<系列>/<id>/<id>.schema.yaml`），是天然的并行点。
//!
//! # 两阶段波次 + 银行家准入
//!
//! ```text
//! 波次 wave[0] = 清单里所有方案
//!   阶段 A（配置）：所有方案并行 compile_schema_config（各写各自 staging 配置）
//!                  → 收集每个 staged 的 schema/dependencies
//!       ↓ 依赖发现的下一波方案并入 wave[1]
//!   阶段 B（产物）：读每个 staged，提取它用到的 dict 集合与 aux group 集合，
//!                  注册进 BankerPool（worker 槽 + dict:<name> + aux:<group>），
//!                  逐个 request 准入，Granted 的并行 compile_schema_payload，
//!                  完成后 release；Wait/Unsafe 的下一轮重试。
//! 波次 wave[1] = 新发现的依赖方案（去重）→ 同两步
//! ...直到没有新依赖
//! ```
//!
//! # 银行家算法在这里防的是什么（真实死锁，不是摆设）
//!
//! 阶段 B 每个任务同时占 **1 个 worker 槽** + 它引用的**每本词典锁** +
//! **每个辅助码产物锁**。共享产物冲突真实存在：
//!
//! * `qiaoma` 与 `qiaoma_jf` 共用 `dictionary: qiaoma` → 同一 `qiaoma.table.bin`；
//! * `double_pinyin.flypy` 与 `ziranma` 共用 `pinyin.luna` → 同一 `luna` 棱镜；
//! * `prism_index` 写同一个 `<dict>/prisms.list`；辅助码写同一个 `<group>.aux.bin`。
//!
//! 资源竞争能形成环等待（任务 X 持 worker-1 等 `dict:qiaoma`，任务 Y 持
//! `dict:qiaoma` 等 worker-2）。银行家池在每次分配前跑安全序列检查，只批准
//! "分配后所有在途任务仍能完成"的请求；不安全则回滚、让任务等待。同一本词典
//! 的产物因此**必然串行**（锁总量=1），不同词典则尽可能并行。
//!
//! # 串行回退与产物一致性
//!
//! * 启用开关（见 [`is_enabled_for`]）：优先环境变量 `WEASEL_DEPLOY_PARALLEL`
//!   （非空且不为 `0`/`false`/`off`），其次 `<user>/weasel.yaml` 顶层 `deploy.parallel`；
//!   均未配置则默认关闭，仍走 [`super::deployment_tasks`] 串行路径，行为零变化。
//!   `WEASEL_DEPLOY_WORKERS` / `deploy.workers` 同理控制并发度（上限 6）。
//! * 两阶段调用的 `compile_schema_config` / `compile_schema_payload` 与串行路径是
//!   **同一组函数**（`deployment_tasks.rs` 拆出的），故产物逐字节一致。
//! * 台账 `append_deploy_record` 由主线程按清单顺序串行写，消除读-改-写竞态。
//! * `max_workers == 1` 退化为串行，用于对照测试。
//!
//! # 线程安全边界
//!
//! 编译链对 `Deployer` **只读** 4 个路径字段，无写操作；`config::compile` 内部
//! 用 `Rc<RefCell<Item>>` 但不跨线程传 `Item`（worker 内编译完即写盘丢弃）；
//! 日志是 `eprintln!`（stderr 自带锁）；`load_file` 无全局缓存。故每个 worker
//! 线程持 [`DeployPaths`] 快照构造局部 `Deployer` 调用编译函数是安全的。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Instant;

use super::banker::{BankerError, BankerPool, RequestOutcome};
use super::deployment_tasks::{
    compile_schema_config, compile_schema_payload, load_schema_manifest, DeploymentTask,
};
use super::tree::{checksum, log_error, log_info, log_warn};
use super::deployer::{Deployer, wall_now_ms};

/// 从 `<user>/weasel.yaml` 顶层 `deploy:` 段读出的并行部署配置。
///
/// 用**零依赖的逐行扫描**而非 YAML 库：`weasel.yaml` 是宿主与设置界面共用、
/// 会被其它进程并发改写的文件，部署器只读不写，且只认顶层 `deploy:` 段下的
/// 两个标量键（`parallel` / `workers`）。解析失败或键缺失一律回退到默认，
/// 绝不让配置文件的问题影响部署能否进行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct DeployConfig {
    parallel: Option<bool>,
    workers: Option<usize>,
    /// 阶段 B 的出队策略：true = HRRN（默认），false = 清单 FIFO（对照用）。
    hrrn: Option<bool>,
}

/// 把标量文本按 YAML 布尔语义解析（`true/yes/on/1` 为真，`false/no/off/0` 为假）。
fn parse_bool(value: &str) -> Option<bool> {
    let v = value.trim().trim_matches(['\'', '"']).to_ascii_lowercase();
    match v.as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

/// 逐行扫描 `weasel.yaml` 顶层 `deploy:` 段，抽出 `parallel` / `workers`。
///
/// 只认**无缩进**的 `deploy:` 作为段起点，段内在缩进 1~7 空格的 `键: 值`；
/// 遇到下一个无缩进的顶层键即停。注释行（`#`）与空行忽略。
fn read_deploy_config(user_data_dir: &Path) -> DeployConfig {
    let path = user_data_dir.join("weasel.yaml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => return DeployConfig::default(),
    };
    let mut cfg = DeployConfig::default();
    let mut in_section = false;
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start_matches(' ').len();
        let body = line.trim();
        if indent == 0 {
            // 顶层键：只有 `deploy:` 会开启段，其余顶层键结束段。
            in_section = body.starts_with("deploy:") || body == "deploy";
            continue;
        }
        if !in_section || !(1..=7).contains(&indent) {
            continue;
        }
        let Some((key, value)) = body.split_once(':') else {
            continue;
        };
        // 去掉行内注释（`value # comment`），保留引号内的 `#`。
        let value = strip_inline_comment(value).trim();
        match key.trim() {
            "parallel" => cfg.parallel = parse_bool(value),
            "hrrn" => cfg.hrrn = parse_bool(value),
            "workers" => {
                cfg.workers = value
                    .trim_matches(['\'', '"'])
                    .parse::<usize>()
                    .ok()
                    .filter(|&n| n > 0)
            }
            _ => {}
        }
    }
    cfg
}

/// 去掉 YAML 行内注释：`key: 4 # 说明` → `4`。引号内的 `#` 不当注释。
fn strip_inline_comment(value: &str) -> &str {
    let mut quote: Option<char> = None;
    for (i, ch) in value.char_indices() {
        match ch {
            '\'' | '"' if quote.is_none() => quote = Some(ch),
            '\'' | '"' if quote == Some(ch) => quote = None,
            '#' if quote.is_none() && (i == 0 || value[..i].ends_with(char::is_whitespace)) => {
                return &value[..i];
            }
            _ => {}
        }
    }
    value
}

/// 开关优先级的**纯函数**：环境变量 > 配置文件 > 默认关闭（串行路径零风险）。
/// 抽出来是为了确定性单测——不碰进程全局环境变量。
fn is_enabled_from(env: Option<&str>, configured: Option<bool>) -> bool {
    match env {
        Some(v) => {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && v != "0" && v != "false" && v != "off"
        }
        None => configured.unwrap_or(false),
    }
}

/// 并发上限的**纯函数**：环境变量 > 配置文件 > CPU 核数，硬封顶 6。
fn max_workers_from(env: Option<&str>, configured: Option<usize>, cpus: usize) -> usize {
    let from_env = env
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0);
    from_env.or(configured).unwrap_or(cpus).min(6)
}

/// 并发上限。优先级：环境变量 `WEASEL_DEPLOY_WORKERS` > `weasel.yaml` 的
/// `deploy.workers` > min(CPU 核数, 6)。上限硬封顶 6。
pub fn max_workers_for(user_data_dir: &Path) -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let cfg = read_deploy_config(user_data_dir);
    let env = std::env::var("WEASEL_DEPLOY_WORKERS").ok();
    max_workers_from(env.as_deref(), cfg.workers, cpus)
}

/// 是否启用并行部署。优先级：环境变量 `WEASEL_DEPLOY_PARALLEL` >
/// `weasel.yaml` 的 `deploy.parallel` > 默认关闭（串行路径零风险）。
pub fn is_enabled_for(user_data_dir: &Path) -> bool {
    let cfg = read_deploy_config(user_data_dir);
    let env = std::env::var("WEASEL_DEPLOY_PARALLEL").ok();
    is_enabled_from(env.as_deref(), cfg.parallel)
}

/// HRRN 出队开关的**纯函数**：环境变量 > 配置文件 > 默认开启。
/// 并行部署默认就用最高响应比出队（这是并行的应有之义）；
/// `WEASEL_DEPLOY_HRRN=0` 或 `deploy.hrrn: false` 才退回清单 FIFO（对照测试用）。
fn hrrn_enabled_from(env: Option<&str>, configured: Option<bool>) -> bool {
    match env {
        Some(v) => {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && v != "0" && v != "false" && v != "off"
        }
        None => configured.unwrap_or(true),
    }
}

/// 阶段 B 是否用 HRRN 出队（默认 true）。优先级同 `is_enabled_for`。
pub fn hrrn_enabled_for(user_data_dir: &Path) -> bool {
    let cfg = read_deploy_config(user_data_dir);
    let env = std::env::var("WEASEL_DEPLOY_HRRN").ok();
    hrrn_enabled_from(env.as_deref(), cfg.hrrn)
}

/// 无路径兼容入口：从进程启动目录下的用户根读配置。
/// 生产部署一律走 [`is_enabled_for`]（由 `deployer.user_data_dir` 提供准确路径）。
fn is_enabled() -> bool {
    is_enabled_for(Path::new("."))
}

fn max_workers() -> usize {
    max_workers_for(Path::new("."))
}

/// 编译链需要的**纯路径快照**（`Clone + Send + Sync`），供 worker 构造局部 `Deployer`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployPaths {
    pub program_dir: PathBuf,
    pub shared_data_dir: PathBuf,
    pub user_data_dir: PathBuf,
    pub staging_dir: PathBuf,
    pub prebuilt_data_dir: PathBuf,
}

impl DeployPaths {
    pub fn from_deployer(deployer: &Deployer) -> Self {
        Self {
            program_dir: deployer.program_dir.clone(),
            shared_data_dir: deployer.shared_data_dir.clone(),
            user_data_dir: deployer.user_data_dir.clone(),
            staging_dir: deployer.staging_dir.clone(),
            prebuilt_data_dir: deployer.prebuilt_data_dir.clone(),
        }
    }

    fn local_deployer(&self) -> Deployer {
        let mut local =
            Deployer::with_data_dirs(self.shared_data_dir.clone(), self.user_data_dir.clone());
        local.set_program_dir(self.program_dir.clone());
        local.staging_dir = self.staging_dir.clone();
        local.prebuilt_data_dir = self.prebuilt_data_dir.clone();
        local
    }

    /// staged 编译产物的路径：`<staging>/<id>/<id>.schema.yaml`。
    fn staged_schema(&self, schema_id: &str) -> PathBuf {
        self.staging_dir
            .join(schema_id)
            .join(format!("{schema_id}.schema.yaml"))
    }
}

/// 一个待编译方案的静态描述（线程间只读传递）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct CompileJob {
    id: String,
    source: PathBuf,
    /// 该方案引用的词典名集合（主 + 变体），从 staged 提取。
    dicts: Vec<String>,
    /// 该方案声明的辅助码组名集合，从 staged 提取。
    aux_groups: Vec<String>,
}

/// 阶段 A 的结果。
#[derive(Debug, Clone)]
struct ConfigOutcome {
    id: String,
    ok: bool,
    dependencies: Vec<String>,
    /// 阶段 A 配置编译的真实耗时（毫秒，worker 内 `Instant` 实测）。
    /// 首次部署（无历史 est 账本）时作为 HRRN 预计耗时 `est_ms` 的回退值。
    config_ms: u64,
    /// 该方案配置编译的**完成时刻**（单调时钟）。阶段 B 用它作为 HRRN 的到达
    /// 时刻：并行 A 阶段各方案完成有先后，首轮等待时间就真实分化，不会全为 0。
    ready_at: Instant,
}

/// 阶段 B 的结果。
#[derive(Debug, Clone)]
struct PayloadOutcome {
    id: String,
    ok: bool,
    error: Option<String>,
    /// 该方案阶段 B 编译的真实耗时（毫秒，worker 内 `Instant` 实测）。
    /// 落盘后作为下一轮 HRRN 的预计耗时 `est_ms`——不是拍脑袋的常数。
    elapsed_ms: u64,
}

/// HRRN（最高响应比优先）的等待任务。
///
/// `arrival` 用**单调时钟**（`Instant`），与串行 `deployer.rs::next_task` 同语义；
/// `est_ms` 是该方案**上一轮实测的阶段 B 编译耗时**，首次部署回退到阶段 A 的
/// 配置编译实测耗时，再回退到 1 秒基线。
#[derive(Debug, Clone)]
struct PendingPayload {
    id: String,
    /// 进入等待队列的时刻（等待时间的起点）= 该方案阶段 A 配置编译完成时刻。
    arrival: Instant,
    /// 预计耗时（毫秒）。HRRN 公式的分子分母，永远 clamp 到 >= 1。
    est_ms: u64,
    /// 清单序号（同响应比时的 FIFO 平局打破，保证确定性）。
    seq: usize,
}

/// HRRN 响应比 = (已等待 + 预计耗时) / 预计耗时。
///
/// 与 `deployer.rs::response_ratio` **同一个公式**（那里是串行维护队列，这里是
/// 并行编译队列）；单列成纯函数是为了拿具体数字回归：短任务响应比追得快，
/// 长任务等久了响应比也会涨——都不饿死。
#[inline]
fn hrrn_response_ratio(wait_ms: u64, est_ms: u64) -> f64 {
    let est_ms = est_ms.max(1);
    (wait_ms + est_ms) as f64 / est_ms as f64
}

/// 从等待队列里按**最高响应比**挑一个出队（`swap_remove`，与串行 `next_task` 同）。
///
/// 比较键 `(响应比, -清单序号)` 取最大者：响应比高者优先；响应比完全相同（浮点
/// 严格相等）时清单序号小者优先（FIFO 打破平局，确定性）。等待越久响应比越高，
/// 所以被银行家挡回的任务下一轮自然往前排——不会饿死。
/// 纯逻辑：不碰 IO、不碰银行家池，输入输出可完全用数字钉住。
fn take_highest_response_ratio(
    pending: &mut Vec<PendingPayload>,
    now: Instant,
) -> Option<PendingPayload> {
    if pending.is_empty() {
        return None;
    }
    let mut best = 0usize;
    // Reverse(seq)：seq 越小 → Reverse 越大 → 越优先（FIFO 平局打破）。
    let mut best_key: (f64, std::cmp::Reverse<usize>) = (f64::NEG_INFINITY, std::cmp::Reverse(usize::MAX));
    for (idx, item) in pending.iter().enumerate() {
        let waited = now.saturating_duration_since(item.arrival);
        let ratio = hrrn_response_ratio(waited.as_millis() as u64, item.est_ms);
        let key = (ratio, std::cmp::Reverse(item.seq));
        if key > best_key {
            best = idx;
            best_key = key;
        }
    }
    Some(pending.swap_remove(best))
}

/// 预计耗时 `est_ms` 的选择（纯函数，便于具体数字回归）。
/// 优先级：上一轮实测账本 > 本轮阶段 A 配置编译实测 > 1 秒基线；永远 clamp 到 >= 1。
fn pick_est(id: &str, history: &BTreeMap<String, u64>, config: &BTreeMap<String, u64>) -> u64 {
    history
        .get(id)
        .copied()
        .or_else(|| config.get(id).copied())
        .unwrap_or(1000)
        .max(1)
}

/// 阶段 B 每个方案预计耗时的持久化账本：`<user>/var/deploy_hrrn_est.json`。
///
/// 与串行队列的 `var/deploy_stats.json` **分开**，互不污染：这里记的是"每个方案
/// 阶段 B 编译耗时"，那里记的是"每类维护任务耗时"。JSON 是 `id -> 毫秒` 的映射，
/// 只在部署成功落盘；读失败/写失败一律静默回退，绝不影响部署能否进行。
fn est_ledger_path(deployer: &Deployer) -> PathBuf {
    let root = deployer
        .records_root_override_for_test()
        .unwrap_or(&deployer.user_data_dir);
    root.join("var").join("deploy_hrrn_est.json")
}

/// 读上一轮实测耗时账本。任何解析失败都返回空表（→ 全部回退阶段 A 实测）。
fn load_est_ledger(path: &Path) -> BTreeMap<String, u64> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    serde_json::from_str::<BTreeMap<String, u64>>(&text).unwrap_or_default()
}

/// 写回本轮实测耗时账本（覆盖式，只留最新一轮）。写失败静默。
fn store_est_ledger(path: &Path, est: &BTreeMap<String, u64>) {
    let Ok(text) = serde_json::to_string(est) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, text);
}

/// 银行家资源计划：`[worker]*N + [dict:<name>]*1 + [aux:<group>]*1`。
#[derive(Debug, Clone)]
struct ResourcePlan {
    names: Vec<String>,
    totals: Vec<usize>,
    job_ids: Vec<String>,
    /// job id → 该任务占用的资源下标（含 worker）。
    job_resource_indices: BTreeMap<String, Vec<usize>>,
}

impl ResourcePlan {
    fn build(jobs: &[CompileJob], worker_slots: usize) -> Self {
        // 顺序固定：worker 在前，dict/aux 按首次出现顺序（用 BTreeSet 保证确定）。
        let mut names = vec!["worker".to_string()];
        let mut totals = vec![worker_slots];
        let mut seen: BTreeSet<String> = BTreeSet::new();
        // 先把所有资源名收集排序，保证资源向量顺序与 jobs 遍历顺序无关。
        let mut dict_names = BTreeSet::new();
        let mut aux_names = BTreeSet::new();
        for job in jobs {
            dict_names.extend(job.dicts.iter().cloned());
            aux_names.extend(job.aux_groups.iter().cloned());
        }
        for dict in dict_names {
            let name = format!("dict:{dict}");
            seen.insert(name.clone());
            names.push(name);
            totals.push(1);
        }
        for group in aux_names {
            let name = format!("aux:{group}");
            seen.insert(name.clone());
            names.push(name);
            totals.push(1);
        }
        let _ = &seen;

        let index: BTreeMap<&str, usize> = names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), i))
            .collect();
        let mut job_ids = Vec::with_capacity(jobs.len());
        let mut job_resource_indices = BTreeMap::new();
        for job in jobs {
            job_ids.push(job.id.clone());
            let mut indices = vec![index["worker"]];
            for dict in &job.dicts {
                indices.push(index[format!("dict:{dict}").as_str()]);
            }
            for group in &job.aux_groups {
                indices.push(index[format!("aux:{group}").as_str()]);
            }
            job_resource_indices.insert(job.id.clone(), indices);
        }
        Self {
            names,
            totals,
            job_ids,
            job_resource_indices,
        }
    }

    /// 任务的事前最大需求 = 实际请求（一次性占全部所需资源）：每个资源 1。
    fn max_for(&self, job_id: &str) -> Vec<usize> {
        let mut max = vec![0usize; self.names.len()];
        if let Some(indices) = self.job_resource_indices.get(job_id) {
            for &i in indices {
                max[i] = 1;
            }
        }
        max
    }
}

/// 阶段 B 的**出队策略**（只决定"下一个尝试准入谁"，不碰银行家的安全判定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SchedulePolicy {
    /// 最高响应比优先：(等待 + est) / est 最大者先出队（默认）。
    #[default]
    Hrrn,
    /// 清单顺序 FIFO（对照用，`deploy.hrrn: false` 或 `WEASEL_DEPLOY_HRRN=0`）。
    Fifo,
}

impl SchedulePolicy {
    fn label(self) -> &'static str {
        match self {
            SchedulePolicy::Hrrn => "hrrn",
            SchedulePolicy::Fifo => "fifo",
        }
    }
}

/// 一次银行家调度的执行统计（日志/测试用）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScheduleStats {
    pub worker_slots: usize,
    /// 任务因 Wait/Unsafe 被延后的次数。
    pub deferred_rounds: usize,
    /// 因不安全状态回滚的次数。
    pub unsafe_rejections: usize,
    /// 被批准的分配次数（每次附带一个安全序列）。
    pub granted_sequences: usize,
    /// 调度器为推进等待队列跑的轮次数。
    pub rounds: usize,
    /// 出队策略：`"hrrn"`（最高响应比）或 `"fifo"`（清单顺序，对照用）。
    pub policy: &'static str,
    /// HRRN 实际出队顺序（id 列表），可直接对照 FIFO 清单顺序验证"真的重排过"。
    pub hrrn_order: Vec<String>,
}

/// 并行部署的整体统计，供 CLI / 测试读取。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParallelStats {
    pub success: usize,
    pub failure: usize,
    pub elapsed_ms: u64,
    pub schedule: ScheduleStats,
    /// 写入台账的记录数。
    pub records: usize,
}

/// 用 rayon 固定线程池并行 map，`R` 在 worker 内产出后不跨线程共享可变状态。
fn parallel_map<R, F>(paths: &DeployPaths, jobs: &[CompileJob], workers: usize, run: F) -> Vec<R>
where
    R: Send + 'static,
    F: Fn(&DeployPaths, &CompileJob) -> R + Send + Sync,
{
    use rayon::prelude::*;
    if jobs.is_empty() {
        return Vec::new();
    }
    let threads = workers.max(1).min(jobs.len());
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("weasel-deploy-{i}"))
        .build()
        .expect("build deploy rayon pool");
    pool.install(|| jobs.par_iter().map(|job| run(paths, job)).collect())
}

/// 阶段 A：并行编译所有方案的配置，返回每个方案的配置结果（含 dependencies）。
fn run_config_stage(
    paths: &DeployPaths,
    jobs: &[CompileJob],
    workers: usize,
) -> Vec<ConfigOutcome> {
    parallel_map(paths, jobs, workers, |paths, job| {
        let local = paths.local_deployer();
        let started = Instant::now();
        let schema_id = compile_schema_config(&local, &job.source, false);
        let config_ms = started.elapsed().as_millis() as u64;
        let dependencies = schema_id
            .as_ref()
            .map(|id| dependencies_of_staged(paths, id))
            .unwrap_or_default();
        ConfigOutcome {
            id: job.id.clone(),
            ok: schema_id.is_some(),
            dependencies,
            config_ms,
            ready_at: Instant::now(),
        }
    })
}

/// 从 staged 产物读 `schema/dependencies`（编译产物仍是 YAML，用 `load_file`）。
fn dependencies_of_staged(paths: &DeployPaths, schema_id: &str) -> Vec<String> {
    let staged = paths.staged_schema(schema_id);
    let Ok(value) = crate::load_file(&staged) else {
        return Vec::new();
    };
    value
        .pointer("/schema/dependencies")
        .and_then(|list| list.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 阶段 B 调度：**HRRN 出队 + 银行家准入 + rayon 并行**，三者正交。
///
/// * **HRRN**（[`SchedulePolicy::Hrrn`]）：每轮从 waiting 里挑响应比
///   `(等待 + est) / est` 最高者尝试准入；被 `Wait`/`Unsafe` 挡回的任务**保留原
///   arrival**，等待越久响应比越高，最终一定出队——不会被同名词典锁饿死。
/// * **银行家**：只回答"这次分配后系统是否仍安全"，与安全序列无关的排序由 HRRN 定。
/// * **rayon**：只负责把已批准的任务并行跑起来。
///
/// 主线程独占银行家池账本（`pool`），worker 只编译、不碰账本；台账由更外层按清单
/// 顺序串行写（消除读-改-写竞态）。
fn run_payload_stage(
    paths: &DeployPaths,
    jobs: &[CompileJob],
    plan: &ResourcePlan,
    workers: usize,
    policy: SchedulePolicy,
    est_ms: &BTreeMap<String, u64>,
    config_ms: &BTreeMap<String, u64>,
    ready_at: &BTreeMap<String, Instant>,
) -> (Vec<PayloadOutcome>, ScheduleStats, BTreeMap<String, u64>) {
    let mut pool = BankerPool::from_names(&plan.names, &plan.totals)
        .expect("resource plan must be a valid banker pool");
    for job_id in &plan.job_ids {
        pool.register(job_id, &plan.max_for(job_id))
            .expect("register job max");
    }

    let mut stats = ScheduleStats {
        worker_slots: workers,
        policy: policy.label(),
        ..Default::default()
    };
    let thread_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers.max(1))
        .thread_name(|i| format!("weasel-payload-{i}"))
        .build()
        .expect("build payload rayon pool");
    // 完成回报带上 worker 内实测的阶段 B 编译耗时（毫秒），供落盘复用为下轮 est。
    let (done_tx, done_rx) = mpsc::channel::<(String, bool, Option<String>, u64)>();
    let jobs_by_id: BTreeMap<String, CompileJob> = jobs
        .iter()
        .map(|job| (job.id.clone(), job.clone()))
        .collect();
    let paths = paths.clone();

    // 入队：清单顺序作为**初始顺序**（FIFO 策略直接 drain 它；HRRN 每轮重排）。
    // arrival = 该方案阶段 A 配置编译完成时刻（并行 A 天然有先后 → 首轮等待真实分化）；
    // est 优先级：上一轮实测 est 账本 > 本轮阶段 A 配置编译实测 > 1 秒基线。
    let stage_born = Instant::now();
    let mut waiting: Vec<PendingPayload> = plan
        .job_ids
        .iter()
        .enumerate()
        .map(|(seq, id)| PendingPayload {
            id: id.clone(),
            arrival: ready_at.get(id).copied().unwrap_or(stage_born),
            est_ms: pick_est(id, est_ms, config_ms),
            seq,
        })
        .collect();
    let mut in_flight = 0usize;
    let mut results: BTreeMap<String, PayloadOutcome> = plan
        .job_ids
        .iter()
        .map(|id| {
            (
                id.clone(),
                PayloadOutcome {
                    id: id.clone(),
                    ok: false,
                    error: None,
                    elapsed_ms: 0,
                },
            )
        })
        .collect();
    // 本轮实测耗时（id → 阶段 B ms），函数返回后由外层落盘。
    let mut measured: BTreeMap<String, u64> = BTreeMap::new();

    let guard_limit = jobs.len().saturating_mul(4).saturating_add(64);
    while (!waiting.is_empty() || in_flight > 0) && stats.rounds < guard_limit {
        stats.rounds += 1;

        // 1. 尽量批准 waiting 中的任务。HRRN 每轮重新按响应比排序（等待在累积），
        //    FIFO 保持清单顺序。两者都逐个尝试 request，被挡回的原样放回 next_waiting。
        let mut next_waiting: Vec<PendingPayload> = Vec::new();
        let mut granted_this_round = 0usize;
        while !waiting.is_empty() {
            let pending = match policy {
                SchedulePolicy::Hrrn => {
                    take_highest_response_ratio(&mut waiting, Instant::now())
                }
                SchedulePolicy::Fifo => Some(waiting.remove(0)),
            };
            let Some(pending) = pending else { break };
            let job_id = pending.id.clone();
            let max = plan.max_for(&job_id);
            let Some(job) = jobs_by_id.get(&job_id) else {
                continue;
            };
            match pool.request(&job_id, &max) {
                Ok(RequestOutcome::Granted { .. }) => {
                    stats.granted_sequences += 1;
                    granted_this_round += 1;
                    in_flight += 1;
                    stats.hrrn_order.push(job_id.clone());
                    let tx = done_tx.clone();
                    let paths = paths.clone();
                    let job = job.clone();
                    thread_pool.spawn(move || {
                        let local = paths.local_deployer();
                        let started = Instant::now();
                        let ok = compile_schema_payload(&local, &job.id, false);
                        let elapsed_ms = started.elapsed().as_millis() as u64;
                        let error = if ok {
                            None
                        } else {
                            Some(format!("阶段 B 编译失败: {}", job.id))
                        };
                        let _ = tx.send((job_id.clone(), ok, error, elapsed_ms));
                    });
                }
                Ok(RequestOutcome::Wait { .. }) => {
                    stats.deferred_rounds += 1;
                    next_waiting.push(pending);
                }
                Ok(RequestOutcome::Unsafe) => {
                    stats.deferred_rounds += 1;
                    stats.unsafe_rejections += 1;
                    next_waiting.push(pending);
                }
                Err(BankerError::UnknownTask(id)) => {
                    log_error(&format!("[parallel] 未注册任务 {id}"));
                    next_waiting.push(pending);
                }
                Err(err) => {
                    log_error(&format!("[parallel] 银行家池请求错误: {err}"));
                    next_waiting.push(pending);
                }
            }
        }
        waiting = next_waiting;

        // 2. 若有在途任务，等一个完成 → 归还资源 → 解锁等待者。
        if in_flight > 0 {
            // 本轮有批准时先非阻塞回收已完成；否则阻塞等（避免忙等）。
            if granted_this_round == 0 {
                match done_rx.recv() {
                    Ok((id, ok, error, elapsed_ms)) => {
                        let _ = pool.release_all(&id);
                        in_flight -= 1;
                        measured.insert(id.clone(), elapsed_ms);
                        if let Some(outcome) = results.get_mut(&id) {
                            outcome.ok = ok;
                            outcome.error = error;
                            outcome.elapsed_ms = elapsed_ms;
                        }
                    }
                    Err(_) => in_flight = 0,
                }
            }
            while let Ok((id, ok, error, elapsed_ms)) = done_rx.try_recv() {
                let _ = pool.release_all(&id);
                in_flight -= 1;
                measured.insert(id.clone(), elapsed_ms);
                if let Some(outcome) = results.get_mut(&id) {
                    outcome.ok = ok;
                    outcome.error = error;
                    outcome.elapsed_ms = elapsed_ms;
                }
            }
        } else if !waiting.is_empty() {
            // 没有在途、又有任务等不到资源 → 需求超过池总量，标记失败退出。
            log_error(&format!(
                "[parallel] 无法为 {} 个任务分配资源，标记为失败",
                waiting.len()
            ));
            for pending in &waiting {
                if let Some(outcome) = results.get_mut(&pending.id) {
                    outcome.error = Some("资源准入失败".to_string());
                }
            }
            break;
        }
    }

    // 收尾：循环因 guard_limit 退出时把仍在途的任务标记未完成。
    while in_flight > 0 {
        match done_rx.try_recv() {
            Ok((id, ok, error, elapsed_ms)) => {
                let _ = pool.release_all(&id);
                in_flight -= 1;
                measured.insert(id.clone(), elapsed_ms);
                if let Some(outcome) = results.get_mut(&id) {
                    outcome.ok = ok;
                    outcome.error = error;
                    outcome.elapsed_ms = elapsed_ms;
                }
            }
            Err(_) => break,
        }
    }
    for outcome in results.values_mut() {
        if !outcome.ok && outcome.error.is_none() {
            outcome.error = Some("调度未完成".to_string());
        }
    }

    let outcomes: Vec<PayloadOutcome> = plan
        .job_ids
        .iter()
        .filter_map(|id| results.get(id).cloned())
        .collect();
    (outcomes, stats, measured)
}

/// 从 staged 配置提取该方案用到的词典名与辅助码组名（决定阶段 B 的资源锁）。
fn extract_resource_names(staged: &serde_json::Value) -> (Vec<String>, Vec<String>) {
    let mut dicts = BTreeSet::new();
    let mut aux_groups = BTreeSet::new();

    if let Some(translator) = staged.pointer("/translator") {
        if let Some(name) = dict_name(translator).filter(|name| !name.is_empty()) {
            dicts.insert(name.to_string());
        }
        if let Some(object) = translator.as_object() {
            for node in object.values() {
                if let Some(name) = dict_name(node).filter(|name| !name.is_empty()) {
                    dicts.insert(name.to_string());
                }
            }
        }
    }
    if let Some(algebra) = staged.pointer("/build/auxiliary/algebra") {
        match algebra {
            serde_json::Value::Array(items) if !items.is_empty() => {
                aux_groups.insert("auto".to_string());
            }
            serde_json::Value::Object(named) => {
                for name in named.keys() {
                    aux_groups.insert(name.clone());
                }
            }
            _ => {}
        }
    }
    (dicts.into_iter().collect(), aux_groups.into_iter().collect())
}

/// 与 `deployment_tasks::dict_name_in` 同语义。
fn dict_name(node: &serde_json::Value) -> Option<&str> {
    node.get("dict")
        .or_else(|| node.get("dictionary"))
        .and_then(|value| value.as_str())
}

/// 并行 workspace 部署主入口（读环境变量决定 worker 数）。
///
/// 返回 `Ok(统计)` = 全部成功；`Err((成功数, 失败数))` = 有失败（对齐串行语义）。
pub fn run_workspace_update_parallel(
    deployer: &mut Deployer,
) -> Result<ParallelStats, (usize, usize)> {
    run_workspace_update_parallel_with(deployer, max_workers())
}

/// 显式指定并发度的并行部署入口（测试/CLI 用，不读环境变量）。
///
/// `workers <= 1` 时退化为串行调度（仍走同一套银行家池，但一次只批一个任务），
/// 用于"并行与串行产物逐字节一致"的对照测试。
pub fn run_workspace_update_parallel_with(
    deployer: &mut Deployer,
    workers: usize,
) -> Result<ParallelStats, (usize, usize)> {
    let started = Instant::now();
    log_info("[parallel] workspace 部署（银行家资源池）启动");

    // 1. 出厂 default.yaml 补齐（主线程，与串行路径同）。
    let default_update = DeploymentTask::ConfigFileUpdate {
        file_name: "default.yaml".to_string(),
        version_key: "config_version".to_string(),
    };
    default_update.run(deployer);

    // 2. 方案清单（点名制）。
    let manifest_outcome =
        super::schema_list::ensure_manifest(&deployer.user_data_dir, &deployer.shared_data_dir);
    if manifest_outcome.generated() {
        log_info(&format!(
            "[parallel] 方案清单缺失，已生成 {} 项（active = {}）",
            manifest_outcome.manifest().schemas.len(),
            manifest_outcome.manifest().active.as_deref().unwrap_or("<无>")
        ));
    }
    let manifest = load_schema_manifest(&deployer.user_data_dir);
    let raw_ids = manifest.schemas.clone();
    if raw_ids.is_empty() {
        log_warn("[parallel] 方案清单为空");
        return Err((0, 0));
    }
    // 2026-11-11 修正：串行/并行两条路径必须同语义。只过滤内存清单，不回写 schema.yaml；
    // 显式禁用 + 「父方案未勾选压住子方案」都在 schema_disabled 里统一算。
    let (schema_ids, disabled) = super::schema_disabled::enabled_ids(
        &deployer.user_data_dir,
        &deployer.shared_data_dir,
        &raw_ids,
    );
    if schema_ids.is_empty() {
        log_warn(&format!(
            "[parallel] {} 个勾选方案全被压住（显式禁用或父方案未勾选）；源 schema.yaml 原样保留，本次不部署任何方案。",
            raw_ids.len()
        ));
        return Err((0, 0));
    }
    log_info(&format!(
        "[parallel] 清单原始 {} 项 → 实际部署 {} 项（当前不部署 {} 项），worker 槽 {workers}",
        raw_ids.len(),
        schema_ids.len(),
        disabled.len()
    ));
    if !disabled.is_empty() {
        log_info(&format!(
            "[parallel] 当前不部署（显式禁用或父未选压住；不编译、不写进 schema_list）：{}",
            disabled.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }

    let paths = DeployPaths::from_deployer(deployer);
    let policy = if hrrn_enabled_for(&deployer.user_data_dir) {
        SchedulePolicy::Hrrn
    } else {
        SchedulePolicy::Fifo
    };
    // HRRN 预计耗时账本：上一轮实测的阶段 B 编译耗时。读失败 → 空表 → 回退阶段 A 实测。
    let est_ledger = est_ledger_path(deployer);
    let mut est_history = load_est_ledger(&est_ledger);
    let est_source = if est_history.is_empty() {
        "阶段A配置编译实测(首轮)"
    } else {
        "上轮阶段B编译实测"
    };
    log_info(&format!(
        "[parallel] 出队策略={}，预计耗时来源={}，历史样本={}",
        policy.label(),
        est_source,
        est_history.len()
    ));

    let mut built: BTreeSet<String> = BTreeSet::new();
    let mut wave: Vec<String> = schema_ids.clone();
    let mut total_success = 0usize;
    let mut total_failure = 0usize;
    // 台账记录：(schema, hash, arrived_ms, finished_ms)，主线程串行写。
    let mut records: Vec<(String, u32, u64, u64)> = Vec::new();
    let mut aggregate = ScheduleStats {
        policy: policy.label(),
        ..Default::default()
    };

    while !wave.is_empty() {
        // 3a. 构造本波次 jobs（去重 + 源存在性，与串行一致）。
        let mut jobs: Vec<CompileJob> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for id in &wave {
            if built.contains(id) || !seen.insert(id.clone()) {
                continue;
            }
            let source = deployer.layout().schema_source(id);
            if !source.is_file() {
                log_warn(&format!("[parallel] missing input schema: {id}"));
                total_failure += 1;
                built.insert(id.clone());
                continue;
            }
            jobs.push(CompileJob {
                id: id.clone(),
                source,
                dicts: Vec::new(),
                aux_groups: Vec::new(),
            });
        }
        if jobs.is_empty() {
            break;
        }

        // 3b. 阶段 A：并行编译配置 + 到达时刻。
        let hashes: BTreeMap<String, u32> = jobs
            .iter()
            .map(|job| (job.id.clone(), checksum(&job.source)))
            .collect();
        let arrivals: BTreeMap<String, u64> = jobs
            .iter()
            .map(|job| (job.id.clone(), wall_now_ms()))
            .collect();
        let config_results = run_config_stage(&paths, &jobs, workers);
        // 阶段 A 实测耗时 → est 回退值（首次部署 / 无历史样本时生效）；
        // 完成时刻 → 阶段 B 的 HRRN 到达时刻（并行 A 完成有先后）。
        let config_ms: BTreeMap<String, u64> = config_results
            .iter()
            .map(|result| (result.id.clone(), result.config_ms.max(1)))
            .collect();
        let ready_at: BTreeMap<String, Instant> = config_results
            .iter()
            .map(|result| (result.id.clone(), result.ready_at))
            .collect();
        let mut next_wave: Vec<String> = Vec::new();
        for result in &config_results {
            built.insert(result.id.clone());
            if result.ok {
                total_success += 1;
                for dep in &result.dependencies {
                    if !built.contains(dep) {
                        next_wave.push(dep.clone());
                    }
                }
            } else {
                total_failure += 1;
                log_warn(&format!("[parallel] {} 阶段 A 失败", result.id));
            }
        }

        // 3c. 用 staged 精确填充资源需求。
        let mut payload_jobs = jobs.clone();
        for job in &mut payload_jobs {
            if let Ok(value) = crate::load_file(&paths.staged_schema(&job.id)) {
                let (dicts, aux) = extract_resource_names(&value);
                job.dicts = dicts;
                job.aux_groups = aux;
            }
        }

        // 3d. 阶段 B：HRRN 出队 + 银行家准入 + 并行编译产物。
        let plan = ResourcePlan::build(&payload_jobs, workers);
        let (payload_outcomes, stats, measured) = run_payload_stage(
            &paths,
            &payload_jobs,
            &plan,
            workers,
            policy,
            &est_history,
            &config_ms,
            &ready_at,
        );
        // 本波实测耗时并入总账本（下一轮部署的 est）；按 id 累积去重取最新。
        est_history.extend(measured);
        aggregate.worker_slots = stats.worker_slots;
        aggregate.deferred_rounds += stats.deferred_rounds;
        aggregate.unsafe_rejections += stats.unsafe_rejections;
        aggregate.granted_sequences += stats.granted_sequences;
        aggregate.rounds += stats.rounds;
        aggregate.hrrn_order.extend(stats.hrrn_order);
        for outcome in payload_outcomes {
            // 先借用 id 算好各项，最后再 move，避免 "borrow after move"。
            let hash = hashes.get(&outcome.id).copied().unwrap_or(0);
            let arrived = arrivals.get(&outcome.id).copied().unwrap_or(0);
            if !outcome.ok {
                log_warn(&format!(
                    "[parallel] {} 阶段 B 失败: {}",
                    outcome.id,
                    outcome.error.as_deref().unwrap_or("<无>")
                ));
            }
            records.push((outcome.id, hash, arrived, wall_now_ms()));
        }

        wave = next_wave;
    }

    // 4. 台账：主线程按清单顺序串行写（消除读-改-写竞态）。
    records.sort_by_key(|(schema, _, _, _)| {
        schema_ids
            .iter()
            .position(|id| id == schema)
            .unwrap_or(usize::MAX)
    });
    for (schema, hash, arrived, finished) in &records {
        deployer.append_deploy_record(schema, *hash, *arrived, *finished);
    }

    // 5. schema_list 写回 staging/default.yaml（与串行路径第 5 步同）。
    write_schema_list(deployer, &schema_ids);

    // 6. 落盘本轮实测耗时 → 下一轮 HRRN 的预计耗时 est。失败静默，不影响部署结果。
    store_est_ledger(&est_ledger, &est_history);

    let elapsed = started.elapsed();
    let hrrn_order = aggregate.hrrn_order.join(",");
    let manifest_order = schema_ids.join(",");
    let reorder = hrrn_order != manifest_order;
    log_info(&format!(
        "[parallel] 完成: {total_success} success, {total_failure} failure; \
         worker={}, 轮次={}, 延迟={}, 不安全拒绝={}, 批准={}, 台账={}, 耗时={}ms, \
         出队={}, 重排={}, est样本={}",
        aggregate.worker_slots,
        aggregate.rounds,
        aggregate.deferred_rounds,
        aggregate.unsafe_rejections,
        aggregate.granted_sequences,
        records.len(),
        elapsed.as_millis(),
        policy.label(),
        if reorder { "是" } else { "否" },
        est_history.len()
    ));
    log_info(&format!(
        "[parallel] 出队顺序(实际): {hrrn_order}\n[parallel] 清单顺序(基线): {manifest_order}"
    ));

    let stats = ParallelStats {
        success: total_success,
        failure: total_failure,
        elapsed_ms: elapsed.as_millis() as u64,
        schedule: aggregate,
        records: records.len(),
    };
    if total_failure == 0 {
        Ok(stats)
    } else {
        Err((total_success, total_failure))
    }
}

/// `schema_list` 写回 `staging/default.yaml`（与串行路径同）。
fn write_schema_list(deployer: &Deployer, schema_ids: &[String]) {
    use crate::config;
    if schema_ids.is_empty() {
        return;
    }
    let default_path = if deployer.user_data_dir.join("default.yaml").is_file() {
        deployer.user_data_dir.join("default.yaml")
    } else {
        deployer.shared_data_dir.join("default.yaml")
    };
    let Ok(value) = crate::load_file(&default_path) else {
        return;
    };
    let mut value = value;
    let list: Vec<serde_json::Value> = schema_ids
        .iter()
        .map(|id| serde_json::json!({ "schema": id }))
        .collect();
    if let Some(map) = value.as_object_mut() {
        map.insert("schema_list".to_string(), serde_json::Value::Array(list));
    }
    let output = deployer.staging_dir.join("default.yaml");
    if let Some(parent) = output.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match config::to_yaml_string(&config::data::convert_from_yaml(&value, None)) {
        Ok(yaml) => {
            if std::fs::write(&output, yaml).is_ok() {
                log_info("[parallel] schema_list 已写回 staging/default.yaml");
            }
        }
        Err(err) => log_warn(&format!("[parallel] 无法序列化 default.yaml: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lever::banker::RequestOutcome;
    use std::time::Duration;

    fn job(id: &str, dicts: &[&str], aux: &[&str]) -> CompileJob {
        CompileJob {
            id: id.into(),
            source: PathBuf::from(format!("{id}.schema.yaml")),
            dicts: dicts.iter().map(|s| s.to_string()).collect(),
            aux_groups: aux.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// 具体输入 → 具体输出：两张同名词典的方案 + 一张不同词典，
    /// 银行家池必须让同名字典串行（第二把锁总量=1），不同词典可并行。
    #[test]
    fn resource_plan_serializes_same_dict_and_parallels_distinct() {
        let jobs = vec![
            job("qiaoma", &["qiaoma"], &[]),
            job("qiaoma_jf", &["qiaoma"], &[]), // 与 qiaoma 共用
            job("wubi86", &["wubi86"], &[]),
        ];
        let plan = ResourcePlan::build(&jobs, 2);
        assert_eq!(plan.names, vec!["worker", "dict:qiaoma", "dict:wubi86"]);
        assert_eq!(plan.totals, vec![2, 1, 1]);
        assert_eq!(plan.max_for("qiaoma"), vec![1, 1, 0]);
        assert_eq!(plan.max_for("qiaoma_jf"), vec![1, 1, 0]);
        assert_eq!(plan.max_for("wubi86"), vec![1, 0, 1]);

        let mut pool = BankerPool::from_names(&plan.names, &plan.totals).unwrap();
        for id in &plan.job_ids {
            pool.register(id, &plan.max_for(id)).unwrap();
        }
        assert!(matches!(
            pool.request("qiaoma", &plan.max_for("qiaoma")).unwrap(),
            RequestOutcome::Granted { .. }
        ));
        let before = pool.available().to_vec();
        assert_eq!(
            pool.request("qiaoma_jf", &plan.max_for("qiaoma_jf")).unwrap(),
            RequestOutcome::Wait {
                resource: "dict:qiaoma".into(),
                requested: 1,
                available: 0,
            }
        );
        assert_eq!(pool.available(), &before[..]);
        assert!(matches!(
            pool.request("wubi86", &plan.max_for("wubi86")).unwrap(),
            RequestOutcome::Granted { .. }
        ));
        assert!(pool.is_consistent());
    }

    /// 教科书式 AB-BA：资源看起来够，但分配后会进入不安全状态，必须回滚。
    #[test]
    fn unsafe_allocation_rolls_back_in_payload_model() {
        let mut pool = BankerPool::new(&["R"], &[4]).unwrap();
        pool.register("a", &[3]).unwrap();
        pool.register("b", &[3]).unwrap();
        assert!(matches!(
            pool.request("a", &[2]).unwrap(),
            RequestOutcome::Granted { .. }
        ));
        assert!(matches!(
            pool.request("b", &[1]).unwrap(),
            RequestOutcome::Granted { .. }
        ));
        assert_eq!(pool.available(), &[1]);
        // b 再要 1 → available 归 0，a need=1、b need=1 都拿不到 → 不安全 → 回滚。
        assert_eq!(
            pool.request("b", &[1]).unwrap(),
            RequestOutcome::Unsafe
        );
        assert_eq!(pool.available(), &[1]);
        assert_eq!(pool.allocation("b"), Some(&[1][..]));
        assert!(pool.is_consistent());
    }

    /// 具体输入 → 具体输出：从 staged 提取 dict（主 + 变体）与 aux group。
    #[test]
    fn extract_resource_names_from_staged() {
        let staged = serde_json::json!({
            "translator": {
                "dict": "pinyin",
                "simp": { "dictionary": "pinyin.simp" },
                "trad": { "dict": "pinyin.trad" },
                "prism": "pinyin"
            },
            "build": {
                "auxiliary": {
                    "dictionary": "aux.yaml",
                    "algebra": { "zhijie": ["a|b|"], "abc": ["c|d|"] }
                }
            }
        });
        let (dicts, aux) = extract_resource_names(&staged);
        assert_eq!(dicts, vec!["pinyin", "pinyin.simp", "pinyin.trad"]);
        assert_eq!(aux, vec!["abc", "zhijie"]);
    }

    #[test]
    fn extract_single_aux_group_and_no_dict() {
        let staged = serde_json::json!({
            "build": { "auxiliary": { "dictionary": "aux", "algebra": ["a|b|"] } }
        });
        let (dicts, aux) = extract_resource_names(&staged);
        assert!(dicts.is_empty());
        assert_eq!(aux, vec!["auto"]);
    }

    /// 开关语义（纯逻辑判断表，不改全局 env）。
    #[test]
    fn enable_switch_truth_table() {
        let classify = |v: &str| {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && v != "0" && v != "false" && v != "off"
        };
        assert!(!classify(""));
        assert!(!classify("0"));
        assert!(!classify("false"));
        assert!(!classify("OFF"));
        assert!(classify("1"));
        assert!(classify("true"));
        assert!(classify("yes"));
    }

    /// DeployPaths 快照 → 局部 Deployer 的路径必须与源一致（线程安全的根基）。
    #[test]
    fn deploy_paths_snapshot_roundtrip() {
        let mut d = Deployer::with_data_dirs("C:\\shared", "C:\\user");
        d.set_program_dir("C:\\program");
        d.staging_dir = PathBuf::from("C:\\user\\build");
        d.prebuilt_data_dir = PathBuf::from("C:\\prebuilt");
        let snapshot = DeployPaths::from_deployer(&d);
        assert_eq!(snapshot.program_dir, Path::new("C:\\program"));
        assert_eq!(snapshot.shared_data_dir, Path::new("C:\\shared"));
        assert_eq!(snapshot.user_data_dir, Path::new("C:\\user"));
        assert_eq!(snapshot.staging_dir, Path::new("C:\\user\\build"));
        assert_eq!(snapshot.prebuilt_data_dir, Path::new("C:\\prebuilt"));
        let local = snapshot.local_deployer();
        assert_eq!(local.shared_data_dir, snapshot.shared_data_dir);
        assert_eq!(local.staging_dir, snapshot.staging_dir);
    }

    /// 写一个临时 weasel.yaml，返回它的父目录（测试结束由 TempGuard 清理）。
    fn write_weasel(text: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "weasel_parallel_cfg_{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("weasel.yaml"), text).unwrap();
        dir
    }

    /// 具体输入 → 具体输出：顶层 deploy 段两种写法（内联/分块）+ 注释/引号/行内注释。
    #[test]
    fn reads_deploy_config_from_weasel_yaml() {
        let dir = write_weasel(
            "sound:\n  enabled: true\n\
             deploy:            # 部署器配置\n  parallel: true\n  workers: 3 # 三槽\n\
             switcher:\n  hotkeys: []\n",
        );
        let cfg = read_deploy_config(&dir);
        assert_eq!(cfg.parallel, Some(true), "顶层 deploy 段的 parallel 应读到 true");
        assert_eq!(cfg.workers, Some(3), "行内注释前的 3 应读到");
        std::fs::remove_dir_all(&dir).ok();

        let dir = write_weasel("deploy:\n  parallel: 'off'\n  workers: \"5\"\n");
        let cfg = read_deploy_config(&dir);
        assert_eq!(cfg.parallel, Some(false), "引号里的 off 应解析为 false");
        assert_eq!(cfg.workers, Some(5), "引号里的 5 应解析为 5");
        std::fs::remove_dir_all(&dir).ok();

        // 非顶层（`  deploy:` 被缩进）不算段，必须忽略。
        let dir = write_weasel("some_parent:\n  deploy:\n    parallel: true\n    workers: 6\n");
        let cfg = read_deploy_config(&dir);
        assert_eq!(cfg, DeployConfig::default(), "缩进的 deploy 不是顶层段");
        std::fs::remove_dir_all(&dir).ok();

        // 非法值 / 缺文件都回退默认，不 panic。
        let dir = write_weasel("deploy:\n  parallel: maybe\n  workers: zero\n");
        assert_eq!(read_deploy_config(&dir), DeployConfig::default());
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(read_deploy_config(Path::new("/no/such/dir")), DeployConfig::default());
    }

    /// 优先级纯函数的具体输入 → 具体输出：环境变量 > 配置文件 > 默认。
    #[test]
    fn switch_precedence_env_then_file_then_default() {
        // is_enabled_from：无 env 时读文件；有 env 时 env 覆盖；都没有则 false。
        assert!(!is_enabled_from(None, None), "无 env 无配置 → 默认关闭");
        assert!(is_enabled_from(None, Some(true)), "无 env → 文件 true");
        assert!(!is_enabled_from(None, Some(false)), "无 env → 文件 false");
        assert!(!is_enabled_from(Some("0"), Some(true)), "env=0 覆盖文件 true");
        assert!(!is_enabled_from(Some(" false "), Some(true)), "env=false 覆盖");
        assert!(!is_enabled_from(Some("off"), Some(true)), "env=off 覆盖");
        assert!(is_enabled_from(Some("1"), Some(false)), "env=1 覆盖文件 false");
        assert!(is_enabled_from(Some("yes"), None), "env=yes 启用");

        // max_workers_from：env > 文件 > CPU，非法 env 回退文件，硬封顶 6。
        assert_eq!(max_workers_from(None, None, 4), 4, "无配置用 CPU 核数(4<6)");
        assert_eq!(max_workers_from(None, Some(2), 8), 2, "无 env 用文件 2");
        assert_eq!(max_workers_from(Some("4"), Some(2), 8), 4, "env=4 覆盖文件 2");
        assert_eq!(max_workers_from(Some("0"), Some(2), 8), 2, "env=0 非法回退文件");
        assert_eq!(max_workers_from(Some("abc"), Some(3), 8), 3, "env 非法回退文件");
        assert_eq!(max_workers_from(Some("9"), None, 4), 6, "env=9 硬封顶 6");
        assert_eq!(max_workers_from(None, Some(20), 4), 6, "文件 workers=20 硬封顶 6");
    }

    /// is_enabled_for / max_workers_for 端到端：真实临时 weasel.yaml → 决策。
    #[test]
    fn for_dir_reads_real_weasel_file() {
        let dir = write_weasel("deploy:\n  parallel: true\n  workers: 3\n");
        // 测试进程可能带 CI 注入的 env；这里只断言"函数能读到文件"这一路径的
        // 组合结果（若外部 env 覆盖，也是设计内的最高优先级，不算错误）。
        let cfg = read_deploy_config(&dir);
        assert_eq!(cfg.parallel, Some(true));
        assert_eq!(cfg.workers, Some(3));
        assert!(is_enabled_from(std::env::var("WEASEL_DEPLOY_PARALLEL").ok().as_deref(), cfg.parallel));
        assert_eq!(
            max_workers_from(
                std::env::var("WEASEL_DEPLOY_WORKERS").ok().as_deref(),
                cfg.workers,
                4,
            ),
            3
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ====================================================================
    // HRRN（最高响应比优先）单测 —— 用户要求"具体输入输出，不只是通过/失败"
    // ====================================================================

    /// 响应比公式的具体数字表（与串行 deployer.rs::response_ratio 同公式）。
    /// 响应比 = (等待 + est) / est；est clamp 到 >= 1。
    #[test]
    fn hrrn_response_ratio_exact_numbers() {
        let cases: &[(u64, u64, f64)] = &[
            (0, 1000, 1.0),      // 刚到达：响应比 1
            (50, 100, 1.5),      // 短任务等 50ms：1.5
            (60000, 10000, 7.0), // 长任务等 60s：(60000+10000)/10000 = 7
            (1000, 1, 1001.0),   // est=1ms 的极短任务：响应比飙升
            (500, 0, 501.0),     // est=0 clamp 到 1：(500+1)/1
        ];
        let mut table = String::from("等待ms   est_ms   响应比\n");
        for &(wait, est, want) in cases {
            let got = hrrn_response_ratio(wait, est);
            table.push_str(&format!("{wait:>6}  {est:>7}   {got:.3} (期望 {want:.3})\n"));
            assert!((got - want).abs() < 1e-9, "wait={wait} est={est}: {got} != {want}");
        }
        eprintln!("\n=== HRRN 响应比具体数字 ===\n{table}");
    }

    /// 同一批 4 个任务、同一到达时刻，**HRRN 出队顺序必须不同于 FIFO 清单顺序**。
    /// 输入：4 个方案 est 分别 100/1000/300/2000ms，全部同时到达 →
    /// 首轮响应比全为 1.0（等待 0），按 id 字典序打破平局；这里用**不同到达时刻**
    /// 制造等待差，验证响应比真正 drove 重排。
    #[test]
    fn hrrn_reorders_vs_fifo_with_concrete_sequence() {
        let now = Instant::now();
        let mk = |id: &str, waited_ms: u64, est_ms: u64, seq: usize| PendingPayload {
            id: id.into(),
            arrival: now - Duration::from_millis(waited_ms),
            est_ms,
            seq,
        };
        // 清单(FIFO)顺序：a(seq0), b(seq1), c(seq2), d(seq3)
        //    a: 等 0ms   est 100  → R=(0+100)/100   = 1.00
        //    b: 等 500ms est 1000 → R=(500+1000)/1000 = 1.50
        //    c: 等 100ms est 100  → R=(100+100)/100  = 2.00  ← 最高
        //    d: 等 900ms est 2000 → R=(900+2000)/2000 = 1.45
        // HRRN 出队首项应是 c（R=2.00），不是清单首项 a。
        let mut pending = vec![
            mk("a", 0, 100, 0),
            mk("b", 500, 1000, 1),
            mk("c", 100, 100, 2),
            mk("d", 900, 2000, 3),
        ];
        let first = take_highest_response_ratio(&mut pending, now).unwrap();
        assert_eq!(first.id, "c", "最高响应比 2.00 的 c 应第一个出队（FIFO 会是 a）");
        assert_eq!(hrrn_response_ratio(100, 100), 2.0);
        // 剩余 3 个：a R=1.00, b R=1.50, d R=1.45 → 次高是 b
        let second = take_highest_response_ratio(&mut pending, now).unwrap();
        assert_eq!(second.id, "b", "次高响应比 1.50 的 b 应第二个出队");
        eprintln!(
            "\n=== HRRN vs FIFO 出队（同一批 4 任务）===\n\
             清单 FIFO : [a, b, c, d]\n\
             HRRN 实际 : [c, b, ...]  (c 等 100ms/est100 → R=2.00 反超清单首项 a 的 R=1.00)\n"
        );
    }

    /// 长任务不饿死：一个 est 很大的任务等得够久，响应比反超刚到达的短任务。
    #[test]
    fn hrrn_long_job_does_not_starve() {
        let now = Instant::now();
        let mut pending = vec![
            PendingPayload {
                id: "long".into(),
                arrival: now - Duration::from_secs(60), // 等了 60s
                est_ms: 10_000,                          // est 10s → R=7.0
                seq: 0,
            },
            PendingPayload {
                id: "short".into(),
                arrival: now, // 刚到达
                est_ms: 100,  // est 100ms → R=1.0
                seq: 1,
            },
        ];
        let first = take_highest_response_ratio(&mut pending, now).unwrap();
        assert_eq!(
            first.id, "long",
            "等了 60s 的长任务 R=7.0 应反超刚到的短任务 R=1.0（防饿死）"
        );
    }

    /// est 回退链的具体数字：历史账本 > 本轮阶段 A 实测 > 1s 基线。
    #[test]
    fn hrrn_est_fallback_chain_exact_numbers() {
        let history: BTreeMap<String, u64> =
            [("qiaoma".into(), 852), ("pinyin".into(), 4312)].into_iter().collect();
        let config: BTreeMap<String, u64> =
            [("qiaoma".into(), 120), ("wubi86".into(), 95)].into_iter().collect();
        assert_eq!(pick_est("qiaoma", &history, &config), 852, "历史优先于阶段 A");
        assert_eq!(pick_est("pinyin", &history, &config), 4312, "历史命中");
        assert_eq!(pick_est("wubi86", &history, &config), 95, "无历史 → 阶段 A 实测");
        assert_eq!(pick_est("fresh", &history, &config), 1000, "都无 → 1s 基线");
        // clamp 到 >= 1
        let zero: BTreeMap<String, u64> = [("z".into(), 0)].into_iter().collect();
        assert_eq!(pick_est("z", &zero, &zero), 1, "est=0 clamp 到 1");
    }

    /// HRRN 开关的真值表 + 配置解析（`deploy.hrrn`），默认 true。
    #[test]
    fn hrrn_switch_defaults_on_and_reads_config() {
        // 默认开启（并行即用 HRRN）；显式 false / env 关闭回退 FIFO。
        assert!(hrrn_enabled_from(None, None), "无配置 → 默认 HRRN");
        assert!(hrrn_enabled_from(None, Some(true)), "配置 true");
        assert!(!hrrn_enabled_from(None, Some(false)), "配置 false → FIFO");
        assert!(!hrrn_enabled_from(Some("0"), Some(true)), "env=0 覆盖 → FIFO");
        assert!(hrrn_enabled_from(Some("1"), Some(false)), "env=1 覆盖配置 false");
        assert!(!hrrn_enabled_from(Some("off"), None), "env=off → FIFO");

        // 从真实 weasel.yaml 读 deploy.hrrn。
        let dir = write_weasel("deploy:\n  parallel: true\n  hrrn: false\n  workers: 4\n");
        let cfg = read_deploy_config(&dir);
        assert_eq!(cfg.hrrn, Some(false), "顶层 deploy.hrrn 应读到 false");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// est 账本写读 roundtrip（具体 JSON 字节 → 具体映射），且失败路径静默。
    #[test]
    fn hrrn_est_ledger_roundtrip() {
        let dir = write_weasel("unused: true\n");
        let path = dir.join("var").join("deploy_hrrn_est.json");
        let mut map = BTreeMap::new();
        map.insert("qiaoma".to_string(), 852u64);
        map.insert("pinyin".to_string(), 4312u64);
        store_est_ledger(&path, &map);
        let loaded = load_est_ledger(&path);
        assert_eq!(loaded, map, "写读 roundtrip 应逐项一致");
        // 文件不存在 → 空表（回退链起点），不 panic。
        assert!(load_est_ledger(&dir.join("var").join("missing.json")).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
