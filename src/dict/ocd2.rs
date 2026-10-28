//! OpenCC 2.0 二进制词典(`.ocd2`)的纯 Rust 读取器。
//!
//! 格式(`opencc/src/MarisaDict.cpp` + `SerializedValues.cpp`,逐字段对齐):
//! ```text
//! [19B]  "OPENCC_MARISA_0.2.5"          魔数
//! [trie] marisa 0.2.5 blob              key(源字串)→ key_id,`io_size()` 字节
//! [值区] u32 numItems                   = 键数
//!        u32 valueTotalLength           拼接值串总长(含每值结尾 NUL)
//!        [valueTotalLength B]           值串池
//!        numItems × { u16 numValues,
//!                     numValues × { u16 numValueBytes, [numValueBytes B] } }
//! ```
//! 第 i 组值对应 marisa key_id = i(OpenCC 用 predictive_search 的 id 顺序落盘)。

use std::path::Path;

use crate::dict::marisa::Trie;

pub const OCD2_HEADER: &[u8] = b"OPENCC_MARISA_0.2.5";

pub struct Ocd2Dict {
    trie: Trie,
    /// `values[key_id]` = 该键的转换变体列表(OpenCC 的多值)。
    values: Vec<Vec<String>>,
}

impl Ocd2Dict {
    pub fn open(path: impl AsRef<Path>) -> Option<Self> {
        let data = std::fs::read(path).ok()?;
        Self::from_bytes(&data)
    }

    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < OCD2_HEADER.len() || &data[..OCD2_HEADER.len()] != OCD2_HEADER {
            return None;
        }
        let body = &data[OCD2_HEADER.len()..];
        let trie = Trie::from_bytes(body).ok()?;
        let trie_size = trie.io_size();
        let values_data = body.get(trie_size..)?;
        let values = parse_values(values_data)?;
        if values.len() != trie.len() {
            return None; // 键数不一致(OpenCC 同款校验)
        }
        Some(Self { trie, values })
    }

    pub fn len(&self) -> usize {
        self.trie.len()
    }

    pub fn is_empty(&self) -> bool {
        self.trie.is_empty()
    }

    /// 查一个源字串的全部转换变体。
    pub fn lookup(&self, key: &str) -> Option<&[String]> {
        let id = self.trie.lookup(key)?;
        self.values.get(id as usize).map(|v| v.as_slice())
    }
}

fn parse_values(data: &[u8]) -> Option<Vec<Vec<String>>> {
    let mut offset = 0usize;
    let read_u32 = |offset: &mut usize| -> Option<u32> {
        let b = data.get(*offset..*offset + 4)?;
        *offset += 4;
        Some(u32::from_le_bytes(b.try_into().unwrap()))
    };
    let read_u16 = |offset: &mut usize| -> Option<u16> {
        let b = data.get(*offset..*offset + 2)?;
        *offset += 2;
        Some(u16::from_le_bytes(b.try_into().unwrap()))
    };

    let num_items = read_u32(&mut offset)? as usize;
    let value_total_length = read_u32(&mut offset)? as usize;
    let value_pool = data.get(offset..offset + value_total_length)?;
    offset += value_total_length;

    let mut values: Vec<Vec<String>> = Vec::with_capacity(num_items);
    let mut pool_cursor = 0usize;
    for _ in 0..num_items {
        let num_values = read_u16(&mut offset)? as usize;
        let mut item: Vec<String> = Vec::with_capacity(num_values);
        for _ in 0..num_values {
            let num_value_bytes = read_u16(&mut offset)? as usize;
            if num_value_bytes == 0 || pool_cursor + num_value_bytes > value_pool.len() {
                return None;
            }
            let raw = &value_pool[pool_cursor..pool_cursor + num_value_bytes];
            pool_cursor += num_value_bytes;
            // 每个值以 NUL 结尾
            let end = raw.len().saturating_sub(1);
            item.push(String::from_utf8_lossy(&raw[..end]).into_owned());
        }
        values.push(item);
    }
    Some(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_real_t2s_characters_dict() {
        let path = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .join("Rime-Next");
        let shared = path.parent().map(|p| p.join("Rime-Next"));
        let dir = std::env::var_os("APPDATA")
            .map(|a| std::path::PathBuf::from(a).join("Rime-Next"))
            .unwrap();
        let _ = (&path, &shared, &dir);
        // 真机数据:优先程序目录,其次用户目录
        let candidates = [
            std::path::PathBuf::from(r"C:\Program Files\Weasel-Next\Rime-Next\opencc\TSCharacters.ocd2"),
            dir.join(crate::lever::paths::OPENCC_DIR).join("TSCharacters.ocd2"),
        ];
        let Some(path) = candidates.iter().find(|p| p.is_file()) else {
            eprintln!("跳过:没有真机 opencc 数据");
            return;
        };
        let dict = Ocd2Dict::open(path).expect("TSCharacters.ocd2 应能打开");
        assert!(dict.len() > 1000, "单字表应有大量键");
        // 「乾」的简体是「干」;「發」→「发」
        assert!(
            dict.lookup("乾").map_or(false, |v| v.iter().any(|s| s == "干")),
            "乾 应能转换出 干"
        );
        assert!(dict.lookup("不存在的鍵").is_none());
    }
}
