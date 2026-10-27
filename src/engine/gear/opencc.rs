//! OpenCC 配置(`t2s.json`)与转换链的纯 Rust 实现。
//!
//! C++ 走 `libopencc`;lrn 直接读 `.ocd2`(marisa 词典,见
//! [`crate::dict::ocd2`])+ 手写转换,不再链接 C++ 运行时。
//!
//! 配置结构(`opencc/t2s.json`):
//! ```json
//! {"segmentation": {"type": "mmseg", "dict": {"type": "ocd2", "file": "TSPhrases.ocd2"}},
//!  "conversion_chain": [{"dict": {"type": "ocd2", "file": "TSPhrases.ocd2"}},
//!                       {"dict": {"type": "ocd2", "file": "TSCharacters.ocd2"}}]}
//! ```
//! * 分词:对 segmentation 词典做**最长匹配**(OpenCC 的 mmseg;无命中的
//!   字符自成一段);
//! * 逐段经链转换:链上第一个含该段的词典胜,取其**第一个**变体
//!   (OpenCC `Conversion::Convert` 的单值语义;多变体是 `RandomConvertText`
//!   的事,librime 的 `random: true` 才用)。

use std::path::{Path, PathBuf};

use crate::dict::ocd2::Ocd2Dict;

pub struct Opencc {
    segmentation: Ocd2Dict,
    chain: Vec<Ocd2Dict>,
}

/// 在数据根里定位 `opencc/<名>`:用户目录优先、共享目录其次
/// (C++ `SimplifierComponent::Create` 的查找序)。
fn resolve_data_file(name: &str, data_roots: &[PathBuf]) -> Option<PathBuf> {
    for root in data_roots {
        let candidate = root.join("opencc").join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 从配置 JSON 收集一个 dict 节点(`ocd2` 文件 / `group` 递归)指向的词典。
fn collect_dicts(
    node: &serde_json::Value,
    data_roots: &[PathBuf],
    out: &mut Vec<Ocd2Dict>,
) -> Option<()> {
    match node.get("type").and_then(|v| v.as_str())? {
        "ocd2" => {
            let file = node.get("file").and_then(|v| v.as_str())?;
            let path = resolve_data_file(file, data_roots)?;
            out.push(Ocd2Dict::open(path)?);
            Some(())
        }
        "group" => {
            for dict in node.get("dicts")?.as_array()? {
                collect_dicts(dict, data_roots, out)?;
            }
            Some(())
        }
        _ => None,
    }
}

impl Opencc {
    /// 打开一份 OpenCC 配置。`config` 是相对名(`t2s.json`)或绝对路径。
    pub fn open(config: &str, data_roots: &[PathBuf]) -> Option<Self> {
        let config_path = if Path::new(config).is_absolute() {
            PathBuf::from(config)
        } else {
            resolve_data_file(config, data_roots)?
        };
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).ok()?).ok()?;

        let seg_node = value.pointer("/segmentation/dict")?;
        let mut segmentation = Vec::new();
        collect_dicts(seg_node, data_roots, &mut segmentation)?;
        let segmentation = segmentation.into_iter().next()?;

        let mut chain = Vec::new();
        for link in value.get("conversion_chain")?.as_array()? {
            collect_dicts(link.get("dict")?, data_roots, &mut chain)?;
        }
        Some(Self { segmentation, chain })
    }

    /// 转换一段文本。没有任何变化时返回 `None`。
    pub fn convert(&self, text: &str) -> Option<String> {
        let mut out = String::with_capacity(text.len());
        let mut changed = false;
        let bytes = text.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            // mmseg:当前位置起的最长命中(逐字节退;UTF-8 多字节按字节试,
            // 词典键本身是合法 UTF-8,半截键查不到自然落空)
            let mut matched = None;
            for end in (i + 1..=bytes.len()).rev() {
                if !text.is_char_boundary(end) {
                    continue;
                }
                let unit = &text[i..end];
                if self.segmentation.lookup(unit).is_some() {
                    matched = Some(end);
                    break;
                }
            }
            let end = match matched {
                Some(end) => end,
                // 无命中的字符自成一段
                None => {
                    let mut e = i + 1;
                    while e < bytes.len() && !text.is_char_boundary(e) {
                        e += 1;
                    }
                    e
                }
            };
            let unit = &text[i..end];
            out.push_str(&self.convert_unit(unit, &mut changed));
            i = end;
        }
        changed.then_some(out)
    }

    /// 一段经链转换:第一个含它的词典胜,取第一个变体。
    fn convert_unit(&self, unit: &str, changed: &mut bool) -> String {
        for dict in &self.chain {
            if let Some(values) = dict.lookup(unit)
                && let Some(first) = values.first()
            {
                if first != unit {
                    *changed = true;
                }
                return first.clone();
            }
        }
        unit.to_string()
    }
}
