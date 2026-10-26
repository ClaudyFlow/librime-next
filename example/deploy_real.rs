//! 真机数据复现：用户目录复制到临时沙箱（**不动真数据**），共享目录指向真实程序目录，
//! 跑完整 `deploy_workspace()`，统计成败并打印失败项。
//!
//! 用途：复现真机 `deploy()` 报 18 success / 2 failure（yustar / yustar_sc 词典源解析）
//! 与 symbols.yaml 路径异常，验证修复效果。
//!
//! 用法：`cargo run --example deploy_real`

use std::path::{Path, PathBuf};

use librime_next::lever::deployer::Deployer;

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<u64> {
    let mut copied = 0u64;
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copied += copy_dir(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
            copied += 1;
        }
    }
    Ok(copied)
}

fn main() {
    let shared = PathBuf::from(r"C:\Program Files\Weasel-Next\Rime-Next");
    let real_user = match std::env::var_os("APPDATA") {
        Some(appdata) => PathBuf::from(appdata).join("Rime-Next"),
        None => {
            eprintln!("APPDATA 未设置");
            std::process::exit(1);
        }
    };
    if !shared.is_dir() || !real_user.is_dir() {
        eprintln!(
            "跳过：真机目录不存在（shared={}, user={})",
            shared.display(),
            real_user.display()
        );
        std::process::exit(2);
    }

    // 沙箱：用户目录复制副本；build/ 与 backup/ 不拷（旧产物会干扰增量判断）
    let work = std::env::temp_dir().join(format!("deploy-real-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let sandbox_user = work.join("user");
    let n = copy_dir(&real_user, &sandbox_user).expect("复制用户目录");
    let _ = std::fs::remove_dir_all(sandbox_user.join("build"));
    let staging = work.join("build");
    println!("沙箱用户目录：{}（复制 {n} 个文件）", sandbox_user.display());
    println!("共享目录：{}", shared.display());

    let mut deployer = Deployer::with_data_dirs(&shared, &sandbox_user);
    deployer.staging_dir = staging.clone();
    deployer.distribution_name = "Weasel-Next".into();
    deployer.distribution_code_name = "weasel-next".into();
    deployer.distribution_version = "0.14.0".into();
    deployer.app_name = "小狼毫".into();

    let t0 = std::time::Instant::now();
    let ok = deployer.deploy_workspace();
    println!(
        "\n=== deploy_workspace 返回 {ok}（耗时 {:.1}s）===",
        t0.elapsed().as_secs_f64()
    );

    // 统计 staging 里的方案产物
    let mut schemas = 0usize;
    let mut files = 0usize;
    if let Ok(entries) = std::fs::read_dir(&staging) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                schemas += 1;
                files += std::fs::read_dir(entry.path()).map(|d| d.count()).unwrap_or(0);
            }
        }
    }
    println!("staging：{schemas} 个方案目录，{files} 个产物文件");
    println!("工作目录：{}", work.display());
}
