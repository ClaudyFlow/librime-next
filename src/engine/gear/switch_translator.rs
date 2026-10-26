//! `switch_translator`（对照 C++ `gear/switch_translator.{h,cc}`）。
//!
//! 方案菜单里的"开关"部分：把当前方案的 `switches:` 清单铺成候选。两种形态：
//!
//! - **单项开关**（`name:`）：正文是当前状态标签，注释是 `→ 另一状态`；
//! - **单选组**（`options:`）：组内每项一条候选，选中的那条注释打 ✓。
//!
//! 出厂 `default.yaml` 还有两个相关设置：`switcher/fold_options`（默认折叠成一行，
//! 选中才展开）与 `switcher/abbreviate_options`（状态标签只取首字）。
//!
//! ## 为什么不实现 `Translator` trait
//!
//! C++ 的 `SwitchTranslator::Query` 第一件事是 `dynamic_cast<Switcher*>(engine_)`，
//! 拿不到就返回 null —— 它**只对 Switcher 有效**，永远不会出现在方案的
//! `engine/translators` 名单里（出厂 124 个方案确实一个都没列）。
//!
//! LRN 的 `Translator::query` 拿不到 `Engine`（只有 input / segment / history），
//! 所以这里做成一个纯构建函数，由 [`crate::engine::switcher::Switcher`] 直接调用。
//! 行为与 C++ 一致，只是省掉了那层用不上的 trait 外壳。
//!
//! ## 一个必须保留的副作用
//!
//! C++ 在**建菜单时**就对每个单选组调 `RadioGroup::SelectOption(GetSelectedOption())`，
//! 把"组内第一个处于打开状态的项"同步回 `Context`。所以单选组的默认值不是配置里
//! 写死的，而是每次打开菜单时按当前 option 状态重算 —— 这里照抄（`build_items`
//! 收 `&mut Context` 就是为了这个）。

use std::collections::HashSet;

use serde_json::Value;

use crate::engine::candidate::Candidate;
use crate::engine::context::Context;
use crate::engine::switches::{SwitchOption, SwitchType, Switches};
use crate::engine::switcher::{SwitcherCommand, SwitcherItem};

/// 候选注释里的右箭头（`switch_translator.cc:17`，`"\xe2\x86\x92 "`）。
const RIGHT_ARROW: &str = "\u{2192} ";
/// 单选组选中标记（`switch_translator.cc:19`，U+2713 前面带一个空格）。
const RADIO_SELECTED: &str = " \u{2713}";

/// 折叠开关列表的展示参数（`switcher/option_list_*`）。
struct FoldedStyle {
    prefix: String,
    suffix: String,
    separator: String,
    abbreviate: bool,
}

impl FoldedStyle {
    fn from_config(config: &Value) -> Self {
        let s = |key: &str| {
            config
                .pointer(&format!("/switcher/{key}"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        FoldedStyle {
            prefix: s("option_list_prefix"),
            suffix: s("option_list_suffix"),
            // C++ 默认值就是单个空格（`separator_ = " "`），配置缺省时保持
            separator: config
                .pointer("/switcher/option_list_separator")
                .and_then(Value::as_str)
                .unwrap_or(" ")
                .to_string(),
            abbreviate: config
                .pointer("/switcher/abbreviate_options")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }
}

/// 一条开关候选 + 它的动作。
fn toggle_item(option: &SwitchOption, context: &Context, save_options: &HashSet<String>) -> SwitcherItem {
    let current = context.get_option(&option.option_name);
    let text = option.state_label(current as usize, false).unwrap_or_default();
    let next = option
        .state_label((!current) as usize, false)
        .unwrap_or_default();
    let mut candidate = Candidate::new("switch", 0, 0, text, 0.0);
    if !next.is_empty() {
        candidate = candidate.with_comment(format!("{RIGHT_ARROW}{next}"));
    }
    SwitcherItem {
        candidate,
        command: SwitcherCommand::Toggle {
            option: option.option_name.clone(),
            target: !current,
            auto_save: save_options.contains(&option.option_name),
        },
    }
}

/// 单选组里的一条候选。注释里的 ✓ 由调用方在同步组状态后统一刷新。
fn radio_item(option: &SwitchOption) -> SwitcherItem {
    let text = option.state_label(option.option_index, false).unwrap_or_default();
    SwitcherItem {
        candidate: Candidate::new("switch", 0, 0, text, 0.0),
        command: SwitcherCommand::Radio {
            group: option.group.clone(),
            option: option.option_name.clone(),
        },
    }
}

/// 单选组的同步（C++ `RadioGroup::GetSelectedOption` + `SelectOption`）。
///
/// 选出"组内第一个当前处于打开状态的项"，没有就取组内第一条；然后把这一个设为
/// 打开、其余关闭。C++ 在这一步对每条候选调 `UpdateState` 刷 ✓ 注释，这里等价地
/// 在 `items` 上补注释。
///
/// 副作用（改 `context` 的 option、写 user config）由调用方在 `apply` 时做 —— 但
/// **建菜单时也要同步一次**，否则第一次打开菜单看到的选中项与配置不符。
fn sync_radio_group(
    group: &[String],
    context: &mut Context,
    items: &mut [SwitcherItem],
    save_options: &HashSet<String>,
    option_writes: &mut Vec<(String, bool)>,
) {
    if group.is_empty() {
        return;
    }
    let selected = group
        .iter()
        .find(|name| context.get_option(name))
        .cloned()
        .unwrap_or_else(|| group[0].clone());
    for name in group {
        let should_be_on = *name == selected;
        if context.get_option(name) != should_be_on {
            context.set_option(name, should_be_on);
            if save_options.contains(name) {
                option_writes.push((name.clone(), should_be_on));
            }
        }
    }
    // 刷新 ✓ 注释
    for item in items.iter_mut() {
        if let SwitcherCommand::Radio { option, .. } = &item.command {
            if group.contains(option) {
                item.candidate.comment = if *option == selected {
                    RADIO_SELECTED.to_string()
                } else {
                    String::new()
                };
            }
        }
    }
}

/// 建开关部分的候选（C++ `SwitchTranslation::LoadSwitches`）。
///
/// 返回的条目顺序与 C++ 一致：先按 `switches:` 配置顺序铺开，折叠打开时**整段
/// 换成一条** `unfold` 候选。
///
/// `option_writes` 收回"建菜单时顺带写 user config"的那部分 —— 调用方（Switcher）
/// 负责落盘，这个模块不碰文件。
pub fn build_items(
    schema_config: &Value,
    switcher_config: &Value,
    context: &mut Context,
    save_options: &HashSet<String>,
    option_writes: &mut Vec<(String, bool)>,
) -> Vec<SwitcherItem> {
    let switches = Switches::new(schema_config);
    let mut items: Vec<SwitcherItem> = Vec::new();
    // 记录每个组第一次出现的下标，便于随后整组同步
    let mut groups: Vec<Vec<String>> = Vec::new();

    for option in switches.all_options() {
        // 没有 `states:` 的开关在菜单里不显示
        if !option.has_state_label(0) {
            continue;
        }
        match option.kind {
            SwitchType::ToggleOption => {
                items.push(toggle_item(&option, context, save_options));
            }
            SwitchType::RadioGroup => {
                if option.option_index == 0 {
                    groups.push(option.group.clone());
                }
                items.push(radio_item(&option));
            }
        }
    }

    // C++ 在铺完之后对每个组做一次同步（含第一个打开项的判定与 ✓ 注释）
    for group in &groups {
        sync_radio_group(group, context, &mut items, save_options, option_writes);
    }

    // 折叠：`_fold_options` 打开时把整段开关压成一行
    if context.get_option("_fold_options") {
        let style = FoldedStyle::from_config(switcher_config);
        let mut labels: Vec<String> = Vec::new();
        for option in switches.all_options() {
            match option.kind {
                SwitchType::ToggleOption => {
                    let current = context.get_option(&option.option_name) as usize;
                    if let Some(label) = option.state_label(current, style.abbreviate) {
                        labels.push(label);
                    }
                }
                SwitchType::RadioGroup => {
                    // 单选组只显示"当前打开的那个"
                    if context.get_option(&option.option_name)
                        && let Some(label) = option.state_label(option.option_index, style.abbreviate)
                    {
                        labels.push(label);
                    }
                }
            }
        }
        // 只有一项时不值得折叠（C++ `if (folded_options->size() > 1)`）
        if labels.len() > 1 {
            let text = format!(
                "{}{}{}",
                style.prefix,
                labels.join(&style.separator),
                style.suffix
            );
            items.clear();
            items.push(SwitcherItem {
                candidate: Candidate::new("unfold", 0, 0, text, 0.0),
                command: SwitcherCommand::UnfoldOptions,
            });
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::switcher::SwitcherCommand;
    use serde_json::json;

    fn schema_config() -> Value {
        json!({
            "switches": [
                { "name": "ascii_mode", "states": ["中文", "西文"] },
                { "options": ["zh_simp", "zh_trad"], "states": ["简体", "繁體"] },
                { "name": "full_shape", "states": ["半角", "全角"] },
                { "name": "no_label" }
            ]
        })
    }

    fn build(ctx: &mut Context, fold: bool) -> Vec<SwitcherItem> {
        ctx.set_option("_fold_options", fold);
        let mut writes = Vec::new();
        build_items(
            &schema_config(),
            &json!({}),
            ctx,
            &HashSet::new(),
            &mut writes,
        )
    }

    #[test]
    fn builds_one_item_per_toggle_and_radio_option() {
        let mut ctx = Context::new();
        let items = build(&mut ctx, false);
        let texts: Vec<&str> = items.iter().map(|i| i.candidate.text.as_str()).collect();
        // `no_label` 没 states ⇒ 跳过
        assert_eq!(texts, vec!["中文", "简体", "繁體", "半角"]);
    }

    #[test]
    fn toggle_comment_shows_the_target_state() {
        let mut ctx = Context::new();
        let items = build(&mut ctx, false);
        assert_eq!(items[0].candidate.comment, "\u{2192} 西文", "关闭时应提示切到西文");
        ctx.set_option("ascii_mode", true);
        let items = build(&mut ctx, false);
        assert_eq!(items[0].candidate.text, "西文");
        assert_eq!(items[0].candidate.comment, "\u{2192} 中文");
    }

    #[test]
    fn radio_group_marks_the_selected_one() {
        let mut ctx = Context::new();
        let items = build(&mut ctx, false);
        // 组内第一条默认打开
        assert_eq!(items[1].candidate.comment, RADIO_SELECTED);
        assert_eq!(items[2].candidate.comment, "");
        assert!(
            ctx.get_option("zh_simp"),
            "建菜单时就该把组内第一项同步成打开"
        );
    }

    #[test]
    fn radio_group_follows_existing_selection() {
        let mut ctx = Context::new();
        ctx.set_option("zh_trad", true);
        let items = build(&mut ctx, false);
        assert_eq!(items[1].candidate.comment, "");
        assert_eq!(items[2].candidate.comment, RADIO_SELECTED);
    }

    #[test]
    fn fold_collapses_everything_into_one_unfold_item() {
        let mut ctx = Context::new();
        let items = build(&mut ctx, true);
        assert_eq!(items.len(), 1, "折叠后只剩一条");
        assert_eq!(items[0].candidate.kind, "unfold");
        // 中文 + 简体（组内选中）+ 半角，空格分隔
        assert_eq!(items[0].candidate.text, "中文 简体 半角");
        assert!(matches!(
            items[0].command,
            SwitcherCommand::UnfoldOptions
        ));
    }

    #[test]
    fn abbreviate_options_shortens_labels() {
        let mut ctx = Context::new();
        ctx.set_option("_fold_options", true);
        let mut writes = Vec::new();
        let items = build_items(
            &schema_config(),
            &json!({ "switcher": { "abbreviate_options": true } }),
            &mut ctx,
            &HashSet::new(),
            &mut writes,
        );
        assert_eq!(items[0].candidate.text, "中 简 半", "只取首字");
    }

    #[test]
    fn fold_with_a_single_label_does_not_collapse() {
        // 只配一个带标签的开关 ⇒ 折叠没意义，保持原样
        let mut ctx = Context::new();
        ctx.set_option("_fold_options", true);
        let mut writes = Vec::new();
        let items = build_items(
            &json!({ "switches": [{ "name": "only", "states": ["开", "关"] }] }),
            &json!({}),
            &mut ctx,
            &HashSet::new(),
            &mut writes,
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].candidate.kind, "switch", "没折叠，还是普通开关");
    }

    #[test]
    fn auto_save_flags_come_from_save_options() {
        let mut ctx = Context::new();
        let mut save = HashSet::new();
        save.insert("full_shape".to_string());
        let mut writes = Vec::new();
        let items = build_items(&schema_config(), &json!({}), &mut ctx, &save, &mut writes);

        let toggle = items
            .iter()
            .find(|i| matches!(&i.command, SwitcherCommand::Toggle { option, .. } if option == "full_shape"))
            .expect("应有 full_shape");
        match &toggle.command {
            SwitcherCommand::Toggle { auto_save, target, .. } => {
                assert!(auto_save, "full_shape 在 save_options 里");
                assert!(*target, "默认是关，所以目标是开");
            }
            _ => unreachable!(),
        }
        let ascii = items
            .iter()
            .find(|i| matches!(&i.command, SwitcherCommand::Toggle { option, .. } if option == "ascii_mode"))
            .unwrap();
        match &ascii.command {
            SwitcherCommand::Toggle { auto_save, .. } => assert!(!auto_save),
            _ => unreachable!(),
        }
    }
}
