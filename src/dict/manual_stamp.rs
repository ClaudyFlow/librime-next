//! `dict/manual_stamp_codec.h`（librime-evo）移植：手排时间戳的**短码**编解码。
//!
//! 手排位置需要"谁更新"的判定（两处手排冲突时新的覆盖旧的），而 64 位十进制
//! 时间戳在 userdb 的每个 value 里都要多占十几个字节 —— 十万条就是 1.5 MB 的
//! 纯噪声。evo 的解法是**以 2026-01-01 为纪元、转 36 进制**，
//! 秒数变成 5~6 个字符（`6fn3k`），比十进制短一半以上。
//!
//! ```text
//! 0                      → "0"      （未手排）
//! 1767225600 (2026-01-01) → "1"
//! 1767225601             → "2"
//! 1798761600 (2027-01-01) → "6fn3k"
//! ```
//!
//! 注意 `FromUnixSeconds` 把"早于纪元"的时间压成 1（而不是负数或 0）：
//! 0 专门留给"没有手排"，系统时钟被拨回 2025 也不能让手排凭空消失。

/// 纪元：2026-01-01 00:00:00 UTC。
pub const EPOCH_UNIX_SECONDS: u64 = 1_767_225_600;

const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// `Format(value)`：36 进制小写、无前导零；`0` 单独写作 `"0"`。
pub fn format(value: u64) -> String {
    if value == 0 {
        return "0".to_string();
    }
    let mut reversed = [0u8; 13];
    let mut size = 0usize;
    let mut rest = value;
    while rest > 0 && size < reversed.len() {
        reversed[size] = DIGITS[(rest % 36) as usize];
        rest /= 36;
        size += 1;
    }
    reversed[..size].iter().rev().map(|b| *b as char).collect()
}

/// `Parse(raw)`：`"0"` → 0；非法（空、超长、前导零、非 36 进制字符、溢出）→ `None`。
pub fn parse(raw: &str) -> Option<u64> {
    if raw.is_empty() || raw.len() > 13 || (raw.len() > 1 && raw.starts_with('0')) {
        return None;
    }
    let mut value: u64 = 0;
    for byte in raw.bytes() {
        let digit = match byte {
            b'0'..=b'9' => u64::from(byte - b'0'),
            b'a'..=b'z' => u64::from(byte - b'a') + 10,
            _ => return None,
        };
        if value > (u64::MAX - digit) / 36 {
            return None;
        }
        value = value * 36 + digit;
    }
    Some(value)
}

/// `FromUnixSeconds(seconds)`：纪元之前的时刻压成 1（0 留给"未手排"）。
pub fn from_unix_seconds(seconds: u64) -> u64 {
    if seconds < EPOCH_UNIX_SECONDS {
        1
    } else {
        seconds - EPOCH_UNIX_SECONDS + 1
    }
}

/// 当前时刻的手排版本号（同一秒内的连续操作靠调用方 `max(last + 1)` 递推）。
pub fn now() -> u64 {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    from_unix_seconds(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_its_own_token() {
        assert_eq!(format(0), "0");
        assert_eq!(parse("0"), Some(0));
    }

    #[test]
    fn epoch_is_one_and_roundtrips() {
        assert_eq!(from_unix_seconds(EPOCH_UNIX_SECONDS), 1);
        assert_eq!(format(1), "1");
        // 2027-01-01 00:00:00 UTC = 纪元 + 365 天
        let value = from_unix_seconds(EPOCH_UNIX_SECONDS + 365 * 86_400);
        assert_eq!(format(value), format(31_536_001));
        assert_eq!(parse(&format(value)), Some(value));
    }

    #[test]
    fn before_epoch_saturates_to_one() {
        assert_eq!(from_unix_seconds(0), 1);
        assert_eq!(from_unix_seconds(EPOCH_UNIX_SECONDS - 1), 1);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert_eq!(parse(""), None, "空串非法");
        assert_eq!(parse("01"), None, "前导零非法（短码不允许）");
        assert_eq!(parse("ABC"), None, "只认小写");
        assert_eq!(parse("zzzzzzzzzzzzzz"), None, "超过 13 字符非法");
    }

    #[test]
    fn short_code_is_actually_short() {
        // 2026~2031 年的手排戳都落在 6 个字符以内，比十进制（10 位）短一半。
        let value = from_unix_seconds(EPOCH_UNIX_SECONDS + 5 * 365 * 86_400);
        assert!(format(value).len() <= 6, "实际 {}", format(value));
    }
}
