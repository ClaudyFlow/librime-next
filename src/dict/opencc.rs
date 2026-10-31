//! OpenCC 引擎的**最小**移植 —— 仅覆盖 `simplifier` 需要的「简繁 / 地域变体」转换。
//!
//! 数据来自 `Rime-Next/opencc/`：
//!
//! * `*.ocd2` 是 OpenCC 的 **Marisa** 字典格式：19 字节定长 magic
//!   `OPENCC_MARISA_0.2.5`（**不带** null 结尾）+ 一段 marisa trie
//!   （自带 16 字节头 `"We love Marisa.\0"`，键 = 源字串）+ `SerializedValues`：
//!   `u32 numItems` → `u32 valueTotalLength` → `valueTotalLength` 字节值缓冲区
//!   （每个值 null 结尾）→ `numItems` 个条目描述符（`u16 numValues` + 每值
//!   `u16 numValueBytes` 含 null）。值字符串在缓冲区里按顺序存放。
//!   注意：rsmarisa 的 `io_size()` 与实际字节数有偏差，值区起点**不能**用
//!   `io_size()` 算，必须用计数 reader 精确定位 trie 末尾。
//! * `*.json` 是转换链配置（`conversion_chain`），链里可以是单个 `ocd2`，
//!   也可以是 `group`（多个 `ocd2` 顺序套用）。
//!
//! 复用 `rsmarisa`（纯 Rust）读 marisa trie，不引入任何外部 C++ 依赖。
//! 转换算法与 OpenCC 一致：**最长匹配**——从左到右扫描，每个位置取能匹配到的最长键。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rsmarisa::grimoire::io::reader::Reader;
use rsmarisa::{Agent, Trie};
use serde_json::Value as Json;

/// 一个 `.ocd2` 词典：trie 存键（源字串），`values` 按 trie id 存目标字串。
pub struct Ocd2 {
    trie: Trie,
    values: Vec<String>,
}

impl Ocd2 {
    /// 从文件加载一个 `.ocd2` 词典。
    pub fn load(path: impl AsRef<Path>) -> io::Result<Ocd2> {
        let blob = fs::read(path.as_ref())?;
        // 1) 跳过定长 magic（OpenCC 写的是 `OPENCC_MARISA_0.2.5` 这 19 字节，不带 null 结尾）
        const MAGIC: &[u8] = b"OPENCC_MARISA_0.2.5";
        if !blob.starts_with(MAGIC) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ocd2: 缺 magic 串（不是 OPENCC_MARISA 格式）",
            ));
        }
        let magic_end = MAGIC.len();
        // 2) 读 marisa trie —— 用**计数 reader** 精确跟踪它消耗的字节数
        //    （rsmarisa 的 `io_size()` 与实际写出字节有偏差，不能用来定位值区起点）
        let trie_blob = &blob[magic_end..];
        let (counting, counter) = CountingReader::new(std::io::Cursor::new(trie_blob));
        let mut reader = Reader::from_reader(counting);
        let mut trie = Trie::new();
        trie.read(&mut reader)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("ocd2 trie 读失败: {e}")))?;
        let trie_bytes = counter.load(std::sync::atomic::Ordering::SeqCst) as usize;
        if magic_end + trie_bytes > blob.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ocd2: trie 长度越界",
            ));
        }
        // 3) trie 之后是 SerializedValues 区：
        //    u32 numItems → u32 valueTotalLength → valueBuf(valueTotalLength 字节，每值 null 结尾)
        //    → numItems 个条目描述符：u16 numValues + 每值 u16 numValueBytes(含 null)
        let sv = &blob[magic_end + trie_bytes..];
        let mut pos = 0usize;
        let num_items = read_u32(sv, &mut pos)? as usize;
        let value_total_len = read_u32(sv, &mut pos)? as usize;
        if pos + value_total_len > sv.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ocd2: 值缓冲区越界",
            ));
        }
        let value_buf = &sv[pos..pos + value_total_len];
        pos += value_total_len;
        // 4) 读取 numItems 个条目的描述符，按顺序从 value_buf 取值
        //    OpenCC 每个键可能有多个值（variant 场景），值缓冲区按所有值顺序存放，
        //    所以 buf_pos 必须累加条目内**所有值**的长度，不能只累加第一个。
        let mut values = Vec::with_capacity(num_items);
        let mut buf_pos = 0usize;
        for _ in 0..num_items {
            let num_values = read_u16(sv, &mut pos)? as usize;
            // 读取本条目的所有值长度描述符（每值 u16，含 null）
            let mut val_lens = Vec::with_capacity(num_values);
            for _ in 0..num_values {
                val_lens.push(read_u16(sv, &mut pos)? as usize);
            }
            // 取第一个值（simplifier 只需要一个转换结果）
            if num_values > 0 {
                let val_len = val_lens[0];
                if val_len == 0 || buf_pos + val_len > value_buf.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "ocd2: 值偏移越界",
                    ));
                }
                // 最后一个字节是 null 结尾，去掉它
                let val_bytes = &value_buf[buf_pos..buf_pos + val_len - 1];
                values.push(String::from_utf8_lossy(val_bytes).into_owned());
            } else {
                values.push(String::new());
            }
            // 累加条目内所有值的长度（含 null），推进缓冲区指针
            buf_pos += val_lens.iter().sum::<usize>();
        }
        Ok(Ocd2 { trie, values })
    }

    /// 查一个键，返回对应的目标字串（未命中返回 `None`）。
    pub fn lookup(&self, key: &str) -> Option<&str> {
        let mut agent = Agent::new();
        agent.set_query_str(key);
        if self.trie.lookup(&mut agent) {
            let id = agent.key().id();
            self.values.get(id).map(|s| s.as_str())
        } else {
            None
        }
    }

    /// 最长匹配替换（OpenCC 的核心算法）。
    pub fn convert(&self, text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while i < chars.len() {
            let mut matched = false;
            // 从最长（剩余全部）到最短（1）逐一尝试
            for len in (1..=(chars.len() - i)).rev() {
                let key: String = chars[i..i + len].iter().collect();
                if let Some(val) = self.lookup(&key) {
                    out.push_str(val);
                    i += len;
                    matched = true;
                    break;
                }
            }
            if !matched {
                out.push(chars[i]);
                i += 1;
            }
        }
        out
    }
}

/// 从字节流读一个 `uint32`（小端）。
fn read_u32(blob: &[u8], pos: &mut usize) -> io::Result<u32> {
    if *pos + 4 > blob.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "ocd2: 读 u32 越界",
        ));
    }
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&blob[*pos..*pos + 4]);
    *pos += 4;
    Ok(u32::from_le_bytes(buf))
}

/// 从字节流读一个 `uint16`（小端）。
fn read_u16(blob: &[u8], pos: &mut usize) -> io::Result<u16> {
    if *pos + 2 > blob.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "ocd2: 读 u16 越界",
        ));
    }
    let mut buf = [0u8; 2];
    buf.copy_from_slice(&blob[*pos..*pos + 2]);
    *pos += 2;
    Ok(u16::from_le_bytes(buf))
}

/// 计数 reader：包装任意 `io::Read`，记录累计读取的字节数。
///
/// 用来精确定位 trie 结束后值区的起点（rsmarisa 的 `io_size()` 与实际
/// 写出字节有偏差，不能依赖它来切分 trie 与值区）。
/// 计数通过 `Rc<AtomicUsize>` 共享，`Trie::read` 之后仍可读取。
struct CountingReader<R> {
    inner: R,
    count: std::rc::Rc<std::sync::atomic::AtomicUsize>,
}

impl<R> CountingReader<R> {
    fn new(inner: R) -> (Self, std::rc::Rc<std::sync::atomic::AtomicUsize>) {
        let count = std::rc::Rc::new(std::sync::atomic::AtomicUsize::new(0));
        (Self { inner, count: count.clone() }, count)
    }
}

impl<R: io::Read> io::Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count.fetch_add(n, std::sync::atomic::Ordering::SeqCst);
        Ok(n)
    }
}

/// 一条转换链（`*.json` 的 `conversion_chain`，可含 `group`）。
#[derive(Clone)]
pub struct Opencc {
    dicts: Vec<std::sync::Arc<Ocd2>>,
}

impl Opencc {
    /// 从 `xxx.json` 配置加载整条链。
    pub fn load(config_path: impl AsRef<Path>) -> io::Result<Opencc> {
        let text = fs::read_to_string(config_path.as_ref())?;
        let json: Json = serde_json::from_str(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("opencc json 解析失败: {e}")))?;
        let dir = config_path
            .as_ref()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let mut dicts: Vec<std::sync::Arc<Ocd2>> = Vec::new();
        if let Some(chain) = json.get("conversion_chain").and_then(|v| v.as_array()) {
            for item in chain {
                if let Some(dict) = item.get("dict") {
                    collect_dicts(dict, &dir, &mut dicts)?;
                }
            }
        }
        Ok(Opencc { dicts })
    }

    /// 把整段文本按链依次转换（链里每个词典顺序套用）。
    pub fn convert(&self, text: &str) -> String {
        let mut s = text.to_string();
        for d in &self.dicts {
            s = d.convert(&s);
        }
        s
    }
}

/// 递归展开 `conversion_chain` 里的 `dict`（单个 `ocd2` 或 `group` 嵌套）。
fn collect_dicts(dict: &Json, dir: &Path, out: &mut Vec<std::sync::Arc<Ocd2>>) -> io::Result<()> {
    match dict.get("type").and_then(|v| v.as_str()) {
        Some("group") => {
            if let Some(dicts) = dict.get("dicts").and_then(|v| v.as_array()) {
                for sub in dicts {
                    // group 里的每个元素直接是 `{type, file}` 或嵌套的 group，
                    // 没有外层 "dict" 包装
                    collect_dicts(sub, dir, out)?;
                }
            }
        }
        Some("ocd2") => {
            if let Some(file) = dict.get("file").and_then(|v| v.as_str()) {
                out.push(std::sync::Arc::new(Ocd2::load(dir.join(file))?));
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 仓库里的真实 opencc 数据（相对 crate 根）。
    fn opencc_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("Rime-Next")
            .join(crate::lever::paths::OPENCC_DIR)
    }

    #[test]
    fn ocd2_lookup_s2t() {
        let ocd = Ocd2::load(opencc_dir().join("STCharacters.ocd2")).expect("读 STCharacters.ocd2");
        assert_eq!(ocd.lookup("汉"), Some("漢"));
        assert_eq!(ocd.lookup("语"), Some("語"));
        // OpenCC 不收录繁简同形字（"你" 繁体也是"你"），所以查不到
        assert!(ocd.lookup("你").is_none(), "繁简同形字不在 STCharacters 里");
        assert!(ocd.lookup("zzzzz").is_none(), "不存在的键应返回 None");
    }

    #[test]
    fn ocd2_lookup_t2s() {
        let ocd = Ocd2::load(opencc_dir().join("TSCharacters.ocd2")).expect("读 TSCharacters.ocd2");
        assert_eq!(ocd.lookup("漢"), Some("汉"));
        assert_eq!(ocd.lookup("語"), Some("语"));
    }

    #[test]
    fn s2t_chain_converts_phrases_and_chars() {
        let occ = Opencc::load(opencc_dir().join("s2t.json")).expect("加载 s2t.json");
        assert_eq!(occ.convert("汉"), "漢", "单字");
        assert_eq!(occ.convert("汉语"), "漢語", "词组优先走 STPhrases");
        assert_eq!(occ.convert("你"), "你", "同形字不变");
        assert_eq!(occ.convert("你好世界"), "你好世界", "全同形串不变");
    }

    #[test]
    fn t2s_chain_converts() {
        let occ = Opencc::load(opencc_dir().join("t2s.json")).expect("加载 t2s.json");
        assert_eq!(occ.convert("漢"), "汉");
        assert_eq!(occ.convert("漢語"), "汉语");
    }
}
