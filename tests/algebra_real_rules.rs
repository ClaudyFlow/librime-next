//! **真实 algebra 规则的差分基线**：把项目和用户方案里真实存在的 `speller/algebra`
//! 规则逐条抄进来，钉住两件事：
//!
//! 1. **每条规则都能编译**（不许因为"子集不支持"而失败）；
//! 2. **代表性输入产出稳定结果**（改了就知道：断言里连规则原文一起写死）。
//!
//! ## 规则出处（逐字节抄自磁盘，`algebra_real_rules_live_matches_embedded_tables` 会再对一次）
//!
//! | 常量 | 来源 |
//! |---|---|
//! | [`YULING_ALGEBRA`] | `%APPDATA%\Rime-Next\schema\yuling\yuling.schema.yaml`（`speller/algebra`） |
//! | [`DOUBLE_PINYIN_ZIRANMA_ALGEBRA`] | `...\double_pinyin_ziranma\double_pinyin_ziranma.schema.yaml` |
//! | [`DOUBLE_PINYIN_FLYPY_ALGEBRA`] | `...\double_pinyin_flypy\double_pinyin_flypy.schema.yaml` |
//! | [`YUHAO_PINYIN_ALGEBRA`] | `...\yuhao_pinyin\yuhao_pinyin.schema.yaml` |
//! | [`YUHAO_QUANPIN_ALGEBRA`] | `...\yuhao_quanpin\yuhao_quanpin.schema.yaml` |
//!
//! `yuling_chaifen` / `yuling_extreme` **没有自己的 `speller/algebra`**
//! （`yuling_extreme` 用 `__include: yuling.schema:/` 继承 `yuling` 的那一条），
//! 由 `algebra_real_rules_chaifen_and_extreme_have_no_own_algebra` 在文件存在时确认。
//!
//! ## 为什么要抄进常量而不是每次读磁盘
//!
//! 测试得在**没有** `%APPDATA%` 的机器上也能跑（CI / 干净 clone）。所以：常量是基线，
//! 磁盘文件是"二次校验" —— 文件在就比对，不在就打印跳过。改方案的人会先被这条测试
//! 拦住，然后被要求"要么同步常量，要么确认新规则能编译"。
//!
//! ## 任务口径：4 份指定来源（`%APPDATA%\Rime-Next\schema\...`）
//!
//! | 来源 | 自己写的 `speller/algebra` | 生效条数 |
//! |---|---|---|
//! | `yuling/yuling.schema.yaml` | 1 条 | 1 |
//! | `double_pinyin_ziranma/double_pinyin_ziranma.schema.yaml` | 29 条 | 29 |
//! | `yuling_extreme/yuling_extreme.schema.yaml` | 无（`__include: yuling.schema:/`） | 1（继承 yuling） |
//! | `yuling_chaifen/yuling_chaifen.schema.yaml` | 无（整份文件没有 `speller`） | 0 |
//!
//! 合计 **30 条不同的规则原文**（按"每个方案各算一遍"是 31 条），全部可编译，
//! **0 条**落在子集之外。[`algebra_real_rules_specified_sources_compile`] 每次从磁盘重读
//! 这 4 份（跟着 `__include` 走一层继承）并逐条编译、逐条打印；`%APPDATA%` 不在就
//! 打印跳过，不失败。
//!
//! ## 曾经"不支持"、2026-10-25 起必须可用的两处写法
//!
//! `speller/algebra` 里的规则一直全部可编译（4 份指定来源 30 条；搬进常量的 5 套方案
//! 65 条；`schema/` 下 58 份方案文件的全部 163 条规则，0 条失败），所以本文件**不需要
//! 任何 `#[ignore]`**。
//!
//! `recognizer/patterns` 里还有两处 fancy 写法（`(?!)` 环视、`+?` 懒惰量词），那是
//! recognizer 层的正则、不走 `Calculus`。它们曾被 `regex` crate 拒绝（当时本文件钉的是
//! "必须报错"）；换成 `fancy-regex` 之后语义边界翻过来了 ——
//! [`algebra_real_rules_fancy_patterns_compile_and_match`] 现在钉的是"**能编译且结果正确**"。

use std::path::{Path, PathBuf};

use librime_next::algo::calculus::regex::Regex;
use librime_next::algo::{Calculus, Projection, Script, Spelling};

// ---------------------------------------------------------------------------
// 规则原文
// ---------------------------------------------------------------------------

/// `yuling.schema.yaml:159-160` 的 `speller/algebra`。
const YULING_ALGEBRA: &[&str] = &["derive/^([a-y])([a-y])/$1z/"];

/// `double_pinyin_ziranma.schema.yaml:27-56` 的 `speller/algebra`。
const DOUBLE_PINYIN_ZIRANMA_ALGEBRA: &[&str] = &[
    "derive/^([jqxy])u$/$1v/",
    "xform/^zh/V/",
    "xform/^ch/I/",
    "xform/^sh/U/",
    "xform/^([aoe])(ng)?$/$1$1$2/",
    "derive/^([aoe])([ioun])$/$1$1$2/",
    "xform/iu$/Q/",
    "xform/(.)ei$/$1Z/",
    "xform/uan$/R/",
    "xform/[uv]e$/T/",
    "xform/un$/P/",
    "xform/uo$/O/",
    "xform/ie$/X/",
    "xform/i?ong$/S/",
    "xform/ing$|uai$/Y/",
    "xform/(.)ai$/$1L/",
    "xform/(.)en$/$1F/",
    "xform/(.)eng$/$1G/",
    "xform/[iu]ang$/D/",
    "xform/(.)ang$/$1H/",
    "xform/ian$/M/",
    "xform/(.)an$/$1J/",
    "xform/(.)ou$/$1B/",
    "xform/[iu]a$/W/",
    "xform/iao$/C/",
    "xform/(.)ao$/$1K/",
    "xform/ui$/V/",
    "xform/in$/N/",
    "xlit/QWRTYUIOPSDFGHJKLZXCVBNM/qwrtyuiopsdfghjklzxcvbnm/",
];

/// `double_pinyin_flypy.schema.yaml:27-56` 的 `speller/algebra`（键位与小鹤双拼一致）。
const DOUBLE_PINYIN_FLYPY_ALGEBRA: &[&str] = &[
    "derive/^([jqxy])u$/$1v/",
    "xform/^zh/V/",
    "xform/^ch/I/",
    "xform/^sh/U/",
    "xform/^([aoe])(ng)?$/$1$1$2/",
    "derive/^([aoe])([ioun])$/$1$1$2/",
    "xform/iu$/Q/",
    "xform/(.)ei$/$1W/",
    "xform/uan$/R/",
    "xform/[uv]e$/T/",
    "xform/un$/Y/",
    "xform/uo$/O/",
    "xform/ie$/P/",
    "xform/i?ong$/S/",
    "xform/ing$|uai$/K/",
    "xform/(.)ai$/$1D/",
    "xform/(.)en$/$1F/",
    "xform/(.)eng$/$1G/",
    "xform/[iu]ang$/L/",
    "xform/(.)ang$/$1H/",
    "xform/ian$/M/",
    "xform/(.)an$/$1J/",
    "xform/(.)ou$/$1Z/",
    "xform/[iu]a$/X/",
    "xform/iao$/N/",
    "xform/(.)ao$/$1C/",
    "xform/ui$/V/",
    "xform/in$/B/",
    "xlit/QWRTYUIOPSDFGHJKLZXCVBNM/qwrtyuiopsdfghjklzxcvbnm/",
];

/// `yuhao_pinyin.schema.yaml:49-53` 的 `speller/algebra`。
const YUHAO_PINYIN_ALGEBRA: &[&str] = &[
    "abbrev/^([a-z]).+$/$1/",
    "abbrev/^([zcs]h).+$/$1/",
    "derive/^([nl])ue$/$1ve/",
    "derive/^([jqxy])u/$1v/",
];

/// `yuhao_quanpin.schema.yaml:20-21` 的 `speller/algebra`。
const YUHAO_QUANPIN_ALGEBRA: &[&str] = &["derive/^([nl])ue$/$1ve/", "derive/^([jqxy])u/$1v/"];

/// 一份真实方案：id + 相对 `schema/` 的路径 + 它的 algebra。
struct RealSchema {
    id: &'static str,
    relative_path: &'static str,
    algebra: &'static [&'static str],
}

const REAL_SCHEMAS: &[RealSchema] = &[
    RealSchema {
        id: "yuling",
        relative_path: "yuling/yuling.schema.yaml",
        algebra: YULING_ALGEBRA,
    },
    RealSchema {
        id: "double_pinyin_ziranma",
        relative_path: "double_pinyin_ziranma/double_pinyin_ziranma.schema.yaml",
        algebra: DOUBLE_PINYIN_ZIRANMA_ALGEBRA,
    },
    RealSchema {
        id: "double_pinyin_flypy",
        relative_path: "double_pinyin_flypy/double_pinyin_flypy.schema.yaml",
        algebra: DOUBLE_PINYIN_FLYPY_ALGEBRA,
    },
    RealSchema {
        id: "yuhao_pinyin",
        relative_path: "yuhao_pinyin/yuhao_pinyin.schema.yaml",
        algebra: YUHAO_PINYIN_ALGEBRA,
    },
    RealSchema {
        id: "yuhao_quanpin",
        relative_path: "yuhao_quanpin/yuhao_quanpin.schema.yaml",
        algebra: YUHAO_QUANPIN_ALGEBRA,
    },
];

/// 任务口径的 4 份来源：`(id, 相对 `schema/` 的路径)`。
///
/// 后两份**没有自己的规则**，但同样要读一遍再下结论 —— 不能凭印象说"它们没有"：
/// * `yuling_extreme` 整份文件只有 `__include: yuling.schema:/` 与 `speller/+:
///   auto_select_pattern`，algebra 是继承 `yuling` 的那 1 条；
/// * `yuling_chaifen` 是纯拆分表方案，连 `speller:` 都没有。
const SPECIFIED_SCHEMAS: &[(&str, &str)] = &[
    ("yuling", "yuling/yuling.schema.yaml"),
    (
        "double_pinyin_ziranma",
        "double_pinyin_ziranma/double_pinyin_ziranma.schema.yaml",
    ),
    (
        "yuling_extreme",
        "yuling_extreme/yuling_extreme.schema.yaml",
    ),
    (
        "yuling_chaifen",
        "yuling_chaifen/yuling_chaifen.schema.yaml",
    ),
];

/// 一份方案**生效**的 algebra：`__include` 继承来的 + 自己声明的 + `speller/+` 补丁。
#[derive(Default)]
struct EffectiveAlgebra {
    rules: Vec<String>,
    /// 从哪份文件继承来的（`None` = 自己声明）。
    inherited_from: Option<String>,
    /// `__include` 指向的方案本机找不到 —— 规则**无法判定**，要打印出来，不能当 0 条。
    unresolved_include: Option<String>,
}

fn rule_strings(table: &[&str]) -> Vec<String> {
    table.iter().map(|rule| rule.to_string()).collect()
}

fn project(table: &[&str]) -> Projection {
    Projection::from_rules(&rule_strings(table))
        .unwrap_or_else(|error| panic!("整套规则应当能编译：{error}"))
}

/// 把规则链作用到单个拼写上，返回 (结果串, 是否被改)。
fn apply(rule: &str, input: &str) -> (String, bool) {
    let calc = Calculus::new();
    let mut calculation = calc
        .try_parse(rule)
        .unwrap_or_else(|error| panic!("规则应当能编译：{error}"));
    let mut spelling = Spelling::new(input);
    let changed = calculation.apply(&mut spelling);
    (spelling.str, changed)
}

/// 一个音节经过整套规则后得到的**拼写全集**（排好序）。
///
/// 用 `Script` 而不是逐条 `apply_str`：真实流程是"拼写集"，`derive` 会同时保留原串和新串，
/// 只看链尾字符串会漏掉一半信息。
fn spellings_of(table: &[&str], input: &str) -> Vec<String> {
    let mut projection = project(table);
    let mut script = Script::new();
    assert!(script.add_syllable(input), "音节 {input} 应当能加进 Script");
    projection.apply_script(&mut script);
    let mut keys: Vec<String> = script.keys().cloned().collect();
    keys.sort();
    keys
}

// ---------------------------------------------------------------------------
// 1) 全部规则都能编译
// ---------------------------------------------------------------------------

#[test]
fn algebra_real_rules_all_compile() {
    let calc = Calculus::new();
    let mut total = 0usize;
    for schema in REAL_SCHEMAS {
        println!("-- {}（{} 条）", schema.id, schema.algebra.len());
        for rule in schema.algebra {
            total += 1;
            calc.try_parse(rule).unwrap_or_else(|error| {
                panic!("{} 的规则编译失败：`{rule}`（{error}）", schema.id);
            });
        }
        // 整条链也必须能建起来（`Projection::from_rules` 走的是同一个 Calculus）
        project(schema.algebra);
    }
    // 抄进来的规则总数：1 + 29 + 29 + 4 + 2；少于这个数说明有人删了规则原文
    assert_eq!(total, 65, "真实 algebra 规则总数变了，检查上面的常量");
    println!("常量基线：{total} 条规则全部可编译");
}

// ---------------------------------------------------------------------------
// 1b) 任务口径：4 份指定来源，逐条打印 + 逐条编译
// ---------------------------------------------------------------------------
//
// 先把"真实规则里到底出现过哪些正则结构"数清楚，再谈边界。统计口径 = 4 份指定
// 来源的 30 条不同规则原文（`xlit` 那条不是正则，单列）：
//
//   结构            出现的规则数   例子                        子集支持   边界说明
//   --------------  -----------  --------------------------  --------  ------------------------------
//   `^` 串首            7        `derive/^([a-y])([a-y])/`    ✓        **边界**：只有"整串"语义，
//                                                                      没有 `(?m)` 多行；speller 的
//                                                                      拼写本来就是单串
//   `$` 串尾           26        `xform/iu$/Q/`、`(.)ei$`     ✓        **边界**：同上，非多行
//   `(...)` 捕获组     12        `^([jqxy])u$`、`(.)ei$`      ✓        **边界**：只额外支持 `(?:…)`；
//                                                                      命名组 `(?<n>…)` 会报错
//   `[a-y]` 区间        1        `^([a-y])([a-y])/`           ✓        **边界**：按 ASCII 字节，只认
//                                                                      枚举/`-` 区间；`[[:alpha:]]`、
//                                                                      `\p{L}` 报错
//   `[uv]` 枚举         6        `[uv]e$`、`[iu]ang$`         ✓        —
//   `.` 任意字节        8        `(.)an$`、`(.)ou$`           ✓        **边界**：`.` = 一个**字节**
//                                                                      （boost 对 std::string 也是），
//                                                                      对 CJK 是 3 次匹配
//   `?` 可选            2        `(ng)?`、`i?ong$`            ✓        **边界**：贪婪；`+?` `*?` `??`
//                                                                      这类懒惰量词直接报错
//   `|` 分支            1        `xform/ing$|uai$/Y/`         ✓        **边界**：优先级最低，等价
//                                                                      `(ing$)|(uai$)`，加括号才改范围
//   `$1` `$2` 回引     12        `$1z`、`$1$1$2`、`$1v`        ✓        **边界**：替换串只认 `$1`
//                                                                      `${1}` `$&`；`${name}` 报错
//   `xlit/…/…/`         1        `xlit/QWRT…/qwr…/`           非正则    逐字符映射，不经过 `Regex`
//
// 没出现的（子集**支持**，写新规则时算免费）：`*` `+` `{n,m}` `[^…]` `(?:…)`
// `\w \s \d \l \u` `\b`、模式内 `\1..\9`、`${1}`、`$&`。
//
// **落在子集边界外**的（30 条里一条都没用到，引擎对它们是**编译期报错**、不静默
// 降级，所以"全部编译通过"等价于"没有用到不支持的写法"）：环视 `(?=)` `(?!)`
// `(?<=)` `(?<!)`、内联选项 `(?i)` `(?m)`、命名组、原子组 `(?>`、懒惰/占有量词
// `*?` `+?` `??`、POSIX 类 `[[:alpha:]]`、`\p{…}` `\A` `\Z` `\Q…\E`、八进制
// `\012`、量词下界>上界、`\3` 但只有 2 个组。
// 真实配置里唯一的例子在 `recognizer/patterns`（不是 algebra）：
//   `^(?![`;]).*[A-Z][-_+.'0-9A-Za-z]*$`（环视）、`^z([a-z]+?)*$`（懒惰量词）；
// 由 [`algebra_real_rules_known_unsupported_patterns_are_rejected`] 钉住"必须被拒绝"。

#[test]
fn algebra_real_rules_specified_sources_compile() {
    let Some(root) = schema_root() else {
        return skip("没有 %APPDATA%\\Rime-Next\\schema");
    };
    let calc = Calculus::new();
    // `read_sources` 数的是"文件读到并解析了"（`yuling_chaifen` 是 0 条，但读到了）
    let mut read_sources = 0usize;
    let mut total = 0usize;
    let mut compiled = 0usize;
    // 去重后的规则原文：`yuling_extreme` 继承 `yuling`，按方案算 31 条、去重 30 条
    let mut unique: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // (来源 id, 第几条, 规则原文, 诊断)
    let mut failures: Vec<(String, usize, String, String)> = Vec::new();
    println!("== 指定来源：{}", root.display());
    for (id, relative_path) in SPECIFIED_SCHEMAS {
        let Some(effective) = effective_algebra(&root, relative_path) else {
            skip(relative_path);
            continue;
        };
        if let Some(include) = &effective.unresolved_include {
            println!(
                "跳过 {id}（{relative_path}）：`__include: {include}` 本机没有，\
                 继承来的规则无法判定（不猜测、也不当 0 条）"
            );
            continue;
        }
        read_sources += 1;
        let inherited = match &effective.inherited_from {
            Some(from) => format!("，`__include` 继承自 {from}"),
            None => String::new(),
        };
        println!(
            "\n-- {id}（{relative_path}{inherited}）：{} 条",
            effective.rules.len()
        );
        for (index, rule) in effective.rules.iter().enumerate() {
            total += 1;
            unique.insert(rule.clone());
            match calc.try_parse(rule) {
                Ok(_) => {
                    compiled += 1;
                    println!("  [{:02}] ok   {rule}", index + 1);
                }
                Err(error) => {
                    println!("  [{:02}] BAD  {rule}", index + 1);
                    failures.push((
                        (*id).to_string(),
                        index + 1,
                        rule.clone(),
                        error.to_string(),
                    ));
                }
            }
        }
        // 整条链也要能建起来（`Projection::from_rules` 走的是同一个 `Calculus`）
        if !effective.rules.is_empty() {
            Projection::from_rules(&effective.rules).unwrap_or_else(|error| {
                panic!("{id} 的整条 algebra 链（`Projection::from_rules`）建不起来：{error}")
            });
        }
    }
    println!(
        "\n合计：{total} 条规则（按方案各算一遍；去重 {} 条），{compiled} 条可编译，{} 条不支持；\
         {read_sources}/{} 份来源已读到",
        unique.len(),
        failures.len(),
        SPECIFIED_SCHEMAS.len()
    );
    if total == 0 {
        println!("这 4 份来源里没有可执行的规则（文件都不在，或 algebra 都是空的）——跳过，不失败");
        return;
    }
    if !failures.is_empty() {
        let mut message = format!(
            "真实规则里有 {} 条用了 regex 子集**不支持**的语法（逐条原文）：\n",
            failures.len()
        );
        for (source, index, rule, error) in &failures {
            message.push_str(&format!(
                "- {source} 第 {index} 条：`{rule}`\n  诊断：{error}\n"
            ));
        }
        panic!("{message}");
    }
    assert_eq!(compiled, total, "可编译条数应当等于总条数");
}

// ---------------------------------------------------------------------------
// 2) 单条规则的稳定输出（"改了就知道"的钉子）
// ---------------------------------------------------------------------------

/// `(规则原文, [(输入, 期望输出)])`；`是否被改` 由 `期望输出 != 输入` 推出并一起断言
/// （`Calculation::apply` 的返回值必须与实际改动一致）。
const RULE_PINS: &[(&str, &[(&str, &str)])] = &[
    (
        // yuling 唯一的一条：第二码用 z 通配
        "derive/^([a-y])([a-y])/$1z/",
        &[("aa", "az"), ("az", "az"), ("za", "za"), ("zz", "zz")],
    ),
    (
        // 自成一音节：ang → aang，再到 (.)ang$ 那步变 ah
        "xform/^([aoe])(ng)?$/$1$1$2/",
        &[
            ("ang", "aang"),
            ("eng", "eeng"),
            ("a", "aa"),
            ("e", "ee"),
            ("ong", "oong"),
            ("ing", "ing"),
        ],
    ),
    (
        "derive/^([aoe])([ioun])$/$1$1$2/",
        &[
            ("ai", "aai"),
            ("an", "aan"),
            ("ao", "aao"),
            ("ei", "eei"),
            ("en", "een"),
            ("er", "er"),
        ],
    ),
    (
        "xform/(.)ei$/$1Z/",
        &[("bei", "bZ"), ("fei", "fZ"), ("wei", "wZ"), ("ei", "ei")],
    ),
    (
        "xform/i?ong$/S/",
        &[("ong", "S"), ("iong", "S"), ("dong", "dS"), ("xiong", "xS")],
    ),
    (
        "xform/ing$|uai$/Y/",
        &[("bing", "bY"), ("kuai", "kY"), ("bang", "bang")],
    ),
    (
        "xform/(^|[ '])i/$1ch/",
        &[("i", "ch"), ("a i", "a ch"), ("shi", "shi")],
    ),
    (
        "abbrev/^([a-z]).+$/$1/",
        &[("zhang", "z"), ("shang", "s"), ("a", "a")],
    ),
    (
        "abbrev/^([zcs]h).+$/$1/",
        &[
            ("zhang", "zh"),
            ("shang", "sh"),
            ("chang", "ch"),
            ("bang", "bang"),
        ],
    ),
    (
        "derive/^([nl])ue$/$1ve/",
        &[("nue", "nve"), ("lue", "lve"), ("jue", "jue")],
    ),
    (
        "derive/^([jqxy])u/$1v/",
        &[
            ("ju", "jv"),
            ("qu", "qv"),
            ("xu", "xv"),
            ("yu", "yv"),
            ("zu", "zu"),
        ],
    ),
    (
        "xform/[uv]e$/T/",
        &[("lve", "lT"), ("lue", "lT"), ("jue", "jT"), ("la", "la")],
    ),
    (
        "xform/^zh/V/",
        &[
            ("zhang", "Vang"),
            ("zha", "Va"),
            ("zh", "V"),
            ("zang", "zang"),
        ],
    ),
    (
        "xlit/QWRTYUIOPSDFGHJKLZXCVBNM/qwrtyuiopsdfghjklzxcvbnm/",
        &[("QWR", "qwr"), ("ZFURO", "zfuro"), ("abc", "abc")],
    ),
];

#[test]
fn algebra_real_rules_single_outputs_are_pinned() {
    for (rule, cases) in RULE_PINS {
        for (input, expected) in *cases {
            let (output, changed) = apply(rule, input);
            println!("  `{rule}`  `{input}` -> `{output}`（changed={changed}）");
            assert_eq!(
                output, *expected,
                "规则 `{rule}` 作用于 `{input}` 的结果变了"
            );
            assert_eq!(
                changed,
                output != *input,
                "规则 `{rule}` 作用于 `{input}`：apply 返回值与实际改动不一致"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 3) 整套规则的稳定输出（拼写全集）
// ---------------------------------------------------------------------------

/// `(schema id, [(输入音节, 期望拼写全集)])`。
const SCRIPT_PINS: &[(&str, &[(&str, &[&str])])] = &[
    (
        "yuling",
        &[
            ("aa", &["aa", "az"]),
            ("ab", &["ab", "az"]),
            ("az", &["az"]),
            ("zz", &["zz"]),
        ],
    ),
    (
        "double_pinyin_ziranma",
        &[
            ("zhang", &["vh"]),
            ("ang", &["ah"]),
            ("kuai", &["ky"]),
            ("iong", &["s"]),
            ("a", &["aa"]),
            ("er", &["er"]),
            ("lve", &["lt"]),
            ("nv", &["nv"]),
        ],
    ),
    (
        "double_pinyin_flypy",
        &[
            ("zhang", &["vh"]),
            ("kuai", &["kk"]),
            ("ong", &["os"]),
            ("shuang", &["ul"]),
        ],
    ),
    (
        "yuhao_pinyin",
        &[
            ("zhang", &["z", "zh", "zhang"]),
            ("nue", &["n", "nue", "nve"]),
            ("ju", &["j", "ju", "jv"]),
            ("a", &["a"]),
        ],
    ),
    (
        "yuhao_quanpin",
        &[
            ("nue", &["nue", "nve"]),
            ("ju", &["ju", "jv"]),
            ("nue2", &["nue2"]),
        ],
    ),
];

#[test]
fn algebra_real_rules_script_outputs_are_pinned() {
    for (id, cases) in SCRIPT_PINS {
        let schema = REAL_SCHEMAS
            .iter()
            .find(|schema| schema.id == *id)
            .unwrap_or_else(|| panic!("没有 {id} 的规则表"));
        for (input, expected) in *cases {
            let expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
            let actual = spellings_of(schema.algebra, input);
            println!("  {id:24} `{input}` -> {actual:?}");
            assert_eq!(actual, expected, "{id} 对 `{input}` 的拼写集变了");
        }
    }
}

// ---------------------------------------------------------------------------
// 4) 磁盘上的真实方案 = 常量（文件不在就跳过）
// ---------------------------------------------------------------------------

/// `%APPDATA%\Rime-Next\schema`，不存在就 `None`（非 Windows / 干净机器）。
fn schema_root() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    let mut path = PathBuf::from(appdata);
    path.push("Rime-Next");
    path.push("schema");
    path.is_dir().then_some(path)
}

fn skip(reason: &str) {
    println!("跳过：{reason}（本用例只在本机有真实方案时才有意义）");
}

/// 读一份方案文件**自己声明**的 `speller/algebra`；文件不在返回 `None`。
///
/// 两种写法都要认：
/// * `speller: algebra: [...]` —— 普通写法（yuling / 两个双拼 / yuhao_pinyin）；
/// * `speller/+: algebra: [...]` —— 补丁式写法（yuhao_quanpin 先
///   `__include: yuhao_pinyin.schema:/`，再用 `speller/+` 覆盖）。
///   `edit_node` 对**列表**是整体替换（不是追加），所以补丁里的列表就是最终值；
///   这一点用 `build/yuhao_quanpin.schema.yaml` 的产物核对过（只剩这两条）。
fn live_algebra(root: &Path, relative_path: &str) -> Option<Vec<String>> {
    let path = root.join(relative_path);
    if !path.is_file() {
        return None;
    }
    let tree = librime_next::load_file(&path)
        .unwrap_or_else(|error| panic!("读不了 {}: {error}", path.display()));
    let base = tree
        .get("speller")
        .and_then(|speller| speller.get("algebra"));
    let patched = tree
        .get("speller/+")
        .and_then(|speller| speller.get("algebra"));
    match patched.or(base) {
        None => Some(Vec::new()),
        Some(value) => Some(
            value
                .as_array()
                .unwrap_or_else(|| panic!("{} 的 algebra 不是列表", path.display()))
                .iter()
                .map(|item| {
                    item.as_str()
                        .unwrap_or_else(|| panic!("{} 的规则不是字符串", path.display()))
                        .to_string()
                })
                .collect(),
        ),
    }
}

/// `__include: "yuling.schema:/"` → `"yuling.schema"`（只要文件名主干）。
///
/// 只处理**文件级** include（`yuling.schema:/`）；带子路径的写法
/// （`yuling.schema:/speller/algebra`）这里用不到，也不装作认识。
fn include_stem(include: &str) -> Option<&str> {
    let stem = include.split(':').next()?.trim();
    (!stem.is_empty()).then(|| stem.trim_end_matches('/'))
}

/// 在 `schema/` 下找 `<stem>.yaml`：先 `schema/<id>/`，再 `schema/` 本身。
///
/// Rime 的方案布局是 `schema/<id>/<id>.schema.yaml`；这里只扫**一层**子目录，
/// 不碰 `build/`，也不依赖 `default.yaml` 的 `schema_list`。
fn find_schema_file(root: &Path, stem: &str) -> Option<PathBuf> {
    let file_name = format!("{stem}.yaml");
    let direct = root.join(&file_name);
    if direct.is_file() {
        return Some(direct);
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .map(|dir| dir.join(&file_name))
        .find(|path| path.is_file())
}

/// 从配置树里取一条规则列表（`"speller/algebra"`）；键不存在返回 `None`。
fn rule_list(tree: &serde_json::Value, path: &str) -> Option<Vec<String>> {
    let node = get_path(tree, path)?;
    let array = node.as_array().unwrap_or_else(|| panic!("{path} 不是列表"));
    Some(
        array
            .iter()
            .map(|item| {
                item.as_str()
                    .unwrap_or_else(|| panic!("{path} 里有非字符串规则"))
                    .to_string()
            })
            .collect(),
    )
}

/// 读一份方案**生效**的 algebra。
///
/// 合并顺序对应 Rime 的语义（`__include` 是**深合并**，本地键覆盖继承值）：
/// 1. 自己写的 `speller: algebra:` / `speller/+: algebra:` **优先** —— 有就不看
///    `__include`。`double_pinyin_ziranma` 就属于这种：它 `__include` 了
///    `luna_pinyin_simp.schema:/`（为了 engine/translator 骨架），但 algebra 自己写；
/// 2. 自己没写才跟着 `__include: <id>.schema:/` 走（最多 4 层防环），
///    `yuling_extreme` 靠这条继承 `yuling` 的那 1 条；
/// 3. 两者都没有 = 0 条（`yuling_chaifen`）。
///
/// 自己没写 algebra、而 `__include` 指向的方案本机不存在时，规则**无法判定**：
/// 记在 [`EffectiveAlgebra::unresolved_include`] 里由调用方打印，不冒充 0 条。
///
/// 文件不在返回 `None`（调用方打印跳过）；文件在但没有 algebra 返回空 `rules`。
fn effective_algebra(root: &Path, relative_path: &str) -> Option<EffectiveAlgebra> {
    fn walk(root: &Path, relative_path: &str, depth: usize) -> EffectiveAlgebra {
        let path = root.join(relative_path);
        let tree = librime_next::load_file(&path)
            .unwrap_or_else(|error| panic!("读不了 {}: {error}", path.display()));
        let mut effective = EffectiveAlgebra::default();
        // 1) 自己的 algebra 优先；`speller/+` 的列表是整体替换（Rime 的 edit_node 语义）
        if let Some(own) =
            rule_list(&tree, "speller/+/algebra").or_else(|| rule_list(&tree, "speller/algebra"))
        {
            effective.rules = own;
            return effective;
        }
        // 2) 自己没写，才看 `__include`
        let Some(stem) = tree
            .get("__include")
            .and_then(|node| node.as_str())
            .and_then(include_stem)
        else {
            return effective; // 3) 两者都没有 = 0 条
        };
        if depth >= 4 {
            effective.unresolved_include = Some(format!("{stem}.yaml（继承层数过深，疑成环）"));
            return effective;
        }
        let Some(target) = find_schema_file(root, stem) else {
            effective.unresolved_include = Some(format!("{stem}.yaml"));
            return effective;
        };
        let relative = target
            .strip_prefix(root)
            .unwrap_or_else(|_| panic!("{} 不在 {} 下", target.display(), root.display()))
            .to_string_lossy()
            .replace('\\', "/");
        let inherited = walk(root, &relative, depth + 1);
        effective.rules = inherited.rules;
        effective.inherited_from = Some(relative);
        effective.unresolved_include = inherited.unresolved_include;
        effective
    }
    if !root.join(relative_path).is_file() {
        return None;
    }
    Some(walk(root, relative_path, 0))
}

#[test]
fn algebra_real_rules_live_matches_embedded_tables() {
    let Some(root) = schema_root() else {
        return skip("没有 %APPDATA%\\Rime-Next\\schema");
    };
    let calc = Calculus::new();
    for schema in REAL_SCHEMAS {
        let Some(live) = live_algebra(&root, schema.relative_path) else {
            skip(schema.relative_path);
            continue;
        };
        assert_eq!(
            live,
            rule_strings(schema.algebra),
            "{} 的 speller/algebra 与测试里的常量不一致：方案改过就把常量同步过来\
             （并确认新规则能被编译）",
            schema.id
        );
        // 磁盘上的每一条再单独编译一次（防止"常量对但解析器退化成别的意思"）
        for rule in &live {
            calc.try_parse(rule)
                .unwrap_or_else(|error| panic!("{} 的磁盘规则编译失败：{error}", schema.id));
        }
    }
}

#[test]
fn algebra_real_rules_chaifen_and_extreme_have_no_own_algebra() {
    let Some(root) = schema_root() else {
        return skip("没有 %APPDATA%\\Rime-Next\\schema");
    };
    for relative_path in [
        "yuling_chaifen/yuling_chaifen.schema.yaml",
        "yuling_extreme/yuling_extreme.schema.yaml",
    ] {
        let Some(algebra) = live_algebra(&root, relative_path) else {
            skip(relative_path);
            continue;
        };
        assert!(
            algebra.is_empty(),
            "{relative_path} 本不该自带 speller/algebra\
             （yuling_extreme 用 `__include: yuling.schema:/` 继承 yuling 的那一条），实际：{algebra:?}"
        );
    }
}

fn get_path<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut node = value;
    for segment in path.split('/') {
        node = node.get(segment)?;
    }
    Some(node)
}

fn walk_yaml(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut items: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    items.sort();
    for item in items {
        if item.is_dir() {
            walk_yaml(&item, out);
        } else if item.extension().and_then(|extension| extension.to_str()) == Some("yaml") {
            out.push(item);
        }
    }
}

/// 把 `schema/` 下**所有**方案文件里走 `Calculus` 的规则都编译一遍
/// （`speller/algebra` + 各种 `*_format`）。
///
/// 比 [`algebra_real_rules_live_matches_embedded_tables`] 宽：那个只对 5 套搬进常量的方案
/// 逐字节比对，这个把目录里所有方案（含用户新加的）扫一遍，防止"新方案里出现一条
/// 子集不支持的写法"没人发现。本机实测 58 份方案文件 / 163 条规则 / **0 条不支持**。
#[test]
fn algebra_real_rules_all_live_schema_rules_compile() {
    let Some(root) = schema_root() else {
        return skip("没有 %APPDATA%\\Rime-Next\\schema");
    };
    let mut files = Vec::new();
    walk_yaml(&root, &mut files);
    let calc = Calculus::new();
    let mut total = 0usize;
    for path in &files {
        let Ok(tree) = librime_next::load_file(path) else {
            continue;
        };
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("?");
        let mut rules: Vec<String> = Vec::new();
        for key in [
            "speller/algebra",
            "speller/+/algebra",
            "translator/preedit_format",
            "translator/comment_format",
            "reverse_lookup/preedit_format",
            "reverse_lookup/comment_format",
            "preedit_format",
            "comment_format",
        ] {
            let Some(node) = get_path(&tree, key) else {
                continue;
            };
            let Some(array) = node.as_array() else {
                continue;
            };
            for item in array {
                if let Some(rule) = item.as_str() {
                    // 只有"像规则"的串才走 Calculus（这些字段里也可能放说明文字）
                    if rule.contains('/') || rule.starts_with("reorder") {
                        rules.push(rule.to_string());
                    }
                }
            }
        }
        for rule in &rules {
            calc.try_parse(rule)
                .unwrap_or_else(|error| panic!("{name} 里的规则编译失败（{error}）"));
        }
        total += rules.len();
    }
    assert!(
        total > 0,
        "在 {} 下一份规则都没读到：APPDATA 布局可能变了",
        root.display()
    );
    println!(
        "扫过 {} 份方案文件，共 {total} 条规则，全部可编译",
        files.len()
    );
}

// ---------------------------------------------------------------------------
// 5) 曾经"子集不支持"的两条真实写法：换 `fancy-regex` 后必须**可用**
// ---------------------------------------------------------------------------

/// `recognizer/patterns` 里那两条真实正则：`(?!)` 环视与 `+?` 懒惰量词。
///
/// 2026-10-25 之前用 `regex` crate，这条测试钉的是"必须编译失败"；换成 `fancy-regex`
/// 之后要钉的翻成反面：**必须编译成功、而且真的能区分该匹配与不该匹配的串**。
/// 谁把这层改回"静默拒绝"，这条会炸。
///
/// 语义断言不依赖 `%APPDATA%`；只有"磁盘上的真实写法也能编译"这一段需要它。
#[test]
fn algebra_real_rules_fancy_patterns_compile_and_match() {
    // ---- 1) 语义基线（不依赖磁盘） --------------------------------------
    let uppercase = Regex::new("^(?![`;]).*[A-Z][-_+.'0-9A-Za-z]*$")
        .expect("环视写法必须能编译（fancy-regex）");
    assert!(uppercase.is_full_match("ABC"));
    assert!(!uppercase.is_full_match("`ABC"), "环视必须挡住反引号开头");
    assert!(!uppercase.is_full_match(";ABC"), "环视必须挡住分号开头");
    assert!(!uppercase.is_full_match("abc"), "没有大写字母就不该命中");

    let reverse_lookup =
        Regex::new("^z([a-z]+?)*$").expect("懒惰量词写法必须能编译（fancy-regex）");
    assert!(reverse_lookup.is_full_match("zabc"));
    assert!(!reverse_lookup.is_full_match("abc"), "z 开头才命中");

    // ---- 2) 磁盘上的真实写法（没有 %APPDATA% 就跳过这一段） -------------
    let Some(root) = schema_root() else {
        return skip("没有 %APPDATA%\\Rime-Next\\schema");
    };
    let path = root.join("yuling/yuling.schema.yaml");
    if !path.is_file() {
        return skip("yuling.schema.yaml");
    }
    let tree = librime_next::load_file(&path)
        .unwrap_or_else(|error| panic!("读不了 {}: {error}", path.display()));
    let patterns = tree
        .get("recognizer")
        .and_then(|recognizer| recognizer.get("patterns"))
        .and_then(|patterns| patterns.as_object())
        .unwrap_or_else(|| panic!("{} 里没有 recognizer/patterns", path.display()))
        .clone();

    // (键, 基线写法)
    let expected = [
        ("uppercase", "^(?![`;]).*[A-Z][-_+.'0-9A-Za-z]*$"),
        ("reverse_lookup", "^z([a-z]+?)*$"),
    ];
    for (key, canonical) in expected {
        let Some(pattern) = patterns.get(key).and_then(|value| value.as_str()) else {
            skip(&format!("recognizer/patterns/{key}"));
            continue;
        };
        if pattern != canonical {
            // 用户改过这条正则：只报告，不假装还认识
            println!("注意：recognizer/patterns/{key} 已不是基线写法：{pattern}");
        }
        if let Err(error) = Regex::new(pattern) {
            panic!("recognizer/patterns/{key} 必须能编译（已换 fancy-regex）：{error}");
        }
    }
}
