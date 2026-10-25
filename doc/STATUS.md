# librime-next 现状总结

> 2026-10-25 实际部署验证后整理。验证方式：用真实源布局
> （`C:/code/Weasel-Next/Rime-Next`）跑单方案 `luna_pinyin_simp` 的
> `SchemaUpdate` 部署，与 `fixtures/build-20261023-current` 里的原版编译产物对比。

---

## 一、功能覆盖

| 层 | 状态 | 说明 |
|---|---|---|
| `config/` | ✅ 全移植 | YAML 装载/编译/合并（`__include`/`__merge`/`__patch`）、资源解析、`load_file`/`to_yaml_string` |
| `algo/` | ✅ | 公式、编码器、文法规则、拼写器（纯算法） |
| `dict/` | ✅ 表面全 | prism / table / reverse / marisa / DictCompiler 三件套产出 |
| `lever/` | ✅ 11/11 | installation_update / config_file_update / schema_update / workspace_update / cleanup_trash / prebuild_all_schemas / backup_config_files / detect_modifications / user_dict_upgrade / user_dict_sync / clean_old_log_files |
| `engine/` | ◐ 部分 | Session / Context / Composition / Menu + Speller + Selector + Translator；**缺** Recognizer / KeyBinder |
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

测试基线：`cargo test` → lib 238 + lever 8 + config 2 + engine e2e 6 + key_event 3 + 1，**258 过 / 0 败**。

---

## 二、与原版 librime 的区别

| 维度 | LR（C++） | LRN（Rust） |
|---|---|---|
| **语言** | C++（yaml-cpp / boost） | Rust（serde_json / 零依赖 YAML 解析） |
| **YAML 输出格式** | yaml-cpp `EmitScalar` 规则（流式映射、特定缩进） | 自己的 `to_yaml_string`（格式不同但语义等价） |
| **`__build_info`** | 编译产物里有 `__build_info/timestamps`（mtime 记录） | **没写**——产物比原版大 1,327 B（43,308 vs 41,981） |
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
| **`__build_info` 缺失** | 编译产物没写 `__build_info/timestamps`，`ConfigNeedsUpdate` 的 mtime 比对无法工作（增量更新失效，每次都全量重编译） |
| **YAML 输出格式不同** | 文本层面差异大（702 行 vs 425 行），语义层面基本等价（schema 配置、engine 组件列表一致） |

---

## 四、下一步优先级

1. **`__build_info` 写入** —— 让 `ConfigNeedsUpdate` 的 mtime 比对能工作，增量更新才有意义
2. **gear 补 Recognizer / KeyBinder** —— 让非拼音方案（五笔/双拼）能跑
3. **#20 组件移植** —— 让引擎能动态创建组件（替换 rime.dll 的最终闸门）
4. **#19 dict corrector** —— 纠错功能

---

## 五、验证方法

```bash
# 单方案部署（示例脚本）
cargo run --manifest-path librime-next/Cargo.toml --example deploy_luna

# 语义对比（我们的产物 vs fixtures 原版）
cargo run --manifest-path librime-next/Cargo.toml --example schema_diff <our.yaml> <fixture.yaml>

# 全量测试
cargo test --manifest-path librime-next/Cargo.toml

# ABI 检查（需先构建 cdylib）
cargo build --manifest-path librime-next/Cargo.toml
librime-next/tools/abi_check/run_abi_check.sh
```
