//! 权重矩阵——int8 量化或 f32,按文件存储精度持有,不展开。
//!
//! 量化矩阵不展开:per-output-row scale 是整行的常数因子,
//! 从 dot product 里提出来,在结果上乘一次即可。

/// 权重矩阵(int8 量化或 f32)。
pub struct Matrix {
    pub quantized: bool,
    pub floats: Vec<f32>,
    pub ints: Vec<i8>,
    /// per-output-row scale(量化时用)。
    pub scales: Vec<f32>,
}

impl Matrix {
    pub fn len(&self) -> usize {
        if self.quantized { self.ints.len() } else { self.floats.len() }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `dot(x, row) * scale[row]`,非量化时 scale = 1。
    pub fn dot_row(&self, x: &[f32], row: usize, width: usize) -> f32 {
        let offset = row * width;
        if self.quantized {
            let ints = &self.ints[offset..offset + width];
            let raw = dot_f32_i8(x, ints);
            raw * self.scales[row]
        } else {
            dot_f32_f32(x, &self.floats[offset..offset + width])
        }
    }

    /// 取一行作为 f32(用于 token embedding 表)。
    pub fn row_into(&self, index: usize, width: usize, out: &mut [f32]) {
        let offset = index * width;
        if self.quantized {
            for (i, j) in (offset..offset + width).enumerate() {
                out[i] = self.ints[j] as f32 * self.scales.get(index).copied().unwrap_or(1.0);
            }
        } else {
            out.copy_from_slice(&self.floats[offset..offset + width]);
        }
    }
}

/// 4-lane dot product——FP 延迟隐藏(水杉注释:FP add 延迟 4 周期,
/// 单累加链让 FP 空闲 3/4;四路并行让向量化器折叠成 packed adds)。
///
/// x86_64 上运行时检测 AVX2+FMA:命中就走 `_mm256_fmadd_ps` 八车道内核
/// (水杉 `MSIME_NEURAL_AVX2` 的同款加速,Rust 用 `std::arch` 表达);
/// 未命中/其他架构退回标量四路。累加顺序改变 ⇒ 末位有效数字与标量不同——
/// 与 C++ 参考的取舍一致(对数概率只做相对比较,有富余)。
pub fn dot_f32_f32(a: &[f32], b: &[f32]) -> f32 {
    #[cfg(target_arch = "x86_64")]
    if avx2_fma_available() && a.len() >= 32 {
        // SAFETY:上面刚检测过 avx2+fma;两切片长度由 min 对齐。
        return unsafe { dot_f32_f32_avx2(a, b) };
    }
    let (mut s0, mut s1, mut s2, mut s3) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    let n = a.len().min(b.len());
    let mut i = 0;
    while i + 4 <= n {
        s0 += a[i] * b[i];
        s1 += a[i + 1] * b[i + 1];
        s2 += a[i + 2] * b[i + 2];
        s3 += a[i + 3] * b[i + 3];
        i += 4;
    }
    let mut total = (s0 + s1) + (s2 + s3);
    while i < n {
        total += a[i] * b[i];
        i += 1;
    }
    total
}

/// f32 × i8 点积(int8 量化矩阵用)。
///
/// AVX2 路径直接吃 i8:`cvtepi8`→`cvtepi32`→`cvtepi32_ps`→fmadd,
/// 省掉标量路径"每元素 i8→f32 展宽"的转换开销(水杉标量注释里的痛点)。
fn dot_f32_i8(a: &[f32], b: &[i8]) -> f32 {
    #[cfg(target_arch = "x86_64")]
    if avx2_fma_available() && a.len() >= 32 {
        // SAFETY:同上。
        return unsafe { dot_f32_i8_avx2(a, b) };
    }
    let (mut s0, mut s1, mut s2, mut s3) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    let n = a.len().min(b.len());
    let mut i = 0;
    while i + 4 <= n {
        s0 += a[i] * b[i] as f32;
        s1 += a[i + 1] * b[i + 1] as f32;
        s2 += a[i + 2] * b[i + 2] as f32;
        s3 += a[i + 3] * b[i + 3] as f32;
        i += 4;
    }
    let mut total = (s0 + s1) + (s2 + s3);
    while i < n {
        total += a[i] * b[i] as f32;
        i += 1;
    }
    total
}

#[cfg(target_arch = "x86_64")]
fn avx2_fma_available() -> bool {
    static FLAGS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAGS.get_or_init(|| {
        std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma")
    })
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn dot_f32_f32_avx2(a: &[f32], b: &[f32]) -> f32 {
    use std::arch::x86_64::*;
    let n = a.len().min(b.len());
    let mut acc0 = _mm256_setzero_ps();
    let mut acc1 = _mm256_setzero_ps();
    let mut acc2 = _mm256_setzero_ps();
    let mut acc3 = _mm256_setzero_ps();
    let mut i = 0;
    // 四条独立 fmadd 链:隐藏 FMA 4 周期延迟(与标量四路同一理由)。
    while i + 32 <= n {
        let pa = a.as_ptr().add(i);
        let pb = b.as_ptr().add(i);
        acc0 = _mm256_fmadd_ps(_mm256_loadu_ps(pa), _mm256_loadu_ps(pb), acc0);
        acc1 = _mm256_fmadd_ps(
            _mm256_loadu_ps(pa.add(8)),
            _mm256_loadu_ps(pb.add(8)),
            acc1,
        );
        acc2 = _mm256_fmadd_ps(
            _mm256_loadu_ps(pa.add(16)),
            _mm256_loadu_ps(pb.add(16)),
            acc2,
        );
        acc3 = _mm256_fmadd_ps(
            _mm256_loadu_ps(pa.add(24)),
            _mm256_loadu_ps(pb.add(24)),
            acc3,
        );
        i += 32;
    }
    while i + 8 <= n {
        acc0 = _mm256_fmadd_ps(
            _mm256_loadu_ps(a.as_ptr().add(i)),
            _mm256_loadu_ps(b.as_ptr().add(i)),
            acc0,
        );
        i += 8;
    }
    let mut sum = hsum256(acc0) + hsum256(acc1) + hsum256(acc2) + hsum256(acc3);
    while i < n {
        sum += a[i] * b[i];
        i += 1;
    }
    sum
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn dot_f32_i8_avx2(a: &[f32], b: &[i8]) -> f32 {
    use std::arch::x86_64::*;
    let n = a.len().min(b.len());
    let mut acc0 = _mm256_setzero_ps();
    let mut acc1 = _mm256_setzero_ps();
    let mut acc2 = _mm256_setzero_ps();
    let mut acc3 = _mm256_setzero_ps();
    let mut i = 0;
    while i + 32 <= n {
        // 每 8 个 i8 只占 8 字节，而 `*const __m128i` 的 `.add()` 前进的是
        // **16 字节**。早先版本写 `pi.add(1/2/3)`，于是第 2~4 条 lane 配到的是
        // 错位的字节 —— 点积算成了"a 的前半段 × b 的后半段"这种拼盘。
        // 这个内核只被 `dot_row`（单步参考 API）用到，所以生产路径一直没暴露；
        // 一旦拿它做生成（`next_log_probabilities`），输出立刻变成噪声。
        // 正确做法：按字节偏移各取低 64 位。
        let bytes = b.as_ptr().add(i);
        let lane0 = _mm256_cvtepi32_ps(_mm256_cvtepi8_epi32(_mm_loadl_epi64(
            bytes as *const __m128i,
        )));
        let lane1 = _mm256_cvtepi32_ps(_mm256_cvtepi8_epi32(_mm_loadl_epi64(
            bytes.add(8) as *const __m128i,
        )));
        let lane2 = _mm256_cvtepi32_ps(_mm256_cvtepi8_epi32(_mm_loadl_epi64(
            bytes.add(16) as *const __m128i,
        )));
        let lane3 = _mm256_cvtepi32_ps(_mm256_cvtepi8_epi32(_mm_loadl_epi64(
            bytes.add(24) as *const __m128i,
        )));
        acc0 = _mm256_fmadd_ps(_mm256_loadu_ps(a.as_ptr().add(i)), lane0, acc0);
        acc1 = _mm256_fmadd_ps(_mm256_loadu_ps(a.as_ptr().add(i + 8)), lane1, acc1);
        acc2 = _mm256_fmadd_ps(_mm256_loadu_ps(a.as_ptr().add(i + 16)), lane2, acc2);
        acc3 = _mm256_fmadd_ps(_mm256_loadu_ps(a.as_ptr().add(i + 24)), lane3, acc3);
        i += 32;
    }
    while i + 8 <= n {
        acc0 = _mm256_fmadd_ps(
            _mm256_loadu_ps(a.as_ptr().add(i)),
            _mm256_cvtepi32_ps(_mm256_cvtepi8_epi32(_mm_loadu_si128(
                b.as_ptr().add(i) as *const __m128i
            ))),
            acc0,
        );
        i += 8;
    }
    let mut sum = hsum256(acc0) + hsum256(acc1) + hsum256(acc2) + hsum256(acc3);
    while i < n {
        sum += a[i] * b[i] as f32;
        i += 1;
    }
    sum
}

#[cfg(target_arch = "x86_64")]
unsafe fn hsum256(v: std::arch::x86_64::__m256) -> f32 {
    use std::arch::x86_64::*;
    let high = _mm256_extractf128_ps(v, 1);
    let low = _mm256_castps256_ps128(v);
    let sum128 = _mm_add_ps(low, high);
    // 128 位水平加:两次 movehdup + add,或直接存内存再和——存内存最直白。
    let mut tmp = [0.0f32; 4];
    _mm_storeu_ps(tmp.as_mut_ptr(), sum128);
    tmp[0] + tmp[1] + tmp[2] + tmp[3]
}

/// 精确 GELU(Abramowitz & Stegun 7.1.26 erf 近似,误差 < 1.5e-7)。
/// **不能用 tanh 近似**——模型不是用 tanh 训练的。
pub fn gelu(x: f32) -> f32 {
    const INV_SQRT2: f32 = 0.707_106_78;
    0.5 * x * (1.0 + erf_approx(x * INV_SQRT2))
}

fn erf_approx(x: f32) -> f32 {
    let sign = if x < 0.0 { -1.0f32 } else { 1.0f32 };
    let x = x.abs();
    let t = 1.0 / (1.0 + 0.327_591_1 * x);
    let poly = t
        * (0.254_829_6
            + t * (-0.284_496_74 + t * (1.421_413_7 + t * (-1.453_152 + t * 1.061_405_4))));
    sign * (1.0 - poly * (-x * x).exp())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `dot_f32_i8` 必须逐位等于"反量化后做 f32 点积"。
    ///
    /// 2026-11-03 的教训：AVX2 分支把 `*const __m128i` 的 `.add()`（前进 16 字节）
    /// 当成 i8 的 8 字节步长，第 2~4 条 lane 全配错位。因为只有 `dot_row`
    /// （单步参考 API）走这条内核，生产打分路径用的是 `dot_f32_f32`，
    /// 所以错了很久没人发现 —— 一旦拿它做生成，输出直接成噪声。
    ///
    /// 长度覆盖 8（标量尾）、32/64（32 字节主循环）三种情况，值刻意取正负混合，
    /// 错位或符号截断都会让测试炸。
    #[test]
    fn dot_f32_i8_matches_dequantized_f32() {
        let cases: &[usize] = &[8, 32, 64, 96];
        for &len in cases {
            let a: Vec<f32> = (0..len).map(|i| (i as f32 - 7.0) * 0.13).collect();
            let b: Vec<i8> = (0..len)
                .map(|i| if i % 3 == 0 { -((i as i32 % 50) as i8) } else { (i as i32 % 97) as i8 })
                .collect();
            let expected: f32 = a.iter().zip(&b).map(|(x, y)| x * (*y as f32)).sum();
            let actual = dot_f32_i8(&a, &b);
            assert!(
                (actual - expected).abs() <= expected.abs().max(1.0) * 1e-5,
                "len={len}: 内核 {actual} vs 反量化参考 {expected}"
            );
        }
    }

    /// 点积与向量顺序不可交换:换掉后半段必须改变结果。
    ///
    /// 专门盯"错位拼接"这种错法 —— 它能让两个不同的向量算出同一个值。
    #[test]
    fn dot_f32_i8_is_order_sensitive() {
        let a: Vec<f32> = (0..64).map(|i| 1.0 + i as f32 * 0.01).collect();
        let first: Vec<i8> = (0..64).map(|i| (i % 40) as i8).collect();
        let mut second = first.clone();
        second.swap(2, 40);
        assert_ne!(dot_f32_i8(&a, &first), dot_f32_i8(&a, &second));
    }
}
