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
use crate::engine::gear::abc_segmentor::AbcSegmentor;
use crate::engine::gear::fallback_segmentor::FallbackSegmentor;
use crate::engine::gear::filter::Filter;
use crate::engine::gear::matcher::Matcher;
use crate::engine::gear::processor::{ProcessResult, Processor};
use crate::engine::gear::registry::{ComponentEntry, install_engine_components};
use crate::engine::gear::segmentor::{Segmentor, SegmentorView};
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
    segmentors: Vec<ComponentEntry<Box<dyn Segmentor + Send + Sync>>>,
    filters: Vec<ComponentEntry<Box<dyn Filter + Send + Sync>>>,
    /// 组件请求引擎代转的按键队列（见 [`Engine::enqueue_synthetic_key`]）。
    synthetic_queue: Vec<KeyEvent>,
    build_dir: std::path::PathBuf,
    /// 数据根列表，按 **C++ `ResourceResolver` 的查找顺序**：用户目录优先、共享目录其次。
    ///
    /// 为什么不能只留 `build_dir.parent()`：生产环境里 `build_dir` 是 `<用户目录>/build`
    /// （`Deployer::staging_dir`），而 `model/<语言>.gram` 在用户目录、`opencc/` 在共享目录
    /// —— 两东西不在同一个根下，必须两个都查。
    data_roots: Vec<std::path::PathBuf>,
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
        let schema = Schema::open(build_dir, schema_id)?;
        let data_roots = if data_roots.is_empty() {
            build_dir.parent().map(Path::to_path_buf).into_iter().collect()
        } else {
            data_roots.to_vec()
        };
        let mut engine = Engine {
            schema,
            context: Context::new(),
            translators: Vec::new(),
            processors: Vec::new(),
            segmentors: Vec::new(),
            filters: Vec::new(),
            synthetic_queue: Vec::new(),
            build_dir: build_dir.to_path_buf(),
            data_roots,
        };
        engine.initialize_components();
        Some(engine)
    }

    /// C++ `ConcreteEngine::InitializeComponents`：读 `engine/processors` 与
    /// `engine/translators`，按名单逐个构造。
    fn initialize_components(&mut self) {
        let (processors, segmentors, translators, filters) =
            install_engine_components(&self.schema.config, &self.build_dir, &self.data_roots);
        self.filters = filters;
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
        let input_before = self.context.input.clone();
        let mut result = self.run_processor_chain(key);
        // 组字状态可能改变了 option（如 AsciiComposer 切 ascii_mode），重算启用条件
        self.refresh_component_conditions();
        // 组件代转的按键（key_binder 的 send / send_sequence）：此时处理器列表已归位
        while let Some(synthetic) = self.synthetic_queue.pop() {
            if self.run_processor_chain(&synthetic) {
                result = true;
            }
            self.refresh_component_conditions();
        }
        if self.context.input != input_before {
            self.compose();
        }
        result
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
    /// 不存在"转发赶不上后续处理器"的差别。
    pub fn enqueue_synthetic_key(&mut self, key: KeyEvent) {
        self.synthetic_queue.push(key);
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
    pub fn apply_schema(&mut self, schema_id: &str) -> bool {
        match Schema::open(&self.build_dir, schema_id) {
            Some(schema) => {
                self.schema = schema;
                self.initialize_components();
                self.context.clear();
                true
            }
            None => {
                eprintln!("[engine] unknown schema: {schema_id}");
                false
            }
        }
    }

    /// 设定开关（给组件之外的调用方复用）。
    pub fn set_option(&mut self, name: &str, value: bool) {
        self.context.set_option(name, value);
    }

    /// C++ `ConcreteEngine::Compose`：caret 感知的重算入口。
    ///
    /// 三处必须与 C++ 一致：
    /// 1. 参与组合的是 **caret 之前**的活动输入；
    /// 2. caret 落在已确认段首尾且后面还有字符时，改用**整串** reset（给光标前后一起重算的机会）；
    /// 3. `reset` 是**差量**的 —— 已确认段（`status >= Selected`）不会被后续编辑推翻。
    pub fn compose(&mut self) {
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
            let comp = &mut self.context.composition;
            comp.segmentation.segments.clear();
            comp.segmentation
                .segments
                .push(Segmentation::segment_zero(0));
            return;
        }
        self.calculate_segmentation(caret);
        self.translate_segments();
    }

    /// C++ `CalculateSegmentation`：让每个 segmentor 依次 `Proceed`，跑完一轮 `Forward`。
    fn calculate_segmentation(&mut self, caret_pos: usize) {
        // 同样的借用规避：把组件列表取到局部再遍历
        let segmentors = std::mem::take(&mut self.segmentors);
        let view = SegmentorView {
            ascii_mode: self.context.get_option("ascii_mode"),
        };
        {
            let segs = &mut self.context.composition.segmentation;
            while !segs.has_finished() {
                let start_pos = segs.current_start();
                for entry in &segmentors {
                    if !entry.is_enabled() {
                        continue;
                    }
                    if !entry.component.proceed(segs, view) {
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
        let segs = &mut self.context.composition.segmentation.segments;
        for seg in segs.iter_mut() {
            if seg.status >= SegStatus::Guess {
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
            // 段标签决定翻译器是否认领这段（C++ `segment.HasAnyTagIn(tags_)`）
            let seg_tags = seg.tags.clone();
            for entry in &mut translators {
                if !entry.is_enabled() {
                    continue;
                }
                for mut c in entry.component.query(text, seg.start, &seg_tags) {
                    // translator 返回的是**相对本段**的位置，这里还原成绝对偏移
                    c.start += seg.start;
                    c.end += seg.start;
                    menu.add(c);
                }
            }
            menu.prepare();
            seg.menu = Some(menu);
            seg.status = SegStatus::Guess;
            seg.selected_index = 0;
        }
        self.translators = translators;
        self.apply_filters();
    }

    /// C++ `TranslateSegments` 里 `menu->AddFilter(...)` 那段：逐段套用 filter 链。
    fn apply_filters(&mut self) {
        if self.filters.is_empty() {
            return;
        }
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
            // 这一段该套哪些 filter（只读 `self.context`）
            let applicable: Vec<bool> = self
                .filters
                .iter()
                .map(|entry| {
                    let seg = &self.context.composition.segmentation.segments[i];
                    entry.is_enabled() && entry.component.applies_to_segment(&self.context, seg)
                })
                .collect();
            for (entry, applies) in self.filters.iter().zip(applicable) {
                if !applies {
                    continue;
                }
                entry.component.apply(&self.context, menu.candidates_mut());
            }
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
        Engine {
            schema: Schema::empty_for_test(),
            context: Context::new(),
            translators: Vec::new(),
            processors: Vec::new(),
            segmentors: Vec::new(),
            filters: Vec::new(),
            synthetic_queue: Vec::new(),
            build_dir: std::path::PathBuf::new(),
            data_roots: Vec::new(),
        }
    }
}
