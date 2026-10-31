//! 提交历史（对照 C++ `commit_history.h` / `commit_history.cc`）。
//!
//! 这是**跨提交**的一份小账本，记最近 20 条"用户到底上屏了什么"。四个下游要靠它：
//!
//! | 消费者 | 用哪部分 |
//! |---|---|
//! | `history_translator` | 全部非 `thru` 记录，做"最近输入"候选 |
//! | `punctuator` | 末条的 `text` 与 `kind`，判断 `3.14` / `1,000` 这类数字串 |
//! | `table_translator` | 尾部连续的 `table`/`user_table`/`sentence`/`uniquified`，拼短语去编码 |
//! | `script_translator` / `table_translator` | `latest_text()`，句首的上下文提示 |
//!
//! 记录分三类：
//!
//! * `thru` —— 没被任何 processor 吃掉的**可打印 ASCII 键**（C++ 在上屏后补记，
//!   所以 `3` `.` `1` `4` 打完后历史里是四条 `thru`）；
//! * `raw` —— 原样上屏的串（未翻译的段、`ascii_composer` 的临时西文结算）；
//! * 其余 —— 候选自己的 `kind`（`table` / `script` / `user_table` / `punct` …）。
//!
//! ## 三个必须照抄的细节
//!
//! 1. **`BackSpace` / `Return` 清空整本账**（`commit_history.cc:26-28`）。删除后上下文
//!    没了，留着旧记录会拿错上下文拼短语。
//! 2. **确认段（`status >= Confirmed`）终结一条记录**：后面同类型的候选不能续写，
//!    否则"两次确认的单字"会被并成一个词。
//! 3. **无候选的段不重置"当前记录指针"**（`commit_history.cc:58-61`）。这是 C++ 的
//!    既有行为：`Push({"raw", ...})` 之后 `last` 仍指向**上上条**记录，于是后面
//!    同类型的候选会追加到那条上。看着像 bug，但下游（尤其 `encode_commit_history`）
//!    见到的顺序就是它，照抄。

use crate::engine::composition::Composition;
use crate::engine::key_event::KeyEvent;
use crate::engine::segmentation::SegStatus;

/// C++ `CommitHistory::kMaxRecords`。
pub const K_MAX_RECORDS: usize = 20;

/// `thru` 记录的 `kind`：没有被翻译、直接作为按键穿过的字符。
pub const KIND_THRU: &str = "thru";
/// `raw` 记录的 `kind`：原样上屏的串。
pub const KIND_RAW: &str = "raw";

/// X11 keysym：退格。
const XK_BACKSPACE: u32 = 0xff08;
/// X11 keysym：回车。
const XK_RETURN: u32 = 0xff0d;

/// 一条提交记录（C++ `CommitRecord`）。
///
/// `kind` 对应 C++ 的 `type` —— 改名是因为 `type` 在 Rust 里是关键字。
/// 语义完全一致：它就是候选的 `kind`（`Candidate::kind`）。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CommitRecord {
    pub kind: String,
    pub text: String,
}

impl CommitRecord {
    pub fn new(kind: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            text: text.into(),
        }
    }

    /// `CommitRecord(int keycode)`：`type = "thru"`、`text` 是那一个字符。
    pub fn thru(ch: char) -> Self {
        Self::new(KIND_THRU, ch.to_string())
    }

    pub fn is_thru(&self) -> bool {
        self.kind == KIND_THRU
    }

    pub fn is_raw(&self) -> bool {
        self.kind == KIND_RAW
    }
}

/// 提交历史（C++ `CommitHistory : list<CommitRecord>`）。
///
/// 内部用 `Vec` 而不是 `VecDeque`：上限只有 20，`remove(0)` 的开销可以忽略，
/// 换来的是 [`Self::records`] 能直接返回切片。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitHistory {
    records: Vec<CommitRecord>,
}

impl CommitHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// `CommitHistory::Push(const CommitRecord&)`：追加并裁到上限。
    pub fn push(&mut self, record: CommitRecord) {
        self.records.push(record);
        while self.records.len() > K_MAX_RECORDS {
            self.records.remove(0);
        }
    }

    /// `CommitHistory::Push(const KeyEvent&)`：只在**无修饰键**时记。
    ///
    /// 退格/回车清空（上下文断了），可打印 ASCII 记一条 `thru`，其余忽略
    /// —— 方向键、F1 这些不该出现在"最近输入"里。
    pub fn push_key(&mut self, key: &KeyEvent) {
        if key.modifier != 0 {
            return;
        }
        match key.keycode {
            XK_BACKSPACE | XK_RETURN => self.clear(),
            0x20..=0x7e => self.push(CommitRecord::thru(char::from(key.keycode as u8))),
            _ => {}
        }
    }

    /// `CommitHistory::Push(const Composition&, const string& input)`。
    ///
    /// 逐段走：有选中候选就按它的 `kind`/`text` 记账（相邻同类型**续写**一条），
    /// 没候选就把那段原始输入记成 `raw`；最后把越过末段的尾巴也记成 `raw`。
    ///
    /// 第 3 条"硬约束"（见模块注释）体现在 `last` 的处理上：`raw` 那条**不**重置
    /// `last`，所以后续同类型候选仍会续写到更早的那条记录里。
    pub fn push_composition(&mut self, composition: &Composition, input: &str) {
        let mut last: Option<usize> = None;
        let mut end = 0usize;
        for seg in &composition.segmentation.segments {
            if let Some(cand) = seg.get_selected_candidate() {
                let joined = match last {
                    Some(i) => match self.records.get_mut(i) {
                        // C++：`if (last && last->type == cand->type())`
                        Some(rec) if rec.kind == cand.kind => {
                            rec.text.push_str(&cand.text);
                            true
                        }
                        _ => false,
                    },
                    None => false,
                };
                if !joined {
                    let (index, _) = self.append(CommitRecord::new(&cand.kind, &cand.text));
                    last = Some(index);
                }
                if seg.status >= SegStatus::Confirmed {
                    // `last = NULL`：确认段终结一条记录
                    last = None;
                }
                end = cand.end;
            } else {
                let lo = seg.start.min(input.len());
                let hi = seg.end.min(input.len());
                let (_, dropped) =
                    self.append(CommitRecord::new(KIND_RAW, input.get(lo..hi).unwrap_or("")));
                // 注意：C++ 这里**不**动 `last`（`commit_history.cc:58-61`），照抄；
                // 但索引要跟着队首平移，否则续写会写错记录。
                last = rebase(last, dropped);
                end = seg.end;
            }
        }
        if input.len() > end {
            let (_, dropped) = self.append(CommitRecord::new(KIND_RAW, input.get(end..).unwrap_or("")));
            let _ = rebase(last, dropped);
        }
    }

    /// 追加一条记录。返回它在 `records` 里的下标，以及"这次是否挤掉了队首"。
    ///
    /// `push_composition` 需要下标来续写（C++ 的 `last` 是个稳定指针），
    /// 而挤掉队首会让所有下标平移，所以两件事一起返回。
    fn append(&mut self, record: CommitRecord) -> (usize, bool) {
        self.records.push(record);
        let dropped = self.records.len() > K_MAX_RECORDS;
        if dropped {
            self.records.remove(0);
        }
        (self.records.len() - 1, dropped)
    }

    /// `CommitHistory::repr()`：`[type]text[type]text…`，只用于日志。
    pub fn repr(&self) -> String {
        let mut out = String::new();
        for record in &self.records {
            out.push('[');
            out.push_str(&record.kind);
            out.push(']');
            out.push_str(&record.text);
        }
        out
    }

    /// `latest_text()`：末条的文本，空历史给空串。
    pub fn latest_text(&self) -> String {
        self.records
            .last()
            .map(|r| r.text.clone())
            .unwrap_or_default()
    }

    pub fn last(&self) -> Option<&CommitRecord> {
        self.records.last()
    }

    /// 从新到旧遍历（C++ 里是 `rbegin()` / `rend()`）。
    pub fn iter_rev(&self) -> impl DoubleEndedIterator<Item = &CommitRecord> {
        self.records.iter().rev()
    }

    pub fn records(&self) -> &[CommitRecord] {
        &self.records
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn clear(&mut self) {
        self.records.clear();
    }
}

/// 队首被挤掉后，把记录下标往前挪一位（指向 0 的那条已不存在，给 `None`）。
///
/// C++ 的 `last` 是 `std::list` 的节点指针 —— 插入/删头都不影响它指向的元素；
/// Rust 用下标代替，就必须手动平移。
fn rebase(index: Option<usize>, dropped: bool) -> Option<usize> {
    if !dropped {
        return index;
    }
    index.and_then(|i| if i == 0 { None } else { Some(i - 1) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::candidate::Candidate;
    use crate::engine::composition::Composition;
    use crate::engine::gear::translation::VecTranslation;
    use crate::engine::menu::Menu;
    use crate::engine::segmentation::Segment;

    /// 懒化后没有 `Menu::add`,候选只能经"翻译流"装进菜单(测试用的最小流)。
    fn menu_with(candidate: Candidate) -> Menu {
        let menu = Menu::new();
        menu.add_translation(Box::new(VecTranslation::new(vec![candidate])));
        menu
    }

    /// 造一个"一段、已选中某候选"的组合。
    fn composition_with_selection(kind: &str, text: &str, input: &str) -> Composition {
        let mut comp = Composition::new();
        comp.segmentation.input = input.to_string();
        let mut seg = Segment::new(0, input.len());
        seg.status = SegStatus::Selected;
        seg.menu = Some(menu_with(Candidate::new(kind, 0, input.len(), text, 1.0)));
        seg.selected_index = 0;
        comp.segmentation.segments.push(seg);
        comp
    }

    #[test]
    fn push_key_records_printable_ascii_as_thru() {
        let mut h = CommitHistory::new();
        h.push_key(&KeyEvent::new(b'3' as u32, 0));
        h.push_key(&KeyEvent::new(b'.' as u32, 0));
        assert_eq!(h.len(), 2);
        assert_eq!(h.records()[0], CommitRecord::new(KIND_THRU, "3"));
        assert_eq!(h.latest_text(), ".");
    }

    #[test]
    fn push_key_ignores_modified_and_non_printable_keys() {
        let mut h = CommitHistory::new();
        // 带修饰键：不记
        h.push_key(&KeyEvent::new(b'a' as u32, crate::engine::key_event::K_CONTROL_MASK));
        // 方向键 / F1：不记
        h.push_key(&KeyEvent::new(0xff51, 0));
        assert!(h.is_empty(), "带修饰键或非可打印键都不该入账");
    }

    #[test]
    fn push_key_clears_on_backspace_and_return() {
        let mut h = CommitHistory::new();
        h.push_key(&KeyEvent::new(b'1' as u32, 0));
        h.push_key(&KeyEvent::new(b'2' as u32, 0));
        assert_eq!(h.len(), 2);
        h.push_key(&KeyEvent::new(XK_BACKSPACE, 0));
        assert!(h.is_empty(), "退格应清空整本账");
        h.push_key(&KeyEvent::new(b'9' as u32, 0));
        h.push_key(&KeyEvent::new(XK_RETURN, 0));
        assert!(h.is_empty(), "回车也应清空");
    }

    #[test]
    fn history_is_capped_at_twenty_records() {
        let mut h = CommitHistory::new();
        for i in 0..25u8 {
            h.push(CommitRecord::new("table", ((b'a' + i) as char).to_string()));
        }
        assert_eq!(h.len(), K_MAX_RECORDS, "只留最近 20 条");
        assert_eq!(h.records()[0].text, "f", "最老的 5 条（a..e）被挤掉");
    }

    #[test]
    fn push_composition_joins_adjacent_same_kind() {
        let mut comp = Composition::new();
        comp.segmentation.input = "nihao".to_string();
        for (start, end, text) in [(0usize, 2usize, "你"), (2, 5, "好")] {
            let mut seg = Segment::new(start, end);
            seg.status = SegStatus::Selected;
            seg.menu = Some(menu_with(Candidate::new("script", start, end, text, 1.0)));
            comp.segmentation.segments.push(seg);
        }
        let mut h = CommitHistory::new();
        h.push_composition(&comp, "nihao");
        assert_eq!(h.len(), 1, "相邻同类型并成一条");
        assert_eq!(h.records()[0], CommitRecord::new("script", "你好"));
    }

    #[test]
    fn confirmed_segment_terminates_the_record() {
        let mut comp = Composition::new();
        comp.segmentation.input = "nihao".to_string();
        let mut first = Segment::new(0, 2);
        first.status = SegStatus::Confirmed;
        first.menu = Some(menu_with(Candidate::new("script", 0, 2, "你", 1.0)));
        comp.segmentation.segments.push(first);
        let mut second = Segment::new(2, 5);
        second.status = SegStatus::Selected;
        second.menu = Some(menu_with(Candidate::new("script", 2, 5, "好", 1.0)));
        comp.segmentation.segments.push(second);

        let mut h = CommitHistory::new();
        h.push_composition(&comp, "nihao");
        assert_eq!(h.len(), 2, "确认段终结记录，后面的同类型另起一条");
        assert_eq!(h.records()[0].text, "你");
        assert_eq!(h.records()[1].text, "好");
    }

    #[test]
    fn untranslated_tail_is_recorded_as_raw() {
        let mut comp = composition_with_selection("table", "我", "w");
        let mut h = CommitHistory::new();
        h.push_composition(&comp, "wxyz");
        assert_eq!(h.len(), 2);
        assert_eq!(h.records()[0], CommitRecord::new("table", "我"));
        assert_eq!(h.records()[1], CommitRecord::new(KIND_RAW, "xyz"));
        comp.segmentation.segments.clear();
    }

    #[test]
    fn segment_without_candidate_is_raw() {
        let mut comp = Composition::new();
        comp.segmentation.input = "abc".to_string();
        comp.segmentation.segments.push(Segment::new(0, 3));
        let mut h = CommitHistory::new();
        h.push_composition(&comp, "abc");
        assert_eq!(h.len(), 1);
        assert_eq!(h.records()[0], CommitRecord::new(KIND_RAW, "abc"));
    }
}
