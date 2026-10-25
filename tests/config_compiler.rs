//! 逐条移植 `librime/test/config_compiler_test.cc` 的断言。
//!
//! 数据是 `librime/data/test/` 里的原文件（`config_compiler_test.yaml` 等），
//! 覆盖 include（本地/外部/文件）、patch（引用/字面量/列表）、依赖链与优先级、
//! optional 引用、merge/append、原地 patch 造列表、循环依赖的 best-effort 解析。
//!
//! 注意：C++ 测试用的是**不带插件**的 `ConfigComponent<ConfigBuilder>`，
//! 这里同样用 `ConfigBuilder::new()`，不装 auto-patch/default/preset 插件。

use std::path::PathBuf;

use librime_next::config::{Config, ConfigBuilder, ResourceResolver};

fn load(id: &str) -> Config {
    let data_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let resolver = ResourceResolver::new(vec![data_dir]);
    let data = ConfigBuilder::new().load_config(resolver, id);
    Config::with_data(data)
}

fn s(config: &Config, path: &str) -> String {
    config
        .get_string(path)
        .unwrap_or_else(|| panic!("missing string at {path}"))
}

fn i(config: &Config, path: &str) -> i32 {
    config
        .get_int(path)
        .unwrap_or_else(|| panic!("missing int at {path}"))
}

#[test]
fn ported_librime_config_compiler_tests() {
    let config = load("config_compiler_test");

    // IncludeLocalReference
    for n in 0..4 {
        let prefix = format!("include_local_reference/@{n}/");
        assert!(config.is_null(&(prefix.clone() + "__include")));
        assert_eq!(
            s(&config, &(prefix.clone() + "terrans/player")),
            "slayers_boxer"
        );
        assert_eq!(s(&config, &(prefix.clone() + "protoss/player")), "grrrr");
        assert_eq!(s(&config, &(prefix.clone() + "zerg/player")), "yellow");
    }

    // IncludeExternalReference
    let prefix = "include_external_reference/";
    assert!(config.is_null(&(prefix.to_string() + "terrans/__include")));
    assert_eq!(
        i(&config, &(prefix.to_string() + "terrans/tank/cost/mineral")),
        150
    );
    assert_eq!(
        i(&config, &(prefix.to_string() + "terrans/tank/cost/gas")),
        100
    );
    assert!(config.is_null(&(prefix.to_string() + "protoss")));
    assert!(config.is_null(&(prefix.to_string() + "zerg")));

    // IncludeExternalFile
    let prefix = "include_external_file/";
    assert!(config.is_null(&(prefix.to_string() + "__include")));
    assert!(config.is_map(&(prefix.to_string() + "terrans")));
    assert!(config.is_map(&(prefix.to_string() + "protoss")));
    assert!(config.is_map(&(prefix.to_string() + "zerg")));
    assert_eq!(
        i(&config, &(prefix.to_string() + "terrans/supply/produced")),
        28
    );
    assert!(config.is_list(&(prefix.to_string() + "protoss/air_force")));
    assert_eq!(s(&config, &(prefix.to_string() + "zerg/queen")), "Kerrigan");

    // PatchReference
    let prefix = "patch_reference/";
    assert!(config.is_null(&(prefix.to_string() + "__patch")));
    assert_eq!(
        config.get_list_size(&(prefix.to_string() + "battlefields")),
        4
    );
    assert_eq!(
        s(&config, &(prefix.to_string() + "battlefields/@3")),
        "match point"
    );

    // PatchLiteral
    let prefix = "patch_literal/";
    assert!(config.is_null(&(prefix.to_string() + "__patch")));
    assert_eq!(
        config.get_list_size(&(prefix.to_string() + "zerg/ground_units")),
        6
    );
    assert_eq!(
        s(&config, &(prefix.to_string() + "zerg/ground_units/@5")),
        "lurker"
    );
    assert_eq!(config.get_list_size("/starcraft/zerg/ground_units"), 5);

    // PatchList
    let prefix = "patch_list/";
    assert!(config.is_null(&(prefix.to_string() + "protoss/__patch")));
    assert_eq!(
        config.get_list_size(&(prefix.to_string() + "protoss/ground_units")),
        8
    );
    assert_eq!(
        s(&config, &(prefix.to_string() + "protoss/ground_units/@6")),
        "dark templar"
    );
    assert_eq!(
        s(&config, &(prefix.to_string() + "protoss/ground_units/@7")),
        "dark archon"
    );
    assert_eq!(config.get_list_size("/starcraft/protoss/ground_units"), 6);

    // DependencyChaining / DependencyPriorities
    let config = load("config_dependency_test");
    let prefix = "dependency_chaining/";
    for key in ["alpha", "beta", "delta"] {
        assert!(config.is_null(&format!("{prefix}{key}/__include")));
        assert_eq!(s(&config, &format!("{prefix}{key}")), "success");
    }
    let prefix = "dependency_priorities/";
    for key in ["terrans", "protoss"] {
        assert!(config.is_null(&format!("{prefix}{key}/__include")));
        assert!(config.is_null(&format!("{prefix}{key}/__patch")));
    }
    assert_eq!(s(&config, &(prefix.to_string() + "terrans/player")), "nada");
    assert_eq!(s(&config, &(prefix.to_string() + "protoss/player")), "bisu");

    // OptionalReference
    let config = load("config_optional_reference_test");
    assert!(config.is_null("/__include"));
    assert!(config.is_null("/__patch"));
    assert_eq!(config.get_bool("/untouched"), Some(true));

    // Merge tests
    let config = load("config_merge_test");

    // AppendWithInclude
    let prefix = "append_with_include/";
    assert!(config.is_null(&(prefix.to_string() + "list/__include")));
    assert!(config.is_null(&(prefix.to_string() + "list/__append")));
    assert!(config.is_list(&(prefix.to_string() + "list")));
    assert_eq!(config.get_list_size(&(prefix.to_string() + "list")), 8);
    assert_eq!(
        s(&config, &(prefix.to_string() + "list/@6")),
        "dark templar"
    );
    assert_eq!(s(&config, &(prefix.to_string() + "list/@7")), "dark archon");
    assert_eq!(config.get_list_size("/starcraft/protoss/ground_units"), 6);

    // AppendWithPatch
    let prefix = "append_with_patch/";
    assert!(config.is_null(&(prefix.to_string() + "__include")));
    assert!(config.is_null(&(prefix.to_string() + "__patch")));
    assert_eq!(
        s(&config, &(prefix.to_string() + "terrans/player")),
        "slayers_boxer, nada"
    );
    assert_eq!(
        config.get_list_size(&(prefix.to_string() + "terrans/air_units")),
        2
    );
    assert_eq!(
        s(&config, &(prefix.to_string() + "terrans/air_units/@0")),
        "wraith"
    );
    assert_eq!(
        s(&config, &(prefix.to_string() + "terrans/air_units/@1")),
        "battlecruiser"
    );
    assert_eq!(
        config.get_list_size(&(prefix.to_string() + "protoss/ground_units")),
        8
    );
    assert_eq!(s(&config, "/starcraft/terrans/player"), "slayers_boxer");
    assert!(config.is_null("/starcraft/terrans/air_units"));
    assert_eq!(config.get_list_size("/starcraft/protoss/ground_units"), 6);

    // MergeTree
    let prefix = "merge_tree/";
    assert!(config.is_null(&(prefix.to_string() + "__include")));
    assert_eq!(
        s(&config, &(prefix.to_string() + "terrans/player")),
        "slayers_boxer"
    );
    assert!(config.is_null(&(prefix.to_string() + "terrans/air_units")));
    assert_eq!(
        config.get_list_size(&(prefix.to_string() + "terrans/ground_units")),
        7
    );
    assert_eq!(
        s(&config, &(prefix.to_string() + "terrans/ground_units/@5")),
        "medic"
    );
    assert_eq!(
        s(&config, &(prefix.to_string() + "terrans/ground_units/@6")),
        "goliath"
    );
    assert_eq!(
        s(&config, &(prefix.to_string() + "protoss/player")),
        "grrrr"
    );
    assert!(config.is_null(&(prefix.to_string() + "protoss/ground_units/__append")));
    assert_eq!(
        config.get_list_size(&(prefix.to_string() + "protoss/ground_units")),
        8
    );
    assert_eq!(
        s(&config, &(prefix.to_string() + "protoss/ground_units/@6")),
        "dark templar"
    );
    assert_eq!(
        s(&config, &(prefix.to_string() + "protoss/ground_units/@7")),
        "dark archon"
    );
    assert_eq!(s(&config, &(prefix.to_string() + "zerg/player")), "yellow");
    assert_eq!(
        config.get_list_size(&(prefix.to_string() + "zerg/ground_units")),
        0
    );
    assert!(config.is_null("/starcraft/terrans/ground_units"));
    assert_eq!(config.get_list_size("/starcraft/protoss/ground_units"), 6);
    assert_eq!(config.get_list_size("/starcraft/zerg/ground_units"), 5);

    // CreateListWithInplacePatch
    let prefix = "create_list_with_inplace_patch/";
    assert!(config.is_list(&(prefix.to_string() + "all_ground_units")));
    assert_eq!(
        config.get_list_size(&(prefix.to_string() + "all_ground_units")),
        16
    );

    // CircularDependencyTest.BestEffortResolution
    let config = load("config_circular_dependency_test");
    let prefix = "test/";
    assert!(config.is_null(&(prefix.to_string() + "__patch")));
    assert!(config.is_null(&(prefix.to_string() + "work/__include")));
    assert_eq!(s(&config, &(prefix.to_string() + "home")), "naive");
    assert_eq!(s(&config, &(prefix.to_string() + "work")), "excited");
}

/// `EditNode` 的算子：索引插入、`@next`、字符串追加、`/=` 覆盖、`__merge`。
#[test]
fn edit_operators() {
    let config = load("edit_operator_test");

    // letters/1+ 在 1 号位插入 [x, y]，@next 追加 z
    assert_eq!(config.get_list_size("indexed/letters"), 6);
    for (index, expected) in ["a", "x", "y", "b", "c", "z"].iter().enumerate() {
        assert_eq!(s(&config, &format!("indexed/letters/@{index}")), *expected);
    }
    // text/+ 追加字符串，随后 text/= 覆盖
    assert_eq!(s(&config, "indexed/text"), "over");
    // map/k1/= 覆盖单个键
    assert_eq!(s(&config, "indexed/map/k1"), "v2");
    // 原树不被修改（include 之后是在副本上 patch）
    assert_eq!(config.get_list_size("base/letters"), 3);
    assert_eq!(s(&config, "base/text"), "hello");
    assert_eq!(s(&config, "base/map/k1"), "v1");

    // __merge：映射递归合并、标量覆盖
    assert_eq!(s(&config, "merged/text"), "merged");
    assert_eq!(s(&config, "merged/map/k1"), "v1");
    assert_eq!(s(&config, "merged/map/k2"), "v2");
    assert_eq!(config.get_list_size("merged/letters"), 3);
    assert!(config.is_null("base/map/k2") || config.get_string("base/map/k2").is_none());
}
