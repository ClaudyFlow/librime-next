/*
 * 用真实 rime.dll（librime-next 的 cdylib）走 C ABI 做**部分部署**的最小 loader。
 *
 * 与 dll_abi_check.c 的区别：去掉所有「运行模式」断言（process_key /
 * get_context / select_candidate …），只保留宿主部署路径会走的那条：
 *   LoadLibrary → rime_get_api → setup → deployer_initialize → deploy → finalize
 * 这样不受引擎运行态影响，纯验证「用 wn 的 rn 文件能否把方案部署出来」。
 *
 * 用法：
 *   partial_deploy <librime_next.dll> <shared_data_dir> <user_data_dir>
 * 退出码：0 = deploy() 返回成功；1 = 加载/部署失败。
 */
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <windows.h>

#include "rime_api.h"

typedef RimeApi* (*get_api_fn)(void);

int main(int argc, char** argv) {
  if (argc < 4) {
    fprintf(stderr,
            "usage: %s <librime_next.dll> <shared_data_dir> <user_data_dir>\n",
            argv[0]);
    return 2;
  }
  const char* dll_path = argv[1];
  const char* shared_data_dir = argv[2];
  const char* user_data_dir = argv[3];

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
  RimeApi* api = get_api();
  if (!api) {
    fprintf(stderr, "rime_get_api() 返回 NULL\n");
    return 1;
  }
  printf("  api->data_size = %d, sizeof(RimeApi) = %zu\n", api->data_size,
         sizeof(RimeApi));

  printf("\n== setup / deployer_initialize ==\n");
  RimeTraits traits = {0};
  traits.data_size = sizeof(RimeTraits) - sizeof(traits.data_size);
  traits.shared_data_dir = shared_data_dir;
  traits.user_data_dir = user_data_dir;
  traits.distribution_name = "librime-next";
  traits.distribution_code_name = "librime-next";
  traits.distribution_version = "0.1.0";
  traits.app_name = "rime.partial_deploy";
  traits.modules = NULL;
  traits.min_log_level = 2;
  traits.log_dir = "";

  api->setup(&traits);
  api->deployer_initialize(&traits);
  printf("  setup() + deployer_initialize() 完成\n");

  printf("\n== deploy()（部分部署，按 user_data_dir 的 schema_list）==\n");
  Bool deployed = api->deploy();
  printf("  deploy() -> %d\n", (int)deployed);

  api->finalize();
  FreeLibrary(h);

  printf("\n== 部署结果：%s ==\n", deployed ? "成功" : "失败");
  return deployed ? 0 : 1;
}
