# LRN vs 补丁版 librime 能力对照

这份表是 LRN 与补丁版 librime 的**唯一对照基准**。以后每次推进后只更新数字与状态，
章节结构与判定口径保持不变。

- **LRN**：`librime-next/`，Rust 重写，产出 ABI 兼容的 `librime_next.dll`
- **补丁版**：`librime/`，上游 C++ librime + `weasel-next.patch` 补丁集
- **出厂引用**：`Rime-Next/schema/` 下 124 个 yaml 中引用该名字的**文件数**。
  这个数字才是"真实影响面"，组件计数不是。

## 一、四层能力总览

| 层 | LRN | 补丁版 | 判定 |
|---|---|---|---|
| 输入组件 | 22 / 35 | 35 / 35 | 落后，缺 13 项；出厂方案实际只引用其中 3 项 |
| **整句组句（WordGraph + Poet）** | **无**，`ScriptTranslator` 只枚举前缀切分逐段查表出词级候选 | 有，`ScriptSyllabifier::BuildSyllableGraph` + `Poet::MakeSentences` | **落后**，输入 `nihaoshijie` 出不了「你好世界」整句 |
| 用户词典 UserDb 持久化 | 占位桩（`src/dict/user_db.rs` 仅一行注释） | LevelDB 实装，`%APPDATA%/Rime-Next/userdb/` 下 4 个 `.userdb` 在用 | **落后**，影响全部方案的学词与调频 |
| 语法模型 octagram grammar | 无（`src/lever/paths.rs` 只有 `.gram` 摆放规则） | 有，4 个 `.gram` 共约 470 MB 在用 | **落后**，整块功能缺失；**依赖上一行**，没有整句就没有可打分的候选序列 |
| TOML 解析 | 0 行（方案清单读 `schema.yaml`） | 手写 `toml_lite.cc` +259 行 | **领先**，补丁版这 259 行可归零 |

## 二、组件明细（35 项，含 2 个别名）

### processor（13）

| 组件 | LRN | 出厂引用 |
|---|---|---|
| `ascii_composer` | ✅ | 27 |
| `express_editor` | ✅ | 24 |
| `fluid_editor` | ✅ | 0 |
| `fluency_editor`（`FluidEditor` 别名） | ✅ | 0 |
| `key_binder` | ✅ | 28 |
| `navigator` | ✅ | 26 |
| `punctuator` | ✅ | 26 |
| `recognizer` | ✅ | 26 |
| `selector` | ✅ | 26 |
| `speller` | ✅ | 32 |
| `chord_composer` | ❌ | 0 |
| `shape_processor` | ❌ | 0 |
| `streaming_chord_processor` | ❌ | 0 |

### segmentor（6）

| 组件 | LRN | 出厂引用 |
|---|---|---|
| `abc_segmentor` | ✅ | 25 |
| `ascii_segmentor` | ✅ | 24 |
| `matcher` | ✅ | 24 |
| `punct_segmentor` | ✅ | 25 |
| `fallback_segmentor` | ✅ | 25 |
| `affix_segmentor` | ❌ | 6 |

### translator（9）

| 组件 | LRN | 出厂引用 |
|---|---|---|
| `punct_translator` | ✅ | 25 |
| `table_translator` | ✅ | 19 |
| `script_translator` | ✅ | 10 |
| `r10n_translator`（`ScriptTranslator` 别名） | ✅ | 0 |
| `reverse_lookup_translator` | ❌ | **18** |
| `history_translator` | ❌ | **12** |
| `echo_translator` | ❌ | 0 |
| `schema_list_translator` | ❌ | 0 |
| `switch_translator` | ❌ | 0 |

### filter（6）

| 组件 | LRN | 出厂引用 |
|---|---|---|
| `uniquifier` | ✅ | 22 |
| `simplifier` | ✅ | 20 |
| `single_char_filter` | ✅ | 2 |
| `charset_filter` | ❌ | 0（`enable_charset_filter` 是 option 名，不是组件） |
| `cjk_minifier`（`CharsetFilter` 别名） | ❌ | 0 |
| `reverse_lookup_filter` | ❌ | 0 |

### formatter（1）

| 组件 | LRN | 出厂引用 |
|---|---|---|
| `shape_formatter` | ❌ | 0 |

## 三、已确认的等价项（不是差距，不要重复追）

**lua 运行时**。补丁版产物 `rime.dll` 里**没有** lua 组件：对补丁版编译产物做
`strings` 符号检查，`lua_filter` / `lua_translator` / `lua_processor` 全部 0 命中，
只有 `.lua` 扩展名字符串和 input_stat 插件的 `lua_string` 符号。出厂 28 个方案的
`lua_filter`、11 个的 `lua_translator` 在补丁版里本来就走"组件创建失败 → 跳过"。
LRN 缺 lua 与补丁版**行为等价**，不列缺口。

**目录布局规则**。系列文件夹恒等式、`.bin`→`build/<系列>/`、`.gram`→`model/`
平铺、`.txt`→`eassy/`、零扁平回退。补丁版靠硬改 `resource.cc`(+88/-29) 与
`dict_compiler.cc`(+62/-6)；LRN 在 `src/lever/paths.rs` 单点建模。结论一致，实现路径不同。

**ABI**。`rime_get_api` 的 100 个函数指针一致，可直接换 dll。

## 四、待确认事项

补丁版 patch 快照（Oct 25）里方案清单仍写 `schema/schema.toml`（靠 `toml_lite.cc` 解析），
而 LRN 读 `<user>/schema/schema.yaml`。当前 `%APPDATA%/Rime-Next/schema/` 下**两个文件都在**。
需要核实补丁版 dll 的实际读取路径，避免换引擎时方案清单来源不一致。

## 五、重跑数据的命令

```bash
# 出厂引用文件数（对每个组件名跑一次）
cd /c/code/Weasel-Next/Rime-Next/schema
grep -rl '\b<组件名>\b' --include=*.yaml . | wc -l

# LRN 已注册组件
grep -nE '^\s+"[a-z_0-9]+" =>' /c/code/Weasel-Next/librime-next/src/engine/gear/registry.rs

# 补丁版产物实际支持的组件（符号检查）
D=/tmp/librime_backup_20261025/build-mingw/bin/librime.dll
strings -a "$D" | grep -cx '<组件名>'   # 1=有, 0=无

# C++ 侧全组件清单（ground truth）
grep -E 'r\.Register' /c/code/librime/src/rime/gear/gears_module.cc
```

## 六、推进顺序与变更记录

按表的工作顺序（依赖优先，其次影响面）：

1. **整句组句** `WordGraph` + `Poet` —— grammar 的前置；无此项则输入长词只能逐词选
2. octagram grammar —— 4 个方案直接依赖，470 MB 模型在用；挂在第 1 项之上
3. UserDb 持久化 —— 全部方案的学词/调频
4. `reverse_lookup_translator` + `reverse_lookup_filter` —— 18 个文件
5. `history_translator` —— 12 个文件
6. `affix_segmentor` —— 6 个文件
7. 其余 8 项（出厂 0 引用，按需再补）

| 日期 | 变更 |
|---|---|
| 2026-10-26 | 建表。基准：组件 22/35、整句缺、UserDb 缺、grammar 缺、TOML 领先。simplifier + opencc 已端到端验证（`66ab7ab`） |
| 2026-10-26 | 补「整句组句」一层：核查 `ScriptTranslator` 为纯词级，`Poet`/`WordGraph` 未移植，grammar 因此无落点。定为第 1 优先级 |
