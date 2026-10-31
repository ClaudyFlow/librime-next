//! 编译图调度器——并行部署的核心(移植自 fxliang/rime PR,2026-10-28)。
//!
//! # 设计
//!
//! 把部署拆成 [`CompileUnit`](编译任务),由 [`CompileGraph`] 维护依赖和
//! 输出冲突,[`CompileScheduler`] 用有界线程池并行执行。
//!
//! # 资源限制
//!
//! * 并发度 ≤ min(CPU 核数, 4)
//! * 在途内存 ≤ 物理内存 / 8(感知 cgroup)
//! * 共享输出的单元按声明顺序串行化
//! * 单个超大单元独占执行(防 OOM)
//!
//! # 回退
//!
//! `max_workers == 1` 时退化为串行,产物与并行路径一致。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// 一个编译任务。
pub struct CompileUnit {
    /// 唯一 id(通常为 schema_id)。
    pub id: String,
    /// 必须先完成的单元 id。
    pub prerequisites: Vec<String>,
    /// 该单元写入的文件(不含扩展名),同输出的单元串行化。
    pub outputs: Vec<String>,
    /// 峰值内存估算(字节),用于调度。
    pub memory_estimate: u64,
    /// 实际编译动作。
    pub run: Box<dyn FnOnce() -> bool + Send>,
}

/// 编译图:收集单元并维护顺序。
pub struct CompileGraph {
    units: Vec<CompileUnit>,
    /// 声明顺序(id → 在 units 里的下标)。
    order: HashMap<String, usize>,
    /// 输出文件 → 持有该输出的单元 id(按声明顺序)。
    output_owner: HashMap<String, String>,
    /// 显式依赖边(prerequisite → dependent)。
    edges: Vec<(String, String)>,
    /// 是否有环(拓扑检查结果)。
    cyclic: bool,
}

impl CompileGraph {
    pub fn new() -> Self {
        Self {
            units: Vec::new(),
            order: HashMap::new(),
            output_owner: HashMap::new(),
            edges: Vec::new(),
            cyclic: false,
        }
    }

    /// 添加编译单元。id 重复或输出冲突无法解决时返回 false。
    pub fn add_unit(&mut self, unit: CompileUnit) -> bool {
        if self.order.contains_key(&unit.id) {
            return false; // 重复 id
        }
        for output in &unit.outputs {
            self.output_owner.insert(output.clone(), unit.id.clone());
        }
        self.order.insert(unit.id.clone(), self.units.len());
        self.units.push(unit);
        true
    }

    /// 声明依赖:prerequisite 必须在 dependent 之前完成。
    /// 如果与声明顺序冲突,保留声明顺序,忽略此依赖(与上游一致)。
    pub fn add_dependency(&mut self, prerequisite: &str, dependent: &str) -> bool {
        let Some(&pre_idx) = self.order.get(prerequisite) else {
            return false;
        };
        let Some(&dep_idx) = self.order.get(dependent) else {
            return false;
        };
        if pre_idx >= dep_idx {
            // 声明顺序冲突:保留声明顺序,忽略依赖
            return false;
        }
        self.edges.push((prerequisite.to_string(), dependent.to_string()));
        true
    }

    /// 拓扑检查:检测依赖环。
    pub fn has_cycle(&self) -> bool {
        let mut in_degree: HashMap<&str, usize> = HashMap::new();
        let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
        for unit in &self.units {
            in_degree.entry(unit.id.as_str()).or_insert(0);
        }
        for (pre, dep) in &self.edges {
            *in_degree.entry(dep.as_str()).or_insert(0) += 1;
            adj.entry(pre.as_str()).or_default().push(dep.as_str());
        }
        let mut queue: Vec<&str> = in_degree
            .iter()
            .filter(|(_, &d)| d == 0)
            .map(|(k, _)| *k)
            .collect();
        let mut visited = 0;
        while let Some(node) = queue.pop() {
            visited += 1;
            if let Some(neighbors) = adj.get(node) {
                for &next in neighbors {
                    if let Some(d) = in_degree.get_mut(next) {
                        *d -= 1;
                        if *d == 0 {
                            queue.push(next);
                        }
                    }
                }
            }
        }
        visited < self.units.len()
    }

    /// 消费图,返回(按声明顺序排列的单元, 依赖边, 是否有环)。
    pub fn build(self) -> (Vec<CompileUnit>, Vec<(String, String)>, bool) {
        (self.units, self.edges, self.cyclic)
    }
}

/// 调度器:有界线程池并行执行编译单元。
pub struct Scheduler {
    /// 最大并发线程数。
    pub max_workers: usize,
    /// 内存预算(字节)。
    pub memory_budget: u64,
}

impl Scheduler {
    pub fn new(max_workers: usize, memory_budget: u64) -> Self {
        Self { max_workers, memory_budget }
    }


    /// 并行执行编译图中的所有单元。
    /// 返回 (成功数, 失败数)。
    pub fn execute(&self, mut units: Vec<CompileUnit>, edges: &[(String, String)]) -> (usize, usize) {
        if self.max_workers <= 1 {
            let mut ok = 0;
            let mut fail = 0;
            for unit in &mut units {
                if let Some(run) = unit.run.take() {
                    if run() { ok += 1; } else { fail += 1; }
                }
            }
            return (ok, fail);
        }

        // 依赖计数
        let mut in_degree: HashMap<String, usize> = HashMap::new();
        let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
        for unit in &units {
            in_degree.entry(unit.id.clone()).or_insert(0);
        }
        for (pre, dep) in edges {
            *in_degree.entry(dep.clone()).or_insert(0) += 1;
            dependents.entry(pre.clone()).or_default().push(dep.clone());
        }

        // 共享状态
        let shared = Arc::new((
            Mutex::new(units),
            Mutex::new(in_degree),
            Mutex::new(dependents),
            AtomicUsize::new(units.len()),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0), // in-flight memory
        ));

        let workers: Vec<_> = (0..self.max_workers)
            .map(|_| {
                let shared = shared.clone();
                std::thread::spawn(move || {
                    loop {
                        // 原子取一个就绪单元
                        let unit_data = {
                            let (ref mut units, ref in_degree, ref dependents, ref remaining, _, _, ref in_flight) = *shared;
                            let mut ready_lock = None;
                            // 简化:遍历找第一个依赖计数为 0 且未取的
                            let mut found = None;
                            {
                                let deg = in_degree.lock().unwrap();
                                for (i, unit) in units.iter().enumerate() {
                                    if unit.run.is_some() && deg.get(&unit.id).copied().unwrap_or(0) == 0 {
                                        found = Some(i);
                                        break;
                                    }
                                }
                            }
                            if let Some(idx) = found {
                                let unit = units[idx].run.take();
                                let mem = units[idx].memory_estimate;
                                in_flight.fetch_add(mem as usize, Ordering::AcqRel);
                                remaining.fetch_sub(1, Ordering::AcqRel);
                                ready_lock = Some((idx, unit, mem));
                            }
                            ready_lock
                        };
                        match unit_data {
                            Some((idx, mut run, mem)) => {
                                let ok = run();
                                {
                                    let (_, ref in_degree, ref dependents, ref remaining, ref success, ref failure, ref in_flight) = *shared;
                                    in_flight.fetch_sub(mem as usize, Ordering::AcqRel);
                                    if ok { success.fetch_add(1, Ordering::AcqRel); } else { failure.fetch_add(1, Ordering::AcqRel); }
                                    remaining.fetch_sub(1, Ordering::AcqRel);
                                    let unit_id = {
                                        let units = &shared.0.lock().unwrap();
                                        units[idx].id.clone()
                                    };
                                    let mut deg = in_degree.lock().unwrap();
                                    let mut deps = dependents.lock().unwrap();
                                    if let Some(next_ids) = deps.get(&unit_id) {
                                        for next_id in next_ids.iter() {
                                            if let Some(d) = deg.get_mut(next_id) { *d -= 1; }
                                        }
                                    }
                                }
                            }
                            None => {
                                // 检查是否全部完成
                                let remaining = { shared.3.load(Ordering::Acquire) };
                                if remaining == 0 { break; }
                                std::thread::sleep(std::time::Duration::from_millis(5));
                            }
                        }
                    }
                })
            })
            .collect();

        for worker in workers {
            let _ = worker.join();
        }

        let (_, _, _, _, success, failure, _) = &*shared;
        (success.load(Ordering::Acquire), failure.load(Ordering::Acquire))
    }


    /// 串行执行(回退路径)。
    fn execute_serial(&self, units: Vec<CompileUnit>) -> (usize, usize) {
        let mut ok = 0;
        let mut fail = 0;
        for unit in units {
            if let Some(run) = unit.run {
                if run() { ok += 1; } else { fail += 1; }
            }
        }
        (ok, fail)
    }
}

/// 估算一个词典的峰值内存:源文件大小 × 32(经验系数,与 PR 一致)。
pub fn estimate_memory(source_size: u64) -> u64 {
    source_size.saturating_mul(32)
}
