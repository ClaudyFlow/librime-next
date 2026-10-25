//! `darts.h`（darts-clone 0.32h，librime 的 `include/darts.h`，第三方）的 Rust 移植：
//! **只读查询** + **从零构建**。
//!
//! `.prism.bin` 里的双数组 trie 就是这个格式。单位是 `u32`（小端），语义全在低 32 位里：
//!
//! ```text
//! has_leaf() = (unit >> 8) & 1                       // 该节点后面紧跟一个"叶子单元"（值）
//! value()    = unit & 0x7FFF_FFFF                    // 叶子单元里的值
//! label()    = unit & (0x8000_0000 | 0xFF)           // 该单元对应的入边字符；叶子单元恒带 MSB
//! offset()   = (unit >> 10) << ((unit & (1 << 9)) >> 6)   // 位移 0 或 8
//! ```
//!
//! 三个必须照抄的细节：
//!
//! 1. **label 是"入边字符"**：从父节点 `p` 走到字符 `c` 的子节点，位置是
//!    `p ^ offset(p) ^ c`；落到的单元 `label()` 必须等于 `c`。所以"查不到"在 darts 里
//!    既可能是位置越界，也可能是 label 不匹配（返回 `-2`）。
//! 2. **label 0 被留给"值"**：终端节点的值单元在 `p ^ offset(p)`（`c = 0` 的位置），
//!    因此**字符 0 不能当键字节**——`Prism` 的键是 C 字符串，天然没有 0。
//! 3. `exactMatchSearch` 在 `length != 0` 和 `length == 0` 两种模式下**循环条件不同**：
//!    前者按给定长度走，后者撞 `'\0'` 才停（`darts.h:407-437`）。这一层统一用
//!    `&[u8]`，长度永远显式给出，所以只实现前者——语义上等价，因为 C++ 里
//!    `Prism` 的调用点全都传了长度。
//!
//! # 构建
//!
//! C++ 用 `Darts::DoubleArray::build(count, keys)`，它会先建 DAWG 再排布，值 = 键在
//! 输入数组里的**下标**。这里不需要字节相同，只需要**语义相同**（`get_value(key) == 下标`），
//! 所以用最朴素的 first-fit 排布（[`DoubleArray::build`]）：BFS 扫节点，给每个节点找一个
//! 偏移 `o`，使 `p ^ o ^ c` 这一整组位置都空着；`o` 从上一次的 `o`（`hint`）开始往上试。
//! 这条 `hint` 单调递增的剪枝是 darts-clone 自己也做的（`find_valid_offset`），
//! 它让"没命中"的尝试次数保持常数级——不然就是 O(n²)。

/// 一个 `u32` 单元。内部只存原始位模式，语义由方法给出。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Unit(pub u32);

impl Unit {
    #[inline]
    pub fn has_leaf(self) -> bool {
        (self.0 >> 8) & 1 == 1
    }
    #[inline]
    pub fn value(self) -> u32 {
        self.0 & 0x7fff_ffff
    }
    #[inline]
    pub fn label(self) -> u32 {
        self.0 & (0x8000_0000 | 0xFF)
    }
    #[inline]
    pub fn offset(self) -> u32 {
        (self.0 >> 10) << ((self.0 & (1 << 9)) >> 6)
    }
    /// 叶子单元（带值）的位模式：`value | MSB`
    #[inline]
    fn make_leaf(value: u32) -> Self {
        Unit(value | 0x8000_0000)
    }
    /// 普通单元的位模式。
    ///
    /// `offset < 2²¹` 用 21 位直存；再大就必须是 256 的倍数，用 `bit9 = 1` 的
    /// "右移 8 位"形式（`offset() = (unit >> 10) << 8`）。darts-clone 的
    /// `DoubleArrayBuilderUnit::set_offset` 也是这两档。
    #[inline]
    fn make_node(label: u8, offset: u32, has_leaf: bool) -> Self {
        let (encoded, shifted) = if offset < (1 << 21) {
            (offset, 0u32)
        } else {
            debug_assert_eq!(offset % 256, 0, "大 offset 必须 256 对齐");
            debug_assert!(offset < (1 << 29), "offset 超出 darts 的 29 位编码");
            (offset >> 8, 1u32)
        };
        Unit((encoded << 10) | (shifted << 9) | (u32::from(has_leaf) << 8) | u32::from(label))
    }
}

/// darts 双数组 trie。
#[derive(Clone, Default, Debug)]
pub struct DoubleArray {
    units: Vec<Unit>,
}

/// `traverse()` 的返回值：`>= 0` 命中（值），`-1` 路径走通但没有值，`-2` 路径不成立。
pub const TRAVERSE_NO_VALUE: i32 = -1;
pub const TRAVERSE_INVALID: i32 = -2;

impl DoubleArray {
    /// `set_array(ptr, size)`：把 `.prism.bin` 里那段 `u32` 数组接过来。
    pub fn from_bytes(bytes: &[u8], num_units: usize) -> Option<Self> {
        let need = num_units.checked_mul(4)?;
        if bytes.len() < need {
            return None;
        }
        let mut units = Vec::with_capacity(num_units);
        for i in 0..num_units {
            let p = i * 4;
            units.push(Unit(u32::from_le_bytes(
                bytes[p..p + 4].try_into().ok()?,
            )));
        }
        Some(Self { units })
    }

    pub fn from_units(units: Vec<Unit>) -> Self {
        Self { units }
    }

    /// `size()`：单元数
    pub fn size(&self) -> usize {
        self.units.len()
    }

    /// `total_size()`：镜像字节数
    pub fn total_size(&self) -> usize {
        self.units.len() * 4
    }

    pub fn units(&self) -> &[Unit] {
        &self.units
    }

    #[inline]
    fn unit(&self, pos: usize) -> Option<Unit> {
        self.units.get(pos).copied()
    }

    /// `exactMatchSearch<int>`（`darts.h:407`）。找不到返回 `None`。
    pub fn exact_match_search(&self, key: &[u8]) -> Option<u32> {
        let mut node_pos = 0usize;
        let mut unit = self.unit(node_pos)?;
        for &c in key {
            node_pos ^= unit.offset() as usize ^ usize::from(c);
            unit = self.unit(node_pos)?;
            if unit.label() != u32::from(c) {
                return None;
            }
        }
        if !unit.has_leaf() {
            return None;
        }
        let leaf = self.unit(node_pos ^ unit.offset() as usize)?;
        Some(leaf.value())
    }

    /// `commonPrefixSearch`（`darts.h:441`）：所有"是 `key` 前缀"的键，按长度升序。
    ///
    /// 返回 `(value, 键长)`。C++ 版本要求调用方先把 `results` 开成 `key.length()` 个，
    /// 这里直接返回 `Vec`。
    pub fn common_prefix_search(&self, key: &[u8], max_results: usize) -> Vec<(u32, usize)> {
        let mut out = Vec::new();
        let mut node_pos = 0usize;
        let Some(root) = self.unit(node_pos) else {
            return out;
        };
        node_pos ^= root.offset() as usize;
        for (i, &c) in key.iter().enumerate() {
            node_pos ^= usize::from(c);
            let Some(unit) = self.unit(node_pos) else {
                break;
            };
            if unit.label() != u32::from(c) {
                break;
            }
            node_pos ^= unit.offset() as usize;
            if unit.has_leaf() {
                let Some(leaf) = self.unit(node_pos) else {
                    break;
                };
                if out.len() < max_results {
                    out.push((leaf.value(), i + 1));
                }
            }
        }
        out
    }

    /// `traverse(key, node_pos, key_pos, length)`（`darts.h:487`）。
    ///
    /// `node_pos` / `key_pos` 是**进出参**：成功走完时 `key_pos == key.len()`、
    /// `node_pos` 停在该节点；中途失败时两者停在**最后一个成功的字符**上（C++ 也是这样，
    /// `ExpandSearch` 靠这个做 BFS 的续走）。
    pub fn traverse(&self, key: &[u8], node_pos: &mut usize, key_pos: &mut usize) -> i32 {
        let mut id = *node_pos;
        let Some(mut unit) = self.unit(id) else {
            return TRAVERSE_INVALID;
        };
        while *key_pos < key.len() {
            let c = key[*key_pos];
            id ^= unit.offset() as usize ^ usize::from(c);
            let Some(next) = self.unit(id) else {
                return TRAVERSE_INVALID;
            };
            unit = next;
            if unit.label() != u32::from(c) {
                return TRAVERSE_INVALID;
            }
            *node_pos = id;
            *key_pos += 1;
        }
        if !unit.has_leaf() {
            return TRAVERSE_NO_VALUE;
        }
        let Some(leaf) = self.unit(id ^ unit.offset() as usize) else {
            return TRAVERSE_INVALID;
        };
        leaf.value() as i32
    }

    // ------------------------------------------------------------------ 构建

    /// `DoubleArray::build()`：值 = 键在 `keys` 里的下标。
    ///
    /// 返回 `None` 表示偏移超出 darts 的 21 位编码范围（真实词库不会）。
    /// **空输入也是合法的**：产出一个只有根单元的数组（`Prism::Build` 遇到空拼写表时
    /// 仍要写出 4 字节镜像，`trie_->total_size()` 不能是 0）。
    pub fn build(keys: &[&[u8]]) -> Option<Self> {
        let mut trie = RawTrie::default();
        trie.new_node(); // 根节点：先建出来，空输入也只剩它
        for (i, key) in keys.iter().enumerate() {
            trie.insert(key, i as u32);
        }
        trie.layout()
    }
}

/// 构建期的朴素 trie（只在 `build` 里活一次）。
#[derive(Default)]
struct RawTrie {
    children: Vec<std::collections::BTreeMap<u8, usize>>,
    value: Vec<Option<u32>>,
}

impl RawTrie {
    fn new_node(&mut self) -> usize {
        self.children.push(Default::default());
        self.value.push(None);
        self.children.len() - 1
    }

    fn insert(&mut self, key: &[u8], value: u32) {
        if self.children.is_empty() {
            self.new_node();
        }
        let mut node = 0usize;
        for &c in key {
            // ⚠️ 0 是保留标签（叶子的位置），键里不能有它；有的话只能丢弃。
            if c == 0 {
                return;
            }
            node = match self.children[node].get(&c) {
                Some(&child) => child,
                None => {
                    let child = self.new_node();
                    self.children[node].insert(c, child);
                    child
                }
            };
        }
        // 重复键：保留第一个（C++ 的 marisa 也会把它们合并成同一个 id）
        self.value[node].get_or_insert(value);
    }

    fn layout(self) -> Option<DoubleArray> {
        let mut units: Vec<Unit> = Vec::new();
        let mut used: Vec<bool> = Vec::new();
        let reserve = |pos: usize, units: &mut Vec<Unit>, used: &mut Vec<bool>| {
            if used.len() <= pos {
                used.resize(pos + 1, false);
                units.resize(pos + 1, Unit(0));
            }
        };
        reserve(0, &mut units, &mut used);
        used[0] = true;

        // DFS（深度优先），不是 BFS。见 `find_offset` 的文档：DFS 让"当前节点的位置"
        // 始终贴着分配前沿，`offset = pos ^ base` 才能落在 darts 编码得下的范围里，
        // 数组也才能压到 ~节点数（BFS 会把位置散到整个数组，偏移迅速爆炸）。
        let mut stack = vec![(0usize, 0usize, 0u8)];
        // 分配前沿（base 空间）：只增不减，darts-clone 的 `extras_head_` 同款剪枝。
        let mut hint = 1usize;
        while let Some((pos, node, label)) = stack.pop() {
            let labels: Vec<u8> = self.children[node].keys().copied().collect();
            let has_value = self.value[node].is_some();
            let offset = find_offset(&used, pos, &labels, has_value, &mut hint)?;
            units[pos] = Unit::make_node(label, offset, has_value);
            let base = pos ^ offset as usize;
            if has_value {
                debug_assert_eq!(base, pos ^ offset as usize);
                reserve(base, &mut units, &mut used);
                debug_assert!(!used[base], "leaf position must be reserved free");
                used[base] = true;
                units[base] = Unit::make_leaf(self.value[node].unwrap());
            }
            for &c in &labels {
                let child_pos = base ^ usize::from(c);
                reserve(child_pos, &mut units, &mut used);
                debug_assert!(!used[child_pos], "child position must be reserved free");
                used[child_pos] = true;
                // 先占位（label 就是入边字符），offset 等轮到它时再填
                units[child_pos] = Unit(u32::from(c));
                stack.push((child_pos, self.children[node][&c], c));
            }
        }
        // 事后校验：每个"声明有叶子"的**节点单元**（MSB 清），必须在 pos ^ offset
        // 处放着一个带 MSB 的叶子单元。
        // ⚠️ 不能用"叶子单元的 has_leaf() 必须为假"这类说法：叶子单元的位模式是
        // `value | MSB`，bit 8 是**值的一部分**（`has_leaf()` 读到它纯属偶然），
        // darts-clone 原版也没有这个不变量。2026-10-25 修正：原先的断言写成
        // `!has_leaf() || MSB`，把所有合法带值节点（has_leaf=1、MSB=0）误杀。
        debug_assert!(
            units.iter().enumerate().all(|(pos, u)| {
                if u.has_leaf() && u.0 & 0x8000_0000 == 0 {
                    // 普通节点单元声明 has_leaf → 叶子必须就位且带 MSB
                    units
                        .get(pos ^ u.offset() as usize)
                        .is_some_and(|leaf| leaf.0 & 0x8000_0000 != 0)
                } else {
                    true
                }
            }),
            "has_leaf 节点必须在 pos ^ offset 处放带 MSB 的叶子单元"
        );
        Some(DoubleArray { units })
    }
}

/// 找 `offset`：`child = pos ^ offset ^ label`，也就是 `base = pos ^ offset` 处放
/// "0 号孩子"（叶子/基址），其余孩子在 `base ^ label`。
///
/// `hint` 是**单调递增的 base**（不是 offset）：按 DFS 顺序，当前节点的位置总在分配
/// 前沿附近，所以从 `hint` 往后找几步就能找到空位，数组也保持紧凑。
///
/// # 大数组：offset ≥ 2²¹ 时必须 256 对齐
///
/// darts 的单元只有 21 位存 offset；`offset >= 1<<21` 时要用 bit9 的"右移 8 位"形式，
/// 于是 offset 必须是 256 的倍数（`offset = pos ^ base` 的低 8 位为零 →
/// `base ≡ pos (mod 256)`）。真实词库（`yuling_chaifen` 的音节表有 370 万前缀）
/// **一定**会走到这条分支，所以不能省；省了就会在 `offset >= 1<<21` 时返回 `None`。
fn find_offset(
    used: &[bool],
    pos: usize,
    labels: &[u8],
    has_value: bool,
    hint: &mut usize,
) -> Option<u32> {
    let free = |p: usize| p >= used.len() || !used[p];
    let mut base = (*hint).max(1);
    loop {
        if base >= (1 << 29) {
            return None;
        }
        let offset = pos ^ base;
        if offset >= (1 << 21) {
            // 大 offset：跳到与 pos 同余 mod 256 的 base（见函数文档）。
            let want = pos & 0xFF;
            let current = base & 0xFF;
            if current != want {
                base += (want + 256 - current) & 0xFF;
                continue;
            }
        }
        if offset == 0 {
            // offset 0 在 darts 里等于"没设置过"，不能用；跳过这一格。
            base += 1;
            continue;
        }
        let base_ok = !has_value || free(base);
        if base_ok
            && labels.iter().all(|&c| {
                let child = base ^ usize::from(c);
                child != 0 && free(child)
            })
        {
            *hint = base;
            return Some(offset as u32);
        }
        base += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Vec<&'static [u8]> {
        vec![b"a", b"ab", b"abc", b"b", b"ba", b"\xe4\xbd\xa0"]
    }

    #[test]
    fn build_then_query_roundtrip() {
        let ks = keys();
        let da = DoubleArray::build(&ks).expect("layout");
        for (i, k) in ks.iter().enumerate() {
            assert_eq!(
                da.exact_match_search(k),
                Some(i as u32),
                "key {:?} should map to {i}",
                std::str::from_utf8(k)
            );
        }
        assert_eq!(da.exact_match_search(b"abcd"), None);
        assert_eq!(da.exact_match_search(b"z"), None);
        assert_eq!(da.exact_match_search(b""), None); // 空键没有值
    }

    #[test]
    fn common_prefix_search_is_length_ascending() {
        let ks = keys();
        let da = DoubleArray::build(&ks).unwrap();
        let got = da.common_prefix_search(b"abc", 8);
        assert_eq!(got, vec![(0, 1), (1, 2), (2, 3)]);
        // 不是前缀的键不能出现
        let got = da.common_prefix_search(b"b", 8);
        assert_eq!(got, vec![(3, 1)]);
    }

    #[test]
    fn traverse_reports_three_states() {
        let ks = keys();
        let da = DoubleArray::build(&ks).unwrap();
        // 命中
        let (mut node, mut pos) = (0usize, 0usize);
        assert_eq!(da.traverse(b"ab", &mut node, &mut pos), 1);
        assert_eq!(pos, 2);
        // 走通但没有值："abc" 有值；用 "ab" 之后再加一个不存在字符
        let (mut node, mut pos) = (0usize, 0usize);
        assert_eq!(da.traverse(b"ax", &mut node, &mut pos), TRAVERSE_INVALID);
        assert_eq!(pos, 1, "失败时 key_pos 停在最后一个成功字符之后");
    }

    #[test]
    fn offset_encoding_roundtrips() {
        for off in [1u32, 2, 1000, (1 << 21) - 1] {
            let u = Unit::make_node(7, off, false);
            assert_eq!(u.offset(), off);
            assert_eq!(u.label(), 7);
            assert!(!u.has_leaf());
        }
        // 大 offset 走 bit9 的 8 位移位形式（真实词库 > 2M 单元时一定会用上）
        for off in [1u32 << 21, 3 << 21, (1 << 29) - 256] {
            let u = Unit::make_node(7, off, false);
            assert_eq!(u.offset(), off, "shifted offset {off}");
            assert_eq!(u.label(), 7);
            assert!(!u.has_leaf());
        }
        let leaf = Unit::make_leaf(12345);
        assert_eq!(leaf.value(), 12345);
        assert!(leaf.label() & 0x8000_0000 != 0, "叶子单元的 label 恒带 MSB");
    }

    #[test]
    fn duplicate_keys_share_the_first_value() {
        let da = DoubleArray::build(&[b"x" as &[u8], b"x"]).unwrap();
        assert_eq!(da.exact_match_search(b"x"), Some(0));
    }

    /// 空输入：只留根单元（`Prism::Build` 写空拼写表时要有一段非空镜像）。
    #[test]
    fn empty_keys_build_a_root_only_array() {
        let da = DoubleArray::build(&[]).expect("空输入也能建");
        assert_eq!(da.size(), 1, "只有根单元");
        assert_eq!(da.total_size(), 4);
        assert_eq!(da.exact_match_search(b""), None);
        assert_eq!(da.exact_match_search(b"a"), None);
    }
}
