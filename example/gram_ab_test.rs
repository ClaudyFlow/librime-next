//! gram 开关对照:同一组句子,WEASEL_GRAM 开/关的首候选质量对比。
//! 判据:目标句是否在 top-1 / top-3(召回)。
use librime_next::engine::Session;
use std::path::PathBuf;

const CASES: &[(&str, &str)] = &[
    ("jintiantianqihenhao", "今天天气很好"),
    ("womenyiqiqu", "我们一起去"),
    ("zhongguorenmin", "中国人民"),
    ("xianzaigan Shenme", "现在干什么"),
    ("womingtianbushangban", "我明天不上班"),
    ("tamenlianmangpaoguolai", "他们连忙跑过来"),
    ("jihuoshiquanmieshuile", "大火及时扑灭了"),
    ("zhengfaguanligongzi", "政法干警工资"),
    ("yidaoqiheidaduixiang", "一到七号的对象"),
    ("qingchunyongbuhui", "青春永不会"),
];

fn top_candidates(build: &PathBuf, schema: &str, input: &str, n: usize) -> Vec<String> {
    let mut s = Session::create(build, schema);
    if s.engine.is_none() { return Vec::new(); }
    for c in input.chars() { s.process_key(c as u32, 0); }
    let ctx = s.context().unwrap();
    let mut out = Vec::new();
    if let Some(seg) = ctx.composition.segmentation.segments.last()
        && let Some(menu) = seg.menu.as_ref()
    {
        let count = menu.prepare(n);
        for i in 0..count {
            if let Some(c) = menu.candidate_at(i) { out.push(c.text); }
        }
    }
    out
}

fn main() {
    let appdata = std::env::var("APPDATA").unwrap();
    let build = PathBuf::from(format!("{appdata}/Rime-Next/build"));
    let schema = "pinyin.luna";
    println!("用例数 {}", CASES.len());
    for (label, env_off) in [("gram 开", false), ("gram 关", true)] {
        unsafe {
            if env_off { std::env::set_var("WEASEL_GRAM", "off"); } else { std::env::remove_var("WEASEL_GRAM"); }
        }
        let (mut hit1, mut hit3) = (0usize, 0usize);
        for (input, expected) in CASES {
            let cands = top_candidates(&build, schema, input, 3);
            if cands.first().map_or(false, |t| t == *expected) { hit1 += 1; }
            if cands.iter().take(3).any(|t| t == expected) { hit3 += 1; }
            let shown: Vec<String> = cands.iter().take(3).cloned().collect();
            println!("[{label}] {input} → {shown:?} {}", if cands.first().map_or(false,|t|t==*expected) {"✓1"} else if cands.iter().take(3).any(|t|t==expected) {"✓3"} else {"✗"});
        }
        println!("== {label}: top1 {hit1}/{} top3 {hit3}/{} ==", CASES.len(), CASES.len());
    }
}
