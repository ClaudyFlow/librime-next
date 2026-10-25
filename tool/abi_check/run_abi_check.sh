#!/usr/bin/env bash
# 构建 librime-next 的 cdylib，并用**原版** rime_api.h 编写的验证器跑一遍完整 ABI 链路。
#
# 用法：./run_abi_check.sh [仓库根目录]
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="${1:-$(cd "$HERE/../../.." && pwd)}"
LIBRIME_SRC="${LIBRIME_SRC:-C:/code/librime/src}"
CC="${CC:-/c/Program Files/llvm-mingw/bin/gcc}"

DLL="$ROOT/target/debug/librime_next.dll"
E2E="$ROOT/librime-next/test/e2e"
WORK="${TMPDIR:-/tmp}/rime_next_abi_check"
USER_DIR="$WORK/user"

echo "==> 清理上次产物"
rm -rf "$WORK"
mkdir -p "$USER_DIR"

# 方案清单：deployer / schema 选择都从这里读
cat > "$USER_DIR/default.yaml" <<'EOF'
patch:
  schema_list:
    - schema: luna_pinyin_simp
EOF

echo "==> 构建 cdylib (cargo build)"
cargo build --manifest-path "$ROOT/librime-next/Cargo.toml"

echo "==> 编译 ABI 验证器 (对照原版 rime_api.h)"
"$CC" -O1 -o "$HERE/dll_abi_check.exe" "$HERE/dll_abi_check.c" -I"$LIBRIME_SRC"

echo "==> 运行验证器"
"$HERE/dll_abi_check.exe" \
  "$(cygpath -w "$DLL" 2>/dev/null || echo "$DLL")" \
  "$(cygpath -w "$E2E/userdata" 2>/dev/null || echo "$E2E/userdata")" \
  "$(cygpath -w "$USER_DIR" 2>/dev/null || echo "$USER_DIR")" \
  "luna_pinyin_simp" \
  "$(cygpath -w "$E2E/build" 2>/dev/null || echo "$E2E/build")"
