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
| 输入组件 | 25 / 35 | 35 / 35 | 落后，缺 10 项；出厂方案实际只引用其中 1 项（`affix_segmentor` 6） |
| **整句组句（WordGraph + Poet）** | **有**，`src/engine/gear/poet.rs` + `Dictionary::lookup_from_graph` | 有，`ScriptSyllabifier::BuildSyllableGraph` + `Poet::MakeSentences` | **已对齐**（`b1b0125`）。`nihaoshijie` → 「你好世界」；`nihao` → 「你好」（精确词条不组句） |
| 用户词典 UserDb 持久化 | **有**，`db.rs` + `level_db.rs`（rusty-leveldb）+ `user_db.rs` + `user_dictionary.rs`；`script_translator` 与 `table_translator` 两条学词链路都已打通，另补 `UnityTableEncoder` + `DictEntryIterator` | LevelDB 实装，`%APPDATA%/Rime-Next/userdb/` 下 4 个 `.userdb` 在用 | **已对齐**（拼音 + 形码双路） |
| 语法模型 octagram grammar | **有**，`gram_db.rs` + `grammar.rs`（Darts trie + 紧凑 n-gram 编码 + 五元打分） | 有，4 个 `.gram` 共约 470 MB 在用 | **已对齐**（未提交时）。`zhongguoren` → 「中國人」排第一 |
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
| `reverse_lookup_translator` | ✅ | **18** |
| `history_translator` | ✅ | **12** |
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
| `reverse_lookup_filter` | ✅ | 0 |

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

## 四、移植 grammar 的三条硬约束（踩过坑，写下来）

**1. `grammar/language` 必须与方案输出字形一致。** 繁体词条配 `zh-hans-*` 模型时，
编码后的键与模型里的键对不上，语法分全部退化成 `non_collocation_penalty`，表现为
"模型加载成功、排序毫无变化"。e2e 里 `luna_pinyin`（繁体词典）必须配
`zh-hant-t-essay-bgc`，期望输出是「中國人」而不是「中国人」。

**2. `.gram` 与 `opencc/` 不在同一个根下。** 出厂布局里 `model/` 跟**用户目录**、
`opencc/` 在**共享目录**。所以 `Engine` 持有的是 `data_roots: [user_data_dir, shared_data_dir]`
而不是单个 `build_dir.parent()`；`Octagram::load` 与 `Simplifier` 都遍历这个列表查找。

**3. `.gram` 的键是编码后的紧凑形式，不是明文 UTF-8。** `GramDb::lookup` 必须内部走
`grammar::encode`（`U+0000`→`0xE0`、ASCII 原样、`0x4000..0xA000`→两字节，
其余 `0xE0|n` + n×7bit）。第一版直接拿明文查 trie，5 组常见词全部 0 命中。

## 五、待确认事项

**`table_translator` 的学词闭环已补齐**（2026-10-26）。原先记的三处缺口全部落地：
`UnityTableEncoder`（`src/engine/gear/unity_table_encoder.rs`）、`TableTranslator::memorize`
的 `is_constructed(e)` 分支、表侧 `Dictionary::lookup_words` + `DictEntryIterator`。
e2e `table_translator_wubi86_learns_committed_word` 走完"打 `wq` → 选「你」→ 提交 →
`<user>/userdb/wubi86.userdb` 出现 `wq \t你` / `c=1` → 新会话以 `user_table` 身份排首"。

移植这条路径时踩到的三个坑，已写进代码注释，重做时别再犯：

1. **质量分是 `exp(weight)`，不是线性加权。** `table_translator.cc:311` 写的是
   `std::exp(e->weight) + initial_quality`。形码词表权重来自 `table_db` 的 1e8 倍率，
   `exp` 之后量级回落到个位数到百位；照抄拼音那套"音节数×1000+weight"会让所有形码候选
   挤在同一档、排序失效。
2. **chunk 排序 ≠ 权重排序。** `DictEntryIterator::AddChunk` 后做 `partial_sort`，
   比较序是"精确匹配优先 → `remaining_code` 短的优先 → 权重降序"（`dictionary.cc:182-198`）。
   只按权重排会让补全词压掉精确词。
3. **`TableEncoder` 在 C++ 里是自引用设计。** 它 `set_collector(this)` 即"自己是自己的收集器"，
   靠继承实现。Rust 做不出自引用，改成收集器走入参（`dfs_encode(collector, ...)`），
   编码器只负责公式/排除项/尾锚，收集器由调用方给（`BoundTableEncoder::new(collector)`）。

**反查（`reverse_lookup_*`）已补齐**（2026-10-26）。移植时踩到的五个点：

1. **一份输入、两份方向相反的词典。** 正查取 `reverse_lookup/dictionary`（`stroke`，
   码 → 字：`h` → 「一」），反查表取 `reverse_lookup/target`（默认 `translator`）→
   `translator/dictionary`（`luna_pinyin`）的 `.reverse.bin`（字 → 码：给候选补注释 `yi`）。
   过滤器反过来 —— 它读 `reverse_lookup/dictionary` **自己的**反查表。这不是笔误，
   两边要显示的"码"本来就不是同一套。
2. **`ReverseDb::open` 之后必须 `load()`。** 只 open 只是把文件映射进来，
   两个 trie 都是空的，`lookup` 恒返回 `None` —— 表现为"候选出得来、注释永远是空的"。
   C++ 对应 `ReverseLookupDictionary::Load()` 里的 `db_->Load()`。
3. **`enable_completion` 的默认值是反的。** C++ 写的是
   `if (!config->GetBool(ns + "/enable_completion", &enabled)) set_enable_completion(false);`
   —— `GetBool` 返回 false 表示**键不存在**，所以"没配"等于强制关，覆盖
   `TranslatorOptions` 里的默认 true。反查默认不做前缀补全。
4. **`Translator::query` 必须能写 `segment.prompt`。** C++ 在 `Query` 里
   `const_cast<Segment*>(&segment)->prompt = tips_`，好让输入区显示〔筆畫〕。
   LRN 原来只传 `(input, start, seg_tags)`，写不了段。签名改成
   `query(input, &mut Segment)` —— 顺带所有翻译器都能直接读 `segment.tags`，
   不再需要单独传一份 `seg_tags`。
5. **候选坐标必须相对本段。** `Engine::translate_segment` 统一加 `segment.start`。
   `table_translator` 与 `punct_translator` 原来传的是**绝对**位置，多段输入会被加两次
   （单段输入 `start == 0` 时看不出来，所以一直没暴露）。这次一并修正。

补丁版 patch 快照（Oct 25）里方案清单仍写 `schema/schema.toml`（靠 `toml_lite.cc` 解析），
而 LRN 读 `<user>/schema/schema.yaml`。当前 `%APPDATA%/Rime-Next/schema/` 下**两个文件都在**。
需要核实补丁版 dll 的实际读取路径，避免换引擎时方案清单来源不一致。

## 六、重跑数据的命令

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

## 七、推进顺序与变更记录

按表的工作顺序（依赖优先，其次影响面）：

1. ~~整句组句 `WordGraph` + `Poet`~~ ✅ `b1b0125`
2. ~~octagram grammar~~ ✅ `86aad8c`
3. ~~UserDb 持久化~~ ✅ 字典层 + `script_translator` 学词闭环 + `table_translator`
   学词闭环（`UnityTableEncoder` + `DictEntryIterator` + `lookup_words`）
4. ~~`reverse_lookup_translator` + `reverse_lookup_filter`~~ ✅ 18 个文件
5. ~~`history_translator`~~ ✅ 12 个文件（连带落地 `commit_history` 底盘）
6. `affix_segmentor` —— 6 个文件　**← 继续做**
7. 其余 9 项（出厂 0 引用，按需再补）

| 日期 | 变更 |
|---|---|
| 2026-10-26 | 建表。基准：组件 22/35、整句缺、UserDb 缺、grammar 缺、TOML 领先。simplifier + opencc 已端到端验证（`66ab7ab`） |
| 2026-10-26 | 整句组句 ✅（`b1b0125`）；grammar ✅（`86aad8c`）；UserDb 字典层 ✅（`019da51`） |
| 2026-10-26 | `script_translator` 学词闭环 ✅（`5387a9d`） |
| 2026-10-26 | `table_translator` 学词闭环 ✅：`unity_table_encoder.rs`(230行) + `dict_entry_iterator.rs`(492行) + `Dictionary::lookup_words` + `TableTranslator::query` 三路合流。质量公式改为 `exp(weight)`。全量 **434 passed / 0 failed** |
| 2026-10-26 | `reverse_lookup_translator` + `reverse_lookup_filter` ✅：组件 **22/35 → 24/35**，引用加权 **436/472 → 454/472（96%）**。顺带把 `Translator::query` 改成收 `&mut Segment`（要写 `prompt`），并修正 `table`/`punct` 候选坐标的二次偏移。全量 **450 passed / 0 failed** |
| 2026-10-26 | 补「整句组句」一层：核查 `ScriptTranslator` 为纯词级，`Poet`/`WordGraph` 未移植，grammar 因此无落点。定为第 1 优先级 |
| 2026-10-26 | 整句组句落地（`b1b0125`）：`poet.rs` 560 行 + `lookup_from_graph` + `match_extra_code`。实测 `nihaoshijie`→「你好世界」、`nihao`→「你好」、`choose(0)` commit 全文 |
| 2026-10-26 | grammar 落地：`gram_db.rs`（Darts + MappedFile）、`grammar.rs`（encode + Octagram 五元打分）、`data_roots` 架构。实测 `zhongguoren`→「中國人」；单测证明 `中国` 6.83 > `中果` -1.55 |
| 2026-10-26 | **UserDb 字典层落地**：`db.rs`（Db/DbAccessor/Transactional/Source/Sink）、`level_db.rs`（rusty-leveldb + 游标关进 Mutex）、`user_db.rs`（值格式 + `%g` 对齐 + 快照 + merger/importer）、`user_dictionary.rs`（DFS 查询 + 学词 + 事务）。实测：真实 `luna_pinyin.userdb` 读出 1257 条；真实 prism/table 上 `nihao` 捞出用户词「你好」，预测分支捞出「你好吗」 |
| 2026-10-26 | **学词闭环打通**：`Candidate.entry` 带词条 → `Context::collect_commit_elements` → `Engine::apply_commit` → `Translator::memorize` → `UserDictionary::UpdateEntry`。实测：`nihao` 选「你好」后 `<user>/userdb/luna_pinyin.userdb` 出现 `ni hao \t你好` / `c=1`；新会话再打 `nihao`，首候选 quality 99994（用户词典基数 1e5 + 对数权重），排在词表词条前面 |
| 2026-10-26 | `history_translator` ✅ + `Context::commit_history` 底盘：组件 **24/35 → 25/35**，引用加权 **454/472 → 466/472（99%）**。顺带解锁 `table_translator` 的 `encode_commit_history`（从历史里拼短语再编码）。`Translator::query` 加 `&CommitHistory` 入参、`memorize` 同理。全量 **469 passed / 0 failed** |
