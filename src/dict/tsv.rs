//! `src/rime/dict/tsv.h`(73) + `tsv.cc`(89) 的 Rust 移植：TSV（制表符分隔）读写。
//!
//! # 读者拿到什么
//!
//! C++ 的 `TsvReader` 是 `operator()(Sink* sink)` + 一个 `TsvParser`
//! （`row → (key, value)`）。Rust 侧合成一个入口：
//!
//! ```text
//! TsvReader::new(path).read(&mut sink, |row| Some((key, value)))
//! ```
//!
//! 行处理规则（`tsv.cc:15-57`，逐条照抄）：
//!
//! 1. `getline` + `trim_right`（去尾部空白，CRLF 的 `\r` 靠这个去掉）；
//! 2. 空行跳过；
//! 3. `#` 开头且注释未禁用：
//!    * `#@key\tvalue` → 元数据（**必须**两列，否则记 invalid metadata）；
//!    * `# no comment` → 之后不再认注释（词条里可以有 `#`）；
//!    * 其余 `#` 行 → 注释，跳过；
//! 4. 其余行按 `\t` 切列（`KeepToken` 语义：空列保留），交给 parser 转
//!    `(key, value)`，再 `sink.put(key, value)`；任一失败 → 记 invalid entry 并跳过。
//!
//! 返回读到的词条数（与 C++ 的 `num_entries` 同义）。
//!
//! 写侧（`TsvWriter`）没有移植：词典编译链路只读不写。

use std::io;
use std::path::{Path, PathBuf};

use crate::algo::strings::{self, SplitBehavior};

/// `using Tsv = vector<string>`：一行切出来的列。
pub type Tsv = Vec<String>;

/// C++ 的 `Sink`：TSV 行的接收端。
pub trait TsvSink {
    /// `Sink::MetaPut`：`#@key\tvalue` 元数据。默认实现照 C++ 多数 Sink 的样子
    /// （返回 true 表示"收下了"）。
    fn meta_put(&mut self, _key: &str, _value: &str) -> bool {
        true
    }
    /// `Sink::Put`：一条词条。返回 false = 拒收（调用方记 invalid entry）。
    fn put(&mut self, key: &str, value: &str) -> bool;
}

/// `TsvReader`：读一个 TSV 文件喂给 `TsvSink`。
pub struct TsvReader {
    file_path: PathBuf,
}

impl TsvReader {
    pub fn new(file_path: impl Into<PathBuf>) -> Self {
        Self {
            file_path: file_path.into(),
        }
    }

    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    /// `TsvReader::operator()(Sink*)` + parser 参数。
    ///
    /// `parser` 对应 C++ 的 `TsvParser`：把一行（已切列）转成 `(key, value)`；
    /// 返回 `None` 表示这一行不是合法词条。
    pub fn read(
        &self,
        sink: &mut dyn TsvSink,
        parser: impl Fn(&Tsv) -> Option<(String, String)>,
    ) -> io::Result<usize> {
        let text = std::fs::read_to_string(&self.file_path)?;
        Ok(Self::read_text(
            &text,
            &self.file_path.to_string_lossy(),
            sink,
            parser,
        ))
    }

    /// 读**文本**（测试 / 已在外层读过文件的调用方用）。
    /// 规则与 [`Self::read`] 完全相同，只是不做文件 IO；`source` 只进警告信息。
    pub fn read_text(
        text: &str,
        source: &str,
        sink: &mut dyn TsvSink,
        parser: impl Fn(&Tsv) -> Option<(String, String)>,
    ) -> usize {
        let mut num_entries = 0usize;
        let mut enable_comment = true;
        for line in text.lines() {
            // boost::algorithm::trim_right：去尾部空白（C locale 的 6 个空白字符）
            let line = line.trim_end_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
            if line.is_empty() {
                continue;
            }
            if enable_comment && line.starts_with('#') {
                if let Some(meta) = line.strip_prefix("#@") {
                    // 元数据：#@<key>\t<value>，必须两列
                    let row = strings::split(meta, "\t", SplitBehavior::KeepToken);
                    if row.len() != 2 || !sink.meta_put(&row[0], &row[1]) {
                        log_warn(&format!("invalid metadata in file: {source}."));
                    }
                } else if line == "# no comment" {
                    enable_comment = false;
                }
                continue;
            }
            let row = strings::split(line, "\t", SplitBehavior::KeepToken);
            match parser(&row) {
                Some((key, value)) if sink.put(&key, &value) => num_entries += 1,
                _ => log_warn(&format!("invalid entry in file: {source}.")),
            }
        }
        num_entries
    }
}

/// dict 层自己的日志出口（与 `lever::deployment_tasks` 的同名函数保持一致风格）。
fn log_warn(message: &str) {
    eprintln!("[tsv] {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 记录 put/meta_put 的桩。
    #[derive(Default)]
    struct Recorder {
        entries: Vec<(String, String)>,
        meta: Vec<(String, String)>,
        reject_next: bool,
    }

    impl TsvSink for Recorder {
        fn meta_put(&mut self, key: &str, value: &str) -> bool {
            self.meta.push((key.to_string(), value.to_string()));
            true
        }
        fn put(&mut self, key: &str, value: &str) -> bool {
            if self.reject_next {
                self.reject_next = false;
                return false;
            }
            self.entries.push((key.to_string(), value.to_string()));
            true
        }
    }

    /// preset_vocabulary.cc:29-38 的 parser：row[0] = key，row[1] 或 "0" = value。
    fn vocab_parser(row: &Tsv) -> Option<(String, String)> {
        if row.is_empty() || row[0].is_empty() {
            return None;
        }
        let value = if row.len() > 1 { row[1].clone() } else { "0".to_string() };
        Some((row[0].clone(), value))
    }

    #[test]
    fn reads_entries_skips_comments_and_empty_lines() {
        let text = "# a comment\n\
                    #@name\tvalue\n\
                    \n\
                    你好\t100\n\
                    世界\t200\n";
        let mut sink = Recorder::default();
        let n = TsvReader::read_text(text, "test", &mut sink, vocab_parser);
        assert_eq!(n, 2);
        assert_eq!(
            sink.entries,
            vec![
                ("你好".to_string(), "100".to_string()),
                ("世界".to_string(), "200".to_string())
            ]
        );
        assert_eq!(sink.meta, vec![("name".to_string(), "value".to_string())]);
    }

    #[test]
    fn no_comment_line_disables_further_comments() {
        // "# no comment" 之后，连着的 "# 不是注释了" 也要当词条读
        let text = "# no comment\n#字\t1\n";
        let mut sink = Recorder::default();
        let n = TsvReader::read_text(text, "test", &mut sink, vocab_parser);
        assert_eq!(n, 1);
        assert_eq!(sink.entries, vec![("#字".to_string(), "1".to_string())]);
    }

    #[test]
    fn invalid_metadata_needs_exactly_two_columns() {
        let text = "#@only-one\n#@k\tv\tx\n";
        let mut sink = Recorder::default();
        TsvReader::read_text(text, "test", &mut sink, vocab_parser);
        assert!(sink.meta.is_empty(), "一列或三列都不是合法元数据");
    }

    #[test]
    fn rejected_entries_are_not_counted() {
        let text = "a\t1\nb\t2\n";
        let mut sink = Recorder {
            reject_next: true,
            ..Default::default()
        };
        let n = TsvReader::read_text(text, "test", &mut sink, vocab_parser);
        assert_eq!(n, 1, "被 sink 拒收的那条不计入");
        assert_eq!(sink.entries, vec![("b".to_string(), "2".to_string())]);
    }

    #[test]
    fn missing_value_column_defaults_to_zero() {
        let text = "单列\n";
        let mut sink = Recorder::default();
        let n = TsvReader::read_text(text, "test", &mut sink, vocab_parser);
        assert_eq!(n, 1);
        assert_eq!(sink.entries, vec![("单列".to_string(), "0".to_string())]);
    }

    #[test]
    fn trailing_cr_is_trimmed() {
        // CRLF 文件：getline 留下 '\r'，trim_right 负责去掉
        let text = "词\t1\r\n";
        let mut sink = Recorder::default();
        let n = TsvReader::read_text(text, "test", &mut sink, vocab_parser);
        assert_eq!(n, 1);
        assert_eq!(sink.entries, vec![("词".to_string(), "1".to_string())]);
    }
}
