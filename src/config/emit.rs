//! 树 → YAML 的**唯一出口**（`config_data.cc:292-335` 的 `EmitYaml`）。
//!
//! 全 crate 只有这一个模块发射**产物** YAML：
//!
//! | 调用方 | 用途 |
//! |---|---|
//! | [`super::data::ConfigData::save_to_file`] | `ConfigData::Save` 写回文件 |
//! | `main.rs compile` | 把编译产物写成 `build/<方案>.schema.yaml` |
//! | `main.rs emit` | 用同一套发射规则重排任意 schema |
//!
//! 唯一的例外是 `main.rs roundtrip`：那里**故意**用 `serde_yaml` 发射，因为它要拿一个
//! 独立实现当参照物来验证我们的**读入器**（读入器 + 同一个发射器互证是循环论证）。
//! 它不是产物写出路径，理由写在 `main.rs` 的 `roundtrip` 注释里。
//!
//! ## 为什么不用 `serde_yaml` 发射
//!
//! 产物要和 `yaml-cpp` 的输出**语义对齐**（重新解析后的标量类型、引号、block 风格）。
//! `serde_yaml` 的引号/缩进策略与 yaml-cpp 不同，所以按 `EmitScalar` 的规则手写：
//!
//! * 含换行 → literal block（`|`），逐行补缩进；
//! * 只含 `[A-Za-z0-9_.]` → 裸写；空串 → `""`；
//! * 其余 → 双引号，并按 yaml-cpp 的转义表处理 `" \ \n \r \t \0` 与 `\xNN`；
//! * 映射里 `null` 值**整个键跳过**（C++：`if (!it->second || kNull) continue;`）；
//! * `depth >= 3` 切 flow 风格，flow 状态向下传递。
//!
//! **键序不在这里决定**：映射按 `IndexMap` 的插入顺序迭代，也就是**源文件里的键序**
//! （见 [`super::types::MapRef`]）。C++ 那边是 `std::map`，产物按字节序排列，和手写源
//! 对不上；这里有意不追那个顺序，`diff` 比的是树不是字节。
//!
//! 标量风格这些规则逐条对拍过 30 份 librime 产物（`fixture/build-20261023/`），
//! 改动前请先看 [`to_yaml_string`] 的测试与 `CLI roundtrip` 的 30/30。
//!
//! ## 为什么返回 `Result`
//!
//! 当前实现里所有输入都是 `String`，没有已知的失败条件；`Result` 是**显式的失败通道**，
//! 让调用方（`save` / `compile` / `emit`）从一开始就把"写不出去"当回事，而不是等
//! 将来某天出现非 UTF-8 或超长标量时才发现没有地方报错。

use std::fmt;

use super::types::Item;

/// 发射失败。目前没有已知触发条件（见模块文档），字段公开以便外部构造/匹配。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmitError {
    /// 面向用户的失败原因（中文）。
    reason: String,
}

impl fmt::Display for EmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for EmitError {}

/// `EmitYaml` 的入口：把一棵树发射成 YAML 文本（**总是以换行结尾**）。
pub fn to_yaml_string(item: &Item) -> Result<String, EmitError> {
    let mut out = String::new();
    emit(item, &mut out, 0, 0, false);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// 标量 + 空容器可以跟在 `key:` / `- ` 后面，不必换行。
fn inlineable(item: &Item) -> bool {
    match item {
        Item::Null => true,
        Item::Scalar(_) => true,
        Item::List(list) => list.borrow().is_empty(),
        Item::Map(map) => map.borrow().is_empty(),
    }
}

fn push_spaces(out: &mut String, count: usize) {
    for _ in 0..count {
        out.push(' ');
    }
}

/// `EmitYaml(item, emitter, depth)`。
///
/// `depth >= 3` 时切到 flow 风格（C++ 也一样）；flow 状态会传给子孙。
fn emit(item: &Item, out: &mut String, depth: usize, indent: usize, in_flow: bool) {
    match item {
        Item::Null => {}
        Item::Scalar(value) => emit_scalar(value, out, in_flow, indent),
        Item::List(list) => {
            let flow = in_flow || depth >= 3;
            let seq = list.borrow();
            if seq.is_empty() {
                out.push_str("[]");
                return;
            }
            if flow {
                out.push('[');
                for (i, child) in seq.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    emit(child, out, depth + 1, 0, true);
                }
                out.push(']');
            } else {
                for child in seq.iter() {
                    push_spaces(out, indent);
                    if inlineable(child) {
                        out.push_str("- ");
                        emit(child, out, depth + 1, indent + 2, false);
                        out.push('\n');
                    } else {
                        out.push_str("-\n");
                        // flow 子节点自己不带缩进，需要在这里补齐
                        if depth + 1 >= 3 {
                            push_spaces(out, indent + 2);
                        }
                        emit(child, out, depth + 1, indent + 2, false);
                        if !out.ends_with('\n') {
                            out.push('\n');
                        }
                    }
                }
            }
        }
        Item::Map(map) => {
            let flow = in_flow || depth >= 3;
            let entries = map.borrow();
            if entries.is_empty() {
                out.push_str("{}");
                return;
            }
            if flow {
                out.push('{');
                let mut first = true;
                for (key, value) in entries.iter() {
                    if value.is_null() {
                        continue;
                    }
                    if !first {
                        out.push_str(", ");
                    }
                    first = false;
                    emit_scalar(key, out, true, 0);
                    out.push_str(": ");
                    emit(value, out, depth + 1, 0, true);
                }
                out.push('}');
            } else {
                for (key, value) in entries.iter() {
                    // C++ 跳过 null 值（`if (!it->second || kNull) continue;`）
                    if value.is_null() {
                        continue;
                    }
                    push_spaces(out, indent);
                    emit_scalar(key, out, false, indent + 2);
                    out.push(':');
                    if inlineable(value) {
                        out.push(' ');
                        emit(value, out, depth + 1, indent + 2, false);
                        out.push('\n');
                    } else {
                        out.push('\n');
                        // flow 子节点自己不带缩进，需要在这里补齐
                        if depth + 1 >= 3 {
                            push_spaces(out, indent + 2);
                        }
                        emit(value, out, depth + 1, indent + 2, false);
                        if !out.ends_with('\n') {
                            out.push('\n');
                        }
                    }
                }
            }
        }
    }
}

/// `EmitScalar`：含换行 → literal block；非 `[A-Za-z0-9_.]` → 双引号；否则裸写。
fn emit_scalar(value: &str, out: &mut String, in_flow: bool, literal_indent: usize) {
    if value.is_empty() {
        // yaml-cpp 对空串实际写出 `""`（夹具里 yuhao_macro.macros 的空键就是 `"":`）
        out.push_str("\"\"");
        return;
    }
    let multiline = value.contains(['\r', '\n']);
    let plain = !multiline
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.');
    if plain {
        out.push_str(value);
    } else if multiline && !in_flow {
        // `YAML::Literal`。yaml-cpp 的 WriteLiteralString 总是写 `|`（不写 `|-`），
        // 逐字符写、遇 `\n` 换行、其余行首补缩进。
        out.push_str("|\n");
        let mut line_start = true;
        for ch in value.chars() {
            if ch == '\n' {
                out.push('\n');
                line_start = true;
            } else {
                if line_start {
                    push_spaces(out, literal_indent);
                    line_start = false;
                }
                out.push(ch);
            }
        }
    } else {
        write_double_quoted(value, out);
    }
}

fn write_double_quoted(value: &str, out: &mut String) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            ch if (ch as u32) < 0x20 || ch == '\u{7f}' => {
                out.push_str(&format!("\\x{:02x}", ch as u32));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::Item;

    fn yaml(item: &Item) -> String {
        to_yaml_string(item).expect("发射不该失败")
    }

    /// 发射规则的最小钉子：裸写 / 双引号 / literal block / 空容器 / 跳过 null。
    #[test]
    fn scalar_styles_match_yaml_cpp() {
        // 只含 [A-Za-z0-9_.] → 裸写
        assert_eq!(yaml(&Item::scalar("abc_1.z")), "abc_1.z\n");
        // 空串 → ""
        assert_eq!(yaml(&Item::scalar("")), "\"\"\n");
        // 需要引号的（空格、斜杠、中文、`$`）
        assert_eq!(yaml(&Item::scalar("a b")), "\"a b\"\n");
        assert_eq!(yaml(&Item::scalar("/")), "\"/\"\n");
        assert_eq!(yaml(&Item::scalar("〔全拼〕")), "\"〔全拼〕\"\n");
        // 含换行 → literal block（`|`，不写 `|-`）；顶层缩进 0，内容也就贴左边
        assert_eq!(yaml(&Item::scalar("a\nb\n")), "|\na\nb\n");
        // 作为映射值时才按 `键 + 2` 缩进（这才是实际用法：description 之类）。
        // 末尾的空行也是 yaml-cpp 的形状：夹具里 `description: |` 之后、下一个键之前
        // 正好空一行（见 fixture/.../yuling.schema.yaml:378-383）。
        let map = Item::map();
        if let Item::Map(entries) = &map {
            entries
                .borrow_mut()
                .insert("description".into(), Item::scalar("a\nb\n"));
        }
        assert_eq!(yaml(&map), "description: |\n  a\n  b\n\n");
        // 空容器
        assert_eq!(yaml(&Item::list()), "[]\n");
        assert_eq!(yaml(&Item::map()), "{}\n");
    }

    #[test]
    fn map_skips_null_values_and_keeps_key_order() {
        let map = Item::map();
        if let Item::Map(entries) = &map {
            let mut entries = entries.borrow_mut();
            entries.insert("b".into(), Item::scalar("2"));
            entries.insert("a".into(), Item::scalar("1"));
            entries.insert("dropped".into(), Item::Null);
            // 已存在的键只换值、不挪位置（合并时的"已有键保持原位"全靠这条）
            entries.insert("b".into(), Item::scalar("2b"));
        }
        // IndexMap → **插入顺序**（源文件键序），不是字典序；null 值整键跳过（C++ 的 continue）
        assert_eq!(yaml(&map), "b: 2b\na: 1\n");
    }

    #[test]
    fn deep_nodes_become_flow() {
        // depth >= 3 起切 flow：第 4 层（下标 3）写成一行
        let leaf = Item::list();
        if let Item::List(list) = &leaf {
            list.borrow_mut().push(Item::scalar("x"));
            list.borrow_mut().push(Item::scalar("y"));
        }
        let l3 = Item::map();
        if let Item::Map(map) = &l3 {
            map.borrow_mut().insert("l4".into(), leaf);
        }
        let l2 = Item::map();
        if let Item::Map(map) = &l2 {
            map.borrow_mut().insert("l3".into(), l3);
        }
        let root = Item::map();
        if let Item::Map(map) = &root {
            map.borrow_mut().insert("l2".into(), l2);
        }
        assert_eq!(
            yaml(&root),
            "l2:\n  l3:\n    l4:\n      [x, y]\n",
            "depth>=3 的节点转 flow，且缩进由父层补齐"
        );
    }
}
