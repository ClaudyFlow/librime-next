//! 跨提交前缀缓存复用的**可行性验证**（2026-11-08）。
//!
//! 问题：下一次提交给神经预测的上下文，是否严格等于「上一次上下文 + 刚上屏的词」？
//! 只有严格追加时，上一次的 K/V 缓存才能直接续上，省掉第一步整段前向。
//!
//! 这里不加载模型（验证的是**文本关系**，与模型无关），只在 temp 副本上真实提交
//! 一串不同的词，把每次 `pre_commit_context` / 末条提交记录 / 拼出来的 context
//! 打印出来，并统计「严格追加」与「完全相同」各命中几次。
//!
//! 只在 `%TEMP%\rime_perf_copy` 上写，不碰 `%APPDATA%` 与 `C:\Program Files`。
use librime_next::engine::Session;
use std::path::PathBuf;

/// 与 `runtime.rs` 的 `is_neural_predict_word` 同一判据：上一次提交是否真的跑了
/// 同步神经预测（没跑就不会有可复用的缓存）。
fn seeds_cache(word: &str) -> bool {
    word.chars().count() > 1
        && word
            .chars()
            .any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c))
}

/// 打印端头（只在首行用）。
fn main() {
    let copy = std::env::temp_dir().join("rime_perf_copy");
    let build = copy.join("build");
    let data = PathBuf::from(r"C:\Program Files\Rime-Next\Weasel-Next\data");
    let mut session =
        Session::create_with_roots(&build, "luna_pinyin_simp", &[copy.clone(), data]);
    if session.engine.is_none() {
        println!("引擎加载失败（temp 副本不完整）");
        return;
    }

    // 模拟一段连续输入：每次打一串拼音 + 空格上屏，词与词都不同。
    let codes = [
        "nihao", "jintian", "tianqi", "henhao", "women", "xiexie", "zaijian", "mingtian",
    ];

    let mut prev: Option<String> = None;
    let mut prev_seeded = false;
    let (mut append_hits, mut exact_hits, mut total) = (0usize, 0usize, 0usize);
    println!("i  pre(上次末条)  word(本次上屏)  context=pre+word  严格追加?  完全相同?");
    for (i, code) in codes.iter().enumerate() {
        for ch in code.chars() {
            session.process_key(ch as u32, 0);
        }
        session.process_key(0x20, 0);
        let committed = session.take_commit().unwrap_or_default();
        let engine = session.engine.as_ref().expect("引擎已确认存在");
        let pre = engine.context.pre_commit_context.clone();
        let word = engine
            .context
            .commit_history
            .last()
            .map(|record| record.text.clone())
            .unwrap_or_default();
        let context = format!("{pre}{word}");
        let chars = context.chars().count();

        let (append, exact) = match &prev {
            Some(old) => (
                // 缓存只有在"上一次提交真的跑过神经"时才存在。
                prev_seeded && context.starts_with(old.as_str()),
                prev_seeded && &context == old,
            ),
            None => (false, false),
        };
        if prev.is_some() {
            total += 1;
            append_hits += usize::from(append);
            exact_hits += usize::from(exact);
        }
        println!(
            "{i}  {pre:?}  {word:?}({committed:?})  {context:?}({chars}字)  append={append} exact={exact}"
        );
        prev_seeded = seeds_cache(&word);
        prev = Some(context);
    }

    println!(
        "严格追加命中 {append_hits}/{total}，完全相同命中 {exact_hits}/{total}（仅统计上一次确实跑过神经的转移）"
    );
}
