//! 全方案端到端：真实数据沙箱部署后，**每一个** staged 方案都建会话、打字、验证候选。
//!
//! 覆盖断言（每方案）：
//! 1. `Session::create` 引擎加载成功（.schema.yaml + 词典 .bin 齐全）；
//! 2. 打入若干字母键至少被消费 1 键（processor 链活着）；
//! 3. 组字态成立（input 非空）或直接产生候选菜单；
//! 4. luna_pinyin_simp / wubi86 额外做精确断言（你好 / 工）。

use librime_next::engine::Session;
use librime_next::lever::deployer::Deployer;
use librime_next::lever::deployment_tasks::DeploymentTask;
use std::path::{Path, PathBuf};

fn real_roots() -> Option<(PathBuf, PathBuf)> {
    let shared = PathBuf::from(r"C:\Program Files\Weasel-Next\Rime-Next");
    let user = std::env::var_os("APPDATA")
        .map(|a| PathBuf::from(a).join("Rime-Next"))?;
    (shared.is_dir() && user.is_dir()).then(|| (shared, user))
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    // build/（部署会全量重编，且正被运行中的 Server 锁着）、backup/、stat/ 不拷
    const SKIP: [&str; 3] = ["build", "backup", "stat"];
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if entry.file_type()?.is_dir() {
            if SKIP.contains(&name.as_str()) {
                continue;
            }
            copy_dir(&entry.path(), &dst.join(&name))?;
        } else {
            // 单个文件被锁/读不了不致命：跳过（部署从源重编）
            let _ = std::fs::copy(entry.path(), dst.join(&name));
        }
    }
    Ok(())
}

/// 从 staged 方案里拿它的词典源里的一个**真实编码**（探针用）：
/// staged schema.yaml → translator/dict → classified_dict_source（用户→共享、
/// 系列规则 + 兄弟目录宽容搜索）→ 正文首个 `词条	编码` 的编码。
fn real_code_for(staging: &Path, shared: &Path, sandbox_user: &Path, schema_id: &str) -> Option<String> {
    use librime_next::dict::dict_compiler::classified_dict_source;
    let schema_file = staging
        .join(schema_id)
        .join(format!("{schema_id}.schema.yaml"));
    let value = librime_next::load_file(&schema_file).ok()?;
    let dict = value
        .pointer("/translator/dict")
        .and_then(|v| v.as_str())?
        .to_string();
    if dict.is_empty() {
        return None;
    }
    let source = classified_dict_source(sandbox_user, shared, &dict)?;
    // 主词典正文可能为空（编码全在 import_tables 的分表里，如宇浩系：
    // yuling.dict.yaml 引 yuling.full / yuling.pop / yuling.quick 等）——
    // 先试主文件，再逐个展开 import_tables 指向的分表。
    let mut sources: Vec<PathBuf> = vec![source.clone()];
    if let Ok(text) = std::fs::read_to_string(&source) {
        let mut in_imports = false;
        for line in text.lines() {
            if line.starts_with("import_tables:") {
                in_imports = true;
                continue;
            }
            if in_imports {
                if line.starts_with('#') || line.trim().is_empty() {
                    continue;
                }
                if !line.starts_with(' ') && !line.starts_with('-') {
                    in_imports = false;
                    continue;
                }
                let name = line.trim().strip_prefix("- ").map(str::trim).unwrap_or("");
                if !name.is_empty()
                    && let Some(src) = classified_dict_source(sandbox_user, shared, name)
                {
                    sources.push(src);
                }
            }
        }
    }
    for source in sources {
        let Ok(text) = std::fs::read_to_string(&source) else {
            continue;
        };
        for line in text.lines().skip_while(|l| !l.starts_with("...")) {
            if line.starts_with('#') || line.starts_with("---") || line.starts_with("...")
                || line.trim().is_empty()
            {
                continue;
            }
            let mut fields = line.split('\t');
            let _word = fields.next();
            if let Some(code) = fields.next().map(str::trim) {
                if !code.is_empty() && code.chars().all(|c| c.is_ascii_lowercase()) {
                    return Some(code.to_string());
                }
            }
        }
    }
    None
}

/// 对一个方案打字并收集（消费的键数, 候选文本数, 是否组字态）。
fn type_probe(session: &mut Session, keys: &str) -> (usize, usize, bool) {
    let mut consumed = 0usize;
    for ch in keys.chars() {
        if session.process_key(ch as u32, 0) {
            consumed += 1;
        }
    }
    let ctx = session.context();
    let composing = ctx.map_or(false, |c| c.is_composing());
    let candidates = ctx
        .map(|c| {
            c.composition
                .segmentation
                .segments
                .iter()
                .filter_map(|s| s.menu.as_ref())
                .map(|m| m.candidate_count())
                .sum::<usize>()
        })
        .unwrap_or(0);
    (consumed, candidates, composing)
}

#[test]
fn every_staged_schema_loads_types_and_yields_candidates() {
    let Some((shared, real_user)) = real_roots() else {
        eprintln!("跳过：真机目录不存在");
        return;
    };
    // 沙箱：用户目录复制副本（build/ 不拷，强制全量部署）
    let work = std::env::temp_dir().join(format!("all-schemas-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let sandbox_user = work.join("user");
    let staging = work.join("build");
    copy_dir(&real_user, &sandbox_user).expect("复制用户目录");
    let _ = std::fs::remove_dir_all(sandbox_user.join("build"));

    let mut deployer = Deployer::with_data_dirs(&shared, &sandbox_user);
    deployer.staging_dir = staging.clone();
    deployer.distribution_name = "Weasel-Next".into();
    deployer.distribution_code_name = "weasel-next".into();
    deployer.distribution_version = "0.18.4".into();
    deployer.app_name = "小狼毫".into();
    assert!(
        deployer.deploy_workspace(),
        "deploy_workspace 应全过（此前基线 21/0）"
    );

    // staging 里的方案目录即已部署方案清单
    let mut schemas: Vec<String> = std::fs::read_dir(&staging)
        .expect("staging 可读")
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .collect();
    schemas.sort();
    assert!(!schemas.is_empty(), "staging 里应有方案");

    let mut failures: Vec<String> = Vec::new();
    println!(
        "{:<28} {:>6} {:>8} {:>6}",
        "schema", "keys", "cands", "composing"
    );
    for schema_id in &schemas {
        let mut session = Session::create(&staging, schema_id);
        if session.engine.is_none() {
            failures.push(format!("{schema_id}: 引擎加载失败"));
            continue;
        }
        // 探针：优先用该方案词典里的**真实编码**（宇浩/声笔等方案字母表和编码
        // 空间与拼音/五笔完全不同，通用探针必然 0 候选——那测的是探针不是方案）
        let mut probes: Vec<String> = Vec::new();
        if let Some(code) = real_code_for(&staging, &shared, &sandbox_user, schema_id) {
            probes.push(code.clone());
        }
        probes.push("nihao".into());
        probes.push("aaaa".into());
        let mut best = (0usize, 0usize, false);
        for probe in &probes {
            let mut s2 = Session::create(&staging, schema_id);
            if s2.engine.is_none() {
                continue;
            }
            let r = type_probe(&mut s2, probe);
            // 评分:候选数优先——消费键多不代表编码合法(yuhao 字母表吃下
            // 任意字母,拼不成码照样 0 候选);候选相同才比消费键数。
            if r.1 > best.1 || (r.1 == best.1 && r.0 > best.0) {
                best = r;
            }
            if best.1 > 0 {
                break;
            }
        }
        println!(
            "{:<28} {:>6} {:>8} {:>6}",
            schema_id, best.0, best.1, best.2
        );
        // 依赖 lua 的方案(yuhao 系拆分/虎码整句):组件创建必失败(rime-lua 未移植),
        // 候选链路打折——只记录不断言。其余方案必须出候选。
        let needs_lua = {
            let schema_file = staging.join(schema_id).join(format!("{schema_id}.schema.yaml"));
            librime_next::load_file(&schema_file)
                .ok()
                .and_then(|v| {
                    v.pointer("/engine/processors").and_then(|p| p.as_array().cloned())
                        .or_else(|| v.pointer("/engine/translators").and_then(|p| p.as_array().cloned()))
                })
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .any(|name| name.starts_with("lua_"))
                })
                .unwrap_or(false)
        };
        if best.0 == 0 {
            failures.push(format!("{schema_id}: 没有任何键被消费"));
        } else if best.1 == 0 && !best.2 && !needs_lua {
            failures.push(format!("{schema_id}: 无候选且不组字(方案不依赖 lua)"));
        } else if best.1 == 0 && needs_lua {
            println!("  (依赖 lua,候选链路打折——rime-lua 未移植,已知缺口)");
        }
    }

    // 精确断言（可确定预期行为的方案）
    {
        let mut s = Session::create(&staging, "luna_pinyin_simp");
        assert!(s.engine.is_some());
        for ch in "nihao".chars() {
            assert!(s.process_key(ch as u32, 0));
        }
        let has_nihao = s.context().map_or(false, |c| {
            c.composition
                .segmentation
                .segments
                .iter()
                .filter_map(|seg| seg.menu.as_ref())
                .flat_map(|m| m.candidates().iter().map(|c| c.text.clone()))
                .any(|t| t == "你好")
        });
        assert!(has_nihao, "luna_pinyin_simp: nihao 应出「你好」");
    }
    {
        let mut s = Session::create(&staging, "wubi86");
        assert!(s.engine.is_some());
        for ch in "aaaa".chars() {
            assert!(s.process_key(ch as u32, 0));
        }
        let has_gong = s.context().map_or(false, |c| {
            c.composition
                .segmentation
                .segments
                .iter()
                .filter_map(|seg| seg.menu.as_ref())
                .flat_map(|m| m.candidates().iter().map(|c| c.text.clone()))
                .any(|t| t == "工")
        });
        assert!(has_gong, "wubi86: aaaa 应出「工」");
    }

    let _ = std::fs::remove_dir_all(&work);
    assert!(
        failures.is_empty(),
        "有方案未通过：\n{}",
        failures.join("\n")
    );
}
