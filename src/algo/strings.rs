//! librime `src/rime/algo/strings.h`(45 行) + `strings.cc`(38 行) 的 Rust 移植。
//!
//! 只有 `split` / `join` 两个东西，但 `split` 的 `SkipToken` 分支有**两个古怪边界**，
//! 是逐个 `while` 条件照抄出来的，不要"顺手优化"：
//!
//! * `KeepToken` + 空串 → `[""]`（**一条空串**，不是空数组）；
//! * `SkipToken` + 全分隔符（`,,,`）→ `[]`（空数组）。
//!
//! 见 `strings.cc:7-31`。
#![allow(dead_code)]

/// `strings.h:15`：分隔符要不要保留成空 token。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitBehavior {
    /// 保留空 token（连续的 `delim` 会产生空串）
    KeepToken,
    /// 跳过空 token（连续/首尾 `delim` 都不产生空串）
    SkipToken,
}

/// 按 `delim` 里的**任意字符**切分。语义逐行照抄 `strings.cc:7-31`。
pub fn split(str: &str, delim: &str, behavior: SplitBehavior) -> Vec<String> {
    let bytes = str.as_bytes();
    let is_char_boundary = |pos: usize| pos == 0 || pos >= bytes.len() || str.is_char_boundary(pos);
    // C++ 侧的取子串（`str.substr(lastPos, pos - lastPos)`），pos == npos 时截到末尾。
    let substr = |start: usize, end: Option<usize>| -> String {
        let end = end.unwrap_or(bytes.len());
        if start >= end {
            return String::new();
        }
        if is_char_boundary(start) && is_char_boundary(end) {
            str[start..end].to_string()
        } else {
            // C++ 按下标切字节，可能切坏 UTF-8；这里不 panic，退化成替换字符。
            String::from_utf8_lossy(&bytes[start..end]).into_owned()
        }
    };
    // 以下三个小工具对应 `find_first_of` / `find_first_not_of` / `npos`。
    let find_first_of = |from: usize| -> Option<usize> {
        if from >= bytes.len() {
            return None;
        }
        // C++ 的 find_first_of 是**字符**集合匹配；delim 是 ASCII 时逐字节等价。
        bytes[from..]
            .iter()
            .position(|b| delim.as_bytes().contains(b))
            .map(|p| from + p)
    };
    let find_first_not_of = |from: usize| -> Option<usize> {
        if from >= bytes.len() {
            return None;
        }
        bytes[from..]
            .iter()
            .position(|b| !delim.as_bytes().contains(b))
            .map(|p| from + p)
    };

    let mut strings: Vec<String> = Vec::new();
    let mut last_pos: Option<usize> = match behavior {
        SplitBehavior::SkipToken => find_first_not_of(0),
        SplitBehavior::KeepToken => Some(0),
    };
    let mut pos: Option<usize> = last_pos.and_then(|p| find_first_of(p));

    // C++: `while (npos != pos || npos != lastPos)`
    while pos.is_some() || last_pos.is_some() {
        // `strings.cc:19` —— 只有 lastPos 一定存在时才取子串。
        // C++ 在 `lastPos == npos` 时 substr 会抛 out_of_range，但循环条件保证了
        // 此时 pos != npos，两种情况下 `substr(npos, ...)` 都不可达；这里保守处理。
        let start = last_pos.unwrap_or(bytes.len());
        strings.push(substr(start, pos));

        if behavior == SplitBehavior::SkipToken {
            // lastPos = str.find_first_not_of(delim, pos)
            last_pos = match pos {
                Some(p) => find_first_not_of(p),
                None => None,
            };
        } else {
            let Some(p) = pos else { break };
            last_pos = Some(p + 1);
        }
        pos = last_pos.and_then(|p| find_first_of(p));
    }
    strings
}

/// `strings.cc:35-37`：默认 `KeepToken`。
pub fn split_default(str: &str, delim: &str) -> Vec<String> {
    split(str, delim, SplitBehavior::KeepToken)
}

/// `strings.h:23-45` 的 `join`：把若干片段用 `delim` 连起来。
pub fn join<'a, I, T>(iter: I, delim: &str) -> String
where
    I: IntoIterator<Item = T>,
    T: AsRef<str> + 'a,
{
    let mut result = String::new();
    for (index, item) in iter.into_iter().enumerate() {
        if index > 0 {
            result.push_str(delim);
        }
        result.push_str(item.as_ref());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_keep_token() {
        assert_eq!(split("abc def", " ", SplitBehavior::KeepToken), vec!["abc", "def"]);
        // 连续分隔符 → 空 token
        assert_eq!(
            split("abc  def", " ", SplitBehavior::KeepToken),
            vec!["abc", "", "def"]
        );
        // 首尾分隔符 → 空 token
        assert_eq!(
            split(" a ", " ", SplitBehavior::KeepToken),
            vec!["", "a", ""]
        );
        // ⚠️ 空串 → 一条空串（`strings.cc:16` 的 do-while 等价物）
        assert_eq!(split("", " ", SplitBehavior::KeepToken), vec![""]);
        // 分隔符集合按字符取
        assert_eq!(
            split("a b'c", " '", SplitBehavior::KeepToken),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn split_skip_token() {
        assert_eq!(split("abc def", " ", SplitBehavior::SkipToken), vec!["abc", "def"]);
        assert_eq!(
            split("abc  def", " ", SplitBehavior::SkipToken),
            vec!["abc", "def"]
        );
        assert_eq!(split(" a ", " ", SplitBehavior::SkipToken), vec!["a"]);
        // ⚠️ 空串 / 全分隔符 → 空数组
        assert!(split("", " ", SplitBehavior::SkipToken).is_empty());
        assert!(split(",,,", ",", SplitBehavior::SkipToken).is_empty());
        // `RawCode::FromString` 就是走这条路径（`encoder.cc:22-25`）
        assert_eq!(
            split("  abc   def  ", " ", SplitBehavior::SkipToken),
            vec!["abc", "def"]
        );
    }

    #[test]
    fn join_pieces() {
        assert_eq!(join(["abc", "def"].iter(), " "), "abc def");
        assert_eq!(join(Vec::<String>::new().iter(), " "), "");
        assert_eq!(join(["a"].iter(), " "), "a");
    }
}
