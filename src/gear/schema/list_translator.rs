//! `schema_list_translator`（对照 C++ `gear/schema_list_translator.{h,cc}`）。
//!
//! 方案菜单里的"方案"部分：当前方案排第一，其余按**最近使用时间**倒序。
//!
//! ## 排序规则（三条，缺一不可）
//!
//! 1. **当前方案永远排第一**（C++ 先 `Append(New<SchemaSelection>(current_schema))`，
//!    再遍历列表且跳过自己）；
//! 2. 其余按 `user.yaml` 的 `var/schema_access_time/<id>` 时间戳**降序**；
//! 3. `switcher/fix_schema_list_order` 打开时**不做**第 2 步，保持 `schema_list`
//!    里的配置顺序（给"我就想让它按我写的顺序排"的用户）。
//!
//! ## `case` 条件
//!
//! 出厂 `default.yaml` 的 `schema_list` 支持带条件的条目：
//!
//! ```yaml
//! schema_list:
//!   - {case: [mode/wubi, mode/wubi_pinyin], schema: wubi_pinyin}
//!   - {case: [mode/wubi], schema: wubi86}
//!   - {schema: pinyin}
//! ```
//!
//! `case` 里**每一项**都必须在 `default.yaml` 里为真，这条才出现（C++
//! `ParseSchemaListEntry`）。所以用户可以通过 `default.custom.yaml` 里的
//! `mode: {wubi: true}` 之类控制菜单里出现哪些方案。
//!
//! ## 与 C++ 的两处结构差异
//!
//! - C++ 的 `SchemaSelection` / `SchemaAction` 是"候选 + 命令"的双身份对象
//!   （多继承 `SimpleCandidate, SwitcherCommand`），`SchemaAction` 还是个
//!   `ShadowCandidate`（把方案候选的 type 改成 `unfold` 以复用展开命令）。
//!   LRN 的候选是纯数据，所以拆成 [`SwitcherItem`]（候选 + 命令）；"选中当前方案
//!   时先展开开关"这件事由 Switcher 在折叠状态下把命令换成
//!   [`SwitcherCommand::SchemaWithUnfold`] 表达。
//! - C++ 的 `Query` 里 `dynamic_cast<Switcher*>(engine_)` 失败就返回 null ——
//!   同 [`crate::gear::translator::switch_translator`]，它只对 Switcher 有效，所以这里也是纯函数。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::engine::candidate::Candidate;
use crate::gear::schema::Schema;
use crate::gear::processor::switcher::{SwitcherCommand, SwitcherItem};

/// 当前方案候选的 `quality` 基数。LRN 的菜单按插入顺序即可，但给一个递减的分数
/// 能让"万一被排序"时也保持正确顺序（switch 候选固定 0）。
const SCHEMA_QUALITY_BASE: f64 = 1.0e6;

/// C++ `ParseSchemaListEntry`：解出一条 `schema_list` 条目。
///
/// `case` 里任何一项为假 ⇒ 整条不出（返回 `None`）。
pub fn parse_schema_list_entry(config: &Value, item: &Value) -> Option<String> {
    let schema = item.get("schema")?.as_str()?.to_string();
    if let Some(cases) = item.get("case").and_then(Value::as_array) {
        for condition in cases {
            let Some(variable) = condition.as_str() else {
                continue;
            };
            let met = config
                .pointer(&format!("/{}", variable.trim_start_matches('/')))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !met {
                return None;
            }
        }
    }
    Some(schema)
}

/// C++ `Switcher::ForEachSchemaListEntry`：按顺序遍历可用的方案 id。
///
/// 回调返回 `false` 表示中断。返回**已处理**的条目数（`case` 不满足的不计）。
pub fn for_each_schema_list_entry(
    config: &Value,
    mut process: impl FnMut(&str) -> bool,
) -> usize {
    let Some(list) = config.pointer("/schema_list").and_then(Value::as_array) else {
        return 0;
    };
    let mut count = 0;
    for item in list {
        let Some(schema_id) = parse_schema_list_entry(config, item) else {
            continue;
        };
        count += 1;
        if !process(&schema_id) {
            break;
        }
    }
    count
}

/// C++ `Switcher::CreateSchema` 的选法：`previously_selected_schema` 在列表里就用它，
/// 否则用列表第一条。返回 `None` 表示列表为空（或全被 `case` 过滤掉了）。
pub fn pick_initial_schema(config: &Value, previously_selected: Option<&str>) -> Option<String> {
    let mut recent: Option<String> = None;
    let mut picked: Option<String> = None;
    for_each_schema_list_entry(config, |schema_id| {
        let previous = previously_selected.unwrap_or("");
        if previous.is_empty() || previous == schema_id {
            picked = Some(schema_id.to_string());
            return false;
        }
        if recent.is_none() {
            recent = Some(schema_id.to_string());
        }
        true
    });
    picked.or(recent)
}

/// 方案的显示名（`schema/name`）。查不到时退回 id（C++ 同样）。
/// 取方案显示名（菜单候选文字）。
///
/// ⚠️ 2026-11-08 19:xx 修：原来每次都 `Schema::open` 整份解析编译产物（~43KB YAML），
/// 展开 `Ctrl+`` 菜单时 `schema_list` 有 12 项 ⇒ 12 次整解析，实测**热 41ms /
/// 冷 129ms**，几乎吃掉 TIP 每键 RPC 预算（180ms）的一半；用户「点/按数字切方案
/// 会卡一下、数字上屏」就是这条慢路径叠出来的。
///
/// 编译产物里 `schema:` 块固定在前几行（`schema:` → `  name: "…"`），名字只在这里用，
/// 所以：先按行扫出 `schema/name`（亚毫秒），扫不到才回退整份解析；结果按
/// `(build_dir, schema_id)` 进程级缓存 —— 切一次方案会重建组件、但菜单命名不再重复解析。
fn schema_name_of(build_dir: &Path, schema_id: &str) -> String {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<HashMap<(PathBuf, String), String>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let key = (build_dir.to_path_buf(), schema_id.to_string());
    if let Ok(guard) = cache.lock()
        && let Some(name) = guard.get(&key)
    {
        return name.clone();
    }
    let name = read_schema_name_fast(build_dir, schema_id)
        .or_else(|| {
            Schema::open(build_dir, schema_id)
                .map(|s| s.schema_name)
                .filter(|name| !name.is_empty())
        })
        .unwrap_or_else(|| schema_id.to_string());
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, name.clone());
    }
    name
}

/// 只读编译产物开头的 `schema:` 块拿 `name`，不做整份 YAML 解析。
///
/// 编译产物由 `deploy_live` 生成，结构稳定（`schema:` 为顶层键、`name` 缩进两格）。
/// 任何不符合预期的文件都返回 `None`，由调用方回退 [`Schema::open`]（正确性优先）。
fn read_schema_name_fast(build_dir: &Path, schema_id: &str) -> Option<String> {
    let series_path = schema_id.replace('.', "/");
    let candidates = [
        build_dir.join(format!("{schema_id}.schema.yaml")),
        build_dir.join(&series_path).join(format!("{schema_id}.schema.yaml")),
        build_dir.join(schema_id).join(format!("{schema_id}.schema.yaml")),
    ];
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut in_schema_block = false;
        for line in text.lines() {
            if line.starts_with("schema:") {
                in_schema_block = true;
                continue;
            }
            if !in_schema_block {
                continue;
            }
            // 缩进消失 = `schema:` 块结束，后面不会再有名字。
            if !line.starts_with(' ') {
                break;
            }
            let Some(rest) = line.trim_start().strip_prefix("name:") else {
                continue;
            };
            let value = rest.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| {
                    value
                        .strip_prefix('\'')
                        .and_then(|v| v.strip_suffix('\''))
                })
                .unwrap_or(value);
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// 建方案部分的候选（C++ `SchemaListTranslation::LoadSchemaList`）。
///
/// 返回顺序即菜单顺序：当前方案，然后其余（`fix_order` 时按配置顺序，否则按
/// `access_times` 降序 —— 用稳定排序，时间戳相同的保持配置顺序）。
pub fn build_items(
    current_schema_id: &str,
    config: &Value,
    build_dir: &Path,
    access_times: &HashMap<String, i64>,
    fix_order: bool,
) -> Vec<SwitcherItem> {
    let mut items: Vec<SwitcherItem> = Vec::new();

    // ① 当前方案永远第一（即使它不在 schema_list 里 —— 用户可能刚用命令行切过去）
    items.push(SwitcherItem {
        candidate: Candidate::new(
            "schema",
            0,
            0,
            schema_name_of(build_dir, current_schema_id),
            SCHEMA_QUALITY_BASE,
        ),
        command: SwitcherCommand::Schema(current_schema_id.to_string()),
    });

    // ② 其余方案
    let mut rest: Vec<SwitcherItem> = Vec::new();
    for_each_schema_list_entry(config, |schema_id| {
        if schema_id == current_schema_id {
            return true; // 已经在第一条了，跳过
        }
        let quality = access_times.get(schema_id).copied().unwrap_or(0) as f64;
        rest.push(SwitcherItem {
            candidate: Candidate::new(
                "schema",
                0,
                0,
                schema_name_of(build_dir, schema_id),
                quality,
            ),
            command: SwitcherCommand::Schema(schema_id.to_string()),
        });
        true
    });

    // ③ 按最近使用时间倒序（稳定排序，`fix_schema_list_order` 时跳过）
    if !fix_order {
        rest.sort_by(|a, b| {
            b.candidate
                .quality
                .partial_cmp(&a.candidate.quality)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    items.extend(rest);

    // 给最终顺序打上递减分数：万一别处按 quality 排序也不会打乱。
    // （菜单里 schema 候选必须整体排在 switch 候选之前。）
    for (index, item) in items.iter_mut().enumerate() {
        item.candidate.quality = SCHEMA_QUALITY_BASE - index as f64 * 1000.0;
    }
    items
}

// ── 方案 → 子方案（两级菜单）────────────────────────────────────────────
//
// 最小语法：子方案编译产物里写 `schema: { parent: <父方案 id> }`。
// 父方案是**真实方案**（照样可选），不是文件夹；`parent` 缺失或指向清单外的
// id 时按平铺处理（历史行为不变）。

/// 第一级里"有子方案"的父方案后面缀的箭头（表示按下去进第二级）。
const SUBMENU_MARK: &str = " ›";

/// 子方案的父方案 id（`schema/parent`，带进程级缓存）。
///
/// 与 [`schema_name_of`] 用同一套"先扫 `schema:` 块、再回退整份解析"的策略：
/// `Ctrl+`` 每次展开都要问一遍，整份解析（~43KB）会拖慢菜单。
/// 读一个方案声明的输入类型（option 名 + 中文 label），按 `input_types.enabled`
/// 过滤、按 `options` 顺序排。
///
/// 返回 `None` 表示该方案**没声明 `input_types`**（不是分组根，也不该走 InputType
/// 第二级）；返回 `Some(vec![])` 表示声明了但当前**一个可用项都没有**（白名单为空、
/// 或 enabled 与 options 完全不交）。Plan A 后，pinyin 这类父方案靠它成为菜单
/// 第二级的分组根，16 个子方案文件夹已删除。
///
/// 顺序/集合交给与引擎一致的 `collect_input_type_options`：`enabled` 是白名单、
/// `options` 决定顺序；两边都没写时回退部署索引。这里只补中文 label。
pub fn schema_input_types_of(build_dir: &Path, schema_id: &str) -> Option<Vec<(String, String)>> {
    // 每个分组根每次展开/重建菜单都会问一遍；`Schema::open` 整份解析实测热 41ms /
    // 冷 129ms（见 `schema_name_of` 的注释），12 个方案 × 每次 Tab/Ctrl+` 会直接
    // 吃掉 TIP 的每键预算，所以与方案名一样做**进程级缓存**。
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<HashMap<(PathBuf, String), Option<Vec<(String, String)>>>>,
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let key = (build_dir.to_path_buf(), schema_id.to_string());
    if let Ok(guard) = cache.lock()
        && let Some(hit) = guard.get(&key)
    {
        return hit.clone();
    }
    let options = read_input_types_of(build_dir, schema_id);
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, options.clone());
    }
    options
}

/// [`schema_input_types_of`] 的真正读取逻辑（不缓存的那一层）。
fn read_input_types_of(build_dir: &Path, schema_id: &str) -> Option<Vec<(String, String)>> {
    let schema = Schema::open(build_dir, schema_id)?;
    // 没有 `input_types` / `input_type` 段 ⇒ 不是分组根（也不走输入类型子级）。
    let node = crate::gear::dict::dict_compiler::input_types_node(&schema.config)?;
    let names =
        crate::engine::runtime::collect_input_type_options(&schema.config, Some(build_dir));
    if names.is_empty() {
        return Some(Vec::new());
    }
    let labels: std::collections::HashMap<String, String> = node
        .get("options")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    let o = e.get("option").and_then(Value::as_str)?;
                    let l = e.get("label").and_then(Value::as_str).unwrap_or(o);
                    Some((o.to_string(), l.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let label = labels.get(&name).cloned().unwrap_or_else(|| name.clone());
        out.push((name, label));
    }
    Some(out)
}

pub fn schema_parent_of(build_dir: &Path, schema_id: &str) -> Option<String> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<HashMap<(PathBuf, String), Option<String>>>,
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let key = (build_dir.to_path_buf(), schema_id.to_string());
    if let Ok(guard) = cache.lock()
        && let Some(parent) = guard.get(&key)
    {
        return parent.clone();
    }
    let parent = read_schema_parent_fast(build_dir, schema_id).or_else(|| {
        Schema::open(build_dir, schema_id).and_then(|schema| schema.parent)
    });
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, parent.clone());
    }
    parent
}

/// 只扫编译产物的 `schema:` 块拿 `parent`，不做整份 YAML 解析（与
/// [`read_schema_name_fast`] 同一条快速路径）。
fn read_schema_parent_fast(build_dir: &Path, schema_id: &str) -> Option<String> {
    let series_path = schema_id.replace('.', "/");
    let candidates = [
        build_dir.join(format!("{schema_id}.schema.yaml")),
        build_dir.join(&series_path).join(format!("{schema_id}.schema.yaml")),
        build_dir.join(schema_id).join(format!("{schema_id}.schema.yaml")),
    ];
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut in_schema_block = false;
        for line in text.lines() {
            if line.starts_with("schema:") {
                in_schema_block = true;
                continue;
            }
            if !in_schema_block {
                continue;
            }
            if !line.starts_with(' ') {
                break;
            }
            let Some(rest) = line.trim_start().strip_prefix("parent:") else {
                continue;
            };
            let value = rest.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value);
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

pub(crate) fn schema_id_of(item: &SwitcherItem) -> Option<&str> {
    match &item.command {
        SwitcherCommand::Schema(id) | SwitcherCommand::SchemaWithUnfold(id) => Some(id),
        _ => None,
    }
}

/// 父方案"自身默认输入方式"的标签（`schema/parent_label`）。
///
/// 2026-11-12 起第二级菜单不再显示父方案自身，本函数不再参与菜单构造；
/// 保留为公开 API，以兼容仍在写 `parent_label` 的存量方案与设置页读取。
pub fn schema_parent_label_of(build_dir: &Path, schema_id: &str) -> Option<String> {
    Schema::open(build_dir, schema_id).and_then(|schema| schema.parent_label)
}

/// 把平铺的方案条目录成"方案 → 子方案"两级。
///
/// * `group = None`：第一级，**只列主方案**。有子方案的父方案文字后加 ` ›`，
///   命令换成 [`SwitcherCommand::OpenGroup`]（选中 = 进第二级，不切方案、不关菜单）；
///   子方案不在第一级平铺出现。
/// * `group = Some(父 id)`：第二级，**只列它的子方案**。2026-11-12 用户口径：
///   子方案菜单里不要再显示父方案自身（截图里那个「拼音」）；无调全拼由
///   `pinyin.luna`「明月」承担，父方案仍是 `active` 与清单首位。没有子方案时
///   调用方根本不会进第二级，所以不会出现空列表。
/// * `parent` 指向的 id 不在清单里 ⇒ 该子项当顶层处理（**缺字段/悬空都平铺**）。
pub fn two_level_items(
    items: Vec<SwitcherItem>,
    build_dir: &Path,
    group: Option<&str>,
    current: &str,
) -> Vec<SwitcherItem> {
    let ids: Vec<String> = items.iter().map(|i| {
        schema_id_of(i).map(str::to_string).unwrap_or_default()
    }).collect();
    let id_set: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();

    // 父子/分组根判定与「折叠列表」共用同一份（见 [`menu_hierarchy`]）。
    let MenuHierarchy {
        parents,
        group_roots,
    } = menu_hierarchy(&items, build_dir);

    // ---- 第二级：列出该分组根下的输入类型（SwitcherCommand::InputType）----
    if let Some(group_id) = group {
        // 优先 input_types 分组（Plan A 主线）：每个可用输入类型是一行 InputType 命令，
        // 中文文案取自 `input_types.options[].label`。父方案自身（如「明月=无调全拼」）
        // 不再占位 —— 它仍是 active 主方案，由 reset 决定默认类型。
        if let Some(options) = schema_input_types_of(build_dir, group_id) {
            let mut out = Vec::with_capacity(options.len());
            for (name, label) in options {
                out.push(SwitcherItem {
                    candidate: Candidate::new("switch", 0, 0, label, 0.0),
                    command: SwitcherCommand::InputType(name),
                });
            }
            return out;
        }
        // 兜底：旧式 parent 子方案（没有 input_types 的分组根）。
        let mut out = Vec::new();
        for (index, parent) in parents.iter().enumerate() {
            if parent.as_deref() == Some(group_id) {
                out.push(items[index].clone());
            }
        }
        return out;
    }

    // ---- 第一级：子方案不上、其余主方案列出；分组根加 ` ›` 并换成 OpenGroup ----
    let mut out: Vec<SwitcherItem> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        if parents[index].is_some() {
            continue; // 子方案不上第一级
        }
        let id = ids[index].clone();
        let mut head = item.clone();
        if group_roots.contains(&id) {
            head.candidate.text = format!("{}{}", head.candidate.text, SUBMENU_MARK);
            head.command = SwitcherCommand::OpenGroup(id);
        }
        out.push(head);
    }

    // 当前方案若是子方案，把它的父分组提到第 0 条；若当前方案自身是分组根则提自身。
    let promote_id = schema_parent_of(build_dir, current)
        .filter(|parent| id_set.contains(parent.as_str()))
        .or_else(|| {
            if group_roots.contains(current) {
                Some(current.to_string())
            } else {
                None
            }
        });
    if let Some(pid) = promote_id
        && let Some(index) = out.iter().position(|item| {
            matches!(&item.command, SwitcherCommand::OpenGroup(id) if *id == pid)
        })
    {
        let head = out.remove(index);
        out.insert(0, head);
    }
    out
}

// ── 折叠列表（内联展开）──────────────────────────────────────────────
//
// 2026-11-13 用户口径：「把 ctrl` 做成折叠列表，里面是各种方案」——
// 一级行 = 各方案，分组根（拼音这类声明了 input_types 的主方案，或旧式带
// `parent` 子方案的父方案）行尾带 ▸/▾，展开后子项**就地缩进排在下一行**，
// 不换屏、不需要「返回上一级」。下钻式两级菜单保留在 `two_level_items` 里，
// 由 `switcher/two_level_menu` 配置切回。

/// 折叠列表：分组根**收起**时行尾的箭头。
const COLLAPSE_ARROW_CLOSED: &str = " ▸";
/// 折叠列表：分组根**展开**时行尾的箭头。
const COLLAPSE_ARROW_OPEN: &str = " ▾";
/// 折叠列表：子项缩进（全角空格；候选窗排版按字符计宽，不会被当空白裁掉）。
const CHILD_INDENT: &str = "\u{3000}";

/// 菜单里的分组关系（折叠列表与下钻式两级菜单共用同一套判定）。
pub struct MenuHierarchy {
    /// 每个条目（与传入 `items` 同序）的有效父方案 id。
    pub parents: Vec<Option<String>>,
    /// 分组根 id：① 旧式 `parent` 子方案指向它；② 声明了非空 `input_types`。
    pub group_roots: std::collections::HashSet<String>,
}

/// 计算菜单的分组关系。
///
/// 规则（与 [`two_level_items`] 完全一致，抽出来供折叠列表复用）：
/// * 父必须在清单里、且不能是自己，否则当作平铺（悬空 `parent` 不崩）；
/// * 只支持两层：父方案自己也是别人的子方案时丢掉那层声明（避免三层）；
/// * `input_types` / `input_type` 段存在且 `enabled`+`options` 交出不空时才成为
///   分组根 —— Plan A 之后拼音靠这条路成为分组根，旧式子方案靠 `parent` 那条。
pub fn menu_hierarchy(items: &[SwitcherItem], build_dir: &Path) -> MenuHierarchy {
    let ids: Vec<String> = items
        .iter()
        .map(|i| schema_id_of(i).map(str::to_string).unwrap_or_default())
        .collect();
    let id_set: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();

    // 每个条目的"有效父"（父必须在清单里、且不能是自己，否则当顶层）。
    let mut parents: Vec<Option<String>> = ids
        .iter()
        .map(|id| {
            schema_parent_of(build_dir, id)
                .filter(|parent| parent != id && id_set.contains(parent.as_str()))
        })
        .collect();
    // 只支持两层：父方案自己也是别人的子方案时，丢掉它下面那层声明（与设置界面的
    // `schema_tree::build_tree` 同一条规则）—— 更深的方案回到第一级平铺，不消失。
    let declared_children: std::collections::HashSet<String> = parents
        .iter()
        .enumerate()
        .filter(|(_, parent)| parent.is_some())
        .map(|(index, _)| ids[index].clone())
        .collect();
    for parent in parents.iter_mut() {
        if parent
            .as_deref()
            .is_some_and(|id| declared_children.contains(id))
        {
            *parent = None;
        }
    }

    // 分组根：① 旧式 `parent` 子方案指向它；② 声明了 `input_types`（且有可用项）。
    // Plan A 之后 pinyin 这类父方案靠 ② 成为分组根；两机制并存，互不冲突。
    let mut group_roots: std::collections::HashSet<String> = std::collections::HashSet::new();
    for parent in parents.iter().flatten() {
        group_roots.insert(parent.clone());
    }
    for id in &ids {
        if schema_input_types_of(build_dir, id).is_some_and(|opts| !opts.is_empty()) {
            group_roots.insert(id.clone());
        }
    }
    MenuHierarchy {
        parents,
        group_roots,
    }
}

/// 折叠列表模式下的一级条目：分组根就地展开成缩进子项，其余方案平铺。
///
/// * `expanded` 里的分组根展开：行尾 `▾`，紧跟着它的子项（缩进一行）；
/// * 其余分组根收起：行尾 `▸`，只有一行；
/// * 当前方案所在的分组提到第 0 条（与两级菜单「当前方案优先」同一口径）；
/// * `option_is_on` 用来标当前输入类型：分组根右侧注释 = 当前类型中文名，
///   子项右侧注释 = 「当前」。
///
/// 子项命令沿用两级菜单的 [`SwitcherCommand::InputType`]（input_types 分组根）
/// 或原样保留 [`SwitcherCommand::Schema`]（旧式 `parent` 子方案）。
pub fn collapsed_items(
    items: Vec<SwitcherItem>,
    build_dir: &Path,
    current: &str,
    expanded: &std::collections::HashSet<String>,
    option_is_on: &dyn Fn(&str) -> bool,
) -> Vec<SwitcherItem> {
    let ids: Vec<String> = items
        .iter()
        .map(|i| schema_id_of(i).map(str::to_string).unwrap_or_default())
        .collect();
    let hierarchy = menu_hierarchy(&items, build_dir);
    let mut out: Vec<SwitcherItem> = Vec::with_capacity(items.len() + 16);

    for (index, item) in items.iter().enumerate() {
        if hierarchy.parents[index].is_some() {
            continue; // 子方案收在父方案那一行下面
        }
        let id = ids[index].clone();

        // ① input_types 分组根：子项 = 主方案声明的输入类型（全拼/自然码/…）。
        if let Some(options) =
            schema_input_types_of(build_dir, &id).filter(|options| !options.is_empty())
        {
            let is_expanded = expanded.contains(&id);
            let mut head = item.clone();
            head.candidate.text = format!(
                "{}{}",
                head.candidate.text,
                if is_expanded {
                    COLLAPSE_ARROW_OPEN
                } else {
                    COLLAPSE_ARROW_CLOSED
                }
            );
            if let Some((_, label)) = options.iter().find(|(name, _)| option_is_on(name)) {
                head.candidate.comment = label.clone();
            }
            head.command = SwitcherCommand::OpenGroup(id.clone());
            out.push(head);
            if is_expanded {
                for (name, label) in options {
                    let mut candidate =
                        Candidate::new("switch", 0, 0, format!("{CHILD_INDENT}{label}"), 0.0);
                    if option_is_on(&name) {
                        candidate.comment = "当前".to_string();
                    }
                    out.push(SwitcherItem {
                        candidate,
                        command: SwitcherCommand::InputType(name),
                    });
                }
            }
            continue;
        }

        // ② 旧式 parent 分组根：子项 = 声明了 `parent: 它` 的真实子方案。
        if hierarchy.group_roots.contains(&id) {
            let is_expanded = expanded.contains(&id);
            let mut head = item.clone();
            head.candidate.text = format!(
                "{}{}",
                head.candidate.text,
                if is_expanded {
                    COLLAPSE_ARROW_OPEN
                } else {
                    COLLAPSE_ARROW_CLOSED
                }
            );
            head.command = SwitcherCommand::OpenGroup(id.clone());
            out.push(head);
            if is_expanded {
                for (child_index, parent) in hierarchy.parents.iter().enumerate() {
                    if parent.as_deref() == Some(id.as_str()) {
                        let mut child = items[child_index].clone();
                        child.candidate.text =
                            format!("{CHILD_INDENT}{}", child.candidate.text);
                        out.push(child);
                    }
                }
            }
            continue;
        }

        out.push(item.clone());
    }

    // 当前方案若是子方案/输入类型，把它所属的分组根提到第 0 条。
    let promote_id = ids
        .iter()
        .position(|id| id == current)
        .and_then(|index| hierarchy.parents[index].clone())
        .or_else(|| {
            if hierarchy.group_roots.contains(current) {
                Some(current.to_string())
            } else {
                None
            }
        });
    if let Some(pid) = promote_id
        && let Some(index) = out.iter().position(|item| {
            matches!(&item.command, SwitcherCommand::OpenGroup(id) if *id == pid)
        })
    {
        let head = out.remove(index);
        out.insert(0, head);
    }
    out
}

/// 折叠列表首次打开时默认展开的分组根：当前方案自己声明了 `input_types` 就展开
/// 自己（这样一打开就能看到当前输入类型），否则展开它所属的旧式父方案。
pub fn initial_expansion(build_dir: &Path, current: &str) -> Option<String> {
    if schema_input_types_of(build_dir, current).is_some_and(|options| !options.is_empty()) {
        return Some(current.to_string());
    }
    schema_parent_of(build_dir, current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gear::processor::switcher::SwitcherCommand;
    use serde_json::json;
    use std::path::PathBuf;

    fn build_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/e2e/build")
    }

    fn config() -> Value {
        json!({
            "schema_list": [
                { "schema": "luna_pinyin" },
                { "schema": "wubi86" },
                { "case": ["mode/stroke"], "schema": "stroke" }
            ]
        })
    }

    #[test]
    fn case_conditions_gate_entries() {
        let cfg = config();
        let mut ids = Vec::new();
        for_each_schema_list_entry(&cfg, |id| {
            ids.push(id.to_string());
            true
        });
        assert_eq!(ids, vec!["luna_pinyin", "wubi86"], "stroke 的 case 不满足");

        let mut cfg_with_mode = cfg.clone();
        cfg_with_mode["mode"] = json!({ "stroke": true });
        let mut ids = Vec::new();
        for_each_schema_list_entry(&cfg_with_mode, |id| {
            ids.push(id.to_string());
            true
        });
        assert_eq!(ids, vec!["luna_pinyin", "wubi86", "stroke"]);
    }

    #[test]
    fn case_requires_every_condition() {
        let cfg = json!({
            "mode": { "wubi": true, "pinyin": false },
            "schema_list": [
                { "case": ["mode/wubi", "mode/pinyin"], "schema": "both" },
                { "case": ["mode/wubi"], "schema": "wubi_only" }
            ]
        });
        let mut ids = Vec::new();
        for_each_schema_list_entry(&cfg, |id| {
            ids.push(id.to_string());
            true
        });
        assert_eq!(ids, vec!["wubi_only"], "两个条件里有一个假就整条不出");
    }

    #[test]
    fn current_schema_comes_first() {
        let items = build_items(
            "wubi86",
            &config(),
            &build_dir(),
            &HashMap::new(),
            false,
        );
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].command,
            SwitcherCommand::Schema("wubi86".to_string())
        );
        assert_eq!(
            items[1].command,
            SwitcherCommand::Schema("luna_pinyin".to_string())
        );
        assert!(items[0].candidate.quality > items[1].candidate.quality);
    }

    #[test]
    fn rest_is_sorted_by_recency() {
        let mut times = HashMap::new();
        times.insert("luna_pinyin".to_string(), 100);
        times.insert("wubi86".to_string(), 200);
        // 当前是 stroke（不在列表里也照样排第一）
        let items = build_items("stroke", &config(), &build_dir(), &times, false);
        let ids: Vec<&str> = items
            .iter()
            .map(|i| match &i.command {
                SwitcherCommand::Schema(id) => id.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(ids, vec!["stroke", "wubi86", "luna_pinyin"], "最近用过的在前");
    }

    #[test]
    fn fix_schema_list_order_keeps_config_order() {
        let mut times = HashMap::new();
        times.insert("luna_pinyin".to_string(), 100);
        times.insert("wubi86".to_string(), 200);
        let items = build_items("stroke", &config(), &build_dir(), &times, true);
        let ids: Vec<&str> = items
            .iter()
            .map(|i| match &i.command {
                SwitcherCommand::Schema(id) => id.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(
            ids,
            vec!["stroke", "luna_pinyin", "wubi86"],
            "固定顺序时按配置来，不看时间戳"
        );
    }

    #[test]
    fn pick_initial_schema_prefers_previous() {
        let cfg = config();
        assert_eq!(
            pick_initial_schema(&cfg, Some("wubi86")).as_deref(),
            Some("wubi86")
        );
        // 上次用的不在列表里 ⇒ 用第一条
        assert_eq!(
            pick_initial_schema(&cfg, Some("nonexistent")).as_deref(),
            Some("luna_pinyin")
        );
        assert_eq!(pick_initial_schema(&cfg, None).as_deref(), Some("luna_pinyin"));
    }

    #[test]
    fn empty_schema_list_yields_none() {
        assert_eq!(pick_initial_schema(&json!({}), None), None);
    }

    // ── 方案 → 子方案两级菜单 ───────────────────────────────────────────

    fn sandbox(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lrn-group-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建沙箱目录");
        dir
    }

    /// 写一份"编译后"方案：`schema:` 块 + 可选 `parent`。
    fn write_schema(dir: &PathBuf, id: &str, name: &str, parent: Option<&str>) {
        write_schema_with_label(dir, id, name, parent, None);
    }

    /// 写一份"编译后"方案：`schema:` 块 + 可选 `parent` / `parent_label`。
    fn write_schema_with_label(
        dir: &PathBuf,
        id: &str,
        name: &str,
        parent: Option<&str>,
        parent_label: Option<&str>,
    ) {
        let parent_line = parent
            .map(|parent| format!("  parent: {parent}\n"))
            .unwrap_or_default();
        let label_line = parent_label
            .map(|label| format!("  parent_label: {label}\n"))
            .unwrap_or_default();
        std::fs::write(
            dir.join(format!("{id}.schema.yaml")),
            format!("schema:\n  id: {id}\n  name: {name}\n{parent_line}{label_line}"),
        )
        .expect("写方案文件");
    }

    fn group_config() -> Value {
        json!({
            "schema_list": [
                { "schema": "pinyin" },
                { "schema": "pinyin.ziranma" },
                { "schema": "wubi86" }
            ]
        })
    }

    #[test]
    fn main_level_lists_only_parent_schemes() {
        let dir = sandbox("main");
        write_schema(&dir, "pinyin", "拼音", None);
        write_schema(&dir, "pinyin.ziranma", "自然码", Some("pinyin"));
        write_schema(&dir, "wubi86", "五笔", None);

        let flat = build_items("pinyin", &group_config(), &dir, &HashMap::new(), true);
        let main = two_level_items(flat, &dir, None, "pinyin");
        let texts: Vec<&str> = main.iter().map(|i| i.candidate.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["拼音 ›", "五笔"],
            "第一级只列主方案，子方案不内联平铺"
        );
        assert_eq!(
            main[0].command,
            SwitcherCommand::OpenGroup("pinyin".to_string()),
            "有子方案的主方案：选中 = 进第二级，不直接切方案"
        );
        assert_eq!(
            main[1].command,
            SwitcherCommand::Schema("wubi86".to_string()),
            "没有子方案的主方案：选中直接切"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn second_level_lists_only_children_without_parent() {
        let dir = sandbox("level2");
        write_schema_with_label(&dir, "pinyin", "拼音", None, Some("全拼"));
        write_schema(&dir, "pinyin.ziranma", "自然码", Some("pinyin"));
        write_schema(&dir, "wubi86", "五笔", None);

        let flat = build_items("pinyin.ziranma", &group_config(), &dir, &HashMap::new(), true);
        let second = two_level_items(flat, &dir, Some("pinyin"), "pinyin.ziranma");
        let texts: Vec<&str> = second.iter().map(|i| i.candidate.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["自然码"],
            "第二级只列子方案，父方案自身（parent_label=全拼）不再占位"
        );
        assert_eq!(
            second[0].command,
            SwitcherCommand::Schema("pinyin.ziranma".to_string())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn single_child_group_never_renders_an_empty_second_level() {
        let dir = sandbox("onechild");
        write_schema_with_label(&dir, "pinyin", "拼音", None, Some("全拼"));
        write_schema(&dir, "pinyin.ziranma", "自然码", Some("pinyin"));

        let flat = build_items("pinyin", &group_config(), &dir, &HashMap::new(), true);
        let second = two_level_items(flat, &dir, Some("pinyin"), "pinyin");
        let texts: Vec<&str> = second.iter().map(|i| i.candidate.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["自然码"],
            "只有一个子方案时，第二级仍有唯一子方案一项，不会是空列表"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dangling_parent_or_missing_field_stays_flat() {
        let dir = sandbox("flat");
        write_schema(&dir, "orphan", "孤儿", Some("no_such_schema"));
        write_schema(&dir, "wubi86", "五笔", None);
        let cfg = json!({
            "schema_list": [{ "schema": "orphan" }, { "schema": "wubi86" }]
        });

        let flat = build_items("orphan", &cfg, &dir, &HashMap::new(), true);
        let main = two_level_items(flat, &dir, None, "orphan");
        let texts: Vec<&str> = main.iter().map(|i| i.candidate.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["孤儿", "五笔"],
            "父 id 不在清单里 ⇒ 平铺；没有 parent 字段也平铺"
        );
        assert!(
            texts.iter().all(|text| !text.contains('›')),
            "平铺项不带子菜单标记"
        );
        assert_eq!(main[0].command, SwitcherCommand::Schema("orphan".to_string()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn current_child_promotes_its_parent_group_to_front() {
        let dir = sandbox("promote");
        write_schema(&dir, "pinyin", "拼音", None);
        write_schema(&dir, "pinyin.ziranma", "自然码", Some("pinyin"));
        write_schema(&dir, "wubi86", "五笔", None);
        let cfg = json!({
            "schema_list": [
                { "schema": "wubi86" },
                { "schema": "pinyin" },
                { "schema": "pinyin.ziranma" }
            ]
        });

        let flat = build_items("pinyin.ziranma", &cfg, &dir, &HashMap::new(), true);
        let main = two_level_items(flat, &dir, None, "pinyin.ziranma");
        let texts: Vec<&str> = main.iter().map(|i| i.candidate.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["拼音 ›", "五笔"],
            "当前是子方案时，第一级第 0 条显示它所属的主方案"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 自引用（`parent` 指自己）与三层链都不该让方案从菜单里消失：
    /// 前者的父声明作废；后者只保留最上面一层分组，更深的那条回到第一级平铺。
    #[test]
    fn self_parent_and_three_level_chain_never_lose_schemes() {
        let dir = sandbox("selfchain");
        write_schema(&dir, "selfref", "自引用", Some("selfref"));
        write_schema(&dir, "a", "甲", None);
        write_schema(&dir, "b", "乙", Some("a"));
        write_schema(&dir, "c", "丙", Some("b"));
        let cfg = json!({
            "schema_list": [
                { "schema": "selfref" },
                { "schema": "a" },
                { "schema": "b" },
                { "schema": "c" }
            ]
        });

        let flat = build_items("selfref", &cfg, &dir, &HashMap::new(), true);
        let main = two_level_items(flat, &dir, None, "selfref");
        let texts: Vec<&str> = main.iter().map(|i| i.candidate.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["自引用", "甲 ›", "丙"],
            "自引用方案照常平铺；三层链只保留一层分组，丙回到第一级，不丢方案"
        );
        assert_eq!(main[0].command, SwitcherCommand::Schema("selfref".to_string()));
        assert_eq!(main[1].command, SwitcherCommand::OpenGroup("a".to_string()));

        let second = two_level_items(
            build_items("selfref", &cfg, &dir, &HashMap::new(), true),
            &dir,
            Some("a"),
            "selfref",
        );
        let texts: Vec<&str> = second.iter().map(|i| i.candidate.text.as_str()).collect();
        assert_eq!(texts, vec!["乙"], "第二级只列子方案，不插父方案「甲」");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Plan A：主方案不再有子方案文件夹，输入类型由 `input_types` 声明驱动。
    #[test]
    fn input_types_group_generates_input_type_second_level() {
        let dir = sandbox("itypes");
        // 主方案 pinyin 声明 input_types（无 parent、无子方案文件夹）。
        std::fs::write(
            dir.join("pinyin.schema.yaml"),
            "schema:\n  id: pinyin\n  name: 拼音\ninput_types:\n  enabled: [luna, terra]\n  reset: luna\n  options:\n  - {option: luna, label: 明月}\n  - {option: terra, label: 地球}\n",
        )
        .expect("写方案文件");
        write_schema(&dir, "wubi86", "五笔", None);

        let cfg = json!({
            "schema_list": [{ "schema": "pinyin" }, { "schema": "wubi86" }]
        });

        // 第一级：声明 input_types 的 pinyin 成为分组根（加 ›，走 OpenGroup）。
        let flat = build_items("pinyin", &cfg, &dir, &HashMap::new(), true);
        let main = two_level_items(flat, &dir, None, "pinyin");
        let texts: Vec<&str> = main.iter().map(|i| i.candidate.text.as_str()).collect();
        assert_eq!(texts, vec!["拼音 ›", "五笔"], "声明 input_types 的主方案成为分组根");
        assert_eq!(
            main[0].command,
            SwitcherCommand::OpenGroup("pinyin".to_string()),
            "分组根选中 = 进第二级"
        );

        // 第二级：由 input_types.options 生成 InputType 命令（中文 label 作文案）。
        let second = two_level_items(
            build_items("pinyin", &cfg, &dir, &HashMap::new(), true),
            &dir,
            Some("pinyin"),
            "pinyin",
        );
        let texts: Vec<&str> = second.iter().map(|i| i.candidate.text.as_str()).collect();
        assert_eq!(texts, vec!["明月", "地球"], "二级菜单按 label 显示输入类型");
        assert_eq!(
            second[0].command,
            SwitcherCommand::InputType("luna".to_string())
        );
        assert_eq!(
            second[1].command,
            SwitcherCommand::InputType("terra".to_string())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 2026-11-13 数据迁移：方案用**单数** `input_type:` 声明输入类型，引擎也必须认。
    ///
    /// 断点现场：源仓与部署产物都写成 `input_type:`，而引擎只读 `/input_types`，
    /// 拼音因此不是分组根 —— Ctrl+` 里一个输入类型子项都没有。
    #[test]
    fn single_input_type_key_is_recognized() {
        let dir = sandbox("itypes-single");
        std::fs::write(
            dir.join("pinyin.schema.yaml"),
            "schema:\n  id: pinyin\n  name: 拼音\ninput_type:\n  enabled: [pinyin, zrm]\n  reset: pinyin\n  options:\n  - {option: pinyin, label: 全拼}\n  - {option: zrm, label: 自然码}\n",
        )
        .expect("写方案文件");

        let options = schema_input_types_of(&dir, "pinyin").expect("单数 input_type 也要被识别");
        assert_eq!(
            options,
            vec![
                ("pinyin".to_string(), "全拼".to_string()),
                ("zrm".to_string(), "自然码".to_string())
            ],
            "单数键的 options 顺序与中文 label 都要保留"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 折叠列表：分组根行尾 ▸/▾，展开后子项**就地缩进**排在下一行（不换屏）。
    #[test]
    fn collapsed_items_inline_expand_input_types() {
        let dir = sandbox("collapse");
        std::fs::write(
            dir.join("pinyin.schema.yaml"),
            "schema:\n  id: pinyin\n  name: 拼音\ninput_type:\n  enabled: [pinyin, zrm]\n  options:\n  - {option: pinyin, label: 全拼}\n  - {option: zrm, label: 自然码}\n",
        )
        .expect("写方案文件");
        write_schema(&dir, "wubi86", "五笔", None);

        let cfg = json!({
            "schema_list": [{ "schema": "pinyin" }, { "schema": "wubi86" }]
        });
        let expanded: std::collections::HashSet<String> = std::collections::HashSet::new();
        let flat = build_items("pinyin", &cfg, &dir, &HashMap::new(), true);
        let collapsed = collapsed_items(flat, &dir, "pinyin", &expanded, &|_| false);
        let texts: Vec<&str> = collapsed
            .iter()
            .map(|i| i.candidate.text.as_str())
            .collect();
        assert_eq!(
            texts,
            vec!["拼音 ▸", "五笔"],
            "收起时只有分组根一行，行尾是收起箭头"
        );
        assert_eq!(
            collapsed[0].command,
            SwitcherCommand::OpenGroup("pinyin".to_string())
        );

        let mut expanded = std::collections::HashSet::new();
        expanded.insert("pinyin".to_string());
        let flat = build_items("pinyin", &cfg, &dir, &HashMap::new(), true);
        let open = collapsed_items(flat, &dir, "pinyin", &expanded, &|name| name == "zrm");
        let texts: Vec<&str> = open.iter().map(|i| i.candidate.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["拼音 ▾", "\u{3000}全拼", "\u{3000}自然码", "五笔"],
            "展开后输入类型就地缩进排在父行下面，其余方案顺延"
        );
        assert_eq!(open[1].command, SwitcherCommand::InputType("pinyin".to_string()));
        assert_eq!(open[2].command, SwitcherCommand::InputType("zrm".to_string()));
        assert_eq!(
            open[2].candidate.comment, "当前",
            "当前输入类型子行右侧标「当前」"
        );
        assert_eq!(
            open[0].candidate.comment, "自然码",
            "父行右侧注释显示当前输入类型名"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 折叠列表：旧式 `parent:` 子方案也能内联展开（与 input_types 分组共存）。
    #[test]
    fn collapsed_items_inline_expand_legacy_parent_group() {
        let dir = sandbox("collapse-parent");
        write_schema(&dir, "pinyin", "拼音", None);
        write_schema(&dir, "pinyin.ziranma", "自然码", Some("pinyin"));
        write_schema(&dir, "wubi86", "五笔", None);

        let flat = build_items("pinyin", &group_config(), &dir, &HashMap::new(), true);
        let mut expanded = std::collections::HashSet::new();
        expanded.insert("pinyin".to_string());
        let open = collapsed_items(flat, &dir, "pinyin", &expanded, &|_| false);
        let texts: Vec<&str> = open.iter().map(|i| i.candidate.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["拼音 ▾", "\u{3000}自然码", "五笔"],
            "旧式 parent 子方案也内联缩进，不再跳第二级"
        );
        assert!(
            matches!(&open[1].command, SwitcherCommand::Schema(id) if id == "pinyin.ziranma"),
            "旧式子方案子行仍按「切方案」执行"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
