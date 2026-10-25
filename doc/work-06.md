# 06 时工作日志（2026-10-26）

## 本时段做成的事

**punctuator 三组件（05:20–06:10）**

`gear/punctuator.cc` 一个文件里装着三个组件，共用同一份 `PunctConfig`：

- `PunctSegmentor`：给"末尾那个标点字符"切一段，打 `punct` 标签
- `PunctTranslator`：按 `punctuator/half_shape|full_shape` 映射出候选
- `Punctuator`：吃掉标点键，负责轮换 / 成对 / 直接上屏

四种配置写法都支持：唯一映射（`-` → `－`）、轮换列表（`*` → `＊/·/×`，连按往后挪）、
`{commit: …}` 直接上屏、`{pair: [「,」]}` 按奇偶左右交替。真实方案的三个名单里
都列了它们，之前全部缺席。

**刻意没做**：原版一整套数字分隔符逻辑（`digit_separators` / `punct_number` /
`ConvertDigitSeparator` / `ReconvertDigitSeparatorAsPunct`）。它依赖
`Context::commit_history()`（「3.14」「1,000」靠"上一条提交是不是数字"判断），
lrn 目前没有 commit_history。影响：数字后紧跟 `.` `,` 时原版原样上屏，
lrn 走普通标点路径。

## 踩到的第二个"C++ 靠通知器、Rust 靠显式调用"的坑

`Punctuator` 入栈标点后要立刻判断"该不该上屏"，但 lrn 的 `compose` 在
`process_key` 末尾才跑，此刻段还是空的 —— 实测提交出**空串**。
C++ 里这一步由 `PushInput` 触发的 `update_notifier` 同步完成，所以那里不需要显式调用。

（第一处同类坑是 2026-10-25 的"打字后无组合段"。这类差异值得单列一条规律：
**移植 C++ 代码时凡是依赖 notifier 的地方，都要在 Rust 侧显式补一次调用**。）

## 数字

lib **296 → 303**；engine_e2e **7 → 8**（新增：真实方案敲 `,` 上屏 `，`）；
component_assembly 5/5；全量测试正在跑。

## 到此刻为止的组件清单（真实 luna_pinyin_simp 实测）

processor：ascii_composer / recognizer / key_binder / **speller** / punctuator /
selector / navigator / express_editor —— **只剩 lua_processor 缺席**

segmentor：matcher / abc_segmentor / punct_segmentor / fallback_segmentor
（还差 `ascii_segmentor`，未移植）

translator：punct_translator / script_translator
（还差 `table_translator@custom_phrase`、`reverse_lookup_translator`）

filter：uniquifier（还差 simplifier，它依赖 opencc 转换表）

## 剩下约 1 小时 20 分

不打算开 table_translator（1128 行，做不完只会留下半成品）。打算：
① 等全量测试结果；② 补 `ascii_segmentor`（约 60 行，形如 abc 但只认纯 ASCII 串）；
③ 07:00 写最后一份日志 + 收尾整理。
