//! 真机部署（**直接写真实用户目录**）：把 `Rime-Next\schema` 的改动编译进 `<用户>\build` 的棱镜。
//!
//! 用途：改了双拼的 `speller/algebra`（如加简拼）后，必须重跑部署才会重编 prism，
//! 否则引擎还在用旧棱镜，改了等于没改。
//!
//! 用法：`cargo run -p librime-next --example deploy_live [用户目录] [共享目录]`
//!   默认 用户 = `%APPDATA%\Rime-Next`，共享 = `C:\Program Files\Rime-Next\Weasel-Next-0.18.5\Rime-Next`

use std::path::PathBuf;

use librime_next::lever::deployer::Deployer;

fn main() {
    let user = match std::env::args().nth(1) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(
            std::env::var("APPDATA").expect("APPDATA"),
        )
        .join("Rime-Next"),
    };
    let shared = match std::env::args().nth(2) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(r"C:\Program Files\Rime-Next\Weasel-Next-0.18.5\Rime-Next"),
    };
    if !user.is_dir() || !shared.is_dir() {
        eprintln!(
            "目录不存在（user={}, shared={}）",
            user.display(),
            shared.display()
        );
        std::process::exit(2);
    }

    let staging = user.join("build");
    let mut deployer = Deployer::with_data_dirs(&shared, &user);
    deployer.staging_dir = staging.clone();
    deployer.distribution_name = "Weasel-Next".into();
    deployer.distribution_code_name = "weasel-next".into();
    deployer.distribution_version = "0.18.5".into();
    deployer.app_name = "小狼毫".into();

    println!("用户目录：{}", user.display());
    println!("共享目录：{}", shared.display());
    println!("产物目录：{}", staging.display());

    let t0 = std::time::Instant::now();
    let ok = deployer.deploy_workspace();
    println!(
        "\n=== deploy_workspace 返回 {ok}（耗时 {:.1}s）===",
        t0.elapsed().as_secs_f64()
    );
}
