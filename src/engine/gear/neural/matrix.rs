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
pub fn dot_f32_f32(a: &[f32], b: &[f32]) -> f32 {
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
fn dot_f32_i8(a: &[f32], b: &[i8]) -> f32 {
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
