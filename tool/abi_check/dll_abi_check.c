/*
 * librime-next 的 **rime.dll 替代能力验证器**。
 *
 * 它刻意只依赖 C++ 仓库里那份**原版** `src/rime_api.h`，不引用 librime-next 任何头文件：
 * 这样「结构体布局 / 函数指针签名 / 导出符号」三者是否与原版 rime.dll 兼容，
 * 就完全由运行时行为说了算，而不是由「我们自己抄了一份头文件」自证。
 *
 * 走的是宿主（Weasel / 任何 rime 客户端）真实会走的调用链：
 *   LoadLibrary → rime_get_api → setup → initialize → create_session
 *   → select_schema → process_key("nihao") → get_context(看候选)
 *   → select_candidate → get_commit(看提交)
 *
 * 用法：
 *   dll_abi_check <librime_next.dll> <shared_data_dir> <user_data_dir> [schema_id] [staging_dir]
 * 退出码：0 = 全部通过；1 = 有检查失败。
 */
#include <stdio.h>
#include <stdarg.h>
#include <string.h>
#include <stdlib.h>
#include <windows.h>

#include "rime_api.h"

typedef RimeApi* (*get_api_fn)(void);

static int g_failed = 0;

static void check(int cond, const char* fmt, ...) {
  va_list ap;
  va_start(ap, fmt);
  if (cond) {
    printf("  [ OK ] ");
  } else {
    printf("  [FAIL] ");
    g_failed = 1;
  }
  vprintf(fmt, ap);
  printf("\n");
  va_end(ap);
}

/* 逐字符喂按键，返回是否全部被消费 */
static int type_text(RimeApi* api, RimeSessionId sid, const char* text) {
  int all = 1;
  for (const char* p = text; *p; ++p) {
    Bool ok = api->process_key(sid, (int)(unsigned char)*p, 0);
    printf("  [key ] process_key('%c') -> %d\n", *p, (int)ok);
    if (!ok) all = 0;
  }
  return all;
}

int main(int argc, char** argv) {
  if (argc < 4) {
    fprintf(stderr,
            "usage: %s <librime_next.dll> <shared_data_dir> <user_data_dir> [schema_id]\n",
            argv[0]);
    return 2;
  }
  const char* dll_path = argv[1];
  const char* shared_data_dir = argv[2];
  const char* user_data_dir = argv[3];
  const char* schema_id = (argc > 4) ? argv[4] : "luna_pinyin_simp";
  const char* staging_dir = (argc > 5) ? argv[5] : "";

  printf("== 加载 DLL ==\n");
  HMODULE h = LoadLibraryA(dll_path);
  if (!h) {
    fprintf(stderr, "LoadLibraryA(\"%s\") 失败，GetLastError=%lu\n", dll_path,
            GetLastError());
    return 1;
  }
  printf("  LoadLibraryA(\"%s\") = %p\n", dll_path, (void*)h);

  get_api_fn get_api = (get_api_fn)GetProcAddress(h, "rime_get_api");
  if (!get_api) {
    fprintf(stderr, "GetProcAddress(\"rime_get_api\") 失败，GetLastError=%lu\n",
            GetLastError());
    return 1;
  }
  printf("  GetProcAddress(\"rime_get_api\") = %p\n", (void*)get_api);

  RimeApi* api = get_api();
  if (!api) {
    fprintf(stderr, "rime_get_api() 返回 NULL\n");
    return 1;
  }
  printf("  rime_get_api() -> %p\n", (void*)api);
  printf("  api->data_size = %d, sizeof(RimeApi) = %zu\n", api->data_size,
         sizeof(RimeApi));

  /* ---------------------------------------------------------------- 初始化 */
  printf("\n== setup / initialize ==\n");
  RimeTraits traits = {0};
  traits.data_size = sizeof(RimeTraits) - sizeof(traits.data_size);
  traits.shared_data_dir = shared_data_dir;
  traits.user_data_dir = user_data_dir;
  traits.distribution_name = "librime-next";
  traits.distribution_code_name = "librime-next";
  traits.distribution_version = "0.1.0";
  traits.app_name = "rime.abi_check";
  traits.modules = NULL;
  traits.min_log_level = 3;
  traits.log_dir = "";
  if (staging_dir[0]) {
    traits.staging_dir = staging_dir;
    printf("  traits.staging_dir = \"%s\"\n", staging_dir);
  }

  api->setup(&traits);
  api->initialize(&traits);
  printf("  setup() + initialize() 完成\n");

  /* ------------------------------------------------------------ 建会话 / 选方案 */
  printf("\n== create_session / select_schema ==\n");
  RimeSessionId sid = api->create_session();
  check(sid != 0, "create_session() -> session id %zu", (size_t)sid);
  if (!sid) return 1;

  Bool sel = api->select_schema(sid, schema_id);
  check(sel, "select_schema(\"%s\") -> %d", schema_id, (int)sel);

  char schema_buf[256] = {0};
  api->get_current_schema(sid, schema_buf, sizeof(schema_buf));
  check(strcmp(schema_buf, schema_id) == 0, "get_current_schema() -> \"%s\"",
        schema_buf);

  /* ------------------------------------------------------------------ 打字 */
  printf("\n== process_key(\"nihao\") ==\n");
  int typed = type_text(api, sid, "nihao");
  check(typed, "5 个按键全部被消费");

  /* ---------------------------------------------------------------- 看候选 */
  printf("\n== get_context() 读候选 ==\n");
  RimeContext ctx = {0};
  RIME_STRUCT_INIT(RimeContext, ctx);
  if (!api->get_context(sid, &ctx)) {
    fprintf(stderr, "get_context() 返回 False\n");
    return 1;
  }
  printf("  preedit = \"%s\" (len=%d, cursor=%d)\n",
         ctx.composition.preedit ? ctx.composition.preedit : "(null)",
         ctx.composition.length, ctx.composition.cursor_pos);
  printf("  menu: num_candidates=%d page_size=%d page_no=%d highlighted=%d\n",
         ctx.menu.num_candidates, ctx.menu.page_size, ctx.menu.page_no,
         ctx.menu.highlighted_candidate_index);
  for (int i = 0; i < ctx.menu.num_candidates && i < 10; ++i) {
    printf("    cand[%d] = \"%s\"\n", i, ctx.menu.candidates[i].text);
  }
  /* 真实 librime 的 preedit：script_translator 的候选 preedit 按音节加分隔符
   * （luna_pinyin 的 speller/delimiter 是空格）→ "ni hao"（长度 6）。
   * 旧简化引擎回显原始输入 "nihao"（长度 5），那条预期已过时。 */
  check(ctx.composition.length == 6, "preedit 长度为 6（\"ni hao\"，音节分隔符）");
  check(ctx.menu.num_candidates > 0, "菜单里有候选（%d 条）",
        ctx.menu.num_candidates);

  int nihao_idx = -1;
  for (int i = 0; i < ctx.menu.num_candidates; ++i) {
    if (strcmp(ctx.menu.candidates[i].text, "你好") == 0) {
      nihao_idx = i;
      break;
    }
  }
  check(nihao_idx >= 0, "候选里能找到「你好」（index=%d）", nihao_idx);
  api->free_context(&ctx);

  /* ------------------------------------------------------------ 选词并提交 */
  printf("\n== select_candidate + get_commit ==\n");
  if (nihao_idx < 0) {
    g_failed = 1;
  } else {
    Bool ok = api->select_candidate(sid, (size_t)nihao_idx);
    check(ok, "select_candidate(%d) -> %d", nihao_idx, (int)ok);

    RimeCommit commit = {0};
    RIME_STRUCT_INIT(RimeCommit, commit);
    if (api->get_commit(sid, &commit)) {
      printf("  commit text = \"%s\"\n", commit.text ? commit.text : "(null)");
      check(commit.text && strcmp(commit.text, "你好") == 0,
            "提交文本为「你好」");
    } else {
      check(0, "get_commit() 返回 False");
    }
    api->free_commit(&commit);
  }

  /* ------------------------------------------------------------------ 收尾 */
  printf("\n== 状态与清理 ==\n");
  RimeStatus status = {0};
  RIME_STRUCT_INIT(RimeStatus, status);
  if (api->get_status(sid, &status)) {
    printf("  status: schema_id=\"%s\" is_composing=%d\n",
           status.schema_id ? status.schema_id : "(null)",
           (int)status.is_composing);
    check(status.schema_id && strcmp(status.schema_id, schema_id) == 0,
          "status.schema_id == \"%s\"", schema_id);
    api->free_status(&status);
  } else {
    check(0, "get_status() 返回 False");
  }

  api->destroy_session(sid);

  /* ------------------------------------------------------- 部署模式（另一条路径） */
  /* 宿主的 `Librime::deploy()` 会另开一个进程/线程，加载同一个 DLL，然后走：
   *   setup → deployer_initialize → deploy → finalize
   * 这条路径上 `deployer_initialize` 与 `deploy` 是**必需**入口，缺一个宿主就拒绝。 */
  printf("\n== 部署模式：deployer_initialize + deploy ==\n");
  api->setup(&traits);
  api->deployer_initialize(&traits);
  Bool deployed = api->deploy();
  printf("  deploy() -> %d\n", (int)deployed);
  check(deployed, "deploy() 成功（installation_update && workspace_update && "
                  "user_dict_upgrade && cleanup_trash）");

  api->finalize();
  FreeLibrary(h);

  printf("\n== 结果：%s ==\n", g_failed ? "有检查失败" : "全部通过");
  return g_failed ? 1 : 0;
}
