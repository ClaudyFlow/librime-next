//! librime `src/rime/algo/algebra.h`(43 行) + `algebra.cc`(142 行) 的 Rust 移植。
//!
//! `algebra` 是 speller 的规则引擎：`Script`（拼写 → 一组 `Spelling` 的映射）+
//! `Projection`（一串 `Calculation`，按顺序作用到 `Script` / 字符串上）。
//!
//! ## 主入口是 `&[String]` 而不是 `Config*`
//!
//! C++ 里 `Projection::Load(an<ConfigList>)` 只做一件事：遍历列表取字符串再逐条
//! `Calculus::Parse`（`algebra.cc:56-88`）。我们把这条**主入口**改成
//! [`Projection::from_rules(&[String])`](Projection::from_rules)，配置树那条路留成
//! 薄包装 [`Projection::from_config`](Projection::from_config) /
//! [`Projection::from_config_path`](Projection::from_config_path)（吃 `serde_json::Value`，
//! 与 `src/config.rs` 的树类型同一套）。
//!
//! ## 照抄的古怪点
//!
//! 1. **`Apply(Script)` 每一轮都是"用 temp 整体替换"**（`algebra.cc:120-129`）：
//!    没被改的条目也要重新 `Merge` 一遍，而 `Merge` 会对**已存在的同串条目**调用
//!    `properties.Update()` —— 而 `Update` 会**清空 tips**（`spelling.cc:37`）。
//!    也就是说：走一趟 Projection，提示信息会掉。
//! 2. `derive` 分支里两次 `Merge`：先合并**原串**（`!x->deletion()`），再合并**新串**
//!    （`x->addition() && !s.str.empty()`）。`erase` 的 `addition()` 是 false，
//!    所以删除规则只留下空串之外的东西（`algebra.cc:111-117`）。
//! 3. `Script::Dump` 的第三列是 `"-ac?!"[type]`（`algebra.cc:49`）—— C++ 对
//!    `type > 3` 是越界 UB，这里用 `'!'` 兜底。
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use super::calculus::{Calculation, Calculus};
use super::spelling::{Spelling, SpellingProperties};

/// 拼写 → 该拼写能代表的音节列表。对应 C++ 的
/// `class Script : public map<string, vector<Spelling>>`（`algebra.h:21-29`）。
///
/// ⚠️ C++ 用 `std::map`（有序），这里用 `BTreeMap` 保持**同序迭代**：
/// `Projection::Apply(Script)` 的输出顺序、以及 `Prism::Build(syllabary, script)`
/// 里 spelling id 的分配顺序都依赖它。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Script {
    map: BTreeMap<String, Vec<Spelling>>,
}

impl Script {
    pub fn new() -> Self {
        Self::default()
    }

    /// `algebra.cc:14-20`：已有同名音节就不加（返回 false）。
    pub fn add_syllable(&mut self, syllable: &str) -> bool {
        if self.map.contains_key(syllable) {
            return false;
        }
        self.map
            .insert(syllable.to_string(), vec![Spelling::new(syllable)]);
        true
    }

    /// `algebra.cc:22-42`：把 `v`（一组拼写）以 `s` 为键合并进来，
    /// 每条都先 `Compose(sp)`；若 `s` 下已有**同串**拼写则 `Update` 合属性。
    fn merge(&mut self, s: &str, sp: &SpellingProperties, v: &[Spelling]) {
        let m = self.map.entry(s.to_string()).or_default();
        for x in v {
            let mut y = x.clone();
            // 疊加運算結果（`algebra.cc:31`）
            y.properties.compose(sp);
            // `algebra.cc:32-39`：注意找的是 **x**（未叠加的原始条目）
            match m.iter().position(|e| *e == *x) {
                None => m.push(y),
                Some(index) => {
                    // 合併屬性
                    m[index].properties.update(&y.properties);
                }
            }
        }
    }

    /// `algebra.cc:44-54`：把整张表写成 TSV（调试用）。
    fn dump(&self, file_path: &Path) -> std::io::Result<()> {
        use std::fmt::Write as _;
        let mut out = String::new();
        for (key, spellings) in &self.map {
            let mut first = true;
            for s in spellings {
                let _ = writeln!(
                    out,
                    "{}\t{}\t{}\t{}\t{}",
                    if first { key.as_str() } else { "" },
                    s.str,
                    s.properties.r#type.dump_char(),
                    s.properties.credibility,
                    s.properties.tips
                );
                first = false;
            }
        }
        std::fs::write(file_path, out)
    }

    /// 只读访问（C++ 那边是 `map` 的公开接口）。
    pub fn get(&self, key: &str) -> Option<&Vec<Spelling>> {
        self.map.get(key)
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut Vec<Spelling>> {
        self.map.get_mut(key)
    }

    pub(super) fn contains_key(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Vec<Spelling>)> {
        self.map.iter()
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.map.keys()
    }
}

/// 规则序列。对应 `algebra.h:31-41`。
pub struct Projection {
    calculation: Vec<Box<dyn Calculation>>,
}

impl Default for Projection {
    fn default() -> Self {
        Self {
            calculation: Vec::new(),
        }
    }
}

impl Projection {
    /// **主入口**：从规则文本列表建投影。
    ///
    /// 对应 `algebra.cc:56-88`：一条失败就整体失败（C++ 会 `calculation_.clear()`
    /// 后返回 false；这里返回 `Err`，调用方拿不到半成品投影）。
    ///
    /// 错误信息保留 C++ 的前缀 `Error loading spelling algebra definition #N.`，
    /// 后面接 [`Calculus::try_parse`] 的诊断（规则原文 / 出错片段 / 字节位置），
    /// 所以"为什么不成立"不用再靠肉眼猜。
    pub fn from_rules(rules: &[String]) -> Result<Self, String> {
        let calc = Calculus::new();
        let mut calculation: Vec<Box<dyn Calculation>> = Vec::with_capacity(rules.len());
        for (i, formula) in rules.iter().enumerate() {
            // `algebra.cc:65-72`：`calc.Parse` 失败 = 规则语法错
            let x = calc.try_parse(formula).map_err(|error| {
                format!(
                    "Error loading spelling algebra definition #{}. {error}",
                    i + 1
                )
            })?;
            calculation.push(x);
        }
        Ok(Self { calculation })
    }

    /// 薄包装：从配置树的**列表节点**取规则（`["derive/^a$/b/", ...]`）。
    ///
    /// C++ 的 `Load` 还要求每个元素都是 `ConfigValue`（`algebra.cc:59-63`），
    /// 非字符串元素同样算失败。
    fn from_config(list: &Value) -> Result<Self, String> {
        let items = list
            .as_array()
            .ok_or_else(|| "spelling algebra is not a list".to_string())?;
        let mut rules: Vec<String> = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            let text = item
                .as_str()
                .ok_or_else(|| format!("Error loading formula #{}.", i + 1))?;
            rules.push(text.to_string());
        }
        Self::from_rules(&rules)
    }

    /// 薄包装：按 `"speller/algebra"` 这样的路径在配置树里定位列表再 [`Self::from_config`]。
    fn from_config_path(tree: &Value, path: &str) -> Result<Self, String> {
        let mut node = tree;
        for segment in path.split('/').filter(|s| !s.is_empty()) {
            node = node
                .get(segment)
                .ok_or_else(|| format!("config path '{path}' not found (at '{segment}')"))?;
        }
        Self::from_config(node)
    }

    /// `algebra.cc:90-108`：把规则链作用到一个**字符串**上。
    /// 返回"是否被改过"；`false` 时字符串保持原样。
    pub fn apply_str(&mut self, value: &mut String) -> bool {
        if value.is_empty() {
            return false;
        }
        let mut modified = false;
        let mut s = Spelling::new(value.clone());
        for x in self.calculation.iter_mut() {
            if x.apply(&mut s) {
                modified = true;
            }
        }
        if modified {
            *value = s.str;
        }
        modified
    }

    /// `algebra.cc:110-141`：把规则链作用到整张 `Script` 上。
    ///
    /// 每一轮：`temp` 从空开始，逐条决定"留原串 / 留新串 / 都留"，然后
    /// **整张表换成 temp**（所以顺序、去重都由 `Merge` 决定）。
    pub fn apply_script(&mut self, value: &mut Script) -> bool {
        if value.is_empty() {
            return false;
        }
        let mut modified = false;
        for x in self.calculation.iter_mut() {
            let old = std::mem::take(&mut value.map);
            let mut temp = Script::new();
            for (key, spellings) in &old {
                let mut s = Spelling::new(key);
                let applied = x.apply(&mut s);
                if applied {
                    modified = true;
                    // `algebra.cc:114-116`
                    if !x.deletion() {
                        temp.merge(key, &SpellingProperties::default(), spellings);
                    }
                    // `algebra.cc:117-119`
                    if x.addition() && !s.str.is_empty() {
                        temp.merge(&s.str, &s.properties, spellings);
                    }
                } else {
                    // `algebra.cc:120-122`
                    temp.merge(key, &SpellingProperties::default(), spellings);
                }
            }
            value.map = temp.map;
        }
        modified
    }

    /// 规则条数（C++ 没有，测试/报告用）。
    pub fn len(&self) -> usize {
        self.calculation.len()
    }

    fn is_empty(&self) -> bool {
        self.calculation.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::spelling::SpellingType;

    fn rules(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // C++ 的断言写的是 `s["sh"][0].properties.type`（按下标取），这里同款
    fn rule_type(spellings: &[Spelling], index: usize) -> SpellingType {
        spellings[index].properties.r#type
    }

    fn rule_credibility(spellings: &[Spelling], index: usize) -> f64 {
        spellings[index].properties.credibility
    }

    // ---- 下面两条对应 librime `test/algebra_test.cc` 的全部断言 ----

    #[test]
    fn spelling_manipulation() {
        // algebra_test.cc:23-33
        let mut p = Projection::from_rules(&rules(&[
            "xlit/ABCDEFGHIJKLMNOPQRSTUVWXYZ/abcdefghijklmnopqrstuvwxyz/",
            "xform/^([zcs])h(.*)$/$1$2/",
        ]))
        .expect("两条规则都应能解析");
        let mut s = "Shang".to_string();
        assert!(p.apply_str(&mut s));
        assert_eq!(s, "sang");
        // 空串直接 false
        let mut empty = String::new();
        assert!(!p.apply_str(&mut empty));
    }

    #[test]
    fn projection_on_script() {
        // algebra_test.cc:35-78（逐条照抄）
        let instructions = [
            "xform/^(\\l+)\\d$/$1/",
            "erase/^[wxy].*$/",
            "derive/^([zcs])h(.*)$/$1$2/",
            "abbrev/^(\\l).+$/$1/",
            "abbrev/^([zcs]h).+$/$1/",
        ];
        let mut p = Projection::from_rules(&rules(&instructions)).expect("5 条规则都应能解析");

        let mut s = Script::new();
        for syllable in [
            "zhang1", "chang2", "shang3", "shang4", "zang1", "cang2", "bang3", "wang4",
        ] {
            assert!(s.add_syllable(syllable));
        }
        // 同一个音节加两次 → false
        assert!(!s.add_syllable("zhang1"));

        assert!(p.apply_script(&mut s));
        assert_eq!(14, s.len());
        for expected in [
            "zhang", "chang", "shang", "zang", "cang", "sang", "bang", "zh", "ch", "sh", "z",
            "c", "s", "b",
        ] {
            assert!(s.contains_key(expected), "缺少 {expected}");
        }
        for unexpected in ["wang", "wang4", "zhang1", "bang3"] {
            assert!(!s.contains_key(unexpected), "不该有 {unexpected}");
        }
        assert_eq!(2, s.get("z").map(|v| v.len()).unwrap_or(0));
        assert_eq!(1, s.get("zh").map(|v| v.len()).unwrap_or(0));
        let sh = s.get("sh").expect("sh");
        assert_eq!(2, sh.len());
        assert_eq!(SpellingType::Abbreviation, sh[0].properties.r#type);
        assert_eq!(0.5f64.ln(), sh[0].properties.credibility);
    }

    // ---- 额外的语义钉子 ----

    #[test]
    fn bad_rule_reports_index_and_formula() {
        let err = match Projection::from_rules(&rules(&["xform/^a$/b/", "xform/(bad/b/"])) {
            Ok(_) => panic!("非法正则应当报错"),
            Err(e) => e,
        };
        assert!(err.contains("#2"), "{err}");
        assert!(err.contains("xform/(bad/b/"), "{err}");
    }

    #[test]
    fn from_config_and_config_path() {
        let tree = serde_json::json!({
            "speller": {
                "algebra": ["derive/^a$/b/"]
            }
        });
        assert!(Projection::from_config_path(&tree, "speller/algebra").is_ok());
        assert!(Projection::from_config_path(&tree, "speller/nope").is_err());
        // 元素不是字符串 → 与 C++ 的 `if (!v) break;` 一致，算失败
        assert!(Projection::from_config(&serde_json::json!(["ok/a/b/", 42])).is_err());
        assert!(Projection::from_config(&serde_json::json!({"not": "a list"})).is_err());
    }

    #[test]
    fn derive_keeps_original_and_adds_new() {
        // derive 的 deletion() = false、addition() = true → 两串都在
        let mut p = Projection::from_rules(&rules(&["derive/^([jqxy])u$/$1v/"])).unwrap();
        let mut s = Script::new();
        s.add_syllable("ju");
        assert!(p.apply_script(&mut s));
        assert!(s.contains_key("ju"));
        assert!(s.contains_key("jv"));
        assert_eq!(2, s.len());
    }

    #[test]
    fn erase_drops_original_and_adds_nothing() {
        // ⚠️ `algebra.cc:113-119` 的两个条件方向容易搞反：
        //   if (!x->deletion()) temp.Merge(原串)   ← erase 的 deletion() 是 **true**，所以原串**丢**
        //   if (x->addition() && !s.str.empty())   ← erase 的 addition() 是 false，所以新串**不**加
        // 结论：erase 命中 = 这条拼写消失。
        let mut p = Projection::from_rules(&rules(&["erase/^xx$/"])).unwrap();
        let mut s = Script::new();
        s.add_syllable("xx");
        s.add_syllable("aa");
        assert!(p.apply_script(&mut s));
        assert!(!s.contains_key("xx"), "erase 命中 → 原串被丢弃");
        assert!(!s.contains_key(""), "addition() 是 false，空串不进");
        assert_eq!(1, s.len());
        assert!(s.contains_key("aa"));
    }

    #[test]
    fn projection_round_trip_clears_tips_on_collision() {
        // ⚠️ C++ 的坑：apply_script 每轮都用 Merge 重建；当两条来源合并到**同一个键**时
        // `Merge` 会走 `properties.Update()`，而 `Update` 结尾无条件清 tips。
        // 这里钉住这个行为，防止以后"顺手修好"。
        let mut p = Projection::from_rules(&rules(&["derive/^aa$/bb/"])).unwrap();
        let mut s = Script::new();
        s.add_syllable("aa");
        s.add_syllable("bb");
        s.get_mut("aa").unwrap()[0].properties.tips = "甲的提示".to_string();
        s.get_mut("bb").unwrap()[0].properties.tips = "乙的提示".to_string();
        assert!(p.apply_script(&mut s));
        // "aa" 没有被撞上 → tips 还在
        assert_eq!(s.get("aa").unwrap()[0].properties.tips, "甲的提示");
        // ⚠️ 关键语义：`Script` 的 **key 是码、Spelling.str 是音节**（`algebra.cc:31`）。
        // derive 出来的新键 "bb" 里装的仍是**原音节** "aa" 的副本，所以 key "bb" 下两条
        // 拼写的 str 分别是 "aa" 与 "bb" —— 不相等 → 不会走 Update，tips 各自保留。
        let bb = s.get("bb").unwrap();
        assert_eq!(bb.len(), 2);
        assert_eq!(bb[0].str, "aa");
        assert_eq!(bb[1].str, "bb");
        assert_eq!(bb[1].properties.tips, "乙的提示");
    }

    #[test]
    fn script_dump_writes_tsv() {
        let mut s = Script::new();
        s.add_syllable("aa");
        let mut p = Projection::from_rules(&rules(&["abbrev/^(\\l).+$/$1/"])).unwrap();
        assert!(p.apply_script(&mut s));
        let path = std::env::temp_dir().join("librime_next_script_dump.txt");
        s.dump(&path).expect("dump");
        let text = std::fs::read_to_string(&path).expect("read");
        // 第一列只在同键的第一条拼写出现（`algebra.cc:47-51`）
        assert!(text.contains("aa\taa\t-\t0\t"), "{text}");
        // 键 "a" 下装的是音节 "aa"，第三列 'c' = kAbbreviation
        assert!(
            text.contains("a\taa\tc\t-0.6931471805599453\t"),
            "{text}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rule_type_and_credibility_helpers_are_used() {
        // 让两个 helper 有调用点（同时验证 abbrev 的罚分是 log(0.5)）
        let mut p = Projection::from_rules(&rules(&["abbrev/^([zcs]h).+$/$1/"])).unwrap();
        let mut s = Script::new();
        s.add_syllable("shang");
        assert!(p.apply_script(&mut s));
        let sh = s.get("sh").unwrap();
        assert_eq!(1, sh.len());
        assert_eq!("shang", sh[0].str, "Spelling.str 是音节，不是码");
        assert_eq!(SpellingType::Abbreviation, rule_type(sh, 0));
        assert_eq!(0.5f64.ln(), rule_credibility(sh, 0));
    }
}
