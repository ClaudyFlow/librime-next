# 05 时工作日志（2026-10-26）

## 本时段做成的事

**key_binder + Switches（04:00–05:15）**

补丁版 librime 的 `key_binder` 是日常手感最明显的组件（Ctrl+P/N/B/F/A/E、翻页、
开关切换、切方案），之前完全缺席。这次连它依赖的 `Switches` 一起移植：

- `engine/switches.rs`：`switches:` 清单读取 —— toggle 开关与**单选组**（互斥，如
  `zh_simp/zh_trad`），含 `OptionByName` / `ByIndex` / `Cycle` / `Reset`
- `gear/key_binder.rs`：绑定表（`when` + `accept` + `send` / `send_sequence` /
  `toggle` / `set_option` / `unset_option` / `select`）
- 配套：`KeyEvent::parse_sequence`、`Schema::from_config_for_test`、
  `Engine::{apply_schema, set_option}`

两个读代码才发现的细节：

1. **`when` 不是优先级，是"当前状态集合"**。同一键可绑多条，C++ 按
   predicting < paging < has_menu < composing < always 排序后取**第一条状态成立的**，
   所以 predicting 的绑定最优先。
2. 补丁版有一处特有补偿：`.` 常被判成 Page_Down，若后面紧跟字母（敲网址），
   要把 `.` 追补回输入串。这条也照抄了。

## 踩到并修掉的真坑（本时段最有价值的一条）

C++ 的 `ProcessSyntheticKey` 是**同步递归**回调处理器链的。Rust 侧做不到：遍历
`self.processors` 时为了把 `&mut self` 交给组件，必须把列表 `mem::take` 出来 ——
**嵌套调用看到的链表是空的**。

症状：真实方案里有 `{accept: "Control+h", send: BackSpace, when: composing}`，
喂 `nihao` + Ctrl+H，键被消费了（Accepted）但输入纹丝不动，仍是 `nihao`。
排查方式是给处理器链加临时打印，发现合成按键那轮根本没有处理器参与。

改成"组件登记按键 → 主循环结束后重放"，行为等价（原链在 key_binder 返回
Accepted 时就停了，不存在时序差别）。

## 数字

lib **288 → 296**；engine_e2e **6 → 7**（新增 Ctrl+H → BackSpace 端到端断言）；
component_assembly 5/5、key_order 3/3、config_compiler 2/2、algebra 8/8，0 失败。

## 下一个（剩约 2 小时）

`punctuator`（439 行）+ `punct_segmentor` + `punct_translator` —— 标点全半角；
真实方案的 `engine/processors` 与 `segmentors` 里都列了它。若时间不够，
则改为收尾：全量测试 + 整理最终工作日志。

`table_translator`（五笔/郑码/宇浩形码，1128 行）本轮大概率做不完，
它仍是"和补丁版效果一致"的最大单项缺口。
