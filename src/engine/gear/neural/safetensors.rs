//! `.safetensors` 文件加载(HuggingFace 格式的子集——只够读这个模型)。
//!
//! 格式: 8 字节小端 u64 头长度 + JSON 头(键→dtype/shape/offset) + 二进制数据区。
//! 水杉的模型权重是 F32 和 I8 两种 dtype。

use std::collections::HashMap;
use std::path::Path;

/// 一个张量(从 safetensors 解出)。
pub struct Tensor {
    pub dtype: DType,
    pub shape: Vec<usize>,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DType {
    F32,
    F16,
    I8,
}

/// IEEE 754 binary16 → binary32,含 subnormal 处理(水杉参考实现同款)。
fn half_to_f32(bits: u16) -> f32 {
    let sign = ((bits & 0x8000) as u32) << 16;
    let exponent = ((bits >> 10) & 0x1f) as u32;
    let mantissa = (bits & 0x03ff) as u32;
    let out: u32 = if exponent == 0 {
        if mantissa == 0 {
            sign
        } else {
            let leading = mantissa.leading_zeros();
            let exp = 134 - leading;
            let mant = (mantissa << (leading - 8)) & 0x007f_ffff;
            sign | (exp << 23) | mant
        }
    } else if exponent == 0x1f {
        sign | 0x7f80_0000 | (mantissa << 13)
    } else {
        sign | ((exponent + 127 - 15) << 23) | (mantissa << 13)
    };
    f32::from_bits(out)
}

impl Tensor {
    pub fn as_f32(&self) -> Vec<f32> {
        match self.dtype {
            DType::F32 => self
                .data
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
                .collect(),
            DType::F16 => self
                .data
                .chunks_exact(2)
                .map(|c| half_to_f32(u16::from_le_bytes(c.try_into().unwrap())))
                .collect(),
            _ => Vec::new(),
        }
    }
    pub fn as_i8(&self) -> Vec<i8> {
        match self.dtype {
            DType::I8 => self.data.iter().map(|&b| b as i8).collect(),
            _ => Vec::new(),
        }
    }
}

/// 加载 safetensors 文件,返回 (文件级元数据, 张量名 → Tensor)。
pub fn load(path: &Path) -> Option<(HashMap<String, String>, HashMap<String, Tensor>)> {
    let raw = std::fs::read(path).ok()?;
    if raw.len() < 8 {
        return None;
    }
    let header_len = u64::from_le_bytes(raw[..8].try_into().unwrap()) as usize;
    if raw.len() < 8 + header_len {
        return None;
    }
    let header_json: serde_json::Value =
        serde_json::from_slice(&raw[8..8 + header_len]).ok()?;
    let data_offset = 8 + header_len;

    let mut tensors = HashMap::new();
    let mut meta = HashMap::new();

    if let Some(map) = header_json.as_object() {
        for (key, value) in map {
            if key == "__metadata__" {
                if let Some(m) = value.as_object() {
                    for (k, v) in m {
                        meta.insert(k.clone(), v.as_str().unwrap_or("").to_string());
                    }
                }
                continue;
            }
            let dtype_str = value.get("dtype")?.as_str()?;
            let shape: Vec<usize> = value
                .get("shape")?
                .as_array()?
                .iter()
                .filter_map(|v| v.as_u64().map(|n| n as usize))
                .collect();
            let data_offsets = value.get("data_offsets")?.as_array()?;
            let begin = data_offsets.first()?.as_u64()? as usize;
            let end = data_offsets.get(1)?.as_u64()? as usize;
            let data_start = data_offset + begin;
            let data_end = data_offset + end;
            if data_end > raw.len() {
                return None;
            }
            let dtype = match dtype_str {
                "F32" => DType::F32,
                "F16" => DType::F16,
                "I8" => DType::I8,
                _ => continue,
            };
            tensors.insert(
                key.clone(),
                Tensor {
                    dtype,
                    shape,
                    data: raw[data_start..data_end].to_vec(),
                },
            );
        }
    }
    Some((meta, tensors))
}
