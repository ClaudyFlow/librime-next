# 工作日志 13:39（2026-10-26）

## 本段目标
继续执行「把补丁版 librime 的功能移植到 librime-next，直到方案可真实出词」；上一段已落地
Ticket 命名空间语义、TranslatorOptions、动态组件装配，但 table_translator（形码）只写了文件、
未用真实数据验证。

## 完成内容

1. **table_translator 用真实形码数据验证通过**
   - 从已装 Rime 的 `%APPDATA%/Rime-Next/build/` 拷真实 `stroke.*.bin` / `wubi86.*.bin` 进
     `test/e2e/build/`（扁平布局，与 Dictionary::open 约定一致）。
   - 为 e2e 建最小忠实 schema（`wubi86.schema.yaml` / `stroke.schema.yaml`，去掉
     `import_preset` 以免预设缺失导致编译失败，只保留 table_translator 链路）。
   - 实测（具体输入输出）：
     - 五笔 `wq` → 候选含「你」；`vb` → 候选含「好」；选词提交文本恰为「你」/「好」。
     - 笔画 `h` → 候选含「一」；`hs` → 候选含「十」，且「一」作为前缀补全候选出现。
   - 关键纠正：本仓库 wubi86 词典**不含单字母简码**（`一`=ggl/ggll，不是 g），
     之前误判 g/d/k/l/r 为「空候选 bug」，实为这些码根本不在词典里——table_translator 行为正确。

2. **新增 4 个端到端断言测试**（`test/engine_e2e.rs`）：
   `table_translator_wubi86_produces_candidates`、`_stroke_produces_candidates`、
   `_wubi86_select_commit`、`_stroke_select_commit`，覆盖整码命中、前缀补全、选词提交。

3. **calculus.rs 行尾强制 LF+UTF8**：`Calculation` trait 加 `Send + Sync` 超特征（让
   Projection 能装箱进 TranslatorOptions），`.gitattributes` 已 `* text=auto eol=lf`，
   `core.autocrlf=false`，`git add --renormalize` 生效。

4. **全量测试绿**：`cargo test` 退出码 0，全部 `test result: ok`（lib 单测 + lever_deploy
   真实数据 243s + engine e2e 15 例全过）。

5. 更新 `doc/STATUS.md`：`engine/` 行把 table_translator 从「仍缺」改为「形码✅已验证」。

## 仍缺（与补丁版差距，非阻塞方案出词）
- simplifier 滤镜（简繁切换，依赖 opencc 转换表）
- lua_processor / lua_translator / lua_filter
- UserDb（用户词典读写）、commit_history
- `lever::deployment_tasks.rs` 的 shared→user 路径修正（614/898/973 行）待实施

## 下一步
方案已实现：拼音/双拼/五笔/笔画四类方案均能从真实词典出正确候选并提交。待用户决定
是否继续 simplifier / lua / UserDb，以及是否推远程 + 临时目录重新 clone 跑 build+test 验证。
