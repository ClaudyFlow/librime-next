//! `src/rime/switcher.{h,cc}`：方案 / 开关菜单（`F4`、`Control+grave`）。
//!
//! 装 `F4` 或 `` Control+` `` 弹出的那个菜单：上面是方案列表，下面是当前方案的开关
//! （中/西文、简/繁、半角/全角……）。选中一条就执行它的动作，菜单收起。
//!
//! ## C++ 的双身份在 Rust 里怎么落
//!
//! C++ 的 `Switcher` 同时继承 `Processor` 和 `Engine`：
//!
//! - 作为 `Processor`，它被塞进主引擎 `processors_` 的**第一个**（`engine.cc:565-570`），
//!   所以能最先看到按键；
//! - 作为 `Engine`，它有自己的一套 `context_` / `schema_` / `processors_` / `translators_`。
//!
//! 激活时 `engine_->set_active_engine(this)`，然后宿主的 `Session::context()`
//! 返回 `engine_->active_engine()->context()`（`service.cc:60`）—— **切到 switcher
//! 自己的 context**，于是 UI 看到的就变成菜单。
//!
//! Rust 没有多继承，也不需要照抄这套双 context：
//!
//! - `Switcher` 只实现 [`Processor`]，持一份 `default.yaml` 的配置树（C++ 里是
//!   `Switcher` 那个 Engine 的 `.default` schema）；
//! - **菜单直接写进主引擎的 `Context`**。所以不需要 `active_engine` 转发，宿主的
//!   `Session::context()` 本来就读到它。
//!
//! 代价是激活时得给主引擎的 context 打 `dumb`（菜单项不该真上屏），退出时清掉 ——
//! C++ 因为 context 是独立的，这个开关一直挂在 switcher 上不受影响。
//!
//! ## 为什么方案切换要"登记"而不是直接改
//!
//! `Engine::run_processor_chain` 把 `processors` 列表 `mem::take` 到局部再遍历，
//! switcher 自己就在那个列表里。如果它直接调 `engine.apply_schema()`，重建会写到
//! `self.processors`，但遍历结束时又会被那份旧的局部列表覆盖 —— 切换静默失效。
//!
//! 所以 `Switcher` 只调 [`Engine::request_schema`] 登记，由 `Engine::process_key`
//! 在处理器链跑完、列表归位之后统一执行。C++ 是同步递归调用，结果一致。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::engine::candidate::Candidate;
use crate::engine::context::Context;
use crate::engine::gear::processor::{ProcessResult, Processor};
use crate::engine::gear::schema_list_translator;
use crate::engine::gear::switch_translator;
use crate::engine::key_event::{
    KeyEvent, XK_DOWN, XK_ESCAPE, XK_KP_ENTER, XK_LEFT, XK_NEXT, XK_PRIOR, XK_RETURN, XK_RIGHT,
    XK_SPACE, XK_UP,
};
use crate::engine::menu::Menu;
use crate::engine::runtime::Engine;
use crate::engine::segmentation::{SegStatus, Segment};
use crate::lever::tree;

/// 菜单最上面两条之间的动作。
///
/// C++ 里这些是 `SwitcherCommand` 的各个子类（`SchemaSelection` / `Switch` /
/// `RadioOption` / `FoldedOptions` / `SchemaAction`），每个都是"候选 + 动作"的
/// 多重继承对象。LRN 的候选是纯数据，所以动作单独放在这个枚举里。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwitcherCommand {
    /// 切到某个方案（`SchemaSelection::Apply`）。
    Schema(String),
    /// 切到某个方案，但**当前方案**那条在折叠状态下是"先展开开关"
    /// （C++ `SchemaAction`：`ShadowCandidate` 把类型改成 `unfold` 并转发给展开命令）。
    /// 这个变体只在 `_fold_options` 打开时出现在第一条上。
    SchemaWithUnfold(String),
    /// 翻转一个开关（`Switch::Apply`）。
    Toggle {
        option: String,
        target: bool,
        auto_save: bool,
    },
    /// 单选组里选中某一项（`RadioOption::Apply` → `RadioGroup::SelectOption`）。
    Radio { group: Vec<String>, option: String },
    /// 展开被折叠的开关列表（`FoldedOptions::Apply`）—— 菜单**不关**，就地重刷。
    UnfoldOptions,
}

/// 菜单里的一条：候选（给宿主看）+ 动作（选中后执行）。
#[derive(Clone, Debug)]
pub struct SwitcherItem {
    pub candidate: Candidate,
    pub command: SwitcherCommand,
}

// ── 与 Switcher 实例无关的入口（引擎构造 / 切换方案时要用）──────────────

/// 找方案菜单要用的 `default.yaml`：**部署产物优先，其次数据根（用户目录 → 共享目录）**。
///
/// ① `build/default.yaml` = **部署产物**。部署时 `WorkspaceUpdate` 会把
///    `schema/schema.yaml` 的勾选清单原样写进 `staging/default.yaml`
///    （见 `lever/deployment_tasks.rs` 第 5 步），所以这份才是"用户在设置里勾了什么"。
///    C++ `Switcher` 读的也正是它（`Config::Require("config")->Create("default")`
///    解析到部署目录，不是用户的源文件）。
/// ② 数据根里的源文件 `<用户>\default.yaml` / `<共享>\default.yaml` 只在**还没部署过**时兜底。
///
/// ⚠️ 2026-10-29 修：原来只查 ②，于是菜单永远读用户的**源** `default.yaml`。真实故障是
/// 用户那份还是 10-27 的旧 id（`luna_pinyin_simp` / `pinyin` / `wubi86`），而 8 个新方案
/// （`cangjie.5`、`pinyin.terra`…）一个都不在菜单里 —— 表现就是"设置里勾了 8 个，
/// 输入法里只有两项能切"，且切过去的旧 id 早已没有 translator，候选恒为 0。
pub fn load_default_config(build_dir: &Path, data_roots: &[PathBuf]) -> Value {
    let deployed = build_dir.join("default.yaml");
    if deployed.is_file()
        && let Ok(config) = crate::load_file(&deployed)
    {
        return config;
    }
    for root in data_roots {
        let path = root.join("default.yaml");
        if path.is_file()
            && let Ok(config) = crate::load_file(&path)
        {
            return config;
        }
    }
    Value::Object(serde_json::Map::new())
}

/// C++ `Switcher::SetActiveSchema` 的落盘部分（不依赖 `Switcher` 实例）。
///
/// 抽成自由函数是为了让 [`Engine::apply_schema`] 也能调 —— `Switcher` 在引擎里是
/// `Box<dyn Processor>`，引擎拿不到具体类型。C++ 那边是 `switcher_->SetActiveSchema()`，
/// 因为 `switcher_` 是强类型的成员，不存在这个问题。
pub fn record_active_schema(user_dir: Option<&Path>, schema_id: &str) {
    let Some(dir) = user_dir else {
        return;
    };
    let path = dir.join("user.yaml");
    let mut config = if path.is_file() {
        tree::load(&path).unwrap_or_else(|| Value::Object(serde_json::Map::new()))
    } else {
        Value::Object(serde_json::Map::new())
    };
    if !tree::set_scalar(&mut config, "var/previously_selected_schema", schema_id) {
        return;
    }
    tree::set_scalar(
        &mut config,
        &format!("var/schema_access_time/{schema_id}"),
        &tree::now().to_string(),
    );
    tree::save(&config, &path);
}

/// 读 `var/previously_selected_schema`（引擎建不起来时要靠它挑方案）。
pub fn load_previous_schema(user_dir: Option<&Path>) -> Option<String> {
    let path = user_dir?.join("user.yaml");
    if !path.is_file() {
        return None;
    }
    let config = tree::load(&path)?;
    tree::get_string(&config, "var/previously_selected_schema")
}

/// 方案 / 开关菜单（对照 C++ `Switcher`）。
pub struct Switcher {
    /// `default.yaml` 的配置树 —— 方案清单（`schema_list`）、菜单设置
    /// （`switcher/*`）都在这里。C++ 走 `Schema()` 的默认构造（`schema_id = ".default"`
    /// → `Config::Create("default")`），是同一份东西。
    config: Value,
    /// `switcher/caption`：菜单提示语，缺省 `:-)`（C++ `LoadSettings`）。
    caption: String,
    /// `switcher/hotkeys`：唤起键。
    hotkeys: Vec<KeyEvent>,
    /// `switcher/save_options`：这些开关的状态要写进 `user.yaml`。
    save_options: HashSet<String>,
    /// `switcher/fold_options`：一进来就折叠开关列表。
    fold_options: bool,
    /// `switcher/fix_schema_list_order`：方案按配置顺序排，不按最近使用。
    fix_schema_list_order: bool,
    /// 菜单当前是否展开。
    active: bool,
    /// 当前菜单的条目（与 `Segment::menu` 里的候选**同序**，靠下标对应）。
    items: Vec<SwitcherItem>,
    /// 用户目录（写 `user.yaml` 用）。`None` = 没有可写位置（测试沙箱）。
    user_dir: Option<PathBuf>,
    /// `build/` 目录 —— 构造方案候选时要读 `<id>.schema.yaml` 拿显示名。
    build_dir: PathBuf,
    /// `menu/page_size`（来自 `default.yaml`），翻页步长。
    page_size: usize,
}

impl Switcher {
    /// C++ `Switcher::Switcher(ticket)` + `LoadSettings()`。
    ///
    /// `default_config` 是 `default.yaml` 的树；`user_dir` 是写 `user.yaml` 的位置。
    pub fn new(default_config: Value, build_dir: &Path, user_dir: Option<PathBuf>) -> Self {
        let string_at = |path: &str| {
            default_config
                .pointer(path)
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let bool_at = |path: &str| {
            default_config
                .pointer(path)
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };

        let caption = string_at("/switcher/caption")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ":-)".to_string());

        // `switcher/hotkeys`：每项是一个键串（`F4`、`Control+grave`）
        let hotkeys = default_config
            .pointer("/switcher/hotkeys")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .filter_map(KeyEvent::parse)
                    .collect()
            })
            .unwrap_or_default();

        let save_options = default_config
            .pointer("/switcher/save_options")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        let page_size = default_config
            .pointer("/menu/page_size")
            .and_then(Value::as_i64)
            .filter(|n| *n > 0)
            .map_or(5, |n| n as usize);

        Switcher {
            fold_options: bool_at("/switcher/fold_options"),
            fix_schema_list_order: bool_at("/switcher/fix_schema_list_order"),
            config: default_config,
            caption,
            hotkeys,
            save_options,
            active: false,
            items: Vec::new(),
            user_dir,
            build_dir: build_dir.to_path_buf(),
            page_size,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// 菜单条目（宿主 / 测试用）。
    pub fn items(&self) -> &[SwitcherItem] {
        &self.items
    }

    fn is_hotkey(&self, key: &KeyEvent) -> bool {
        // C++ 比较整个 `KeyEvent`（键码 + 修饰位）。按住不放会重复触发（`key.repeat()`），
        // 此时菜单已展开 ⇒ 走"下一个方案"分支。
        self.hotkeys.iter().any(|hotkey| hotkey == key)
    }

    // ── 用户配置（`user.yaml`）──────────────────────────────────────────

    /// 读 `user.yaml`（不存在则空树）。
    fn user_config(&self) -> Value {
        self.user_dir
            .as_ref()
            .map(|dir| dir.join("user.yaml"))
            .filter(|path| path.is_file())
            .and_then(|path| tree::load(&path))
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()))
    }

    fn save_user_config(&self, config: &Value) {
        let Some(dir) = &self.user_dir else {
            return;
        };
        tree::save(config, &dir.join("user.yaml"));
    }

    /// `var/schema_access_time/<id>`：每个方案最近一次被选中的时间戳。
    fn load_access_times(&self) -> HashMap<String, i64> {
        let mut times = HashMap::new();
        let config = self.user_config();
        if let Some(map) = config
            .pointer("/var/schema_access_time")
            .and_then(Value::as_object)
        {
            for (schema_id, value) in map {
                if let Some(timestamp) = value.as_i64() {
                    times.insert(schema_id.clone(), timestamp);
                }
            }
        }
        times
    }

    /// C++ `Switcher::SetActiveSchema`：记下"上次选的方案"和访问时间，并落盘。
    ///
    /// 引擎切方案成功后由 [`Engine::apply_schema`] 调用，不在菜单命令里做 ——
    /// 命令只负责登记切换请求，真正生效时才知道成没成。
    pub fn set_active_schema(&self, schema_id: &str) {
        record_active_schema(self.user_dir.as_deref(), schema_id);
    }

    /// C++ `Switch::Apply` / `RadioGroup::SelectOption` 里写 `var/option/<name>` 的部分。
    fn save_option(&self, option: &str, value: bool) {
        let mut config = self.user_config();
        if !tree::set_scalar(
            &mut config,
            &format!("var/option/{option}"),
            if value { "true" } else { "false" },
        ) {
            return;
        }
        self.save_user_config(&config);
    }

    /// 建菜单时把单选组的同步结果落盘（C++ 在 `SelectOption` 里就地写）。
    fn flush_option_writes(&self, writes: &[(String, bool)]) {
        if writes.is_empty() {
            return;
        }
        let mut config = self.user_config();
        for (option, value) in writes {
            tree::set_scalar(
                &mut config,
                &format!("var/option/{option}"),
                if *value { "true" } else { "false" },
            );
        }
        self.save_user_config(&config);
    }

    /// C++ `Switcher::RestoreSavedOptions`：把上次保存的开关状态读回来。
    ///
    /// 引擎**每次新建**都要调一次（C++ 在 `ConcreteEngine` 构造里，
    /// `engine.cc:156`），否则用户上次设的"简繁/全半角"重启就丢。
    pub fn restore_saved_options(&self, context: &mut Context) {
        if self.save_options.is_empty() {
            return;
        }
        let config = self.user_config();
        for option in &self.save_options {
            if let Some(value) = tree::get_bool(&config, &format!("var/option/{option}")) {
                context.set_option(option, value);
            }
        }
    }

    // ── 菜单 ───────────────────────────────────────────────────────────

    /// C++ `Switcher::SelectNextSchema`：方案菜单的**第二条**（跳过当前方案）。
    ///
    /// 注意 C++ 这里只装 `schema_list_translator`（`switcher.cc:210-214`），
    /// **不含开关** —— 所以单独算，不复用 [`Self::build_items`]。
    ///
    /// `.next` 是 `key_binder` 的 `select: .next` 用的：用户绑了 `Control+grave`
    /// 之类"循环切方案"的键，就靠它。
    pub fn next_schema(&self, current_schema_id: &str) -> Option<String> {
        let access_times = self.load_access_times();
        let items = schema_list_translator::build_items(
            current_schema_id,
            &self.config,
            &self.build_dir,
            &access_times,
            self.fix_schema_list_order,
        );
        // 第 0 条永远是当前方案，第 1 条才是"下一个"
        items.get(1).and_then(|item| match &item.command {
            SwitcherCommand::Schema(schema_id) => Some(schema_id.clone()),
            _ => None,
        })
    }

    /// 建菜单条目（C++ `RefreshMenu` 里两个 translator 的 `Query`）。
    fn build_items(&self, engine: &mut Engine) -> Vec<SwitcherItem> {
        let access_times = self.load_access_times();
        let folded = engine.context.get_option("_fold_options");

        // ① 方案部分
        let mut items = schema_list_translator::build_items(
            &engine.schema.schema_id,
            &self.config,
            &self.build_dir,
            &access_times,
            self.fix_schema_list_order,
        );
        // 折叠状态下，"当前方案"那条要顺带把开关列表展开（C++ `SchemaAction`）
        if folded
            && let Some(first) = items.first_mut()
            && let SwitcherCommand::Schema(id) = &first.command
        {
            first.command = SwitcherCommand::SchemaWithUnfold(id.clone());
        }

        // ② 开关部分
        let mut writes = Vec::new();
        let switch_items = switch_translator::build_items(
            &engine.schema.config,
            &self.config,
            &mut engine.context,
            &self.save_options,
            &mut writes,
        );
        self.flush_option_writes(&writes);
        items.extend(switch_items);
        items
    }

    /// C++ `Switcher::RefreshMenu`：重建菜单段与候选。
    fn refresh_menu(&mut self, engine: &mut Engine) {
        let segments = &mut engine.context.composition.segmentation.segments;
        if segments.is_empty() {
            let mut segment = Segment::new(0, 0);
            segment.prompt = self.caption.clone();
            segments.push(segment);
        }
        self.items = self.build_items(engine);

        let mut menu = Menu::new();
        for item in &self.items {
            // 坐标 0..0：这是个"空段"，候选只是菜单条目，不覆盖任何输入
            menu.add_translation(Box::new(crate::engine::gear::translation::VecTranslation::new(vec![item.candidate.clone()])));
        }
        // ⚠️ 刻意**不**调 `Menu::prepare()`：它会按 quality 排序，而这里的顺序是
        // 建菜单时算好的（方案在前、开关在后，各自内部有序）。`quality` 只作为
        // "万一别处排序"的保险。
        if let Some(segment) = engine.context.composition.segmentation.segments.last_mut() {
            segment.menu = Some(menu);
            segment.status = SegStatus::Guess;
            segment.selected_index = 0;
        }
    }

    /// C++ `Switcher::Activate`。
    fn activate(&mut self, engine: &mut Engine) {
        // 菜单项不该真上屏（C++ 是 switcher 自己的 context 常驻 `dumb`；
        // LRN 共用主引擎 context，所以进出各切一次）
        engine.context.set_option("dumb", true);
        engine.context.set_option("_fold_options", self.fold_options);
        // 进来时把主引擎残留的组合清掉，免得菜单叠在没打完的输入上
        engine.context.clear();
        engine.context.composition.segmentation.segments.clear();
        self.active = true;
        self.refresh_menu(engine);
    }

    /// C++ `Switcher::Deactivate`。
    fn deactivate(&mut self, engine: &mut Engine) {
        self.active = false;
        engine.context.set_option("dumb", false);
        self.items.clear();
        engine.context.clear();
        engine.context.composition.segmentation.segments.clear();
    }

    /// 当前选中的条目下标（`Segment::selected_index`）。
    fn selected_index(&self, engine: &Engine) -> usize {
        engine
            .context
            .composition
            .segmentation
            .segments
            .last()
            .map_or(0, |segment| segment.selected_index)
    }

    fn set_selected_index(&self, engine: &mut Engine, index: usize) {
        if let Some(segment) = engine.context.composition.segmentation.segments.last_mut() {
            segment.selected_index = index;
        }
    }

    /// 上下移动选中项。C++ 在 switcher 里装了 `key_binder` + `navigator`，
    /// 这里内置同样的行为（`menu/page_down_cycle` 默认 false ⇒ 到边界就停）。
    fn move_selection(&mut self, engine: &mut Engine, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let current = self.selected_index(engine) as isize;
        let last = self.items.len() as isize - 1;
        let next = (current + delta).clamp(0, last) as usize;
        self.set_selected_index(engine, next);
    }

    /// C++ `Switcher::HighlightNextSchema`：热键再按一次 → 跳到下一个**方案**候选。
    fn highlight_next_schema(&mut self, engine: &mut Engine) {
        if self.items.is_empty() {
            return;
        }
        let current = self.selected_index(engine);
        let total = self.items.len();
        let mut index = current;
        for _ in 0..total {
            index = (index + 1) % total;
            if self.items[index].candidate.kind == "schema" {
                self.set_selected_index(engine, index);
                return;
            }
        }
    }

    /// 执行一条命令。
    ///
    /// 两种命令**不关菜单**（C++ 里它们的 `Apply` 只是重刷）：
    /// 展开折叠的开关、以及折叠状态下选中"当前方案"。
    fn apply_command(&mut self, engine: &mut Engine, command: SwitcherCommand) {
        match command {
            SwitcherCommand::UnfoldOptions | SwitcherCommand::SchemaWithUnfold(_) => {
                engine.context.set_option("_fold_options", false);
                self.refresh_menu(engine);
            }
            SwitcherCommand::Schema(schema_id) => {
                self.deactivate(engine);
                // 登记，不直接切：处理器链跑完后由 `Engine::process_key` 执行
                engine.request_schema(schema_id);
            }
            SwitcherCommand::Toggle {
                option,
                target,
                auto_save,
            } => {
                self.deactivate(engine);
                engine.context.set_option(&option, target);
                if auto_save {
                    self.save_option(&option, target);
                }
            }
            SwitcherCommand::Radio { group, option } => {
                self.deactivate(engine);
                for name in &group {
                    let selected = *name == option;
                    if engine.context.get_option(name) != selected {
                        engine.context.set_option(name, selected);
                        if self.save_options.contains(name) {
                            self.save_option(name, selected);
                        }
                    }
                }
            }
        }
    }

    /// 确认当前选中项。
    fn confirm(&mut self, engine: &mut Engine) {
        let index = self.selected_index(engine);
        let Some(command) = self.items.get(index).map(|item| item.command.clone()) else {
            self.deactivate(engine);
            return;
        };
        self.apply_command(engine, command);
    }
}

impl Processor for Switcher {
    fn process_key_event(&mut self, engine: &mut Engine, key: &KeyEvent) -> ProcessResult {
        // ① 热键：没开就开，开了就跳到下一个方案（C++ `ProcessKeyEvent` 开头那段）
        if self.is_hotkey(key) {
            if !self.active {
                self.activate(engine);
            } else {
                self.highlight_next_schema(engine);
            }
            return ProcessResult::Accepted;
        }

        if !self.active {
            return ProcessResult::Noop;
        }

        // ② 导航（C++ 交给 switcher 自己的 key_binder / navigator，这里内置）
        match key.keycode {
            XK_UP | XK_LEFT => {
                self.move_selection(engine, -1);
                return ProcessResult::Accepted;
            }
            XK_DOWN | XK_RIGHT => {
                self.move_selection(engine, 1);
                return ProcessResult::Accepted;
            }
            XK_PRIOR => {
                let step = self.page_size as isize;
                self.move_selection(engine, -step);
                return ProcessResult::Accepted;
            }
            XK_NEXT => {
                let step = self.page_size as isize;
                self.move_selection(engine, step);
                return ProcessResult::Accepted;
            }
            _ => {}
        }

        // ③ 数字直选（C++ 用 selector）
        if let Some(ch) = key.printable_char()
            && ('1'..='9').contains(&ch)
        {
            let index = ch as usize - '1' as usize;
            if index < self.items.len() {
                self.set_selected_index(engine, index);
                self.confirm(engine);
            }
            return ProcessResult::Accepted;
        }

        // ④ 松开 / 带修饰的键：吞掉，不做事（C++ 同样 `return kAccepted`）
        if key.release() || key.ctrl() || key.alt() {
            return ProcessResult::Accepted;
        }

        // ⑤ 确认 / 退出
        match key.keycode {
            XK_SPACE | XK_RETURN | XK_KP_ENTER => self.confirm(engine),
            XK_ESCAPE => self.deactivate(engine),
            // 其余键一律吞掉 —— 菜单展开时不该漏到输入区
            _ => {}
        }
        ProcessResult::Accepted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn switcher() -> Switcher {
        Switcher::new(
            json!({
                "switcher": {
                    "caption": "〔方案選單〕",
                    "hotkeys": ["F4", "Control+grave"],
                    "save_options": ["full_shape"]
                },
                "schema_list": [{ "schema": "luna_pinyin" }]
            }),
            Path::new("."),
            None,
        )
    }

    #[test]
    fn reads_settings_from_default_config() {
        let s = switcher();
        assert_eq!(s.caption, "〔方案選單〕");
        assert_eq!(s.hotkeys.len(), 2);
        assert!(s.hotkeys.iter().any(|k| k.keycode == 0xffc1 || k.repr().contains("F4")));
        assert!(s.save_options.contains("full_shape"));
        assert!(!s.is_active());
    }

    #[test]
    fn default_caption_and_page_size() {
        let s = Switcher::new(json!({}), Path::new("."), None);
        assert_eq!(s.caption, ":-)", "C++ 的默认提示语");
        assert_eq!(s.page_size, 5);
        assert!(s.hotkeys.is_empty());
    }

    #[test]
    fn restore_reads_saved_options_from_user_yaml() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "lrn_switcher_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("user.yaml"),
            "var:\n  option:\n    full_shape: true\n    unrelated: true\n",
        )
        .unwrap();

        let s = Switcher::new(
            json!({ "switcher": { "save_options": ["full_shape"] } }),
            Path::new("."),
            Some(dir.clone()),
        );
        let mut ctx = Context::new();
        s.restore_saved_options(&mut ctx);
        assert!(ctx.get_option("full_shape"), "存过的开关要读回来");
        assert!(
            !ctx.get_option("unrelated"),
            "不在 save_options 里的不碰"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 方案菜单必须读**部署产物** `build/default.yaml`（= `schema/schema.yaml` 的勾选清单），
    /// 不能被用户目录里过期的源 `default.yaml` 抢走。
    ///
    /// 回归自 2026-10-29 真机故障：用户设置里勾了 8 个方案，
    /// `<用户>\build\default.yaml` 也是 8 个，但菜单只显示旧源文件里的 4 个
    /// （其中 3 个 id 已废弃、候选恒为 0），表现为"只有两项能用"。
    #[test]
    fn deployed_default_yaml_wins_over_stale_user_source() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "lrn_switcher_deployed_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let user = root.join("user");
        let build = user.join("build");
        std::fs::create_dir_all(&build).unwrap();
        // 部署产物：8 项里取两项代表（顺序 = 菜单顺序）
        std::fs::write(
            build.join("default.yaml"),
            "schema_list:\n  - schema: pinyin.luna\n  - schema: double_pinyin.flypy\n",
        )
        .unwrap();
        // 用户源文件：过期 id + 只剩一项
        std::fs::write(
            user.join("default.yaml"),
            "schema_list:\n  - schema: luna_pinyin_simp\n  - schema: pinyin\n",
        )
        .unwrap();

        let config = load_default_config(&build, &[user.clone()]);
        let ids = schema_list_translator::for_each_schema_list_entry(&config, |id| {
            assert_ne!(id, "luna_pinyin_simp", "过期源文件的清单不该被读到");
            true
        });
        assert_eq!(ids, 2);
        let first = config
            .pointer("/schema_list/0/schema")
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(first, "pinyin.luna");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 没部署过（`build/default.yaml` 不存在）时才回退到数据根的源文件，且用户目录优先。
    #[test]
    fn falls_back_to_data_roots_before_first_deploy() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "lrn_switcher_fallback_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let user = root.join("user");
        let shared = root.join("shared");
        let build = user.join("build");
        std::fs::create_dir_all(&build).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(user.join("default.yaml"), "schema_list:\n  - schema: stroke\n").unwrap();
        std::fs::write(shared.join("default.yaml"), "schema_list:\n  - schema: zhuyin\n").unwrap();

        let config = load_default_config(&build, &[user, shared]);
        let first = config
            .pointer("/schema_list/0/schema")
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(first, "stroke", "用户目录优先于共享目录");
        let _ = std::fs::remove_dir_all(&root);
    }
}
