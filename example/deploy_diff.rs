//! 跑一遍 deploy_workspace，把产物 dump 出来供对比。
//!
//! 安全设计（**不碰真实数据**）：
//! * `user_data_dir` 用只含 `schema/schema.toml` 的**沙箱**（workspace_update 只从
//!   用户目录读 schema.toml 清单）；这样 installation_update / cleanup_trash
//!   的写动作都落在沙箱里，真实的 `%APPDATA%/Rime-Next` 完全不受影响。
//! * `staging_dir` 指到临时目录；
//! * `shared_data_dir` 用真实的 `C:/code/Weasel-Next/Rime-Next`（只读源布局）。

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

    // 沙箱用户目录：只放 schema/schema.toml（从真实用户目录拷一份）
    let real_user =
        PathBuf::from(r"C:\Users\CloudyFlow\AppData\Roaming\Rime-Next");
    let sandbox_user = std::env::temp_dir().join(format!("deploy-sandbox-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox_user);
    std::fs::create_dir_all(sandbox_user.join("schema")).unwrap();
    std::fs::copy(
        real_user.join("schema").join("schema.toml"),
        sandbox_user.join("schema").join("schema.toml"),
    )
    .ok();

    let out_dir = std::env::temp_dir().join(format!("deploy-diff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out_dir);
    let staging = out_dir.join("build");

    let mut deployer = Deployer::with_data_dirs(&shared, &sandbox_user);
    deployer.staging_dir = staging.clone();
    deployer.distribution_name = "Weasel-Next".into();
    deployer.distribution_code_name = "weasel-next".into();
    deployer.distribution_version = "0.14.0".into();
    deployer.app_name = "小狼毫".into();

    let t0 = std::time::Instant::now();
    let ok = deployer.deploy_workspace();
    let elapsed = t0.elapsed();

    let mut tree = Vec::new();
    dump_tree(&out_dir, &out_dir, &mut tree);
    tree.sort();

    println!("=== deploy_workspace 返回: {ok}（耗时 {:.2}s）===", elapsed.as_secs_f64());
    println!("=== 沙箱产物清单（{} 项）===", tree.len());
    for line in &tree {
        println!("{line}");
    }

    let out_file = std::env::temp_dir().join("deploy-diff-output.txt");
    std::fs::write(&out_file, tree.join("\n")).ok();
    println!("\n=== 已写入 {} ===", out_file.display());

    // 供 diff 用的清单：文件相对路径
    let manifest = tree
        .iter()
        .filter(|l| l.starts_with("F  "))
        .map(|l| l[3..].trim().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let manifest_file = std::env::temp_dir().join("deploy-diff-manifest.txt");
    std::fs::write(&manifest_file, manifest).ok();
    println!("=== manifest 已写入 {} ===", manifest_file.display());
}
