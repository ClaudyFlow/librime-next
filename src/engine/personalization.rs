//! R-12 个性化：**用户词频衰减 + 最近选择置顶（带 TTL）**。
//!
//! 目标只有两条，都是"候选打分时读、上屏时写"：
//!
//! 1. **词频**：每次上屏给该候选文本 `+1`（带时间戳），打分时按**天**做指数衰减
//!    （半衰期 [`HALF_LIFE_DAYS`] 天），`bonus = weight * ln(1 + 衰减后的次数)`。
//!    衰减在**读**的时候算，不落盘重写 —— 表只增不缩，旧记录自然变淡。
//! 2. **最近选择置顶**：记录 `(输入串 → 文本, 音节码, 时间戳)`，同一串在
//!    [`PIN_TTL_SECS`] 秒内再打时，把那条**单独**放进最前面（TTL 过了自然失效）。
//!    置顶**不重放存下来的音节码**：只在当前方案的候选流里找同文本候选，找到才
//!    提前；否则跳过。这样切到码表不同的方案时，不会用外方案的码拼出一条本方案
//!    根本不存在的候选（存储格式不变，旧 TSV 照常读）。
//!
//! # 与既有 `user_ngram` 的关系
//!
//! `user_ngram` 已经做了**无衰减**的词频/转移加成，这里不再重复造一套上下文模型：
//! * 本模块只吃"候选文本"这一维，不碰上文、不做转移、不做预测；
//! * 分数是**按天衰减**的，长期不用的词会自己沉下去（R-12 的核心诉求）；
//! * 两条加成都是"只加不减"，叠加时不会互相抵消。
//!
//! # 开关与存储
//!
//! 走 `translator/enable_personalization`（2026-11-09 起默认 **true**）。
//! 关闭时：不写记录、不包打分、不加置顶层 —— 候选与基线**逐位一致**。
//!
//! 默认打开的理由：空表时是纯 no-op；pin 只有 30 分钟 TTL，词频按 30 天
//! 半衰期衰减，最坏情况是「一次误上屏在 30 分钟内轻微置顶」；`PIN_BOOST`
//! 已从 `1e15` 收到 `1e6`，不再改写码长/层级台阶。
//!
//! 存储是本地 TSV（`%APPDATA%/Rime-Next/user_preference.tsv`），与 `user_ngram.tsv`
//! 同目录；不引入依赖、不加载模型。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::gear::dict::vocabulary::Code;
use crate::engine::candidate::Candidate;
use crate::gear::translator::translation::Translation;

/// 衰减半衰期（天）。30 天没再选过的词，权重剩一半。
pub const HALF_LIFE_DAYS: f64 = 30.0;

/// "最近一次选择"的有效期（秒）：约 30 分钟。
pub const PIN_TTL_SECS: i64 = 1800;

/// 词频加成的权重（nats 量级；与 `user_ngram` 的 `DEFAULT_WEIGHT` 同一尺度）。
pub const FREQ_WEIGHT: f64 = 2.0;

/// 置顶加成：只在 `quality` 的**权重分量**内抬高，必须
/// `1 << PIN_BOOST < 1e9`（层级台阶）且不碰 `1e12`（码长台阶）。
///
/// 旧值 `1e15` 等于 `1000 * 1e12`，会把候选的码长分量整体抬 1000：
/// `rescore_filter` 的 ① 整句组按 `quality / 1e12` 取 `s_max`，置顶后
/// 这一组只剩置顶候选自己 ⇒ 30 分钟内只要命中过 pin，整句神经重排就被跳过。
/// 改成 `1e6`：仍远大于任何 `ln(词频)+credibility`（≤ ~20），压得住同层候选，
/// 但 `quality / 1e12` 与层级分量都不变，`s_max` 分组和静态分口径都恢复。
pub const PIN_BOOST: f64 = 1e6;

/// 置顶回溯的扫描上限：只在 TTL 内命中同一输入串时才扫，最多看这么多条。
///
/// 置顶候选通常就在首屏附近；设上限是为了避免为了一个置顶把整句的懒流拉干。
pub const PIN_SCAN_LIMIT: usize = 200;

/// 表的上限（超出按最久未用淘汰），避免无限增长。
pub const MAX_ENTRIES: usize = 4000;

/// 每 N 次记录落一次盘（与 `user_ngram` 的 10 次口径一致）。
pub const FLUSH_EVERY: u32 = 10;

/// 一条词频记录。
#[derive(Clone, Copy, Debug, Default)]
struct Score {
    /// 累计选择次数（只增不减；衰减在读取时算）。
    count: f64,
    /// 最近一次选择的时间戳（Unix 秒）。
    last_ts: i64,
}

/// 最近一次选择。
#[derive(Clone, Debug)]
struct Pin {
    text: String,
    code: Code,
    ts: i64,
}

/// 进程级个性化表。
#[derive(Default)]
pub struct Personalization {
    entries: HashMap<String, Score>,
    pins: HashMap<String, Pin>,
    /// 快照用的只读副本：写时重建，读时只 `Arc::clone`，不打整表拷贝。
    entries_view: Arc<HashMap<String, Score>>,
    pins_view: Arc<HashMap<String, Pin>>,
    /// 自上次落盘以来记录了几次。
    dirty: u32,
}

impl Personalization {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.pins.is_empty()
    }

    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// 上屏一次：词频 +1，并记下"这次的选择"。
    ///
    /// `input` 是本次提交的完整输入串（同一串再打时才置顶）；`entry_text` 是候选文本，
    /// `code` 是它的音节码（用于重建候选与写用户词典）。`code` 为空时不记（整句的
    /// 分词成分会走这里，它们不是用户真正选中的那条候选）。
    pub fn record_choice(&mut self, input: &str, text: &str, code: &Code, now: i64) {
        if text.is_empty() || input.is_empty() {
            return;
        }
        let score = self.entries.entry(text.to_string()).or_default();
        score.count += 1.0;
        score.last_ts = now;
        self.pins.insert(
            input.to_string(),
            Pin {
                text: text.to_string(),
                code: code.clone(),
                ts: now,
            },
        );
        self.dirty += 1;
        self.rebuild_views();
        self.evict_if_needed();
    }

    /// 词频加成（衰减在读取时算）。
    pub fn bonus(&self, text: &str, now: i64) -> f64 {
        let Some(score) = self.entries.get(text) else {
            return 0.0;
        };
        let decayed = decayed_count(score.count, score.last_ts, now);
        FREQ_WEIGHT * (1.0 + decayed).ln()
    }

    /// 这一串输入有没有**仍在 TTL 内**的最近选择。返回 `(文本, 音节码)`。
    pub fn pinned(&self, input: &str, now: i64) -> Option<(&str, &Code)> {
        let pin = self.pins.get(input)?;
        if now - pin.ts > PIN_TTL_SECS {
            return None;
        }
        Some((pin.text.as_str(), &pin.code))
    }

    /// 清空入口：词频与最近选择一起抹掉，并立刻落盘（下次启动也是空的）。
    pub fn clear(&mut self) {
        self.entries.clear();
        self.pins.clear();
        self.dirty += 1;
        self.rebuild_views();
    }

    pub fn should_flush(&self) -> bool {
        self.dirty >= FLUSH_EVERY
    }

    /// 落盘（TSV）：
    ///
    /// * `w<TAB>次数<TAB>时间戳<TAB>文本`
    /// * `p<TAB>时间戳<TAB>输入<TAB>码 id（逗号分隔）<TAB>文本`
    pub fn save(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        use std::io::Write as _;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut out = String::new();
        for (text, score) in &self.entries {
            out.push_str(&format!(
                "w\t{}\t{}\t{}\n",
                score.count, score.last_ts, text
            ));
        }
        for (input, pin) in &self.pins {
            let code = pin
                .code
                .0
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",");
            out.push_str(&format!("p\t{}\t{}\t{}\t{}\n", pin.ts, input, code, pin.text));
        }
        let mut file = std::fs::File::create(path)?;
        file.write_all(out.as_bytes())?;
        self.dirty = 0;
        Ok(())
    }

    /// 读盘；文件不存在时静默返回。
    pub fn load(&mut self, path: &std::path::Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        for line in text.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            match parts.first().copied() {
                Some("w") if parts.len() == 4 => {
                    let Ok(count) = parts[1].parse::<f64>() else {
                        continue;
                    };
                    let Ok(last_ts) = parts[2].parse::<i64>() else {
                        continue;
                    };
                    if parts[3].is_empty() {
                        continue;
                    }
                    self.entries.insert(
                        parts[3].to_string(),
                        Score {
                            count,
                            last_ts,
                        },
                    );
                }
                Some("p") if parts.len() == 5 => {
                    let Ok(ts) = parts[1].parse::<i64>() else {
                        continue;
                    };
                    let code: Vec<crate::gear::dict::vocabulary::SyllableId> = parts[3]
                        .split(',')
                        .filter_map(|id| id.parse::<crate::gear::dict::vocabulary::SyllableId>().ok())
                        .collect();
                    if parts[2].is_empty() || parts[4].is_empty() {
                        continue;
                    }
                    self.pins.insert(
                        parts[2].to_string(),
                        Pin {
                            text: parts[4].to_string(),
                            code: Code(code),
                            ts,
                        },
                    );
                }
                _ => {}
            }
        }
        self.rebuild_views();
    }

    /// 快照：拿两把 `Arc`，之后打分不再碰锁。
    pub fn snapshot(&self, now: i64) -> PreferenceView {
        PreferenceView {
            entries: Arc::clone(&self.entries_view),
            pins: Arc::clone(&self.pins_view),
            now,
        }
    }

    fn rebuild_views(&mut self) {
        self.entries_view = Arc::new(self.entries.clone());
        self.pins_view = Arc::new(self.pins.clone());
    }

    /// 超上限按"最久没选过"淘汰。
    fn evict_if_needed(&mut self) {
        if self.entries.len() <= MAX_ENTRIES {
            return;
        }
        let mut by_age: Vec<(i64, String)> = self
            .entries
            .iter()
            .map(|(text, score)| (score.last_ts, text.clone()))
            .collect();
        by_age.sort_by(|a, b| a.0.cmp(&b.0));
        let excess = self.entries.len() - MAX_ENTRIES;
        for (_, text) in by_age.into_iter().take(excess) {
            self.entries.remove(&text);
        }
        self.rebuild_views();
    }
}

/// 按天指数衰减后的次数：`count * 2^(-days / HALF_LIFE)`。
pub fn decayed_count(count: f64, last_ts: i64, now: i64) -> f64 {
    let days = ((now - last_ts).max(0)) as f64 / 86400.0;
    count * (-std::f64::consts::LN_2 * days / HALF_LIFE_DAYS).exp()
}

/// 当前 Unix 秒（系统时钟回拨时退化为 0，不 panic）。
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// 候选打分期的只读快照。
pub struct PreferenceView {
    entries: Arc<HashMap<String, Score>>,
    pins: Arc<HashMap<String, Pin>>,
    now: i64,
}

impl PreferenceView {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.pins.is_empty()
    }

    /// 这条候选该加多少分（词频加成 + 置顶加成）。只加不减。
    pub fn boost(&self, text: &str, input: &str) -> f64 {
        let mut bonus = self
            .entries
            .get(text)
            .map(|score| FREQ_WEIGHT * (1.0 + decayed_count(score.count, score.last_ts, self.now)).ln())
            .unwrap_or(0.0);
        if self.is_pinned(input, text) {
            bonus += PIN_BOOST;
        }
        bonus
    }

    fn is_pinned(&self, input: &str, text: &str) -> bool {
        self.pins
            .get(input)
            .is_some_and(|pin| pin.ts.abs_diff(self.now) <= PIN_TTL_SECS as u64 && pin.text == text)
    }

    /// pin 的剩余 TTL 比例（1 = 刚记录，0 = 已过期/没命中）。
    ///
    /// 线性重排头把它当连续特征用；只读，不参与原来的 `boost` 口径。
    pub fn pin_ttl_fraction(&self, input: &str, text: &str) -> f64 {
        let Some(pin) = self.pins.get(input) else {
            return 0.0;
        };
        if pin.text != text {
            return 0.0;
        }
        let age = self.now - pin.ts;
        if age < 0 || age > PIN_TTL_SECS {
            return 0.0;
        }
        1.0 - age as f64 / PIN_TTL_SECS as f64
    }

    /// 需要置顶的**文本**（TTL 内）。只给文本：存下来的音节码属于记录时的方案，
    /// 跨方案重放会把外方案的码塞进本方案候选，所以这里一律不交出码。
    pub fn pinned_text(&self, input: &str) -> Option<&str> {
        let pin = self.pins.get(input)?;
        if self.now - pin.ts > PIN_TTL_SECS {
            return None;
        }
        Some(pin.text.as_str())
    }
}

/// 把"最近选择"提升到候选流最前面，**只用当前方案自己的候选**。
///
/// 以前这里会用 TSV 里存的音节码直接造一条候选（`pin_candidate`，已删除）：
/// 切到码表不同的方案后，同一个输入串会拿外方案的码拼出本方案根本不存在的
/// 文本，提交时还会把外方案的码写进用户词典。现在改成：
///
/// * 存储格式**不变**（旧的 `p` 行照常读，不丢已有偏好）；
/// * 存下来的 `code` 不参与重放（只在落盘/回读里保留原样）；
/// * 在 `inner` 流里找**同文本**的候选（最多 [`PIN_SCAN_LIMIT`] 条），找到就把
///   那一条原样（连同它自己的 code / quality / 注释）提到最前；
/// * 找不到（当前方案对这个输入串产不出同一文本）→ 不置顶。
///
/// 因此"同一文本在另一个方案有自己的码"仍会置顶（用的是新方案的码），而
/// "只有外方案的码"永远不会变成候选。词频只改 `quality`，同样不碰码。
pub fn promote_pinned_text(
    mut inner: Box<dyn Translation>,
    view: &PreferenceView,
    input: &str,
) -> Box<dyn Translation> {
    use crate::gear::translator::translation::{ChainedTranslation, VecTranslation};

    let Some(text) = view.pinned_text(input) else {
        return inner;
    };
    // 首条就是它：一步都不用走，保持懒流。
    if inner.peek().is_some_and(|c| c.text == text) {
        return inner;
    }
    let mut head: Vec<Candidate> = Vec::with_capacity(PIN_SCAN_LIMIT + 1);
    let mut pinned: Option<Candidate> = None;
    while head.len() < PIN_SCAN_LIMIT {
        let Some(candidate) = inner.peek() else {
            break;
        };
        inner.next();
        if pinned.is_none() && candidate.text == text {
            pinned = Some(candidate);
        } else {
            head.push(candidate);
        }
    }
    if let Some(pinned) = pinned {
        head.insert(0, pinned);
    }
    if inner.peek().is_none() {
        Box::new(VecTranslation::new(head))
    } else {
        Box::new(ChainedTranslation::new(head, inner))
    }
}

/// 打分包装：只改 `quality`，候选集合与顺序来源不动。
pub struct PreferenceTranslation {
    inner: Box<dyn Translation>,
    view: PreferenceView,
    input: String,
}

impl PreferenceTranslation {
    pub fn new(inner: Box<dyn Translation>, view: PreferenceView, input: &str) -> Self {
        Self {
            inner,
            view,
            input: input.to_string(),
        }
    }
}

impl Translation for PreferenceTranslation {
    fn peek(&mut self) -> Option<Candidate> {
        let mut candidate = self.inner.peek()?;
        candidate.quality += self.view.boost(&candidate.text, &self.input);
        Some(candidate)
    }

    fn next(&mut self) {
        self.inner.next();
    }
}

// ------------------------------------------------------------------ 进程级入口

/// 进程级个性化表（懒加载；落盘位置见 [`store_path`]）。
pub fn global() -> &'static Mutex<Personalization> {
    static GLOBAL: OnceLock<Mutex<Personalization>> = OnceLock::new();
    GLOBAL.get_or_init(|| {
        let mut store = Personalization::new();
        store.load(&store_path());
        Mutex::new(store)
    })
}

/// 落盘位置（`%APPDATA%/Rime-Next/user_preference.tsv`）。
///
/// 测试构建落临时目录：默认打开个性化后，测试里的提交不能再读写真实用户数据。
pub fn store_path() -> std::path::PathBuf {
    #[cfg(test)]
    {
        return std::env::temp_dir()
            .join("Rime-Next-test")
            .join(format!("user_preference-{}.tsv", std::process::id()));
    }
    #[cfg(not(test))]
    {
        std::env::var_os("APPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_default()
            .join("Rime-Next")
            .join("user_preference.tsv")
    }
}

/// 上屏时记一次（开关由调用方负责 —— 关着就不该调到这）。
pub fn record_choice_global(input: &str, text: &str, code: &Code) {
    if let Ok(mut store) = global().lock() {
        store.record_choice(input, text, code, now_secs());
        if store.should_flush() {
            let _ = store.save(&store_path());
        }
    }
}

/// 候选打分期的快照；表为空时返回 `None`（调用方直接跳过包装）。
pub fn snapshot_global() -> Option<PreferenceView> {
    let store = global().lock().ok()?;
    if store.is_empty() {
        return None;
    }
    Some(store.snapshot(now_secs()))
}

/// 清空入口（码级 / 调试 / 用户设置页都可以调）。
pub fn clear_global() {
    if let Ok(mut store) = global().lock() {
        store.clear();
        let _ = store.save(&store_path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(ids: &[crate::gear::dict::vocabulary::SyllableId]) -> Code {
        Code(ids.to_vec())
    }

    #[test]
    fn counts_and_decays_by_day() {
        let mut store = Personalization::new();
        let t0 = 1_600_000_000;
        store.record_choice("nihao", "你好", &code(&[1, 2]), t0);
        let fresh = store.bonus("你好", t0);
        assert!(fresh > 0.0);
        // 30 天（一个半衰期）：衰减后的次数剩一半 ⇒ bonus = ln(1.5) * weight
        let half = store.bonus("你好", t0 + (HALF_LIFE_DAYS * 86400.0) as i64);
        assert!(
            (half - FREQ_WEIGHT * 1.5_f64.ln()).abs() < 1e-9,
            "半衰期后应剩一半：{half} vs {}",
            FREQ_WEIGHT * 1.5_f64.ln()
        );
        assert!(half < fresh, "衰减必须单调下降");
        assert_eq!(store.bonus("没选过", t0), 0.0);
    }

    #[test]
    fn repeated_choices_raise_the_bonus() {
        let mut store = Personalization::new();
        let t0 = 1_600_000_000;
        store.record_choice("nihao", "你好", &code(&[1, 2]), t0);
        let once = store.bonus("你好", t0);
        store.record_choice("nihao", "你好", &code(&[1, 2]), t0);
        let twice = store.bonus("你好", t0);
        assert!(twice > once, "{twice} vs {once}");
        assert!((twice - FREQ_WEIGHT * 3.0_f64.ln()).abs() < 1e-9);
    }

    #[test]
    fn pin_is_top_only_within_ttl() {
        let mut store = Personalization::new();
        let t0 = 1_600_000_000;
        store.record_choice("nihao", "你好", &code(&[1, 2]), t0);
        assert_eq!(store.pinned("nihao", t0).map(|(text, _)| text), Some("你好"));
        assert_eq!(store.pinned("nihao", t0 + PIN_TTL_SECS).map(|(text, _)| text), Some("你好"));
        assert!(
            store.pinned("nihao", t0 + PIN_TTL_SECS + 1).is_none(),
            "TTL 过后不再置顶"
        );
        assert!(store.pinned("niha", t0).is_none(), "别的输入串不吃这份置顶");

        let view = store.snapshot(t0);
        assert!(view.boost("你好", "nihao") >= PIN_BOOST, "置顶加成要能压过质量台阶");
        let expired = store.snapshot(t0 + PIN_TTL_SECS + 1);
        assert!(expired.boost("你好", "nihao") < PIN_BOOST, "TTL 过后只剩词频分");
    }

    /// `PIN_BOOST` 只能动权重分量：码长台阶（`1e12`）与层级台阶（`1e9`）
    /// 都必须逐位不变，否则 `rescore_filter` 的 `s_max` 分组会被置顶改写。
    #[test]
    fn pin_boost_does_not_touch_length_or_tier_steps() {
        let quality = 3e12 + 1e9 + 7.5;
        let pinned = quality + PIN_BOOST;
        assert_eq!(
            (quality / 1e12) as usize,
            (pinned / 1e12) as usize,
            "码长分量被置顶改写了"
        );
        let tier = |q: f64| (q.rem_euclid(1e12) / 1e9) as usize;
        assert_eq!(tier(quality), tier(pinned), "层级分量被置顶改写了");
        assert!(PIN_BOOST < 1e9, "置顶加成一脚踩进了层级台阶");
        assert!(PIN_BOOST > 100.0, "置顶加成压不住普通词频权重");
    }

    #[test]
    fn clear_wipes_everything() {
        let mut store = Personalization::new();
        store.record_choice("nihao", "你好", &code(&[1, 2]), 1_600_000_000);
        assert!(!store.is_empty());
        store.clear();
        assert!(store.is_empty());
        assert_eq!(store.bonus("你好", 1_600_000_000), 0.0);
        assert!(store.pinned("nihao", 1_600_000_000).is_none());
    }

    #[test]
    fn save_and_load_round_trip() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "lrn_pref_{}_{}.tsv",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let t0 = 1_600_000_000;
        let mut store = Personalization::new();
        store.record_choice("nihao", "你好", &code(&[1, 2]), t0);
        store.record_choice("shijie", "世界", &code(&[3, 4]), t0);
        store.save(&path).expect("写盘应成功");
        let mut loaded = Personalization::new();
        loaded.load(&path);
        assert_eq!(loaded.entry_count(), 2);
        assert!((loaded.bonus("你好", t0) - store.bonus("你好", t0)).abs() < 1e-9);
        assert_eq!(loaded.pinned("shijie", t0).map(|(text, _)| text), Some("世界"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn empty_history_is_not_pinned() {
        let store = Personalization::new();
        assert!(store.snapshot(1_600_000_000).is_empty());
        assert_eq!(store.bonus("你好", 1_600_000_000), 0.0);
    }

    /// 置顶是**结构序**：最近选择那条必须先于原来的首候选，且用的是
    /// **当前方案候选自己的码**（不是记录时的码）。
    #[test]
    fn pinned_candidate_keeps_current_scheme_code() {
        use crate::gear::dict::vocabulary::DictEntry;
        use crate::gear::translator::translation::VecTranslation;

        let t0 = 1_600_000_000;
        let mut store = Personalization::new();
        // 记录时方案 A 的码 = [1, 2]。
        store.record_choice("nihao", "你好", &code(&[1, 2]), t0);
        let view = store.snapshot(t0);

        // 当前方案 B 对同一输入也产「你好」，但它自己的码是 [7, 8]。
        let base: Box<dyn Translation> = Box::new(VecTranslation::new(vec![
            Candidate::new("table", 0, 5, "尼好", 2e12),
            Candidate::new("table", 0, 5, "你好", 1e9).with_entry(DictEntry {
                text: "你好".to_string(),
                code: code(&[7, 8]),
                ..Default::default()
            }),
        ]));
        let mut wrapped =
            PreferenceTranslation::new(promote_pinned_text(base, &view, "nihao"), view, "nihao");
        let first = wrapped.peek().expect("应有首候选");
        assert_eq!(first.text, "你好", "最近选择必须排在原来的首候选之前");
        assert_eq!(
            first.entry.as_ref().expect("应挂词条").code,
            code(&[7, 8]),
            "置顶必须用当前方案的码，不能用记录时的 [1, 2]"
        );
        assert!(first.quality >= PIN_BOOST, "置顶加分要能压过质量台阶");
        // 原来的首候选退到第二。
        wrapped.next();
        assert_eq!(wrapped.peek().expect("应有第二候选").text, "尼好");
    }

    /// 跨方案串味回归：方案 A 里选过的串，在方案 B 里**不能**用 A 的码造候选。
    #[test]
    fn foreign_pin_is_not_replayed_in_another_scheme() {
        use crate::gear::dict::vocabulary::DictEntry;
        use crate::gear::translator::translation::VecTranslation;

        let t0 = 1_600_000_000;
        let mut store = Personalization::new();
        // 方案 A：输入 `abc` 选「是」，码是 A 的 id 91。
        store.record_choice("abc", "是", &code(&[91]), t0);
        let view = store.snapshot(t0);

        // 方案 B：同一输入只产「事」（B 自己的码 5），没有「是」。
        let base: Box<dyn Translation> = Box::new(VecTranslation::new(vec![
            Candidate::new("table", 0, 3, "事", 1.0).with_entry(DictEntry {
                text: "事".to_string(),
                code: code(&[5]),
                ..Default::default()
            }),
        ]));
        let mut out = PreferenceTranslation::new(promote_pinned_text(base, &view, "abc"), view, "abc");
        let mut texts = Vec::new();
        let mut codes = Vec::new();
        while let Some(candidate) = out.peek() {
            texts.push(candidate.text.clone());
            codes.push(candidate.entry.as_ref().map(|entry| entry.code.clone()));
            out.next();
        }
        assert_eq!(texts, vec!["事"], "不能冒出方案 A 的「是」");
        assert_eq!(codes, vec![Some(code(&[5]))], "不能出现方案 A 的码 91");
    }

    /// 词频按文本共享只改 `quality`，永远不会改写词条的音节码 —— 跨方案时
    /// 不会产生"错码候选"（与置顶的旧实现要区分开）。
    #[test]
    fn text_level_frequency_never_rewrites_codes() {
        use crate::gear::dict::vocabulary::DictEntry;
        use crate::gear::translator::translation::VecTranslation;

        let t0 = 1_600_000_000;
        let mut store = Personalization::new();
        // 方案 A 的记录（文本级词频 + 一条 A 的最近选择）。
        store.record_choice("abc", "是", &code(&[91]), t0);
        let view = store.snapshot(t0);

        // 方案 B 的候选：文本相同「是」，但码是 B 自己的 5。
        let base_quality = 42.0;
        let base: Box<dyn Translation> = Box::new(VecTranslation::new(vec![
            Candidate::new("table", 0, 3, "是", base_quality).with_entry(DictEntry {
                text: "是".to_string(),
                code: code(&[5]),
                ..Default::default()
            }),
        ]));
        // 换一个输入串：不吃置顶，只吃文本级词频。
        let mut wrapped = PreferenceTranslation::new(base, view, "xyz");
        let only = wrapped.peek().expect("应有候选");
        assert_eq!(
            only.entry.as_ref().expect("应挂词条").code,
            code(&[5]),
            "词频加成不能把外方案的码写进候选"
        );
        assert!(only.quality > base_quality, "词频应只提升分数");
        wrapped.next();
        assert!(wrapped.peek().is_none(), "包装不新增候选，只改分数");
    }
}
