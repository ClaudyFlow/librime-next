//! `plugins/octagram/src/gram_db.{h,cc}`：语法模型数据库（`.gram`）。
//!
//! # 文件格式
//!
//! 与 librime 其它 `MappedFile` 一致，开头是一个固定的 metadata 结构体
//! （`gram_db.h:12-18`）：
//!
//! ```text
//! offset  size  字段
//!      0    32  char format[32]        "Rime::Grammar/1.0\0…"
//!     32     4  uint32_t db_checksum
//!     36     4  uint32_t double_array_size   （单位数，不是字节数）
//!     40     4  OffsetPtr<char> double_array （相对自身位置的偏移）
//!     44     …  darts 双数组镜像（double_array_size * 4 字节）
//! ```
//!
//! # 键的编码
//!
//! trie 里存的键是 **context + word 的拼接**（每条 n-gram 一个键），
//! 值是该 n-gram 的 `log(概率) * 10000`（`gram_db.cc:68`）。
//! 键的字节编码不是 UTF-8，而是 [`crate::engine::gear::grammar::encode`] 的紧凑形式。
//!
//! # 查询方式
//!
//! `Lookup(context, word)` 先在 trie 里走完 `context`，再从停下的节点继续搜
//! `word` 的所有前缀键 —— 相当于问"在 context 之后接上 word 的若干前缀，
//! 各自作为 n-gram 出现过吗、概率多少"（`gram_db.cc:105-116`）。

use std::io;
use std::path::{Path, PathBuf};

use crate::dict::darts::DoubleArray;
use crate::dict::mapped_file::MappedFile;

/// `kGrammarFormat` 的前缀（`gram_db.cc:11`）。只校验前缀，不校验具体版本号。
const K_FORMAT_PREFIX: &str = "Rime::Grammar/";

/// `GramDb::kMaxResults`（`gram_db.h:25`）：一次最多返回 8 条 n-gram 匹配。
pub const K_MAX_RESULTS: usize = 8;

/// `GramDb::kValueScale`（`gram_db.h:26`）：值 = `log(p) * 10000`。
pub const K_VALUE_SCALE: f64 = 10000.0;

/// metadata 里三个字段的偏移（见模块文档）。
const FORMAT_OFFSET: usize = 0;
const FORMAT_LENGTH: usize = 32;
const ARRAY_SIZE_OFFSET: usize = 36;
const ARRAY_PTR_OFFSET: usize = 40;

/// 一次 n-gram 匹配：`(值, 匹配到的字节长度)`。
///
/// 对应 C++ `Darts::DoubleArray::result_pair_type`（`value` / `length`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    /// `log(概率) * kValueScale`，可能为负。
    pub value: i32,
    /// 匹配了多少**字节**（编码后的字节，不是字符数）。
    pub length: usize,
}

/// 语法模型数据库（`class GramDb`，`gram_db.h:22-42`）。
pub struct GramDb {
    file: MappedFile,
    trie: DoubleArray,
    path: PathBuf,
}

impl GramDb {
    /// `GramDb::Load()`（`gram_db.cc:13-48`）。
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let file = MappedFile::open_read_only(&path)?;
        let format = file
            .fixed_str_at(FORMAT_OFFSET, FORMAT_LENGTH)
            .ok_or_else(|| invalid("metadata 不可读"))?;
        if !format.starts_with(K_FORMAT_PREFIX) {
            return Err(invalid(format!("metadata 格式不是 {K_FORMAT_PREFIX}*：{format:?}")));
        }
        let array_size = file
            .u32_at(ARRAY_SIZE_OFFSET)
            .ok_or_else(|| invalid("double_array_size 不可读"))?
            as usize;
        let array_pos = file
            .deref(ARRAY_PTR_OFFSET)
            .ok_or_else(|| invalid("double array image 偏移为 NULL"))?;
        let byte_len = array_size
            .checked_mul(4)
            .ok_or_else(|| invalid("double_array_size 溢出"))?;
        let bytes = file
            .bytes_at(array_pos, byte_len)
            .ok_or_else(|| invalid("double array image 越界"))?;
        let trie = DoubleArray::from_bytes(bytes, array_size)
            .ok_or_else(|| invalid("double array image 解析失败"))?;
        Ok(Self { file, trie, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn array_size(&self) -> usize {
        self.trie.size()
    }

    /// trie 里存了多少字节 —— 诊断用（日志里和 C++ 的 "found double array image of size N" 对齐）。
    pub fn image_bytes(&self) -> usize {
        self.trie.total_size()
    }

    /// 映射文件的字节数 —— 诊断用。
    pub fn file_len(&self) -> usize {
        self.file.used()
    }

    /// `GramDb::Lookup(context, word, results)`（`gram_db.cc:105-116`）。
    ///
    /// 返回 `word` 的各个**前缀**在 `context` 之后作为 n-gram 出现的匹配，
    /// 按长度升序（darts 的 `commonPrefixSearch` 顺序），最多 [`K_MAX_RESULTS`] 条。
    ///
    /// `context` 走不完时直接返回空 —— C++ 里 `traverse` 只走到哪算哪，
    /// 但随后 `key_pos == context.length()` 的检查会把它挡掉。
    ///
    /// # 参数是明文，不是 trie 键
    ///
    /// 这里收 `&str` 并在内部做 [`crate::engine::gear::grammar::encode`]。
    /// C++ 的 `Lookup` 收的是**已编码**的字符串（`octagram.cc` 里传的是 `context_query`），
    /// 那种用法请走 [`Self::lookup_bytes`]。
    pub fn lookup(&self, context: &str, word: &str) -> Vec<Match> {
        let ctx = crate::engine::gear::grammar::encode(context);
        let w = crate::engine::gear::grammar::encode(word);
        self.lookup_bytes(&ctx, &w)
    }

    /// [`Self::lookup`] 的字节版：调用方传的是**已编码**的 trie 键
    /// （语法模型内部就是这样用的，`octagram.cc:128`）。
    pub fn lookup_bytes(&self, context: &[u8], word: &[u8]) -> Vec<Match> {
        let mut node_pos = 0usize;
        let mut key_pos = 0usize;
        self.trie.traverse(context, &mut node_pos, &mut key_pos);
        if key_pos != context.len() {
            return Vec::new();
        }
        self.trie
            .common_prefix_search_from(word, node_pos, K_MAX_RESULTS)
            .into_iter()
            .map(|(value, length)| Match {
                value: value as i32,
                length,
            })
            .collect()
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 语法模型默认落在用户目录（`%APPDATA%/Rime-Next/model`），
    /// 没有就跳过——CI 上不该因为缺 200MB 模型而红。
    fn gram_path(language: &str) -> Option<PathBuf> {
        let appdata = std::env::var_os("APPDATA")?;
        let path = PathBuf::from(appdata)
            .join("Rime-Next")
            .join("model")
            .join(format!("{language}.gram"));
        path.is_file().then_some(path)
    }

    #[test]
    fn open_real_gram_file() {
        let Some(path) = gram_path("zh-hans-t-essay-bgc") else {
            return;
        };
        let db = GramDb::open(&path).expect("应能打开 .gram");
        assert!(db.array_size() > 1000, "双数组应有可观规模");
        // 值区间：log(p)*10000，p ∈ (0,1] → 值 <= 0
        assert!(db.image_bytes() <= db.file_len());
    }

    #[test]
    fn lookup_finds_known_bigram() {
        let Some(path) = gram_path("zh-hans-t-essay-bgc") else {
            return;
        };
        let db = GramDb::open(&path).expect("应能打开 .gram");
        // 「中国」「我们」「你好」都是语料里必然存在的双字搭配。
        for (ctx, word) in [("中", "国"), ("我", "们"), ("你", "好")] {
            let matches = db.lookup(ctx, word);
            assert!(!matches.is_empty(), "「{ctx}」+「{word}」应命中 n-gram");
            let encoded_len = crate::engine::gear::grammar::encode(word).len();
            assert_eq!(
                matches[0].length, encoded_len,
                "首个匹配应覆盖整个词「{word}」"
            );
            assert!(
                matches[0].value > 0,
                "值是频次的对数（`gram_db.cc:68` 的 `log(second) * 10000`），应为正"
            );
        }
        // 语料里没有的搭配查不到 —— 证明不是"什么键都返回"。
        assert!(
            db.lookup("世界", "你好").is_empty(),
            "「世界你好」不该在语料里"
        );
    }
}
