//! librime `src/rime/algo/dynamics.h`(18 行) 的 Rust 移植。
//!
//! 两条打分公式，纯 `f64`。**逐字照抄**，包括其中的魔数：
//!
//! * `formula_d`：`d + da * exp((ta - t) / 200)`（`dynamics.h:6-8`）
//! * `formula_p`：`m = s - (s - u) * pow(1 - exp(-t/10000), 10)`，
//!   然后按 `d < 20` 分两支（`dynamics.h:10-16`）
//!
//! 注意 `kM = 1 / (1 - exp(-0.005))` 是**常数分母**（约 200.5），不是 `pow` 的底数；
//! 而 `d` 除以 `kM` 之后再加进 pow，量纲上很怪——但 `dynamics.h:13-15` 就是这么写的。
#![allow(dead_code)]

/// 词频衰减公式（`dynamics.h:6-8`）。
///
/// * `d`  —— 当前词频/权重
/// * `t`  —— 距上次使用的时间（秒）
/// * `da` —— 衰减幅度
/// * `ta` —— 衰减时间常数
pub fn formula_d(d: f64, t: f64, da: f64, ta: f64) -> f64 {
    d + da * ((ta - t) / 200.0).exp()
}

/// 词频提升公式（`dynamics.h:10-16`）。
///
/// * `s` —— 上限（saturation）
/// * `u` —— 当前值
/// * `t` —— 时间
/// * `d` —— 增量
pub fn formula_p(s: f64, u: f64, t: f64, d: f64) -> f64 {
    // `dynamics.h:11`：kM = 1 / (1 - exp(-0.005)) ≈ 200.5
    let k_m = 1.0 / (1.0 - (-0.005f64).exp());
    let m = s - (s - u) * (1.0 - (-t / 10000.0).exp()).powi(10);
    if d < 20.0 {
        m + (0.5 - m) * (d / k_m)
    } else {
        m + (1.0 - m) * (4f64.powf(d / k_m) - 1.0) / 3.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 取几个点做**回归对比**用：数值全部来自本实现，与 C++ 逐字同式，
    /// 差异只可能来自 `exp` / `powf` 的最后一位。
    #[test]
    fn formula_d_samples() {
        assert_eq!(formula_d(0.0, 0.0, 0.0, 0.0), 0.0);
        // da = 0 时恒等于 d
        assert_eq!(formula_d(3.5, 1234.0, 0.0, 100.0), 3.5);
        // ta == t 时 exp(0) = 1，结果 = d + da
        assert_eq!(formula_d(1.0, 500.0, 2.0, 500.0), 3.0);
        // t 远大于 ta：exp 趋于 0，结果趋于 d
        let v = formula_d(1.0, 100000.0, 2.0, 0.0);
        assert!((v - 1.0).abs() < 1e-12, "got {v}");
        // 普通点
        let v = formula_d(1.0, 0.0, 2.0, 200.0);
        assert!((v - (1.0 + 2.0 * std::f64::consts::E)).abs() < 1e-12, "got {v}");
    }

    #[test]
    fn formula_p_samples() {
        let k_m = 1.0 / (1.0 - (-0.005f64).exp());
        assert!((k_m - 200.500_416_666_493_32).abs() < 1e-9, "kM = {k_m}");
        // ⚠️ 反直觉但必须照抄：t = 0 时 `(1 - exp(0))^10 = 0` → m = s，
        // 而第一支 `m + (0.5 - m) * (d / kM)` 在 d = 0 时等于 m = s，即**等于上限**。
        let m0 = formula_p(1.0, 0.0, 0.0, 0.0);
        assert!((m0 - 1.0).abs() < 1e-12, "got {m0}");
        // d 在 1..19 时反而**低于** m（因为 m > 0.5 时 `0.5 - m` 是负的）
        let small = formula_p(1.0, 0.0, 0.0, 10.0);
        assert!(small < 1.0, "got {small}");
        // d = 20 起走第二支，跨过 20 时分数是**跳上来**的（19→20 变大）
        assert!(formula_p(1.0, 0.0, 0.0, 20.0) > formula_p(1.0, 0.0, 0.0, 19.0));
        // d = 20 走第二支
        let v20 = formula_p(1.0, 0.0, 0.0, 20.0);
        assert!(v20 > 0.0);
        // ⚠️ 单调性是**分段**的，不是整体单调：
        //   d = 1..19（第一支）随 d **下降**（因为 m > 0.5 时 `0.5 - m` 为负）；
        //   d = 20 起（第二支）随 d **上升**；两支在 20 处跳一下。
        // 注意 t 必须 > 0：t = 0 时 m 恒等于 s = 1，第二支的 `(1 - m)` 把增量清零
        let first_branch: Vec<f64> = [1.0, 5.0, 10.0, 19.0]
            .iter()
            .map(|&d| formula_p(1.0, 0.0, 5000.0, d))
            .collect();
        for pair in first_branch.windows(2) {
            assert!(pair[1] < pair[0], "第一支应当下降: {first_branch:?}");
        }
        let second_branch: Vec<f64> = [20.0, 21.0, 50.0]
            .iter()
            .map(|&d| formula_p(1.0, 0.0, 5000.0, d))
            .collect();
        for pair in second_branch.windows(2) {
            assert!(pair[1] > pair[0], "第二支应当上升: {second_branch:?}");
        }
        assert!(second_branch[0] > first_branch[3], "20 处要跳上来");
        // ⚠️ 又一处反直觉：`m = s - (s - u) * (1 - exp(-t/10000))^10` 里，
        // t = 0 时 m = **s**（不是 u），t → ∞ 时 m → **u**。所以"时间过去"不是逼近上限，
        // 而是从上往下滑回当前值 u。d = 0 时 `formula_p` 直接返回 m，可以直接观察：
        //   t = 1000     → m ≈ s = 1
        //   t = 100000   → m ≈ u = 0
        let near_s = formula_p(1.0, 0.0, 1000.0, 0.0);
        let near_u = formula_p(1.0, 0.0, 100000.0, 0.0);
        assert!(near_s > 0.9999, "t=1000 应当贴着上限: {near_s}");
        assert!(near_u < 0.001, "t=100000 应当滑回当前值: {near_u}");
        assert!(near_u < near_s);
    }
}
