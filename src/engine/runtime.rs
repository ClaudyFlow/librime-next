//! `src/rime/engine.{h,cc}`：`Engine`（持有 Schema + Context + 组件，驱动 Compose）。
//!
//! 对照 C++ `ConcreteEngine`：`ProcessKey` 遍历处理器链；`Compose` 重置组合 →
//! 填充单段菜单（由各 `Translator::Query` 产出）。本版处理器链为 `Speller` + `Selector`。

use std::path::Path;

use crate::dict::dictionary::Dictionary;
use crate::engine::composition::Composition;
use crate::engine::context::Context;
use crate::engine::gear::processor::{ProcessResult, Processor};
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
    translators: Vec<Box<dyn Translator + Send + Sync>>,
    processors: Vec<Box<dyn Processor + Send + Sync>>,
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
        // Speller 是配置驱动的（C++ 由 `SpellerComponent::Create` 从 schema 的
        // `speller:` 节点装配），这里直接从方案的编译后配置树构造。
        let speller = Speller::new(&engine.schema.config);
        engine.processors.push(Box::new(speller));
        engine.processors.push(Box::new(Selector));
        Some(engine)
    }

    fn initialize_components(&mut self) {
        if let Some((dict_name, prism_name)) = self.schema.first_translator_dict() {
            if let Some(dict) = Dictionary::open(&self.build_dir, &dict_name, &prism_name) {
                self.translators.push(Box::new(ScriptTranslator::new(dict)));
            }
        }
    }

    /// 处理一次按键（对照 `ConcreteEngine::ProcessKey`）。
    ///
    /// C++ 的对应机制：`Context::PushInput/PopInput/DeleteInput/Clear` 发
    /// `update_notifier_` → `ConcreteEngine::OnContextUpdate` → `Compose(ctx)`；
    /// `Highlight` 虽也发 update，但 `Composition::Reset` 对未变输入不弹段，
    /// 高亮得以保留。Rust 简化版 `compose()` 无条件清段，因此这里只在
    /// **输入串真的变了**时重算——纯高亮导航（Selector）不触发，符合 C++ 行为。
    pub fn process_key(&mut self, key: &KeyEvent) -> bool {
        let input_before = self.context.input.clone();
        // 把 processors 取出到局部，避免「遍历 self.processors 同时把 &mut self 传进去」的借用冲突
        let mut processors = std::mem::take(&mut self.processors);
        let mut result = false;
        for p in &mut processors {
            match p.process_key_event(self, key) {
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
        for t in &self.translators {
            for c in t.query(&input, 0) {
                menu.add(c);
            }
        }
        menu.prepare();
        seg.menu = Some(menu);
        seg.status = SegStatus::Guess;
        self.context
            .composition
            .segmentation
            .segments
            .push(seg);
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
}
