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

/// 共享数据目录（`SHARE_DIR`）。
///
/// 0.18.6 起共享数据在 `<安装目录>\data`（如
/// `C:\Program Files\Rime-Next\Weasel-Next-0.18.6\data`）；`C:\Program Files\Weasel-Next\Rime-Next`
/// 是 0.18.4 时代的旧布局，只有老装机上还在。**别再写死旧路径当唯一来源**——
/// 那会让这个"真实数据"端到端测的是一棵过期树。
fn shared_data_dir() -> Option<PathBuf> {
    // 按安装目录名升序取最高的一个（0.18.5 < 0.18.6），只要它带 data\schema。
    if let Ok(entries) = std::fs::read_dir(r"C:\Program Files\Rime-Next") {
        let mut candidates: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path().join("data"))
            .filter(|data| data.join("schema").is_dir())
            .collect();
        candidates.sort();
        if let Some(data) = candidates.pop() {
            return Some(data);
        }
    }
    let legacy = PathBuf::from(r"C:\Program Files\Weasel-Next\Rime-Next");
    legacy.is_dir().then_some(legacy)
}

fn real_roots() -> Option<(PathBuf, PathBuf)> {
    let shared = shared_data_dir()?;
    let user = std::env::var_os("APPDATA")
        .map(|a| PathBuf::from(a).join("Rime-Next"))?;
    user.is_dir().then(|| (shared, user))
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
    // 方案可以把自己的 `ascii_mode` 开关写成 `reset: 1`（如 sunman）⇒ 引擎按方案默认态
    // 进入**西文优先**，`ascii_composer` 把字母键全部 Rejected 给操作系统。那是方案自身
    // 的默认状态，不是引擎故障；这里象用户按一下 Shift 那样先切回中文再探。
    if let Some(engine) = session.engine.as_mut() {
        engine.context.set_option("ascii_mode", false);
    }
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
                // 惰性菜单：`candidate_count()` 只数已物化的条目，`all_candidates()`
                // 才会 `prepare(usize::MAX)`。这里要的是"这个方案能不能出候选"，
                // 必须走后者，否则恒 0。
                .map(|m| m.all_candidates().len())
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

    // 已部署方案清单：`workspace_update` 会把清单写回 `staging/default.yaml` 的
    // `schema_list`，这是**唯一可靠**的来源。
    // 别用 staging 的顶层目录当清单——2026-10-28 改成系列文件夹布局后，顶层是
    // `pinyin` / `wubi` / `double_pinyin` 这类**系列名**，不是方案 id（`pinyin.luna` / `wubi.86`），
    // 拿它去 `Session::create` 会一律"引擎加载失败"。
    let default_yaml = staging.join("default.yaml");
    let mut schemas: Vec<String> = librime_next::load_file(&default_yaml)
        .ok()
        .and_then(|value| {
            value
                .pointer("/schema_list")
                .and_then(|list| list.as_array())
                .cloned()
        })
        .map(|list| {
            list.iter()
                .filter_map(|item| {
                    item.pointer("/schema")
                        .and_then(|id| id.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    schemas.sort();
    schemas.dedup();
    assert!(
        !schemas.is_empty(),
        "staging/default.yaml 的 schema_list 应有方案（部署没写回清单？）"
    );

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
    // 方案 id 在 2026-10-28 改成点分系列 id：`luna_pinyin_simp` → `pinyin.luna`、
    // `wubi86` → `wubi.86`。两种写法都认，谁在清单里用谁。
    let pick = |candidates: &[&str]| -> Option<String> {
        candidates
            .iter()
            .find(|want| schemas.iter().any(|id| id == *want))
            .map(|id| (*id).to_string())
    };
    {
        let luna = pick(&["pinyin.luna", "luna_pinyin_simp"])
            .expect("staging 里应有拼音方案（pinyin.luna / luna_pinyin_simp）");
        let mut s = Session::create(&staging, &luna);
        assert!(s.engine.is_some(), "{luna}: 引擎应加载成功");
        for ch in "nihao".chars() {
            assert!(s.process_key(ch as u32, 0), "{luna}: 键 {ch} 应被消费");
        }
        let has_nihao = s.context().map_or(false, |c| {
            c.composition
                .segmentation
                .segments
                .iter()
                .filter_map(|seg| seg.menu.as_ref())
                .flat_map(|m| m.all_candidates().into_iter().map(|c| c.text.clone()))
                .any(|t| t == "你好")
        });
        assert!(has_nihao, "{luna}: nihao 应出「你好」");
    }
    {
        let wubi = pick(&["wubi.86", "wubi86"]).expect("staging 里应有五笔方案（wubi.86 / wubi86）");
        let mut s = Session::create(&staging, &wubi);
        assert!(s.engine.is_some(), "{wubi}: 引擎应加载成功");
        for ch in "aaaa".chars() {
            assert!(s.process_key(ch as u32, 0), "{wubi}: 键 {ch} 应被消费");
        }
        let has_gong = s.context().map_or(false, |c| {
            c.composition
                .segmentation
                .segments
                .iter()
                .filter_map(|seg| seg.menu.as_ref())
                .flat_map(|m| m.all_candidates().into_iter().map(|c| c.text.clone()))
                .any(|t| t == "工")
        });
        assert!(has_gong, "{wubi}: aaaa 应出「工」");
    }

    let _ = std::fs::remove_dir_all(&work);
    assert!(
        failures.is_empty(),
        "有方案未通过：\n{}",
        failures.join("\n")
    );
}
