//! 词典击键统计：平均击键 / 最大击键。
//!
//! Rime 词典格式：frontmatter（YAML，`---` 包裹）声明 `columns`，之后是
//! 制表符分隔的数据行 `词条<TAB>编码[<TAB>权重…]`。
//! 这里的"击键"= 编码列（code/spelling）的**字符数**——每个字符对应一次按键：
//!   拼音 `nihao` → 5 键；仓颉 `手日` → 2 键；五笔 `gg` → 2 键。
//! 于是：
//!   - 平均击键 = 所有词条编码字符数之和 / 词条数
//!   - 最大击键 = 最长编码的字符数
//!
//! 这是输入法"键步效率"的静态近似（不依赖引擎运行，故可在 engine 被阻塞时独立跑）。

use std::path::Path;

pub struct KeystrokeStats {
    pub dict_id: String,
    pub path: String,
    pub entry_count: usize,
    pub total_keys: usize,
    pub avg_keys: f64,
    pub max_keys: usize,
    /// 最大击键对应的编码示例。
    pub max_keys_example: String,
}

/// 解析一个 `.dict.yaml`：返回 (columns, data_rows)。
/// frontmatter 用极简解析（足够覆盖 Rime 词典的 `columns: [...]`），
/// 不依赖外部 YAML 库，保证离线可跑；lrn 版应使用 `serde_norway`。
fn parse_dict(text: &str) -> Option<(Vec<String>, Vec<Vec<String>>)> {
    let lines: Vec<&str> = text.lines().collect();

    // 找 frontmatter 边界：第一个 `---` 到下一个 `---`。
    let mut start = None;
    let mut end = None;
    for (i, l) in lines.iter().enumerate() {
        if l.trim() == "---" {
            if start.is_none() {
                start = Some(i);
            } else {
                end = Some(i);
                break;
            }
        }
    }

    let mut columns = vec!["text".to_string(), "code".to_string()];
    if let (Some(s), Some(e)) = (start, end) {
        for l in &lines[s + 1..=e] {
            let t = l.trim();
            if let Some(rest) = t.strip_prefix("columns:") {
                let rest = rest.trim();
                if rest.starts_with('[') && rest.ends_with(']') {
                    let inner = &rest[1..rest.len() - 1];
                    let cols: Vec<String> = inner
                        .split(',')
                        .map(|x| {
                            x.trim()
                                .trim_matches('\'')
                                .trim_matches('"')
                                .to_string()
                        })
                        .filter(|x| !x.is_empty())
                        .collect();
                    if !cols.is_empty() {
                        columns = cols;
                    }
                }
            }
        }
    }

    // 数据行：紧跟在结束 `---` 之后；跳过空行与注释。
    let data_start = end.map(|e| e + 1).unwrap_or(0);
    let mut rows = Vec::new();
    for l in &lines[data_start..] {
        let line = l.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<String> = line.split('\t').map(|s| s.to_string()).collect();
        if parts.len() >= 2 {
            rows.push(parts);
        }
    }
    Some((columns, rows))
}

/// 计算单个词典的平均/最大击键。无法解析或 0 词条返回 `None`。
pub fn compute_keystrokes(dict_path: &Path, resource_id: &str) -> Option<KeystrokeStats> {
    let text = std::fs::read_to_string(dict_path).ok()?;
    let (columns, rows) = parse_dict(&text)?;

    // 编码列：优先名为 code / spelling / input，否则取第二列（默认 [text, code]）。
    let code_idx = columns
        .iter()
        .position(|c| matches!(c.as_str(), "code" | "spelling" | "input"))
        .unwrap_or(1);

    let mut total = 0usize;
    let mut count = 0usize;
    let mut max_keys = 0usize;
    let mut max_example = String::new();

    for row in &rows {
        let Some(code) = row.get(code_idx) else { continue };
        let code = code.trim();
        if code.is_empty() {
            continue;
        }
        // 一个词条可能带多个编码（空格分隔），这里取第一段作为主编码击键。
        let primary = code.split_whitespace().next().unwrap_or(code);
        let k = primary.chars().count();
        total += k;
        count += 1;
        if k > max_keys {
            max_keys = k;
            max_example = primary.to_string();
        }
    }

    if count == 0 {
        return None;
    }

    Some(KeystrokeStats {
        dict_id: resource_id.to_string(),
        path: dict_path.display().to_string(),
        entry_count: count,
        total_keys: total,
        avg_keys: total as f64 / count as f64,
        max_keys,
        max_keys_example: max_example,
    })
}
