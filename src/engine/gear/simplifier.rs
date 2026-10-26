//! `gear/simplifier.cc`：把候选文本用 OpenCC 转换（简繁 / 繁简 / 地域变体）。
//!
//! 与原版一致：
//! * `opencc_config` 默认 `t2s.json`（繁 → 简）；可显式指定（如 `t2tw.json`）。
//! * `option_name` 决定**何时生效**：配了就只在对应开关打开时转换；不配则恒生效。
//! * 只动候选 `text`，不动 `comment` / `preedit`（原版 `tips` 提示变体暂未实现）。

use std::path::Path;

use crate::dict::opencc::Opencc;
use crate::engine::candidate::Candidate;
use crate::engine::context::Context;
use crate::engine::gear::filter::Filter;
use crate::engine::segmentation::Segment;

/// 简繁转换过滤器（对照 C++ `Simplifier`）。
pub struct Simplifier {
    opencc: Opencc,
    option_name: Option<String>,
}

impl Simplifier {
    /// `opencc_dir` 通常是 `<shared_data>/opencc`（生产里等于 `build_dir.parent()/opencc`）。
    pub fn new(
        opencc_dir: &Path,
        config_name: &str,
        option_name: Option<String>,
    ) -> Option<Self> {
        let cfg = opencc_dir.join(config_name);
        let opencc = Opencc::load(&cfg).ok()?;
        Some(Self { opencc, option_name })
    }
}

impl Filter for Simplifier {
    fn applies_to_segment(&self, ctx: &Context, _segment: &Segment) -> bool {
        match &self.option_name {
            // 没配开关 → 恒生效
            None => true,
            // 配了开关 → 只在开关打开时生效
            Some(name) => ctx.get_option(name),
        }
    }

    fn apply(&self, _ctx: &Context, candidates: &mut Vec<Candidate>) {
        for cand in candidates.iter_mut() {
            cand.text = self.opencc.convert(&cand.text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::Context;

    fn opencc_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("Rime-Next")
            .join("opencc")
    }

    fn cand(text: &str) -> Candidate {
        Candidate::new("script", 0, text.len(), text.to_string(), 1.0)
    }

    #[test]
    fn always_on_converts_text() {
        let s = Simplifier::new(&opencc_dir(), "s2t.json", None).expect("加载 s2t");
        let mut list = vec![cand("汉语")];
        s.apply(&Context::new(), &mut list);
        assert_eq!(list[0].text, "漢語");
    }

    #[test]
    fn t2s_converts_text() {
        let s = Simplifier::new(&opencc_dir(), "t2s.json", None).expect("加载 t2s");
        let mut list = vec![cand("漢語")];
        s.apply(&Context::new(), &mut list);
        assert_eq!(list[0].text, "汉语");
    }

    #[test]
    fn gated_by_option() {
        let s = Simplifier::new(&opencc_dir(), "t2s.json", Some("zh_simp".to_string()))
            .expect("加载 t2s");
        let seg = Segment::new(0, 0);

        // 开关关着 → applies_to_segment 返回 false，apply 不会被调用，文本不变
        let ctx = Context::new();
        assert!(!s.applies_to_segment(&ctx, &seg), "zh_simp 关着不应套用");
        let mut off = vec![cand("漢語")];
        if s.applies_to_segment(&ctx, &seg) {
            s.apply(&ctx, &mut off);
        }
        assert_eq!(off[0].text, "漢語", "zh_simp 关着应原样");

        // 开关开着 → applies_to_segment 返回 true，apply 转换
        let mut ctx2 = Context::new();
        ctx2.set_option("zh_simp", true);
        assert!(s.applies_to_segment(&ctx2, &seg), "zh_simp 开着应套用");
        let mut on = vec![cand("漢語")];
        s.apply(&ctx2, &mut on);
        assert_eq!(on[0].text, "汉语");
    }

    #[test]
    fn applies_to_segment_reflects_option() {
        let s = Simplifier::new(&opencc_dir(), "t2s.json", Some("zh_simp".to_string()))
            .expect("加载 t2s");
        let seg = Segment::new(0, 0);
        assert!(!s.applies_to_segment(&Context::new(), &seg), "关着不该套用");
        let mut ctx = Context::new();
        ctx.set_option("zh_simp", true);
        assert!(s.applies_to_segment(&ctx, &seg), "开着该套用");
    }
}
