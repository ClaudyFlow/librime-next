//! `src/rime/engine.{h,cc}`：`Engine`（持有 Schema + Context + 组件，驱动 Compose）。
//!
//! 对照 C++ `ConcreteEngine`：`ProcessKey` 遍历处理器链；输入串一变 =>
//! `OnContextUpdate` => `Compose` => `CalculateSegmentation` + `TranslateSegments`。
//!
//! # 组件装配（`InitializeComponents`）
//!
//! 方案编译后的 `engine/processors` / `engine/translators` 列表由 [`crate::gear::registry`]
//! 逐个创建，**没移植的组件跳过而不是让链条断掉**（等同 C++ `Require` 失败的处理）。
//! 列表为空时回退到 `[speller, selector]` —— 保证只装配ator了 Schema 的测试环境仍可打字。

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};

use crate::gear::dict::dictionary::Dictionary;
use crate::engine::candidate::Candidate;
use crate::engine::composition::Composition;
use crate::engine::context::Context;
use crate::gear::segmentor::abc_segmentor::AbcSegmentor;
use crate::gear::segmentor::fallback_segmentor::FallbackSegmentor;
use crate::gear::filter::filter::Filter;
use crate::gear::segmentor::matcher::Matcher;
use crate::gear::processor::{ProcessResult, Processor};
use crate::gear::registry::{ComponentEntry, install_engine_components};
use crate::gear::schema::list_translator;
use crate::gear::segmentor::{Segmentor, SegmentorView};
use crate::gear::processor::selector::Selector;
use crate::gear::processor::speller::Speller;
use crate::gear::translator::{ScriptTranslator, Translator};
use crate::engine::key_event::{KeyEvent, XK_SPACE};
use crate::engine::menu::Menu;
use crate::gear::schema::Schema;
use crate::signal::{Notification, Signal};
use crate::engine::segmentation::{SegStatus, Segmentation};
use crate::gear::processor::switcher::{self, Switcher};

/// 引擎（对照 C++ `Engine`）。
///
/// 翻译器/处理器以 `dyn Trait + Send + Sync` 持有：引擎会被塞进 `Mutex<Global>` 背后
/// 的 `static`，要求内部类型可跨线程安全共享（否则 `OnceLock<Mutex<Global>>` 不满足 `Sync`）。
pub struct Engine {
    pub schema: Schema,
    pub context: Context,
    translators: Vec<ComponentEntry<Box<dyn Translator + Send + Sync>>>,
    processors: Vec<ComponentEntry<Box<dyn Processor + Send + Sync>>>,
    segmentors: Vec<ComponentEntry<Box<dyn Segmentor + Send + Sync>>>,
    filters: Vec<ComponentEntry<Box<dyn Filter + Send + Sync>>>,
    /// 组件请求引擎代转的按键队列（见 [`Engine::enqueue_synthetic_key`]）。
    ///
    /// 每项是 `(按键, 未命中时是否直接上屏该字符)`。第二个字段专为
    /// `chord_composer` 而设：它 `FinishChord` 时把和弦编码逐键转发，若某键谁都
    /// 不吃（典型是空格），C++ 直接 `CommitText` 上屏（`chord_composer.cc:228-231`）。
    synthetic_queue: std::collections::VecDeque<(KeyEvent, bool)>,
    build_dir: std::path::PathBuf,
    /// 数据根列表，按 **C++ `ResourceResolver` 的查找顺序**：用户目录优先、共享目录其次。
    ///
    /// 为什么不能只留 `build_dir.parent()`：生产环境里 `build_dir` 是 `<用户目录>/build`
    /// （`Deployer::staging_dir`），而 `model/` 在共享目录、`opencc/` 也可能在别处
    /// —— 两者不在同一个根下，必须都查。
    data_roots: Vec<std::path::PathBuf>,
    /// `default.yaml` 的配置树：switcher 的方案清单与菜单设置都在里面。
    ///
    /// C++ 是 `Switcher` 作为 `Engine` 时那个 `.default` schema 的 config；LRN 的
    /// switcher 不做独立引擎，配置树由这里托管。
    pub default_config: serde_json::Value,
    /// 用户目录（`data_roots[0]`）：`user.yaml` 读写的位置。
    pub user_dir: Option<std::path::PathBuf>,
    /// 组件登记的方案切换请求。
    ///
    /// C++ 的 `Switcher` 直接 `engine->ApplySchema()`；Rust 里 `Switcher` 自己就在
    /// `processors` 列表里（已被 `mem::take` 到局部），同步重建会被遍历结束时的
    /// `self.processors = processors` 覆盖掉 —— 所以只登记，由
    /// [`Engine::process_key`] 在列表归位后执行。
    pending_schema: Option<String>,
    /// 交互式切换的**异步**落地点（见 [`Self::apply_schema`]）。
    ///
    /// `true` 时 `apply_schema` 不再重建组件链，只把目标方案记进 `pending_rebuild`
    /// 并立刻返回"已受理"；真正的重建交给**持有会话表的宿主**（C API 的
    /// `rime_process_key`）在放锁之后派发给 `engine::session_rebuild` 的工作线程。
    ///
    /// 为什么必须由宿主代劳：`Engine` 替换不了自己所在的 `Session`（会话表在
    /// `c_api::Global` 里）。`rime_select_schema` 那条路已经是这个形状，两个入口
    /// 走同一个 `request_session_rebuild`，才不会并发重建同一方案。
    ///
    /// 缺省 `false`（同步、历史语义）：直接构造 `Session`/`Engine` 的调用方
    /// （单元测试、e2e、探针）没有会话表可换入，登记了也没人来取。
    async_rebuild: bool,
    /// 待宿主派发的方案切换目标（`apply_schema` 只在这里留一笔）。
    pending_rebuild: Option<String>,
    /// 上次见过的神经重排结果代数(见 [`Self::poll_rescore_landing`])。
    rescore_seen_generation: u64,
    /// 上一次真正应用到候选菜单的落地签名。代数变了但内容没变(恒等 order、
    /// 同文本预测)时跳过 re-compose——抄 msime `glosses_changed` 的「没变不推帧」。
    rescore_applied_signature: Option<crate::gear::filter::rescore::LandingSignature>,
    /// 菜单刚被确认关闭时置位：吞掉宿主紧接着重复投递的那一下空格。
    ///
    /// TSF 的 `OnTestKeyDown` 会真的把按键送进引擎，`OnKeyDown` 靠 `tested_key`
    /// 缓存复用结果。但选中方案会 `request_schema → apply_schema`，推高 context
    /// 代次，缓存可能失效 —— 同一次物理空格于是被送进引擎两次。第二次 switcher
    /// 已退出，空格 `eaten=false` 被宿主透传给应用，表现就是「选完方案上屏一个空格」。
    /// 这里在菜单关闭后吞掉紧随的**一次**空格（不上屏、不进 commit_history）。
    /// 放在 Engine 而不是 Switcher 上，是因为 `apply_schema` 会重建整个组件链。
    pub(crate) swallow_next_space: bool,
    /// 方案切换后宿主可能把**同一次物理按键**再投递一次（尤其 0.18.6 旧 TIP：
    /// state.matches 失败时不缓存测试结果，OnKeyDown 会重发）。如果是数字键，
    /// 第二次进来时 switcher 已关闭、又没有候选菜单，会 eaten=false 透传给应用
    /// ⇒ 用户报「Ctrl+ 换方案时数字上屏」。这里在方案切换后吞掉紧随的一次数字。
    pub(crate) swallow_next_digit: bool,
    /// 方案声明的**输入类型开关**（`translator/prisms[].option|input_type`、
    /// `translator/input_types` 的键，含 `simp` / `trad` 命名空间）。
    ///
    /// 这些 option 被打开/关闭意味着"按键到音节的切分规则换了一套"（全拼↔双拼）。
    /// 已输入的串是按**旧 prism** 切分的，换规则后继续重译会得到完全不同的码
    /// （`nihao` 在全拼是 `ni hao`，在自然码可能是别的音节），所以切换时
    /// **清空当前组字**——与切方案 `apply_schema` 里的 `context.clear()` 一致。
    /// 见 [`Engine::process_key`] 与 [`Engine::set_option`]。
    ///
    /// `pub(crate)`：`switcher` 的单元测试要构造"当前可切输入类型"的引擎状态。
    pub(crate) input_type_options: Vec<String>,
    /// 分阶段计时（2026-11-08）：`WEASEL_STAGE_TRACE=1` 时才填，平时全 0。
    pub last_stages: StageTimings,
    /// 本次按键的阶段关联 id（`WEASEL_STAGE_TRACE=1` 时递增；关闭时为 0）。
    stage_seq: u64,
    /// 信号订阅接收端：引擎订阅自己在全局总线上的 `OptionChanged`（由
    /// `Context::set_option` / 任意组件经 `Context::set_option` 发出），用 Qt 队列连接
    /// 语义在 [`Self::drain_option_signals`] 处同步消费——把"开关变化 ⇒ 组件刷新 / 清空 /
    /// 重译"从直接调用改成信号反应（对齐 Qt 的 `engine->onOptionChanged` 槽）。
    signal_rx: Receiver<Notification>,
    /// 本引擎在全局总线上的订阅句柄（`connect_queued` 返回值），用于 [`Drop`] 退订，避免
    /// 测试/并发会话里残留的 queued 槽污染总线计数与其它引擎的队列。
    signal_sub: u64,
    /// 引擎"已反应过"的开关快照，供信号槽推算 `changed`（对齐 Qt 槽里用旧值比对新值）。
    /// 键缺省视为 `false`，与 `Context::get_option` 的默认值一致。
    signal_option_mirror: HashMap<String, bool>,
}

/// 每引擎唯一会话号（从 1 起，避开 `0` 的"未绑定"语义）。用于在**全局**信号总线上把
/// 本引擎自己发出的 `OptionChanged` 与别的引擎 / 宿主区分开——等价于 Qt 的对象线程亲和
/// 隔离，避免多引擎在总线上互相收信号。`session.rs` 在会话绑定后会用真实会话号覆盖
/// `Context::signal_session_id`，但订阅句柄不变，槽过滤仍按当时的 `signal_session_id` 走。
static NEXT_ENGINE_SESSION: AtomicU64 = AtomicU64::new(1);
fn next_engine_session() -> usize {
    NEXT_ENGINE_SESSION.fetch_add(1, Ordering::Relaxed) as usize
}

impl Drop for Engine {
    fn drop(&mut self) {
        // 退订全局总线：queued 槽不摘会一直挂在快照里，SUBSCRIBER_COUNT 虚高还会
        // 让别的引擎/会话的 set_option 白白发信号。
        crate::signal::disconnect(self.signal_sub);
    }
}

/// [`Engine::drain_option_signals`] 的反应计划——信号槽里决定"要做什么"，调用方拿到
/// `&mut Engine` 后执行（`compose` 需要 `&mut self`，槽里只有字段级拆借）。
#[derive(Default)]
struct OptionReaction {
    /// 重算组件启用条件（`refresh_component_conditions`）。对齐原语义：**每次**收到
    /// 自己的 OptionChanged 都重算，与 changed 无关。
    refresh: bool,
    /// 输入类型开关变了 ⇒ 清空当前组字（context switch，不保留重译）。
    clear: bool,
    /// 组字中 ⇒ 刷新未确认段并重译。
    compose: bool,
}

/// 一次按键在引擎内部的阶段耗时（微秒）。**各段可加**：都取自同一条同步调用链，
/// `total_us` = `proc_us + seg_us + trans_us + filter_us + commit_us + 其它`。
#[derive(Clone, Copy, Default, Debug)]
pub struct StageTimings {
    /// 处理器链（speller / editor / key_binder / punctuator…）：输入缓冲更新在这一段。
    pub proc_us: u64,
    /// `compose()` 里的 `calculate_segmentation`：segmentor 逐段推进（音节切分）。
    pub seg_us: u64,
    /// `translate_segments`：translator 查询（prism 音节匹配 + 词典 lookup + 菜单装配）。
    pub trans_us: u64,
    /// `apply_filters`：滤镜链，神经重排（`neural_rescore`）在这里。
    pub filter_us: u64,
    /// `apply_commit`：上屏、学词、同步神经预测。
    pub commit_us: u64,
    /// 整个 `process_key`。
    pub total_us: u64,
}

impl Engine {
    /// 最近一次按键的分段耗时（未开 `WEASEL_STAGE_TRACE` 时为全 0）。
    pub fn last_stage_timings(&self) -> StageTimings {
        self.last_stages
    }
}

/// 分段计时开关：`WEASEL_STAGE_TRACE` 或标记文件存在才启用（默认关闭，零侵入）。
///
/// 统一委托给 [`crate::engine::stage_trace::enabled`]，避免两套开关判定不一致。
fn stage_trace_enabled() -> bool {
    crate::engine::stage_trace::enabled()
}

/// 切方案分段计时的诊断开关：设 `WEASEL_TRACE_SCHEMA=1` 才打印（排查 TIP 超时用）。
///
/// 背景：`Ctrl+` 选方案后按数字/空格，该键在冷缓存首次切换时可能 250-330ms，
/// 超过 TIP 的 RPC 预算（180ms）⇒ 宿主拿不到响应 ⇒ 数字/空格透传给应用。
fn trace_schema_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("WEASEL_TRACE_SCHEMA").is_some())
}

/// 打印一段耗时（`WEASEL_TRACE_SCHEMA` / `WEASEL_TRACE_COMPONENTS` / 阶段日志开关）。
///
/// 2026-11-10：实现改走 [`crate::engine::switch_trace`]，除 stderr 之外还会追加
/// `%LOCALAPPDATA%\Rime-Next\Logs\schema-switch.log`（按重建 `seq` 分组），
/// 便于事后把「切方案静默 N 秒」拆成建词典 / prism / 重绑 / aux / lua 各段。
fn trace_schema_ms(label: &str, start: std::time::Instant) {
    crate::engine::switch_trace::record_us(label, start.elapsed().as_micros());
}

impl Engine {
    /// 加载方案并装配组件（对照 `Engine::Create` + `InitializeComponents`）。
    ///
    /// 数据根缺省取 `build_dir.parent()` 一个 —— 单根布局（e2e 沙箱）就够用。
    /// 生产路径请用 [`Self::create_with_roots`] 把用户目录与共享目录都传进来。
    pub fn create(build_dir: &Path, schema_id: &str) -> Option<Engine> {
        let roots: Vec<std::path::PathBuf> = build_dir.parent().map(Path::to_path_buf).into_iter().collect();
        Self::create_with_roots(build_dir, schema_id, &roots)
    }

    /// [`Self::create`] 的带数据根版本。`data_roots` 为空时回退 `build_dir.parent()`。
    pub fn create_with_roots(
        build_dir: &Path,
        schema_id: &str,
        data_roots: &[std::path::PathBuf],
    ) -> Option<Engine> {
        let data_roots: Vec<std::path::PathBuf> = if data_roots.is_empty() {
            build_dir.parent().map(Path::to_path_buf).into_iter().collect()
        } else {
            data_roots.to_vec()
        };
        // 会话重建的分段计时从这里起头：同一次重建的所有阶段共用一个 `seq`。
        crate::engine::switch_trace::begin_rebuild();
        let t_total = std::time::Instant::now();
        let t_config = std::time::Instant::now();
        let user_dir = data_roots.first().cloned();
        let default_config = switcher::load_default_config(build_dir, &data_roots);
        trace_schema_ms("load_default_config", t_config);
        // `.default` / 空 id：C++ 的 `Switcher` 在这个位置替引擎挑一个方案
        // （`engine.cc:570-574`：`if (schema_->schema_id() == ".default")` 用
        // `switcher_->CreateSchema()` 的结果）。挑法是"上次用过的，否则清单第一条"。
        let resolved_id = if schema_id == ".default" || schema_id.is_empty() {
            let previous = switcher::load_previous_schema(user_dir.as_deref());
            list_translator::pick_initial_schema(&default_config, previous.as_deref())?
        } else {
            schema_id.to_string()
        };
        let t_open = std::time::Instant::now();
        let Some(schema) = Schema::open(build_dir, &resolved_id) else {
            crate::engine::switch_trace::note(&format!(
                "schema::open({resolved_id}) 失败（.schema.yaml 或编译产物缺失）"
            ));
            return None;
        };
        trace_schema_ms(&format!("schema::open({resolved_id})"), t_open);
        let mut engine = Engine {
            schema,
            context: Context::new(),
            translators: Vec::new(),
            processors: Vec::new(),
            segmentors: Vec::new(),
            filters: Vec::new(),
            synthetic_queue: std::collections::VecDeque::new(),
            build_dir: build_dir.to_path_buf(),
            data_roots,
            default_config,
            user_dir,
            pending_schema: None,
            async_rebuild: false,
            pending_rebuild: None,
            rescore_seen_generation: 0,
            rescore_applied_signature: None,
            swallow_next_space: false,
            swallow_next_digit: false,
            input_type_options: Vec::new(),
            last_stages: StageTimings::default(),
            stage_seq: 0,
            // 占位通道：真正订阅在 initialize_components 之后（初始化期的 set_option
            // 不该触发组件反应，见 `create_with_roots` 尾部的订阅逻辑）。
            signal_rx: mpsc::sync_channel(1).1,
            signal_sub: 0,
            signal_option_mirror: HashMap::new(),
        };
        engine.initialize_components();
        // 独立引擎（测试 / 未挂会话）也给一个唯一号：全局总线按 session_id 过滤，
        // 两个都挂在 0 上的引擎会互收对方的 OptionChanged。
        engine.context.signal_session_id = next_engine_session();
        // 初始化完成后再订阅：`initialize_options` 等初始化期的 `set_option` 只落状态、
        // 不进信号反应（对齐 C++ 初始化时 `UpdateComponentConditions` 不致清空组字）。
        let (signal_tx, signal_rx) = mpsc::sync_channel(256);
        engine.signal_sub = crate::signal::connect_queued(signal_tx);
        engine.signal_rx = signal_rx;
        trace_schema_ms(&format!("create_with_roots 总计({resolved_id})"), t_total);
        Some(engine)
    }

    /// C++ `ConcreteEngine::InitializeComponents`：读 `engine/processors` 与
    /// `engine/translators`，按名单逐个构造。
    fn initialize_components(&mut self) {
        let trace = std::time::Instant::now();
        // 字段级隐私开关按当前方案配置刷新（切方案会重走这里）。
        crate::engine::privacy::set_from_config(&self.schema.config);
        let (processors, segmentors, translators, filters) =
            install_engine_components(&self.schema.config, &self.build_dir, &self.data_roots);
        trace_schema_ms("  install_engine_components", trace);
        self.filters = filters;
        // 输入类型开关清单来自方案配置，必须在任何按键进来之前算好（切方案后
        // `apply_schema` 会重新走这里）。没配 `prisms` / `input_types` 时为空，
        // 下面 `process_key` / `set_option` 的清理逻辑整段跳过。
        self.input_type_options =
            collect_input_type_options(&self.schema.config, Some(&self.build_dir));
        let trace_options = std::time::Instant::now();
        self.initialize_options();
        trace_schema_ms("  initialize_options", trace_options);
        if translators.is_empty() {
            // 方案没列 `engine/translators`（或列表里全是还没移植的 translator）：
            // 退回旧的「按 dictionary/prism 直接开拼音词典」路径，保证还能打字。
            if let Some((dict_name, prism_name)) = self.schema.first_translator_dict() {
                if let Some(dict) = Dictionary::open(&self.build_dir, &dict_name, &prism_name) {
                    self.translators.push(ComponentEntry::new(
                        Box::new(ScriptTranslator::new(dict))
                            as Box<dyn Translator + Send + Sync>,
                        "script_translator",
                    ));
                }
            }
        } else {
            self.translators = translators;
        }
        if processors.is_empty() {
            // 回退：方案没列 `engine/processors`（测试环境 / 极简方案）
            let speller = Speller::new(&self.schema.config);
            self.processors.push(ComponentEntry::new(
                Box::new(speller) as Box<dyn Processor + Send + Sync>,
                "speller",
            ));
            self.processors.push(ComponentEntry::new(
                Box::new(Selector) as Box<dyn Processor + Send + Sync>,
                "selector",
            ));
        } else {
            self.processors = processors;
        }
        // switcher 永远排**第一个**（C++ `engine.cc:565-570` 先 push `switcher_` 再
        // 装方案里的 processors），这样 `F4` / `Control+grave` 不会被 speller 先吃掉。
        //
        // 它不吃 `engine/processors` 的配置，所以不走 registry；构造时要把上次保存的
        // 开关状态读回来（C++ `ConcreteEngine` 构造里 `switcher_->RestoreSavedOptions()`，
        // `engine.cc:156`），否则用户设的简繁/全半角重启就丢。
        let trace = std::time::Instant::now();
        let switcher = Switcher::new(
            self.default_config.clone(),
            &self.build_dir,
            self.user_dir.clone(),
        );
        switcher.restore_saved_options(&mut self.context);
        trace_schema_ms("  switcher(构造+恢复开关)", trace);
        self.processors.insert(
            0,
            ComponentEntry::new(
                Box::new(switcher) as Box<dyn Processor + Send + Sync>,
                "switcher",
            ),
        );
        let trace_tail = std::time::Instant::now();
        if segmentors.is_empty() {
            // 回退：方案没列 `engine/segmentors`（测试环境 / 极简方案）。
            // 顺序照抄 librime 的默认名单：abc → matcher → fallback。
            self.segmentors.push(ComponentEntry::new(
                Box::new(AbcSegmentor::new(&self.schema.config))
                    as Box<dyn Segmentor + Send + Sync>,
                "abc_segmentor",
            ));
            self.segmentors.push(ComponentEntry::new(
                Box::new(Matcher::new(&self.schema.config, "recognizer"))
                    as Box<dyn Segmentor + Send + Sync>,
                "matcher",
            ));
            self.segmentors.push(ComponentEntry::new(
                Box::new(FallbackSegmentor::new()) as Box<dyn Segmentor + Send + Sync>,
                "fallback_segmentor",
            ));
        } else {
            self.segmentors = segmentors;
        }
        self.refresh_component_conditions();
        // C++ 的组件在**构造函数里**就能碰 `engine_->context()`（`ChordComposer` 靠它
        // 打 `_chord_typing`，`chord_composer.cc:48`）。Rust 的构造只拿得到方案配置树，
        // 所以装配完补一个 attach 阶段 —— 顺序上必须晚于 `refresh_component_conditions`，
        // 否则 `when:` 有条件、此刻还没启用/禁用的组件会被误通知。
        self.attach_processors();
        trace_schema_ms("  segmentors(回退)/refresh/attach", trace_tail);
    }

    /// 装配完成后的组件初始化（对照 C++ 组件构造函数里对 `context()` 的副作用）。
    fn attach_processors(&mut self) {
        let mut processors = std::mem::take(&mut self.processors);
        for entry in processors.iter_mut() {
            if entry.is_enabled() {
                entry.component.on_attach(self);
            }
        }
        self.processors = processors;
    }

    /// C++ `UpdateComponentConditions`：opoption 变化后重算每个组件的启用状态。
    fn refresh_component_conditions(&mut self) {
        let ctx = &self.context;
        for entry in &mut self.processors {
            entry.refresh(ctx);
        }
        for entry in &mut self.segmentors {
            entry.refresh(ctx);
        }
        for entry in &mut self.translators {
            entry.refresh(ctx);
        }
        for entry in &mut self.filters {
            entry.refresh(ctx);
        }
    }

    /// 处理一次按键（对照 `ConcreteEngine::ProcessKey`）。
    ///
    /// C++ 的对应机制：`Context::PushInput/PopInput/DeleteInput/Clear` 发
    /// `update_notifier_` -> `ConcreteEngine::OnContextUpdate` -> `Compose(ctx)`；
    /// `Highlight` 虽也发 update，但高亮导航不改变输入串，因此这里只在**输入串真的变了**
    /// 时重算 —— 与 C++ 行为一致。
    pub fn process_key(&mut self, key: &KeyEvent) -> bool {
        let trace = stage_trace_enabled();
        self.stage_seq = crate::engine::stage_trace::begin_key();
        let t_start = trace.then(std::time::Instant::now);
        if trace {
            self.last_stages = StageTimings::default();
        }
        // 菜单确认后，宿主可能把同一次物理空格再投递一次（见字段注释）。
        // 这次空格必须吞掉：否则 switcher 已退出，`express_editor` 在空闲态不接，
        // `eaten=false` 会让宿主把空格透传给应用 ——「选完方案多出一个空格」。
        if self.swallow_next_space {
            self.swallow_next_space = false;
            if key.keycode == XK_SPACE && !key.release() {
                return true;
            }
        }
        // 方案切换后的重放数字（见 swallow_next_digit 字段注释）：只吞一次、
        // 只吞数字，其它键进来就把标记清掉，避免误吞用户后续真正想按的数字。
        if self.swallow_next_digit {
            self.swallow_next_digit = false;
            if !key.release() && (0x30..=0x39).contains(&key.keycode) {
                return true;
            }
        }
        // CapsLock / NumLock / ScrollLock：**不能拦在处理器链之前**。
        // 2026-11-01 早先为治"CapsLock 按一下吐一串 `yinamiao.........,......`"，
        // 这里直接 `return false` —— 结果 `ascii_composer` 再也看不到 CapsLock，
        // `ascii_composer/switch_key/Caps_Lock: clear` 形同虚设，大写锁定切不了中英
        // （用户 2026-11-01 18:5x：「你的 shift 和大写锁定怎么不能切换中英」）。
        // 正确做法：**照常过处理器链**（让 ascii_composer 切中英），只在"没被任何人吃掉"
        // 时**不记历史、不上屏**，那串乱码就没了。
        let is_lock_key = matches!(key.keycode, 0x14 | 0x90 | 0x91 | 0xFFE5);
        let input_before = self.context.input.clone();
        let input_type_before = self.input_type_option_states();
        let mut result = self.run_processor_chain(key);
        // `ConcreteEngine::ProcessKey`（`engine.cc:180-182`）：**没被任何 processor
        // 吃掉**的按键记进提交历史 —— 空格、数字、退格这些"物理穿过"的键靠它，
        // 打完 `3` 再打 `.` 时 punctuator 才知道上一条是数字。合成键（key_binder
        // 代转的）不上屏，所以不记；C++ 同样用 `synthetic` 参数跳过。
        if !result && !is_lock_key {
            self.context.commit_history.push_key(key);
            // `engine.cc:193`：`unhandled_key_notifier` 是**无条件**发的（只在
            // `commit_history` 那里看 `synthetic`），所以合成键同样通知。
            self.notify_unhandled_key(key);
        }
        // 组字状态可能改变了 option（如 AsciiComposer 切 ascii_mode），重算启用条件
        self.refresh_component_conditions();
        // 组件代转的按键（key_binder 的 send / send_sequence、chord_composer 的
        // 和弦编码）：此时处理器列表已归位。
        //
        // **必须 FIFO**：`send_sequence: "{Control+a}{BackSpace}"` 与并击和弦
        // `"abc"` 都是有意义的**顺序**，早年这里用 `Vec::pop()`（LIFO）会把它们
        // 整个倒过来。C++ 是同步递归、天然保序。
        //
        // 处理中新登记的按键追加到队尾（`enqueue_synthetic_key` 往同一队列 push），
        // 于是"嵌套转发"排在当前批次之后，同样保序。
        while let Some((synthetic, commit_if_unhandled)) = self.synthetic_queue.pop_front() {
            let handled = self.run_processor_chain(&synthetic);
            if handled {
                result = true;
            } else {
                if commit_if_unhandled && (0x20..=0x7e).contains(&synthetic.keycode) {
                    // C++ `chord_composer.cc:228-231`：转发失败 ⇒ 该字符直接上屏
                    self.context
                        .commit_raw_text((synthetic.keycode as u8 as char).to_string());
                }
                self.notify_unhandled_key(&synthetic);
            }
            self.refresh_component_conditions();
        }
        // 输入类型（全拼/双拼…）是 context switch：`key_binder` 的 `toggle:` 改完
        // option，下一次 `sync_context` 才会换 prism。已输入的串是按旧 prism 切分的，
        // 换规则后重译会得到完全不同的码，所以这里**清空当前组字**，不保留、不重译。
        // 开关没配 `prisms` / `input_types` 时清单为空，整段零开销跳过。
        if self.input_type_option_states() != input_type_before && self.context.is_composing() {
            self.context.clear();
            // 输入串已清空，下面的 `input != input_before` 本来就会触发 compose；
            // 置位是为了覆盖"输入本来就是空、只挂着 switcher 空段"的边角情况。
            self.context.needs_recompose = true;
        }
        // 组件在本次按键里改过的 option 信号只同步镜像、不重复反应——上面的
        // refresh / 输入类型清空已经是同样的反应，信号槽再走一遍会把用户刚敲的串清掉。
        self.sync_option_mirror_from_signals();
        // 删词这类操作输入没变、但末段已被 reopen，同样要重算（21:xx）。
        if let Some(t) = t_start {
            self.last_stages.proc_us = t.elapsed().as_micros() as u64;
        }
        let recompose = self.context.needs_recompose;
        self.context.needs_recompose = false;
        if self.context.input != input_before || recompose {
            // `Context::PushInput` 等发 `update_notifier` ⇒ `ConcreteEngine::OnContextUpdate`
            // ⇒ `Compose`。两条都在这里补：先通知组件，再重算。
            self.notify_context_update();
            let t_compose = crate::engine::stage_trace::enabled().then(std::time::Instant::now);
            self.compose();
            if let Some(t) = t_compose {
                crate::engine::stage_trace::emit(
                    self.stage_seq,
                    "compose",
                    t.elapsed().as_micros(),
                );
            }
        }
        // 提交可能发生在任何一个 processor 里（editor 的 space / enter / 数字键），
        // 处理器只拿得到 `&mut Context`，所以学词在这里补。
        let t_commit = trace.then(std::time::Instant::now);
        self.apply_commit();
        if let Some(t) = t_commit {
            self.last_stages.commit_us = t.elapsed().as_micros() as u64;
        }
        // 方案切换：处理链跑完、`self.processors` 已归位，这时重建才不会被打断。
        // 异步模式下 `apply_schema` 只是登记（本键立刻返回），重建由宿主在放锁后派发。
        if let Some(schema_id) = self.pending_schema.take() {
            self.apply_schema(&schema_id);
            // 见 swallow_next_digit：切换方案后紧跟的重放数字要吞掉。
            self.swallow_next_digit = true;
        }
        if let Some(t) = t_start {
            self.last_stages.total_us = t.elapsed().as_micros() as u64;
        }
        result
    }

    /// 逐个通知处理器：这个键整条链都没吃下（C++ `unhandled_key_notifier`）。
    fn notify_unhandled_key(&mut self, key: &KeyEvent) {
        // 借用规避：`processors` 取到局部，组件才能拿到 `&mut Engine`
        let mut processors = std::mem::take(&mut self.processors);
        for entry in processors.iter_mut() {
            if entry.is_enabled() {
                entry.component.on_unhandled_key(self, key);
            }
        }
        self.processors = processors;
    }

    /// 逐个通知处理器：上下文变了（C++ `Context::update_notifier`）。
    fn notify_context_update(&mut self) {
        let mut processors = std::mem::take(&mut self.processors);
        for entry in processors.iter_mut() {
            if entry.is_enabled() {
                entry.component.on_context_update(self);
            }
        }
        self.processors = processors;
    }

    /// C++ `Engine::ApplyCommit(commit_entry)`：把刚提交的内容交给各翻译器学词。
    ///
    /// 词条由 [`crate::engine::context::Context::collect_commit_elements`] 在提交瞬间
    /// 收好（那时 `segmentation` 还在），这里只负责分发。没有用户词典的翻译器
    /// 走 trait 的默认实现，什么都不做。
    pub fn apply_commit(&mut self) {
        let elements = self.context.take_commit_elements();
        // 重排头的样本与词条无关（标点/无词条提交也可能选中过候选），
        // 所以在 `elements` 的提前返回之前取走并学习。
        let rerank_samples = self.context.take_rerank_samples();
        if crate::engine::privacy::learning_enabled() {
            crate::gear::neural::rerank_head::observe_samples(&rerank_samples);
        }
        if elements.is_empty() {
            return;
        }
        // 个人 n-gram 学习(青简):只吃选自菜单的提交(有词条 = 菜单选的;
        // raw/ascii 提交没有词条,天然被过滤)。多段组合 = 逐段一个词;
        // 整句提交暂按整条学,回切词(词典贪心)待补(1031日志)。
        {
            // ⚠️ 2026-11-01 第二次修：原来从 `elements`（词条）取 text，实测拿到的是
            // **原始编码**（`user_ngram.tsv` 里出现 `u yinamiao 1`、`u ， 6`），
            // 中文候选根本没学进去。改成从**提交历史**取文本（那才是真正上屏的中文），
            // 并过滤标点/单字。
            let words: Vec<String> = self
                .context
                .commit_history
                .iter_rev()
                .take(1)
                .map(|record| record.text.clone())
                .filter(|text| {
                    let has_cjk = text.chars().any(|c| (0x4E00u32..=0x9FFFu32).contains(&(c as u32)));
                    has_cjk && text.chars().count() > 1
                })
                .collect();
            if !words.is_empty() && crate::engine::privacy::learning_enabled() {
                let t_learn = crate::engine::stage_trace::enabled().then(std::time::Instant::now);
                crate::engine::user_ngram::learn_and_maybe_flush(&words);
                if let Some(t) = t_learn {
                    crate::engine::stage_trace::emit(
                        self.stage_seq,
                        "ngram_learn",
                        t.elapsed().as_micros(),
                    );
                }
            }
        }
        // 上屏后联想（2026-11-07 口径「预测一些上屏十次以上的用户词」；
        // 2026-11-08 双神经模型：联想条 4 条 = key2 + desk2，kkdd 分块）。
        //
        // 神经预测**同步**算（上一棒实测 key 17ms + desk 71ms ≈ 77ms/次提交，
        // 用户认可）——联想条必须和上屏同一帧出现，异步重绘方案已被否。
        // 神经不可用/给不出结果时回退个人 n-gram（口径在 `predict_frequent`：
        // 只留上屏 ≥10 次的多字中文词），由 c_api 经 `RimeContext.predictions`
        // 交给宿主画联想条。
        //
        // 神经的输入是「提交前的上文 + 刚上屏的词」拼成的一整段**已上屏文本**：
        // * 词取**提交历史**末条 —— 那才是真正上屏的中文；`take_commit_elements`
        //   的词条对整句/部分确认可能给原始编码（2026-11-01 踩过）。
        // * 上文取 `pre_commit_context`（`collect_commit_history` 在 push 之前
        //   抢存：提交后 latest_text 已是新词，相邻同类段还会合并成一条）。
        // 2026-11-08 修「预测重复刚上屏的文字」：旧实现把 word 当续写 prefix，
        // 结果每条都是「word + 续写」（「你好」→「你好像是」）。现在 prefix 留空、
        // 整段文本当 context，模型直接生成**下一个词**；合并层再把 word 本身剔掉。
        // 空词/非中文/单字不跑神经（单字联想噪声大，且每次要付 ~77ms）。
        let word = self
            .context
            .commit_history
            .last()
            .map(|record| record.text.clone())
            .unwrap_or_default();
        // 2026-11-09：`WEASEL_PREDICT=off` 原先只关候选窗补位，这条上屏联想
        // 不受管（关了开关照样每提交付一次神经推理、联想条照弹）。总开关与
        // 候选窗那条共用；关掉时联想条整个收起（也不回退个人 n-gram）。
        // 闸门本身在 `rescore::predict_sync` 入口还有一道；这里只需要知道
        // 「要不要回退个人 n-gram」。
        let predictions_on = crate::gear::neural::predictions_enabled();
        let neural = if predictions_on && is_neural_predict_word(&word) {
            // 「不读上文」时只把刚上屏的词交给模型，不带历史上下文。
            let context = if crate::engine::privacy::context_enabled() {
                format!("{}{}", self.context.pre_commit_context, word)
            } else {
                word.clone()
            };
            crate::gear::filter::rescore::predict_sync(&context, &word, 4)
        } else {
            Vec::new()
        };
        self.context.predictions = resolve_predictions(predictions_on, neural, &self.context);
        // 同样的借用规避：`memorize` 要 `&mut self`，先把列表取到局部
        let mut translators = std::mem::take(&mut self.translators);
        // 提交历史按值拷一份（≤20 条）：`translators` 已经移出 `self`，
        // 这里只是为了让下面的循环不必反复借 `self.context`
        let history = self.context.commit_history.clone();
        let t_memorize = crate::engine::stage_trace::enabled().then(std::time::Instant::now);
        for entry in &mut translators {
            if entry.is_enabled() {
                entry.component.memorize(&elements, &history);
            }
        }
        if let Some(t) = t_memorize {
            crate::engine::stage_trace::emit(
                self.stage_seq,
                "memorize",
                t.elapsed().as_micros(),
            );
        }
        self.translators = translators;
    }

    /// 神经重排结果落地检查:后台线程算完一批(代数变了)且当前正在组字,
    /// 就 re-compose 一次——重排滤镜这次能在结果表里命中,候选列表当场
    /// 跳到神经顺序。线程不能直接摸引擎(非 Send),这是轮询侧的信号通道。
    ///
    /// 由 C API 的 `rime_get_context` 在每次快照前调用:宿主每键/每次重绘
    /// 都会读上下文,落地的分最多迟一个读周期就能上屏。
    pub fn poll_rescore_landing(&mut self) {
        let Some(worker) = crate::gear::filter::rescore::global() else {
            return;
        };
        let generation = worker.current_generation();
        if generation == self.rescore_seen_generation {
            return;
        }
        self.rescore_seen_generation = generation;
        // 抄 msime `ApplyCandidateTranslations` 的 `glosses_changed`：代数变了
        // 先逐条比「这次落地的东西」和「屏幕上的东西」。恒等 order / 同文本
        // 预测对候选菜单没有任何影响，再 compose 一次是纯浪费（实测每键一帧）。
        let landed = worker.last_landed();
        let mut needs_recompose = landed.as_ref().map_or(true, |sig| {
            sig.changes_from(self.rescore_applied_signature.as_ref())
        });
        // 抄 CloudInputMethodEditor `advance_rescoring` 的 navigated 闸门
        // （目标仓 GPL-3.0，只取思路）：异步结果落地时若用户已经翻页或
        // 动过高亮，当前页已被接管，重排换序会让整页内容跳变 ⇒ 丢弃这次落地。
        // 下一轮打字（高亮回 0、新批次落地）自然恢复神经顺序。
        if needs_recompose
            && let Some(submitted) = worker.last_landed_nav()
        {
            let current = crate::gear::filter::rescore::NavSnapshot::new(
                self.context.highlight_index(),
                self.context.page_size,
            );
            if submitted.defers(&current) {
                needs_recompose = false;
                tracing::debug!(
                    "[导航闸门] 丢弃异步落地：提交时高亮={}/页大小={}，落地时高亮={}/页大小={}",
                    submitted.highlight, submitted.page_size, current.highlight, current.page_size
                );
            }
        }
        if !needs_recompose {
            return;
        }
        self.rescore_applied_signature = landed;
        // 正在组字才值得重算;空输入重 compose 无害但浪费。
        if self.context.is_composing() && !self.context.input.is_empty() {
            // ⚠️ 2026-11-03：**必须先让末段重新翻译**。
            // `translate_segments` 会跳过「Guess 且有菜单」的段（那是正常路径：不重复翻译），
            // 于是直接 `compose()` 时这段根本不重译，而 `apply_filters` 又只作用在
            // 空转的旧菜单上 —— 滤镜看到的是已被 `prepare()` 消费过的流，头部逐轮被啃，
            // 批次键永远对不上，`order_for` 一次都不命中（探针实测 11 次 compose 全 MISS）。
            // 清掉末段菜单 ⇒ compose 重译 ⇒ 滤镜套在新流上 ⇒ 结果表才可能命中。
            // 高亮位置（`selected_index`）先存后补，避免重排落地把光标弹回第一页。
            let keep = self
                .context
                .composition
                .segmentation
                .segments
                .last()
                .map(|s| s.selected_index);
            // 只对**还没被确认**的末段重译：`Selected/Confirmed` 段是用户已经选定的，
            // 清掉菜单会把确认结果重算掉（那是 `compose` 另一条路的语义）。
            let reopen = self
                .context
                .composition
                .segmentation
                .segments
                .last()
                .map(|s| s.status <= SegStatus::Guess)
                .unwrap_or(false);
            if reopen {
                if let Some(seg) = self.context.composition.segmentation.segments.last_mut() {
                    seg.menu = None;
                }
            }
            self.compose();
            if let Some(idx) = keep
                && let Some(seg) = self.context.composition.segmentation.segments.last_mut()
            {
                let n = seg.menu.as_ref().map(|m| m.prepare(idx + 1)).unwrap_or(0);
                seg.selected_index = idx.min(n.saturating_sub(1));
            }
        }
    }

    /// C++ `ConcreteEngine::ProcessSyntheticKey`：由组件（如 key_binder 的 `send:`）
    /// 请求引擎代转的按键。
    ///
    /// **这里偏离了 C++ 的调用时机**：C++ 是在 `ProcessKeyEvent` 里同步递归调回去的。
    /// Rust 侧做不到 —— 遍历 `self.processors` 时为了把 `&mut self` 交给组件，
    /// 必须先把列表 `mem::take` 出来，嵌套调用看到的就会是空链表（实测踩过：
    /// `Ctrl+H → BackSpace` 转发后什么都没发生）。
    ///
    /// 于是改成**排队**：组件登记按键，主循环结束后由 [`Engine::process_key`]
    /// 逐条重放。行为等价 —— 原处理器链在 key_binder 返回 `Accepted` 时就停了，
    /// 不存在"转发赶不上后续处理器"的差别。队列是 **FIFO** 的，多键序列保序。
    pub fn enqueue_synthetic_key(&mut self, key: KeyEvent) {
        self.synthetic_queue.push_back((key, false));
    }

    /// 同上，但**没人吃下这个键时把该字符直接上屏**。
    ///
    /// 对应 C++ `ChordComposer::FinishChord` 里那一支（`chord_composer.cc:228-231`）：
    /// 和弦编码逐键转发，`ProcessSyntheticKey` 返回 false 就 `CommitText` ——
    /// 典型场景是 `output_format` 把和弦映射成空格，而空格在空闲态无人接管。
    pub fn enqueue_synthetic_key_with_fallback(&mut self, key: KeyEvent) {
        self.synthetic_queue.push_back((key, true));
    }

    /// 跑一遍处理器链（[`Engine::process_key`] 与合成按键重放共用）。
    fn run_processor_chain(&mut self, key: &KeyEvent) -> bool {
        let mut processors = std::mem::take(&mut self.processors);
        let mut result = false;
        for entry in processors.iter_mut() {
            if !entry.is_enabled() {
                continue;
            }
            match entry.component.process_key_event(self, key) {
                ProcessResult::Rejected => break,
                ProcessResult::Accepted => {
                    result = true;
                    break;
                }
                ProcessResult::Noop => continue,
            }
        }
        self.processors = processors;
        result
    }

    /// C++ `ConcreteEngine::ApplySchema`：换方案并重装配组件。
    ///
    /// **交互式切换（Ctrl+` / F4 菜单）也走这里**，所以两种模式下语义不同：
    ///
    /// - `async_rebuild == true`（生产：C API 宿主持有会话表）：只登记目标方案，
    ///   **立刻返回 `true`（= 已受理）**。这次按键不再等重建，宿主
    ///   `rime_process_key` 放锁后把它交给 `session_rebuild` 的工作线程；重建期间
    ///   按键继续进旧会话（旧引擎是只读快照），换入后组合串丢弃
    ///   （跨方案迁移必然错位，见 `Session::transfer_pending_commit`）。
    /// - `async_rebuild == false`（缺省：单元测试/e2e/探针直接持有 `Session`）：
    ///   没有会话表可换入，只能就地同步重建，即历史语义。
    pub fn apply_schema(&mut self, schema_id: &str) -> bool {
        if self.async_rebuild {
            // 注意：这里**不要**碰 `self.schema` / `processors` —— 换入是原子的，
            // 旧引擎在后台任务换入前必须保持原样，否则按键会拿到半新半旧的组件链。
            self.pending_rebuild = Some(schema_id.to_string());
            return true;
        }
        self.apply_schema_sync(schema_id)
    }

    /// 同步重建（历史行为，也是 `apply_schema` 在 `async_rebuild == false` 时的实现）。
    fn apply_schema_sync(&mut self, schema_id: &str) -> bool {
        let t_open = std::time::Instant::now();
        match Schema::open(&self.build_dir, schema_id) {
            Some(schema) => {
                trace_schema_ms(&format!("Schema::open({schema_id})"), t_open);
                self.schema = schema;
                let t_init = std::time::Instant::now();
                self.initialize_components();
                trace_schema_ms(&format!("initialize_components({schema_id})"), t_init);
                // 重新初始化期间 `initialize_options` 的 set_option 已发过信号：组件链
                // 是按新方案全新装配的、条件天然正确，这里只同步镜像，防止下次
                // `set_option` 把这批信号当新变化误清组字。
                self.sync_option_mirror_from_signals();
                self.context.clear();
                // C++ `Schema::page_size` 是引擎级的：selector 翻页、c_api 菜单都读它
                self.context.page_size = self.schema.page_size;
                // C++ `engine.cc:377`：切成功后 `switcher_->SetActiveSchema()`，
                // 记下"上次用的方案"与访问时间 —— 菜单里的方案排序靠它。
                let t_record = std::time::Instant::now();
                switcher::record_active_schema(self.user_dir.as_deref(), schema_id);
                trace_schema_ms(&format!("record_active_schema({schema_id})"), t_record);
                true
            }
            None => {
                eprintln!("[engine] unknown schema: {schema_id}");
                false
            }
        }
    }

    /// 打开/关闭"切方案交给宿主异步重建"（见 [`Self::apply_schema`]）。
    pub fn set_async_rebuild(&mut self, on: bool) {
        self.async_rebuild = on;
    }

    /// 取走异步切换请求（宿主在**放锁之后**调用；见 `c_api::rime_process_key`）。
    pub fn take_pending_rebuild(&mut self) -> Option<String> {
        self.pending_rebuild.take()
    }

    /// 登记一次方案切换请求（供 `switcher` 这类组件用）。
    ///
    /// 不能在组件里直接调 [`Self::apply_schema`]：那时 `processors` 列表正被
    /// `mem::take` 在局部遍历，重建会写进 `self.processors`，但遍历结束又会被那份
    /// 旧列表覆盖 —— 切换静默失效。登记后由 [`Self::process_key`] 统一执行。
    pub fn request_schema(&mut self, schema_id: String) {
        self.pending_schema = Some(schema_id);
    }

    /// 待生效的方案切换请求（测试/诊断用；`process_key` 执行后清空）。
    pub fn pending_schema(&self) -> Option<&str> {
        self.pending_schema.as_deref()
    }

    /// 清掉待生效的方案切换请求（测试用）。
    #[cfg(test)]
    pub(crate) fn clear_pending_schema(&mut self) {
        self.pending_schema = None;
    }

    /// C++ `Switcher::SelectNextSchema`：方案清单里的下一个方案。
    ///
    /// `key_binder` 的 `select: .next` 走这里（`key_binder.cc:155` 临时建一个
    /// `Switcher` 再调它）。返回 `None` = 清单里只有一个方案。
    pub fn next_schema_id(&self) -> Option<String> {
        let switcher = Switcher::new(
            self.default_config.clone(),
            &self.build_dir,
            self.user_dir.clone(),
        );
        switcher.next_schema(&self.schema.schema_id)
    }

    /// 设定开关（给组件之外的调用方复用）。
    ///
    /// C++ `set_option` 会发 `option_update_notifier` ⇒ `OnOptionUpdate` ⇒
    /// `UpdateComponentConditions`——组件的 `when` 条件**立即**重算。漏了这一步,
    /// 宿主经 API 切开关(简繁/西文中文)后,组件列表要等下一次按键才自愈。
    ///
    /// **信号化（Qt 范式）**：这里只负责"发"——`Context::set_option` 把
    /// `OptionChanged` 挂上总线（其中就有本引擎自己的 queued 订阅），随后
    /// [`Self::drain_option_signals`] 消费自己的队列并给出反应计划。跨模块路径
    /// （开关变化 ⇒ 组件刷新 / 清空 / 重译）全部经信号，不再有 set_option →
    /// refresh 的直接调用；组件在任何地方经 `Context::set_option` 改开关也走同一条路。
    pub fn set_option(&mut self, name: &str, value: bool) {
        self.context.set_option(name, value);
        let act = self.drain_option_signals();
        if act.refresh {
            self.refresh_component_conditions();
        }
        if act.clear {
            // 与 `process_key` 的 key_binder 路径同一语义：换输入类型 ⇒ 清空组字。
            // 这里覆盖 TIP 侧直接调 `set_option` 的入口，避免"按键切清空、接口切
            // 保留重译"两套行为。
            self.context.clear();
            return;
        }
        if act.compose {
            // C++ OnOptionUpdate:组字中 ⇒ `RefreshNonConfirmedComposition` ⇒ 重算
            self.context.refresh_non_confirmed_composition();
            self.compose();
        }
    }

    /// Qt 槽 `Engine::onOptionChanged` 的等价物：从本引擎的信号队列取出
    /// `OptionChanged` 并推算反应计划。
    ///
    /// `changed` 用镜像（上次已反应过的值）推算，对齐 Qt 槽里"用旧值比对新值"；
    /// 镜像缺省视为 `false`，与 `Context::get_option` 的默认一致。其它引擎 / 宿主
    /// 发的信号按 `session_id` 过滤掉（对象亲和隔离）。
    fn drain_option_signals(&mut self) -> OptionReaction {
        // 字段级拆借：闭包要可变借用镜像/context/输入类型清单，drain 又要借
        // signal_rx —— 整个 `&mut self` 会撞借用，按字段拆开才互不相交。
        let Engine {
            signal_rx,
            signal_option_mirror,
            context,
            input_type_options,
            ..
        } = self;
        let mut act = OptionReaction::default();
        crate::signal::drain(signal_rx, |note| {
            let Signal::OptionChanged { name, on } = &note.signal else {
                return;
            };
            if note.session_id != context.signal_session_id {
                return;
            }
            let prev = signal_option_mirror.get(name).copied().unwrap_or(false);
            signal_option_mirror.insert(name.clone(), *on);
            act.refresh = true;
            if prev != *on && input_type_options.iter().any(|option| option == name) {
                act.clear = true;
            }
            if context.is_composing() {
                act.compose = true;
            }
        });
        act
    }

    /// 只消费队列、更新镜像，**不做任何反应**。
    ///
    /// `process_key` 里组件改 option 的反应已由那里的 refresh/输入类型清空逻辑
    /// （对照 C++ `ProcessKey` 尾部）做过；若不把那些信号从这里收走并同步镜像，
    /// 下次 [`Self::set_option`] 会把它们当新变化再反应一遍——可能把用户刚敲的串清掉。
    fn sync_option_mirror_from_signals(&mut self) {
        let Engine {
            signal_rx,
            signal_option_mirror,
            context,
            ..
        } = self;
        crate::signal::drain(signal_rx, |note| {
            if note.session_id != context.signal_session_id {
                return;
            }
            if let Signal::OptionChanged { name, on } = &note.signal {
                signal_option_mirror.insert(name.clone(), *on);
            }
        });
    }

    /// 当前各输入类型开关的状态快照（用于检测一次按键里是否发生了切换）。
    fn input_type_option_states(&self) -> Vec<bool> {
        self.input_type_options
            .iter()
            .map(|name| self.context.get_option(name))
            .collect()
    }

    /// C++ `ConcreteEngine::Compose`：caret 感知的重算入口。
    ///
    /// 三处必须与 C++ 一致：
    /// 1. 参与组合的是 **caret 之前**的活动输入；
    /// 2. caret 落在已确认段首尾且后面还有字符时，改用**整串** reset（给光标前后一起重算的机会）；
    /// 3. `reset` 是**差量**的 —— 已确认段（`status >= Selected`）不会被后续编辑推翻。
    pub fn compose(&mut self) {
        let trace = stage_trace_enabled();
        let t_seg = trace.then(std::time::Instant::now);
        let input = self.context.input.clone();
        let caret = self.context.caret_pos.min(input.len());
        {
            let comp = &mut self.context.composition;
            comp.reset(input.get(..caret).unwrap_or(&input));
            if caret < input.len() && caret == comp.segmentation.confirmed_position() {
                comp.reset(&input);
            }
        }
        if input.is_empty() {
            // C++ `Compose("")` 的落点：`Reset("")` 弹光旧段后 `Forward()` 补的
            // 空段，会被 `CalculateSegmentation` 尾部的 `Trim()` 摘掉——组合最终
            // 为**空**，`IsComposing()` 才回到 false（Esc 清场的判定就靠它）。
            // 这里不能学 C++ 补空段再摘，直接清干净即可。
            let comp = &mut self.context.composition;
            comp.segmentation.segments.clear();
            return;
        }
        self.calculate_segmentation(caret);
        if let Some(t) = t_seg {
            self.last_stages.seg_us = t.elapsed().as_micros() as u64;
        }
        let t_trans = t_seg.map(|_| std::time::Instant::now());
        self.translate_segments();
        if let Some(t) = t_trans {
            self.last_stages.trans_us = t.elapsed().as_micros() as u64;
        }
        let t_filter = t_seg.map(|_| std::time::Instant::now());
        self.apply_filters();
        if let Some(t) = t_filter {
            self.last_stages.filter_us = t.elapsed().as_micros() as u64;
        }
    }

    /// C++ `ConcreteEngine::InitializeOptions`（`engine.cc:616-635`）：
    /// 方案 `switches:` 里配了 `reset:` 的开关，引擎启动/换方案时落到 context
    /// 选项——明月简体的 `zh_simp`（`reset: 1`）就靠它置真，simplifier 才会
    /// 生效。此前没移植，所有开关选项默认 false，simplifier 永远不生效。
    fn initialize_options(&mut self) {
        let switches = crate::gear::schema::switches::Switches::new(&self.schema.config);
        for option in switches.all_options() {
            if option.reset_value < 0 {
                continue; // 没配 reset 的开关保持默认
            }
            let value = match option.kind {
                crate::gear::schema::switches::SwitchType::ToggleOption => option.reset_value != 0,
                crate::gear::schema::switches::SwitchType::RadioGroup => {
                    option.option_index as i32 == option.reset_value
                }
            };
            self.context.set_option(&option.option_name, value);
        }
        // 顶层 `input_types.reset` = 默认选中的输入类型。若它不在可用清单里（比如
        // 从 `enabled` 里删掉了），回退到清单第一项并记日志：报错会让整个方案起
        // 不来，回退到"options 顺序的第一项"则是确定且可预测的，键盘仍可用。
        if let Some(reset) = crate::gear::dict::dict_compiler::input_types_reset(&self.schema.config) {
            use crate::lever::tree::log_warn;
            let chosen = if self.input_type_options.iter().any(|name| name == &reset) {
                reset
            } else if let Some(fallback) = self.input_type_options.first().cloned() {
                log_warn(&format!(
                    "input_types.reset: '{reset}' 不在可用输入类型里（可用：{}），回退到 '{fallback}'",
                    self.input_type_options.join(", ")
                ));
                fallback
            } else {
                reset
            };
            for option in &self.input_type_options {
                self.context.set_option(option, option == &chosen);
            }
        }
    }

    /// 切换输入类型（互斥单选）：把 `name` 设为 true，其余 `input_type_options` 设
    /// false，并清空已组字。供方案菜单第二级的 [`crate::gear::processor::switcher::SwitcherCommand::InputType`]
    /// 使用——与 `initialize_options` 的互斥循环同语义，但运行期可随时调用，
    /// 不依赖 `input_types.reset`。
    ///
    /// 运行期若只 `set_option(name, true)` 而不清兄弟项，会有多个 input_type 开关同时
    /// 为真；`input_type_speller` / translator 的 `sync_context` 用 `find` 取**第一个**为
    /// 真的项，于是实际生效类型退化为"字母序最小的那一个"，表现不可预测。这里强制
    /// 互斥，保证 `name` 是唯一为真的项。
    pub(crate) fn select_input_type(&mut self, name: &str) {
        if !self.input_type_options.iter().any(|option| option == name) {
            return; // 不在可用清单里：忽略，避免越权 option 名
        }
        // 先拷出清单再改 self：循环里调 `set_option` 会可变借用 `self`，若直接迭代
        // `&self.input_type_options` 会触发借用冲突（E0502）。
        let options = self.input_type_options.clone();
        for option in &options {
            let selected = option == name;
            if self.context.get_option(option) != selected {
                self.set_option(option, selected);
            }
        }
        // 换类型清空组字（与 `set_option` 的 input_type 分支一致；上面循环里最后一个
        // 变化的 input_type 已触发过 clear，这里再补一次确保干净）。
        self.context.clear();
        self.swallow_next_space = true;
    }

    /// C++ `CalculateSegmentation`：让每个 segmentor 依次 `Proceed`，跑完一轮 `Forward`。
    fn calculate_segmentation(&mut self, caret_pos: usize) {
        // 同样的借用规避：把组件列表取到局部再遍历
        let segmentors = std::mem::take(&mut self.segmentors);
        let ctx = &mut self.context;
        let view = SegmentorView {
            ascii_mode: ctx.get_option("ascii_mode"),
            full_shape: ctx.get_option("full_shape"),
        };
        // `commit_history` 与 `composition.segmentation` 是兄弟字段，分开借不必拷贝
        // （C++ 的 `PunctSegmentor::Proceed` 也是直接读 `context()->commit_history()`）。
        let history = &ctx.commit_history;
        {
            let segs = &mut ctx.composition.segmentation;
            while !segs.has_finished() {
                let start_pos = segs.current_start();
                for entry in &segmentors {
                    if !entry.is_enabled() {
                        continue;
                    }
                    if !entry.component.proceed(segs, view, history) {
                        break;
                    }
                }
                // 没有任何推进 → 收手，否则死循环
                if start_pos == segs.current_end() {
                    break;
                }
                // caret 之后只允许多一段
                if start_pos >= caret_pos {
                    break;
                }
                if !segs.has_finished() {
                    segs.forward();
                }
            }
            if !segs.segments.is_empty()
                && !segs
                    .segments
                    .last()
                    .map_or(false, |s| s.has_tag("placeholder"))
            {
                segs.trim();
            }
            if !segs.segments.is_empty()
                && segs
                    .segments
                    .last()
                    .map_or(false, |s| s.status >= SegStatus::Selected)
            {
                segs.forward();
            }
        }
        self.segmentors = segmentors;
    }

    /// C++ `TranslateSegments`：逐段取子串喂给每个 translator，候选偏移回绝对位置。
    fn translate_segments(&mut self) {
        let mut translators = std::mem::take(&mut self.translators);
        let input = self.context.composition.segmentation.input.clone();
        // translator 要读的运行期开关（`full_shape` 之类）在这里同步一次 ——
        // 它们的 `query` 拿不到 `Context`（会和 `&mut segments` 借冲突）。
        for entry in &mut translators {
            entry.component.sync_context(&self.context);
        }
        // 提交历史要按值拷一份：`segs` 借的是 `self.context` 的可变字段，
        // 不能再同时借 `self.context.commit_history`（最多 20 条，拷贝可忽略）。
        let history = self.context.commit_history.clone();
        let segs = &mut self.context.composition.segmentation.segments;
        for seg in segs.iter_mut() {
            // 删词（`Context::delete_candidate`）会把段 `reopen` 成 Guess 且清空菜单，
            // 并置 `needs_recompose` 期望下一轮 `compose` 重译出"去掉被删词"的新菜单。
            // 若只按 `status >= Guess` 跳过，这段就永远得不到翻译、菜单恒空 —— 用户报的
            // "删词后候选全空、什么都看不见"正是它。真正译好的段是「Guess **且** 有菜单」，
            // 这类才跳过；菜单被清掉的 Guess 段必须重新翻译。
            if seg.status >= SegStatus::Guess && seg.menu.is_some() {
                continue;
            }
            let len = seg.end.saturating_sub(seg.start);
            let Some(text) = input.get(seg.start..seg.start + len) else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            let mut menu = Menu::new();
            // 兜底候选（`echo_translator`）：**只在正经候选全空时**才进菜单。
            // C++ 语义由 `EchoTranslation::Compare`（`other->exhausted()` 时 echo 优先）
            // 实现；懒化下翻译器在查询时不知道自己会不会有产出，所以把兜底流
            // 挂成 [`TieredTranslation`] 的**最底层**——上层全部穷尽了它才浮现，
            // 与 C++ 的 Compare 行为逐条对齐。
            let mut fallback: Option<Box<dyn crate::gear::translator::translation::Translation>> = None;
            // 段标签决定翻译器是否认领这段（C++ `segment.HasAnyTagIn(tags_)`）——
            // 标签现在由 translator 自己从 `segment.tags` 读，不再单独传。
            for entry in &mut translators {
                if !entry.is_enabled() {
                    continue;
                }
                let is_fallback = entry.component.is_fallback();
                if is_fallback {
                    // echo_translator 至多一条候选,急切取(代价可忽略);
                    // 候选自带绝对偏移(echo 收 segment 自己算)。
                    if let Some(mut translation) = entry.component.query(text, seg, &history) {
                        let mut items = Vec::new();
                        while let Some(c) = translation.peek() {
                            items.push(c);
                            translation.next();
                            if items.len() > 4 { break; } // echo 至多几条,防御
                        }
                        if !items.is_empty() {
                            fallback = Some(Box::new(
                                crate::gear::translator::translation::VecTranslation::new(items),
                            ));
                        }
                    }
                    continue;
                }
                // 懒化管线:翻译器直接交出流式 Translation(Menu 内部归并)。
                if let Some(translation) = entry.component.query(text, seg, &history) {
                    menu.add_translation(translation);
                }
            }
            if let Some(fallback) = fallback {
                // C++ `EchoTranslation::Compare`:别的翻译还活着 echo 就让位。
                // 懒流在装配期探一口(menu.prepare(1))——有产出就不挂兜底;
                // 全空才把兜底流挂成最底层。
                if menu.prepare(1) == 0 {
                    menu.add_tier(fallback);
                }
            }
            seg.menu = Some(menu);
            seg.status = SegStatus::Guess;
            seg.selected_index = 0;
        }
        self.translators = translators;
    }

    /// C++ `TranslateSegments` 里 `menu->AddFilter(...)` 那段：逐段套用 filter 链。
    fn apply_filters(&mut self) {
        let n = self.context.composition.segmentation.segments.len();
        for i in 0..n {
            // 把该段的 `menu` 所有权先 `take` 出来：否则 `apply` 要借 `&self.context`
            // （不可变）的同时，`seg` 还持有 `self.context` 字段的可变借用，会冲突。
            let Some(mut menu) = self
                .context
                .composition
                .segmentation
                .segments
                .get_mut(i)
                .and_then(|seg| seg.menu.take())
            else {
                continue;
            };
            // 2026-11-03：菜单一旦套过滤镜链就不再套第二遍。
            // 没有这条时，同一段在 re-compose 时会于**已被 `prepare()` 消费过的**流上
            // 再包一层滤镜（`Menu::add_filter` 就是把 `result` 换成新包装），头部逐轮
            // 被啃：探针实测同一输入连续 compose，重排批次 6→3→2→1 条，批次键因此
            // 永远对不上，`order_for` 一次都没命中（神经重排等于全程空转）。
            if menu.filters_ready() {
                self.context.composition.segmentation.segments[i].menu = Some(menu);
                continue;
            }
            // 这一段该套哪些 filter
            // `apply` 取 `&mut self`：`Context` 不可变借用与 `filters` 可变借用是
            // 不同字段，分开绑一次就能同时用。
            let context = &self.context;
            let filters = &mut self.filters;
            let applicable: Vec<bool> = filters
                .iter_mut()
                .map(|entry| {
                    let seg = &context.composition.segmentation.segments[i];
                    entry.is_enabled() && entry.component.applies_to_segment(context, seg)
                })
                .collect();
            for (entry, applies) in filters.iter_mut().zip(applicable) {
                if !applies {
                    continue;
                }
                menu.add_filter(context, entry.component.as_mut());
            }
            // 神经重排:最后一段、滤镜链最外层(见 neural/rescore_filter.rs)。
            if i + 1 == n {
                // 方案配置 `translator/enable_rerank_head`（缺省 true = 影子模式）。
                let head_enabled = crate::gear::neural::rerank_head::scheme_enabled(
                    &self.schema.config,
                );
                // 个性化开关只影响线性头的 pin 特征；关掉后与基线逐位一致。
                let personalization_enabled = self
                    .schema
                    .config
                    .pointer("/translator/enable_personalization")
                    .and_then(|value| {
                        value
                            .as_bool()
                            .or_else(|| value.as_i64().map(|number| number != 0))
                    })
                    .unwrap_or(true);
                let mut neural_filter =
                    crate::gear::neural::NeuralRescoreFilter::new(
                        head_enabled,
                        personalization_enabled,
                    );
                if neural_filter.applies_to_segment(
                    context,
                    &context.composition.segmentation.segments[i],
                ) {
                    menu.add_filter(context, &mut neural_filter);
                }
                // 续写预测:后挂 ⇒ 包在重排外面，拿到的是重排后的顺序。
                // 只在候选稀薄时才真出手(见 neural/predict_filter.rs)。
                let mut predict_filter = crate::gear::neural::NeuralPredictFilter;
                if predict_filter.applies_to_segment(
                    context,
                    &context.composition.segmentation.segments[i],
                ) {
                    menu.add_filter(context, &mut predict_filter);
                }
            }
            menu.mark_filters_ready();
            self.context.composition.segmentation.segments[i].menu = Some(menu);
        }
    }

    /// 已装配的 processor 名单（诊断用：验证 `engine/processors` 真的被读进来了）。
    pub fn processor_names(&self) -> Vec<&str> {
        self.processors.iter().map(|e| e.name.as_str()).collect()
    }

    /// 已装配的 translator 名单。
    pub fn translator_names(&self) -> Vec<&str> {
        self.translators.iter().map(|e| e.name.as_str()).collect()
    }

    /// 已装配的 segmentor 名单。
    pub fn segmentor_names(&self) -> Vec<&str> {
        self.segmentors.iter().map(|e| e.name.as_str()).collect()
    }

    /// 已装配的 filter 名单。
    pub fn filter_names(&self) -> Vec<&str> {
        self.filters.iter().map(|e| e.name.as_str()).collect()
    }

    pub fn schema_id(&self) -> &str {
        &self.schema.schema_id
    }

    pub fn schema_name(&self) -> &str {
        &self.schema.schema_name
    }

    pub fn composition(&self) -> &Composition {
        &self.context.composition
    }

    /// 测试专用：不加载方案的空引擎（处理器链与翻译器链为空）。
    #[cfg(test)]
    pub fn empty_for_test() -> Engine {
        let mut engine = Engine {
            schema: Schema::empty_for_test(),
            context: Context::new(),
            translators: Vec::new(),
            processors: Vec::new(),
            segmentors: Vec::new(),
            filters: Vec::new(),
            synthetic_queue: std::collections::VecDeque::new(),
            build_dir: std::path::PathBuf::new(),
            data_roots: Vec::new(),
            default_config: serde_json::Value::Object(serde_json::Map::new()),
            user_dir: None,
            pending_schema: None,
            async_rebuild: false,
            pending_rebuild: None,
            rescore_seen_generation: 0,
            rescore_applied_signature: None,
            swallow_next_space: false,
            swallow_next_digit: false,
            input_type_options: Vec::new(),
            last_stages: StageTimings::default(),
            stage_seq: 0,
            signal_rx: mpsc::sync_channel(1).1,
            signal_sub: 0,
            signal_option_mirror: HashMap::new(),
        };
        engine.context.signal_session_id = next_engine_session();
        let (signal_tx, signal_rx) = mpsc::sync_channel(256);
        engine.signal_sub = crate::signal::connect_queued(signal_tx);
        engine.signal_rx = signal_rx;
        engine
    }

    /// 测试用：读信号镜像（验证信号真的走了一圈 emit → 自己的队列 → drain）。
    #[cfg(test)]
    pub(crate) fn signal_mirror_get(&self, name: &str) -> Option<bool> {
        self.signal_option_mirror.get(name).copied()
    }
}

/// 上屏联想条的最终内容裁决。
///
/// * 总开关关（`WEASEL_PREDICT=off` / `enable_neural_inference=false`）⇒ **空表**，
///   连个人 n-gram 也不回退（2026-11-09 用户口径：关掉就该一条都不出）；
/// * 神经给了结果 ⇒ 取前 4 条；
/// * 神经不可用/没结果 ⇒ 回退个人 n-gram。
///
/// 抽成纯函数只为让这条口径有回归用例 —— 判断逻辑本身没有第二处实现。
fn resolve_predictions(
    predictions_on: bool,
    neural: Vec<(String, f64)>,
    context: &crate::engine::context::Context,
) -> Vec<String> {
    if !predictions_on {
        return Vec::new();
    }
    if neural.is_empty() {
        return crate::engine::user_ngram::predictions_for_context(context);
    }
    neural
        .into_iter()
        .map(|(text, _score)| text)
        .take(4)
        .collect()
}

/// 上屏联想条的神经预测只对「多字中文词」出手：单字/纯西文/标点没有"下一个词"
/// 的语境，而且提交路径上每次要付 ~77ms（见 [`Engine::apply_commit`]），不值得。
fn is_neural_predict_word(word: &str) -> bool {
    word.chars().count() > 1
        && word
            .chars()
            .any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c))
}

/// 从方案配置里收集**输入类型开关**名。
///
/// 来源与 [`ScriptTranslator::apply_prism_config`] 解析的完全一致：
/// - `/translator/prisms`：`[{name, option|input_type}]` 或 `{input_type: prism}`；
/// - `/translator/prisms: input_type`：展开顶层 `/input_type/options[].option`；
/// - `/translator/input_types`：`{input_type: prism}`；
/// - `/translator/simp/...`、`/translator/trad/...`（简繁变体的同一套声明）。
///
/// 某个命名空间**完全没有**手写声明时，若 `build_dir` 里有部署期棱镜索引
/// （`build/<dict>/prisms.list`），用索引里的组名补上 —— 这保证删掉 `prisms`
/// 后，切换输入类型仍会清空已输入串，与手写清单的行为一致。索引缺失/过期时
/// 不补，回退到旧行为。
///
/// 这些名字既是 `translator` 读取的 context switch，也是 `key_binder` 要
/// `toggle:` 的开关 —— 两处必须用同一份清单，所以只在这里定义一次。
pub(crate) fn collect_input_type_options(
    config: &serde_json::Value,
    build_dir: Option<&std::path::Path>,
) -> Vec<String> {
    let prism_index = build_dir.and_then(|dir| {
        let dict_name = config.pointer("/translator/dict")?.as_str()?;
        crate::gear::dict::prism_index::read_index(dir, dict_name)
    });
    // 顶层 `input_types` 是权威清单：`enabled` 决定**集合**（未列的不该有产物），
    // `options` 只决定**顺序与文案**。两种情况都先按 options 排序，再把 options
    // 没提到、但确实启用的名字按字母序补在后面。
    // 2026-11-13：单数 `input_type` 与复数 `input_types` 都认（见
    // `dict_compiler::input_types_node`）；数据迁移期两边都能跑。
    if let Some(node) = crate::gear::dict::dict_compiler::input_types_node(config) {
        let order = crate::gear::translator::translator::input_type_option_names(node);
        let enabled = match crate::gear::dict::dict_compiler::input_types_enabled(config) {
            Some(enabled) => enabled,
            None => prism_index
                .as_ref()
                .map(|entries| entries.iter().map(|entry| entry.group.clone()).collect())
                .unwrap_or_default(),
        };
        let mut options: Vec<String> = Vec::new();
        for name in &order {
            if enabled.iter().any(|item| item == name) {
                push_input_type_option(&mut options, name);
            }
        }
        let mut rest: Vec<String> = enabled
            .into_iter()
            .filter(|name| !options.iter().any(|item| item == name))
            .collect();
        rest.sort();
        for name in &rest {
            push_input_type_option(&mut options, name);
        }
        return options;
    }
    const PREFIXES: [&str; 3] = ["/translator", "/translator/simp", "/translator/trad"];
    // 任何命名空间只要写了手写名单，就完全按写的来（旧 schema 行为不变）；
    // 全部没有声明时才用部署索引补全。
    let declared_any = PREFIXES.iter().any(|prefix| {
        config
            .pointer(&format!("{prefix}/prisms"))
            .is_some()
            || config
                .pointer(&format!("{prefix}/input_types"))
                .and_then(serde_json::Value::as_object)
                .is_some()
    });
    if !declared_any {
        return prism_index
            .unwrap_or_default()
            .into_iter()
            .map(|entry| entry.group)
            .collect();
    }

    let mut options: Vec<String> = Vec::new();
    for prefix in PREFIXES {
        if let Some(prisms) = config.pointer(&format!("{prefix}/prisms")) {
            match prisms {
                serde_json::Value::Array(items) => {
                    for item in items {
                        let name = item
                            .get("option")
                            .or_else(|| item.get("input_type"))
                            .and_then(serde_json::Value::as_str);
                        if let Some(name) = name {
                            push_input_type_option(&mut options, name);
                        }
                    }
                }
                serde_json::Value::Object(map) => {
                    for name in map.keys() {
                        push_input_type_option(&mut options, name);
                    }
                }
                serde_json::Value::String(reference)
                    if reference.trim() == "input_types" && prism_index.is_some() =>
                {
                    if let Some(node) =
                        crate::gear::dict::dict_compiler::input_types_node(config)
                    {
                        for name in
                            crate::gear::translator::translator::input_type_option_names(node)
                        {
                            push_input_type_option(&mut options, &name);
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(map) = config
            .pointer(&format!("{prefix}/input_types"))
            .and_then(serde_json::Value::as_object)
        {
            for name in map.keys() {
                push_input_type_option(&mut options, name);
            }
        }
    }
    options
}

fn push_input_type_option(options: &mut Vec<String>, name: &str) {
    if !name.is_empty() && !options.iter().any(|existing| existing == name) {
        options.push(name.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gear::processor::key_binder::KeyBinder;
    use serde_json::json;

    /// 空引擎 + 给定配置树 + 一个真的 `key_binder`（切换动作必须经过处理器链，
    /// 才能覆盖 `process_key` 里"按键处理期间 option 变了"的检测点）。
    fn engine_with_key_binder(config: serde_json::Value) -> Engine {
        let mut engine = Engine::empty_for_test();
        engine.schema = Schema::from_config_for_test(config);
        engine.input_type_options = collect_input_type_options(&engine.schema.config, None);
        let binder = KeyBinder::new(&engine.schema.config);
        engine.processors.push(ComponentEntry::new(
            Box::new(binder) as Box<dyn Processor + Send + Sync>,
            "key_binder",
        ));
        engine
    }

    fn multi_input_config() -> serde_json::Value {
        json!({
            "translator": {
                "prisms": [
                    { "name": "pinyin.multi.ziranma", "option": "ziranma" },
                    { "name": "pinyin.multi.flypy", "input_type": "flypy" }
                ],
                "input_types": { "full": "pinyin.multi" }
            },
            "key_binder": {
                "bindings": [
                    { "when": "always", "accept": "Control+period", "toggle": "ziranma" }
                ]
            }
        })
    }

    #[test]
    fn collect_input_type_options_reads_all_declaration_forms() {
        let config = json!({
            "translator": {
                "prisms": [
                    { "name": "a", "option": "ziranma" },
                    { "name": "b", "input_type": "flypy" }
                ],
                "input_types": { "full": "pinyin.multi" },
                "simp": { "prisms": { "ziranma": "a" } }
            }
        });
        assert_eq!(
            collect_input_type_options(&config, None),
            vec!["ziranma", "flypy", "full"]
        );
    }

    #[test]
    fn set_option_reacts_via_signal_queue_changed_gates_clear() {
        // 输入：组字中（input="nihao"）+ `ziranma` 是输入类型开关。
        // 期望：首次 set_option(true)（镜像 false → true，changed）⇒ 信号槽反应清空组字；
        //       再设同值（镜像已是 true，changed=false）⇒ 组字保留 —— changed 由信号槽
        //       用镜像推算（Qt 槽"旧值比对新值"），set_option 本体不再直接调 clear。
        let mut engine = Engine::empty_for_test();
        engine.input_type_options = vec!["ziranma".into()];
        engine.context.input = "nihao".into();

        engine.set_option("ziranma", true);
        assert_eq!(
            engine.context.input, "",
            "changed 的输入类型开关应经信号反应清空组字（nihao → 空）"
        );
        assert_eq!(
            engine.signal_mirror_get("ziranma"),
            Some(true),
            "镜像应记录信号槽已反应过的值"
        );

        engine.context.input = "nihao".into();
        engine.set_option("ziranma", true);
        assert_eq!(engine.context.input, "nihao", "同值重复设置（changed=false）不应清空组字");
    }

    #[test]
    fn set_option_signals_do_not_cross_engines() {
        // 两个引擎都挂在全局总线上：A 的 set_option 只进 A 自己的镜像，
        // B 不得收到（session_id 过滤 = Qt 对象亲和隔离）。
        let mut a = Engine::empty_for_test();
        let mut b = Engine::empty_for_test();
        a.set_option("ascii_mode", true);
        assert_eq!(
            a.signal_mirror_get("ascii_mode"),
            Some(true),
            "A 应消费自己发出的 OptionChanged"
        );
        assert_eq!(
            b.signal_mirror_get("ascii_mode"),
            None,
            "B 不应收到 A 的信号"
        );
        b.set_option("ascii_mode", true);
        assert_eq!(b.signal_mirror_get("ascii_mode"), Some(true), "B 自己设开关才轮到 B 的镜像");
    }

    #[test]
    fn collect_input_type_options_falls_back_to_prism_index() {
        let build =
            std::env::temp_dir().join(format!("librime-input-types-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&build);
        let dir = crate::gear::dict::prism_index::dict_dir(&build, "pinyin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pinyin.zrm.prism.bin"), "x").unwrap();
        crate::gear::dict::prism_index::write_index(&build, "pinyin", &["zrm".to_string()])
            .expect("write index");

        // 完全没有手写声明 → 用索引里的组名。
        let bare = json!({"translator": {"dict": "pinyin"}});
        assert_eq!(
            collect_input_type_options(&bare, Some(&build)),
            vec!["zrm"]
        );
        // 有手写声明 → 只按写的来，索引不参与。
        let explicit = json!({
            "translator": {
                "dict": "pinyin",
                "prisms": [{"name": "pinyin.flypy", "option": "flypy"}]
            }
        });
        assert_eq!(
            collect_input_type_options(&explicit, Some(&build)),
            vec!["flypy"]
        );
        let _ = std::fs::remove_dir_all(&build);
    }

    #[test]
    fn collect_input_type_options_honours_input_types_enabled_and_order() {
        let config = json!({
            "translator": {"dict": "pinyin"},
            "input_types": {
                "enabled": ["zrm", "flypy"],
                "reset": "zrm",
                "options": [
                    {"option": "pinyin", "label": "全拼"},
                    {"option": "flypy", "label": "小鹤双拼"},
                    {"option": "zrm", "label": "自然码"},
                    {"option": "sogou", "label": "搜狗双拼"}
                ]
            }
        });
        // 集合由 `enabled` 决定，顺序由 `options` 决定；未启用的 pinyin/sogou 不出现。
        assert_eq!(
            collect_input_type_options(&config, None),
            vec!["flypy", "zrm"]
        );
        // `enabled` 里有、`options` 没提到的名字按字母序补在后面。
        let mut config2 = config.clone();
        config2["input_types"]["enabled"] = json!(["zrm", "terra"]);
        assert_eq!(
            collect_input_type_options(&config2, None),
            vec!["zrm", "terra"]
        );
        // 不写 `enabled`：集合回退到部署索引，顺序仍按 `options`。
        let build = std::env::temp_dir().join(format!(
            "librime-input-types-order-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&build);
        let dir = crate::gear::dict::prism_index::dict_dir(&build, "pinyin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pinyin.zrm.prism.bin"), "x").unwrap();
        crate::gear::dict::prism_index::write_index(&build, "pinyin", &["zrm".to_string()])
            .expect("write index");
        let mut config3 = config.clone();
        config3["input_types"]
            .as_object_mut()
            .unwrap()
            .remove("enabled");
        assert_eq!(
            collect_input_type_options(&config3, Some(&build)),
            vec!["zrm"]
        );
        let _ = std::fs::remove_dir_all(&build);
    }

    #[test]
    fn switching_input_type_clears_composition() {
        let mut engine = engine_with_key_binder(multi_input_config());
        assert_eq!(
            engine.input_type_options,
            vec!["ziranma", "flypy", "full"],
            "输入类型开关应从 translator 声明里收集"
        );
        engine.context.push_input_ch('n');
        engine.context.push_input_ch('i');
        assert!(engine.context.is_composing());

        let key = KeyEvent::parse("Control+period").unwrap();
        assert!(engine.process_key(&key), "绑定应被 key_binder 接受");

        assert!(engine.context.get_option("ziranma"), "开关应已打开");
        assert_eq!(engine.context.input, "", "换输入类型后组字应被清空");
        assert!(!engine.context.is_composing(), "组字状态应回到空闲");
    }

    #[test]
    fn switching_unrelated_option_keeps_composition() {
        // 没有 translator/prisms ⇒ 输入类型清单为空 ⇒ 老行为（不清空）。
        let config = json!({
            "switches": [{ "name": "full_shape" }],
            "key_binder": {
                "bindings": [
                    { "when": "always", "accept": "Control+period", "toggle": "full_shape" }
                ]
            }
        });
        let mut engine = engine_with_key_binder(config);
        assert!(engine.input_type_options.is_empty());
        engine.context.push_input_ch('n');
        engine.context.push_input_ch('i');

        let key = KeyEvent::parse("Control+period").unwrap();
        assert!(engine.process_key(&key));

        assert!(engine.context.get_option("full_shape"));
        assert_eq!(engine.context.input, "ni", "非输入类型开关不应动组字");
        assert!(engine.context.is_composing());
    }

    #[test]
    fn set_option_on_input_type_switch_clears_composition() {
        let mut engine = engine_with_key_binder(multi_input_config());
        engine.context.push_input_ch('n');
        engine.context.push_input_ch('i');
        assert!(engine.context.is_composing());

        // TIP / C API 走的是 `Engine::set_option`，语义必须与按键切换一致。
        engine.set_option("ziranma", true);
        assert_eq!(engine.context.input, "");
        assert!(!engine.context.is_composing());
    }

    #[test]
    fn select_input_type_switches_exclusively() {
        let mut engine = engine_with_key_binder(multi_input_config());
        // 先同时打开两个输入类型开关（模拟越权/重复切换留下的脏状态）。
        engine.set_option("ziranma", true);
        engine.set_option("full", true);
        assert!(engine.context.get_option("ziranma"));
        assert!(engine.context.get_option("full"));

        // 切到 flypy：必须互斥 —— 目标开、其余全部关。
        engine.select_input_type("flypy");
        assert!(engine.context.get_option("flypy"), "目标类型应打开");
        assert!(!engine.context.get_option("ziranma"), "其余类型必须互斥关闭");
        assert!(!engine.context.get_option("full"), "其余类型必须互斥关闭");

        // 越权类型名：忽略，不改变任何开关状态。
        let before = engine.context.get_option("flypy");
        engine.select_input_type("nonexistent");
        assert_eq!(
            engine.context.get_option("flypy"),
            before,
            "越权类型名不应改变状态"
        );
    }

    /// 2026-11-09：总开关关掉后联想条必须是空表 —— 神经结果和 n-gram 回退都不许出。
    #[test]
    fn predictions_are_empty_when_switch_off() {
        let context = crate::engine::context::Context::new();
        let neural = vec![("你好".to_string(), -0.1), ("世界".to_string(), -0.2)];
        assert!(
            resolve_predictions(false, neural, &context).is_empty(),
            "WEASEL_PREDICT=off 时不得下发任何联想词"
        );
    }

    /// 开关开着时神经结果原样取前 4 条（别把这条口径改坏）。
    #[test]
    fn predictions_take_neural_when_switch_on() {
        let context = crate::engine::context::Context::new();
        let neural: Vec<(String, f64)> = (0..6)
            .map(|i| (format!("词{i}"), -(i as f64)))
            .collect();
        let out = resolve_predictions(true, neural, &context);
        assert_eq!(out, vec!["词0", "词1", "词2", "词3"]);
    }
}
