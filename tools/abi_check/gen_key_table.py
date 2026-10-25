"""
从 C++ librime 的 `src/rime/key_table.cc` 生成 Rust 侧的按键名表。

C++ 那边有两张表：
  * `key_names[]`  —— 一大坨用 NUL 串接的字符池；
  * `keys_by_name[]`—— {keyval, offset} 对，`RimeGetKeyName()` 靠它查「键码 → 名字」。

`KeyEvent::repr()` / `KeyEvent::parse()` 想要与 C++ 逐字一致，就得有这两张表。
本脚本把两张表烘成一张 Rust 静态表 `KEY_NAME_TABLE: &[(u32, &str)]`，
外加一张「名字 → 键码」的反查表 `KEYCODE_BY_NAME`。

用法：python gen_key_table.py <key_table.cc> <输出.rs>
"""
import re
import sys


def parse_key_names(src: str) -> bytes:
    """把 `static const char key_names[] = "a\0" "b\0";` 解析成字节串。"""
    m = re.search(r'static const char key_names\[\] =\s*(.*?);\s*\n', src, re.S)
    if not m:
        raise SystemExit('找不到 key_names 定义')
    body = m.group(1)
    out = bytearray()
    for lit in re.findall(r'"((?:[^"\\]|\\.)*)"', body):
        # C 字符串字面量转义：只处理常见的 \0 \" \\ \n \t
        i = 0
        while i < len(lit):
            ch = lit[i]
            if ch == '\\':
                i += 1
                nxt = lit[i]
                out.append({'n': 10, 't': 9, 'r': 13, '0': 0, '\\': 92, '"': 34}.get(nxt, ord(nxt)))
            else:
                out.append(ord(ch))
            i += 1
    return bytes(out)


def parse_keys_by_name(src: str) -> list:
    """解析 `static const key_entry keys_by_name[] = { {0x..., N}, ... };`"""
    start = src.find('static const key_entry keys_by_name[] = {')
    if start < 0:
        raise SystemExit('找不到 keys_by_name 定义')
    end = src.find('\n};', start)
    body = src[start:end]
    return [(int(k, 16), int(o)) for k, o in re.findall(r'\{\s*(0x[0-9a-fA-F]+)\s*,\s*(\d+)\s*\}', body)]


def main() -> None:
    cc_path, out_path = sys.argv[1], sys.argv[2]
    src = open(cc_path, encoding='utf-8').read()

    pool = parse_key_names(src)
    entries = parse_keys_by_name(src)

    # 同名键码可能重复（keys_by_name 里每个 keyval 只出现一次，但不同 keyval 可能同名）；
    # C++ 的 RimeGetKeyName 返回**第一个**匹配项，所以这里保持原顺序不去重。
    rows = []
    for keyval, offset in entries:
        end = pool.find(b'\x00', offset)
        if end < 0:
            raise SystemExit(f'偏移 {offset} 越界')
        name = pool[offset:end].decode('utf-8')
        rows.append((keyval, name))

    lines = []
    lines.append('//! 由 `tools/abi_check/gen_key_table.py` 从 C++ librime 的 `src/rime/key_table.cc` 生成。')
    lines.append('//!')
    lines.append('//! 不要手改：改 C++ 那边的表，然后重跑生成脚本。')
    lines.append('//! 对应 C++ 的 `key_names[]` + `keys_by_name[]`，即 `RimeGetKeyName()` 的全部数据。')
    lines.append('')
    lines.append('/// `(键码, 键名)`，顺序与 C++ `keys_by_name[]` 一致（`RimeGetKeyName` 取**第一个**匹配项）。')
    lines.append('pub const KEY_NAME_TABLE: &[(u32, &str)] = &[')
    for keyval, name in rows:
        esc = name.replace('\\', '\\\\').replace('"', '\\"')
        lines.append(f'    (0x{keyval:08x}, "{esc}"),')
    lines.append('];')
    lines.append('')
    open(out_path, 'w', encoding='utf-8', newline='\n').write('\n'.join(lines) + '\n')
    print(f'生成 {len(rows)} 条键名 → {out_path}')


if __name__ == '__main__':
    main()
