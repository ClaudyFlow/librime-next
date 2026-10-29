//! 单方案编译探针：直接调 `config::compile`，把失败原因打出来。
//!
//! 用法：`probe_schema <schema.yaml> <default.yaml> <数据根...>`
//!
//! 存在的理由：部署流程里 `SchemaUpdate` 失败只返回 false（C++ 同款），
//! 错误不会出现在 `deploy` 的输出里；排查「某个方案进不了包」时用它看真因。
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("用法：probe_schema <schema.yaml> <default.yaml> [数据根...]");
        std::process::exit(2);
    }
    let source = PathBuf::from(&args[1]);
    let defaults = PathBuf::from(&args[2]);
    let roots: Vec<PathBuf> = args[3..].iter().map(PathBuf::from).collect();
    match librime_next::config::compile(&source, &defaults, &roots) {
        Ok(tree) => {
            let yaml = librime_next::config::to_yaml_string(&tree).unwrap_or_default();
            println!("OK yaml_bytes={}", yaml.len());
            for line in yaml.lines().take(8) {
                println!("  {line}");
            }
        }
        Err(error) => {
            println!("ERR {error}");
            std::process::exit(1);
        }
    }
}
