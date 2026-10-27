//! 繁简双变体验证:一个 pinyin 方案,繁简开关二选一。
//!
//! * simplification = true(默认,reset 1)→ simp 翻译器 → pinyin.simp 词库 → 简体候选
//! * simplification = false        → trad 翻译器 → 词库缺失 → 借 simp 词库 +
//!   opencc(s2t)实时转繁体
//!
//! 用法:cargo run --example probe_variants

use librime_next::engine::Session;
use librime_next::lever::deployer::Deployer;
use librime_next::lever::deployment_tasks::DeploymentTask;
use std::path::PathBuf;

fn candidates(session: &Session) -> Vec<String> {
    session
        .context()
        .map(|c| {
            c.composition
                .segmentation
                .segments
                .iter()
                .filter_map(|s| s.menu.as_ref())
                .flat_map(|m| m.candidates().iter().map(|c| c.text.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn copy_dir(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap().flatten() {
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), to).unwrap();
        }
    }
}

fn main() {
    let shared = PathBuf::from(r"C:\Program Files\Weasel-Next\Rime-Next");
    let repo_schema = PathBuf::from(r"C:\Code\Weasel-Next\Rime-Next\schema");
    let source = repo_schema.join("pinyin").join("pinyin.schema.yaml");

    let work = std::env::temp_dir().join(format!("pinyin-variants-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let sandbox_user = work.join("user");
    let staging = work.join("build");
    std::fs::create_dir_all(&sandbox_user).unwrap();
    // 标准布局:用户目录持有方案文件(部署 config_source 用户优先)。
    // 从仓库复制 pinyin 方案夹(含 simp/ 系列词库),模拟真实安装。
    copy_dir(&repo_schema.join("pinyin"), &sandbox_user.join("schema").join("pinyin"));

    let mut deployer = Deployer::with_data_dirs(&shared, &sandbox_user);
    deployer.staging_dir = staging.clone();
    let ok = DeploymentTask::SchemaUpdate {
        source_path: source,
        verbose: true,
    }
    .run(&mut deployer);
    println!("pinyin SchemaUpdate: {ok}");
    assert!(ok, "部署应成功(simp 词库系列路径 + id: 标准)");

    // 共享目录的 roots(simplifier 的 opencc 数据在共享 opencc/)
    let roots = vec![sandbox_user.clone(), shared.clone()];

    // ---- 简体态(开关默认 true) ----------------------------------------
    let mut session = Session::create_with_roots(&staging, "pinyin", &roots);
    println!("引擎加载(简体态): {}", session.engine.is_some());
    for ch in "tianqibucuo".chars() {
        session.process_key(ch as u32, 0);
    }
    println!("翻译器(简体态): {:?}", session.engine.as_ref().map(|e| e.translator_names()));
    let simp = candidates(&session);
    println!(
        "简体态候选前5: {:?}",
        &simp[..simp.len().min(5)]
    );

    // ---- 繁体态(开关切 false → trad 翻译器 + opencc 兜底) --------------
    session
        .engine_mut()
        .unwrap()
        .set_option("simplification", false);
    session.process_key(0xff1b, 0); // Esc 清场
    for ch in "tianqibucuo".chars() {
        session.process_key(ch as u32, 0);
    }
    println!("翻译器(繁体态): {:?}", session.engine.as_ref().map(|e| e.translator_names()));
    println!("simplification 选项 = {}", session.context().unwrap().get_option("simplification"));
    let trad = candidates(&session);
    println!(
        "繁体态候选 {} 条,前8: {:?}",
        trad.len(),
        &trad[..trad.len().min(8)]
    );

    // 断言:两态不同,简体态是简体字,繁体态是繁体字
    assert!(!simp.is_empty() && !trad.is_empty(), "两态都应有候选");
    assert_ne!(simp, trad, "繁简两态候选应不同");
    assert!(
        simp.iter().any(|t| t.contains("天气")),
        "简体态应有「天气」"
    );
    assert!(
        trad.iter().any(|t| t.contains("天氣")),
        "繁体态应有「天氣」(opencc 兜底转换)"
    );
    println!("✔ 繁简双变体:一个方案,两本词库路径(其一 opencc 兜底),开关切换生效");
    let _ = std::fs::remove_dir_all(&work);
}
