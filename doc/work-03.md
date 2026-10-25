# 03 时工作日志（2026-10-26）

## 本时段做成的事

**1. 彻底移除 TOML（01:57–02:10）—— 提交 `f92ae15`**

lrn 里 TOML 只有一个真触点：读 `<user>/schema/schema.toml` 的点名制方案清单
（对应补丁 C++ 的 `toml_lite.cc::LoadSchemaIdsFromToml`）和为它手写的极简解析器。
整段删掉，换成 `SchemaManifest` + `load_schema_manifest()`，读 `<user>/schema/schema.yaml`
（`paths.rs` 新增常量 `SCHEMA_MANIFEST`）。写了两个新单测，其中一个专门钉死
"磁盘上摆着 schema.toml 也读不进来"。

顺带把 `%APPDATA%\Rime-Next\schema\schema.toml` 里的 12 项清单迁移成了
`schema.yaml`（**toml 没删**——装机跑的仍是补丁版 C++ 引擎，它读 toml，两边并存不冲突）。

**2. 分段框架（02:10–03:15）—— 提交 `ad…`（本时段最重要的进展）**

之前 `compose()` 是"整段输入一个大段"的简化模型，`engine/segmentors` 完全没参与。
现在照 C++ `ConcreteEngine` 拆对了：

- `Segmentor` trait + `abc_segmentor` / `matcher` / `fallback_segmentor`
- `compose()` 按 C++ 写：caret 感知、`reset` 差量保留已确认段
- `calculate_segmentation()`：`while(!finished)` → 逐个 segmentor → `Forward()`
- `translate_segments()`：逐段取子串喂 translator，候选偏移回绝对位置
- `Recognizer` processor（放行 `@` 这类不在 alphabet 里的键）

踩到的两个 C++ 细节：模式表是 `std::map`（按 tag **字典序**遍历，顺序影响谁先命中）；
`Matcher` 的 `pop_back` 循环每轮要重取当前起点，不能快照。

## 数字

lib 测试 **271 → 286**；e2e 6/6、component_assembly 3/3（新增"多段切分"断言）、
key_order 3/3、config_compiler 2/2、algebra 8/8 —— 全是真实 luna_pinyin_simp 产物跑的。

## 下一个

`key_binder`（365 行 C++，开关与方案切换靠它）→ `punctuator`（439 行，标点全半角）。
这两个是"打字立刻能感觉到"的剩余缺口；`table_translator`（形码）排在它们后面，
因为它更大且此刻五笔类方案仍完全跑不了。

## 备注

- 到目前为止，lrn 与补丁版 librime 的差距主要在：**没有 table_translator / 没有 filter 链 /
  没有 punctuator / 没有 UserDb**。 disk Huffman hard-coded 缺省回放这次没碰到。
- 未移植组件继续按"跳过而不是断链"处理，因此引擎行为降级但不崩。
