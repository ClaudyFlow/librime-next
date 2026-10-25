//! librime `src/rime/algo/utilities.h`(37 行) + `utilities.cc`(53 行) 的 Rust 移植，
//! 外加 `src/rime/algo/fs.h`(19 行) 的 `filesystem::to_time_t`。
//!
//! ## `CompareVersionString` 的古怪之处（`utilities.cc:11-30`）
//!
//! 用 `.` 分段、逐段按十进制累加比较，但**越界读不报错**：
//! `while (i < m || j < n)` —— 一方走完后，另一个还会继续比，
//! 走完的那一侧 `v` 恒为 0。所以 `"1.2"` < `"1.2.1"`，而 `"1.2.0"` == `"1.2"`。
//! 分段时 `++i` 无条件自增跳过 `.`，即使当前字符不是 `.`（最后一段也会多跳一格）。
//!
//! ## CRC32
//!
//! `ChecksumComputer` 用 `boost::crc_32_type`（标准 IEEE CRC-32，反射、初值/终值
//! `0xFFFFFFFF`）。boost 的构造函数参数 `initial_remainder` 是**"上次的余数"**，
//! 也就是 `crc_32_type(0)` 等价于全新的一次计算 —— 所以 `ChecksumComputer::new(0)`
//! 必须给出标准 CRC-32（`"123456789"` → `0xCBF43926`），这是本模块的锚点。
#![allow(dead_code)]

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// `utilities.cc:11-30`：版本号比较。返回 -1 / 0 / 1。
pub fn compare_version_string(x: &str, y: &str) -> i32 {
    let xb = x.as_bytes();
    let yb = y.as_bytes();
    let (m, n) = (xb.len(), yb.len());
    let (mut i, mut j) = (0usize, 0usize);
    while i < m || j < n {
        let mut v1: i32 = 0;
        let mut v2: i32 = 0;
        while i < m && xb[i] != b'.' {
            v1 = v1 * 10 + (xb[i] as i32 - b'0' as i32);
            i += 1;
        }
        i += 1; // `utilities.cc:19`：无条件跳过 '.'
        while j < n && yb[j] != b'.' {
            v2 = v2 * 10 + (yb[j] as i32 - b'0' as i32);
            j += 1;
        }
        j += 1; // `utilities.cc:24`
        if v1 > v2 {
            return 1;
        }
        if v1 < v2 {
            return -1;
        }
    }
    0
}

/// CRC-32（IEEE 802.3，反射，polynomial `0x04C11DB7` 的反射形式 `0xEDB88320`）。
/// 与 `boost::crc_32_type` 一致。
pub struct ChecksumComputer {
    /// 寄存器；`boost::crc_optimal` 里 `rem_` 的对应物。
    rem: u32,
}

impl Default for ChecksumComputer {
    fn default() -> Self {
        Self::new(0)
    }
}

impl ChecksumComputer {
    /// `initial_remainder = 0` 表示"从头开始算"（`utilities.cc:32-33`）。
    pub fn new(initial_remainder: u32) -> Self {
        // boost 的 `crc_optimal(init_rem)` 把传入值当作**已累积的余数**；
        // 传 0 等价于初始化寄存器为 0xFFFFFFFF，因此这里异或回来。
        Self {
            rem: initial_remainder ^ 0xFFFF_FFFF,
        }
    }

    /// `utilities.cc:45-47`：逐段喂字节。
    fn process_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.rem = (self.rem >> 8) ^ CRC32_TABLE[((self.rem ^ b as u32) & 0xFF) as usize];
        }
    }

    /// `utilities.cc:35-43`：以 64 KB 分块读文件；**读不到就当空文件**（C++ 直接 return）。
    pub fn process_file(&mut self, file_path: &Path) {
        let Ok(bytes) = std::fs::read(file_path) else {
            return; // C++: `if (!fin) return;`
        };
        // 分块喂（行为与一次喂完等价，保留分块只是为了照抄 buffer_size 的意图）
        const BUFFER_SIZE: usize = 64 * 1024;
        for chunk in bytes.chunks(BUFFER_SIZE) {
            self.process_bytes(chunk);
        }
    }

    /// `utilities.cc:49-51`：`crc_.checksum()` = 余数再异或终值。
    pub fn checksum(&self) -> u32 {
        self.rem ^ 0xFFFF_FFFF
    }
}

/// `utilities.h:29-33`：一次性算一个文件的 CRC32。
/// `dict_compiler` 的 `dict_file_checksum` / `Checksum(schema_file)` 用它。
pub fn checksum(file_path: &Path) -> u32 {
    let mut c = ChecksumComputer::new(0);
    c.process_file(file_path);
    c.checksum()
}

static CRC32_TABLE: [u32; 256] = build_crc32_table();

const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// `fs.h:9-16` 的 `filesystem::to_time_t`：把 `file_time_type` 平移到
/// `system_clock` 再转 `time_t`。Rust 侧直接用 `SystemTime` → Unix 秒。
pub fn to_time_t(tp: SystemTime) -> i64 {
    match tp.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        // 早于 1970 的时间点：C++ 侧同样会得到负值
        Err(e) => -(e.duration().as_secs() as i64) - if e.duration().subsec_nanos() > 0 { 1 } else { 0 },
    }
}

/// `fs.h` 的便捷入口：文件的 mtime → Unix 秒。
pub fn file_modified_time(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .map(to_time_t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_vector() {
        // 锚点：标准 CRC-32 对 "123456789" = 0xCBF43926（= boost::crc_32_type 的文档示例）
        let mut c = ChecksumComputer::new(0);
        c.process_bytes(b"123456789");
        assert_eq!(c.checksum(), 0xCBF4_3926);
        // 空输入 → 0
        assert_eq!(ChecksumComputer::new(0).checksum(), 0);
    }

    #[test]
    fn crc32_incremental_matches_one_shot() {
        let mut a = ChecksumComputer::new(0);
        a.process_bytes(b"hello world");
        let mut b = ChecksumComputer::new(0);
        b.process_bytes(b"hello ");
        b.process_bytes(b"world");
        assert_eq!(a.checksum(), b.checksum());
    }

    #[test]
    fn compare_version_string_cases() {
        assert_eq!(compare_version_string("1.2.3", "1.2.3"), 0);
        assert_eq!(compare_version_string("1.2.3", "1.2.4"), -1);
        assert_eq!(compare_version_string("1.2.10", "1.2.9"), 1);
        // 段数不同：短的那边后续段按 0 算
        assert_eq!(compare_version_string("1.2", "1.2.0"), 0);
        assert_eq!(compare_version_string("1.2", "1.2.1"), -1);
        assert_eq!(compare_version_string("2", "1.9.9"), 1);
        // 非数字字符按 C++ 的 `c - '0'` 走（'a' - '0' = 49），不做校验
        assert_eq!(compare_version_string("1.0.0", "1.0.0"), 0);
    }

    #[test]
    fn checksum_of_file() {
        let path = std::env::temp_dir().join("librime_next_crc32_test.bin");
        std::fs::write(&path, b"123456789").expect("write");
        assert_eq!(checksum(&path), 0xCBF4_3926);
        let _ = std::fs::remove_file(&path);
        // 不存在的文件 → C++ 的 `if (!fin) return;`，即空输入 → 0
        assert_eq!(checksum(Path::new("no such file")), 0);
    }

    #[test]
    fn to_time_t_roundtrip() {
        let now = SystemTime::now();
        let secs = to_time_t(now);
        let back = now.duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert!((secs - back as i64).abs() <= 1);
        assert_eq!(to_time_t(UNIX_EPOCH), 0);
    }
}
