//! 银行家算法资源池（死锁避免）——并行部署的准入控制器。
//!
//! # 为什么部署需要它
//!
//! 方案并行编译时，两类资源会互相竞争、可能形成死锁或产物互踩：
//!
//! * **执行槽**（`worker`）：并发编译线程数，上限 `min(CPU 核数, 6)`；
//! * **独占产物**（如 `dict:qiaoma`、`aux:alternate`）：同一本词典的
//!   `table.bin` / `prism.bin` / `prisms.list` 只能被一个任务写。
//!   多个方案引用同一本词典（`qiaoma` 与 `qiaoma_jf` 都用 `dictionary: qiaoma`）
//!   时，若并行编译会互踩产物。
//!
//! 一个任务往往**同时**需要多种资源（1 个 worker + 若干本词典锁）。若调度器
//! 见缝就分配，就可能出现环形等待：A 拿着 `worker-1` 等 `dict:qiaoma`，B 拿着
//! `dict:qiaoma` 等 `worker-2`——谁也推进不了。银行家算法在**每次分配前**先问
//! 一句：**「假设批准这次请求，剩下的资源还能让所有在途任务跑完吗？」**
//! 能（存在安全序列）才分配，不能就拒绝，让请求方等待。
//!
//! # 算法（经典银行家算法）
//!
//! 维护三类账本（下标 = 资源类型，键 = 任务 id）：
//!
//! * `total` / `available`：每类资源的总量 / 当前可用；
//! * `max`：每个任务**事前声明**的最大需求（启动前就知道，见
//!   [`BankerPool::register`]）；
//! * `allocation`：已分配给每个任务的数量。
//!
//! `need = max - allocation` 由两者现算，不单独存，避免账本不一致。
//!
//! 请求分三步（[`BankerPool::request`]）：
//!
//! 1. `request <= need`？否则 [`BankerError::OverNeed`]（程序 bug，不是死锁）；
//! 2. `request <= available`？否则 [`RequestOutcome::Wait`]（资源不足，等）；
//! 3. **试探分配**，跑 [`BankerPool::is_safe`]（安全序列算法）：
//!    安全 → [`RequestOutcome::Granted`] 落账；
//!    不安全 → **回滚这次分配**，返回 [`RequestOutcome::Unsafe`]，等别的任务释放。
//!
//! 安全序列算法（资源分配图的死锁避免判定）：
//!
//! ```text
//! work = available
//! finish = { 所有任务: false }
//! 循环: 找一个未完成且 need[i] <= work 的任务 i
//!   找到: work += allocation[i]; finish[i] = true; 记入序列
//! 若所有任务 finish = true → 存在安全序列（返回该序列）
//! 否则 → 不安全状态，拒绝这次分配
//! ```
//!
//! # 与其它模块的关系
//!
//! * 本模块**只做算法，不碰 IO、不碰线程**——可以纯逻辑单测，输入→输出全是数字。
//! * 任务 id 用字符串（通常是 schema id，或 `schema@dict` 这样的复合键）。
//! * 资源名用 `String`（约定：`worker`、`dict:<name>`、`aux:<group>`）。
//! * 调度器（[`super::parallel_deploy`]）用本池做准入；池返回 `Wait`/`Unsafe`
//!   时，调度器把任务留在等待队列，某个任务 `release` 后再重试。
//! * `max_workers == 1` 时调度器走串行回退，产物与并行路径**逐字节一致**。

use std::collections::BTreeMap;

/// 资源池的配置/操作错误。这些是**调用方用错了 API**，不是"资源暂时不够"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BankerError {
    /// 任务 id 未注册。
    UnknownTask(String),
    /// 重复注册同一任务 id。
    DuplicateTask(String),
    /// 向量长度与声明的资源类型数不符。
    DimensionMismatch {
        task: String,
        expected: usize,
        got: usize,
    },
    /// 请求量超过该任务事前声明的最大需求（`request > need`）。
    OverNeed {
        task: String,
        resource: String,
        requested: usize,
        need: usize,
    },
    /// 资源名重复。
    DuplicateResource(String),
    /// 资源类型数为 0，或总量/需求向量为空。
    EmptyResource,
}

impl std::fmt::Display for BankerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTask(id) => write!(f, "任务 `{id}` 未注册"),
            Self::DuplicateTask(id) => write!(f, "任务 `{id}` 重复注册"),
            Self::DimensionMismatch { task, expected, got } => write!(
                f,
                "任务 `{task}` 需求向量长度 {got}，应为 {expected}"
            ),
            Self::OverNeed {
                task,
                resource,
                requested,
                need,
            } => write!(
                f,
                "任务 `{task}` 请求 {requested} 个 `{resource}`，但其 need 只有 {need}"
            ),
            Self::DuplicateResource(name) => write!(f, "资源 `{name}` 重复定义"),
            Self::EmptyResource => write!(f, "资源池至少要有一种资源"),
        }
    }
}

impl std::error::Error for BankerError {}

/// 一次资源请求的判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestOutcome {
    /// 批准：已落账，系统仍处安全状态；附带**本次算出的安全序列**。
    Granted { safe_sequence: Vec<String> },
    /// 资源不足：之类资源当前 available 不够，未改任何账本，调用方应等待。
    Wait {
        resource: String,
        requested: usize,
        available: usize,
    },
    /// 试探分配后进入不安全状态：已**回滚**这次分配，未改任何账本。
    /// 调用方应等待其它任务释放资源后再重试。
    Unsafe,
}

/// 银行家算法资源池。只依赖标准库，无 IO / 无线程 / 无全局状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BankerPool {
    /// 资源类型名（顺序固定，下标 = 资源索引）。
    resources: Vec<String>,
    /// 每类资源总量。
    total: Vec<usize>,
    /// 每类资源当前可用。
    available: Vec<usize>,
    /// 任务 → 事前声明的最大需求。BTreeMap 保证输出确定（按 id 排序）。
    max: BTreeMap<String, Vec<usize>>,
    /// 任务 → 当前已分配。
    allocation: BTreeMap<String, Vec<usize>>,
}

impl BankerPool {
    /// 构造资源池。`total[i]` 对应 `resources[i]` 的总量。
    pub fn new(resources: &[&str], total: &[usize]) -> Result<Self, BankerError> {
        let owned: Vec<String> = resources.iter().map(|s| (*s).to_string()).collect();
        Self::from_names(&owned, total)
    }

    /// 与 [`Self::new`] 等价，但资源名是 `String`（供资源名运行时生成的调度器使用）。
    pub fn from_names(resources: &[String], total: &[usize]) -> Result<Self, BankerError> {
        if resources.is_empty() {
            return Err(BankerError::EmptyResource);
        }
        if resources.len() != total.len() {
            // 用空 task 名表达"构造参数维度不符"（此处还没有任务）。
            return Err(BankerError::DimensionMismatch {
                task: "<pool>".to_string(),
                expected: resources.len(),
                got: total.len(),
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        for name in resources {
            if !seen.insert(name.as_str()) {
                return Err(BankerError::DuplicateResource(name.clone()));
            }
        }
        Ok(Self {
            resources: resources.to_vec(),
            total: total.to_vec(),
            available: total.to_vec(),
            max: BTreeMap::new(),
            allocation: BTreeMap::new(),
        })
    }

    /// 注册一个任务及其**最大需求**。`max[i]` 对应 `resources[i]`。
    /// 需求不得超过该类资源总量（否则该任务永远无法满足，属于配置错误）。
    pub fn register(&mut self, task: &str, max: &[usize]) -> Result<(), BankerError> {
        if self.max.contains_key(task) {
            return Err(BankerError::DuplicateTask(task.to_string()));
        }
        if max.len() != self.resources.len() {
            return Err(BankerError::DimensionMismatch {
                task: task.to_string(),
                expected: self.resources.len(),
                got: max.len(),
            });
        }
        for (index, &wanted) in max.iter().enumerate() {
            if wanted > self.total[index] {
                // 用 OverNeed 表达"事前需求不可能满足"：need 此刻就是 max。
                return Err(BankerError::OverNeed {
                    task: task.to_string(),
                    resource: self.resources[index].clone(),
                    requested: wanted,
                    need: self.total[index],
                });
            }
        }
        self.max.insert(task.to_string(), max.to_vec());
        self.allocation
            .insert(task.to_string(), vec![0; self.resources.len()]);
        Ok(())
    }

    /// 注销任务并归还它占用的全部资源，返回归还量向量（长度 = 资源类型数）。
    pub fn unregister(&mut self, task: &str) -> Result<Vec<usize>, BankerError> {
        let allocated = self
            .allocation
            .remove(task)
            .ok_or_else(|| BankerError::UnknownTask(task.to_string()))?;
        self.max.remove(task);
        for (index, &amount) in allocated.iter().enumerate() {
            self.available[index] += amount;
        }
        Ok(allocated)
    }

    /// 任务的剩余需求 `need = max - allocation`。未注册返回 `None`。
    pub fn need(&self, task: &str) -> Option<Vec<usize>> {
        let max = self.max.get(task)?;
        let allocated = self.allocation.get(task)?;
        Some(
            max.iter()
                .zip(allocated)
                .map(|(&m, &a)| m.saturating_sub(a))
                .collect(),
        )
    }

    /// 任务的事前最大需求。
    pub fn max(&self, task: &str) -> Option<&[usize]> {
        self.max.get(task).map(Vec::as_slice)
    }

    /// 任务当前已分配。
    pub fn allocation(&self, task: &str) -> Option<&[usize]> {
        self.allocation.get(task).map(Vec::as_slice)
    }

    /// 当前可用资源向量。
    pub fn available(&self) -> &[usize] {
        &self.available
    }

    /// 资源总量向量。
    pub fn total(&self) -> &[usize] {
        &self.total
    }

    /// 资源名（顺序 = 向量下标）。
    pub fn resources(&self) -> &[String] {
        &self.resources
    }

    /// 资源名 → 下标。
    pub fn resource_index(&self, name: &str) -> Option<usize> {
        self.resources.iter().position(|r| r == name)
    }

    /// 全部已注册任务 id（按字典序）。
    pub fn tasks(&self) -> impl Iterator<Item = &str> {
        self.max.keys().map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.max.is_empty()
    }

    pub fn len(&self) -> usize {
        self.max.len()
    }

    /// 安全检查：返回一个**安全序列**（按此顺序执行，每个任务都能拿到剩余
    /// 资源并结束）。存在则返回 `Some(序列)`，系统处于不安全状态返回 `None`。
    ///
    /// 遍历 `max`（BTreeMap）保证同状态每次得到**同一个**序列，便于测试钉住。
    pub fn is_safe(&self) -> Option<Vec<String>> {
        let mut work = self.available.clone();
        let mut finished: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut sequence = Vec::with_capacity(self.max.len());

        loop {
            let mut progressed = false;
            for (task, max) in &self.max {
                if finished.contains(task) {
                    continue;
                }
                let allocated = &self.allocation[task];
                // need[i] <= work ?
                if max
                    .iter()
                    .zip(allocated)
                    .zip(&work)
                    .all(|((&m, &a), &w)| m.saturating_sub(a) <= w)
                {
                    // 假设此任务完成：它占有的资源全部归还 work。
                    for (index, &amount) in allocated.iter().enumerate() {
                        work[index] += amount;
                    }
                    finished.insert(task.clone());
                    sequence.push(task.clone());
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }

        if finished.len() == self.max.len() {
            Some(sequence)
        } else {
            None
        }
    }

    /// 请求资源（银行家请求算法）。详见模块文档三步流程。
    ///
    /// **无论返回哪种结果，不满足条件时账本都不变**——`Wait` 与 `Unsafe`
    /// 分支都保证零副作用，调用方可以直接重试。
    pub fn request(&mut self, task: &str, request: &[usize]) -> Result<RequestOutcome, BankerError> {
        if request.len() != self.resources.len() {
            return Err(BankerError::DimensionMismatch {
                task: task.to_string(),
                expected: self.resources.len(),
                got: request.len(),
            });
        }
        let need = self
            .need(task)
            .ok_or_else(|| BankerError::UnknownTask(task.to_string()))?;
        // 1. 请求不得超过事前声明的剩余需求。
        for (index, &wanted) in request.iter().enumerate() {
            if wanted > need[index] {
                return Err(BankerError::OverNeed {
                    task: task.to_string(),
                    resource: self.resources[index].clone(),
                    requested: wanted,
                    need: need[index],
                });
            }
        }
        // 2. 请求不得超过当前可用。
        for (index, &wanted) in request.iter().enumerate() {
            if wanted > self.available[index] {
                return Ok(RequestOutcome::Wait {
                    resource: self.resources[index].clone(),
                    requested: wanted,
                    available: self.available[index],
                });
            }
        }
        // 3. 试探分配。
        for (index, &wanted) in request.iter().enumerate() {
            self.available[index] -= wanted;
            self.allocation.get_mut(task).expect("注册时已建账")[index] += wanted;
        }
        match self.is_safe() {
            Some(safe_sequence) => Ok(RequestOutcome::Granted { safe_sequence }),
            None => {
                // 不安全 → 回滚，账本恢复到请求前。
                for (index, &wanted) in request.iter().enumerate() {
                    self.available[index] += wanted;
                    self.allocation.get_mut(task).expect("注册时已建账")[index] -= wanted;
                }
                Ok(RequestOutcome::Unsafe)
            }
        }
    }

    /// 释放任务占用的**部分**资源；`release[i]` 不得超过该任务已分配量。
    pub fn release(&mut self, task: &str, release: &[usize]) -> Result<(), BankerError> {
        if release.len() != self.resources.len() {
            return Err(BankerError::DimensionMismatch {
                task: task.to_string(),
                expected: self.resources.len(),
                got: release.len(),
            });
        }
        let allocated = self
            .allocation
            .get_mut(task)
            .ok_or_else(|| BankerError::UnknownTask(task.to_string()))?;
        for (index, &amount) in release.iter().enumerate() {
            if amount > allocated[index] {
                return Err(BankerError::OverNeed {
                    task: task.to_string(),
                    resource: self.resources[index].clone(),
                    requested: amount,
                    need: allocated[index],
                });
            }
        }
        for (index, &amount) in release.iter().enumerate() {
            allocated[index] -= amount;
            self.available[index] += amount;
        }
        Ok(())
    }

    /// 释放任务占用的**全部**资源，返回归还量向量。任务保持注册（可再次请求）。
    pub fn release_all(&mut self, task: &str) -> Result<Vec<usize>, BankerError> {
        let allocated = self
            .allocation
            .get_mut(task)
            .ok_or_else(|| BankerError::UnknownTask(task.to_string()))?
            .clone();
        self.release(task, &allocated)?;
        Ok(allocated)
    }

    /// 校验账本自洽：`available + 已分配总和 == 总量`。测试用。
    pub fn is_consistent(&self) -> bool {
        for (index, &total) in self.total.iter().enumerate() {
            let allocated: usize = self
                .allocation
                .values()
                .map(|task| task[index])
                .sum();
            if self.available[index] + allocated != total {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 经典教科书案例（Silberschatz《操作系统概念》银行家算法示例）：
    /// 5 个进程 P0..P4，3 类资源 A=10, B=5, C=7。
    ///
    /// ```text
    ///        Allocation   Max     Need
    ///        A B C       A B C    A B C
    /// P0     0 1 0       7 5 3    7 4 3
    /// P1     2 0 0       3 2 2    1 2 2
    /// P2     3 0 2       9 0 2    6 0 0
    /// P3     2 1 1       2 2 2    0 1 1
    /// P4     0 0 2       4 3 3    4 3 1
    /// Available = 3 3 2
    /// ```
    /// 安全序列：<P1, P3, P4, P0, P2>（BTreeMap 字典序扫描得到）。
    fn textbook() -> BankerPool {
        let mut pool = BankerPool::new(&["A", "B", "C"], &[10, 5, 7]).unwrap();
        let maxima: &[(&str, [usize; 3])] = &[
            ("P0", [7, 5, 3]),
            ("P1", [3, 2, 2]),
            ("P2", [9, 0, 2]),
            ("P3", [2, 2, 2]),
            ("P4", [4, 3, 3]),
        ];
        for (id, max) in maxima {
            pool.register(id, max).unwrap();
        }
        // 用 request 逐条授予初始分配（每条都必须 Granted 且账本守恒）。
        let initial_alloc: &[(&str, [usize; 3])] = &[
            ("P0", [0, 1, 0]),
            ("P1", [2, 0, 0]),
            ("P2", [3, 0, 2]),
            ("P3", [2, 1, 1]),
            ("P4", [0, 0, 2]),
        ];
        for (id, alloc) in initial_alloc {
            // 分配过程中状态逐步变化，每一步的安全序列不同（最终态的精确序列
            // 由 textbook_initial_state_has_expected_safe_sequence 单独钉住），
            // 这里只断言"每步都被批准且账本守恒"。
            assert!(
                matches!(
                    pool.request(id, alloc).unwrap(),
                    RequestOutcome::Granted { .. }
                ),
                "初始分配 {id} 应被批准"
            );
            assert!(pool.is_consistent());
        }
        assert_eq!(pool.available(), &[3, 3, 2]);
        assert!(pool.is_consistent());
        pool
    }

    #[test]
    fn textbook_initial_state_has_expected_safe_sequence() {
        let pool = textbook();
        assert_eq!(
            pool.is_safe(),
            Some(
                vec!["P1", "P3", "P4", "P0", "P2"]
                    .into_iter()
                    .map(str::to_string)
                    .collect()
            )
        );
        // 具体数字：need = max - allocation
        assert_eq!(pool.need("P0"), Some(vec![7, 4, 3]));
        assert_eq!(pool.need("P1"), Some(vec![1, 2, 2]));
        assert_eq!(pool.need("P2"), Some(vec![6, 0, 0]));
        assert_eq!(pool.need("P3"), Some(vec![0, 1, 1]));
        assert_eq!(pool.need("P4"), Some(vec![4, 3, 1]));
    }

    #[test]
    fn textbook_p1_request_1_0_2_is_granted() {
        let mut pool = textbook();
        // P1 请求 (1,0,2)：need=(1,2,2) 够、available=(3,3,2) 够，分配后仍安全。
        let outcome = pool.request("P1", &[1, 0, 2]).unwrap();
        assert_eq!(outcome, RequestOutcome::Granted {
            safe_sequence: vec!["P1", "P3", "P4", "P0", "P2"]
                .into_iter()
                .map(str::to_string)
                .collect()
        });
        // 具体数字：available 3 3 2 → 2 3 0；P1 分配 2 0 0 → 3 0 2。
        assert_eq!(pool.available(), &[2, 3, 0]);
        assert_eq!(pool.allocation("P1"), Some(&[3, 0, 2][..]));
        assert_eq!(pool.need("P1"), Some(vec![0, 2, 0]));
        assert!(pool.is_consistent());
    }

    #[test]
    fn textbook_p4_request_3_3_0_is_unsafe_and_rolled_back() {
        let mut pool = textbook();
        let before_available = pool.available().to_vec();
        let before_p4 = pool.allocation("P4").unwrap().to_vec();
        // P4 请求 (3,3,0)：need=(4,3,1) 够、available=(3,3,2) 也够，
        // 但试探分配后 available=(0,0,2)，没有任何任务的 need 能被满足 → 不安全。
        assert_eq!(
            pool.request("P4", &[3, 3, 0]).unwrap(),
            RequestOutcome::Unsafe
        );
        // 关键：不安全分支必须**完整回滚**，账本与请求前逐字节一致。
        assert_eq!(pool.available(), &before_available[..]);
        assert_eq!(pool.allocation("P4"), Some(&before_p4[..]));
        assert_eq!(pool.available(), &[3, 3, 2]);
        assert!(pool.is_consistent());
        assert!(pool.is_safe().is_some());
    }

    #[test]
    fn request_exceeding_available_waits_without_touching_books() {
        let mut pool = textbook();
        // P2 need=(6,0,0)，请求 (4,0,0) 合法但 available A=3 < 4 → 等待。
        assert_eq!(
            pool.request("P2", &[4, 0, 0]).unwrap(),
            RequestOutcome::Wait {
                resource: "A".to_string(),
                requested: 4,
                available: 3,
            }
        );
        assert_eq!(pool.available(), &[3, 3, 2]);
        assert_eq!(pool.allocation("P2"), Some(&[3, 0, 2][..]));
        assert!(pool.is_consistent());
    }

    #[test]
    fn request_exceeding_need_is_an_error() {
        let mut pool = textbook();
        // P0 need=(7,4,3)，请求 C=4 > 3 → 调用方 bug，不是资源不足。
        assert_eq!(
            pool.request("P0", &[0, 0, 4]),
            Err(BankerError::OverNeed {
                task: "P0".to_string(),
                resource: "C".to_string(),
                requested: 4,
                need: 3,
            })
        );
        assert_eq!(pool.available(), &[3, 3, 2]);
    }

    #[test]
    fn release_then_unsafe_request_becomes_safe() {
        let mut pool = textbook();
        // 先释放 P2 的全部资源：available 3 3 2 + 3 0 2 = 6 3 4。
        assert_eq!(pool.release_all("P2").unwrap(), vec![3, 0, 2]);
        assert_eq!(pool.available(), &[6, 3, 4]);
        assert_eq!(pool.allocation("P2"), Some(&[0, 0, 0][..]));
        // 此时 P4 请求 (3,3,0) 应该安全（这是教科书里"释放后可行"的结论）。
        assert!(matches!(
            pool.request("P4", &[3, 3, 0]).unwrap(),
            RequestOutcome::Granted { .. }
        ));
        assert_eq!(pool.available(), &[3, 0, 4]);
        assert!(pool.is_consistent());
        assert!(pool.is_safe().is_some());
    }

    #[test]
    fn deadlock_cycle_is_refused_before_it_forms() {
        // 构造一个真实的环形等待：两个任务都要 worker + 同一本词典。
        // 资源：2 个 worker 槽，1 把 dict:qiaoma 锁。
        let mut pool = BankerPool::new(&["worker", "dict:qiaoma"], &[2, 1]).unwrap();
        pool.register("qiaoma", &[1, 1]).unwrap();
        pool.register("qiaoma_jf", &[1, 1]).unwrap();

        // qiaoma 先拿 (worker=1, lock=1)：分配后 available=(1,0)。
        // 安全序列检查：qiaoma need 归零后可归还资源，qiaoma_jf 仍可完成 → 安全。
        assert!(matches!(
            pool.request("qiaoma", &[1, 1]).unwrap(),
            RequestOutcome::Granted { .. }
        ));
        assert_eq!(pool.available(), &[1, 0]);

        // qiaoma_jf 请求 (worker=1, lock=1)：lock available=0 → 等待（不是 Unsafe）。
        assert_eq!(
            pool.request("qiaoma_jf", &[1, 1]).unwrap(),
            RequestOutcome::Wait {
                resource: "dict:qiaoma".to_string(),
                requested: 1,
                available: 0,
            }
        );
        assert_eq!(pool.available(), &[1, 0]);
        // qiaoma 完成后归还，qiaoma_jf 立刻可获批。
        assert_eq!(pool.release_all("qiaoma").unwrap(), vec![1, 1]);
        assert_eq!(pool.available(), &[2, 1]);
        assert!(matches!(
            pool.request("qiaoma_jf", &[1, 1]).unwrap(),
            RequestOutcome::Granted { .. }
        ));
        assert!(pool.is_consistent());
    }

    #[test]
    fn unsafe_allocation_is_rejected_even_when_resources_look_free() {
        // 经典"资源看起来够、但分配后无安全序列"的最小反例。
        // 2 个任务各需 2 个同类资源，总量 3：
        //   各分 1 后 available=1。谁先拿到剩下的 1 谁完成、另一个等待——安全。
        // 但若是 3 个任务各需 2、总量 4，各分 1 后 available=1，
        // 再给任一个 1 → 它完成归还 2，形成连锁 → 仍安全。
        // 用教科书反例：A=10 B=5 C=7 的 P4 请求已经覆盖，这里验证
        // 一个"分配后 neither P0(need 7 4 3) nor others 能被满足"的构造。
        let mut pool = BankerPool::new(&["R"], &[4]).unwrap();
        pool.register("a", &[3]).unwrap();
        pool.register("b", &[3]).unwrap();
        // a 拿 2，b 拿 1 → available=1。a need=1（可完成），b need=2（不可）。
        assert!(matches!(
            pool.request("a", &[2]).unwrap(),
            RequestOutcome::Granted { .. }
        ));
        assert!(matches!(
            pool.request("b", &[1]).unwrap(),
            RequestOutcome::Granted { .. }
        ));
        assert_eq!(pool.available(), &[1]);
        // b 再请求 1 → available 归 0：a need=1 拿不到、b need=1 也拿不到 → 不安全。
        assert_eq!(
            pool.request("b", &[1]).unwrap(),
            RequestOutcome::Unsafe
        );
        assert_eq!(pool.available(), &[1]);
        assert_eq!(pool.allocation("b"), Some(&[1][..]));
        // a 拿到最后 1 个完成后归还，b 才能完成。
        assert!(matches!(
            pool.request("a", &[1]).unwrap(),
            RequestOutcome::Granted { .. }
        ));
    }

    #[test]
    fn registry_errors_and_validation() {
        let mut pool = BankerPool::new(&["worker"], &[2]).unwrap();
        assert_eq!(
            pool.register("bad", &[3]),
            Err(BankerError::OverNeed {
                task: "bad".to_string(),
                resource: "worker".to_string(),
                requested: 3,
                need: 2,
            })
        );
        pool.register("a", &[1]).unwrap();
        assert_eq!(
            pool.register("a", &[1]),
            Err(BankerError::DuplicateTask("a".to_string()))
        );
        assert_eq!(
            pool.register("b", &[1, 2]),
            Err(BankerError::DimensionMismatch {
                task: "b".to_string(),
                expected: 1,
                got: 2,
            })
        );
        assert_eq!(
            pool.request("ghost", &[1]),
            Err(BankerError::UnknownTask("ghost".to_string()))
        );
        assert_eq!(
            pool.release("ghost", &[1]),
            Err(BankerError::UnknownTask("ghost".to_string()))
        );
        assert_eq!(
            BankerPool::new(&["x", "x"], &[1, 1]),
            Err(BankerError::DuplicateResource("x".to_string()))
        );
        assert_eq!(BankerPool::new(&[], &[]), Err(BankerError::EmptyResource));
        assert_eq!(
            BankerPool::new(&["x"], &[]),
            Err(BankerError::DimensionMismatch {
                task: "<pool>".to_string(),
                expected: 1,
                got: 0,
            })
        );
    }

    #[test]
    fn unregister_returns_everything_and_keeps_pool_consistent() {
        let mut pool = textbook();
        assert_eq!(pool.unregister("P1").unwrap(), vec![2, 0, 0]);
        assert_eq!(pool.available(), &[5, 3, 2]);
        assert_eq!(pool.need("P1"), None);
        assert!(pool.is_consistent());
        assert_eq!(
            pool.unregister("P1"),
            Err(BankerError::UnknownTask("P1".to_string()))
        );
    }

    #[test]
    fn partial_release_is_accounted_exactly() {
        let mut pool = textbook();
        // 只还 P0 的 B=1：available 3 3 2 → 3 4 2。
        pool.release("P0", &[0, 1, 0]).unwrap();
        assert_eq!(pool.available(), &[3, 4, 2]);
        assert_eq!(pool.allocation("P0"), Some(&[0, 0, 0][..]));
        // 再还超过已分配量 → 报错且账本不变。
        assert_eq!(
            pool.release("P0", &[1, 0, 0]),
            Err(BankerError::OverNeed {
                task: "P0".to_string(),
                resource: "A".to_string(),
                requested: 1,
                need: 0,
            })
        );
        assert_eq!(pool.available(), &[3, 4, 2]);
        assert!(pool.is_consistent());
    }

    #[test]
    fn safe_sequence_is_deterministic_and_recomputed_each_call() {
        let pool = textbook();
        let first = pool.is_safe();
        for _ in 0..20 {
            assert_eq!(pool.is_safe(), first);
        }
        // 输入 → 输出具体到序列。
        assert_eq!(
            first.unwrap(),
            vec!["P1", "P3", "P4", "P0", "P2"]
        );
    }
}
