//! Ctrl+` 折叠式方案选单探针（2026-11-13 20:3x）。
//!
//! 背景：`Ctrl+`` 的方案选单从「下钻式两级菜单」改成「内联折叠列表」——
//! 一级行 = 各方案，主方案（拼音）行尾 `▾` 可展开，14 个输入类型（全拼/自然码/…）
//! 缩进列在下面；同时引擎要**同时认** `input_type:`（单数，live 数据实际写法）与
//! `input_types:`（复数，历史写法）。
//!
//! 本探针不打开候选窗、不依赖宿主：直接拿 live **编译产物**（`<user>/build`
//! 通常是指向 `%APPDATA%\Rime-Next\build` 的 junction）+ live **共享数据目录**，
//! 用真实 `Engine::process_key` 跑完整个交互，逐条断言：
//!
//! ① 菜单第一项 = `<方案名> ▾`、comment = 当前输入类型；紧接着 14 个全角空格缩进的
//!    输入类型子项（文案/顺序与方案 `input_type.options` 一致，「当前」只标一项）；
//! ② Down **按下**只走 1 条、Down **松开**不动（TIP 会把 keyup 也送进引擎，
//!    2026-11-08 实机「按一次跳 2 条」就是这个双步回归）；
//! ③ Page Down 步长 = `context.page_size`（= 方案 `menu/page_size`，live 是 6）；
//! ④ Tab 在分组根行收起/展开；Esc 先收折叠、再按才关菜单；
//! ⑤ 选「自然码」= 只切输入类型 option、`schema_id` 不变、不登记切方案；
//! ⑥ 重开菜单时父行 comment / 子行「当前」跟着变成自然码。
//!
//! 用法（`user_root` 必须是**临时副本**：引擎会写 `<user>/user.yaml` 的
//! `var/previously_selected_schema`，不能拿 live 用户目录直接跑）：
//!
//! ```text
//! probe_collapse_menu <user_root> <shared_root>
//! ```
//!
//! 例：
//! ```text
//! target\release\examples\probe_collapse_menu.exe C:\Temp\probe_user "C:\Program Files\Rime-Next\Weasel-Next\data"
//! ```
//!
//! 2026-11-13 20:3x 新建（上一棒只写了单测 916 全绿，没有对 live 数据的无头验证）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use serde_json::Value;

use librime_next::engine::key_event::{
    KeyEvent, K_RELEASE_MASK, XK_DOWN, XK_ESCAPE, XK_NEXT, XK_PRIOR, XK_SPACE, XK_TAB, XK_UP,
};
use librime_next::engine::runtime::Engine;
use librime_next::gear::schema::Schema;

/// 折叠列表子项缩进（与 `schema_list_translator::CHILD_INDENT` 同值：全角空格）。
const CHILD_INDENT: &str = "\u{3000}";
const ARROW_OPEN: &str = " ▾";
const ARROW_CLOSED: &str = " ▸";

/// 菜单一行（kind / text / comment）。
#[derive(Debug, Clone)]
struct Row {
    kind: String,
    text: String,
    comment: String,
}

fn menu_rows(eng: &Engine) -> Vec<Row> {
    eng.context
        .composition
        .segmentation
        .segments
        .last()
        .and_then(|segment| segment.menu.as_ref())
        .map(|menu| {
            menu.all_candidates()
                .into_iter()
                .map(|candidate| Row {
                    kind: candidate.kind,
                    text: candidate.text,
                    comment: candidate.comment,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn selected_index(eng: &Engine) -> usize {
    eng.context
        .composition
        .segmentation
        .segments
        .last()
        .map_or(0, |segment| segment.selected_index)
}

/// 按一下（无修饰）。
fn press(eng: &mut Engine, keycode: u32) -> bool {
    eng.process_key(&KeyEvent::new(keycode, 0))
}

/// 松开一下（带 Release 位，模拟 TIP 把 keyup 也送进引擎）。
fn release(eng: &mut Engine, keycode: u32) -> bool {
    eng.process_key(&KeyEvent::new(keycode, K_RELEASE_MASK))
}

/// 与 Switcher 解析 hotkeys 完全相同的方式拿 Ctrl+`（配置里写的是 `Control+grave`）。
fn hotkey() -> KeyEvent {
    KeyEvent::parse("Control+grave").expect("Control+grave 必须能解析")
}

/// 与引擎 `dict_compiler::input_types_node` + `collect_input_type_options` 同口径地
/// 从**引擎自己加载的**方案配置里读「应出现的输入类型（option 名, 中文 label）」。
///
/// 只用于算期望值；引擎侧真读没读到（单复数断点回归）时，菜单里不会有这些子项，
/// 本函数算出的期望会让断言失败 —— 这正是我们要的回归判据。
fn expected_input_types(config: &Value) -> Option<Vec<(String, String)>> {
    // 两键都认；优先带 options 的节点（保住中文文案与顺序）。
    let plural = config.get("input_types").filter(|v| v.is_object());
    let singular = config.get("input_type").filter(|v| v.is_object());
    let node = if plural.is_some_and(|n| n.get("options").is_some()) {
        plural
    } else if singular.is_some_and(|n| n.get("options").is_some()) {
        singular
    } else {
        plural.or(singular)
    }?;

    let labels: HashMap<String, String> = node
        .get("options")
        .and_then(Value::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|entry| {
                    let option = entry.get("option").and_then(Value::as_str)?;
                    let label = entry
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or(option);
                    Some((option.to_string(), label.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    let order: Vec<String> = node
        .get("options")
        .and_then(Value::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|entry| {
                    entry
                        .get("option")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    let enabled: Vec<String> = node
        .get("enabled")
        .and_then(Value::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let mut out: Vec<(String, String)> = Vec::new();
    // ① options 顺序 ∩ enabled
    for name in &order {
        if enabled.iter().any(|item| item == name) {
            let label = labels.get(name).cloned().unwrap_or_else(|| name.clone());
            out.push((name.clone(), label));
        }
    }
    // ② enabled 里 options 没提到的，按字母序补在后面（引擎同口径）
    let mut rest: Vec<String> = enabled
        .iter()
        .filter(|name| !order.iter().any(|item| item == *name))
        .cloned()
        .collect();
    rest.sort();
    for name in rest {
        out.push((name.clone(), name));
    }
    Some(out)
}

fn main() {
    let user = PathBuf::from(std::env::args().nth(1).expect("user_root"));
    let shared = PathBuf::from(std::env::args().nth(2).expect("shared_root"));
    let build = user.join("build");
    let roots = vec![user.clone(), shared.clone()];

    println!("user   = {}", user.display());
    println!("shared = {}", shared.display());
    println!("build  = {}", build.display());
    assert!(
        build.join("default.yaml").is_file(),
        "{} 下没有 default.yaml —— user/build 应是指向 %APPDATA%\\Rime-Next\\build 的 junction",
        build.display()
    );
    let hotkey = hotkey();
    println!("热键   = {}（{:?}）", hotkey.repr(), hotkey);

    let mut failures = 0usize;
    let mut check = |name: &str, ok: bool| {
        if ok {
            println!("  OK   {name}");
        } else {
            failures += 1;
            println!("  FAIL {name}");
        }
    };
    let check_eq = |name: &str, actual: &str, expected: &str, failures: &mut usize| {
        if actual == expected {
            println!("  OK   {name}");
        } else {
            *failures += 1;
            println!("  FAIL {name}\n         实际 = {actual:?}\n         期望 = {expected:?}");
        }
    };

    // ── 0. 方案数据 & 引擎 ────────────────────────────────────────────────
    let Some(schema) = Schema::open(&build, "pinyin") else {
        eprintln!("Schema::open(pinyin) 失败：{}", build.display());
        std::process::exit(2);
    };
    let schema_name = schema.schema_name.clone();
    let schema_page_size = schema.page_size;
    println!("\n=== 0. 环境 ===");
    println!(
        "方案 pinyin：name={schema_name:?} menu/page_size={schema_page_size}（编译产物）"
    );

    let Some(mut eng) = Engine::create_with_roots(&build, "pinyin", &roots) else {
        eprintln!("Engine::create_with_roots 失败");
        std::process::exit(2);
    };
    // 与 live 启动路径一致：apply_schema 会把 context.page_size 落成方案值。
    let applied = eng.apply_schema("pinyin");
    check("apply_schema(pinyin)", applied);
    println!(
        "context.page_size = {}（期望等于方案的 menu/page_size）",
        eng.context.page_size
    );
    check_eq(
        "context.page_size == 方案 menu/page_size",
        &eng.context.page_size.to_string(),
        &schema_page_size.to_string(),
        &mut failures,
    );

    let singular = eng.schema.config.get("input_type").is_some();
    let plural = eng.schema.config.get("input_types").is_some();
    println!(
        "方案顶层键：input_type（单数）={singular} / input_types（复数）={plural}"
    );
    check(
        "live 编译产物确实是单数 input_type 写法（本棒核心断点）",
        singular && !plural,
    );
    let expected = expected_input_types(&eng.schema.config).unwrap_or_default();
    println!("方案声明的可用输入类型 {} 项：{:?}", expected.len(),
        expected.iter().map(|(_, label)| label.as_str()).collect::<Vec<_>>());
    check("输入类型清单非空（单复数兼容层生效）", expected.len() >= 14);

    // 打开菜单的耗时（TIP 每键预算 180ms，菜单展开必须远低于它）。
    let started = Instant::now();
    let eaten = eng.process_key(&hotkey);
    let open_ms = started.elapsed().as_secs_f64() * 1000.0;
    println!("\n=== 1. Ctrl+` 打开折叠菜单（eaten={eaten} {open_ms:.1}ms）===");
    check("Ctrl+` 被引擎吃掉", eaten);
    let rows = menu_rows(&eng);
    print_rows("菜单", &rows);
    check_eq(
        "第 0 行 = 方案名 + ▾（默认展开当前方案）",
        &rows.first().map(|r| r.text.clone()).unwrap_or_default(),
        &format!("{schema_name}{ARROW_OPEN}"),
        &mut failures,
    );
    check_eq(
        "第 0 行 comment = 当前输入类型（全拼）",
        &rows.first().map(|r| r.comment.clone()).unwrap_or_default(),
        "全拼",
        &mut failures,
    );
    check_eq("打开后高亮在第 0 行", &selected_index(&eng).to_string(), "0", &mut failures);
    check(
        "第 0 行 kind = schema",
        rows.first().is_some_and(|r| r.kind == "schema"),
    );

    // 14 个缩进子项：文案/顺序与 input_type.options 一致。
    let mut child_mismatch = Vec::new();
    for (offset, (name, label)) in expected.iter().enumerate() {
        let row = rows.get(1 + offset);
        let want = format!("{CHILD_INDENT}{label} （option={name}）");
        let actual = row
            .map(|r| format!("{} （kind={} comment={:?}）", r.text, r.kind, r.comment))
            .unwrap_or_else(|| "<缺行>".to_string());
        if row.map(|r| r.text.as_str()) != Some(format!("{CHILD_INDENT}{label}").as_str())
            || row.map(|r| r.kind.as_str()) != Some("switch")
        {
            child_mismatch.push(format!("第 {} 行 {want} 实际是 {actual}", 2 + offset));
        }
    }
    check(
        &format!(
            "缩进子项 {} 项文案/顺序与 options 一致",
            expected.len()
        ),
        child_mismatch.is_empty(),
    );
    for line in &child_mismatch {
        println!("         {line}");
    }
    let current_marked = rows
        .iter()
        .skip(1)
        .take(expected.len())
        .enumerate()
        .filter(|(_, row)| row.comment == "当前")
        .map(|(index, _)| expected[index].1.clone())
        .collect::<Vec<_>>();
    check_eq(
        "「当前」只标在默认输入类型（全拼）上",
        &format!("{current_marked:?}"),
        "[\"全拼\"]",
        &mut failures,
    );
    let schema_rows = rows.iter().filter(|r| r.kind == "schema").count();
    check_eq("菜单里一级方案行 = 12（当前 + 其余 11）", &schema_rows.to_string(), "12", &mut failures);

    // ── 2. Down 按下只走 1 条；Down 松开不动（双步回归）──────────────────
    println!("\n=== 2. Down 按下/松开（TIP 会送 keyup）===");
    let before = selected_index(&eng);
    let down_press = press(&mut eng, XK_DOWN);
    let after_press = selected_index(&eng);
    check("Down 按下被吃掉", down_press);
    check_eq("Down 按下只走 1 条", &after_press.to_string(),
        &(before + 1).to_string(), &mut failures);
    let down_release = release(&mut eng, XK_DOWN);
    let after_release = selected_index(&eng);
    check("Down 松开被吃掉（不落到应用）", down_release);
    check_eq("Down 松开不动（一次物理按键 = 一步）", &after_release.to_string(),
        &after_press.to_string(), &mut failures);
    let down_press2 = press(&mut eng, XK_DOWN);
    let after_press2 = selected_index(&eng);
    check("再按一次 Down 被吃掉", down_press2);
    check_eq("再按一次 Down 再走 1 条", &after_press2.to_string(),
        &(after_press + 1).to_string(), &mut failures);
    let up_press = press(&mut eng, XK_UP);
    let _ = up_press;
    release(&mut eng, XK_UP);
    check_eq("Up 按下走回 1 条", &selected_index(&eng).to_string(),
        &"1".to_string(), &mut failures);

    // ── 3. Page Down 步长 = context.page_size ─────────────────────────────
    println!("\n=== 3. Page Down 步长（context.page_size）===");
    let press_page = press(&mut eng, XK_NEXT);
    let page_size = eng.context.page_size;
    let expected_index = (1 + page_size).min(rows.len() - 1);
    check("PageDown 被吃掉", press_page);
    check_eq(
        "PageDown 走 1 页（= context.page_size）",
        &selected_index(&eng).to_string(),
        &expected_index.to_string(),
        &mut failures,
    );
    release(&mut eng, XK_NEXT);
    check_eq(
        "PageDown 松开不动",
        &selected_index(&eng).to_string(),
        &expected_index.to_string(),
        &mut failures,
    );
    press(&mut eng, XK_PRIOR);
    release(&mut eng, XK_PRIOR);
    // 回到第 0 行（PageUp 落点 = 1，再 Up 一次）。
    press(&mut eng, XK_UP);
    release(&mut eng, XK_UP);
    check_eq("回到第 0 行", &selected_index(&eng).to_string(), "0", &mut failures);

    // ── 4. Tab 收起 / 展开；Esc 先收折叠再关菜单 ─────────────────────────
    println!("\n=== 4. Tab 收起/展开、Esc 两级退出 ===");
    let tab1 = press(&mut eng, XK_TAB);
    check("Tab（分组根行）被吃掉", tab1);
    let collapsed = menu_rows(&eng);
    print_rows("Tab 收起后", &collapsed);
    check_eq(
        "Tab 收起：第 0 行变 ▸",
        &collapsed.first().map(|r| r.text.clone()).unwrap_or_default(),
        &format!("{schema_name}{ARROW_CLOSED}"),
        &mut failures,
    );
    check_eq(
        "Tab 收起：缩进子项全部隐藏",
        &collapsed.iter().filter(|r| r.kind == "switch").count().to_string(),
        "0",
        &mut failures,
    );
    check_eq(
        "Tab 收起：高亮留在分组根行",
        &selected_index(&eng).to_string(),
        "0",
        &mut failures,
    );
    let tab2 = press(&mut eng, XK_TAB);
    check("再按 Tab 被吃掉", tab2);
    let expanded = menu_rows(&eng);
    check_eq(
        "再按 Tab 重新展开（▾ + 14 子项）",
        &format!(
            "{:?}",
            (
                expanded.first().map(|r| r.text.clone()).unwrap_or_default(),
                expanded.iter().filter(|r| r.kind == "switch").count()
            )
        ),
        &format!("{:?}", (format!("{schema_name}{ARROW_OPEN}"), expected.len())),
        &mut failures,
    );

    let esc1 = press(&mut eng, XK_ESCAPE);
    let after_esc1 = menu_rows(&eng);
    check("Esc 第一下被吃掉", esc1);
    check("Esc 第一下：菜单还开着", !after_esc1.is_empty());
    check_eq(
        "Esc 第一下：先收起折叠（▸）",
        &after_esc1.first().map(|r| r.text.clone()).unwrap_or_default(),
        &format!("{schema_name}{ARROW_CLOSED}"),
        &mut failures,
    );
    let esc2 = press(&mut eng, XK_ESCAPE);
    let after_esc2 = menu_rows(&eng);
    check("Esc 第二下被吃掉", esc2);
    check("Esc 第二下：菜单关闭（无菜单行）", after_esc2.is_empty());

    // ── 5. 选「自然码」= 只切输入类型，不换方案 ──────────────────────────
    println!("\n=== 5. 选「自然码」→ 只设 option、schema_id 不变 ===");
    let reopened = eng.process_key(&hotkey);
    let rows = menu_rows(&eng);
    check("重开菜单", reopened);
    check(
        &format!("重开菜单后 2 行是「{}」（第 2 行）", "自然码"),
        rows.get(2).map(|r| r.text.as_str()) == Some(format!("{CHILD_INDENT}自然码").as_str()),
    );
    press(&mut eng, XK_DOWN);
    press(&mut eng, XK_DOWN);
    check_eq("高亮移到「自然码」行", &selected_index(&eng).to_string(), "2", &mut failures);
    let space = press(&mut eng, XK_SPACE);
    check("空格确认被吃掉", space);
    check("确认后菜单关闭", menu_rows(&eng).is_empty());
    check_eq("schema_id 仍是 pinyin（没有切方案）", &eng.schema.schema_id, "pinyin", &mut failures);
    check(
        "没有登记切方案请求",
        eng.pending_schema().is_none(),
    );
    check("自然码 option = true", eng.context.get_option("zrm"));
    check("全拼 option = false（互斥单选）", !eng.context.get_option("pinyin"));

    // ── 6. 重开菜单：父行 comment / 子行「当前」跟随新输入类型 ────────────
    println!("\n=== 6. 重开菜单：当前输入类型跟随（全拼 → 自然码）===");
    let _ = eng.process_key(&hotkey);
    let rows = menu_rows(&eng);
    check_eq(
        "父行 comment = 自然码",
        &rows.first().map(|r| r.comment.clone()).unwrap_or_default(),
        "自然码",
        &mut failures,
    );
    let marked = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.comment == "当前")
        .map(|(i, _)| i)
        .collect::<Vec<_>>();
    check_eq("只有一行标「当前」", &format!("{marked:?}"), "[2]", &mut failures);
    check_eq(
        "「当前」行是自然码子项",
        &rows.get(2).map(|r| r.text.clone()).unwrap_or_default(),
        &format!("{CHILD_INDENT}自然码"),
        &mut failures,
    );
    press(&mut eng, XK_ESCAPE);
    release(&mut eng, XK_ESCAPE);

    // ── 收尾 ─────────────────────────────────────────────────────────────
    println!("\n=== 汇总 ===");
    if failures == 0 {
        println!("ALL PASS：折叠列表 + 单数 input_type 兼容 + release 守卫 全部通过（0 FAIL）");
    } else {
        println!("FAILED：{failures} 项断言失败");
        std::process::exit(1);
    }
}

fn print_rows(title: &str, rows: &[Row]) {
    println!("{title}（{} 行）:", rows.len());
    for (index, row) in rows.iter().enumerate() {
        let comment = if row.comment.is_empty() {
            String::new()
        } else {
            format!("  〔{}〕", row.comment)
        };
        println!("  [{index:2}] <{}> {}{comment}", row.kind, row.text);
    }
}
