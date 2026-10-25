# 04 时工作日志（2026-10-26）

## 本时段做成的事

**filter 链（03:15–04:00）—— 提交（本次第三个组件类别接进引擎）**

之前候选列表砍到 `menu.prepare()` 就完事，方案里声明的 `engine/filters` 没参与。
现在补齐：

- `gear/filter.rs`：`Filter` trait + `Uniquifier` + `SingleCharFilter`
- `runtime.rs`：新增 `filters` 字段与 `apply_filters()`，对应 C++ `TranslateSegments`
  里的 `menu->AddFilter(...)` 那段
- `registry.rs`：`create_filter` 工厂 + `engine/filters` 名单解析
- `menu.rs`：`candidates_mut()`，供 filter 就地改写列表

**写进注释的模型差异**：C++ 的 filter 接的是**懒式流**（一边 peek 一边吐），
lrn 是**急切列表**（排好序的 `Vec<Candidate>`），所以 `apply` 直接改写列表。
可见差别：uniquifier 在原版会把重复项挂在首个候选身上做成 `UniquifiedCandidate`
（候选数不变），lrn 直接删掉后出现的重复项（候选数变少）。对前端呈现一致。

一处刻意放宽：`SingleCharFilter` 原版只对 type 为 table/user_table 的 Phrase 生效，
lrn 现在只有 script 一种翻译器，照抄会恒等于空操作，先对所有候选生效。

**用实测守住"链真的被调用"**：真实 luna_pinyin_simp + 输入 "nihao"，
关掉 filter 链是 **294** 条候选，挂上 uniquifier 后 **293** 条 —— 正好删掉 1 条重复。
这个数字写进了 `test/component_assembly.rs` 的注释，断言"候选里不出重复词条"。

## 数字

lib 288 / component_assembly 5 / e2e 6 / key_order 3 / config_compiler 2 / algebra 8，全绿。

## 到这一时段为止，四类组件已全部动态装配

| 类别 | 配置节点 | 状态 |
|---|---|---|
| processor | `engine/processors` | ascii_composer / recognizer / speller / selector / navigator / editor |
| segmentor | `engine/segmentors` | matcher / abc / fallback |
| translator | `engine/translators` | script_translator（拼音） |
| filter | `engine/filters` | uniquifier / single_char |

未移植的（key_binder / punctuator / table_translator / simplifier / lua_*）一律
"跳过后继续"，不再让链条断掉。

## 踩到一个自己埋的坑

`Cargo.toml` 里那段注释正文含有 `[[example]]` 字样，脚本做字符串替换时把它当成
真实段落改坏了。提醒：**以后改 Cargo.toml 别用无约束的文本替换**。已修复。

## 下一个

`key_binder`（365 行 C++ + 依赖 `Switches`，开关/方案切换靠它）。它比 punctuator
更贴近日常手感，但因要移植 `switches.h/cc` 的 radio group 逻辑，不一定能在一个时段做完。
