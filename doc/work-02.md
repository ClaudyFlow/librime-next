# 02 时工作日志（2026-10-26）

## 时间线

01:57 起步，本时段的主题是**去 TOML** 与**全局目录命名审计**。

## 做了什么

1. **目录命名审计（全仓库扫描，脚本比对）**：找出 3 组单复数并存
   —— `doc`/`docs`（仓库根，同一概念两个文件夹）、`example`/`examples`（lrn 已改、`plugin/core` 未改）、
   `test`/`tests`（lrn 已改，`Server`、`common`、`plugin/librime` 未改）。

2. **彻底移除 TOML**（用户拍板：toml 不好用）。
   lrn 里 TOML 的实际触点只有一处真家伙：`load_schema_ids_from_toml` +
   手写的 `parse_toml_string_array`/`collect_quoted`，读 `<user>/schema/schema.toml`
   （对应 C++ 补丁 `toml_lite.cc` 的 `LoadSchemaIdsFromToml`）。
   全部删掉，换成：

   - `paths.rs` 新增常量 `SCHEMA_MANIFEST = "schema.yaml"`
   - `deployment_tasks.rs` 新增 `SchemaManifest { active, schemas }` + `load_schema_manifest()`
     （走现有 YAML 装载，清单写法从 `schemas = [ "a" ]` 变成 `schemas:` 列表，**字段语义一字未改**）
   - 新增两个单测：YAML 清单能被读出、`schema.toml` 摆着也**读不进来**（钉死"不认 TOML"）
   - `c_api.rs` 两处候选路径 `schema/schema.toml` → `schema/schema.yaml`
   - 注释/文档里的 `schema.toml` 全部改掉

3. **一次性数据迁移**：把 `%APPDATA%\Rime-Next\schema\schema.toml` 的 12 项清单逐条搬成
   `schema/schema.yaml`（`active: double_pinyin_ziranma`）。** toml 原文件没删** ——
   装机跑的仍是补丁版 C++ 引擎，它读 toml，两边并存不冲突。正式发布时这份 yaml 该由安装器产出。

## 结果

lib 测试 **270 → 271 通过，0 失败**。全量测试正在跑（lever_deploy 约 4 分钟）。

## 未决/要你拍板

- `.theme.toml` 后缀仍在 `paths.rs`/`scan.rs` 的**摆放规则**里（会被判为"不匹配系列即错"）。
  但它不是被解析的对象 —— lrn 里**已经没有任何 TOML 解析器**。要不要连这个后缀一起改名
  （比如 `.theme.yaml`）属于数据格式迁移，归安装器。
- `sound.toml` / `theme.toml`（用户目录根）是 Weasel 前端自己的，不归引擎管。
