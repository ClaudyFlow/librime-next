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
//!   同 [`super::switch_translator`]，它只对 Switcher 有效，所以这里也是纯函数。

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use crate::engine::candidate::Candidate;
use crate::engine::schema::Schema;
use crate::engine::switcher::{SwitcherCommand, SwitcherItem};

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
fn schema_name_of(build_dir: &Path, schema_id: &str) -> String {
    Schema::open(build_dir, schema_id)
        .map(|s| s.schema_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| schema_id.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::switcher::SwitcherCommand;
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
}
