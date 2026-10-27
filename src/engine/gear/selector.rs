//! `gears/selector.cc` 全量移植：候选导航 + 数字选词。
//!
//! C++ 的四张键位表（横/竖排 × 堆叠/线性）在这里编码成一张匹配表，方向由
//! 上下文选项 `_vertical` / `_linear`（兼容旧 `_horizontal`）在按键时决定——
//! 和 C++ 一样是**运行时**选表，不是装配期。
//!
//! # 与 C++ 的差异
//!
//! * KP_*（小键盘）键位 C++ 有绑，宿主键盘不产生该 keysym 时无差，暂未收录；
//! * `menu->Prepare(n)` 在急切菜单模型下就是 `candidate_count`。

use crate::engine::gear::processor::{ProcessResult, Processor};
use crate::engine::key_event::{
    KeyEvent, K_SUPER_MASK, XK_0, XK_9, XK_DOWN, XK_END, XK_HOME, XK_LEFT, XK_NEXT, XK_PRIOR,
    XK_RIGHT, XK_UP,
};
use crate::engine::Engine;

/// 候选导航动作（对照 `Selector` 的六个 Handler，返回"是否处理"）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    PreviousCandidate,
    NextCandidate,
    PreviousPage,
    NextPage,
    Home,
    End,
}

/// 一张键位表的一个条目。
struct Binding {
    keycode: u32,
    action: Action,
}

/// `Horizontal | Stacked`（默认）。
const BINDINGS_H_STACKED: &[Binding] = &[
    Binding { keycode: XK_UP, action: Action::PreviousCandidate },
    Binding { keycode: XK_DOWN, action: Action::NextCandidate },
    Binding { keycode: XK_PRIOR, action: Action::PreviousPage },
    Binding { keycode: XK_NEXT, action: Action::NextPage },
    Binding { keycode: XK_HOME, action: Action::Home },
    Binding { keycode: XK_END, action: Action::End },
];

/// `Horizontal | Linear`。
const BINDINGS_H_LINEAR: &[Binding] = &[
    Binding { keycode: XK_LEFT, action: Action::PreviousCandidate },
    Binding { keycode: XK_RIGHT, action: Action::NextCandidate },
    Binding { keycode: XK_UP, action: Action::PreviousPage },
    Binding { keycode: XK_DOWN, action: Action::NextPage },
    Binding { keycode: XK_PRIOR, action: Action::PreviousPage },
    Binding { keycode: XK_NEXT, action: Action::NextPage },
    Binding { keycode: XK_HOME, action: Action::Home },
    Binding { keycode: XK_END, action: Action::End },
];

/// `Vertical | Stacked`。
const BINDINGS_V_STACKED: &[Binding] = &[
    Binding { keycode: XK_RIGHT, action: Action::PreviousCandidate },
    Binding { keycode: XK_LEFT, action: Action::NextCandidate },
    Binding { keycode: XK_PRIOR, action: Action::PreviousPage },
    Binding { keycode: XK_NEXT, action: Action::NextPage },
    Binding { keycode: XK_HOME, action: Action::Home },
    Binding { keycode: XK_END, action: Action::End },
];

/// `Vertical | Linear`。
const BINDINGS_V_LINEAR: &[Binding] = &[
    Binding { keycode: XK_UP, action: Action::PreviousCandidate },
    Binding { keycode: XK_DOWN, action: Action::NextCandidate },
    Binding { keycode: XK_RIGHT, action: Action::PreviousPage },
    Binding { keycode: XK_LEFT, action: Action::NextPage },
    Binding { keycode: XK_PRIOR, action: Action::PreviousPage },
    Binding { keycode: XK_NEXT, action: Action::NextPage },
    Binding { keycode: XK_HOME, action: Action::Home },
    Binding { keycode: XK_END, action: Action::End },
];

pub struct Selector;

impl Selector {
    /// 竖排/线性判定（`is_vertical_text` / `is_linear_layout`）。
    fn orientation_bindings(engine: &Engine) -> &'static [Binding] {
        let ctx = &engine.context;
        let vertical = ctx.get_option("_vertical");
        let linear = ctx.get_option("_linear") || ctx.get_option("_horizontal");
        match (vertical, linear) {
            (false, false) => BINDINGS_H_STACKED,
            (false, true) => BINDINGS_H_LINEAR,
            (true, false) => BINDINGS_V_STACKED,
            (true, true) => BINDINGS_V_LINEAR,
        }
    }

    /// 打上 `paging` 标签（key_binder 的 `when: paging` 依赖它）。
    fn tag_paging(engine: &mut Engine) {
        if let Some(back) = engine.context.composition.back_mut() {
            if !back.tags.iter().any(|t| t == "paging") {
                back.tags.push("paging".to_string());
            }
        }
    }

    /// `Selector::PreviousCandidate`。
    fn previous_candidate(engine: &mut Engine, linear: bool) -> bool {
        // 线性布局且光标不在输入尾：让 navigator 处理（C++ 同款让路）
        if linear && engine.context.caret_pos < engine.context.input.len() {
            return false;
        }
        let index = engine.context.highlight_index();
        if index == 0 {
            // 堆叠布局下退无可退仍算处理（C++ 返回 !is_linear）
            return !linear;
        }
        if engine.context.highlight(index - 1) {
            Self::tag_paging(engine);
        }
        true
    }

    /// `Selector::NextCandidate`。
    fn next_candidate(engine: &mut Engine, linear: bool) -> bool {
        if linear && engine.context.caret_pos < engine.context.input.len() {
            return false;
        }
        let index = engine.context.highlight_index();
        let count = engine.context.menu_candidate_count();
        if count == 0 || index + 1 >= count {
            // 已到末尾：消费按键但不移动（C++ `candidate_count <= index → true`）
            return true;
        }
        if engine.context.highlight(index + 1) {
            Self::tag_paging(engine);
        }
        true
    }

    /// `Selector::PreviousPage`。
    fn previous_page(engine: &mut Engine) -> bool {
        let page_size = engine.schema.page_size.max(1) as usize;
        let index = engine.context.highlight_index();
        let target = index.saturating_sub(page_size);
        if engine.context.highlight(target) {
            Self::tag_paging(engine);
        }
        true
    }

    /// `Selector::NextPage`。
    fn next_page(engine: &mut Engine) -> bool {
        let page_size = engine.schema.page_size.max(1) as usize;
        let index = engine.context.highlight_index();
        let count = engine.context.menu_candidate_count();
        if count == 0 {
            return false;
        }
        let target = index + page_size;
        let page_start = (target / page_size) * page_size;
        if count <= page_start {
            // 超过最后一页：`page_down_cycle` 开着就回第一页，否则消费但不动
            if !engine.context.get_option("_page_down_cycle") {
                return true;
            }
            if engine.context.highlight(0) {
                Self::tag_paging(engine);
            }
            return true;
        }
        let target = target.min(count - 1);
        if engine.context.highlight(target) {
            Self::tag_paging(engine);
        }
        true
    }

    /// `Selector::Home`。
    fn home(engine: &mut Engine) -> bool {
        if engine.context.highlight_index() > 0 {
            if engine.context.highlight(0) {
                Self::tag_paging(engine);
            }
            return true;
        }
        // 让 navigator 处理（C++ 返回 false）
        false
    }

    /// `Selector::End`。
    fn end(engine: &mut Engine) -> bool {
        if engine.context.caret_pos < engine.context.input.len() {
            return false; // navigator 应处理
        }
        Self::home(engine)
    }

    /// `Selector::SelectCandidateAt`：当前页内第 `index` 个候选。
    fn select_candidate_at(engine: &mut Engine, index: usize) -> bool {
        let page_size = engine.schema.page_size.max(1);
        if index >= page_size {
            return false;
        }
        let selected = engine.context.highlight_index();
        let page_start = (selected / page_size) * page_size;
        engine.context.select(page_start + index).is_some()
    }
}

impl Processor for Selector {
    fn process_key_event(&mut self, engine: &mut Engine, key: &KeyEvent) -> ProcessResult {
        if key.release() || key.alt() || key.modifier & K_SUPER_MASK != 0 {
            return ProcessResult::Noop;
        }
        // C++：`composition.empty()` → Noop；末段无菜单或 `raw` 段 → Noop
        if engine.context.composition.empty() {
            return ProcessResult::Noop;
        }
        {
            let Some(current) = engine.context.composition.back() else {
                return ProcessResult::Noop;
            };
            if current.menu.is_none() || current.has_tag("raw") {
                return ProcessResult::Noop;
            }
        }

        let linear =
            engine.context.get_option("_linear") || engine.context.get_option("_horizontal");

        // 1) 方向/翻页键位表
        let bindings = Self::orientation_bindings(engine);
        if !key.ctrl()
            && let Some(binding) = bindings.iter().find(|b| b.keycode == key.keycode)
        {
            let handled = match binding.action {
                Action::PreviousCandidate => Self::previous_candidate(engine, linear),
                Action::NextCandidate => Self::next_candidate(engine, linear),
                Action::PreviousPage => Self::previous_page(engine),
                Action::NextPage => Self::next_page(engine),
                Action::Home => Self::home(engine),
                Action::End => Self::end(engine),
            };
            if handled {
                return ProcessResult::Accepted;
            }
        }

        // 2) 选词键：`menu/alternative_select_keys` 优先，其次数字 1..0
        //    （C++：`(ch - XK_0 + 9) % 10`，即 1→0 … 9→8、0→9）
        let ch = key.keycode;
        if !key.ctrl() && (0x20..0x7f).contains(&ch) {
            let select_keys = engine.schema.select_keys.as_str();
            let index = if !select_keys.is_empty() {
                select_keys.find(ch as u8 as char).map(|pos| pos as isize)
            } else if (XK_0..=XK_9).contains(&ch) {
                Some((((ch - XK_0) + 9) % 10) as isize)
            } else {
                None
            };
            if let Some(index) = index.filter(|i| *i >= 0) {
                if Self::select_candidate_at(engine, index as usize) {
                    return ProcessResult::Accepted;
                }
            }
        }
        ProcessResult::Noop
    }
}
