//! `dict/mapped_file.h`(173) + `dict/mapped_file.cc`(168) 的 Rust 移植。
//!
//! # 这一层是整个 `dict/` 的地基：`OffsetPtr`
//!
//! C++ 把文件 mmap 成一块内存，然后在**任意位置**放结构体；结构体里的指针不是地址，
//! 而是 **`i32` 相对偏移**：
//!
//! ```text
//! OffsetPtr<T> { i32 offset_; }
//!   get() = (char*)&offset_ + offset_        // 相对**自己这个字段**的地址
//!   0 表示 NULL（所以不能指向自己，见 mapped_file.h:17 的注释）
//! ```
//!
//! 这带来两个必须照抄的后果：
//!
//! 1. 同一个"指针"换台机器也有效（位置无关），但整个文件**不能重排**——
//!    挪一个字节，所有偏移全废；
//! 2. `OffsetPtr` 既可以是 NULL 也可以是**负数**（偏移可以为负）。C++ 里
//!    `Allocate` 只往后长，所以实际出现的都是正偏移，但读的时候不能假设。
//!
//! # 布局常量（照 `mapped_file.h:38-66`）
//!
//! ```text
//! OffsetPtr<T>  : i32        // 4 字节，字段地址 = 自身位置
//! String        : i32 data   // 4 字节
//! Array<T>      : u32 size; T at[]      // at 紧跟在 size 后面（+4）
//! List<T>       : u32 size; i32 at      // at 是 OffsetPtr（+4）
//! ```
//!
//! ⚠️ `Array<T>` 的 `at` 在 `+4` 而不是 `align_of::<T>()` 处：C++ 的
//! `struct Array { Size size; T at[1]; }` 里 `size` 是 4 字节、`at` 紧接着它，
//! 只有**结构体整体**对齐到 `alignof(T)`（所以 `CreateArray` 会多分几个字节）。
//! 读的时候只要知道 `at` 在 `+4` 就行。
//!
//! # Rust 侧为什么不是真的 mmap
//!
//! 不加依赖就没有跨平台 mmap。读路径 = 一次性 `fs::read` 成 `Vec<u8>`（这些文件
//! 最大 6 MB，且引擎加载后本来就要常驻）；写路径 = 一个零初始化的 `Vec<u8>` 当
//! "文件镜像"，`allocate` 往后长，`save` 落盘。**对外语义（偏移 + 布局 + 对齐）与
//! C++ 完全一致**，只是把裸指针换成"偏移量 + 带边界检查的访问"：
//! C++ 越界是 UB，这里返回 `None`。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// `OffsetPtr<T>` 的字节数
pub const OFFSET_PTR_SIZE: usize = 4;
/// `String` 的字节数
pub const STRING_SIZE: usize = 4;

/// `RIME_ALIGNED(size, T)`（`mapped_file.h:137`）
#[inline]
pub const fn align_up(size: usize, align: usize) -> usize {
    if align <= 1 {
        return size;
    }
    (size + align - 1) & !(align - 1)
}

/// 一份 `MappedFile` 的字节镜像。
#[derive(Debug, Clone)]
pub struct MappedFile {
    path: PathBuf,
    buf: Vec<u8>,
    /// 写模式下"已经分配到哪"（对应 C++ 的 `size_`）；读模式下等于 `buf.len()`。
    used: usize,
    writable: bool,
}

impl MappedFile {
    // ---------------------------------------------------------------- 打开 / 关闭

    /// `MappedFile::OpenReadOnly()`（`mapped_file.cc:73`）。
    pub fn open_read_only(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let buf = fs::read(&path)?;
        let used = buf.len();
        Ok(Self {
            path,
            buf,
            used,
            writable: false,
        })
    }

    /// `MappedFile::Create(capacity)`（`mapped_file.cc:47`）：清空文件并给足容量。
    ///
    /// C++ 会先 `Resize(capacity)`（已存在时）或建一个 `capacity` 字节的文件，
    /// 然后以读写打开、`size_ = 0`。这里就是一块零初始化的镜像。
    pub fn create(path: impl Into<PathBuf>, capacity: usize) -> io::Result<Self> {
        let path = path.into();
        Ok(Self {
            path,
            buf: vec![0u8; capacity],
            used: 0,
            writable: true,
        })
    }

    /// `MappedFile::Exists()`
    pub fn exists(path: impl AsRef<Path>) -> bool {
        path.as_ref().exists()
    }

    /// `MappedFile::Remove()`
    pub fn remove(path: impl AsRef<Path>) -> io::Result<()> {
        match fs::remove_file(path.as_ref()) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `file_size()`：文件的**容量**（不是已用长度）。
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// 已用长度：读模式下是文件长度；写模式下是"分配水位"。
    pub fn used(&self) -> usize {
        self.used
    }

    // ---------------------------------------------------------------- 读

    #[inline]
    fn check(&self, pos: usize, len: usize) -> Option<()> {
        if pos.checked_add(len).is_some_and(|end| end <= self.buf.len()) {
            Some(())
        } else {
            None
        }
    }

    /// `Find<T>(offset)` 的第一步：地址本身。
    pub fn pos(&self, offset: usize) -> Option<usize> {
        self.check(offset, 1)?;
        Some(offset)
    }

    pub fn u8_at(&self, pos: usize) -> Option<u8> {
        self.check(pos, 1)?;
        Some(self.buf[pos])
    }

    pub fn u32_at(&self, pos: usize) -> Option<u32> {
        self.check(pos, 4)?;
        Some(u32::from_le_bytes(self.buf[pos..pos + 4].try_into().ok()?))
    }

    pub fn i32_at(&self, pos: usize) -> Option<i32> {
        self.u32_at(pos).map(|v| v as i32)
    }

    pub fn f32_at(&self, pos: usize) -> Option<f32> {
        self.u32_at(pos).map(f32::from_bits)
    }

    pub fn bytes_at(&self, pos: usize, len: usize) -> Option<&[u8]> {
        self.check(pos, len)?;
        Some(&self.buf[pos..pos + len])
    }

    /// 从 `pos` 读一个 NUL 结尾的字节串（不校验 UTF-8）。
    pub fn cstr_bytes_at(&self, pos: usize) -> Option<&[u8]> {
        if pos >= self.buf.len() {
            return None;
        }
        let tail = &self.buf[pos..];
        let end = tail.iter().position(|&b| b == 0)?;
        Some(&tail[..end])
    }

    /// 从 `pos` 读一个 NUL 结尾的字符串。Rime 的字符串都是 UTF-8，非法时返回 `None`
    /// （C++ 直接当 `char*` 用，坏数据要到上层才炸；这里提前拒绝）。
    pub fn str_at(&self, pos: usize) -> Option<&str> {
        std::str::from_utf8(self.cstr_bytes_at(pos)?).ok()
    }

    /// `OffsetPtr::get()`：`pos` 是**那个 i32 字段自己的地址**。
    /// 0 → `None`（NULL）；否则 `pos + offset`（必须落在文件内）。
    pub fn deref(&self, field_pos: usize) -> Option<usize> {
        let offset = self.i32_at(field_pos)?;
        if offset == 0 {
            return None;
        }
        let target = (field_pos as i64).checked_add(offset as i64)?;
        if target < 0 {
            return None;
        }
        self.pos(target as usize)
    }

    /// `String::c_str()`（`mapped_file.h:48`）：`String` 只存一个 OffsetPtr。
    pub fn string_at(&self, pos: usize) -> Option<&str> {
        let data = self.deref(pos)?;
        self.str_at(data)
    }

    /// `Array<T>::size`
    pub fn array_size(&self, pos: usize) -> Option<u32> {
        self.u32_at(pos)
    }

    /// `Array<T>::at[i]`（数据从 `pos + 4` 开始）
    pub fn array_item(&self, pos: usize, index: usize, elem_size: usize) -> Option<usize> {
        let start = pos.checked_add(4)?;
        let p = start.checked_add(index.checked_mul(elem_size)?)?;
        self.check(p, elem_size)?;
        Some(p)
    }

    /// `List<T>::size`
    pub fn list_size(&self, pos: usize) -> Option<u32> {
        self.u32_at(pos)
    }

    /// `List<T>::at`（`at` 是 OffsetPtr，在 `pos + 4`）
    pub fn list_data(&self, pos: usize) -> Option<usize> {
        self.deref(pos + 4)
    }

    pub fn list_item(&self, pos: usize, index: usize, elem_size: usize) -> Option<usize> {
        let data = self.list_data(pos)?;
        let p = data.checked_add(index.checked_mul(elem_size)?)?;
        self.check(p, elem_size)?;
        Some(p)
    }

    /// 读 `char[N]` 里的 NUL 结尾串（`metadata->format` 那种定长字段）。
    pub fn fixed_str_at(&self, pos: usize, len: usize) -> Option<&str> {
        let raw = self.bytes_at(pos, len)?;
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        std::str::from_utf8(&raw[..end]).ok()
    }

    // ---------------------------------------------------------------- 写

    fn ensure(&mut self, needed: usize) {
        if self.buf.len() < needed {
            self.buf.resize(needed, 0);
        }
    }

    /// `MappedFile::Allocate<T>(count)`（`mapped_file.h:143`）：
    /// 把水位对齐到 `align`，再占 `size` 字节，返回起始位置。
    pub fn allocate(&mut self, size: usize, align: usize) -> usize {
        let pos = align_up(self.used, align);
        let end = pos + size;
        self.ensure(end);
        self.used = end;
        pos
    }

    /// `Allocate<T>(1)`，`size = T` 的字节数。
    pub fn alloc_struct(&mut self, size: usize, align: usize) -> usize {
        self.allocate(size, align)
    }

    pub fn put_bytes(&mut self, pos: usize, bytes: &[u8]) {
        self.ensure(pos + bytes.len());
        self.buf[pos..pos + bytes.len()].copy_from_slice(bytes);
    }

    pub fn put_u32(&mut self, pos: usize, value: u32) {
        self.put_bytes(pos, &value.to_le_bytes());
    }

    pub fn put_i32(&mut self, pos: usize, value: i32) {
        self.put_u32(pos, value as u32);
    }

    pub fn put_f32(&mut self, pos: usize, value: f32) {
        self.put_u32(pos, value.to_bits());
    }

    pub fn put_u8(&mut self, pos: usize, value: u8) {
        self.put_bytes(pos, &[value]);
    }

    /// `CreateArray<T>(n)`（`mapped_file.h:170`）：`u32 size` + `n` 个元素。
    ///
    /// C++ 算的是 `sizeof(Array<T>) + sizeof(T) * (n - 1)`，其中
    /// `sizeof(Array<T>) = 4 + sizeof(T)`（含尾部 padding），所以总长就是
    /// `4 + sizeof(T) * n`。这里直接用这个式子。
    pub fn create_array(&mut self, n: usize, elem_size: usize, align: usize) -> usize {
        let pos = align_up(self.used, align.max(4));
        let total = 4 + elem_size * n;
        self.ensure(pos + total);
        self.used = pos + total;
        self.put_u32(pos, n as u32);
        pos
    }

    /// `MappedFile::CopyString(src, dest)`（`mapped_file.cc:158`）：
    /// 分配 `len + 1` 字节、拷贝、把结果写进 `dest` 的 OffsetPtr。返回字符串数据的位置。
    pub fn copy_string(&mut self, src: &str, dest_field_pos: usize) -> usize {
        let bytes = src.as_bytes();
        let data = self.allocate(bytes.len() + 1, 1);
        self.put_bytes(data, bytes);
        self.put_u8(data + bytes.len(), 0);
        self.set_offset(dest_field_pos, data);
        data
    }

    /// `dest->data = ptr`：把绝对地址 `target` 写成相对偏移。
    pub fn set_offset(&mut self, field_pos: usize, target: usize) {
        let offset = target as i64 - field_pos as i64;
        self.put_i32(field_pos, offset as i32);
    }

    /// `MappedFile::ShrinkToFit()`：把容量收到已用长度。
    pub fn shrink_to_fit(&mut self) {
        self.buf.truncate(self.used);
    }

    /// `MappedFile::Save()`：收尾 + 落盘（C++ 是 `ShrinkToFit` + munmap）。
    pub fn save(&mut self) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::other("mapped file is read-only"));
        }
        self.shrink_to_fit();
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.path, &self.buf)
    }

    /// 拿整个镜像（测试 / 校验用）。
    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }
}

/// 读 `metadata->format`（`char[32]`）并解析出版本号，对应 `prism.cc:105` /
/// `table.cc:290` 里那套 `strncmp` + `atof`。
///
/// 返回 `None` 表示前缀不匹配（`invalid metadata`）。
pub fn parse_format_version<'a>(format: &'a str, prefix: &str) -> Option<(f64, &'a str)> {
    let rest = format.strip_prefix(prefix)?;
    // C++ `atof`：前缀能解析多少算多少；Rust 的 `parse` 要求整串，所以只取前缀。
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .unwrap_or(rest.len());
    let version: f64 = rest[..end].parse().ok()?;
    Some((version, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_ptr_is_relative_to_its_own_field() {
        // 造一个"文件"：字段在 8，目标在 20 → offset = +12
        let mut f = MappedFile::create("unused", 64).unwrap();
        f.put_u32(8, 12);
        assert_eq!(f.deref(8), Some(20));
        // 负偏移也要能用
        f.put_u32(40, (-8i32) as u32);
        assert_eq!(f.deref(40), Some(32));
        // 0 = NULL
        f.put_u32(12, 0);
        assert_eq!(f.deref(12), None);
        // 越界 = None（C++ 这里是 UB）
        f.put_u32(16, 1_000);
        assert_eq!(f.deref(16), None);
    }

    #[test]
    fn array_and_list_layout() {
        let mut f = MappedFile::create("unused", 128).unwrap();
        // Array<i32>：size@pos，at[0]@pos+4
        let a = f.create_array(3, 4, 4);
        assert_eq!(f.u32_at(a), Some(3));
        assert_eq!(f.array_item(a, 0, 4), Some(a + 4));
        assert_eq!(f.array_item(a, 2, 4), Some(a + 12));
        // Array 总长 = 4 + 4*3 = 16
        assert_eq!(f.used(), a + 16);
        // List<i32>：size@pos，at 是偏移
        let l = f.allocate(8, 4);
        f.put_u32(l, 2);
        let data = f.allocate(8, 4);
        f.set_offset(l + 4, data);
        assert_eq!(f.list_data(l), Some(data));
        assert_eq!(f.list_item(l, 1, 4), Some(data + 4));
    }

    #[test]
    fn create_array_size_matches_cpp_formula() {
        // C++：sizeof(Array<T>) + sizeof(T)*(n-1) = (4+sizeof(T)) + sizeof(T)*(n-1)
        for (elem, align) in [(4usize, 4usize), (8, 4), (12, 4), (16, 4), (1, 1)] {
            for n in [0usize, 1, 5] {
                let mut f = MappedFile::create("unused", 4096).unwrap();
                let start = f.used();
                let pos = f.create_array(n, elem, align);
                assert_eq!(pos, start);
                assert_eq!(f.used() - start, 4 + elem * n, "elem={elem} n={n}");
            }
        }
    }

    #[test]
    fn copy_string_roundtrip() {
        let mut f = MappedFile::create("unused", 64).unwrap();
        let field = f.allocate(4, 4);
        let data = f.copy_string("你好", field);
        assert_eq!(f.u8_at(data + 6), Some(0));
        assert_eq!(f.string_at(field), Some("你好"));
    }

    #[test]
    fn format_version_parsing() {
        assert_eq!(
            parse_format_version("Rime::Prism/5.0", "Rime::Prism/"),
            Some((5.0, "5.0"))
        );
        assert_eq!(
            parse_format_version("Rime::Table/4.0", "Rime::Table/"),
            Some((4.0, "4.0"))
        );
        assert_eq!(parse_format_version("Rime::Table/4.0", "Rime::Prism/"), None);
        // C++ atof 能吃 "-5.0"；这里也要能
        assert_eq!(
            parse_format_version("Rime::Prism/-5.0", "Rime::Prism/"),
            Some((-5.0, "-5.0"))
        );
    }
}
