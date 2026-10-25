# 07 时工作日志（2026-10-26，本夜收尾）

## 本时段

**ascii_segmentor（06:10–06:35）**：C++ 只有 30 行的组件 —— 开着 `ascii_mode`
时把从当前起点到串尾整段切成一段、打 `raw` 标签，然后 `return false` 结束本轮。
真实方案的 `engine/segmentors` 里第一个就是它。

顺带给 `Segmentor::proceed` 加了 `SegmentorView` 参数。原因：C++ 的 segmentor 持有
`Engine*` 能直接读 `ctx->get_option("ascii_mode")`，lrn 的 segmentor 列表在
`calculate_segmentation` 里被 `mem::take` 出来遍历，此时拿不到 `&Engine`
（会和 `&mut segmentation` 打架）—— 只把需要的开关挑出来传进去。

## 全夜成果（01:57 → 07:00）

| 时段 | 提交 | 内容 |
|---|---|---|
| 02 | `f92ae15` | 彻底移除 TOML：方案清单改 `<user>/schema/schema.yaml`，并做了一次性数据迁移 |
| 03 | `9b925c1` | 分段框架：Segmentor 链 + `CalculateSegmentation` + 逐段翻译 + recognizer |
| 04 | （filter） | filter 链：`Uniquifier` / `SingleCharFilter`，实测删掉 1 条重复候选 |
| 05 | （key_binder） | key_binder + Switches；修掉"合成按键看不到处理器链"的坑 |
| 06 | `97949e1` | punctuator 三组件（标点全半角 / 轮换 / 成对 / 直接上屏） |
| 07 | （ascii_segmentor） | 西文模式整段 raw |

**测试：lib 271 → 306，全量 333 通过 / 0 失败**（含真实 luna_pinyin_simp 的
端到端：nihao→你好、Ctrl+H→退格、`,`→`，`、多段切分、uniquifier 去重）。

## 定型的一条规律

C++ 靠**通知器**串起来的地方（`PushInput` → `update_notifier` → `Compose`），
Rust 侧必须**显式补一次调用**。本夜踩到两处：标点上屏时看到空段（提交空串）、
以及上一夜的"打字后无组合段"。下一处大概率是 UserDb / commit_history。

## 仍缺（按影响排序）

1. **table_translator** —— 五笔 / 郑码 / 宇浩仍完全跑不了（最大单项）
2. **simplifier** 等依赖转换表的 filter
3. **lua_processor / lua_translator / lua_filter**
4. **UserDb**（用户词典读写与同步，只有布局）
5. **commit_history**（数字分隔符逻辑卡在它上面）
6. punctuator 的数字分隔符部分（依赖 5）

## 待你拍板

- `.theme.toml` 后缀要不要一起改成 `.theme.yaml`（属数据格式迁移，归安装器）
- 宿主仓库的单复数重复目录：`doc`/`docs`、`Server/tests`、`common/tests`、
  `plugin/core/examples`、`plugin/librime/tests`、`artifact/`（67 个文件**完全未被 git 跟踪**）
- `test/e2e`：e2e 是缩写，与"禁缩写"规则冲突，改 `end_to_end` 还是保留
