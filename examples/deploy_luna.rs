//! 逐步部署测试：只跑一个方案 luna_pinyin_simp 的 SchemaUpdate。
//!
//! 安全设计（**不碰真实数据**）：
//! * `shared_data_dir` = 真实源布局 `C:/code/Weasel-Next/Rime-Next`（只读）
//! * `user_data_dir` = 临时沙箱（SchemaUpdate 不读用户目录的方案清单，只读源文件）
//! * `staging_dir` = 临时目录
//!
//! luna_pinyin_simp 的 `__include: luna_pinyin.schema:/` 依赖 luna_pinyin 源文件，
//! 编译时会一并解析。

use std::path::PathBuf;

use librime_next::lever::deployer::Deployer;
use librime_next::lever::deployment_tasks::DeploymentTask;

fn dump_tree(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .display()
            .to_string();
        if path.is_dir() {
            out.push(format!("D  {rel}/"));
            dump_tree(&path, base, out);
        } else {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            out.push(format!("F  {rel}  ({size} B)"));
        }
    }
}

fn main() {
    let shared = PathBuf::from(r"C:\code\Weasel-Next\Rime-Next");

    let work = std::env::temp_dir().join(format!("deploy-luna-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let sandbox_user = work.join("user");
    let staging = work.join("build");
    std::fs::create_dir_all(&sandbox_user).unwrap();

    let mut deployer = Deployer::with_data_dirs(&shared, &sandbox_user);
    deployer.staging_dir = staging.clone();
    deployer.distribution_name = "Weasel-Next".into();
    deployer.distribution_code_name = "weasel-next".into();
    deployer.distribution_version = "0.14.0".into();
    deployer.app_name = "小狼毫".into();

    // 只部署一个方案：luna_pinyin_simp
    let source = shared
        .join("schema")
        .join("luna_pinyin_simp")
        .join("luna_pinyin_simp.schema.yaml");

    println!("=== 源文件: {} ===", source.display());
    println!("exists = {}", source.is_file());

    let t0 = std::time::Instant::now();
    let ok = DeploymentTask::SchemaUpdate {
        source_path: source,
        verbose: true,
    }
    .run(&mut deployer);
    let elapsed = t0.elapsed();

    println!("\n=== SchemaUpdate 返回: {ok}（耗时 {:.2}s）===", elapsed.as_secs_f64());

    let mut tree = Vec::new();
    dump_tree(&work, &work, &mut tree);
    tree.sort();

    println!("\n=== staging 产物（{} 项）===", tree.len());
    for line in &tree {
        println!("{line}");
    }

    // 检查关键产物
    let compiled = staging
        .join("luna_pinyin_simp")
        .join("luna_pinyin_simp.schema.yaml");
    println!("\n=== 编译产物是否存在: {} ===", compiled.is_file());
    if compiled.is_file() {
        println!("=== 编译产物内容（前 30 行）===");
        if let Ok(text) = std::fs::read_to_string(&compiled) {
            for line in text.lines().take(30) {
                println!("  {line}");
            }
        }
    }

    println!("\n=== 工作目录: {} ===", work.display());
}
