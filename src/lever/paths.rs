//! 资源路径解析 —— `src/rime/resource.cc`（100 行）里 **lever 层用得到的那一部分**。
//!
//! ## 为什么单独成文件
//!
//! C++ 的 `ResourceResolver` 被 `config` / `dict` / `lever` 共用（`rime/resource.h` 不属于
//! 任何一个子目录）。Rust 侧 `config/resource.rs` 已经移植了它，但
//! `new` 之外的 `resolve_path` / `to_file_path` / `add_override` 都是 `pub(super)`，
//! `lever` 调不到；`config.rs` 又是本任务的禁区。于是这里按**同一份规则**重写一份
//! lever 专用的解析器，并用单测把 `resource.cc` 的分目录规则钉住。
//!
//! 等 `resource` 那层单独移植、把 API 公开，本文件应当整体删掉换成 `use`。
//!
//! ## 规则（Weasel-Next 补丁版 `ResourceResolver::ResolvePath`，2026-10-25 系列文件夹）
//!
//! ```text
//! 显式路径：id 含 '/' 或 '\' → <root>/<id><后缀>        （不套分类，import_tables 的相对路径写法）
//! 扁平路径 = <root>/<prefix><id><suffix>          （prefix 恒空；仅未归类资源走这条）
//! 分目录：① 后缀 .bin       → <root>/<系列…>/<id><后缀>     （staging 根，编译产物）
//!         ② 后缀 .gram      → <root>/model/<id><后缀>       （语言模型按 id 平铺，不建系列夹）
//!         ③ id 以 .schema/.dict 结尾且后缀 .yaml
//!              staging 根 → <root>/<系列…>/<id>.yaml
//!              其它根      → <root>/schema/<系列…>/<id>.yaml
//!         ④ 后缀 .schema.yaml / .dict.yaml → 同 ③
//!         ⑤ 后缀 .txt        → <root>/eassy/<id><后缀>      （essay 按 id 平铺，不建系列夹）
//!         ⑥ 后缀 .theme.toml / .theme.yaml → <root>/theme/<系列…>/<id><后缀>（主题按系列分层）
//!         ⑦ 后缀 .lua        → <root>/plugin/<id>/<id>.lua  （字面 id 保留点号；
//!             rime-lua 实际走 package.path 点号即目录，C++ 此分支暂无调用方、保持原样）
//! ```
//!
//! **系列分层**：id 按 '.' 分段，每个点号对应一级文件夹，文件名始终是完整 id：
//! `yuling` → `schema/yuling/yuling.schema.yaml`；
//! `yuling.five` → `schema/yuling/five/yuling.five.dict.yaml`；
//! `a.b.c` → `schema/a/b/c/a.b.c.schema.yaml`。
//! 恒等式：文件夹链 == id 按点号分段，少任何一级该资源任何 ID 都解析不到。
//!
//! ③④ **永不回退扁平**：nested 不存在也返回 nested 路径，由调用方 `exists()` 判失败。
//! （2026-10-24 之前是"nested 存在 → nested；否则 flat 存在 → flat；否则 nested"，
//! 回退留着就会继续长出一坨扁平文件，布局等于没分，故删除。）
//!
//! `FallbackResourceResolver`（`resource.cc:88`）：主根找不到时，用回退根按
//! **同一套分目录规则**再解一次（也不是扁平路径）。
//!
//! ## 三个根由宿主经 get/set 注入，lrn 持有为状态，只写死通用子目录名
//!
//! **三个根路径（程序文件夹 `PROGRAM_DIR` / 共享数据目录 `SHARE_DIR` / 用户文件夹 `USER_DIR`）
//! 由 Weasel 通过 API（`RimeTraits` 的 `shared_data_dir` / `user_data_dir`）经 get/set 注入**，
//! librime-next 把它们作为**状态持有**（`Layout` / `Deployer` 的 `program_dir` /
//! `shared_data_dir` / `user_data_dir` 字段），**不写死字符串值、也不决定根叫什么**。
//! 其中 `SHARE_DIR` = 宿主传入的 `shared_data_dir`（即 Weasel 设的 `Rime-Next` 数据目录），
//! `PROGRAM_DIR`（安装目录，SHARE 的父目录）由 lrn 从 `shared_data_dir` 推出、可被 set 覆盖。
//! lrn 只写死"各个版本输入法通用、一定不变"的**子目录名**
//! （`SCHEMA_DIR` / `BUILD_DIR` / `MODEL_DIR` / `EASSY_DIR` / `THEME_DIR` / `PLUGIN_DIR` …），
//! 全部相对这三个根拼出。
//!
//! | 用途 | 根（用户数据目录优先） | 回退根（共享数据目录） | 后缀 |
//! |---|---|---|---|
//! | 源配置 `config_source_file` | 用户数据目录 | 共享数据目录 | `.yaml` |
//! | 源方案 `schema_source_file` | 用户数据目录 | 共享数据目录 | `.schema.yaml` |
//! | 已部署产物（`compiled_config` / `compiled_schema`） | `用户文件夹/build` | （出厂预编译已取消，通常为空） | `.yaml` / `.schema.yaml` |
//! | 用户配置 `user_config` | 用户文件夹（**无回退**） | — | `.yaml` |

use std::path::{Path, PathBuf};

// =====================================================================
// 通用子目录名（写死在 librime-next 内部）
// =====================================================================
// 这些名字是"各个版本输入法通用、一定不变"的目录段，所有路径相对宿主注入的
// 根（共享数据目录 / 用户数据目录）拼出来。根路径本身由 Weasel 通过 `RimeTraits`
// API 注入，lrn 不设置、也不决定根路径（2026-10-25 架构澄清）。
pub const SCHEMA_DIR: &str = "schema";
pub const BUILD_DIR: &str = "build";
pub const MODEL_DIR: &str = "model";
pub const EASSY_DIR: &str = "eassy";
pub const THEME_DIR: &str = "theme";
pub const PLUGIN_DIR: &str = "plugin";
pub const USERDB_DIR: &str = "userdb";
pub const TRASH_DIR: &str = "trash";
pub const SYNC_DIR: &str = "sync";
pub const BACKUP_DIR: &str = "backup";
pub const OPENCC_DIR: &str = "opencc";

/// `schema/` 下的**方案清单文件名**（点名制：`schemas:` 列出要部署的方案 id）。
///
/// 曾经是 `schema.toml` —— 2026-10-26 起**全仓库不认 TOML**（用户拍板：toml 不好用），
/// 清单改用 YAML，文件名就是它。对应关系见 C++ `toml_lite.cc` 的 `LoadSchemaIdsFromToml`，
/// 那条 TOML 分支在 lrn 里没有移植。
pub const SCHEMA_MANIFEST: &str = "schema.yaml";

/// Weasel-Next 的部署布局（`setup.cc` 里 `RimeTraits` 的四个目录 + `Deployer` 的几个字段）。
///
/// **三个根路径（程序文件夹 / 共享数据目录 / 用户文件夹）由宿主经 API 注入**，
/// lrn 把它们作为**状态持有**（`program_dir` / `shared_data_dir` / `user_data_dir`），
/// 由宿主的 get/set（RimeTraits）设进实际路径；lrn 不写死字符串值、也不决定根叫什么。
/// lrn 只写死通用子目录名（`SCHEMA_DIR` 等）。
#[derive(Debug, Clone)]
pub struct Layout {
    /// 程序文件夹（`PROGRAM_DIR`）：宿主经 API 注入的安装目录，是共享数据目录的**父目录**。
    /// 只含共享数据目录（`shared_data_dir`），禁止任何设置文件、禁止任何 `build`。
    pub program_dir: PathBuf,
    /// 共享数据目录（`SHARE_DIR`，即 Weasel 设的 `Rime-Next` 数据目录）。
    /// 所有基础方案与数据（`schema`/`model`/`eassy`/`theme`/`plugin`）都在这里。
    pub shared_data_dir: PathBuf,
    /// 用户文件夹（`USER_DIR`，如 `%APPDATA%\Rime-Next`）。
    pub user_data_dir: PathBuf,
    /// 部署产物目录（`<用户目录>/build`）。**build 只存在于用户文件夹**（2026-10-25 硬规则）。
    pub staging_dir: PathBuf,
    /// 预编译产物目录。2026-10-25 起**出厂预编译取消**（程序文件夹禁止 build），
    /// 故通常为空；运行时已部署产物只从 `staging_dir`（用户目录 build）读取。
    pub prebuilt_data_dir: PathBuf,
}

impl Layout {
    /// 从 `Deployer` 的目录字段搭出来。三个根由宿主经 API 注入，直接收下。
    ///
    /// * `program_dir`：程序文件夹（`PROGRAM_DIR`，宿主经 API 注入的安装目录）。
    /// * `shared_data_dir`：共享数据目录（`SHARE_DIR`，Weasel 通过 API 注入的 `Rime-Next` 数据目录）。
    /// * `user_data_dir`：用户文件夹（`USER_DIR`，Weasel 通过 API 注入）。
    pub fn new(
        program_dir: impl Into<PathBuf>,
        shared_data_dir: impl Into<PathBuf>,
        user_data_dir: impl Into<PathBuf>,
        staging_dir: impl Into<PathBuf>,
        prebuilt_data_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            program_dir: program_dir.into(),
            shared_data_dir: shared_data_dir.into(),
            user_data_dir: user_data_dir.into(),
            staging_dir: staging_dir.into(),
            prebuilt_data_dir: prebuilt_data_dir.into(),
        }
    }

    /// **程序文件夹**（`PROGRAM_DIR`，宿主经 API 注入）：安装目录，是共享数据目录的父目录。
    /// 禁止出现任何设置文件、禁止出现 `build`。
    pub fn program_dir(&self) -> &Path {
        &self.program_dir
    }

    /// **用户文件夹**（`USER_DIR`，宿主通过 API 注入，如 `%APPDATA%\Rime-Next`）：
    /// 所有设置与部署产物（`build` 等）都写到这里。**部署强制写死到用户文件夹**。
    pub fn user_folder(&self) -> &Path {
        &self.user_data_dir
    }

    /// **共享数据目录**（`SHARE_DIR`，宿主通过 API 注入的 `Rime-Next` 数据目录）：
    /// 所有基础方案与数据从这里读。
    pub fn shared_folder(&self) -> &Path {
        &self.shared_data_dir
    }

    /// `config_source_file`：`<id>.yaml`，用户目录优先、共享数据目录回退。
    pub fn config_source(&self, resource_id: &str) -> PathBuf {
        fallback_resolve(
            self.user_folder(),
            self.shared_folder(),
            resource_id,
            "",
            ".yaml",
        )
    }

    /// `schema_source_file`：`<id>.schema.yaml`，用户文件夹优先、共享数据目录回退。
    pub fn schema_source(&self, schema_id: &str) -> PathBuf {
        fallback_resolve(
            self.user_folder(),
            self.shared_folder(),
            schema_id,
            "",
            ".schema.yaml",
        )
    }

    /// 用户配置（`user_config` 组件）：`<用户文件夹>/<id>.yaml`，**没有**回退根。
    pub fn user_config(&self, resource_id: &str) -> PathBuf {
        resolve(self.user_folder(), resource_id, "", ".yaml")
    }

    /// 已部署产物（`DeployedConfigResourceProvider` / `compiled_schema`）：
    /// 实际部署时只查 `用户文件夹/build`（出厂预编译已取消，prebuilt 恒空）。
    pub fn deployed(&self, resource_id: &str, suffix: &str) -> PathBuf {
        fallback_resolve(&self.staging_dir, &self.prebuilt_data_dir, resource_id, "", suffix)
    }

    /// 已编译配置（`compiled_config`）的**写**目标：只走 `staging_dir`，不回退。
    ///
    /// `SaveOutputPlugin` 拿的是 `CreateStagingResourceResolver`（`save_output_plugin.cc`），
    /// 所以写目标永远是 `<staging>/<id>.yaml` 或 `<staging>/<leaf>/<id>.yaml`。
    /// staging 根由 [`super::deployer::Deployer::layout`] 强制为 `用户文件夹/build`
    /// （2026-10-25 硬规则：build 只存在于用户文件夹）。
    pub fn staged_output(&self, resource_id: &str) -> PathBuf {
        resolve(&self.staging_dir, resource_id, "", ".yaml")
    }

    /// 已编译方案配置：`<staging>/<id>/<id>.schema.yaml`（回退到 prebuilt）。
    pub fn compiled_schema(&self, schema_id: &str) -> PathBuf {
        self.deployed(schema_id, ".schema.yaml")
    }

    /// Weasel-Next 的用户词典目录：`<用户文件夹>/userdb`（官方是用户目录根部，
    /// 见 `plugin/librime/src/input_stat_translator.cc` 的注释）。
    pub fn user_db_dir(&self) -> PathBuf {
        self.user_folder().join(USERDB_DIR)
    }

    /// 回收站：`<用户文件夹>/trash`。
    pub fn trash_dir(&self) -> PathBuf {
        self.user_folder().join(TRASH_DIR)
    }

    /// 方案源文件候选（用于"方案在不在"的判断，按搜索顺序）：
    /// 每根只有分类路径 `schema/<系列…>/<id>.schema.yaml` 与 `schema/<系列…>/<id>.yaml`。
    /// **没有扁平候选**（2026-10-25 删：扁平的 `<id>.schema.yaml` 不再被识别）。
    /// 系列分层（2026-10-25）：id 按 '.' 分段，每个点号一级文件夹。
    pub fn schema_source_candidates(&self, schema_id: &str) -> Vec<PathBuf> {
        let mut candidates = Vec::new();
        let series = series_subdir(schema_id);
        for root in [self.user_folder(), self.shared_folder()] {
            candidates.push(
                root.join(SCHEMA_DIR)
                    .join(&series)
                    .join(format!("{schema_id}.schema.yaml")),
            );
            candidates.push(
                root.join(SCHEMA_DIR)
                    .join(&series)
                    .join(format!("{schema_id}.yaml")),
            );
        }
        candidates
    }
}

/// `ResourceResolver::ToFilePath`：缺 prefix / 缺 suffix 就补上。
pub fn to_file_path(resource_id: &str, prefix: &str, suffix: &str) -> String {
    let has_parent = Path::new(resource_id)
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty());
    let missing_prefix = !has_parent && !resource_id.starts_with(prefix);
    let missing_suffix = !resource_id.ends_with(suffix);
    format!(
        "{}{}{}",
        if missing_prefix { prefix } else { "" },
        resource_id,
        if missing_suffix { suffix } else { "" }
    )
}

/// staging 根判定（`resource.cc:42-49`）：目录名是 `build` 或以 `/build` 结尾。
pub fn is_staging_root(root: &Path) -> bool {
    let generic = root.to_string_lossy().replace('\\', "/");
    let trimmed = generic.trim_end_matches('/');
    root.file_name().is_some_and(|name| name == "build") || trimmed.ends_with("/build")
}

/// 系列文件夹链（`resource.cc` 的 `series_subdir`，2026-10-25）：id 里的每个 '.'
/// 对应一级文件夹。
///   "yuling" → "yuling"；"yuling.five" → "yuling/five"；"a.b.c" → "a/b/c"
/// 文件名始终是完整 id —— 文件夹链与文件名主干按点号一一对应（恒等式，扫描侧同）。
pub fn series_subdir(resource_id: &str) -> String {
    resource_id.replace('.', "/")
}

/// `resource_id` 里 `.schema` / `.dict` 资源的目录名（`<leaf>`，即去掉类型后缀的 stem）。
fn schema_leaf(resource_id: &str) -> Option<String> {
    if resource_id.ends_with(".schema") || resource_id.ends_with(".dict") {
        let leaf = resource_id
            .rsplit_once('.')
            .map(|(head, _)| head)
            .unwrap_or(resource_id);
        Some(leaf.to_string())
    } else {
        None
    }
}

/// `ResourceResolver::ResolvePath`（单根版）。
///
/// 2026-10-25：`.schema` / `.dict` 资源**永不回退扁平** —— 分类路径不存在也返回
/// 分类路径，由调用方 `exists()` 判失败。未归类资源（`default.yaml` 等）保持根目录。
/// 2026-10-25 系列文件夹：id 含点号时按 '.' 分段建多级文件夹；id 含 '/' 时视为
/// 显式相对路径，不套分类规则（import_tables 的 `schema/yuhao/yuling.quick` 写法）。
pub fn resolve(root: &Path, resource_id: &str, prefix: &str, suffix: &str) -> PathBuf {
    let base = to_file_path(resource_id, prefix, suffix);

    // 显式路径风格：id 带目录分隔符 → <root>/<file>，不套分类（C++ resource.cc 同规则）。
    if resource_id.contains('/') || resource_id.contains('\\') {
        return root.join(&base);
    }

    // ① / ②：.bin 与 .gram 必须待在固定子目录里，C++ 明确写了"没有回退"。
    if suffix.ends_with(".bin") {
        return root.join(series_subdir(resource_id)).join(&base);
    }
    if suffix.ends_with(".gram") {
        return root.join(MODEL_DIR).join(&base);
    }
    // ⑤ .txt → eassy/<id>.txt（essay 按 id 平铺，不建系列夹；C++ resource.cc:110-111）
    if suffix.ends_with(".txt") {
        return root.join(EASSY_DIR).join(&base);
    }
    // ⑥ .theme.toml / .theme.yaml → theme/<系列…>/<id><后缀>（主题按系列分层；C++ resource.cc:112-113）
    //    2026-10-27：主题文件改用 YAML（全仓库退出 TOML），后缀要一并认 —— 否则
    //    `*.theme.yaml` 会落到"未归类资源"分支，被摆到根目录而不是 `theme/<系列>/`。
    if suffix.ends_with(".theme.toml") || suffix.ends_with(".theme.yaml") {
        return root
            .join(THEME_DIR)
            .join(series_subdir(resource_id))
            .join(&base);
    }
    // ⑦ .lua → plugin/<id>/<id>.lua（C++ resource.cc:114-115：字面 id 保留点号，
    //    不转成系列目录；rime-lua 的 package.path 另有点号即目录约定，此分支暂无
    //    调用方，与 C++ 保持一致）
    if suffix.ends_with(".lua") {
        return root.join(PLUGIN_DIR).join(resource_id).join(&base);
    }

    // ③ / ④：配置类资源分目录，**无扁平回退**。
    let nested_subdir: Option<String> = if suffix == ".yaml" || suffix.ends_with(".schema.yaml")
        || suffix.ends_with(".dict.yaml")
    {
        if let Some(leaf) = schema_leaf(resource_id) {
            Some(if is_staging_root(root) {
                series_subdir(&leaf)
            } else {
                format!("{SCHEMA_DIR}/{}", series_subdir(&leaf))
            })
        } else if suffix.ends_with(".schema.yaml") || suffix.ends_with(".dict.yaml") {
            // 后缀带 .schema.yaml 时资源 id 本身不带 `.schema`，系列链按整个 id 分段
            Some(if is_staging_root(root) {
                series_subdir(resource_id)
            } else {
                format!("{SCHEMA_DIR}/{}", series_subdir(resource_id))
            })
        } else {
            None
        }
    } else {
        None
    };

    if let Some(subdir) = nested_subdir {
        return root.join(subdir).join(&base);
    }
    root.join(&base)
}

/// `FallbackResourceResolver::ResolvePath`：主根找不到时，用回退根按**同一套分目录
/// 规则**再解一次（2026-10-25 起回退根也不拼扁平路径）。
pub fn fallback_resolve(
    root: &Path,
    fallback_root: &Path,
    resource_id: &str,
    prefix: &str,
    suffix: &str,
) -> PathBuf {
    let default_path = resolve(root, resource_id, prefix, suffix);
    if !default_path.exists() {
        // ⚠️ 回退根也必须走 `resolve()`（分目录规则同样适用），不能只拼扁平路径。
        // C++ 的 `FallbackResourceResolver::ResolvePath` 是 `fallback_->ResolvePath(...)`，
        // 即"用同一个解析器再解一次"，方案源在 `<共享>/schema/<id>/<id>.schema.yaml` 这种
        // 子目录里才找得到（2026-10-24 修：原来只查扁平 `<共享>/<id>.schema.yaml`）。
        let fallback = resolve(fallback_root, resource_id, prefix, suffix);
        if fallback.exists() {
            return fallback;
        }
    }
    default_path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("librime-lever-paths-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
    }

    #[test]
    fn to_file_path_adds_suffix() {
        assert_eq!(to_file_path("default", "", ".yaml"), "default.yaml");
        assert_eq!(to_file_path("default.yaml", "", ".yaml"), "default.yaml");
        assert_eq!(
            to_file_path("luna_pinyin", "", ".schema.yaml"),
            "luna_pinyin.schema.yaml"
        );
        assert_eq!(to_file_path("sub/x", "", ".yaml"), "sub/x.yaml");
    }

    #[test]
    fn staging_root_detection() {
        assert!(is_staging_root(Path::new("C:/Rime-Next/build")));
        assert!(is_staging_root(Path::new("C:/Rime-Next/build/")));
        assert!(is_staging_root(Path::new("C:\\Rime-Next\\build")));
        assert!(!is_staging_root(Path::new("C:/Rime-Next/schema")));
    }

    #[test]
    fn series_subdir_splits_on_dots() {
        assert_eq!(series_subdir("yuling"), "yuling");
        assert_eq!(series_subdir("yuling.five"), "yuling/five");
        assert_eq!(series_subdir("a.b.c"), "a/b/c");
    }

    #[test]
    fn resolve_dotted_ids_into_series_folders() {
        let root = temp("series");
        // 源目录：裸 id 无点号 → schema/<id>/；带点号 → schema/<系列>/<成员>/
        assert_eq!(
            resolve(&root, "yuling", "", ".schema.yaml"),
            root.join("schema/yuling/yuling.schema.yaml")
        );
        assert_eq!(
            resolve(&root, "yuling.five", "", ".dict.yaml"),
            root.join("schema/yuling/five/yuling.five.dict.yaml")
        );
        // 类型后缀是 .yaml、id 自带 .schema/.dict 时同理（stem 去后缀再分层）
        assert_eq!(
            resolve(&root, "yuling.five.dict", "", ".yaml"),
            root.join("schema/yuling/five/yuling.five.dict.yaml")
        );
        // staging 根：没有 schema/ 前缀，成员层级保留
        let build = root.join("build");
        assert_eq!(
            resolve(&build, "yuling.five", "", ".schema.yaml"),
            build.join("yuling/five/yuling.five.schema.yaml")
        );
        // .bin：编译产物同样按系列分层
        assert_eq!(
            resolve(&build, "yuling.five", "", ".table.bin"),
            build.join("yuling/five/yuling.five.table.bin")
        );
        assert_eq!(
            resolve(&build, "yuling", "", ".prism.bin"),
            build.join("yuling/yuling.prism.bin")
        );
        // .gram：语言模型按 id 平铺在 model/，不建系列文件夹
        assert_eq!(
            resolve(&root, "zh-hant-t-essay-bgc", "", ".gram"),
            root.join("model/zh-hant-t-essay-bgc.gram")
        );
        // 显式路径风格：含 '/' 不套分类（import_tables 的相对路径写法）
        assert_eq!(
            resolve(&root, "schema/yuhao/yuling.quick", "", ".dict.yaml"),
            root.join("schema/yuhao/yuling.quick.dict.yaml")
        );
        // 未归类配置保持扁平
        assert_eq!(resolve(&root, "default", "", ".yaml"), root.join("default.yaml"));
        assert_eq!(
            resolve(&root, "default.custom", "", ".yaml"),
            root.join("default.custom.yaml")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_txt_theme_lua_into_classified_dirs() {
        let root = temp("classified");
        // .txt → eassy/ 平铺（不建系列夹，C++ resource.cc:110-111）
        assert_eq!(
            resolve(&root, "essay", "", ".txt"),
            root.join("eassy/essay.txt")
        );
        assert_eq!(
            resolve(&root, "zh-hant-t-essay-bgc", "", ".txt"),
            root.join("eassy/zh-hant-t-essay-bgc.txt")
        );
        // .theme.toml → theme/<系列…>/（带点号分层，C++ resource.cc:112-113）
        assert_eq!(
            resolve(&root, "weasel", "", ".theme.toml"),
            root.join("theme/weasel/weasel.theme.toml")
        );
        assert_eq!(
            resolve(&root, "yuling.five", "", ".theme.toml"),
            root.join("theme/yuling/five/yuling.five.theme.toml")
        );
        // 2026-10-27：主题改用 YAML，`.theme.yaml` 必须走同一条规则 ——
        // 否则会被当成"未归类资源"摆到根目录，宿主渲染器就找不着皮肤了。
        assert_eq!(
            resolve(&root, "weasel", "", ".theme.yaml"),
            root.join("theme/weasel/weasel.theme.yaml")
        );
        assert_eq!(
            resolve(&root, "yuling.five", "", ".theme.yaml"),
            root.join("theme/yuling/five/yuling.five.theme.yaml")
        );
        // .lua → plugin/<id>/<id>.lua（字面 id 保留点号，C++ resource.cc:114-115）
        assert_eq!(
            resolve(&root, "calculator", "", ".lua"),
            root.join("plugin/calculator/calculator.lua")
        );
        // id 自带 .lua 后缀时 to_file_path 不重复拼（与 C++ ToFilePath 同规则）
        assert_eq!(
            resolve(&root, "rime.lua", "", ".lua"),
            root.join("plugin/rime.lua/rime.lua")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn schema_source_never_falls_back_to_flat() {
        let root = temp("schema-source");
        let user = root.join("user");
        let shared = root.join("shared");
        let layout = Layout::new(&root, &shared, &user, user.join("build"), shared.join("build"));
        // 都没有 → nested（用户目录）是唯一候选
        assert_eq!(
            layout.schema_source("luna"),
            user.join("schema/luna/luna.schema.yaml")
        );
        // 扁平存在也**不退回**：只认分类路径（2026-10-25 删回退）
        touch(&user.join("luna.schema.yaml"));
        assert_eq!(
            layout.schema_source("luna"),
            user.join("schema/luna/luna.schema.yaml")
        );
        // nested 存在时优先 nested
        touch(&user.join("schema/luna/luna.schema.yaml"));
        assert_eq!(
            layout.schema_source("luna"),
            user.join("schema/luna/luna.schema.yaml")
        );
        // 用户目录没有 → 共享目录（nested）
        touch(&shared.join("schema/stroke/stroke.schema.yaml"));
        assert_eq!(
            layout.schema_source("stroke"),
            shared.join("schema/stroke/stroke.schema.yaml")
        );
        // 共享目录只有扁平 → 仍然找不到（返回用户目录的 nested 路径，由调用方判不存在）
        touch(&shared.join("wubi86.schema.yaml"));
        assert_eq!(
            layout.schema_source("wubi86"),
            user.join("schema/wubi86/wubi86.schema.yaml")
        );
        // 系列 id：候选是 schema/<系列>/<成员>/<id>.schema.yaml，无扁平候选
        assert_eq!(
            layout.schema_source("yuling.five"),
            user.join("schema/yuling/five/yuling.five.schema.yaml")
        );
        touch(&user.join("yuling.five.schema.yaml")); // 扁平存在也不回退
        assert_eq!(
            layout.schema_source("yuling.five"),
            user.join("schema/yuling/five/yuling.five.schema.yaml")
        );
        touch(&user.join("schema/yuling/five/yuling.five.schema.yaml"));
        assert_eq!(
            layout.schema_source("yuling.five"),
            user.join("schema/yuling/five/yuling.five.schema.yaml")
        );
        // 用户目录没有 → 共享目录的系列路径
        touch(&shared.join("schema/yustar/words/yustar.words.schema.yaml"));
        assert_eq!(
            layout.schema_source("yustar.words"),
            shared.join("schema/yustar/words/yustar.words.schema.yaml")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn compiled_config_writes_into_schema_subdir() {
        let root = temp("compiled");
        let layout = Layout::new(&root, root.join("shared"), root.join("user"), root.join("build"), root.join("prebuilt"));
        // `.schema` 资源在 staging 根下是 `<staging>/<leaf>/<leaf>.schema.yaml`
        assert_eq!(
            layout.staged_output("luna.schema"),
            root.join("build/luna/luna.schema.yaml")
        );
        // 非 schema 资源保持扁平
        assert_eq!(layout.staged_output("default"), root.join("build/default.yaml"));
    }

    #[test]
    fn deployed_never_falls_back_to_prebuilt_flat() {
        let root = temp("deployed");
        // 出厂产物目录名必须是 `build`（staging 根判定看目录名），与真实布局一致。
        let staging = root.join("user/build");
        let prebuilt = root.join("shared/build");
        let layout = Layout::new(&root, root.join("shared"), root.join("user"), &staging, &prebuilt);
        // prebuilt 只有扁平文件 → 不认（2026-10-25 删回退），返回 staging 的 nested 路径
        touch(&prebuilt.join("stroke.schema.yaml"));
        assert_eq!(
            layout.compiled_schema("stroke"),
            staging.join("stroke/stroke.schema.yaml"),
            "prebuilt 扁平不再被回退"
        );
        // prebuilt 有嵌套文件 → 用嵌套
        touch(&prebuilt.join("stroke/stroke.schema.yaml"));
        assert_eq!(
            layout.compiled_schema("stroke"),
            prebuilt.join("stroke/stroke.schema.yaml")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn user_db_and_trash_dirs() {
        let root = temp("dirs");
        let layout = Layout::new(Path::new("p"), Path::new("s"), root.join("user"), root.join("user/build"), Path::new("p"));
        assert_eq!(layout.user_db_dir(), root.join("user/userdb"));
        assert_eq!(layout.trash_dir(), root.join("user/trash"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
