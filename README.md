# librime-next

[librime](https://github.com/rime/librime) 的 **Rust 移植版** —— 目标是产出与原版
`rime.dll` **ABI 兼容**的替代品，让现有前端（Weasel / 小狼毫 RS）无需改一行头文件
就能把引擎换成这一份。

```text
C++ librime 的 src/rime/  ──►  librime-next/src/
        config/  algo/  dict/  lever/  gear/  rime_api.h
```

## 现状

| C++ 目录 | 行数 | Rust 模块 | 状态 |
|---|---|---|---|
| `src/rime/config/` | 2,516 | `config` | 已移植 |
| `src/rime/algo/` | 1,829 | `algo` | 已移植 |
| `src/rime/dict/` | 7,372 | `dict` | 已移植 |
| `src/rime/lever/` | 1,968 | `lever` | 已移植（11/11 部署任务） |
| `src/rime/gear/` | 9,560 | `engine::gear` | 已移植（MVP：Speller / Selector / ScriptTranslator） |
| `src/rime/`（顶层） | ~1,700 | `engine` | 已移植（MVP） |
| `src/rime_api.{h,cc}` | — | `c_api` | 已移植（MVP）：`rime_get_api()` 导出 `RimeApi`，字段逐字对齐 |

**测试**：258 个全过、0 失败（含用真实 Rime 数据的端到端用例）。

## 目录

```text
src/
  algo/      公式、编码器、文法规则、拼写器（纯算法）
  config/    YAML 装载 / 编译 / 合并（__include / __merge / __patch）
  dict/      prism / table / reverse / marisa 字符串表 / 词典门面
  engine/    Session / Context / Composition / Menu / gear 处理器
  lever/     部署层：11 个部署任务 + 路径分类 + 资源树
  c_api.rs   对外 ABI 边界（rime_api.h 的 Rust 版）
tool/
  abi_check/ 对照**原版** rime_api.h 的 ABI 验证器
example/    可运行用例（对外的是 library，这里演示怎么串起来）
test/       集成测试（见 Cargo.toml 的显式登记说明）
fixture/    真实编译产物快照，用来对拍
```

**目录名单数**：本仓库任何目录都不用复数（永远只有一个的文件夹叫复数名会造成语义错位）。
代价是 `test/` 与 `example/` 离开了 cargo 的默认约定目录名，必须在 `Cargo.toml` 里
逐个 [[test]] / [[example]] 登记。**新增集成测试/example 时别忘了同步登记**。

`src/` 下的目录结构**刻意与 C++ 的 `librime/src/rime/` 一一对应**，两边可以逐文件对照着读。

## 构建

```bash
cargo build --release          # 产出 target/release/librime_next.dll
cargo test                     # 全量测试
```

`Cargo.toml` 里 `crate-type = ["lib", "cdylib"]` —— cdylib 就是 rime.dll 替代品。

## ABI 验证（关键）

`tool/abi_check/dll_abi_check.c` **只 include C++ 仓库那份原版 `src/rime_api.h`**，
不引用本项目的任何头文件 —— 这样结构体布局、函数指针签名、导出符号是否与原版 rime.dll
兼容，就完全由运行时行为说了算，而不是由「自己抄了一份头文件」自证。

```bash
tool/abi_check/run_abi_check.sh          # 构建 cdylib + 编译验证器 + 跑一遍
```

它走的是宿主真实会走的调用链：

```text
LoadLibrary → rime_get_api → setup → initialize → create_session → select_schema
→ process_key("nihao") → get_context → select_candidate → get_commit → get_status
→ （部署模式）deployer_initialize → deploy → finalize
```

当前结果：11 项全过，`api->data_size = 804` 与 C++ `sizeof(RimeApi) - 4` 精确吻合，
候选首项为「你好」，提交文本为「你好」，`deploy()` 返回真。

### 踩过的 ABI 坑（值得记住）

* **结构体少一个字段就会让其后所有字段整体偏移。** `RimeApi` 曾漏掉 C++ 第 67 位的
  `config_create_map`，`data_size` 变成 796（原版期望 804），从该位往后的函数指针全部错位。
* **键码是 X11 keysym，不是 Windows 虚拟键。** 宿主把 Windows VK 转成 X11 keysym 再发过来
  （退格 `0xff08`、回车 `0xff0d`、方向键 `0xff51..0xff54`），可打印键取解码后的字符码。
  用 Windows VK 码会让退格/回车/方向键**永远不命中**。
* **修饰键位由 C++ `RimeModifier` 规定**：Shift=0 Lock=1 Control=2 Alt(Mod1)=3 …,
  Handled=24 Forward/Ignored=25 Super=26 Hyper=27 Meta=28 Release=30，mask=`0x5f001fff`。
* **拼音拼写 prism 的 `common_prefix_search` 返回「拼写 id」，而 `.table.bin` 的词条编码用
  「音节 id」**，两者必须经 `Prism::spelling_syllable_id()` 转换。漏了会查出错位的单字。
* **真实 `.prism.bin` 是 format 4.0**（不是 5.0）。

## 键名表

`src/engine/key_names.rs` 是**生成物**，由 `tool/abi_check/gen_key_table.py` 从 C++ 的
`src/rime/key_table.cc` 解析 `key_names[]` + `keys_by_name[]` 得到（1306 条 X11 keysym→名字）。
不要手改；改 C++ 那边的表，然后重跑生成脚本。

```bash
python tool/abi_check/gen_key_table.py <librime>/src/rime/key_table.cc \
                                        src/engine/key_names.rs
```

## 测试数据

* `fixture/build-20261023*` —— 真实 build 目录快照（30 份方案的编译产物），用于对拍。
* `test/e2e/build/` —— 扁平布局的真实词典二进制（`luna_pinyin` 的 table/prism/reverse），
  端到端用例「按键 → 分词 → 查词 → 候选 → 选择 → 提交」要用。

## 许可

BSD-3-Clause，与上游 librime 一致。移植代码保留上游版权声明。
