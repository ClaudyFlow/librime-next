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
use crate::gear::processor::{ProcessResult, Processor};
use crate::gear::schema::list_translator;
use crate::gear::translator::switch_translator;
use crate::engine::key_event::{
    KeyEvent, K_REPEAT_MASK, XK_BACKSPACE, XK_DOWN, XK_ESCAPE, XK_KP_ENTER,
    XK_LEFT, XK_NEXT, XK_PRIOR, XK_RETURN, XK_RIGHT, XK_SPACE, XK_TAB, XK_UP,
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
    /// 进入一个"方案 → 子方案"分组的第二级（父方案 id）。
    /// 菜单**不关**，就地重刷；第二级里再选一次才切方案。
    OpenGroup(String),
    /// 切换主方案的某个**输入类型**（单方案多输入类型，如 pinyin 的明月/地球/注音…）。
    /// 与 [`SwitcherCommand::Schema`] 不同：方案 id 不变（仍是主方案，主方案始终
    /// active），只切一个 context 开关，且各输入类型**互斥单选**。菜单第二级由
    /// 主方案的 `input_types.options` 生成这些条目（Plan A：取代原来的子方案文件夹）。
    InputType(String),
    /// 展开被折叠的开关列表（`FoldedOptions::Apply`）—— 菜单**不关**，就地重刷。
    UnfoldOptions,
    /// 第二级末尾那条可见的「返回上一级」：与 Esc / 退格 / Tab 等价，
    /// 但让只用方向键+空格的用户也能退回去（不必知道隐藏快捷键）。
    Back,
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
        match tree::load(&path) {
            Some(value) => value,
            // ⚠️ 2026-11-11：文件在却解析不出来（历史遗留重复 weasel_next 键），
            // 绝不能像以前那样用空树重写——那会把整段 SHA256 哈希冲掉。
            None => {
                eprintln!("[switcher] user.yaml 解析失败，跳过写回以保护 weasel_next 哈希段");
                return;
            }
        }
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
    save_user_yaml_preserving_manifest(&config, &path);
}

/// 保存 `user.yaml` 时保护 `weasel_next` 哈希段。
///
/// `tree::save` 会重写整个文件并丢掉 YAML 注释标记；而 `deploy_manifest::load`
/// 正是靠标记块找哈希数据的——标记一丢就返回 `None`，每次部署都会被判「需要部署」。
/// 所以写盘前先原样抠出标记块，写完再拼回文件末尾。
fn save_user_yaml_preserving_manifest(config: &Value, path: &Path) {
    let preserved = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| extract_weasel_next_block(&text));
    let mut config = config.clone();
    // 树里若还有 weasel_next 数据键，先摘掉，避免拼回后出现两个同名顶层键
    if let Some(object) = config.as_object_mut() {
        object.remove("weasel_next");
    }
    tree::save(&config, path);
    if let Some(block) = preserved {
        let mut saved = std::fs::read_to_string(path).unwrap_or_default();
        while saved.ends_with("\n\n") {
            saved.pop();
        }
        if !saved.ends_with('\n') {
            saved.push('\n');
        }
        saved.push_str(&block);
        saved.push('\n');
        let _ = std::fs::write(path, saved);
    }
}

/// 抠出 `user.yaml` 里的 `weasel_next` 带标记块（含两条注释行）。
fn extract_weasel_next_block(text: &str) -> Option<String> {
    const BEGIN: &str = "# >>> weasel_next";
    const END: &str = "# <<< weasel_next";
    let begin = text.find(BEGIN)?;
    let tail = &text[begin..];
    let end = tail.find(END)?;
    let line_end = tail[end..]
        .find('\n')
        .map(|offset| end + offset)
        .unwrap_or(tail.len());
    Some(tail[..line_end].to_string())
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
    /// `switcher/caption`：第一级菜单提示语，缺省「方案」（两字标题）。
    /// C++ 缺省是 `:-)`，这里按 2026-11-09 用户口径改成两字标题。
    caption: String,
    /// `switcher/group_caption`：第二级（子方案）菜单提示语，缺省「方案」。
    /// 两级标题都恰好两字，候选窗编码栏上方不会挤成长句。
    group_caption: String,
    /// `switcher/hotkeys`：唤起键（打开方案菜单，标准 Rime 三步交互）。
    hotkeys: Vec<KeyEvent>,
    /// `switcher/direct_next_hotkeys`：**按一下直接切到下一方案**，不弹菜单。
    ///
    /// 2026-11-07 加：用户要的是「Ctrl+` 切方案」，而不是「打开菜单 → 再按高亮
    /// → 空格确认」；而且菜单依赖候选窗显示链路（锚点/可见性），一旦宿主没给插入点
    /// 就整帧不显示，看起来像「按键直接过去了」。直切绕开整条 UI 链路，
    /// 按键被 `Accepted` 吞掉、方案立即重建，确定有反馈。缺省空 = 标准 Rime 行为。
    direct_next: Vec<KeyEvent>,
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
    /// 当前进入的"方案 → 子方案"分组（父方案 id）。
    /// `None` = 第一级（只列主方案）；`Some` = 第二级（只列该父下的子方案 + 返回项）。
    group: Option<String>,
    /// `switcher/two_level_menu`：`true` = 旧的下钻式两级菜单（第二级整屏替换）；
    /// `false`（缺省）= **内联折叠列表**（分组根行就地展开子项）。
    ///
    /// 2026-11-13 用户口径：「把 Ctrl+` 做成折叠列表，里面是各种方案」。
    /// 下钻式保留一个配置开关：验收不满意时只改 `default.yaml` 就能切回，不必重编。
    two_level_menu: bool,
    /// 折叠列表模式下当前**展开**的分组根 id（`input_types` 主方案或旧式父方案）。
    expanded: HashSet<String>,
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
            .unwrap_or_else(|| "方案".to_string());
        let group_caption = string_at("/switcher/group_caption")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "方案".to_string());

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

        // `switcher/direct_next_hotkeys`：与 hotkeys 同样的键串列表，但语义是直切。
        let direct_next = default_config
            .pointer("/switcher/direct_next_hotkeys")
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

        // 必须在 `config: default_config` 之前取值：`bool_at` 闭包借走了它的所有权。
        let two_level_menu = bool_at("/switcher/two_level_menu");

        Switcher {
            fold_options: bool_at("/switcher/fold_options"),
            fix_schema_list_order: bool_at("/switcher/fix_schema_list_order"),
            config: default_config,
            caption,
            group_caption,
            hotkeys,
            direct_next,
            save_options,
            active: false,
            items: Vec::new(),
            user_dir,
            build_dir: build_dir.to_path_buf(),
            page_size,
            group: None,
            two_level_menu,
            expanded: HashSet::new(),
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
        save_user_yaml_preserving_manifest(config, &dir.join("user.yaml"));
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
        let items = list_translator::build_items(
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

        // ① 方案部分（两级：父方案 + 子方案）
        let flat = list_translator::build_items(
            &engine.schema.schema_id,
            &self.config,
            &self.build_dir,
            &access_times,
            self.fix_schema_list_order,
        );
        let mut items = if self.two_level_menu {
            // 旧下钻式两级菜单：第二级整屏替换（配置开关，保留做回退）。
            list_translator::two_level_items(
                flat,
                &self.build_dir,
                self.group.as_deref(),
                &engine.schema.schema_id,
            )
        } else {
            // 折叠列表（2026-11-13 默认）：一级行 = 各方案；当前方案所属的分组根
            // 就地展开，输入类型子项缩进排在下一行（不换屏、无「返回上一级」）。
            let expanded = self.expanded.clone();
            list_translator::collapsed_items(
                flat,
                &self.build_dir,
                &engine.schema.schema_id,
                &expanded,
                &|name| engine.context.get_option(name),
            )
        };
        // 折叠状态下，"当前方案"那条要顺带把开关列表展开（C++ `SchemaAction`）；
        // 该行为只在第一级生效——第二级已经不列父方案自身，全是子方案。
        if folded
            && self.group.is_none()
            && let Some(first) = items.first_mut()
            && let SwitcherCommand::Schema(id) = &first.command
        {
            first.command = SwitcherCommand::SchemaWithUnfold(id.clone());
        }

        // ② 第二级末尾补一条可见的「返回上一级」。
        //    放末尾而不是开头：refresh_menu 每次把高亮归到 0，若放开头，
        //    一进第二级按空格就直接退回去了（子方案应当排在前面）。
        if self.group.is_some() {
            items.push(SwitcherItem {
                candidate: Candidate::new("switch", 0, 0, "‹ 返回上一级", 0.0),
                command: SwitcherCommand::Back,
            });
            return items;
        }
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
        // ⚠️ 层级切换时 prompt 必须跟着换：旧实现只在 segments 为空时写一次，
        //    于是从「方案」进第二级后标题还停在第一级那句话，看起来像没进级。
        {
            let segments = &mut engine.context.composition.segmentation.segments;
            if segments.is_empty() {
                segments.push(Segment::new(0, 0));
            }
            if let Some(segment) = segments.last_mut() {
                // 折叠列表没有第二级，标题恒为第一级 caption。
                segment.prompt = if self.two_level_menu && self.group.is_some() {
                    self.group_caption.clone()
                } else {
                    self.caption.clone()
                };
            }
        }
        self.items = self.build_items(engine);

        let mut menu = Menu::new();
        // 坐标 0..0：这是个"空段"，候选只是菜单条目，不覆盖任何输入。
        // ⚠️ 必须**一次性**塞进一个 VecTranslation：add_translation 每调一次都会把
        // 已有流包成 MergedTranslation，而 MergedTranslation 按 candidate.quality
        // 降序归并。当前方案自带 SCHEMA_QUALITY_BASE(1e6)，一旦分条加入，它会在
        // 渲染层被顶到最前；但数字键/空格确认仍按 self.items 的原下标取命令，
        // 于是「显示第 1 项、按 2 选到第 2 项」的错位就出现了（2026-11-11 用户
        // 截图复现：第二级显示「汉心龙, 拼音, 明月…」，按 2 却切到 pinyin.hxlong）。
        // 单个 VecTranslation 让菜单顺序 == self.items 下标，confirm 才不会错位。
        menu.add_translation(Box::new(
            crate::gear::translator::translation::VecTranslation::new(
                self.items.iter().map(|item| item.candidate.clone()).collect(),
            ),
        ));
        // 这里也不调 `Menu::prepare()`：顺序已在 VecTranslation 里钉死，
        // 宿主翻页只是按页切片，不会再按 quality 重排。
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
        self.group = None;
        self.expanded.clear();
        if !self.two_level_menu {
            // 折叠列表首次打开：默认展开当前方案所属的分组（拼音 → 直接看到
            // 当前输入类型；旧式子方案 → 看到它所在的父方案）。
            if let Some(group) = list_translator::initial_expansion(
                &self.build_dir,
                &engine.schema.schema_id,
            ) {
                self.expanded.insert(group);
            }
        }
        self.active = true;
        self.refresh_menu(engine);
    }

    /// C++ `Switcher::Deactivate`。
    fn deactivate(&mut self, engine: &mut Engine) {
        self.active = false;
        engine.context.set_option("dumb", false);
        // 折叠态是菜单内部状态，收起后别留在 context 里（下次 activate 会按
        // `switcher/fold_options` 重新置位）。
        engine.context.set_option("_fold_options", false);
        self.items.clear();
        self.group = None;
        self.expanded.clear();
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
    /// `UnfoldOptions` 不关菜单（C++ 里它的 `Apply` 只是重刷）。
    ///
    /// ⚠️ 2026-11-08：`SchemaWithUnfold` 原本也走「先展开开关、菜单不关」，
    /// 但 `fold_options: true` 时第 0 条（当前方案）正是它 —— 用户按一下空格
    /// 只展开、再按一下才关菜单，就是「选方案要按两下」。现按 `Schema` 同样处理：
    /// 一次确认直接选中并收起菜单。展开开关列表的入口另议，不再占用空格。
    fn apply_command(&mut self, engine: &mut Engine, command: SwitcherCommand) {
        match command {
            SwitcherCommand::UnfoldOptions => {
                engine.context.set_option("_fold_options", false);
                self.refresh_menu(engine);
            }
            SwitcherCommand::OpenGroup(group) => {
                if self.two_level_menu {
                    // 旧下钻式：进第二级，不切方案、不关菜单；Esc/退格/Tab 返回。
                    self.group = Some(group);
                    self.refresh_menu(engine);
                } else {
                    // 折叠列表：就地展开/收起，菜单不关，高亮留在这一行上。
                    if !self.expanded.remove(&group) {
                        self.expanded.insert(group.clone());
                    }
                    self.refresh_menu(engine);
                    if let Some(index) = self.items.iter().position(|item| {
                        matches!(&item.command, SwitcherCommand::OpenGroup(id) if *id == group)
                    }) {
                        self.set_selected_index(engine, index);
                    }
                }
            }
            SwitcherCommand::Schema(schema_id)
            | SwitcherCommand::SchemaWithUnfold(schema_id) => {
                self.deactivate(engine);
                // 登记，不直接切：处理器链跑完后由 `Engine::process_key` 执行
                engine.request_schema(schema_id);
                // 切方案会推高 context 代次，TSF 的 `tested_key` 缓存失效后
                // `OnKeyDown` 会把同一次物理空格再送一遍。那时 switcher 已退出，
                // 空格 `eaten=false` 透传给应用 = 上屏空格。标记让引擎吞掉它。
                // 只在「切方案」时置：Esc / 开关 / 单选关菜单不该吞后续空格。
                engine.swallow_next_space = true;
            }
            SwitcherCommand::InputType(name) => {
                self.deactivate(engine);
                // 互斥单选切输入类型 + 清空组字；swallow 让同一次确认空格不落上屏
                // （与切方案同一语义：菜单确认键不该作为正文空格吞吐）。
                engine.select_input_type(&name);
                engine.swallow_next_space = true;
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
            SwitcherCommand::Back => {
                // 菜单不关；回到第一级并把高亮放回刚进来的那个父方案。
                self.go_back(engine);
            }
        }
    }

    /// 从第二级退回第一级（菜单不关）。返回 `true` 表示确实退了一级。
    ///
    /// 退回后把高亮放回刚才进入的那个父方案，避免每次都跳回第 0 条。
    fn go_back(&mut self, engine: &mut Engine) -> bool {
        let Some(group) = self.group.take() else {
            return false;
        };
        self.refresh_menu(engine);
        if let Some(index) = self.items.iter().position(|item| {
            matches!(&item.command, SwitcherCommand::OpenGroup(id) if *id == group)
        }) {
            self.set_selected_index(engine, index);
        }
        true
    }

    /// 折叠列表：收起全部分组（菜单不关）；高亮回到原选中项所属的分组根行。
    fn collapse_all(&mut self, engine: &mut Engine) {
        let owner = self.owner_group(engine);
        self.expanded.clear();
        self.refresh_menu(engine);
        if let Some(owner) = owner
            && let Some(index) = self.items.iter().position(|item| {
                matches!(&item.command, SwitcherCommand::OpenGroup(id) if *id == owner)
            })
        {
            self.set_selected_index(engine, index);
        }
    }

    /// 当前选中项所属的分组根：选中分组根行 = 自己；选中子行 = 行号之前最近的
    /// 那个分组根行。折叠列表收起时用它把高亮放回原分组。
    fn owner_group(&self, engine: &Engine) -> Option<String> {
        let index = self.selected_index(engine);
        match self.items.get(index).map(|item| &item.command) {
            Some(SwitcherCommand::OpenGroup(id)) => Some(id.clone()),
            Some(SwitcherCommand::InputType(_)) => self.items
                [..index.min(self.items.len())]
                .iter()
                .rev()
                .find_map(|item| match &item.command {
                    SwitcherCommand::OpenGroup(id) => Some(id.clone()),
                    _ => None,
                }),
            _ => None,
        }
    }

    /// 确认当前选中项。
    fn confirm(&mut self, engine: &mut Engine) {
        let index = self.selected_index(engine);
        let Some(command) = self.items.get(index).map(|item| item.command.clone()) else {
            self.deactivate(engine);
            return;
        };
        // 切方案分支会在 `apply_command` 里置 `swallow_next_space`；
        // 其余命令（开关 / 单选 / 收起）不置，避免误吞用户后续输入的空格。
        self.apply_command(engine, command);
    }
}

impl Processor for Switcher {
    fn process_key_event(&mut self, engine: &mut Engine, key: &KeyEvent) -> ProcessResult {
        // ⓪ 直切热键：按一下立刻换下一方案，不弹菜单、不等确认。
        //    `key.release()` 带 Release 位，不会匹配这里的键串，所以只在按下方向触发。
        //    `request_schema` 只登记，`Engine::process_key` 在处理器链跑完后 `apply_schema`。
        if !self.active && self.direct_next.iter().any(|hotkey| hotkey == key) {
            if let Some(next) = self.next_schema(&engine.schema.schema_id) {
                engine.request_schema(next);
            }
            return ProcessResult::Accepted;
        }

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

        // ①.5 OS 自动重复帧：方向键 / 翻页键长按会以约 30Hz 重复投递，
        // 高亮会飞速掠过中间方案（用户 2026-11-08：「上下键移动还是很快，
        // 中间的方案经常被跳过」）。只认首次按下，重复帧吞掉不移动。
        // 其它键（字母 / 数字 / 空格）的自动重复照旧放行，不影响连续输入。
        if key.repeat()
            && matches!(
                key.keycode,
                XK_UP | XK_DOWN | XK_LEFT | XK_RIGHT | XK_PRIOR | XK_NEXT
            )
        {
            return ProcessResult::Accepted;
        }

        // ①.6 松开键：菜单展开时任何松键都不动作。C++ `Navigator::ProcessKeyEvent`
        // 和 `Selector::ProcessKeyEvent` 开头都是 `if (key_event.release()) return kNoop;`，
        // 再由 `Switcher` 末尾统一 `return kAccepted` 吞掉；本移植原先只在 ④ 过滤
        // release（已在导航之后），TIP 又把 keyup 照常送进引擎 ⇒ **一次物理方向键
        // 走两步（按下 1 步 + 松开 1 步）**。2026-11-08 用户两次报「上下键移动太快、
        // 中间的方案经常被跳过」，实机截图实证：按一次 Down 高亮从第 1 条跳到第 3 条。
        // 别把这行挪到 ② 之后 —— 挪回去就是旧 bug。
        if key.release() {
            return ProcessResult::Accepted;
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
                // 翻页步长取 `context.page_size`（当前方案 `menu/page_size`）——
                // 与 c_api 切页、渲染端每页显示条数同源；原来用 default.yaml 的
                // `switcher/menu.page_size`，两份不一致时 PageDown/数字会落到
                // 别的页上（菜单行更多时更明显）。
                let step = engine.context.page_size.max(1) as isize;
                self.move_selection(engine, -step);
                return ProcessResult::Accepted;
            }
            XK_NEXT => {
                let step = engine.context.page_size.max(1) as isize;
                self.move_selection(engine, step);
                return ProcessResult::Accepted;
            }
            _ => {}
        }

        // ③ 数字直选（C++ `Selector::SelectCandidateAt`：**页内相对**，不是全局下标）。
        //    2026-11-08 用户报障：菜单翻到第 2/3 页后按数字，选中的永远是第 1 页
        //    同位置的条目 —— 症状即这里原先用绝对下标。C++ 语义：
        //    `page_start = selected_index / page_size * page_size`，选 `page_start + n`；
        //    数字超出页容量（page_size=5 时按 6-9）不做事，只吃键。
        if let Some(ch) = key.printable_char()
            && ('1'..='9').contains(&ch)
        {
            let page_size = engine.context.page_size.max(1);
            let offset = ch as usize - '1' as usize;
            if offset < page_size {
                let page_start = self.selected_index(engine) / page_size * page_size;
                let index = page_start + offset;
                if index < self.items.len() {
                    self.set_selected_index(engine, index);
                    self.confirm(engine);
                }
            }
            return ProcessResult::Accepted;
        }

        // ④ 松开 / 带修饰的键：吞掉，不做事（C++ 同样 `return kAccepted`）
        if key.release() || key.ctrl() || key.alt() {
            return ProcessResult::Accepted;
        }

        // ⑤ 确认 / 退出 / 分级
        match key.keycode {
            XK_SPACE | XK_RETURN | XK_KP_ENTER => self.confirm(engine),
            // Esc：下钻式第二级先回第一级、第一级才收起；折叠列表则先把展开的
            // 分组全部收起（高亮回原分组），再按一次才关菜单。
            XK_ESCAPE => {
                if self.two_level_menu {
                    if !self.go_back(engine) {
                        self.deactivate(engine);
                    }
                } else if self.expanded.is_empty() {
                    self.deactivate(engine);
                } else {
                    self.collapse_all(engine);
                }
            }
            // 退格：下钻式只在第二级有效（返回上一级）；折叠列表=收起所有展开分组。
            XK_BACKSPACE => {
                if self.two_level_menu {
                    self.go_back(engine);
                } else if !self.expanded.is_empty() {
                    self.collapse_all(engine);
                }
            }
            // Tab：分组根行上展开/收起（折叠列表）或进第二级（下钻式）。菜单不关。
            XK_TAB => {
                if self.two_level_menu && self.group.is_some() {
                    self.go_back(engine);
                } else if matches!(
                    self.items.get(self.selected_index(engine)).map(|item| &item.command),
                    Some(SwitcherCommand::OpenGroup(_))
                ) {
                    self.confirm(engine);
                }
            }
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
                    "caption": "〔方案菜单〕",
                    "hotkeys": ["F4", "Control+grave"],
                    "save_options": ["full_shape"]
                },
                "schema_list": [{ "schema": "luna_pinyin" }]
            }),
            Path::new("."),
            None,
        )
    }

    /// 切方案写回 `user.yaml` 时，`weasel_next` 哈希段（含标记）必须原样保留。
    /// 这是哈希段消失的直接回归：以前 `tree::load` 遇到重复键失败 → 空树重写 →
    /// 整段 SHA256 被冲掉，标记块一丢 `deploy_manifest::load` 就返回 `None`。
    #[test]
    fn saving_user_yaml_keeps_weasel_next_manifest_block() {
        let root = std::env::temp_dir().join(format!("switcher-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("user.yaml");
        let manifest = concat!(
            "# >>> weasel_next 部署记录（自动生成，请勿手改）>>>\n",
            "weasel_next:\n",
            "  deployed_at: \"2026-11-11 11:00:00\"\n",
            "  folders:\n",
            "    \"shared/schema/pinyin\": \"2026-11-11 11:00:00 abcdef\"\n",
            "  records:\n",
            "    - schema: \"pinyin\"\n",
            "      hash: \"1ccbd668\"\n",
            "      arrived_at: \"2026-11-11T02:52:56.105Z\"\n",
            "      finished_at: \"2026-11-11T02:53:07.666Z\"\n",
            "# <<< weasel_next 部署记录 <<<\n",
        );
        std::fs::write(
            &path,
            format!("var:\n  previously_selected_schema: cangjie.5\n{manifest}"),
        )
        .unwrap();

        record_active_schema(Some(&root), "pinyin");
        let text = std::fs::read_to_string(&path).unwrap();
        println!("after record_active_schema:\n{text}");
        assert!(
            text.contains("# >>> weasel_next"),
            "标记块必须还在，deploy_manifest::load 靠它找哈希"
        );
        assert!(text.contains("\"shared/schema/pinyin\""), "SHA256 哈希不能丢");
        assert!(text.contains("- schema: \"pinyin\""), "方案记录不能丢");
        assert!(
            text.contains("previously_selected_schema: pinyin"),
            "新方案要写进去"
        );
        assert_eq!(text.matches("weasel_next:").count(), 1, "不能出现重复顶层键");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 第二级的 Esc 只回上一级（菜单不关）；再按一次才收起菜单。
    #[test]
    fn esc_in_second_level_returns_to_main_level_first() {
        let mut s = switcher();
        s.two_level_menu = true;
        s.active = true;
        s.group = Some("pinyin".to_string());
        let mut engine = Engine::empty_for_test();

        assert_eq!(
            s.process_key_event(&mut engine, &KeyEvent::new(XK_ESCAPE, 0)),
            ProcessResult::Accepted
        );
        assert!(s.is_active(), "第二级 Esc 只是返回，不关菜单");
        assert_eq!(s.group, None, "已回到第一级");

        s.process_key_event(&mut engine, &KeyEvent::new(XK_ESCAPE, 0));
        assert!(!s.is_active(), "第一级 Esc 才收起菜单（原行为）");
    }

    /// 退格只在第二级作为"返回上一级"；第一级不响应，也不关菜单。
    #[test]
    fn backspace_goes_back_only_in_second_level() {
        let mut s = switcher();
        s.two_level_menu = true;
        s.active = true;
        let mut engine = Engine::empty_for_test();

        s.process_key_event(&mut engine, &KeyEvent::new(XK_BACKSPACE, 0));
        assert!(s.is_active(), "第一级退格不关菜单");

        s.group = Some("pinyin".to_string());
        s.process_key_event(&mut engine, &KeyEvent::new(XK_BACKSPACE, 0));
        assert!(s.is_active());
        assert_eq!(s.group, None);
    }

    #[test]
    fn reads_settings_from_default_config() {
        let s = switcher();
        assert_eq!(s.caption, "〔方案菜单〕");
        assert_eq!(s.hotkeys.len(), 2);
        assert!(s.hotkeys.iter().any(|k| k.keycode == 0xffc1 || k.repr().contains("F4")));
        assert!(s.save_options.contains("full_shape"));
        assert!(!s.is_active());
    }

    /// 2026-11-07 回归：`Ctrl+grave` 配成直切键后，不应再被当成"打开菜单"的热键。
    #[test]
    fn direct_next_hotkeys_are_separate_from_menu_hotkeys() {
        let s = Switcher::new(
            json!({
                "switcher": {
                    "hotkeys": ["F4"],
                    "direct_next_hotkeys": ["Control+grave"]
                },
                "schema_list": [{ "schema": "luna_pinyin" }]
            }),
            Path::new("."),
            None,
        );
        assert_eq!(
            s.direct_next,
            vec![KeyEvent::parse("Control+grave").unwrap()],
            "直切键应被解析成 keycode=0x60 + Control"
        );
        assert_eq!(s.hotkeys.len(), 1, "F4 仍负责打开菜单");
        assert!(!s.is_hotkey(&KeyEvent::parse("Control+grave").unwrap()));
    }

    #[test]
    fn default_caption_and_page_size() {
        let s = Switcher::new(json!({}), Path::new("."), None);
        assert_eq!(s.caption, "方案", "第一级缺省两字标题（C++ 的 :-) 已被覆盖）");
        assert_eq!(s.group_caption, "方案", "第二级缺省两字标题");
        assert_eq!(s.page_size, 5);
        assert!(s.hotkeys.is_empty());
    }

    /// 两字标题都能被 default.yaml 覆盖，避免部署方想改字就得改代码。
    #[test]
    fn captions_are_overridable_from_default_yaml() {
        let s = Switcher::new(
            json!({ "switcher": { "caption": "选案", "group_caption": "样式" } }),
            Path::new("."),
            None,
        );
        assert_eq!(s.caption, "选案");
        assert_eq!(s.group_caption, "样式");
    }

    /// 两级菜单：标题跟着层级换，第二级有一条可见的「返回上一级」。
    #[test]
    fn two_level_titles_and_visible_back_item() {
        let mut s = Switcher::new(
            json!({ "schema_list": [{ "schema": "luna_pinyin" }] }),
            Path::new("."),
            None,
        );
        s.two_level_menu = true;
        let mut engine = Engine::empty_for_test();
        s.active = true;
        let prompt = |engine: &Engine| {
            engine
                .context
                .composition
                .segmentation
                .segments
                .last()
                .map(|segment| segment.prompt.clone())
                .unwrap_or_default()
        };

        s.refresh_menu(&mut engine);
        assert_eq!(prompt(&engine), "方案", "第一级两字标题");
        assert!(
            !s.items.iter().any(|i| matches!(i.command, SwitcherCommand::Back)),
            "第一级不该有返回项"
        );

        s.group = Some("pinyin".to_string());
        s.refresh_menu(&mut engine);
        assert_eq!(prompt(&engine), "方案", "进第二级标题必须跟着换");
        let last = s.items.len() - 1;
        assert!(matches!(s.items[last].command, SwitcherCommand::Back));
        assert_eq!(s.items[last].candidate.text, "‹ 返回上一级");

        s.set_selected_index(&mut engine, last);
        s.confirm(&mut engine);
        assert!(s.is_active(), "返回上一级不关菜单");
        assert_eq!(s.group, None, "已回第一级");
        assert_eq!(prompt(&engine), "方案", "退回后标题恢复第一级");
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
        let ids = list_translator::for_each_schema_list_entry(&config, |id| {
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

    /// 2026-11-08 用户报障：Ctrl+` 菜单里按空格「要按两下才选方案，按完还上屏空格」。
    /// 回归两件事：① `fold_options` 下第 0 条 `SchemaWithUnfold` 一次确认就收菜单；
    /// ② 菜单收起后宿主重复投递的那一下空格被吞，不进 commit_text。
    #[test]
    fn folded_schema_confirms_in_one_space_and_swallows_replayed_space() {
        let config = json!({
            "switcher": {
                "caption": "〔方案菜单〕",
                "hotkeys": ["Control+grave"],
                "fold_options": true
            },
            "schema_list": [{ "schema": "luna_pinyin" }, { "schema": "cangjie.5" }]
        });
        let mut s = Switcher::new(config.clone(), Path::new("."), None);
        let mut engine = Engine::empty_for_test();
        s.activate(&mut engine);
        assert!(s.is_active());

        // 模拟折叠状态下第 0 条（当前方案）被改写成 SchemaWithUnfold
        let mut items = list_translator::build_items(
            "luna_pinyin",
            &config,
            Path::new("."),
            &HashMap::new(),
            false,
        );
        assert!(!items.is_empty(), "方案清单至少要有当前方案这一条");
        items[0].command = SwitcherCommand::SchemaWithUnfold("luna_pinyin".to_string());
        s.items = items;
        engine
            .context
            .composition
            .segmentation
            .segments
            .last_mut()
            .unwrap()
            .selected_index = 0;

        // ① 一下空格（confirm）就该收菜单
        s.confirm(&mut engine);
        assert!(!s.is_active(), "一次确认必须关掉菜单，不能只展开开关列表");
        assert!(engine.swallow_next_space, "菜单收起后要防宿主重复投递空格");

        // ② 宿主重复投递的那一下空格：吞掉、不上屏
        let eaten = engine.process_key(&KeyEvent::new(XK_SPACE, 0));
        assert!(eaten, "重复投递的空格必须被吞（eaten=true）");
        assert!(!engine.swallow_next_space, "标记只能用一次");
        assert!(engine.context.commit_text.is_none(), "不能有空格上屏");
        assert!(engine.context.input.is_empty(), "不能进输入串");
    }

    /// 吞空格标记只在「切方案」时置：Esc / 开关 / 单选收起菜单后，用户紧接着
    /// 按的空格必须照常处理，不能被悄悄吃掉。
    #[test]
    fn escape_closing_menu_does_not_swallow_following_space() {
        let mut s = Switcher::new(
            json!({
                "switcher": { "hotkeys": ["Control+grave"] },
                "schema_list": [{ "schema": "luna_pinyin" }]
            }),
            Path::new("."),
            None,
        );
        let mut engine = Engine::empty_for_test();
        s.activate(&mut engine);
        assert!(s.is_active());
        s.deactivate(&mut engine);
        assert!(!engine.swallow_next_space, "Esc 关菜单不置吞空格标记");
        assert!(
            !engine.process_key(&KeyEvent::new(XK_SPACE, 0)),
            "后续空格应照常走处理器链，不能被吞"
        );
    }

    /// 2026-11-08 用户报障：菜单翻到第 2/3 页后按数字，选中的永远是第 1 页
    /// 同位置的条目。C++ `Selector::SelectCandidateAt` 语义是**页内相对**：
    /// `page_start = selected_index / page_size * page_size`，选 `page_start + n`；
    /// 数字超出页容量不做事（只吃键）。
    #[test]
    fn digit_selects_within_current_page_not_first_page() {
        let mut s = Switcher::new(
            json!({
                "switcher": { "hotkeys": ["F4"] },
                "schema_list": [
                    { "schema": "s0" }, { "schema": "s1" }, { "schema": "s2" },
                    { "schema": "s3" }, { "schema": "s4" }, { "schema": "s5" },
                    { "schema": "s6" }
                ]
            }),
            Path::new("."),
            None,
        );
        assert_eq!(s.page_size, 5, "default.yaml 缺省页容量是 5");
        let mut engine = Engine::empty_for_test();
        // 手工塞 7 条方案条目，不依赖真实 build 产物；
        // activate→refresh_menu 会重建 items，所以每轮重开都要重塞
        let fake_items = || {
            (0..7)
                .map(|i| SwitcherItem {
                    candidate: Candidate::new("schema", 0, 0, format!("方案{i}"), 0.0),
                    command: SwitcherCommand::Schema(format!("s{i}")),
                })
                .collect::<Vec<_>>()
        };

        // 翻到第 2 页（高亮 index 5），按 `1` → 应确认本页第 1 条 = 全局 s5，
        // 而不是第 1 页第 1 条 s0（修复前的行为）
        s.activate(&mut engine);
        s.items = fake_items();
        s.set_selected_index(&mut engine, 5);
        s.process_key_event(&mut engine, &KeyEvent::new('1' as u32, 0));
        assert!(!s.is_active(), "数字直选要收菜单");
        assert_eq!(
            engine.pending_schema(),
            Some("s5"),
            "第 2 页按 1 必须选本页第 1 条（全局 index 5）"
        );

        // 重新打开，按 `2` → s6（第 2 页第 2 条）
        engine.clear_pending_schema();
        s.activate(&mut engine);
        s.items = fake_items();
        s.set_selected_index(&mut engine, 5);
        s.process_key_event(&mut engine, &KeyEvent::new('2' as u32, 0));
        assert_eq!(engine.pending_schema(), Some("s6"));

        // 第 1 页按 `5` → s4（页容量内照旧）
        engine.clear_pending_schema();
        s.activate(&mut engine);
        s.items = fake_items();
        s.set_selected_index(&mut engine, 2);
        s.process_key_event(&mut engine, &KeyEvent::new('5' as u32, 0));
        assert_eq!(engine.pending_schema(), Some("s4"));

        // 数字超出页容量（page_size=5 时按 6）：不选中、菜单不关
        engine.clear_pending_schema();
        s.activate(&mut engine);
        s.items = fake_items();
        s.set_selected_index(&mut engine, 5);
        s.process_key_event(&mut engine, &KeyEvent::new('6' as u32, 0));
        assert!(s.is_active(), "超页容量的数字只吃键，不确认");
        assert_eq!(engine.pending_schema(), None, "不能触发任何切换");
    }

    /// 2026-11-08 用户报障：「按上下键的移动还是很快，中间的方案经常被跳过」。
    /// OS 自动重复以约 30Hz 投递方向键，switcher 必须只认首次按下。
    #[test]
    fn auto_repeated_navigation_is_swallowed_not_skipped() {
        let mut s = Switcher::new(
            json!({
                "switcher": { "hotkeys": ["F4"] },
                "schema_list": [
                    { "schema": "s0" }, { "schema": "s1" }, { "schema": "s2" },
                    { "schema": "s3" }, { "schema": "s4" }, { "schema": "s5" }
                ]
            }),
            Path::new("."),
            None,
        );
        let mut engine = Engine::empty_for_test();
        s.activate(&mut engine);
        s.items = (0..6)
            .map(|i| SwitcherItem {
                candidate: Candidate::new("schema", 0, 0, format!("方案{i}"), 0.0),
                command: SwitcherCommand::Schema(format!("s{i}")),
            })
            .collect();
        s.set_selected_index(&mut engine, 0);

        // 30 帧自动重复的 Down：一次都不许移动
        for _ in 0..30 {
            assert_eq!(
                s.process_key_event(&mut engine, &KeyEvent::new(XK_DOWN, K_REPEAT_MASK)),
                ProcessResult::Accepted,
                "重复帧要被吞掉（Accepted 但不移动）"
            );
        }
        assert_eq!(s.selected_index(&engine), 0, "高亮必须仍停在第 0 条");

        // 真正松开再按下：移动一格
        s.process_key_event(&mut engine, &KeyEvent::new(XK_DOWN, 0));
        assert_eq!(s.selected_index(&engine), 1);

        // 翻页键的重复帧同样吞掉
        for _ in 0..10 {
            s.process_key_event(&mut engine, &KeyEvent::new(XK_NEXT, K_REPEAT_MASK));
        }
        assert_eq!(s.selected_index(&engine), 1);

        // 非导航键的自动重复照常放行（不能影响连续输入/退格）
        assert_ne!(
            s.process_key_event(&mut engine, &KeyEvent::new(XK_SPACE, K_REPEAT_MASK)),
            ProcessResult::Noop,
            "空格的重复帧仍应走正常确认路径"
        );
    }

    // ── 方案 → 子方案（两级菜单）────────────────────────────────────────

    /// 造一个只含编译产物的临时 build 目录：父方案 + 子方案 + 平铺方案。
    fn parent_fixture(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "lrn_switcher_group_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建方案沙箱");
        let write = |id: &str, name: &str, parent: Option<&str>, label: Option<&str>| {
            let mut text = format!("schema:\n  id: {id}\n  name: {name}\n");
            if let Some(parent) = parent {
                text.push_str(&format!("  parent: {parent}\n"));
            }
            if let Some(label) = label {
                text.push_str(&format!("  parent_label: {label}\n"));
            }
            std::fs::write(dir.join(format!("{id}.schema.yaml")), text).expect("写方案文件");
        };
        write("pinyin", "拼音", None, Some("全拼"));
        write("pinyin.ziranma", "自然码", Some("pinyin"), None);
        write("wubi86", "五笔", None, None);
        dir
    }

    fn parent_switcher(dir: &Path) -> Switcher {
        Switcher::new(
            json!({
                "switcher": { "hotkeys": ["F4"] },
                "schema_list": [
                    { "schema": "pinyin" },
                    { "schema": "pinyin.ziranma" },
                    { "schema": "wubi86" }
                ]
            }),
            dir,
            None,
        )
    }

    fn item_texts(s: &Switcher) -> Vec<String> {
        s.items().iter().map(|item| item.candidate.text.clone()).collect()
    }

    /// 用户口径：父方案一行、子方案收在其下；Tab 折叠/展开；且**收起后还能再展开**
    /// （此前有过「收起后展不开」的 bug）。
    ///
    /// 2026-11-13 起默认是**折叠列表**：展开不换屏，子项缩进排在父行下面。
    #[test]
    fn group_can_be_expanded_again_after_collapsing() {
        let dir = parent_fixture("reenter");
        let mut s = parent_switcher(&dir);
        let mut engine = Engine::empty_for_test();
        engine.schema.schema_id = "pinyin".to_string();
        s.activate(&mut engine);

        assert_eq!(
            item_texts(&s),
            vec!["拼音 ▸", "五笔"],
            "折叠列表：一级只列方案，有子项的父方案带收起箭头"
        );
        assert_eq!(
            s.items()[0].command,
            SwitcherCommand::OpenGroup("pinyin".to_string())
        );
        s.set_selected_index(&mut engine, 0);

        for round in 1..=3 {
            s.process_key_event(&mut engine, &KeyEvent::new(XK_TAB, 0));
            assert!(
                s.expanded.contains("pinyin"),
                "第 {round} 次 Tab 必须能展开（收起后展不开是回归点）"
            );
            assert_eq!(
                item_texts(&s),
                vec!["拼音 ▾", "\u{3000}自然码", "五笔"],
                "展开后子方案缩进排在父行下面，不换屏、没有返回项"
            );
            assert!(s.is_active(), "展开/收起只是重刷菜单，菜单不关");
            assert_eq!(s.selected_index(&engine), 0, "高亮留在父方案行上");

            s.process_key_event(&mut engine, &KeyEvent::new(XK_TAB, 0));
            assert!(!s.expanded.contains("pinyin"), "再按一次 Tab 收起");
            assert_eq!(item_texts(&s), vec!["拼音 ▸", "五笔"]);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 第二级只有子方案：选中它就直接切到该子方案（重建后选项独立生效）。
    /// 2026-11-12 起父方案自身不再占第二级首项，所以首项就是唯一子方案。
    #[test]
    fn selecting_child_switches_child_schema() {
        let dir = parent_fixture("select");
        let mut s = parent_switcher(&dir);
        s.two_level_menu = true;
        let mut engine = Engine::empty_for_test();
        engine.schema.schema_id = "pinyin".to_string();
        s.activate(&mut engine);

        s.set_selected_index(&mut engine, 0);
        s.process_key_event(&mut engine, &KeyEvent::new(XK_TAB, 0));
        assert_eq!(item_texts(&s), vec!["自然码", "‹ 返回上一级"]);
        s.set_selected_index(&mut engine, 0);
        s.confirm(&mut engine);
        assert!(!s.is_active(), "选方案要收菜单");
        assert_eq!(engine.pending_schema(), Some("pinyin.ziranma"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-11-11 用户截图「选不了方案」根因回归：
    /// 渲染菜单曾被 MergedTranslation 按 quality 重排（当前子方案 1e6 顶到首位），
    /// 而 confirm 仍按 self.items 下标取命令，显示项和实际切换错位。
    #[test]
    fn menu_display_order_matches_command_indices_in_second_level() {
        let dir = parent_fixture("order");
        std::fs::write(
            dir.join("pinyin.luna.schema.yaml"),
            "schema:\n  id: pinyin.luna\n  name: 明月\n  parent: pinyin\n",
        )
        .expect("写第二个子方案");
        let mut s = Switcher::new(
            json!({
                "switcher": { "hotkeys": ["F4"] },
                "schema_list": [
                    { "schema": "pinyin" },
                    { "schema": "pinyin.luna" },
                    { "schema": "pinyin.ziranma" },
                    { "schema": "wubi86" }
                ]
            }),
            &dir,
            None,
        );
        s.two_level_menu = true;
        let mut engine = Engine::empty_for_test();
        // 当前是子方案：build_items 给它 SCHEMA_QUALITY_BASE=1e6，
        // 旧实现分条 add_translation 时会把它排到显示首位。
        engine.schema.schema_id = "pinyin.ziranma".to_string();
        s.activate(&mut engine);
        s.set_selected_index(&mut engine, 0);
        s.process_key_event(&mut engine, &KeyEvent::new(XK_TAB, 0));

        let displayed = engine
            .context
            .composition
            .segmentation
            .segments
            .last()
            .and_then(|seg| seg.menu.as_ref())
            .map(|menu| {
                menu.all_candidates()
                    .iter()
                    .map(|c| c.text.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        assert_eq!(
            displayed,
            item_texts(&s),
            "渲染顺序必须与 self.items 下标同序"
        );
        assert_eq!(
            displayed,
            vec!["自然码", "明月", "‹ 返回上一级"],
            "第二级只列子方案（自然码、明月）+ 返回项，父方案自身不占位"
        );

        // 按显示的下标选：1=自然码，2=明月，
        // 不能再出现「显示第 1 项却切到第 2 项」的错位。
        s.process_key_event(&mut engine, &KeyEvent::new('1' as u32, 0));
        assert_eq!(engine.pending_schema(), Some("pinyin.ziranma"));
        engine.clear_pending_schema();

        s.activate(&mut engine);
        s.set_selected_index(&mut engine, 0);
        s.process_key_event(&mut engine, &KeyEvent::new(XK_TAB, 0));
        s.process_key_event(&mut engine, &KeyEvent::new('2' as u32, 0));
        assert_eq!(engine.pending_schema(), Some("pinyin.luna"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `parent` 指向清单里不存在的方案：平铺、不崩、不进第二级。
    #[test]
    fn dangling_parent_stays_flat_and_does_not_break_menu() {
        let dir = parent_fixture("dangling");
        std::fs::write(
            dir.join("orphan.schema.yaml"),
            "schema:\n  id: orphan\n  name: 孤儿\n  parent: no_such_schema\n",
        )
        .unwrap();
        let mut s = Switcher::new(
            json!({
                "switcher": { "hotkeys": ["F4"] },
                "schema_list": [{ "schema": "orphan" }, { "schema": "wubi86" }]
            }),
            &dir,
            None,
        );
        let mut engine = Engine::empty_for_test();
        engine.schema.schema_id = "orphan".to_string();
        s.activate(&mut engine);

        assert_eq!(item_texts(&s), vec!["孤儿", "五笔"]);
        assert!(
            s.items()
                .iter()
                .all(|item| matches!(item.command, SwitcherCommand::Schema(_))),
            "悬空 parent 不能被当成可展开分组"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 折叠列表（2026-11-13 默认菜单形态）────────────────────────────

    /// 造一个声明 input_types 的编译产物目录：拼音（全拼/自然码两型）+ 五笔。
    fn input_type_fixture(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "lrn_switcher_itypes_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建输入类型沙箱");
        // ⚠️ 用**单数** `input_type:`：与 2026-11-13 的 live 数据一致，
        // 引擎必须两键都认，否则拼音不会成为分组根。
        std::fs::write(
            dir.join("pinyin.schema.yaml"),
            "schema:\n  id: pinyin\n  name: 拼音\ninput_type:\n  enabled: [pinyin, zrm]\n  reset: pinyin\n  options:\n  - {option: pinyin, label: 全拼}\n  - {option: zrm, label: 自然码}\n",
        )
        .expect("写拼音方案");
        std::fs::write(dir.join("wubi86.schema.yaml"), "schema:\n  id: wubi86\n  name: 五笔\n")
            .expect("写五笔方案");
        dir
    }

    fn input_type_switcher(dir: &Path) -> Switcher {
        Switcher::new(
            json!({
                "switcher": { "hotkeys": ["F4"] },
                "schema_list": [{ "schema": "pinyin" }, { "schema": "wubi86" }]
            }),
            dir,
            None,
        )
    }

    /// 给测试引擎装上「拼音」方案配置与输入类型清单（切输入类型要用）。
    fn prime_input_types(engine: &mut Engine, dir: &Path) {
        engine.schema.schema_id = "pinyin".to_string();
        engine.schema.config = json!({
            "schema": { "id": "pinyin", "name": "拼音" },
            "input_type": {
                "enabled": ["pinyin", "zrm"],
                "reset": "pinyin",
                "options": [
                    {"option": "pinyin", "label": "全拼"},
                    {"option": "zrm", "label": "自然码"}
                ]
            }
        });
        engine.input_type_options =
            crate::engine::runtime::collect_input_type_options(&engine.schema.config, Some(dir));
    }

    /// 折叠列表：Ctrl+` 一打开就展开当前方案的分组，输入类型缩进列在下面。
    #[test]
    fn collapsed_menu_expands_current_input_types_inline() {
        let dir = input_type_fixture("open");
        let mut s = input_type_switcher(&dir);
        let mut engine = Engine::empty_for_test();
        prime_input_types(&mut engine, &dir);
        s.activate(&mut engine);

        assert_eq!(
            item_texts(&s),
            vec!["拼音 ▾", "\u{3000}全拼", "\u{3000}自然码", "五笔"],
            "当前方案（拼音）的分组默认展开，输入类型就地缩进"
        );
        assert_eq!(
            s.items()[0].command,
            SwitcherCommand::OpenGroup("pinyin".to_string())
        );
        assert_eq!(
            s.items()[1].command,
            SwitcherCommand::InputType("pinyin".to_string())
        );
        assert_eq!(
            s.items()[2].command,
            SwitcherCommand::InputType("zrm".to_string())
        );

        // Tab 在分组根行上收起；再按一次重新展开（收起后展不开是回归点）。
        s.set_selected_index(&mut engine, 0);
        s.process_key_event(&mut engine, &KeyEvent::new(XK_TAB, 0));
        assert_eq!(item_texts(&s), vec!["拼音 ▸", "五笔"]);
        s.process_key_event(&mut engine, &KeyEvent::new(XK_TAB, 0));
        assert_eq!(
            item_texts(&s),
            vec!["拼音 ▾", "\u{3000}全拼", "\u{3000}自然码", "五笔"]
        );

        // Esc：先收起展开的分组（菜单不关），再按一次才关菜单。
        s.process_key_event(&mut engine, &KeyEvent::new(XK_ESCAPE, 0));
        assert!(s.is_active(), "Esc 先收折叠，不关菜单");
        assert_eq!(item_texts(&s), vec!["拼音 ▸", "五笔"]);
        assert_eq!(s.selected_index(&engine), 0, "高亮留在原分组根行上");
        s.process_key_event(&mut engine, &KeyEvent::new(XK_ESCAPE, 0));
        assert!(!s.is_active(), "没有展开项后 Esc 才关菜单");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 折叠列表：选中输入类型子行 = 只切输入类型，不换方案、菜单收起。
    #[test]
    fn selecting_input_type_child_switches_option_only() {
        let dir = input_type_fixture("pick");
        let mut s = input_type_switcher(&dir);
        let mut engine = Engine::empty_for_test();
        prime_input_types(&mut engine, &dir);
        s.activate(&mut engine);

        // 行 0=拼音（▾）、1=全拼、2=自然码、3=五笔。
        s.set_selected_index(&mut engine, 2);
        s.process_key_event(&mut engine, &KeyEvent::new(XK_SPACE, 0));
        assert!(!s.is_active(), "选输入类型后菜单收起");
        assert!(
            engine.context.get_option("zrm"),
            "自然码开关要被选中（互斥单选）"
        );
        assert!(
            !engine.context.get_option("pinyin"),
            "同组其它输入类型必须被关掉"
        );
        assert_eq!(engine.pending_schema(), None, "输入类型不是方案，不能触发切方案");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
