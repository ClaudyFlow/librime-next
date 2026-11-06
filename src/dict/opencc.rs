//! OpenCC 引擎的**最小**移植 —— 覆盖 `simplifier` 需要的「简繁 / 地域变体 /
//! 自定义文本词典」转换。
//!
//! # 两种词典后端（`opencc/*.json` 里 `dict.type`）
//!
//! * `ocd2` —— OpenCC 的 **Marisa** 二进制格式：19 字节定长 magic
//!   `OPENCC_MARISA_0.2.5`（**不带** null 结尾）+ 一段 marisa trie（自带 16 字节
//!   头 `"We love Marisa.\0"`，键 = 源字串）+ `SerializedValues`：
//!   `u32 numItems` → `u32 valueTotalLength` → `valueTotalLength` 字节值缓冲区
//!   （每个值 null 结尾）→ `numItems` 个条目描述符（`u16 numValues` + 每值
//!   `u16 numValueBytes` 含 null）。值字符串在缓冲区里按顺序存放。
//!   注意：rsmarisa 的 `io_size()` 与实际字节数有偏差，值区起点**不能**用
//!   `io_size()` 算，必须用计数 reader 精确定位 trie 末尾。
//! * `text` —— OpenCC 的**纯文本**词典（`OpenCC/src/TextDict.cpp` 的
//!   `NewFromFile`）：每行 `键<TAB>值`，值可以含空格，**多值靠重复键**。
//!   社区方案（巧码等）的自定义 opencc 全是这个后端 ——
//!   `qiao_cf.json` / `emoji.json` / `pinyin.json` 等配置无一例外。
//!   早期实现只认 `ocd2`，这些 json 的 `conversion_chain` 被整条跳过，
//!   于是「opencc 配置读到了但一个字都不转」。
//!
//! `*.json` 是转换链配置（`conversion_chain`），链里可以是单个词典，也可以是
//! `group`（多个词典顺序套用）。
//!
//! 复用 `rsmarisa`（纯 Rust）读 marisa trie，不引入任何外部 C++ 依赖。
//! 转换算法与 OpenCC 一致：**最长匹配**——从左到右扫描，每个位置取能匹配到的最长键。
//!
//! # 三个入口，对应 C++ `gear/opencc.cc` 的三个方法
//!
//! | 方法 | C++ 对应 | 语义 |
//! |---|---|---|
//! | [`Opencc::convert_text`] | `ConvertText` | 全链最长匹配，每级取**默认值**（第一个） |
//! | [`Opencc::convert_word`] | `ConvertWord` | 精确匹配取**全部变体**；任一级精确命中才算成功 |
//! | [`Opencc::convert`] | — | `convert_text` 的 `String` 便捷版（返回值而非 Option） |
//!
//! `ConvertWord` 是 `simplifier` 的主路径：s2t 里「里」有两个变体
//! （里 / 裏），C++ 会把**两条候选都产出来**，所以候选数可能比原翻译多。

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rsmarisa::grimoire::io::reader::Reader;
use rsmarisa::{Agent, Trie};
use serde_json::Value as Json;

/// 一个 `.ocd2` 词典：trie 存键（源字串），`values` 按 trie id 存目标字串**列表**
/// （OpenCC 允许一个键多个转换变体）。
pub struct Ocd2 {
    trie: Trie,
    values: Vec<Vec<String>>,
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
        trie.read(&mut reader).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("ocd2 trie 读失败: {e}"),
            )
        })?;
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
        //    所以 buf_pos 必须累加条目内**所有值**的长度。
        let mut values = Vec::with_capacity(num_items);
        let mut buf_pos = 0usize;
        for _ in 0..num_items {
            let num_values = read_u16(sv, &mut pos)? as usize;
            // 读取本条目的所有值长度描述符（每值 u16，含 null）
            let mut val_lens = Vec::with_capacity(num_values);
            for _ in 0..num_values {
                val_lens.push(read_u16(sv, &mut pos)? as usize);
            }
            let mut entry_values = Vec::with_capacity(num_values);
            for &val_len in &val_lens {
                if val_len == 0 || buf_pos + val_len > value_buf.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "ocd2: 值偏移越界",
                    ));
                }
                // 最后一个字节是 null 结尾，去掉它
                let val_bytes = &value_buf[buf_pos..buf_pos + val_len - 1];
                entry_values.push(String::from_utf8_lossy(val_bytes).into_owned());
                buf_pos += val_len;
            }
            values.push(entry_values);
        }
        Ok(Ocd2 { trie, values })
    }

    /// 查一个键的**全部**转换变体（未命中返回 `None`）。
    pub fn lookup(&self, key: &str) -> Option<&[String]> {
        let mut agent = Agent::new();
        agent.set_query_str(key);
        if self.trie.lookup(&mut agent) {
            let id = agent.key().id();
            self.values.get(id).map(|v| v.as_slice())
        } else {
            None
        }
    }

    /// 查一个键的默认（第一个）转换值。
    pub fn lookup_default(&self, key: &str) -> Option<&str> {
        self.lookup(key)
            .and_then(|values| values.first())
            .map(|s| s.as_str())
    }

    /// 最长前缀匹配：返回（键的**字节**长度，默认转换值）。
    pub fn longest_prefix<'a>(&'a self, text: &str) -> Option<(usize, &'a str)> {
        longest_prefix_by(text, |key| self.lookup_default(key))
    }

    /// 最长匹配替换（OpenCC `ConversionChain::Convert` 的单级语义）。
    pub fn convert(&self, text: &str) -> String {
        convert_longest(text, |key| self.lookup_default(key))
    }
}

/// 一个 `text` 后端词典（OpenCC `TextDict`）：每行 `键<TAB>值`，重复键 = 多值。
pub struct TextDict {
    map: HashMap<String, Vec<String>>,
}

impl TextDict {
    /// 从文件加载（UTF-8，`#` 开头的行当注释）。
    pub fn load(path: impl AsRef<Path>) -> io::Result<TextDict> {
        let text = fs::read_to_string(path.as_ref())?;
        Ok(Self::parse(&text))
    }

    /// 解析文本内容（不落盘，测试/内嵌数据用）。
    pub fn parse(text: &str) -> TextDict {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('\t') else {
                continue;
            };
            if key.is_empty() {
                continue;
            }
            map.entry(key.to_string()).or_default().push(value.to_string());
        }
        TextDict { map }
    }

    /// 条目数（测试/诊断用）。
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 精确匹配的全部变体。
    pub fn lookup(&self, key: &str) -> Option<&[String]> {
        self.map.get(key).map(|v| v.as_slice())
    }

    /// 最长前缀匹配：返回（键的**字节**长度，默认转换值）。
    pub fn longest_prefix<'a>(&'a self, text: &str) -> Option<(usize, &'a str)> {
        longest_prefix_by(text, |key| {
            self.map.get(key).and_then(|v| v.first()).map(|s| s.as_str())
        })
    }

    /// 最长匹配替换。
    pub fn convert(&self, text: &str) -> String {
        convert_longest(text, |key| {
            self.map.get(key).and_then(|v| v.first()).map(|s| s.as_str())
        })
    }
}

/// 两种后端统一的词典句柄。
pub enum Dict {
    Ocd2(Ocd2),
    Text(TextDict),
}

impl Dict {
    /// 精确匹配的全部变体（`Dict::Match`）。
    pub fn exact(&self, key: &str) -> Option<&[String]> {
        match self {
            Dict::Ocd2(d) => d.lookup(key),
            Dict::Text(d) => d.lookup(key),
        }
    }

    /// 最长前缀匹配（`Dict::MatchPrefix`）。
    pub fn longest_prefix<'a>(&'a self, text: &str) -> Option<(usize, &'a str)> {
        match self {
            Dict::Ocd2(d) => d.longest_prefix(text),
            Dict::Text(d) => d.longest_prefix(text),
        }
    }

    /// 最长匹配替换（单级）。
    pub fn convert(&self, text: &str) -> String {
        match self {
            Dict::Ocd2(d) => d.convert(text),
            Dict::Text(d) => d.convert(text),
        }
    }
}

/// 沿着字符边界从最长到最短试前缀，命中就返回（键字节长度, 值）。
fn longest_prefix_by<'a, F>(text: &str, mut lookup: F) -> Option<(usize, &'a str)>
where
    F: FnMut(&str) -> Option<&'a str>,
{
    // 从最长前缀往回退：先试整个串，再逐字缩短
    let mut ends: Vec<usize> = text.char_indices().map(|(i, c)| i + c.len_utf8()).collect();
    while let Some(end) = ends.pop() {
        if let Some(value) = lookup(&text[..end]) {
            return Some((end, value));
        }
    }
    None
}

/// 从左到右最长匹配替换整段文本。
fn convert_longest<'a, F>(text: &str, mut lookup: F) -> String
where
    F: FnMut(&str) -> Option<&'a str>,
{
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        match longest_prefix_by(rest, &mut lookup) {
            Some((len, value)) => {
                out.push_str(value);
                rest = &rest[len..];
            }
            None => {
                // 没命中：原样搬一个字符
                let ch = rest.chars().next().expect("rest 非空");
                out.push(ch);
                rest = &rest[ch.len_utf8()..];
            }
        }
    }
    out
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
        (
            Self {
                inner,
                count: count.clone(),
            },
            count,
        )
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
    chain: std::sync::Arc<Vec<Dict>>,
    /// 配置文件名（如 `s2t.json`，诊断日志用）。
    config_name: String,
}

impl Opencc {
    /// 从 `xxx.json` 配置加载整条链。
    pub fn load(config_path: impl AsRef<Path>) -> io::Result<Opencc> {
        let path = config_path.as_ref();
        let text = fs::read_to_string(path)?;
        let json: Json = serde_json::from_str(&text).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("opencc json 解析失败: {e}"),
            )
        })?;
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let mut chain: Vec<Dict> = Vec::new();
        if let Some(items) = json.get("conversion_chain").and_then(|v| v.as_array()) {
            for item in items {
                if let Some(dict) = item.get("dict") {
                    collect_dicts(dict, &dir, &mut chain)?;
                }
            }
        }
        if chain.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "opencc 配置 {} 的 conversion_chain 里没有可用词典（支持 type: ocd2 / text）",
                    path.display()
                ),
            ));
        }
        Ok(Opencc {
            chain: std::sync::Arc::new(chain),
            config_name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        })
    }

    /// 配置文件名（如 `s2t.json`）。
    pub fn config_name(&self) -> &str {
        &self.config_name
    }

    /// 链里词典个数（诊断用）。
    pub fn dict_count(&self) -> usize {
        self.chain.len()
    }

    /// `ConvertText`：全链最长匹配（每级取默认值）。
    pub fn convert_text(&self, text: &str) -> Option<String> {
        if self.chain.is_empty() {
            return None;
        }
        let mut current = text.to_string();
        for dict in self.chain.iter() {
            current = dict.convert(&current);
        }
        Some(current)
    }

    /// `ConvertWord`：精确匹配取**全部**变体，逐级套用。
    ///
    /// 返回 `None` = 整条链没有任何一级精确命中（C++ `matched == false`）。
    /// 注意：即使某一级没精确命中，该级的**最长匹配**结果仍会传给下一级
    /// （`s2t` 把「里」展开成 里/裏，`t2tw` 再把 裏 转成 裡 —— 链式语义不能断）。
    pub fn convert_word(&self, text: &str) -> Option<Vec<String>> {
        let mut words = vec![text.to_string()];
        let mut matched = false;
        for dict in self.chain.iter() {
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut converted: Vec<String> = Vec::new();
            for word in &words {
                match dict.exact(word) {
                    Some(values) => {
                        matched = true;
                        for value in values {
                            if seen.insert(value.clone()) {
                                converted.push(value.clone());
                            }
                        }
                    }
                    None => {
                        // 没精确命中：仍要走最长匹配转换，保住链式语义
                        let partial = dict.convert(word);
                        if seen.insert(partial.clone()) {
                            converted.push(partial);
                        }
                    }
                }
            }
            words = converted;
        }
        if matched && !words.is_empty() {
            Some(words)
        } else {
            None
        }
    }

    /// 便捷版：`convert_text` 的结果，失败时原样返回。
    pub fn convert(&self, text: &str) -> String {
        self.convert_text(text).unwrap_or_else(|| text.to_string())
    }
}

/// 递归展开 `conversion_chain` 里的 `dict`（单个词典或 `group` 嵌套）。
///
/// 支持的后端：`ocd2`（二进制）与 `text`（纯文本）。`type` 缺失时按扩展名猜
/// （社区里手写 json 常常漏掉 `type`）。
fn collect_dicts(dict: &Json, dir: &Path, out: &mut Vec<Dict>) -> io::Result<()> {
    let kind = dict.get("type").and_then(|v| v.as_str());
    let file = dict.get("file").and_then(|v| v.as_str());
    match kind {
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
            if let Some(file) = file {
                out.push(Dict::Ocd2(Ocd2::load(dir.join(file))?));
            }
        }
        Some("text") => {
            if let Some(file) = file {
                out.push(Dict::Text(TextDict::load(dir.join(file))?));
            }
        }
        Some(other) => {
            eprintln!("[opencc] 跳过未知词典类型 `{other}`（{file:?}）");
        }
        None => {
            if let Some(file) = file {
                let path = dir.join(file);
                if file.ends_with(".ocd2") {
                    out.push(Dict::Ocd2(Ocd2::load(path)?));
                } else if file.ends_with(".txt") {
                    out.push(Dict::Text(TextDict::load(path)?));
                } else {
                    eprintln!("[opencc] 跳过无法判定类型的词典 `{file}`");
                }
            }
        }
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

    /// 社区方案（巧码）自带的那份 opencc 目录（仓库外的暂存副本，可能不存在）。
    fn community_opencc_dir() -> Option<PathBuf> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(".tmp_qiaoma")
            .join("shared")
            .join(crate::lever::paths::OPENCC_DIR);
        dir.is_dir().then_some(dir)
    }

    #[test]
    fn ocd2_lookup_s2t() {
        let ocd = Ocd2::load(opencc_dir().join("STCharacters.ocd2")).expect("读 STCharacters.ocd2");
        assert_eq!(ocd.lookup_default("汉"), Some("漢"));
        assert_eq!(ocd.lookup_default("语"), Some("語"));
        // OpenCC 不收录繁简同形字（"你" 繁体也是"你"），所以查不到
        assert!(ocd.lookup("你").is_none(), "繁简同形字不在 STCharacters 里");
        assert!(ocd.lookup("zzzzz").is_none(), "不存在的键应返回 None");
    }

    #[test]
    fn ocd2_lookup_t2s() {
        let ocd = Ocd2::load(opencc_dir().join("TSCharacters.ocd2")).expect("读 TSCharacters.ocd2");
        assert_eq!(ocd.lookup_default("漢"), Some("汉"));
        assert_eq!(ocd.lookup_default("語"), Some("语"));
    }

    #[test]
    fn s2t_chain_converts_phrases_and_chars() {
        let occ = Opencc::load(opencc_dir().join("s2t.json")).expect("加载 s2t.json");
        assert_eq!(occ.convert("汉"), "漢", "单字");
        assert_eq!(occ.convert("汉语"), "漢語", "词组优先走 STPhrases");
        assert_eq!(occ.convert("你"), "你", "同形字不变");
        assert_eq!(occ.convert("你好世界"), "你好世界", "全同形串不变");
        assert_eq!(occ.dict_count(), 2, "s2t = STPhrases + STCharacters");
    }

    #[test]
    fn t2s_chain_converts() {
        let occ = Opencc::load(opencc_dir().join("t2s.json")).expect("加载 t2s.json");
        assert_eq!(occ.convert("漢"), "汉");
        assert_eq!(occ.convert("漢語"), "汉语");
    }

    #[test]
    fn convert_word_returns_all_variants() {
        let occ = Opencc::load(opencc_dir().join("s2t.json")).expect("加载 s2t.json");
        //「里」在 STCharacters 里有 里/裏 两个变体 —— C++ 会产出两条候选
        let forms = occ.convert_word("里").expect("精确命中");
        assert!(forms.len() >= 2, "应有多变体: {forms:?}");
        assert!(forms.iter().any(|f| f == "裏"), "{forms:?}");
        // 整句没精确命中 ⇒ None
        assert!(occ.convert_word("汉语言文学").is_none());
    }

    #[test]
    fn text_dict_parses_and_converts() {
        let dict = TextDict::parse("汉\t漢\n里\t里\n里\t裏\n# 注释\n坏行没有制表符\n");
        assert_eq!(dict.len(), 2, "重复键算一个条目");
        assert_eq!(dict.lookup("汉"), Some(["漢".to_string()].as_slice()));
        assert_eq!(dict.lookup("里").map(|v| v.len()), Some(2), "重复键 = 多值");
        assert_eq!(dict.convert("汉语"), "漢语", "未收录的字原样保留");
    }

    /// 巧码那批 `type: text` 配置：修之前 `conversion_chain` 被整条跳过，
    /// 现在必须真的能读出词典并转换。
    #[test]
    fn community_text_backend_configs_load() {
        let Some(dir) = community_opencc_dir() else {
            eprintln!("跳过:没有社区方案副本 (.tmp_qiaoma)");
            return;
        };
        for name in [
            "qiao_cf.json",
            "emoji.json",
            "pinyin.json",
            "jmdz.json",
            "hanzi_dic.json",
            "bqj.json",
            "yitizi.json",
            "dic_4w_en.json",
            "jmzj.json",
            "pinyin_word.json",
        ] {
            let occ =
                Opencc::load(dir.join(name)).unwrap_or_else(|e| panic!("{name} 加载失败: {e}"));
            assert!(occ.dict_count() >= 1, "{name} 链为空");
        }
        // 真实内容抽查：部件拆分 / 单字拼音
        let cf = Opencc::load(dir.join("qiao_cf.json")).expect("qiao_cf.json");
        let out = cf.convert("⺏");
        assert!(out.contains('八'), "qiao_cf 应把 ⺏ 拆成部件: {out}");
        let py = Opencc::load(dir.join("pinyin.json")).expect("pinyin.json");
        assert_eq!(py.convert("㐀"), "qiū");
        // 多级链（pinyin_word = PYPhrases + PYCharacters）
        let pw = Opencc::load(dir.join("pinyin_word.json")).expect("pinyin_word.json");
        assert_eq!(pw.dict_count(), 2, "group 里两个词典都要进链");
    }
}
