//! 候选导航处理器：上下 / 翻页移动高亮（不直接提交，对照 C++ `gears/selector`）。

use crate::engine::gear::processor::{ProcessResult, Processor};
use crate::engine::key_event::{KeyEvent, XK_DOWN, XK_LEFT, XK_NEXT, XK_PRIOR, XK_RIGHT, XK_UP};

pub struct Selector;

impl Processor for Selector {
    fn process_key_event(&mut self, engine: &mut crate::engine::Engine, key: &KeyEvent) -> ProcessResult {
        let page = engine.context.page_size.max(1) as isize;
        match key.keycode {
            XK_UP | XK_LEFT => {
                engine.context.move_highlight(-1);
                ProcessResult::Accepted
            }
            XK_DOWN | XK_RIGHT => {
                engine.context.move_highlight(1);
                ProcessResult::Accepted
            }
            XK_PRIOR => {
                engine.context.move_highlight(-page);
                ProcessResult::Accepted
            }
            XK_NEXT => {
                engine.context.move_highlight(page);
                ProcessResult::Accepted
            }
            _ => ProcessResult::Noop,
        }
    }
}
