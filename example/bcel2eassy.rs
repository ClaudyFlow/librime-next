//! 百度细胞词库(.bdict/.bcel)→ eassy txt(词\t词频)转换器。
//!
//! 用法:`bcel2eassy <输入.bdict> <输出.yaml>`,输出直接丢进
//! `data\eassy\baidu\xxx.txt`——eassy_matcher 递归扫描所有 .txt,自动合并。
//!
//! 格式(照 imewlconverter 的已验证实现反向):
//! * 0x60 处 Int32LE = 词条区结束偏移;≤0 时旧版头部,0x44 处 Int32LE 长度
//!   +0x350 推算;
//! * 词条自 0x350 起:Int32LE 音节数(>1000 视为损坏,0 停止)
//!   + 音节数×2 字节(声母/韵母索引对,eassy 用不上但必须跳过)
//!   + 词文本 UTF-16LE(2×音节数字节);
//! * 无词频字段 → 统一给默认词频 1000。

use std::io::Write;

const DEFAULT_FREQ: f64 = 1000.0;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(output)) = (args.next(), args.next()) else {
        eprintln!("用法: bcel2eassy <输入.bdict/.bcel/.scel> <输出.yaml>");
        std::process::exit(2);
    };
    let data = match std::fs::read(&input) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("读取 {input} 失败: {e}");
            std::process::exit(1);
        }
    };
    let lower = input.to_ascii_lowercase();
    let words = if lower.ends_with(".scel") {
        parse_scel(&data).unwrap_or_else(|e| {
            eprintln!("解析失败: {e}");
            std::process::exit(1);
        })
    } else {
        parse_bdict(&data).unwrap_or_else(|e| {
            eprintln!("解析失败: {e}");
            std::process::exit(1);
        })
    };
    let mut out = match std::fs::File::create(&output) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("写 {output} 失败: {e}");
            std::process::exit(1);
        }
    };
    // 输出 = Rime 标准词库 YAML:UTF-8、LF、三行头。
    let slug = std::path::Path::new(&output)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("lexicon")
        .to_string();
    let _ = writeln!(out, "# Rime dictionary");
    let _ = writeln!(out, "---");
    let _ = writeln!(out, "id: {slug}");
    let _ = writeln!(out, "name: {slug}");
    let _ = writeln!(out, "version: \"1.0\"");
    let _ = writeln!(out, "sort: original");
    let _ = writeln!(out, "...");
    for word in &words {
        let _ = writeln!(out, "{word}\t{}", DEFAULT_FREQ as u64);
    }
    println!("{input}: {} 条词 → {output}", words.len());
}

fn i32le(data: &[u8], pos: usize) -> Option<i32> {
    let b = data.get(pos..pos + 4)?;
    Some(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn utf16le(data: &[u8], pos: usize, bytes: usize) -> Option<String> {
    let raw = data.get(pos..pos + bytes)?;
    let units: Vec<u16> = raw
        .chunks_exact(2)
        .map(|p| u16::from_le_bytes([p[0], p[1]]))
        .collect();
    Some(String::from_utf16_lossy(&units))
}

fn parse_bdict(data: &[u8]) -> Result<Vec<String>, String> {
    // 词条区结束偏移:新版头部 0x60;旧版该字段为 0,用 0x44 长度 + 0x350 推算。
    let mut end = i32le(data, 0x60).unwrap_or(0);
    if end <= 0 {
        end = 0x350 + i32le(data, 0x44).unwrap_or(0);
    }
    let end = end as usize;
    if end > data.len() {
        return Err(format!("结束偏移 {end} 超过文件大小 {}", data.len()));
    }
    let mut pos = 0x350;
    let mut words = Vec::new();
    while pos + 4 <= end {
        let Some(len) = i32le(data, pos) else { break };
        if !(0..=1000).contains(&len) {
            break; // 0=正常结束;>1000=损坏
        }
        if len == 0 {
            break;
        }
        pos += 4;
        pos += 2 * len as usize; // 拼音索引对,跳过
        let Some(word) = utf16le(data, pos, 2 * len as usize) else { break };
        pos += 2 * len as usize;
        let word = word.trim();
        if !word.is_empty() {
            words.push(word.to_string());
        }
    }
    Ok(words)
}

#[test]
fn parses_synthetic_bdict() {
    // 构造两个词条的迷你 bdict:词「你好」(2 音节)、「世界」。
    let mut d = vec![0u8; 0x400];
    d[0x350..0x354].copy_from_slice(&2i32.to_le_bytes());
    d[0x354..0x356].copy_from_slice(&[1u8, 2u8]); // 拼音索引对,内容随意
    d[0x356..0x358].copy_from_slice(&[3u8, 4u8]);
    let word: Vec<u8> = "你好".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    d[0x358..0x358 + word.len()].copy_from_slice(&word);
    let second_at = 0x358 + word.len();
    d[second_at..second_at + 4].copy_from_slice(&2i32.to_le_bytes());
    d[second_at + 4..second_at + 6].copy_from_slice(&[5u8, 6u8]);
    d[second_at + 6..second_at + 8].copy_from_slice(&[7u8, 8u8]);
    let word2: Vec<u8> = "世界".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    d[second_at + 8..second_at + 8 + word2.len()].copy_from_slice(&word2);
    let end = (second_at + 8 + word2.len()) as i32;
    d[0x60..0x64].copy_from_slice(&end.to_le_bytes());

    let words = parse_bdict(&d).unwrap();
    assert_eq!(words, vec!["你好", "世界"]);
}

/// 搜狗 .scel 细胞词库:0x120=词条组数,0x1540=拼音表长度,
/// 组 = 同音词数 u16 + 音节索引字节数 u16 + 索引字节 + N×(词字节 u16 + UTF16 词
/// + 2+4+6 字节跳过)。索引 < 拼音表长 → 查表,否则为字母(key-表长+97)。
fn parse_scel(data: &[u8]) -> Result<Vec<String>, String> {
    let rd_i32 = |pos: usize| -> Option<i32> { i32le(data, pos) };
    let rd_u16 = |pos: usize| -> Option<u16> {
        let b = data.get(pos..pos + 2)?;
        Some(u16::from_le_bytes([b[0], b[1]]))
    };
    let dict_len = rd_i32(0x120).unwrap_or(0).max(0) as usize;
    let py_dic_len = rd_i32(0x1540).unwrap_or(0).max(0) as usize;

    let mut pos = 0x1544;
    eprintln!("[dbg] dict_len={dict_len} py_dic_len={py_dic_len} 文件大小={}", data.len());
    let mut py_dic: Vec<String> = Vec::with_capacity(py_dic_len);
    for _ in 0..py_dic_len {
        pos += 2; // idx(顺序即下标,不读)
        let size = rd_u16(pos).ok_or("拼音表截断")? as usize;
        pos += 2;
        let Some(s) = utf16le(data, pos, size) else {
            return Err("拼音表截断".into());
        };
        pos += size;
        py_dic.push(s);
    }

    eprintln!("[dbg] 拼音表读完 pos={pos:#x} 表项={}", py_dic.len());
    let mut words = Vec::new();
    for _ in 0..dict_len {
        let Some(hdr) = data.get(pos..pos + 4) else { break };
        let same_py = u16::from_le_bytes([hdr[0], hdr[1]]) as usize;
        let syllables = u16::from_le_bytes([hdr[2], hdr[3]]) as usize;
        let idx_pos = pos + 4;
        let after_idx = idx_pos + syllables;
        if after_idx > data.len() {
            break;
        }
        let mut word_py = Vec::with_capacity(syllables / 2);
        for i in 0..syllables / 2 {
            let b0 = *data.get(idx_pos + i * 2).unwrap_or(&0) as usize;
            let b1 = *data.get(idx_pos + i * 2 + 1).unwrap_or(&0) as usize;
            let key = b0 + b1 * 256;
            word_py.push(if key < py_dic.len() {
                py_dic[key].clone()
            } else {
                char::from_u32((key - py_dic.len()) as u32 + 97)
                    .unwrap_or('?')
                    .to_string()
            });
        }
        pos = after_idx;
        for _ in 0..same_py {
            let Some(hz) = rd_u16(pos) else { break };
            let hz_bytes = hz as usize;
            pos += 2;
            let Some(word) = utf16le(data, pos, hz_bytes) else { break };
            pos += hz_bytes + 2 + 4 + 6; // 词 + unknown1(2) + unknown2(4) + 6
            let word = word.trim();
            if !word.is_empty() {
                words.push(word.to_string());
            }
        }
    }
    Ok(words)
}
