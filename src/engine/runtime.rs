//! `src/rime/engine.{h,cc}`：`Engine`（持有 Schema + Context + 组件，驱动 Compose）。
//!
//! 对照 C++ `ConcreteEngine`：`ProcessKey` 遍历处理器链；输入串一变 =>
//! `OnContextUpdate` => `Compose` => `CalculateSegmentation` + `TranslateSegments`。
//!
//! # 组件装配（`InitializeComponents`）
//!
//! 方案编译后的 `engine/processors` / `engine/translators` 列表由 [`crate::engine::gear::registry`]
//! 逐个创建，**没移植的组件跳过而不是让链条断掉**（等同 C++ `Require` 失败的处理）。
//! 列表为空时回退到 `[speller, selector]` —— 保证只装配ator了 Schema 的测试环境仍可打字。

use std::path::Path;

use crate::dict::dictionary::Dictionary;
use crate::engine::composition::Composition;
use crate::engine::context::Context;
use crate::engine::gear::processor::{ProcessResult, Processor};
use crate::engine::gear::registry::{ComponentEntry, install_engine_components};
use crate::engine::gear::selector::Selector;
use crate::engine::gear::speller::Speller;
use crate::engine::gear::translator::{ScriptTranslator, Translator};
use crate::engine::key_event::KeyEvent;
use crate::engine::menu::Menu;
use crate::engine::schema::Schema;
use crate::engine::segmentation::{SegStatus, Segmentation};

/// 引擎（对照 C++ `Engine`）。
///
/// 翻译器/处理器以 `dyn Trait + Send + Sync` 持有：引擎会被塞进 `Mutex<Global>` 背后
/// 的 `static`，要求内部类型可跨线程安全共享（否则 `OnceLock<Mutex<Global>>` 不满足 `Sync`）。
pub struct Engine {
    pub schema: Schema,
    pub context: Context,
    translators: Vec<ComponentEntry<Box<dyn Translator + Send + Sync>>>,
    processors: Vec<ComponentEntry<Box<dyn Processor + Send + Sync>>>,
    build_dir: std::path::PathBuf,
}

impl Engine {
    /// 加载方案并装配组件（对照 `Engine::Create` + `InitializeComponents`）。
    pub fn create(build_dir: &Path, schema_id: &str) -> Option<Engine> {
        let schema = Schema::open(build_dir, schema_id)?;
        let mut engine = Engine {
            schema,
            context: Context::new(),
            translators: Vec::new(),
            processors: Vec::new(),
            build_dir: build_dir.to_path_buf(),
        };
        engine.initialize_components();
        Some(engine)
    }

    /// C++ `ConcreteEngine::InitializeComponents`：读 `engine/processors` 与
    /// `engine/translators`，按名单逐个构造。
    fn initialize_components(&mut self) {
        let (processors, translators) =
            install_engine_components(&self.schema.config, &self.build_dir);
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
        self.refresh_component_conditions();
    }

    /// C++ `UpdateComponentConditions`：opoption 变化后重算每个组件的启用状态。
    fn refresh_component_conditions(&mut self) {
        let ctx = &self.context;
        for entry in &mut self.processors {
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
        let input_before = self.context.input.clone();
        // 把 processors 取出到局部，避免「遍历 self.processors 同时把 &mut self 传进去」的借用冲突
        let mut processors = std::mem::take(&mut self.processors);
        let mut result = false;
        for entry in &mut processors {
            if !entry.is_enabled() {
                continue;
            }
            match entry.component.process_key_event(self, key) {
                ProcessResult::Rejected => {
                    result = false;
                    break;
                }
                ProcessResult::Accepted => {
                    result = true;
                    break;
                }
                ProcessResult::Noop => continue,
            }
        }
        self.processors = processors;
        // 组字状态可能改变了 option（如 AsciiComposer 切 ascii_mode），重算启用条件
        self.refresh_component_conditions();
        if self.context.input != input_before {
            self.compose();
        }
        result
    }

    /// 重算组合（对照 `ConcreteEngine::Compose` + `TranslateSegments`）。
    ///
    /// 简化模型：单段覆盖整段 `input`；菜单由所有 `Translator::Query` 铺平、排序、去重。
    pub fn compose(&mut self) {
        let input = self.context.input.clone();
        self.context.composition.reset(&input);
        self.context.composition.segmentation.segments.clear();
        if input.is_empty() {
            self.context
                .composition
                .segmentation
                .segments
                .push(Segmentation::segment_zero(0));
            return;
        }
        let mut seg = Segmentation::segment_zero(input.len());
        let mut menu = Menu::new();
        let mut translators = std::mem::take(&mut self.translators);
        for entry in &translators {
            if !entry.is_enabled() {
                continue;
            }
            for c in entry.component.query(&input, 0) {
                menu.add(c);
            }
        }
        self.translators = translators;
        menu.prepare();
        seg.menu = Some(menu);
        seg.status = SegStatus::Guess;
        self.context
            .composition
            .segmentation
            .segments
            .push(seg);
    }

    /// 已装配的 processor 名单（诊断用：验证 `engine/processors` 真的被读进来了）。
    pub fn processor_names(&self) -> Vec<&str> {
        self.processors.iter().map(|e| e.name.as_str()).collect()
    }

    /// 已装配的 translator 名单。
    pub fn translator_names(&self) -> Vec<&str> {
        self.translators.iter().map(|e| e.name.as_str()).collect()
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
        Engine {
            schema: Schema::empty_for_test(),
            context: Context::new(),
            translators: Vec::new(),
            processors: Vec::new(),
            build_dir: std::path::PathBuf::new(),
        }
    }
}
