//! librime `src/rime/algo/spelling.h`(50 行) + `spelling.cc`(41 行) 的 Rust 移植。
//!
//! 只有两个结构：`SpellingProperties`（一条拼写的可信度/类型/提示）与 `Spelling`
//! （字符串 + 属性）。160 行 C++ 里有 3 处语义**不能想当然**：
//!
//! 1. `Spelling::operator==` **只比 `str`**，属性完全不参与（`spelling.h:44-45`）——
//!    `Script::Merge` 靠它去重，所以"同一个拼写串 + 不同属性"会被合并成一条；
//! 2. `Compose` 的类型取**最大值**（越"模糊"的数字越大），而 `Update` 取**最小值**
//!    （越"精确"的数字越小）—— 两个方向相反（`spelling.cc:6-8` / `spelling.cc:22-27`）；
//! 3. `Update` 结尾**无条件清空 tips**（`spelling.cc:37`），注释说明理由是提示只对其中
//!    一个来源有效，合并后可能不适用。
#![allow(dead_code)]

use std::cmp::Ordering;

/// 拼写类型。**必须**保持这个声明顺序（= C++ 的枚举值 0..5）：
/// 大小关系被 `Compose`（取 max）/ `Update`（取 min）/ 音节图的剪枝逻辑直接当权重用。
/// 见 `spelling.h:14-21`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(u8)]
pub enum SpellingType {
    /// `kNormalSpelling`：用户完整输入了所有编码
    #[default]
    Normal = 0,
    /// `kFuzzySpelling`：模糊音
    Fuzzy = 1,
    /// `kAbbreviation`：简拼
    Abbreviation = 2,
    /// `kCompletion`：补全（用户还没打完）
    Completion = 3,
    /// `kAmbiguousSpelling`：歧义切分点
    Ambiguous = 4,
    /// `kInvalidSpelling`：无效
    Invalid = 5,
}

impl SpellingType {
    /// C++ 里可以隐式转 int；Rust 侧显式一点。
    fn as_i32(self) -> i32 {
        self as i32
    }

    /// `Script::Dump` 里 `"-ac?!"[type]` 的对照表（`algebra.cc:49`）。
    /// C++ 对 type > 3 是越界 UB，这里给 `!` 兜底。
    pub(super) fn dump_char(self) -> char {
        match self {
            SpellingType::Normal => '-',
            SpellingType::Fuzzy => 'a',
            SpellingType::Abbreviation => 'c',
            SpellingType::Completion => '?',
            _ => '!',
        }
    }
}

/// 一条拼写的属性。对应 `spelling.h:23-34`。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpellingProperties {
    /// 拼写类型；越小越"精确"
    pub r#type: SpellingType,
    /// 该拼写在输入串里的结束位置（音节图里用；`spelling.h:26`）
    pub(super) end_pos: usize,
    /// 可信度：`Compose` 累加、`Update` 取最大
    pub credibility: f64,
    /// 提示串（如简拼提示）
    pub(super) tips: String,
    /// 是否为纠错拼写
    pub(super) is_correction: bool,
}

impl SpellingProperties {
    fn new() -> Self {
        Self::default()
    }

    /// 疊加運算規則，把運算規則產生的屬性應用到當前拼寫上。`spelling.cc:5-19`。
    ///
    /// * 类型取**更模糊**者（`spelling.cc:6-8`）；
    /// * 可信度**累加**（`spelling.cc:10`）—— 所以 fuzz/abbrev/correction 各自加一个
    ///   `log(0.5)` / `log(0.01)` 的罚分；
    /// * 纠错标记**只置位不清零**（`spelling.cc:12-14`）；
    /// * tips 非空才覆盖（`spelling.cc:16-18`）。
    pub(super) fn compose(&mut self, delta: &SpellingProperties) {
        // 類型取最模糊者
        if delta.r#type > self.r#type {
            self.r#type = delta.r#type;
        }
        // 權重累加
        self.credibility += delta.credibility;
        // 糾錯標記
        if delta.is_correction {
            self.is_correction = true;
        }
        if !delta.tips.is_empty() {
            self.tips = delta.tips.clone();
        }
        // 注意：end_pos 不参与 Compose（C++ 里也没写）。
    }

    /// 更新優化拼寫，把另一個候選拼寫合併到當前記錄。`spelling.cc:21-38`。
    pub(super) fn update(&mut self, other: &SpellingProperties) {
        match self.r#type.cmp(&other.r#type) {
            // 類型相同, 以權重高者爲主, 同步糾錯標記
            Ordering::Equal => {
                self.is_correction = self.is_correction && other.is_correction;
            }
            // 採納更優的類型；糾錯狀態跟隨類型變更
            Ordering::Greater => {
                self.r#type = other.r#type;
                self.is_correction = other.is_correction;
            }
            Ordering::Less => {}
        }
        // 保留最大權重
        if other.credibility > self.credibility {
            self.credibility = other.credibility;
        }
        // 提示是針對其中一個拼寫來源的, 可能對合併後的拼寫不適用
        self.tips.clear();
    }

    /// `spelling.h:26` 的 `size_t end_pos;` —— **只读**访问器。
    ///
    /// 字段本身保持 `pub(super)`（只有 algo 层能写）；dict 层 `Table::Query`
    /// （`table.cc:597` 的 `props->end_pos`）要读它来算"这条边落在输入串的哪个位置"。
    pub fn end_pos(&self) -> usize {
        self.end_pos
    }
}

/// 一条拼写。`spelling.h:36-47`。
#[derive(Debug, Clone, Default)]
pub struct Spelling {
    pub str: String,
    pub properties: SpellingProperties,
}

impl Spelling {
    pub fn new(s: impl Into<String>) -> Self {
        Self {
            str: s.into(),
            properties: SpellingProperties::default(),
        }
    }
}

// ⚠️ 只比 `str`：C++ `operator==` 就是 `str == other.str`（`spelling.h:44`）。
// 派生 PartialEq 会把 properties 也算进去，`Script::Merge` 的去重就全错了。
impl PartialEq for Spelling {
    fn eq(&self, other: &Self) -> bool {
        self.str == other.str
    }
}

impl Eq for Spelling {}

impl PartialOrd for Spelling {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Spelling {
    /// `spelling.h:45`：`bool operator<(const Spelling& other) { return str < other.str; }`
    fn cmp(&self, other: &Self) -> Ordering {
        self.str.cmp(&other.str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spelling_type_order_matches_cpp_enum() {
        // 顺序就是 C++ 的枚举值顺序，剪枝/取 min/取 max 全靠它
        assert!(SpellingType::Normal < SpellingType::Fuzzy);
        assert!(SpellingType::Fuzzy < SpellingType::Abbreviation);
        assert!(SpellingType::Abbreviation < SpellingType::Completion);
        assert!(SpellingType::Completion < SpellingType::Ambiguous);
        assert!(SpellingType::Ambiguous < SpellingType::Invalid);
        assert_eq!(SpellingType::Normal.as_i32(), 0);
        assert_eq!(SpellingType::Invalid.as_i32(), 5);
        assert_eq!(SpellingType::default(), SpellingType::Normal);
    }

    #[test]
    fn spelling_equality_ignores_properties() {
        // 照抄 `spelling.h:44`
        let mut a = Spelling::new("shang");
        let b = Spelling::new("shang");
        a.properties.r#type = SpellingType::Fuzzy;
        a.properties.credibility = -1.0;
        assert_eq!(a, b);
        assert_ne!(Spelling::new("a"), Spelling::new("b"));
    }

    #[test]
    fn compose_type_takes_max_and_credibility_accumulates() {
        // 照抄 `spelling.cc:5-19`
        let mut p = SpellingProperties::default();
        let mut delta = SpellingProperties::default();
        delta.r#type = SpellingType::Abbreviation;
        delta.credibility = -0.6931471805599453;
        delta.tips = "tip".into();
        p.compose(&delta);
        assert_eq!(p.r#type, SpellingType::Abbreviation);
        assert_eq!(p.credibility, -0.6931471805599453);
        assert_eq!(p.tips, "tip");
        // 再来一次：类型保持更模糊的（Normal 不会把 Abbreviation 降下去）
        let mut milder = SpellingProperties::default();
        milder.r#type = SpellingType::Normal;
        milder.credibility = -0.6931471805599453;
        p.compose(&milder);
        assert_eq!(p.r#type, SpellingType::Abbreviation);
        assert_eq!(p.credibility, -1.3862943611198906);
        // 纠错标记只置位
        let mut c = SpellingProperties::default();
        c.is_correction = true;
        p.compose(&c);
        assert!(p.is_correction);
        p.compose(&SpellingProperties::default());
        assert!(p.is_correction);
        // 空 tips 不覆盖
        assert_eq!(p.tips, "tip");
    }

    #[test]
    fn update_takes_better_type_and_max_credibility_and_clears_tips() {
        // 照抄 `spelling.cc:21-38`
        let mut p = SpellingProperties::default();
        p.r#type = SpellingType::Abbreviation;
        p.credibility = -0.5;
        p.tips = "a".into();
        p.is_correction = true;

        let mut other = SpellingProperties::default();
        other.r#type = SpellingType::Fuzzy;
        other.credibility = -0.2;
        other.is_correction = false;
        other.tips = "b".into();
        p.update(&other);
        assert_eq!(p.r#type, SpellingType::Fuzzy); // 采纳更优类型
        assert!(!p.is_correction); // 纠错状态跟随类型变更
        assert_eq!(p.credibility, -0.2); // 保留最大权重（-0.2 > -0.5）
        assert_eq!(p.tips, ""); // tips 无条件清空

        // 类型相同：is_correction 取与
        let mut same = SpellingProperties::default();
        same.r#type = SpellingType::Normal;
        same.is_correction = true;
        let mut other_same = SpellingProperties::default();
        other_same.r#type = SpellingType::Normal;
        other_same.is_correction = false;
        same.update(&other_same);
        assert!(!same.is_correction);
        assert_eq!(same.r#type, SpellingType::Normal);
    }
}
