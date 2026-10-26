# librime-next 现状总结

> 2026-10-25 实际部署验证后整理。验证方式：用真实源布局
> （`C:/code/Weasel-Next/Rime-Next`）跑全量 `deploy_workspace()`（19 方案 69 产物），
> 单方案 `luna_pinyin_simp` 的 `SchemaUpdate` 部署，并与 `fixture/build-20261023-current`
> 的原版编译产物做语义 diff。

---

## 一、功能覆盖

| 层 | 状态 | 说明 |
|---|---|---|
| `config/` | ✅ 全移植 | YAML 装载/编译/合并（`__include`/`__merge`/`__patch`）、资源解析、`load_file`/`to_yaml_string` |
| `algo/` | ✅ | 公式、编码器、文法规则、拼写器（纯算法） |
| `dict/` | ✅ 表面全 | prism / table / reverse / marisa / DictCompiler 三件套产出 |
| `lever/` | ✅ 11/11 | installation_update / config_file_update / schema_update / workspace_update / cleanup_trash / prebuild_all_schemas / backup_config_files / detect_modifications / user_dict_upgrade / user_dict_sync / clean_old_log_files |
| `engine/` | ◐ 部分（2026-10-26 大幅推进） | 四类组件**全部按配置动态装配**：processor（ascii_composer / recognizer / key_binder / speller / punctuator / selector / navigator / editor）、segmentor（ascii / matcher / abc / punct / fallback）、translator（punct / script / **table 形码✅已验证**）、filter（uniquifier / single_char / **simplifier 简繁✅已验证**）；compose 已是 C++ 的 `CalculateSegmentation` + `TranslateSegments` 真模型。**仍缺** lua_processor / lua_translator / lua_filter、UserDb、commit_history、reverse_lookup_translator / reverse_lookup_filter |
| `c_api` | ✅ ABI 闭环 | `rime_get_api()` 导出，`RimeApi` 字段与原版 `rime_api.h` 逐字对齐（`data_size=804` 精确吻合），11 项 ABI 检查全过 |

### 实测部署结果（luna_pinyin_simp）

单方案 `SchemaUpdate` **成功**，1.35s 内产出 4 个文件：

| 产物 | 大小 |
|---|---|
| `luna_pinyin.table.bin` | 1,271,984 B |
| `luna_pinyin.reverse.bin` | 252,792 B |
| `luna_pinyin_simp.prism.bin` | 34,292 B |
| `luna_pinyin_simp.schema.yaml` | 43,308 B |

`__include: luna_pinyin.schema:/` 的依赖解析也通了——自动编译了 luna_pinyin 的 table + reverse 三件套中的两件。

### 语义 diff 结果（我们的产物 vs 原版 `-current` fixture）

用 `schema_diff` 工具对 `luna_pinyin_simp.schema.yaml` 做递归 JSON 语义对比：

- 顶层 14 个 key 全部一致（custom_phrase / engine / filters / grammar / key_binder / menu / punctuator / recognizer / reverse_lookup / schema / simplifier / speller / switches / translator）
- **递归差异：0**（忽略 `__build_info`）
- translator 字段（dictionary / preedit_format / prism）逐字一致
- engine 组件数一致（filters 2 / processors 8 / segmentors 5 / translators 4）

结论：LRN config 层编译行为与 C++ 原版**语义完全等价**。

> 注：早期用 `build-20261023`（非 `-current`）fixture 对比时出现 punctuator 差异，
> 已查明是**数据演进**（fixture symbols mtime = 2026-10-09 旧版，当前 symbols.yaml mtime = 2026-10-16 新版），
> 新版 symbols.yaml 设计上把符号表也留在 punctuator.symbols 里供 recognizer 取用，非移植缺陷。

### 全量部署测试（deploy_workspace）

沙箱跑完整 `deploy_workspace()`，19 个方案全部成功，3分31秒产出 69 个文件、212MB：

- 方案目录 19 个（含 luna_pinyin / stroke / tiger_sentence / wubi86 / yuhao×2 / yuling×6 / yustar×7）
- 每个方案按系列文件夹布局：`build/<方案>/<方案>.{table,reverse,prism}.bin` + `<方案>.schema.yaml`
- `build/default.yaml` 同步写入
- 对比 fixture 基线（2026-10-23 的 15 方案 30 文件），当前数据目录已增至 19 方案，产物数量随之增加

测试基线：`cargo test` → lib 321 + lever 1 + config 2 + engine e2e 18 + key_order 3 + algebra 8 + doc 1（忽略），**全过 / 0 败**。

---

## 二、与原版 librime 的区别

| 维度 | LR（C++） | LRN（Rust） |
|---|---|---|
| **语言** | C++（yaml-cpp / boost） | Rust（serde_json / 零依赖 YAML 解析） |
| **YAML 输出格式** | yaml-cpp `EmitScalar` 规则（流式映射、特定缩进） | 自己的 `to_yaml_string`（格式不同但语义等价） |
| **`__build_info`** | 编译产物里有 `__build_info/timestamps`（mtime 记录） | ✅ 已写（`plugins.rs` 的 `BuildInfoPlugin`，`with_core_plugins()` 已装配；2026-10-25 复核实测产物尾部有 `__build_info`。早前"没写"系**过时记录**） |
| **目录布局** | 扁平 `build/<id>.bin` | **系列文件夹**：`build/<id>/<id>.bin`，id 点号 = 文件夹层级，零扁平回退 |
| **toml_lite** | upstream 已删（改用 ConfigItem API） | 自己实现极简 TOML 解析器读 `schema.toml` |
| **部署线程** | `std::async` 起工作线程 | **同步**执行（`StartWork` 跑完才返回） |
| **异常处理** | `catch(std::exception)` | `catch_unwind` |
| **组件注册** | `Registry` + `levers_module.cc` 注册 11 个任务 | 枚举 `DeploymentTask` + `from_name` 名字表 |
| **gear** | 完整 processors/segmentors/translators 链 | 只有 Speller + Selector（缺 Recognizer / KeyBinder / Punctuator 等） |
| **键码** | X11 keysym | X11 keysym（已对齐宿主 `Tip/src/keyboard.rs` 的约定） |

---

## 三、当前缺陷

### 🔴 硬闸门（不能替换 rime.dll 的原因）

| # | 缺陷 | 影响 |
|---|---|---|
| **#20** | 引擎侧组件移植：`SchemaComponent::Create` / `switcher_settings` / `customizer` / 组件注册 | 引擎无法动态创建/切换组件，目前靠 `config::compile` 一次性装配替代。宿主 deploy 模式要求组件可动态加载 |
| **#26** | gear 缺 Recognizer / KeyBinder | 非拼音方案（五笔/双拼/自定义编码）的识别链路不完整。luna_pinyin_simp 能跑是因为它的 engine 组件恰好都在 Speller+Selector 范围内 |

### 🟡 功能缺口（不阻塞替换，但缺功能）

| # | 缺陷 | 影响 |
|---|---|---|
| **#19** | dict corrector：`SymDeleteCollector::Collect` / `NearSearchCorrector::ToleranceSearch` | 纠错功能缺（输错拼音不会自动修正） |
| **UserDb** | `user_dict_manager` 只有接口，无实现 | 用户词库的读写/同步/升级全部空转（`user_dict_upgrade` 和 `user_dict_sync` 在没有组件时返回 true，等于没做事） |
| **SymlinkingPrebuiltDictionaries** | 未移植 | 出厂预编译词典的符号链接没做（`workspace_update` 里记日志跳过） |

### 🟢 与原版产物的差异（不致命但需注意）

| 差异 | 说明 |
|---|---|
| **~~`__build_info` 缺失~~（已澄清）** | 2026-10-25 复核实测产物**有** `__build_info/timestamps`（rime_version=1.17.0 + 全部资源 mtime），`ConfigNeedsUpdate` 的 mtime 比对链路完整；此条系早期误记。另注：装机实时基线（`%APPDATA%/Rime-Next/build/`）的 schema.yaml 反而**没有**它——那是装机版 C++（weasel-next.patch）行为，非标准 C++；LRN 比装机基线更对齐标准 C++ |
| **YAML 输出格式不同** | 文本层面差异大（702 行 vs 425 行），语义层面**完全等价**（schema 配置、engine 组件列表一致） |
| **prism 拼写总数 2432 vs 1792** | 部分部署实测比对（见第六节）：LRN 把 correction 拼写编入 prism（**对齐 C++ 原版 `prism.cc:242` 标记 `is_correction`**），装机实时基线用不含这些 correction 的旧源/配置部署（两边 `schema_checksum` 不同：LRN `0x011b50b8` / 基线 `0x29a1ad47`）。核心拼写集合等价（`common_prefix_search` 返回相同 spelling_id），功能无差异 |

---

## 四、下一步优先级

> 2026-10-25 复核：`__build_info` 实测已在写（见上），第 1 项完成；真实方案组件用量普查：
> processors 需 `ascii_composer/recognizer/key_binder/speller/punctuator/navigator/selector/express_editor`、
> segmentors 需 `fallback/ascii/abc/punct/matcher`、translators 需
> `punct/table/script/reverse_lookup`、filters 需 `uniquifier/simplifier/single_char`（30 个方案去重统计）。

1. ~~**`__build_info` 写入**~~ ✅ 已在写（此前记录有误）
2. **#20 组件机制 + gear 补齐** —— Registry/动态装配是底座，然后按上面的组件清单逐个移植（Recognizer / KeyBinder 优先）
3. **#19 dict corrector** —— 纠错功能

---

## 五、验证方法

```bash
# 单方案部署（示例脚本）
cargo run --manifest-path librime-next/Cargo.toml --example deploy_luna

# 语义对比（我们的产物 vs fixture 原版）
cargo run --manifest-path librime-next/Cargo.toml --example schema_diff <our.yaml> <fixture.yaml>

# 全量测试
cargo test --manifest-path librime-next/Cargo.toml

# ABI 检查（需先构建 cdylib）
cargo build --manifest-path librime-next/Cargo.toml
librime-next/tool/abi_check/run_abi_check.sh
```

---

## 六、部分部署实测对比（2026-10-25 晚）

**目的**：用真实 dll（`target/debug/librime_next.dll` = librime-next 的 cdylib，已确认导出 `rime_get_api`、`data_size=804`）走 C ABI 做部分部署，产物与用户文件夹现有 C++ 基线（`%APPDATA%/Rime-Next/build/luna_pinyin_simp/`）逐文件对比。

**方法**：`tool/abi_check/partial_deploy.c` 加载 dll → `setup` → `deployer_initialize` → `deploy` → `finalize`，沙箱 user_dir 只列 `luna_pinyin_simp`。实测 install 阶段用 shared 的 `default.yaml` 覆盖了沙箱配置，触发**全量部署**（19 方案 / 24s 完成），但聚焦对比 luna_pinyin_simp 部分。

### 结果

| 文件 | LRN 最新 dll 部署 | C++ 实时基线 | 结论 |
|---|---|---|---|
| 文件结构 | `luna_pinyin_simp/` 下 `prism.bin` + `schema.yaml` | 同（无 table/reverse） | **一致** |
| `luna_pinyin_simp.schema.yaml` | 43308 B | 42918 B（差 390 B） | 语义 **完全等价**，差异仅 `__build_info` 段（LRN 有 / 基线无） |
| `luna_pinyin_simp.prism.bin` | 34292 B / 2432 拼写 | 31732 B / 1792 拼写 | 字节不同；LRN 多 **640 个 correction 拼写** |

### prism 差异根因

- C++ 原版 `Prism::Build`（`src/rime/dict/prism.cc:142-256`）**不跳过 correction 拼写**——把 script 里所有拼写（含 `is_correction` 的）都编入 trie，并在 `:242` 标记 `kTypeIsCorrectionMask`。
- LRN 的 `Prism::build`（`src/dict/prism.rs:391-505`）行为对齐：把源 schema speller 的 `pinyin:/spelling_correction` + `pinyin:/key_correction` 生成的 correction 拼写也编入 → 2432 个。
- 对用户文件夹实时基线（1792）而言，它部署时所用的源/配置**不含这些 correction**（两边 `schema_checksum` 不同：`0x011b50b8` vs `0x29a1ad47` 即证）。属**源演进 / 装机配置差**，非代码缺陷。
- 功能等价铁证：相同输入 `common_prefix_search` 返回**完全相同的 spelling_id**（`nihao→[349,365]`、`zhong→[639,652,667,670,671]` 等）。

### 结论

luna_pinyin_simp 的部分部署产物与 C++ 实时基线**语义完全等价**；prism 的字节差来自 correction 拼写（LRN 对齐 C++ 原版行为，基线对齐旧装机配置），**无需代码修复**。

### ⚠️ 阻塞：lib 当前无法编译

对比完成后发现 `git status` 显示 `src/engine/` 有 **884 行未提交改动**（composition/context/speller/menu/schema/segmentation + 新 `gear/segmentors.rs`），这些是本日"engine 部分移植"的进行中代码，**从未编译验证**，导致 `cargo build` 整库失败（`Context::move_highlight`/`commit_composition`、`Segmentation::segment_zero`、`Regex::is_full_match` 等未实现）。18:09 部署用的 dll 靠旧 target 缓存编过；任何新的 `cargo build`（含改 prism.rs 后重编）都会失败。需先决定：修复使可编译 / 回退 / 保持现状。

---

## 七、engine 阻塞修复 + 扫描器镜像 + 全量测试通过（2026-10-25 深夜）

**engine 19 个编译错误全部修复**（`cargo build` 产出 `librime_next.dll`，58.8MB）：

| 缺失 | 修复方式 |
|---|---|
| `Context::move_highlight` / `commit_composition` / `menu_candidate_count` / `Segmentation::segment_zero` | 从 HEAD 取回实现并按当前结构适配（`committed_prefix` 字段恢复；`highlight()` 方法化） |
| `Context::select` 返回 `Option<SelectResult>` | 恢复"整段覆盖→Committed / 部分覆盖→Continued"语义 |
| `Speller` 当值用 | `Engine::create` 改为从 schema 编译后配置 `Speller::new(&schema.config)` 装配 |
| `Regex::is_full_match` | fancy_regex 无此方法；构造时加 `^(?:…)$` 锚点 + `regex_full_match` helper |
| `compose()` 从未被触发（e2e 全挂的根因） | `Engine::process_key` 补"输入变更才重组"——对齐 C++ `update_notifier → OnContextUpdate → Compose`；高亮导航不变更输入故不重组（符合 C++ 行为） |

**扫描器镜像进 lrn**：`src/lever/scan.rs`（`tool/rime-scan` 算法正式化，serde_norway 解析 frontmatter，路径用 `paths` 常量）。实测与工具基线**逐位一致**：171 文件（schema 41 / dict 79 / config 7 / theme 1 / lua 37 / plugin-data 4 / essay 2）、120 个 yaml 系列摆放 0 误报、OVERALL 4,733,605 词条 avg 9.41 max 84。

**测试**：lib 243（含 scan 5）+ lever_deploy 集成（真实数据目录全量 WorkspaceUpdate，231s）+ e2e 6 + 其余 14，**全过**。

**架构修正**：`Deployer::layout()` 不再强行覆盖 staging/prebuilt，改为忠实反映字段；硬规则执行点移到注入侧（`rime_deployer_initialize` 强制 staging=`<user>/build`、prebuilt 恒空；`set_user_data_dir` 同步重算 staging）。修复了 lever_deploy 测试因 staging 覆盖失效而写穿真实 build 目录的问题。

**遗留**：真实 build 目录被失败测试运行污染（00:02-00:05 写入的 lrn 编译产物，语义等价）；待一次 C++ 真实部署恢复一致。

---

## 八、#20 组件机制第一批：动态装配 + processor 链（2026-10-26 凌晨）

**这是能不能替换 `rime.dll` 的硬闸门之一。** C++ `ConcreteEngine::InitializeComponents`
（`engine.cc:549-575`）读方案里的四个列表（`engine/processors` / `segmentors` /
`translators` / `filters`）逐个从全局 Registry 创建；本批把它移植成**按名单动态装配**，
processor 链不再是硬编码的 `Speller + Selector`。

| 新增文件 | 对照 C++ | 说明 |
|---|---|---|
| `src/engine/gear/registry.rs` | `registry.h` + `gears_module.cc` + `engine.cc` 的 `Condition` | `Ticket` / `Condition`（`all_of` `any_of` `none_of` `!option`）/ `ComponentEntry` / `create_components_from_list` / 组件工厂 |
| `src/engine/gear/key_binding.rs` | `gear/key_binding_processor.h` + `_impl.h` | 绑定表 + **精确匹配** + Shift 兜底 |
| `src/engine/gear/ascii_composer.rs` | `gear/ascii_composer.cc`（装机补丁版） | 中西文切换 / 单击 Shift / 临时西文 / 大写字母进入 inline ascii |
| `src/engine/gear/editor.rs` | `gear/editor.cc` | `express_editor`（默认）/ `fluid_editor`：空格确认、回车上屏原始串、Esc 取消、退格回退 |
| `src/engine/gear/navigator.rs` | `gear/navigator.cc` | 插入符导航：左右 / Home / End / Ctrl+左右按音节跳 |

### 两个容易踩错的 C++ 语义（实测对齐）

1. **绑定查表是精确匹配，不是掩码匹配**。`KeyEvent::operator==`（`key_event.h:43-45`）
   比的是**完整** `(keycode, modifier)`；`kModifierMask` 只用于 `repr()`。
   后果：写 `- {accept: Return}` 的绑定**不会**被 `Release+Return` 命中
   （`kReleaseMask` 是 bit 30，恰好在 `kModifierMask` 里）。第一版实现按掩码查表，
   测试没过才发现。
2. **Shift 有两次兜底**（`key_binding_processor_impl.h:22-40`）：精确匹配失败后，
   带 Shift 时先试"把 Shift 换成 Control"，再试"去掉 Shift"；**Ctrl/Alt 按下则完全不兜底**。
   Editor / Navigator 传 `All`，AsciiComposer 传 `None`。

### 没移植的组件不会被吞掉

工厂对未移植的 klass 返回 `None`，行为等同 C++ `Require` 失败：**打日志 + 跳过该项**，
链条继续。所以列表里的 `recognizer` / `key_binder` / `punctuator` 现在只是缺席，
不会像以前那样整条 processor 链退化为硬编码的 `Speller+Selector`。

### 验证

- 新增测试 `test/component_assembly.rs`：用真实 `luna_pinyin_simp` 产物断言
  `processor_names()` 含 ascii_composer / speller / express_editor / navigator / selector，
  **且顺序来自配置**（ascii_composer 在 speller 之前）；translator 从 `engine/translators`
  产出 script_translator。
- lib 测试 243 → **270**；engine_e2e 6/6、key_order 3/3、config_compiler 2/2、
  algebra_real_rules 8/8、lever_deploy 1（真实目录全量 WorkspaceUpdate）。

### 第二批待办（按依赖顺序）

1. `recognizer`（104 行 C++）：`patterns` → 段 tag，几乎所有方案都列了它
2. `key_binder`（365 行）：开关切换 / 按键转发，简繁切换与方案切换靠它
3. `punctuator`（439 行）：标点全半角 `Opunct` 映射
4. `table_translator`（+1128 行）：五笔 / 郑码 / 宇浩这类**形码**方案目前完全跑不了
5. `segmentors` 链 + `engine/segmentors` 装配（`CalculateSegmentation` 的 `Proceed` 循环）
6. `filters`：`uniquifier`（去重）/ `simplifier`（简繁）/ `single_char_filter`
7. #19 dict corrector：`SymDeleteCollector::Collect` / `NearSearchCorrector::ToleranceSearch`
8. UserDb：用户词典读写同步（`user_dict_manager` 目前只有接口）

## 九、目录命名：强制单数，禁止缩写（2026-10-26）

**规则**（用户拍板）：本仓库任何目录名**一律单数**——"永远只有一个的文件夹叫复数名，
会造成语义错位和识别困难"；缩写只允许 `src` / `doc` 这类极通用的，其余不用。

### 本次改动

| 旧 | 新 | 类型 |
|---|---|---|
| `examples/` | `example/` | 复数 → 单数 |
| `fixtures/` | `fixture/` | 复数 → 单数 |
| `tools/` | `tool/` | 复数 → 单数（与宿主仓库根部的 `tool/` 对齐） |
| `tests/` | `test/` | 复数 → 单数 |

连带更新 15 个文件 / 30 处路径引用（`README.md`、`doc/STATUS.md`、源码注释、
`test/*` 里的 `join("test/e2e/build")`、`tool/abi_check/` 下的脚本）。

### 代价：`test/` 与 `example/` 必须在 Cargo.toml 里显式登记

离开 cargo 约定目录名后自动发现失效，`Cargo.toml` 里逐条写了 `[[test]]` / `[[example]]`。
**风险**：新增集成测试或 example 时，除了放文件还要同步加一段；
漏加的话 cargo **不报错、静默跳过**。这是本次改名唯一的持续成本。

### 已知冲突（改不动或建议保留，需用户定夺）

- `mod tests`（`src/**` 里的单元测试模块）：Rust 生态惯用名，改了不影响编译但偏离工具链约定。
- `target/`：cargo 输出目录，不可改。
- `test/e2e`：`e2e` 是缩写（end-to-end），与规则冲突；业界标准写法，改成 `end_to_end` 可行但更长。
- `sync/`：已从仓库根部清掉——它不是源码目录，而是默认 `Deployer`
  （`user_data_dir="."`、`sync_dir="sync"` 为**相对路径**）跑同步任务时写进 CWD 的产物。
  **真问题**：默认构造用相对路径会污染调用方当前目录，建议宿主必须注入数据目录后才允许跑同步任务。
- 宿主 `Rime-Next/` 数据目录侧仍是 `schema` / `model` / `eassy` / `theme` / `plugin`
  （`eassy` 是 essay 的历史拼写错误），它们已被装机版 C++ 补丁写死，改名属于安装器迁移，不在本次范围。

---

## 十、simplifier + opencc 最小移植（2026-10-26）

**目标**：让 `luna_pinyin_simp` 的简繁转换过滤器真正在方案链路里工作（此前 filters 只有 uniquifier / single_char）。

### 新增文件

| 文件 | 对照 C++ | 说明 |
|---|---|---|
| `src/dict/opencc.rs` | `opencc/` 字典加载 + `SerializedValues` | 最小 OpenCC 引擎：读 `.ocd2`（Marisa trie + 值区）+ `*.json` 的 `conversion_chain`（含 `group` 嵌套），最长匹配转换 |
| `src/engine/gear/simplifier.rs` | `gear/simplifier.cc` | 简繁转换 filter：`opencc_config`（默认 `t2s.json`）/ `option_name`（配了就只在开关打开时转） |

### ocd2 二进制格式破译（关键坑）

`.ocd2` = 定长 19 字节 magic `OPENCC_MARISA_0.2.5`（**不带** null 结尾）+ Marisa trie
（自带 `"We love Marisa.\0"` 头）+ `SerializedValues`（`u32 numItems` → `u32 valueTotalLength`
→ null 结尾值缓冲 → `numItems` 个描述符，每条 `u16 numValues` + 每值 `u16` 长度）。

两个非显然点：
1. **rsmarisa 的 `io_size()` 与实际写出字节有偏差**，不能用它切分 trie/值区边界 →
   用 `CountingReader(Rc<AtomicUsize>)` 包装 `io::Read` 精确计数 trie 消耗字节。
2. **值缓冲要累加条目内所有值的长度**（多 variant 场景），不能只累加第一个，否则后续偏移整体错位（汉→乱码）。

### Filter 调用链升级

`Filter` trait 签名从 `apply(&self, candidates)` 升级为 `applies_to_segment(&self, ctx, seg) -> bool`
（开关门控）+ `apply(&self, ctx, candidates)`（只转换）。`Runtime::apply_filters` 改为：
先 `seg.menu.take()` 拿所有权打破借用别名 → 只读 `ctx` 算每段的 `applicable` → 仅对命中的段调 `apply`
（对齐 C++ runtime 先问开关再调 apply 的语义）。`registry::create_filter` 增加
`"simplifier" => Simplifier::new(build_dir.parent()/opencc, ...)`，配置节点取 `alias:` 块回退 `/simplifier`。

### 验证（端到端，真实方案链路）

`luna_pinyin_simp` 的 filters 实际是 `simplifier@zh_hans`（option_name=zh_hans，
opencc_config=t2s.json，开关 `reset:1` 默认开）/ `@zh_hant_hk` / `@zh_hant_tw`（均带 opencc_config）。
e2e 新测：`type_ascii("yuyan")` 后——
- `zh_hans=ON`  → 候选 `["语言","预言","寓言","雨燕",...]`（简体）
- `zh_hans=OFF` → 候选 `["語言","預言","寓言","雨燕","預演","浴鹽",...]`（繁体）

3 个断言测试全过，证明 simplifier 在真实引擎链路里生效且受开关门控。
（测试用 opencc 数据放 `test/e2e/opencc`，1.2M，与 build 真实数据同源。）

### 仍缺（同一缺口清单顺延）

`reverse_lookup_translator` + `reverse_lookup_filter`、`UserDb` 持久化（替换 `InMemoryUserDb` 桩）、
`charset_filter` + `commit_history`（cangjie5）、`collapse_composition` / `cjk_minifier` /
`echo_translator` / `chord_composer`、`lua_*` / `grammar` / `SchemaSwitcher`。
（e2e 里 `table_translator@custom_phrase` / `reverse_lookup_translator` 创建报错是**预期**——
未移植组件按设计返回 `None`、链条继续，不影响主线。）
