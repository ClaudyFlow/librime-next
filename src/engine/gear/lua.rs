//! rime-lua 兼容层：脚本发现 → 引导 → 组件解析 → 三类组件（processor /
//! translator / filter）真正挂进引擎。
//!
//! # 目录与脚本发现（2026-11-07 拍板：恒等式，无扁平）
//!
//! ```text
//! <root>/plugin/rime.lua             引导脚本（plugin 根下唯一允许的单文件）
//! <root>/plugin/a/b/c/a.b.c.lua      模块恒等式：id 按 '.' 分段，每段一级目录，
//!                                     文件名 = 完整 id
//! ```
//!
//! `lua/` 目录与一切扁平形态（`plugin/x.lua`、`plugin/x/init.lua`、
//! `plugin/<插件>/<其它>.lua`）**不再解析**。实现：自装 searcher **替换**
//! `package.searchers[2]`（package.path 搜寻器）—— 引导脚本往 package.path
//! 里写的扁平模式也找不到，布局规则在引擎层强制，不靠自觉。
//! 用户目录在前（可覆盖），共享目录在后。
//! 引导脚本（`rime.lua`）按「共享目录先、用户目录后」执行，用户那份可以
//! 覆盖官方注册。
//!
//! # 组件解析（`lua_translator@…` 的 `@` 后面那截）
//!
//! rime-lua 的两套约定都在用，必须都认：
//!
//! * `@*名字` —— 到 Lua **全局**里找（引导脚本 `x = require("x")` 注册的），
//!   点号分层（`@*yuhao.core` → `globals.yuhao.core`）；
//! * `@名字` —— `require("名字")` 的返回值。
//!
//! 拿到的既可以是**函数**本身，也可以是**模块表**（取 `translator` / `filter` /
//! `processor` 字段）—— 巧码的 `completion_gate` 返回 `M` 表，宇浩的模块也是表。
//!
//! # 一个组件一个 VM
//!
//! rime-lua 用进程级单 `lua_State`；这里每个组件实例独立一个 `Lua`。差别只在
//! "脚本里的全局变量是否跨组件共享" —— 社区脚本没有这种依赖（要共享的状态都存在
//! 自己的 `env` 里，`env` 是本组件持久的），换来的是互不污染、行为可复现。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use mlua::{Function, Lua, MultiValue, Table, Value};

use crate::engine::candidate::Candidate;
use crate::engine::context::Context;
use crate::engine::gear::filter::Filter;
use crate::engine::gear::processor::{ProcessResult, Processor};
use crate::engine::gear::translation::{Translation as TranslationTrait, VecTranslation};
use crate::engine::gear::translator::Translator;
use crate::engine::key_event::KeyEvent;
use crate::engine::runtime::Engine;
use crate::engine::segmentation::Segment;

// ---------------------------------------------------------------- 插件发现

/// 从方案 engine 段收集 lua 组件需求：`(组件类型, 组件名)`。
///
/// 组件名**保留** `*` 前缀（解析时要区分"全局"与"require"）。
pub fn collect_required_components(engine_config: &serde_json::Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let sections = [
        ("processors", "processor"),
        ("translators", "translator"),
        ("filters", "filter"),
    ];
    for (list_key, kind) in sections {
        let Some(list) = engine_config
            .pointer(&format!("/engine/{list_key}"))
            .and_then(|v| v.as_array())
        else {
            continue;
        };
        for item in list {
            let Some(spec) = item.as_str() else { continue };
            let Some((class, name_part)) = spec.split_once('@') else {
                continue;
            };
            if !class.starts_with("lua_") {
                continue;
            }
            let name = name_part.trim();
            if !name.is_empty() {
                out.push((kind.to_string(), name.to_string()));
            }
        }
    }
    out
}

/// 把一条候选流物化（`translation::drain` 是测试专用，这里要能在线用）。
fn collect_translation(mut translation: Box<dyn TranslationTrait>) -> Vec<Candidate> {
    let mut out = Vec::new();
    while let Some(candidate) = translation.peek() {
        out.push(candidate);
        translation.next();
    }
    out
}

/// 解析出来的组件：主函数 + 生命周期钩子。
struct ResolvedComponent {
    func: Function,
    init: Option<Function>,
    fini: Option<Function>,
}

impl ResolvedComponent {
    fn new(func: Function) -> Self {
        Self {
            func,
            init: None,
            fini: None,
        }
    }
}

// ---------------------------------------------------------------- 副作用通道

/// 脚本对引擎的写操作（C++ 里 lua 直接调 C API，这里先入队、调用结束统一执行）。
#[derive(Debug, Clone)]
enum Command {
    CommitText(String),
    ClearInput,
    SetOption(String, bool),
    /// `Context:set_property` —— 跨组件共享的组合属性（虎整句的已上屏/缓冲状态靠它）。
    SetProperty(String, String),
    PushInput(String),
    SetInput(String),
    ConfirmSelection,
    Select(usize),
    Highlight(usize),
    RefreshNonConfirmed,
}

/// 一次调用内共享的桥接状态（回调闭包通过 `Arc` 访问）。
#[derive(Default)]
struct Bridge {
    /// 脚本产生的副作用。
    commands: Mutex<Vec<Command>>,
    /// 方案配置树（`env.engine.schema.config` 的后端）。
    config: Mutex<serde_json::Value>,
    /// 选项快照（`ctx:get_option` 的后端）。
    options: Mutex<HashMap<String, bool>>,
    /// filter 输入候选的原件（回写时取回 `entry`，学词不能丢）。
    candidates: Arc<Mutex<Vec<Candidate>>>,
    /// 通知器回调：id / 类型（0=commit, 1=update, 2=option_update）/ 函数。
    /// 用 id 是为了 `connection:disconnect()` 能精确摘掉自己那一条。
    hooks: Mutex<Vec<(u64, u8, Function)>>,
    next_hook_id: Mutex<u64>,
    /// 上次观察到的提交条数（用于触发 `commit_notifier`）。
    commit_epoch: Mutex<u64>,
    /// `ctx:get_property` / `set_property` 的后端（每次 refresh_env 从 Context 同步）。
    properties: Mutex<HashMap<String, String>>,
    is_composing: Mutex<bool>,
    has_menu: Mutex<bool>,
    commit_text: Mutex<String>,
}

// ---------------------------------------------------------------- Lua 运行时

/// 一个 Lua VM + 已执行的引导脚本。
pub struct LuaHost {
    lua: Lua,
    /// 数据根（`[0]` 用户目录、`[1]` 共享目录，按 C++ `ResourceResolver` 序）。
    roots: Vec<PathBuf>,
    /// 执行过的引导脚本（诊断）。
    bootstrapped: Vec<PathBuf>,
    link: Arc<Bridge>,
}

/// 引导期注入的 Lua 辅助库：`Set` / `Vector` / `Candidate` / 候选与段的方法表。
///
/// * `Set` —— rime-lua 的"键即元素"集合，`segment.tags - Set({"x"})` 靠它；
/// * `Candidate(type, start, end, text, comment)` —— 翻译器产候选的约定构造器；
/// * `get_genuine` / `to_phrase` —— 候选方法（`to_phrase().code:print()` 是
///   `completion_gate` 数码长的唯一途径）。
const LUA_PRELUDE: &str = r#"
-- 照搬 hchunhui/librime-lua `src/lib/lua.cc::pmain`：
-- LuaJIT / Lua 5.1 只有全局 `unpack`，5.2+ 才改成 `table.unpack`。
-- 宇浩 `yuhao_embeded_cands.lua` 直接写 `table.unpack(args)`，缺这行会炸。
table.unpack = table.unpack or unpack

local set_mt = {}
set_mt.__index = set_mt
set_mt.__sub = function(a, b)
  local r = setmetatable({}, set_mt)
  for k in pairs(a) do if type(k) == "string" and not b[k] then r[k] = true end end
  return r
end
set_mt.__add = function(a, b)
  local r = setmetatable({}, set_mt)
  for k in pairs(a) do if type(k) == "string" then r[k] = true end end
  for k in pairs(b) do if type(k) == "string" then r[k] = true end end
  return r
end
-- 上游 `src/lib/lua.cc::SetReg` 还有交集(__mul)与 `empty()` 方法，
-- 社区脚本用 `tags * Set({...})` 判交集、`Set({}):empty()` 判空。
set_mt.__mul = function(a, b)
  local r = setmetatable({}, set_mt)
  for k in pairs(a) do if type(k) == "string" and b[k] then r[k] = true end end
  return r
end
function set_mt:empty() return next(self) == nil end
function Set(t)
  local s = setmetatable({}, set_mt)
  if t then for _, v in ipairs(t) do s[v] = true end end
  return s
end
function Vector(t) return t or {} end

-- 候选：字段可直接读写，方法在 metatable 上
candidate_mt = {}
candidate_mt.__index = candidate_mt
function candidate_mt:get_genuine() return self end
function candidate_mt:to_phrase()
  local code = self._code or ""
  return {
    text = self.text,
    code = { print = function() return code end },
  }
end
function Candidate(ctype, start, _end, text, comment)
  return setmetatable({
    type = ctype or "",
    start = start or 0,
    _end = _end or 0,
    text = text or "",
    comment = comment or "",
    preedit = "",
    _code = "",
  }, candidate_mt)
end

-- 段：字段直接读写；tags 是「数组 + 集合」双形态
segment_mt = {}
segment_mt.__index = segment_mt
function segment_mt:has_tag(tag)
  local tags = self.tags
  if not tags then return false end
  for _, v in ipairs(tags) do if v == tag then return true end end
  return tags[tag] == true
end
function segment_mt:get_selected_candidate() return nil end

-- rime 的 Config 对象模型：脚本用 `config:get_list(p).size`、
-- `list:get_value_at(i).value`、`map:get(k):get_list()`（宇浩的宏配置）。
-- 配置树由 Rust 侧转成普通 table，这里只负责套壳。
local function raw_array(t)
  if type(t) ~= "table" then return false end
  return #t > 0 or next(t) == nil
end
function __config_list(t)
  if type(t) ~= "table" then return nil end
  local list = {}
  for i = 1, #t do list[i] = t[i] end
  list.size = #t
  function list:get_value_at(i) return __config_value(t[i + 1]) end
  function list:get_at(i) return __config_value(t[i + 1]) end
  return list
end
function __config_map(t)
  if type(t) ~= "table" then return nil end
  local map = {}
  for k, v in pairs(t) do map[k] = v end
  function map:get(key) return __config_value(t[key]) end
  function map:get_list() return raw_array(t) and __config_list(t) or nil end
  return map
end
function __config_value(v)
  if type(v) ~= "table" then
    return {
      value = v,
      get_string = function() return type(v) == "string" and v or nil end,
      get_bool = function() return type(v) == "boolean" and v or nil end,
      get_int = function() return type(v) == "number" and math.floor(v) or nil end,
      get_list = function() return nil end,
      get_map = function() return nil end,
    }
  end
  return {
    value = nil,
    get_list = function() return raw_array(v) and __config_list(v) or nil end,
    get_map = function() return __config_map(v) end,
    get_string = function() return nil end,
    get_bool = function() return nil end,
    get_int = function() return nil end,
  }
end
"#;

/// 照搬 hchunhui/librime-lua `src/lib/lutf8lib-compat.c` + `xluaopen_utf8()`。
///
/// **LuaJIT 是 Lua 5.1 ABI，没有 `utf8` 库**；而本机 `plugin/yuhao/*`、
/// `plugin/tiger/*`、`plugin/input_statistics/*` 大量使用
/// `utf8.len / utf8.codes / utf8.char / utf8.codepoint`。只把 mlua 的 feature
/// 从 `lua54` 换成 `luajit`，这些脚本会整片报
/// `attempt to index a nil value (global 'utf8')` —— 这是比编译更容易踩的坑。
///
/// 上游是 C 版（Lua 5.3 `lutf8lib.c` 移植到 5.1）；这里等价实现成纯 Lua，
/// `boot()` 只在 `_G.utf8 == nil` 时注入，lua54 下完全不参与。
/// 转义统一用 `\ddd`（Lua 5.1 不认 `\xHH`），不用 `//` 整除运算符。
const LUA_UTF8_COMPAT: &str = r#"
if _G.utf8 == nil then
  local unpack_ = table.unpack or unpack
  local utf8 = {}
  utf8.charpattern = "[\0-\127\194-\244][\128-\191]*"

  local function encode(cp)
    if cp < 0x80 then
      return string.char(cp)
    elseif cp < 0x800 then
      return string.char(0xC0 + math.floor(cp / 64), 0x80 + cp % 64)
    elseif cp < 0x10000 then
      return string.char(0xE0 + math.floor(cp / 4096),
                         0x80 + math.floor(cp / 64) % 64,
                         0x80 + cp % 64)
    end
    return string.char(0xF0 + math.floor(cp / 262144),
                       0x80 + math.floor(cp / 4096) % 64,
                       0x80 + math.floor(cp / 64) % 64,
                       0x80 + cp % 64)
  end

  local function decode(s, pos)
    local b = s:byte(pos)
    if not b then return nil end
    if b < 0x80 then return b, pos + 1 end
    local n, cp
    if b < 0xE0 then n, cp = 1, b - 0xC0
    elseif b < 0xF0 then n, cp = 2, b - 0xE0
    elseif b < 0xF8 then n, cp = 3, b - 0xF0
    else return nil end
    for k = 1, n do
      local c = s:byte(pos + k)
      if not c or c < 0x80 or c > 0xBF then return nil end
      cp = cp * 64 + (c - 0x80)
    end
    return cp, pos + n + 1
  end

  function utf8.char(...)
    local out = {}
    for i = 1, select('#', ...) do
      local cp = select(i, ...)
      if type(cp) ~= 'number' or cp < 0 or cp > 0x10FFFF then
        error('value out of range', 2)
      end
      out[#out + 1] = encode(math.floor(cp))
    end
    return table.concat(out)
  end

  function utf8.codepoint(s, i, j)
    i = i or 1
    j = j or i
    if i < 0 then i = #s + 1 + i end
    if j < 0 then j = #s + 1 + j end
    local out, pos = {}, i
    while pos <= j do
      local cp, nextpos = decode(s, pos)
      if not cp then error('invalid UTF-8 code', 2) end
      out[#out + 1] = cp
      pos = nextpos
    end
    return unpack_(out)
  end

  function utf8.len(s, i, j)
    i = i or 1
    j = j or -1
    if i < 0 then i = #s + 1 + i end
    if j < 0 then j = #s + 1 + j end
    if i < 1 or i > #s + 1 then error('initial position out of bounds', 2) end
    local n, pos = 0, i
    while pos <= j do
      local cp, nextpos = decode(s, pos)
      if not cp then return nil, pos end
      n = n + 1
      pos = nextpos
    end
    return n
  end

  function utf8.offset(s, n, i)
    i = i or (n >= 0 and 1 or #s + 1)
    if i < 0 then i = #s + 1 + i end
    if n == 0 then
      local pos = i
      while pos > 1 do
        local b = s:byte(pos - 1)
        if b >= 0x80 and b <= 0xBF then pos = pos - 1 else break end
      end
      return pos
    elseif n > 0 then
      local pos = i
      for _ = 2, n do
        local _, nextpos = decode(s, pos)
        if not nextpos then return nil end
        pos = nextpos
      end
      return pos <= #s and pos or nil
    end
    local pos = i
    for _ = 1, -n do
      pos = pos - 1
      while pos > 1 do
        local b = s:byte(pos)
        if b >= 0x80 and b <= 0xBF then pos = pos - 1 else break end
      end
      if pos < 1 then return nil end
    end
    return pos
  end

  function utf8.codes(s)
    local pos = 1
    return function()
      if pos > #s then return nil end
      local cp, nextpos = decode(s, pos)
      if not cp then error('invalid UTF-8 code', 2) end
      local p = pos
      pos = nextpos
      return p, cp
    end
  end

  _G.utf8 = utf8
end
"#;

impl LuaHost {
    /// 启动 VM：装恒等式 searcher、注册 `rime_api`、执行引导脚本。
    pub fn boot(data_roots: &[PathBuf]) -> mlua::Result<Self> {
        let lua = Lua::new();
        let globals = lua.globals();
        let package: Table = globals.get("package")?;

        // 模块恒等式 searcher（2026-11-07）：`require("a.b.c")` →
        // `<root>/plugin/a/b/c/a.b.c.lua`。替换 `package.searchers[2]`
        // （package.path 搜寻器）⇒ 扁平 / `lua/` 布局在引擎层无法解析，
        // 引导脚本往 package.path 里加什么模式都无效。
        let roots = data_roots.to_vec();
        let searcher = lua.create_function(move |lua, module: String| {
            let segments = module.replace('.', "/");
            for root in &roots {
                let path = root
                    .join("plugin")
                    .join(&segments)
                    .join(format!("{module}.lua"));
                if path.is_file() {
                    let mut source = std::fs::read_to_string(&path)
                        .map_err(|error| mlua::Error::runtime(error.to_string()))?;
                    // `loadfile` 会跳过 UTF-8 BOM，`load(字符串)` 不会 ——
                    // 社区脚本（super_calculator）里真有带 BOM 的，这里对齐。
                    if source.starts_with('\u{feff}') {
                        source.remove(0);
                    }
                    let chunk = lua
                        .load(source)
                        .set_name(format!("@{}", path.display()))
                        .into_function()?;
                    return Ok((
                        Value::Function(chunk),
                        Value::String(lua.create_string(path.display().to_string())?),
                    ));
                }
            }
            let msg = format!("\n\tno file plugin/{segments}/{module}.lua（恒等式布局）");
            Ok((Value::Nil, Value::String(lua.create_string(msg)?)))
        })?;
        let searchers: Table = package.get("searchers")?;
        searchers.set(2, searcher)?;

        lua.load(LUA_PRELUDE).set_name("@prelude").exec()?;

        // 上游 `pmain()` 里的 `xluaopen_utf8(L)`：LuaJIT / Lua 5.1 没有 utf8 库。
        // lua54 下 `_G.utf8` 已存在，这段不会执行（零回归）。
        if matches!(globals.get::<Value>("utf8")?, Value::Nil) {
            lua.load(LUA_UTF8_COMPAT).set_name("@utf8_compat").exec()?;
        }

        // rime-lua 的约定：脚本里裸调 `yield(cand)` 就是 `coroutine.yield(cand)`。
        // 由 C++ 侧把这个全局别名装进去（脚本从来不写 `coroutine.yield`）。
        let coroutine: Table = globals.get("coroutine")?;
        let yield_fn: Function = coroutine.get("yield")?;
        globals.set("yield", yield_fn)?;

        // rime_api：引导脚本靠它拿目录（旧 rime.lua 里写死了这两个函数名）
        let api = lua.create_table()?;
        let mut names: Vec<(&str, Option<String>)> = vec![
            ("get_user_data_dir", data_roots.first().map(|p| to_lua_path(p))),
            ("get_shared_data_dir", data_roots.get(1).map(|p| to_lua_path(p))),
            ("get_config_dir", data_roots.first().map(|p| to_lua_path(p))),
        ];
        for (name, dir) in names.drain(..) {
            let Some(dir) = dir else { continue };
            api.set(
                name.to_string(),
                lua.create_function(move |_, ()| Ok(dir.clone()))?,
            )?;
        }
        // 上游 types.cc 的 rime_api 全家桶：社区脚本（input_statistics）真会调
        api.set(
            "get_distribution_code_name",
            lua.create_function(|_, ()| Ok("Weasel-Next".to_string()))?,
        )?;
        api.set(
            "get_distribution_name",
            lua.create_function(|_, ()| Ok("Weasel-Next".to_string()))?,
        )?;
        api.set(
            "get_distribution_version",
            lua.create_function(|_, ()| Ok("0.0.0".to_string()))?,
        )?;
        api.set(
            "get_version",
            lua.create_function(|_, ()| Ok("librime-next".to_string()))?,
        )?;
        api.set(
            "get_rime_version",
            lua.create_function(|_, ()| Ok("1.0.0-next".to_string()))?,
        )?;
        globals.set("rime_api", api)?;

        let mut host = Self {
            lua,
            roots: data_roots.to_vec(),
            bootstrapped: Vec::new(),
            link: Arc::new(Bridge::default()),
        };
        host.run_bootstrap();
        Ok(host)
    }

    /// 执行引导脚本：共享目录先、用户目录后（后者覆盖前者）。
    /// 只认 `<root>/plugin/rime.lua`（2026-11-07：plugin 根下唯一的单文件）。
    fn run_bootstrap(&mut self) {
        let mut scripts: Vec<PathBuf> = Vec::new();
        for root in self.roots.iter().rev() {
            let path = root.join("plugin").join("rime.lua");
            if path.is_file() {
                scripts.push(path);
            }
        }
        for path in scripts {
            match std::fs::read_to_string(&path) {
                Ok(source) => {
                    let name = format!("@{}", path.display());
                    match self.lua.load(&source).set_name(name).exec() {
                        Ok(()) => self.bootstrapped.push(path),
                        Err(error) => {
                            eprintln!("[lua] 引导脚本 {} 执行失败: {error}", path.display())
                        }
                    }
                }
                Err(error) => eprintln!("[lua] 引导脚本 {} 读取失败: {error}", path.display()),
            }
        }
    }

    /// 已执行的引导脚本（宿主日志用）。
    pub fn bootstrapped(&self) -> &[PathBuf] {
        &self.bootstrapped
    }

    /// 解析一个组件：`*名字` 走全局，`名字` 走 `require`；函数或模块表都行。
    ///
    /// 模块表里的 `init(env)` / `fini(env)` 一并取回 —— rime-lua 的组件生命周期
    /// 就靠这两个钩子（`completion_gate` 的 `min_syllables` 是在 `init` 里读的，
    /// 不调 `init` 它永远是 nil）。
    fn resolve_component(&self, kind: &str, name_space: &str) -> Option<ResolvedComponent> {
        let raw = name_space.trim();
        let (is_global, name) = match raw.strip_prefix('*') {
            Some(rest) => (true, rest),
            None => (false, raw),
        };
        if name.is_empty() {
            return None;
        }
        let value = if is_global {
            match self.globals_lookup(name) {
                Some(value) => value,
                // 全局里没有就退回 require（社区里两种写法混用）
                None => match self.require_module(name) {
                    Ok(value) => value,
                    Err(error) => {
                        eprintln!("[lua] 组件 `{name_space}`：全局没有、require 也失败: {error}");
                        return None;
                    }
                },
            }
        } else {
            match self.require_module(name) {
                Ok(value) => value,
                Err(error) => {
                    eprintln!("[lua] require(\"{name}\") 失败: {error}");
                    return None;
                }
            }
        };
        let field = match kind {
            "translator" => "translator",
            "filter" => "filter",
            "processor" => "processor",
            other => other,
        };
        match value {
            Value::Function(func) => Some(ResolvedComponent::new(func)),
            Value::Table(table) => {
                let func = match table.get::<Value>(field) {
                    Ok(Value::Function(func)) => func,
                    _ => match table.get::<Value>("func") {
                        Ok(Value::Function(func)) => func,
                        _ => {
                            eprintln!("[lua] 组件 `{name_space}` 的返回值里没有 `{field}` 函数");
                            return None;
                        }
                    },
                };
                let mut resolved = ResolvedComponent::new(func);
                if let Ok(Value::Function(init)) = table.get::<Value>("init") {
                    resolved.init = Some(init);
                }
                if let Ok(Value::Function(fini)) = table.get::<Value>("fini") {
                    resolved.fini = Some(fini);
                }
                Some(resolved)
            }
            _ => {
                eprintln!("[lua] 组件 `{name_space}` 的返回值不是函数也不是表");
                None
            }
        }
    }

    /// 按点号分层查全局（`yuhao.core` → `_G.yuhao.core`）。
    fn globals_lookup(&self, name: &str) -> Option<Value> {
        let mut current = Value::Table(self.lua.globals());
        // 上游 `sub_module_init` 用 `*` 分层（`Component*TableTranslator`），
        // 本地历史写法用 `.` 分层；两种都收，社区脚本不用改。
        for segment in name.split(|c| c == '.' || c == '*') {
            let Value::Table(table) = current else {
                return None;
            };
            current = table.get::<Value>(segment).ok()?;
        }
        match current {
            Value::Nil => None,
            other => Some(other),
        }
    }

    fn require_module(&self, name: &str) -> mlua::Result<Value> {
        let require: Function = self.lua.globals().get("require")?;
        require.call::<Value>(name.to_string())
    }

    // ------------------------------------------------------------ env 构造

    /// 建组件持久的 `env` 表（脚本往里面存 `env.first_time` / 连接器等）。
    fn make_env(&self, name_space: &str) -> mlua::Result<Table> {
        let lua = &self.lua;
        let env = lua.create_table()?;
        let engine = lua.create_table()?;
        let context = lua.create_table()?;
        let schema = lua.create_table()?;
        let config = lua.create_table()?;

        // ---- engine.commit_text ----
        let link = self.link.clone();
        engine.set(
            "commit_text",
            lua.create_function(move |_, (_this, text): (Value, String)| {
                if let Ok(mut queue) = link.commands.lock() {
                    queue.push(Command::CommitText(text));
                }
                Ok(())
            })?,
        )?;
        let link = self.link.clone();
        engine.set(
            "commit_composition",
            lua.create_function(move |_, _this: Value| {
                if let Ok(mut queue) = link.commands.lock() {
                    queue.push(Command::CommitText(String::new()));
                }
                Ok(())
            })?,
        )?;

        // ---- context ----
        let link = self.link.clone();
        context.set(
            "get_option",
            lua.create_function(move |_, (_this, name): (Value, String)| {
                Ok(link
                    .options
                    .lock()
                    .map(|options| options.get(&name).copied().unwrap_or(false))
                    .unwrap_or(false))
            })?,
        )?;
        let link = self.link.clone();
        context.set(
            "set_option",
            lua.create_function(move |_, (_this, name, value): (Value, String, bool)| {
                if let Ok(mut queue) = link.commands.lock() {
                    queue.push(Command::SetOption(name, value));
                }
                Ok(())
            })?,
        )?;
        let link = self.link.clone();
        context.set(
            "clear",
            lua.create_function(move |_, _this: Value| {
                if let Ok(mut queue) = link.commands.lock() {
                    queue.push(Command::ClearInput);
                }
                Ok(())
            })?,
        )?;
        // ---- Context 属性与编辑 API（虎整句 1.6 依赖；C++ librime-lua 同名） ----
        let link = self.link.clone();
        context.set(
            "get_property",
            lua.create_function(move |_, (_this, name): (Value, String)| {
                Ok(link
                    .properties
                    .lock()
                    .ok()
                    .and_then(|props| props.get(&name).cloned())
                    .unwrap_or_default())
            })?,
        )?;
        let link = self.link.clone();
        context.set(
            "set_property",
            lua.create_function(move |_, (_this, name, value): (Value, String, String)| {
                if let Ok(mut props) = link.properties.lock() {
                    props.insert(name.clone(), value.clone());
                }
                if let Ok(mut queue) = link.commands.lock() {
                    queue.push(Command::SetProperty(name, value));
                }
                Ok(())
            })?,
        )?;
        let link = self.link.clone();
        context.set(
            "is_composing",
            lua.create_function(move |_, _this: Value| {
                Ok(link.is_composing.lock().map(|v| *v).unwrap_or(false))
            })?,
        )?;
        let link = self.link.clone();
        context.set(
            "has_menu",
            lua.create_function(move |_, _this: Value| {
                Ok(link.has_menu.lock().map(|v| *v).unwrap_or(false))
            })?,
        )?;
        let link = self.link.clone();
        context.set(
            "get_commit_text",
            lua.create_function(move |_, _this: Value| {
                Ok(link
                    .commit_text
                    .lock()
                    .map(|v| v.clone())
                    .unwrap_or_default())
            })?,
        )?;
        let link = self.link.clone();
        context.set(
            "push_input",
            lua.create_function(move |_, (_this, text): (Value, String)| {
                if let Ok(mut queue) = link.commands.lock() {
                    queue.push(Command::PushInput(text));
                }
                Ok(())
            })?,
        )?;
        let link = self.link.clone();
        context.set(
            "set_input",
            lua.create_function(move |_, (_this, text): (Value, String)| {
                if let Ok(mut queue) = link.commands.lock() {
                    queue.push(Command::SetInput(text));
                }
                Ok(())
            })?,
        )?;
        let link = self.link.clone();
        context.set(
            "confirm_current_selection",
            lua.create_function(move |_, _this: Value| {
                if let Ok(mut queue) = link.commands.lock() {
                    queue.push(Command::ConfirmSelection);
                }
                Ok(true)
            })?,
        )?;
        let link = self.link.clone();
        context.set(
            "select",
            lua.create_function(move |_, (_this, index): (Value, usize)| {
                if let Ok(mut queue) = link.commands.lock() {
                    queue.push(Command::Select(index));
                }
                Ok(true)
            })?,
        )?;
        let link = self.link.clone();
        context.set(
            "highlight",
            lua.create_function(move |_, (_this, index): (Value, usize)| {
                if let Ok(mut queue) = link.commands.lock() {
                    queue.push(Command::Highlight(index));
                }
                Ok(true)
            })?,
        )?;
        let link = self.link.clone();
        context.set(
            "refresh_non_confirmed_composition",
            lua.create_function(move |_, _this: Value| {
                if let Ok(mut queue) = link.commands.lock() {
                    queue.push(Command::RefreshNonConfirmed);
                }
                Ok(true)
            })?,
        )?;
        // 三个通知器：connect 返回带 `disconnect` 的连接对象（librime-lua 语义）。
        // 虎整句 1.6 的 prepare_learning 会依次 connect 四个回调、重连前先 disconnect。
        context.set("commit_notifier", self.make_notifier(0)?)?;
        context.set("update_notifier", self.make_notifier(1)?)?;
        context.set("option_update_notifier", self.make_notifier(2)?)?;
        engine.set("context", context)?;

        // ---- schema.config ----
        let link = self.link.clone();
        config.set(
            "get_string",
            lua.create_function(move |_, (_this, path): (Value, String)| {
                Ok(link.config.lock().ok().and_then(|tree| {
                    tree.pointer(&json_pointer(&path))
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                }))
            })?,
        )?;
        let link = self.link.clone();
        config.set(
            "get_int",
            lua.create_function(move |_, (_this, path): (Value, String)| {
                Ok(link.config.lock().ok().and_then(|tree| {
                    tree.pointer(&json_pointer(&path))
                        .and_then(|v| v.as_i64())
                        .map(|v| v as i32)
                }))
            })?,
        )?;
        let link = self.link.clone();
        config.set(
            "get_double",
            lua.create_function(move |_, (_this, path): (Value, String)| {
                Ok(link.config.lock().ok().and_then(|tree| {
                    tree.pointer(&json_pointer(&path)).and_then(|v| v.as_f64())
                }))
            })?,
        )?;
        let link = self.link.clone();
        config.set(
            "get_bool",
            lua.create_function(move |_, (_this, path): (Value, String)| {
                Ok(link.config.lock().ok().and_then(|tree| {
                    tree.pointer(&json_pointer(&path)).and_then(|v| v.as_bool())
                }))
            })?,
        )?;
        // `get_list` / `get_map` / `get_value_at` 返回 rime 的 Config 对象
        // （`list.size` + `list:get_value_at(i).value`），不是裸 Lua 数组 ——
        // 社区脚本全靠这套字段，返回裸表会直接 `attempt to perform arithmetic on nil`。
        let link = self.link.clone();
        config.set(
            "get_list",
            lua.create_function(move |lua, (_this, path): (Value, String)| {
                let items = link
                    .config
                    .lock()
                    .ok()
                    .and_then(|tree| {
                        tree.pointer(&json_pointer(&path))
                            .and_then(|v| v.as_array())
                            .cloned()
                    });
                let Some(items) = items else {
                    return Ok(Value::Nil);
                };
                let raw = json_to_lua(lua, &serde_json::Value::Array(items))?;
                let wrap: Function = lua.globals().get("__config_list")?;
                wrap.call::<Value>(raw)
            })?,
        )?;
        let link = self.link.clone();
        config.set(
            "get_map",
            lua.create_function(move |lua, (_this, path): (Value, String)| {
                let node = link
                    .config
                    .lock()
                    .ok()
                    .and_then(|tree| tree.pointer(&json_pointer(&path)).cloned());
                let Some(node) = node else {
                    return Ok(Value::Nil);
                };
                if !node.is_object() {
                    return Ok(Value::Nil);
                }
                let raw = json_to_lua(lua, &node)?;
                let wrap: Function = lua.globals().get("__config_map")?;
                wrap.call::<Value>(raw)
            })?,
        )?;
        let link = self.link.clone();
        config.set(
            "get_value_at",
            lua.create_function(move |lua, (_this, path): (Value, String)| {
                let node = link
                    .config
                    .lock()
                    .ok()
                    .and_then(|tree| tree.pointer(&json_pointer(&path)).cloned());
                let Some(node) = node else {
                    return Ok(Value::Nil);
                };
                let raw = json_to_lua(lua, &node)?;
                let wrap: Function = lua.globals().get("__config_value")?;
                wrap.call::<Value>(raw)
            })?,
        )?;
        schema.set("config", config)?;
        schema.set("schema_id", "")?;
        engine.set("schema", schema)?;

        env.set("engine", engine)?;
        env.set("name_space", name_space)?;
        env.set("first_time", true)?;
        Ok(env)
    }

    /// 建一个通知器：`connect(func)` → 连接对象，`connection:disconnect()` 摘掉回调。
    /// `kind`：0=commit（会在 [`LuaHost::fire_commit_hooks`] 里触发）／1=update／2=option_update。
    fn make_notifier(&self, kind: u8) -> mlua::Result<Table> {
        let lua = &self.lua;
        let table = lua.create_table()?;
        let link = self.link.clone();
        table.set(
            "connect",
            lua.create_function(move |lua, (_this, func): (Value, Function)| {
                let id = link
                    .next_hook_id
                    .lock()
                    .map(|mut next| {
                        *next += 1;
                        *next
                    })
                    .unwrap_or(1);
                if let Ok(mut hooks) = link.hooks.lock() {
                    hooks.push((id, kind, func));
                }
                let conn = lua.create_table()?;
                let link2 = link.clone();
                conn.set(
                    "disconnect",
                    lua.create_function(move |_, _this: Value| {
                        if let Ok(mut hooks) = link2.hooks.lock() {
                            hooks.retain(|(hook_id, _, _)| *hook_id != id);
                        }
                        Ok(())
                    })?,
                )?;
                Ok(conn)
            })?,
        )?;
        Ok(table)
    }

    /// 每次调用前刷新 env 里的"快照"字段（input / options / schema_id）。
    fn refresh_env(&self, env: &Table, ctx: &CtxSnapshot, config: &serde_json::Value) {
        if let Ok(mut slot) = self.link.config.lock() {
            *slot = config.clone();
        }
        if let Ok(mut options) = self.link.options.lock() {
            *options = ctx.options.clone();
        }
        if let Ok(mut props) = self.link.properties.lock() {
            *props = ctx.properties.clone();
        }
        if let Ok(mut flag) = self.link.is_composing.lock() {
            *flag = ctx.is_composing;
        }
        if let Ok(mut flag) = self.link.has_menu.lock() {
            *flag = ctx.has_menu;
        }
        if let Ok(mut text) = self.link.commit_text.lock() {
            *text = ctx.commit_text.clone();
        }
        let Ok(engine) = env.get::<Table>("engine") else {
            return;
        };
        if let Ok(context) = engine.get::<Table>("context") {
            let _ = context.set("input", ctx.input.clone());
            let _ = context.set("caret_pos", ctx.caret_pos as i64);
            let _ = context.set("composition", self.make_composition(ctx));
        }
        if let Ok(schema) = engine.get::<Table>("schema") {
            let schema_id = config
                .pointer("/schema/schema_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let _ = schema.set("schema_id", schema_id);
        }
    }

    /// `ctx.composition` 的表视图：`:back()` / `:empty()`。
    fn make_composition(&self, ctx: &CtxSnapshot) -> Table {
        let Ok(table) = self.lua.create_table() else {
            return unwrap_or_empty(self.lua.create_table().ok());
        };
        let segment = ctx.segment.clone();
        let _ = table.set("input", ctx.input.clone());
        let _ = table.set("length", ctx.input.chars().count() as i64);
        let _ = table.set("cursor_pos", ctx.caret_pos as i64);
        let back = segment.clone();
        let _ = table.set(
            "back",
            match self.lua.create_function(move |lua, _this: Value| {
                match &back {
                    Some(view) => build_segment_table(lua, view).map(Value::Table),
                    None => Ok(Value::Nil),
                }
            }) {
                Ok(func) => func,
                Err(_) => return table,
            },
        );
        let empty = segment.is_none();
        let _ = table.set(
            "empty",
            match self.lua.create_function(move |_, _this: Value| Ok(empty)) {
                Ok(func) => func,
                Err(_) => return table,
            },
        );
        table
    }

    /// 执行脚本产生的副作用（`engine` 为 `None` 时只清队）。
    fn flush(&self, mut engine: Option<&mut Engine>) -> Vec<Command> {
        let commands = self
            .link
            .commands
            .lock()
            .map(|mut queue| std::mem::take(&mut *queue))
            .unwrap_or_default();
        for command in &commands {
            match command {
                Command::CommitText(text) => {
                    if let Some(engine) = engine.as_deref_mut() {
                        engine.context.commit_direct(text.clone());
                        engine.context.clear();
                    }
                }
                Command::ClearInput => {
                    if let Some(engine) = engine.as_deref_mut() {
                        engine.context.clear();
                    }
                }
                Command::SetOption(name, value) => {
                    if let Some(engine) = engine.as_deref_mut() {
                        engine.context.set_option(name, *value);
                    }
                }
                Command::SetProperty(name, value) => {
                    if let Some(engine) = engine.as_deref_mut() {
                        engine.context.set_property(name, value);
                    }
                }
                Command::PushInput(text) => {
                    if let Some(engine) = engine.as_deref_mut() {
                        engine.context.push_input_str(text);
                    }
                }
                Command::SetInput(text) => {
                    if let Some(engine) = engine.as_deref_mut() {
                        engine.context.set_input(text);
                    }
                }
                Command::ConfirmSelection => {
                    if let Some(engine) = engine.as_deref_mut() {
                        engine.context.confirm_current_selection();
                    }
                }
                Command::Select(index) => {
                    if let Some(engine) = engine.as_deref_mut() {
                        engine.context.select(*index);
                    }
                }
                Command::Highlight(index) => {
                    if let Some(engine) = engine.as_deref_mut() {
                        engine.context.highlight(*index);
                    }
                }
                Command::RefreshNonConfirmed => {
                    if let Some(engine) = engine.as_deref_mut() {
                        engine.context.refresh_non_confirmed_composition();
                    }
                }
            }
        }
        commands
    }

    /// `commit_notifier` 的触发：提交条数变了就回调一遍。
    fn fire_commit_hooks(&self, commit_len: usize, env: &Table) {
        let epoch = commit_len as u64;
        let changed = self
            .link
            .commit_epoch
            .lock()
            .map(|mut last| {
                if *last == epoch {
                    false
                } else {
                    *last = epoch;
                    true
                }
            })
            .unwrap_or(false);
        if !changed {
            return;
        }
        // 回调签名是 `function(ctx)`：把 env.engine.context 传进去，
        // 虎整句的学习回调会调 ctx:is_composing() / ctx:get_commit_text()。
        // 先复制一份函数列表再调用，避免回调里 connect/disconnect 与这里的锁打架。
        let ctx_value = env
            .get::<Table>("engine")
            .and_then(|engine| engine.get::<Table>("context"))
            .map(Value::Table)
            .unwrap_or(Value::Nil);
        let callbacks: Vec<Function> = match self.link.hooks.lock() {
            Ok(hooks) => hooks
                .iter()
                .filter(|(_, kind, _)| *kind == 0)
                .map(|(_, _, func)| func.clone())
                .collect(),
            Err(_) => return,
        };
        for hook in callbacks {
            if let Err(error) = hook.call::<()>(ctx_value.clone()) {
                eprintln!("[lua] commit_notifier 回调失败: {error}");
            }
        }
    }

    // ------------------------------------------------------------ 兼容入口

    /// 让组件结构体持有同一份 VM 的"视图"（`Lua` 本身是引用计数的，克隆零成本）。
    fn reborrow(&self) -> LuaHost {
        LuaHost {
            lua: self.lua.clone(),
            roots: self.roots.clone(),
            bootstrapped: self.bootstrapped.clone(),
            link: self.link.clone(),
        }
    }

    /// 旧接口：直接跑一个 translator 组件，返回 `(text, comment)` 列表。
    ///
    /// 保留给 `example/lua_host_test.rs`；引擎内部走 [`LuaTranslator`]。
    pub fn run_translator(
        &self,
        component: &str,
        input: &str,
        seg_start: usize,
        seg_end: usize,
        context_input: &str,
        config: &serde_json::Value,
    ) -> Vec<(String, String)> {
        let Some(resolved) = self.resolve_component("translator", component) else {
            return Vec::new();
        };
        let Ok(env) = self.make_env(component) else {
            return Vec::new();
        };
        let mut translator = LuaTranslator {
            host: self.reborrow(),
            func: resolved.func,
            init: resolved.init,
            env,
            inited: false,
            name_space: component.to_string(),
            config: config.clone(),
            context: CtxSnapshot::default(),
        };
        translator.context.input = context_input.to_string();
        let mut segment = Segment::new(seg_start, seg_end);
        let Some(translation) = translator.query(input, &mut segment, &Default::default()) else {
            return Vec::new();
        };
        collect_translation(translation)
            .into_iter()
            .map(|c| (c.text, c.comment))
            .collect()
    }

    /// 旧接口：直接跑一个 filter 组件。
    pub fn run_filter(
        &self,
        component: &str,
        items: Vec<Candidate>,
        config: &serde_json::Value,
        ctx: &Context,
    ) -> Vec<Candidate> {
        let Some(resolved) = self.resolve_component("filter", component) else {
            return items;
        };
        let Ok(env) = self.make_env(component) else {
            return items;
        };
        let mut filter = LuaFilter {
            host: self.reborrow(),
            func: resolved.func,
            init: resolved.init,
            env,
            inited: false,
            name_space: component.to_string(),
            config: config.clone(),
        };
        filter.run(items, ctx)
    }
}

/// 兜底的空表（`create_table` 理论上不会失败）。
fn unwrap_or_empty(table: Option<Table>) -> Table {
    table.unwrap_or_else(|| unreachable!("Lua::create_table 失败"))
}

/// 路径转 Lua 的 `/` 分隔形式（Windows 下反斜杠会在 Lua 字符串里被转义）。
fn to_lua_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// 把一段 JSON 配置子树转成 Lua 值（`get_list` / `get_map` 的后端）。
fn json_to_lua(lua: &Lua, value: &serde_json::Value) -> mlua::Result<Value> {
    Ok(match value {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(flag) => Value::Boolean(*flag),
        serde_json::Value::Number(number) => match number.as_i64() {
            Some(int) => Value::Integer(int),
            None => Value::Number(number.as_f64().unwrap_or(0.0)),
        },
        serde_json::Value::String(text) => Value::String(lua.create_string(text)?),
        serde_json::Value::Array(items) => {
            let table = lua.create_table()?;
            for (index, item) in items.iter().enumerate() {
                table.set(index + 1, json_to_lua(lua, item)?)?;
            }
            Value::Table(table)
        }
        serde_json::Value::Object(map) => {
            let table = lua.create_table()?;
            for (key, item) in map {
                table.set(key.as_str(), json_to_lua(lua, item)?)?;
            }
            Value::Table(table)
        }
    })
}

/// 配置路径（`a/b/c`）转 JSON Pointer（`/a/b/c`）。
fn json_pointer(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

/// `Context` 的纯数据快照。
///
/// 组件要 `Send + Sync`，而 `Context.composition.menu` 里藏着
/// `RefCell<Option<Box<dyn Translation>>>` —— 不能塞进结构体。脚本真正用到的
/// 只有 input / options / caret / 最后一段 / 提交条数，快照这几样就够了。
#[derive(Clone, Default)]
struct CtxSnapshot {
    input: String,
    caret_pos: usize,
    options: HashMap<String, bool>,
    properties: HashMap<String, String>,
    is_composing: bool,
    has_menu: bool,
    commit_text: String,
    /// `composition:back()` 对应的最后一段。
    segment: Option<SegmentView>,
    /// 提交历史条数（`commit_notifier` 靠它判断"有没有新提交"）。
    commit_len: usize,
}

impl CtxSnapshot {
    fn from_context(ctx: &Context) -> Self {
        Self {
            input: ctx.input.clone(),
            caret_pos: ctx.caret_pos,
            options: ctx.options.clone(),
            properties: ctx.properties.clone(),
            is_composing: ctx.is_composing(),
            has_menu: ctx.has_menu(),
            commit_text: ctx.get_commit_text(),
            segment: ctx.composition.segmentation.segments.last().map(segment_view),
            commit_len: ctx.commit_history.len(),
        }
    }
}

/// Segment 的中间表示（构造 lua 表用）。
#[derive(Clone)]
struct SegmentView {
    start: usize,
    end: usize,
    tags: Vec<String>,
    prompt: String,
    selected_index: usize,
}

fn segment_view(segment: &Segment) -> SegmentView {
    SegmentView {
        start: segment.start,
        end: segment.end,
        tags: segment.tags.clone(),
        prompt: segment.prompt.clone(),
        selected_index: segment.selected_index,
    }
}

/// 把 Segment 视图变成 lua 表。
///
/// `tags` 同时是**数组**和**集合**：数组部分供 `ipairs`，键部分供
/// `segment.tags - Set({...})`（rime-lua 的 Set 语义）。读回时两边都收。
fn build_segment_table(lua: &Lua, view: &SegmentView) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("start", view.start as i64)?;
    table.set("_end", view.end as i64)?;
    table.set("prompt", view.prompt.clone())?;
    table.set("selected_index", view.selected_index as i64)?;
    let tags = lua.create_table()?;
    for (index, tag) in view.tags.iter().enumerate() {
        tags.set(index + 1, tag.clone())?;
        tags.set(tag.clone(), true)?;
    }
    table.set("tags", tags)?;
    let meta: Table = lua.globals().get("segment_mt")?;
    table.set_metatable(Some(meta));
    Ok(table)
}

/// 从 lua 表读回 tags（数组部分 + 键部分，去重）。
fn read_tags(table: &Table) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Ok(tags) = table.get::<Table>("tags") {
        for value in tags.clone().sequence_values::<String>() {
            if let Ok(tag) = value
                && !out.contains(&tag)
            {
                out.push(tag);
            }
        }
        for pair in tags.pairs::<Value, Value>() {
            if let Ok((Value::String(key), _)) = pair {
                let tag = key.to_string_lossy().to_string();
                if !out.contains(&tag) {
                    out.push(tag);
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------- 候选桥

/// 把 Rust 候选变成 lua 表（字段 + `candidate_mt` 上的方法）。
///
/// `store_id` 是它在 [`Bridge::candidates`] 里的下标：脚本改了 `text` / `comment`
/// 之后，回写时靠它把原始 `entry`（学词用）接回去。
fn candidate_table(lua: &Lua, cand: &Candidate, store_id: Option<usize>) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("text", cand.text.clone())?;
    table.set("comment", cand.comment.clone())?;
    table.set("preedit", cand.preedit.clone())?;
    table.set("type", cand.kind.clone())?;
    table.set("start", cand.start as i64)?;
    table.set("_end", cand.end as i64)?;
    table.set("quality", cand.quality)?;
    // `to_phrase().code:print()` 要用词条编码（`"1,2,3"` 形式）
    table.set(
        "_code",
        cand.entry
            .as_ref()
            .map(|entry| entry.code.to_string())
            .unwrap_or_default(),
    )?;
    if let Some(id) = store_id {
        table.set("_id", id as i64)?;
    }
    let meta: Table = lua.globals().get("candidate_mt")?;
    table.set_metatable(Some(meta));
    Ok(table)
}

/// 从 lua 表读回候选；`entry` 从原件里接回来（学词不能丢）。
fn candidate_from_table(table: &Table, store: &Mutex<Vec<Candidate>>) -> Option<Candidate> {
    let text: String = table.get("text").ok()?;
    let id: Option<i64> = table.get("_id").ok();
    let mut candidate = match id.and_then(|id| {
        store
            .lock()
            .ok()
            .and_then(|items| items.get(id as usize).cloned())
    }) {
        Some(original) => original,
        None => Candidate::new(
            &table
                .get::<String>("type")
                .unwrap_or_else(|_| "lua".to_string()),
            table.get::<i64>("start").unwrap_or(0) as usize,
            table.get::<i64>("_end").unwrap_or(0) as usize,
            text.clone(),
            1.0,
        ),
    };
    candidate.text = text;
    if let Ok(value) = table.get::<Value>("comment") {
        candidate.comment = lua_scalar_to_string(&value);
    }
    if let Ok(value) = table.get::<Value>("preedit") {
        candidate.preedit = lua_scalar_to_string(&value);
    }
    if let Ok(kind) = table.get::<String>("type")
        && !kind.is_empty()
    {
        candidate.kind = kind;
    }
    if let Ok(quality) = table.get::<f64>("quality") {
        candidate.quality = quality;
    }
    if let Ok(start) = table.get::<i64>("start") {
        candidate.start = start as usize;
    }
    if let Ok(end) = table.get::<i64>("_end") {
        candidate.end = end as usize;
    }
    Some(candidate)
}

/// Lua 标量转字符串（注释可能是数字，巧码的 `number_translator` 就传数字）。
fn lua_scalar_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.to_string_lossy().to_string(),
        Value::Integer(number) => number.to_string(),
        Value::Number(number) => number.to_string(),
        Value::Boolean(flag) => flag.to_string(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------- 协程驱动

/// 跑一个 rime-lua 协程：`resume` 到结束，收集每批 `yield` 出来的值。
fn drive_coroutine(
    lua: &Lua,
    func: Function,
    args: MultiValue,
    on_value: &mut dyn FnMut(Value),
) -> mlua::Result<()> {
    let thread = lua.create_thread(func)?;
    let mut pending = Some(args);
    loop {
        let result = match pending.take() {
            Some(args) => thread.resume::<MultiValue>(args),
            None => thread.resume::<MultiValue>(()),
        };
        match result {
            Ok(values) => {
                let empty = values.is_empty();
                for value in values {
                    on_value(value);
                }
                if empty {
                    break;
                }
            }
            Err(error) => {
                let text = error.to_string();
                // 协程跑完后再 resume 会报 dead coroutine —— 那是结束，不是错误
                if !text.contains("cannot resume dead coroutine") {
                    eprintln!("[lua] 脚本执行失败: {text}");
                }
                break;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- translator

/// `lua_translator@…` → Rust `Translator`。
pub struct LuaTranslator {
    host: LuaHost,
    func: Function,
    init: Option<Function>,
    env: Table,
    inited: bool,
    name_space: String,
    /// 方案配置树（translator 拿不到 ticket，构造时存一份）。
    config: serde_json::Value,
    /// 最近一次 `sync_context` 的快照（脚本读 `ctx.input` / `get_option`）。
    context: CtxSnapshot,
}

impl LuaTranslator {
    pub fn create(data_roots: &[PathBuf], name_space: &str, config: serde_json::Value) -> Option<Self> {
        let host = match LuaHost::boot(data_roots) {
            Ok(host) => host,
            Err(error) => {
                eprintln!("[lua] VM 启动失败: {error}");
                return None;
            }
        };
        let resolved = host.resolve_component("translator", name_space)?;
        let env = host.make_env(name_space).ok()?;
        Some(Self {
            host,
            func: resolved.func,
            init: resolved.init,
            env,
            inited: false,
            name_space: name_space.to_string(),
            config,
            context: CtxSnapshot::default(),
        })
    }

    fn ensure_init(&mut self) {
        // 脚本里的 `env.first_time` 与 `init(env)` 是两种约定，都支持
        let first_time = !self.inited;
        self.inited = true;
        let _ = self.env.set("first_time", first_time);
        if first_time
            && let Some(init) = self.init.take()
            && let Err(error) = init.call::<()>(self.env.clone())
        {
            eprintln!("[lua] translator `{}` init 失败: {error}", self.name_space);
        }
    }
}

impl Translator for LuaTranslator {
    fn sync_context(&mut self, ctx: &Context) {
        self.context = CtxSnapshot::from_context(ctx);
    }

    fn query(
        &mut self,
        input: &str,
        segment: &mut Segment,
        _history: &crate::engine::commit_history::CommitHistory,
    ) -> Option<Box<dyn TranslationTrait>> {
        let config = self.config.clone();
        let ctx = self.context.clone();
        self.host.refresh_env(&self.env, &ctx, &config);
        self.ensure_init();

        let view = segment_view(segment);
        let Ok(seg_table) = build_segment_table(&self.host.lua, &view) else {
            return None;
        };
        let lua = &self.host.lua;
        let store = self.host.link.candidates.clone();
        let mut items: Vec<Candidate> = Vec::new();
        {
            let mut on_value = |value: Value| {
                if let Value::Table(table) = value
                    && let Some(candidate) = candidate_from_table(&table, &store)
                {
                    items.push(candidate);
                }
            };
            let input_string = match lua.create_string(input) {
                Ok(value) => value,
                Err(_) => return None,
            };
            let args = MultiValue::from_vec(vec![
                Value::String(input_string),
                Value::Table(seg_table.clone()),
                Value::Table(self.env.clone()),
            ]);
            if let Err(error) = drive_coroutine(lua, self.func.clone(), args, &mut on_value) {
                eprintln!("[lua] translator `{}` 失败: {error}", self.name_space);
            }
        }
        // 脚本可能改写段（prompt / tags / selected_index）
        if let Ok(prompt) = seg_table.get::<String>("prompt") {
            segment.prompt = prompt;
        }
        let tags = read_tags(&seg_table);
        if !tags.is_empty() {
            segment.tags = tags;
        }
        self.host.fire_commit_hooks(ctx.commit_len, &self.env);
        Some(Box::new(VecTranslation::new(items)))
    }
}

// ---------------------------------------------------------------- filter

/// `lua_filter@…` → Rust `Filter`。
pub struct LuaFilter {
    host: LuaHost,
    func: Function,
    init: Option<Function>,
    env: Table,
    inited: bool,
    name_space: String,
    config: serde_json::Value,
}

impl LuaFilter {
    pub fn create(data_roots: &[PathBuf], name_space: &str, config: serde_json::Value) -> Option<Self> {
        let host = match LuaHost::boot(data_roots) {
            Ok(host) => host,
            Err(error) => {
                eprintln!("[lua] VM 启动失败: {error}");
                return None;
            }
        };
        let resolved = host.resolve_component("filter", name_space)?;
        let env = host.make_env(name_space).ok()?;
        Some(Self {
            host,
            func: resolved.func,
            init: resolved.init,
            env,
            inited: false,
            name_space: name_space.to_string(),
            config,
        })
    }

    fn ensure_init(&mut self) {
        let first_time = !self.inited;
        self.inited = true;
        let _ = self.env.set("first_time", first_time);
        if first_time
            && let Some(init) = self.init.take()
            && let Err(error) = init.call::<()>(self.env.clone())
        {
            eprintln!("[lua] filter `{}` init 失败: {error}", self.name_space);
        }
    }

    /// 全量跑一遍脚本：`function filter(input, env)`。
    fn run(&mut self, items: Vec<Candidate>, ctx: &Context) -> Vec<Candidate> {
        let config = self.config.clone();
        let snapshot = CtxSnapshot::from_context(ctx);
        let input_text = ctx.input.clone();
        self.host.refresh_env(&self.env, &snapshot, &config);
        self.ensure_init();

        let lua = &self.host.lua;
        // 候选原件存进 bridge，表里只放 _id（回写时取回 entry）
        if let Ok(mut store) = self.host.link.candidates.lock() {
            *store = items.clone();
        }
        let store = self.host.link.candidates.clone();
        let Ok(input_table) = lua.create_table() else {
            return items;
        };
        // input 对象：rime-lua 把输入串包了一层，脚本既用 `input:sub(1,1)`
        // 也用 `input:match(...)`、`input:gsub(...)`，都转到 Lua 字符串库上
        for (name, arity) in [("sub", 2usize), ("match", 1), ("gsub", 2), ("find", 1)] {
            let text = input_text.clone();
            let function = lua.create_function(move |lua, args: MultiValue| {
                let string_lib: Table = lua.globals().get("string")?;
                let func: Function = string_lib.get(name)?;
                let mut call_args: Vec<Value> = Vec::with_capacity(1 + arity);
                call_args.push(Value::String(lua.create_string(&text)?));
                for index in 0..arity {
                    call_args.push(args.get(index + 1).cloned().unwrap_or(Value::Nil));
                }
                func.call::<MultiValue>(MultiValue::from_vec(call_args))
            });
            if let Ok(function) = function {
                let _ = input_table.set(name, function);
            }
        }
        // iter()：每次调用返回一个带独立游标的迭代器闭包
        let pool = Arc::new(items.clone());
        let store_for_iter = store.clone();
        if let Ok(iter) = lua.create_function(move |lua, _this: Value| {
            let cursor = Arc::new(Mutex::new(0usize));
            let pool = pool.clone();
            let store = store_for_iter.clone();
            lua.create_function(move |lua, ()| {
                let index = {
                    let mut cursor = cursor.lock().unwrap_or_else(|e| e.into_inner());
                    let index = *cursor;
                    *cursor += 1;
                    index
                };
                let Some(candidate) = pool.get(index) else {
                    return Ok(Value::Nil);
                };
                candidate_table(lua, candidate, Some(index)).map(Value::Table)
            })
        }) {
            let _ = input_table.set("iter", iter);
        }

        let mut items_out: Vec<Candidate> = Vec::new();
        let store_for_out = store.clone();
        let mut on_value = |value: Value| {
            if let Value::Table(table) = value
                && let Some(candidate) = candidate_from_table(&table, &store_for_out)
            {
                items_out.push(candidate);
            }
        };
        let args = MultiValue::from_vec(vec![
            Value::Table(input_table),
            Value::Table(self.env.clone()),
        ]);
        if let Err(error) = drive_coroutine(lua, self.func.clone(), args, &mut on_value) {
            eprintln!("[lua] filter `{}` 失败: {error}", self.name_space);
            return items;
        }
        self.host.fire_commit_hooks(snapshot.commit_len, &self.env);
        items_out
    }
}

impl Filter for LuaFilter {
    fn apply(
        &mut self,
        ctx: &Context,
        translation: Box<dyn TranslationTrait>,
        _candidates: &mut Vec<Candidate>,
    ) -> Box<dyn TranslationTrait> {
        let items = collect_translation(translation);
        let out = self.run(items, ctx);
        Box::new(VecTranslation::new(out))
    }
}

// ---------------------------------------------------------------- processor

/// `lua_processor@…` → Rust `Processor`。
pub struct LuaProcessor {
    host: LuaHost,
    func: Function,
    init: Option<Function>,
    env: Table,
    inited: bool,
    name_space: String,
}

impl LuaProcessor {
    pub fn create(data_roots: &[PathBuf], name_space: &str) -> Option<Self> {
        let host = match LuaHost::boot(data_roots) {
            Ok(host) => host,
            Err(error) => {
                eprintln!("[lua] VM 启动失败: {error}");
                return None;
            }
        };
        let resolved = host.resolve_component("processor", name_space)?;
        let env = host.make_env(name_space).ok()?;
        Some(Self {
            host,
            func: resolved.func,
            init: resolved.init,
            env,
            inited: false,
            name_space: name_space.to_string(),
        })
    }
}

impl Processor for LuaProcessor {
    fn process_key_event(&mut self, engine: &mut Engine, key: &KeyEvent) -> ProcessResult {
        let config = engine.schema.config.clone();
        let snapshot = CtxSnapshot::from_context(&engine.context);
        self.host.refresh_env(&self.env, &snapshot, &config);
        let first_time = !self.inited;
        self.inited = true;
        let _ = self.env.set("first_time", first_time);
        if first_time
            && let Some(init) = self.init.take()
            && let Err(error) = init.call::<()>(self.env.clone())
        {
            eprintln!("[lua] processor `{}` init 失败: {error}", self.name_space);
        }

        let lua = &self.host.lua;
        let repr = key.repr();
        let Ok(key_table) = lua.create_table() else {
            return ProcessResult::Noop;
        };
        if let Ok(func) = lua.create_function(move |_, _this: Value| Ok(repr.clone())) {
            let _ = key_table.set("repr", func);
        }
        let _ = key_table.set("keycode", key.keycode as i64);
        let _ = key_table.set("modifier", key.modifier as i64);
        // 修饰键/松键判断：C++ `KeyEvent::release/shift/ctrl/alt/super`。
        // 虎整句 1.6 的 processor 与 backspace 观察器都靠 `key:release()` 区分按键与抬键。
        for (name, value) in [
            ("release", key.release()),
            ("shift", key.shift()),
            ("ctrl", key.ctrl()),
            ("alt", key.alt()),
            ("super", key.super_key()),
        ] {
            if let Ok(func) = lua.create_function(move |_, _this: Value| Ok(value)) {
                let _ = key_table.set(name, func);
            }
        }

        let result = self
            .func
            .call::<Value>((Value::Table(key_table), Value::Table(self.env.clone())));
        let code = match result {
            Ok(Value::Integer(code)) => code,
            Ok(Value::Number(code)) => code as i64,
            Ok(_) => 2,
            Err(error) => {
                eprintln!("[lua] processor `{}` 失败: {error}", self.name_space);
                2
            }
        };
        self.host.flush(Some(engine));
        match code {
            1 => ProcessResult::Accepted,
            0 => ProcessResult::Rejected,
            _ => ProcessResult::Noop,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lrn_lua_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("plugin")).unwrap();
        dir
    }

    /// 恒等式 searcher：无点 id → plugin/x/x.lua；点分 id → plugin/a/b/a.b.lua；
    /// 扁平 / `lua/` 布局不再解析。
    #[test]
    fn identity_searcher_resolves_series_layout_only() {
        let dir = temp_root("identity");
        std::fs::create_dir_all(dir.join("plugin").join("foo")).unwrap();
        std::fs::write(
            dir.join("plugin").join("foo").join("foo.lua"),
            "return { tag = 'flat-name' }",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("plugin").join("a").join("b")).unwrap();
        std::fs::write(
            dir.join("plugin").join("a").join("b").join("a.b.lua"),
            "return { tag = 'dotted' }",
        )
        .unwrap();
        // 旧布局故意留着：不应被解析
        std::fs::write(dir.join("plugin").join("flat.lua"), "return {}").unwrap();
        std::fs::create_dir_all(dir.join("lua")).unwrap();
        std::fs::write(dir.join("lua").join("legacy.lua"), "return {}").unwrap();

        let host = LuaHost::boot(&[dir.clone()]).expect("boot");
        let foo: String = host
            .lua
            .load("return require('foo').tag")
            .eval()
            .expect("require foo");
        assert_eq!(foo, "flat-name");
        let dotted: String = host
            .lua
            .load("return require('a.b').tag")
            .eval()
            .expect("require a.b");
        assert_eq!(dotted, "dotted");
        let flat: bool = host
            .lua
            .load("local ok = pcall(require, 'flat'); return not ok")
            .eval()
            .expect("扁平应失败");
        assert!(flat, "plugin/flat.lua 扁平形态不应被 require 到");
        let legacy: bool = host
            .lua
            .load("local ok = pcall(require, 'legacy'); return not ok")
            .eval()
            .expect("lua/ 目录应失效");
        assert!(legacy, "lua/legacy.lua 不应被 require 到");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn collects_lua_components_from_engine_prescriptions() {
        let config: serde_json::Value = serde_json::json!({
            "engine": {
                "processors": ["ascii_composer", "lua_processor@*enter_code_space"],
                "translators": ["script_translator", "lua_translator@*calc", "lua_translator@lunar"],
                "filters": ["uniquifier", "lua_filter@*code_len"]
            }
        });
        let got = collect_required_components(&config);
        assert_eq!(
            got,
            vec![
                ("processor".into(), "*enter_code_space".into()),
                ("translator".into(), "*calc".into()),
                ("translator".into(), "lunar".into()),
                ("filter".into(), "*code_len".into()),
            ],
            "组件名要保留 `*` 前缀（解析时要区分全局/require）"
        );
        let none: serde_json::Value = serde_json::json!({ "engine": { "processors": ["speller"] } });
        assert!(collect_required_components(&none).is_empty());
    }

    /// 脚本返回函数 + `@*名字`：`require` 解析（恒等式布局）+ 协程 yield。
    #[test]
    fn script_translator_yields_candidates() {
        let dir = temp_root("flat");
        std::fs::create_dir_all(dir.join("plugin").join("demo")).unwrap();
        std::fs::write(
            dir.join("plugin").join("demo").join("demo.lua"),
            r#"
local function translator(input, seg, env)
  yield(Candidate("demo", seg.start, seg._end, "你好", "你好吗"))
  yield(Candidate("demo", seg.start, seg._end, "妳好", ""))
end
return translator
"#,
        )
        .unwrap();

        let mut translator =
            LuaTranslator::create(&[dir.clone()], "*demo", serde_json::json!({}))
                .expect("构造 translator");
        let mut segment = Segment::new(0, 2);
        let stream = translator
            .query("ni", &mut segment, &Default::default())
            .expect("有候选流");
        let items = collect_translation(stream);
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0].text, "你好");
        assert_eq!(items[0].comment, "你好吗");
        assert_eq!(items[1].text, "妳好");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 模块表形式（`return M`，取 `M.translator`）+ `init` + 配置读取。
    #[test]
    fn module_table_with_init_reads_config() {
        let dir = temp_root("module");
        std::fs::create_dir_all(dir.join("plugin").join("mod")).unwrap();
        std::fs::write(
            dir.join("plugin").join("mod").join("mod.lua"),
            r#"
local M = {}
function M.init(env)
  env.prefix = env.engine.schema.config:get_string("demo/prefix") or "X"
end
function M.translator(input, seg, env)
  yield(Candidate("demo", seg.start, seg._end, env.prefix .. input, ""))
end
return M
"#,
        )
        .unwrap();

        let mut translator = LuaTranslator::create(
            &[dir.clone()],
            "mod",
            serde_json::json!({ "demo": { "prefix": "<<" } }),
        )
        .expect("构造 translator");
        let mut segment = Segment::new(0, 2);
        let stream = translator
            .query("ab", &mut segment, &Default::default())
            .expect("有候选流");
        let items = collect_translation(stream);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "<<ab", "init 从配置读到的前缀要生效");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// filter：`input:iter()` 迭代 + 修改字段 + `yield`。
    #[test]
    fn filter_iterates_and_rewrites_comment() {
        let dir = temp_root("filter");
        std::fs::create_dir_all(dir.join("plugin").join("annotate")).unwrap();
        std::fs::write(
            dir.join("plugin").join("annotate").join("annotate.lua"),
            r#"
return function(input, env)
  local n = 0
  for cand in input:iter() do
    n = n + 1
    cand.comment = (cand.comment or "") .. " <" .. n .. ">"
    yield(cand)
  end
end
"#,
        )
        .unwrap();

        let mut filter =
            LuaFilter::create(&[dir.clone()], "*annotate", serde_json::json!({})).expect("构造 filter");
        let ctx = Context::new();
        let cands = vec![
            Candidate::new("table", 0, 2, "你好", 1.0),
            Candidate::new("table", 0, 2, "尼好", 0.5),
        ];
        let out = filter.run(cands, &ctx);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "你好");
        assert_eq!(out[0].comment, " <1>");
        assert_eq!(out[1].comment, " <2>");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// processor：`key:repr()` + `ctx.input` + `engine:commit_text()` 的副作用。
    #[test]
    fn processor_commits_text_from_script() {
        let dir = temp_root("processor");
        std::fs::create_dir_all(dir.join("plugin").join("enter")).unwrap();
        std::fs::write(
            dir.join("plugin").join("enter").join("enter.lua"),
            r#"
local function processor(key, env)
  local ctx = env.engine.context
  if key:repr() == "Return" and ctx.input ~= "" then
    env.engine:commit_text(ctx.input .. " ")
    ctx:clear()
    return 1
  end
  return 2
end
return processor
"#,
        )
        .unwrap();

        let mut processor = LuaProcessor::create(&[dir.clone()], "*enter").expect("构造 processor");
        let mut engine = Engine::empty_for_test();
        engine.context.set_input("ni");
        let result = processor.process_key_event(&mut engine, &KeyEvent::new(0xff0d, 0));
        assert_eq!(result, ProcessResult::Accepted, "回车应被消费");
        assert_eq!(engine.context.input, "", "clear() 要清空输入");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 社区方案（巧码）的 lua 脚本目录；仓库里没有就跳过。
    fn community_root() -> Option<PathBuf> {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(".tmp_qiaoma")
            .join("shared");
        dir.is_dir().then_some(dir)
    }

    /// 实机用户目录的宇浩数据：恒等式布局 + 点分 require
    /// （plugin/yuhao/<名字>/yuhao.<名字>.lua）。机器上没有就跳过。
    #[test]
    fn live_yuhao_modules_require_under_identity_layout() {
        let Some(root) = std::env::var_os("APPDATA").map(|appdata| {
            PathBuf::from(appdata).join("Rime-Next")
        }) else {
            eprintln!("跳过:没有 APPDATA");
            return;
        };
        if !root.join("plugin/yuhao/yuhao_core/yuhao.yuhao_core.lua").is_file() {
            eprintln!("跳过:宇浩未迁移到恒等式布局");
            return;
        }
        let host = LuaHost::boot(&[root]).expect("boot");
        let ok: bool = host
            .lua
            .load(r#"local ok = pcall(require, "yuhao.yuhao_core"); return ok"#)
            .eval()
            .expect("eval");
        assert!(ok, "yuhao.yuhao_core 应能 require（恒等式布局）");
    }

    /// 真脚本 1：`number_translator`（大写金额）。输入 `R123` → 壹佰贰拾叁。
    #[test]
    fn community_number_translator_converts_amount() {
        let Some(root) = community_root() else {
            eprintln!("跳过:没有社区方案副本");
            return;
        };
        // 脚本硬读 `recognizer/patterns/number` 取触发前缀（C++ 里配置一定存在，
        // 缺了会 `attempt to index a nil value`，所以这里要照真实方案给一份）
        let config = serde_json::json!({
            "recognizer": { "patterns": { "number": "^R[0-9]+[.]?[0-9]*" } }
        });
        let host = LuaHost::boot(&[root]).expect("boot");
        let out = host.run_translator("number_translator", "R123", 0, 4, "R123", &config);
        assert!(!out.is_empty(), "应产出候选");
        let texts: Vec<&str> = out.iter().map(|(text, _)| text.as_str()).collect();
        assert!(
            texts.iter().any(|text| text.contains("壹佰贰拾叁")),
            "大写金额没出现: {texts:?}"
        );
    }

    /// 真脚本 2：`unicode`（U 前缀查码）。`U62fc` + `unicode` 标签 → 拼。
    #[test]
    fn community_unicode_translator_looks_up_code_point() {
        let Some(root) = community_root() else {
            eprintln!("跳过:没有社区方案副本");
            return;
        };
        let config = serde_json::json!({
            "recognizer": { "patterns": { "unicode": "^U[a-f0-9]+" } }
        });
        let mut translator =
            LuaTranslator::create(&[root], "*unicode", config).expect("构造 unicode translator");
        // 脚本要 `env.engine.context.composition:back()`（拿当前段改 tags），
        // 真实引擎里一定有点可取的段，测试里补一个
        let mut ctx = Context::new();
        ctx.input = "U62fc".to_string();
        ctx.composition.segmentation.segments.push(Segment::new(0, 5));
        translator.sync_context(&ctx);
        let mut segment = Segment::new(0, 5);
        segment.tags.push("unicode".to_string());
        let stream = translator
            .query("U62fc", &mut segment, &Default::default())
            .expect("有候选流");
        let items = collect_translation(stream);
        let texts: Vec<&str> = items.iter().map(|c| c.text.as_str()).collect();
        assert!(texts.contains(&"拼"), "U62fc 应查成「拼」: {texts:?}");
    }

    /// 真脚本 3：`hide_comment`（开关开时清空注释）。
    #[test]
    fn community_hide_comment_filter_clears_comment() {
        let Some(root) = community_root() else {
            eprintln!("跳过:没有社区方案副本");
            return;
        };
        let mut filter =
            LuaFilter::create(&[root], "*hide_comment", serde_json::json!({})).expect("构造 filter");
        let cands = vec![
            Candidate::new("table", 0, 2, "你好", 1.0).with_comment("ni hao"),
            Candidate::new("table", 0, 2, "尼好", 0.5).with_comment("ni hao"),
        ];
        // 开关关着 ⇒ 注释保留
        let ctx_off = Context::new();
        let out = filter.run(cands.clone(), &ctx_off);
        assert_eq!(out[0].comment, "ni hao");
        // 开关开着 ⇒ 注释被清空
        let mut ctx_on = Context::new();
        ctx_on.set_option("hide_comment", true);
        let out = filter.run(cands, &ctx_on);
        assert_eq!(out[0].comment, "", "hide_comment 开着应清空注释");
        assert_eq!(out[1].comment, "");
    }

    /// 真脚本 4：`custom_phrase_only_at_head`（table 型候选只留起始位置）。
    #[test]
    fn community_custom_phrase_filter_drops_mid_sentence_table_candidates() {
        let Some(root) = community_root() else {
            eprintln!("跳过:没有社区方案副本");
            return;
        };
        let mut filter = LuaFilter::create(&[root], "*custom_phrase_only_at_head", serde_json::json!({}))
            .expect("构造 filter");
        let cands = vec![
            Candidate::new("table", 0, 2, "你好", 1.0),
            // start != 0 的 table 候选应被丢掉
            Candidate::new("table", 2, 4, "世界", 1.0),
            // 非 table 型（如 script）不受限
            Candidate::new("script", 2, 4, "是", 1.0),
        ];
        let out = filter.run(cands, &Context::new());
        let texts: Vec<&str> = out.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["你好", "是"], "只丢中间位置的 table 候选: {texts:?}");
    }

    /// 真脚本 5：`code_length_annotator`（首候选加码长/速度注释）。
    ///
    /// 门槛：`word >= 2 && #code >= 5 && 耗时 > 1s` —— 单测里造不出真耗时，
    /// 只验证它能跑通且不破坏候选（`env.commit_notifier` 的连接也是这条路径）。
    #[test]
    fn community_code_length_annotator_runs_without_breaking_candidates() {
        let Some(root) = community_root() else {
            eprintln!("跳过:没有社区方案副本");
            return;
        };
        let mut filter = LuaFilter::create(&[root], "*code_length_annotator", serde_json::json!({}))
            .expect("构造 filter");
        let cands = vec![
            Candidate::new("table", 0, 5, "你好世界", 1.0),
            Candidate::new("table", 0, 5, "尼好世界", 0.5),
        ];
        let mut ctx = Context::new();
        ctx.input = "cojqvb".to_string();
        ctx.set_option("show_code_ratio", true);
        let out = filter.run(cands, &ctx);
        assert_eq!(out.len(), 2, "候选不应丢");
        assert_eq!(out[0].text, "你好世界");
    }

    /// `Rime-Next/plugin/rime.lua` 这套引导：注册全局 → `@*名字` 能解析。
    #[test]
    fn bootstrap_from_shipped_plugin_registers_tiger() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("Rime-Next");
        if !root.join("plugin").join("rime.lua").is_file() {
            eprintln!("跳过:没有 Rime-Next/plugin/rime.lua");
            return;
        }
        let host = LuaHost::boot(&[root.clone()]).expect("boot");
        assert!(!host.bootstrapped().is_empty(), "引导脚本应被执行");
        // 虎整句模块在 rime.lua 里被 require 并注册成全局
        assert!(
            host.globals_lookup("tiger_sentence_translator").is_some(),
            "tiger_sentence_translator 全局没注册"
        );
        let _ = root;
    }

    /// `Set` 差集（`shijian` 用 `segment.tags - Set({...})`）。
    #[test]
    fn set_helper_supports_difference() {
        let dir = temp_root("set");
        let host = LuaHost::boot(&[dir.clone()]).expect("boot");
        let value: bool = host
            .lua
            .load(r#"local t = Set({"a","b"}) - Set({"b"}); return t["a"] == true and t["b"] == nil"#)
            .eval()
            .expect("执行");
        assert!(value, "Set 差集语义");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 引导脚本能注册全局，`@*全局名` 能解析。
    #[test]
    fn bootstrap_registers_globals_for_star_components() {
        let dir = temp_root("bootstrap");
        std::fs::create_dir_all(dir.join("plugin").join("helper")).unwrap();
        std::fs::write(
            dir.join("plugin").join("helper").join("helper.lua"),
            "return { translator = function(input, seg, env) yield(Candidate('h', seg.start, seg._end, 'H'..input, '')) end }",
        )
        .unwrap();
        std::fs::write(
            dir.join("plugin").join("rime.lua"),
            "helper = require('helper')\nhelper_translator = helper.translator\n",
        )
        .unwrap();

        let mut translator = LuaTranslator::create(&[dir.clone()], "*helper_translator", serde_json::json!({}))
            .expect("全局注册后应能解析");
        let mut segment = Segment::new(0, 2);
        let stream = translator
            .query("x", &mut segment, &Default::default())
            .expect("有候选流");
        let items = collect_translation(stream);
        assert_eq!(items.first().map(|c| c.text.as_str()), Some("Hx"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 照搬上游 `src/lib/lua.cc::SetReg`：交集 `__mul`、`empty()`，
    /// 以及 `pmain()` 的 `table.unpack = table.unpack or unpack`（LuaJIT 5.1 别名）。
    #[test]
    fn set_helper_supports_upstream_intersection_and_unpack_alias() {
        let dir = temp_root("set2");
        let host = LuaHost::boot(&[dir.clone()]).expect("boot");
        let ok: bool = host
            .lua
            .load(
                r#"
local a, b = Set({"a","b"}), Set({"b","c"})
local inter, diff, union = a * b, a - b, a + b
return inter["b"] == true and inter["a"] == nil
  and diff["a"] == true and union["c"] == true
  and Set({}):empty() and not a:empty()
  and table.unpack({7,8}) == 7
"#,
            )
            .eval()
            .expect("执行");
        assert!(ok, "Set 交集 / empty / table.unpack 别名");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 模拟 LuaJIT(5.1) 的 VM（没有 utf8 库）：兜底层语义要和 lua54 一致。
    /// 本机 `plugin/yuhao/*`、`plugin/tiger/*`、`input_statistics.lua` 全靠这几个函数。
    #[test]
    fn utf8_compat_covers_luajit_51_abi() {
        let lua = Lua::new();
        lua.globals().set("utf8", Value::Nil).expect("清空 utf8");
        lua.load(LUA_UTF8_COMPAT).exec().expect("注入 utf8 兼容层");

        let (rebuilt, len, count): (String, i64, i64) = lua
            .load(
                r#"
local s = "A中B"
local rebuilt, n = {}, 0
for p, cp in utf8.codes(s) do
  n = n + 1
  rebuilt[#rebuilt + 1] = utf8.char(cp)
end
return table.concat(rebuilt), utf8.len(s), n
"#,
            )
            .eval()
            .expect("utf8.codes / char / len");
        assert_eq!(rebuilt, "A中B");
        assert_eq!(len, 3);
        assert_eq!(count, 3);

        let (a, b, c, off): (i64, i64, i64, i64) = lua
            .load(
                r#"
local s = "A中B"
local a, b, c = utf8.codepoint(s, 1, 5)
return a, b, c, utf8.offset(s, 2, 1)
"#,
            )
            .eval()
            .expect("utf8.codepoint / offset");
        assert_eq!([a, b, c], [65, 0x4E2D, 66]);
        assert_eq!(off, 2, "第二个字符『中』从字节 2 开始");
    }

    /// 上游 `sub_module_init` 的 `Component*Sub` 写法：`*` 分层也能解析。
    #[test]
    fn star_submodule_path_resolves_like_upstream() {
        let dir = temp_root("submod");
        std::fs::write(
            dir.join("plugin").join("rime.lua"),
            "yuhao = { core = { filter = function(input, env) return input end } }\n",
        )
        .unwrap();
        let host = LuaHost::boot(&[dir.clone()]).expect("boot");
        assert!(
            host.resolve_component("filter", "*yuhao*core").is_some(),
            "`*` 分层应能解析到 _G.yuhao.core"
        );
        assert!(host.resolve_component("filter", "*yuhao.core").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 虎整句 1.6 依赖的 Context 属性与通知器：`set_property`/`get_property` 同次调用可见，
    /// `connect` 返回的连接对象能 `disconnect`，命令会排进 flush 队列。
    #[test]
    fn context_properties_and_notifier_connections() {
        let dir = temp_root("ctxapi");
        let host = LuaHost::boot(&[dir.clone()]).expect("boot");
        let env = host.make_env("ctxapi").expect("env");
        let snapshot = CtxSnapshot {
            input: String::new(),
            caret_pos: 0,
            options: HashMap::new(),
            properties: HashMap::new(),
            is_composing: false,
            has_menu: false,
            commit_text: String::new(),
            segment: None,
            commit_len: 0,
        };
        host.refresh_env(&env, &snapshot, &serde_json::json!({}));
        host.lua.globals().set("test_env", env).unwrap();
        let ok: bool = host
            .lua
            .load(
                r#"
                local c = test_env.engine.context
                c:set_property("tiger_sentence.buffered", "zhhbi")
                if c:get_property("tiger_sentence.buffered") ~= "zhhbi" then return false end
                if c:get_property("missing") ~= "" then return false end
                if c:is_composing() ~= false or c:has_menu() ~= false then return false end
                if c:get_commit_text() ~= "" then return false end
                local conn = c.commit_notifier:connect(function() end)
                if type(conn.disconnect) ~= "function" then return false end
                conn:disconnect()
                local update_conn = c.update_notifier:connect(function() end)
                update_conn:disconnect()
                c:push_input("a")
                return true
                "#,
            )
            .eval()
            .expect("Context API 全部可用");
        assert!(ok);
        let commands = host.link.commands.lock().unwrap();
        assert!(commands
            .iter()
            .any(|c| matches!(c, Command::SetProperty(n, v) if n == "tiger_sentence.buffered" && v == "zhhbi")));
        assert!(commands
            .iter()
            .any(|c| matches!(c, Command::PushInput(v) if v == "a")));
        assert!(
            host.link.hooks.lock().unwrap().is_empty(),
            "disconnect 后不应残留回调"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
