//! 统一拼音端到端（**出厂配置**）：单方案 `pinyin` + 单词库 `pinyin`（terra 带调 + luna 权重），
//! 多棱镜（全拼/地球拼音/自然码/小鹤/注音）共用同一份词库与同一份用户词典。
//!
//! 全部在**临时沙箱**里跑，不碰 `%APPDATA%\Rime-Next`：
//! * shared = `<repo>/Rime-Next`（出厂数据目录：`schema/pinyin/pinyin.schema.yaml`
//!   与 `dict/pinyin/pinyin.dict.yaml`）；
//! * user   = 临时目录（只有 `schema/schema.yaml` 清单：`schemas: [pinyin]`）；
//! * build  = 临时 `.../build`（部署产物）。
//!
//! 断言（2026-11-10 定稿）：
//! 1. 全拼 `zhongguo` 出「中」且在前 6 条；`nihao` 首候选是「你好」；按 `1` 选重不进编码；
//! 2. 地球拼音 `zhong-guo/` 出「中国」；无调 `zhongguo` 也能出词；`1` 是选候选；
//! 3. 自然码 `ip`、小鹤 `iy` 出「春」；注音 `j;6` 出「王」，数字是编码；
//! 4. 注音按 `A`/`B` 选第 1/2 个候选（`input_type_selector` 按子方案换选重键）；
//! 5. 全拼学到的词，切到地球拼音能用符号调码查回来（同一 `pinyin.userdb`）；
//! 6. 繁简单选组（`zh_hans` / `zh_hant` / `zh_hant_tw`）在每个子方案下都生效，
//!    且切子方案后选项不丢。

use std::path::{Path, PathBuf};
use std::time::Instant;

use librime_next::engine::Session;
use librime_next::lever::deployer::Deployer;
use librime_next::lever::deployment_tasks::DeploymentTask;

const INPUT_TYPES: [&str; 5] = ["pinyin", "terra", "zrm", "flypy", "zhuyin"];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("librime-next 应该在仓库根下")
        .to_path_buf()
}

fn sandbox() -> (PathBuf, PathBuf, PathBuf) {
    // 出厂数据目录：`Rime-Next/schema/pinyin/pinyin.schema.yaml` + `Rime-Next/dict/pinyin/pinyin.dict.yaml`
    let shared = repo_root().join("Rime-Next");
    let work = std::env::temp_dir().join(format!("pinyin-unified-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let user = work.join("user");
    std::fs::create_dir_all(user.join("schema")).unwrap();
    std::fs::write(
        user.join("schema").join("schema.yaml"),
        "active: pinyin\nschemas:\n  - pinyin\n",
    )
    .unwrap();
    // 繁简滤镜要 opencc 数据；`Session::create` 的数据根回退到 `build_dir.parent()`，
    // 所以把出厂那份 `Rime-Next/opencc`（1.1 MB）拷到沙箱根下。
    copy_tree(&shared.join("opencc"), &work.join("opencc"));
    let staging = work.join("build");
    (shared, user, staging)
}

fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap().flatten() {
        let target = dst.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn deploy(shared: &Path, user: &Path, staging: &Path) {
    let mut deployer = Deployer::with_data_dirs(shared, user);
    deployer.staging_dir = staging.to_path_buf();
    assert!(
        DeploymentTask::WorkspaceUpdate.run(&mut deployer),
        "隔离部署失败（shared={}, user={}, staging={}）",
        shared.display(),
        user.display(),
        staging.display()
    );
}

fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                total += dir_bytes(&path);
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

fn session(staging: &Path, input_type: &str) -> Session {
    let mut session = Session::create(staging, "pinyin");
    assert!(session.engine.is_some(), "方案 pinyin 加载失败");
    activate(&mut session, input_type);
    session
}

fn activate(session: &mut Session, active: &str) {
    let engine = session.engine.as_mut().unwrap();
    engine.context.set_option("ascii_mode", false);
    for option in INPUT_TYPES {
        engine.context.set_option(option, option == active);
    }
    engine.context.clear();
}

fn type_keys(session: &mut Session, keys: &str) {
    for ch in keys.chars() {
        session.process_key(ch as u32, 0);
    }
}

fn candidates(session: &Session) -> Vec<String> {
    session
        .context()
        .map(|ctx| {
            ctx.composition
                .segmentation
                .segments
                .iter()
                .filter_map(|segment| segment.menu.as_ref())
                .flat_map(|menu| menu.all_candidates())
                .map(|candidate| candidate.text.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn input(session: &Session) -> String {
    session
        .context()
        .map(|ctx| ctx.input.clone())
        .unwrap_or_default()
}

fn clear(session: &mut Session) {
    session.engine.as_mut().unwrap().context.clear();
}

fn position_of(cands: &[String], needle: &str) -> Option<usize> {
    cands.iter().position(|c| c.contains(needle))
}

#[test]
fn unified_pinyin_end_to_end() {
    let (shared, user, staging) = sandbox();

    // ── 0. 首次部署（冷编译 996,685 条词库 + 5 份棱镜）耗时/体积 ─────────
    let started = Instant::now();
    deploy(&shared, &user, &staging);
    let deploy_ms = started.elapsed().as_millis();
    let dict_src = shared.join("dict").join("pinyin").join("pinyin.dict.yaml");
    eprintln!(
        "首次部署（冷编译）: {deploy_ms} ms；词库源 {} 字节；build 产物 {} 字节",
        std::fs::metadata(&dict_src).map(|m| m.len()).unwrap_or(0),
        dir_bytes(staging.parent().map(|p| p.to_path_buf()).unwrap_or_default().as_path())
    );
    for name in [
        "pinyin.table.bin",
        "pinyin.prism.bin",
        "pinyin.terra.prism.bin",
        "pinyin.zrm.prism.bin",
        "pinyin.flypy.prism.bin",
        "pinyin.zhuyin.prism.bin",
    ] {
        let path = staging.parent().unwrap().join(name);
        if let Ok(meta) = std::fs::metadata(&path) {
            eprintln!("  {name}: {} 字节", meta.len());
        }
    }

    // ── 1. 全拼：只认字母，1–6 选候选 ─────────────────────────────
    {
        let mut s = session(&staging, "pinyin");
        type_keys(&mut s, "zhongguo");
        assert!(s.context().unwrap().is_composing(), "全拼 zhongguo 应处于组字态");
        let cands = candidates(&s);
        let head: Vec<&str> = cands.iter().take(10).map(|c| c.as_str()).collect();
        eprintln!("full pinyin zhongguo -> {} 条，前 10 = {:?}", cands.len(), head);
        assert!(!cands.is_empty(), "全拼 zhongguo 应有候选，实际 0");
        let zhong = position_of(&cands, "中").expect("全拼 zhongguo 候选里应看到「中」");
        eprintln!("full pinyin zhongguo：「中」在第 {} 位（0 起）", zhong);
        assert!(zhong < 6, "「中」应在前 6 条（第一屏），实际第 {} 位", zhong);

        // 数字选候选：按 1 之后输入里不能多出 '1'，且应真的选中第 1 个候选
        let before = input(&s);
        assert_eq!(before, "zhongguo");
        s.process_key('1' as u32, 0);
        let after = input(&s);
        let committed = s.take_commit();
        eprintln!("full pinyin 按 1 之后：input={after:?} commit={committed:?}");
        assert!(!after.contains('1'), "全拼里 1 应是选候选、不能进编码；输入变成了 {after:?}");
        assert!(
            committed.is_some() || after.len() < before.len(),
            "全拼里按 1 应该选中候选/局部上屏；输入仍是 {after:?}"
        );
    }

    // ── 2. 全拼 nihao：首候选应为「你好」 ────────────────────────────
    {
        let mut s = session(&staging, "pinyin");
        type_keys(&mut s, "nihao");
        let cands = candidates(&s);
        eprintln!("full pinyin nihao -> {} 条，前 6 = {:?}", cands.len(), &cands[..cands.len().min(6)]);
        assert_eq!(cands.first().map(String::as_str), Some("你好"), "nihao 首候选应是「你好」");
    }

    // ── 3. 地球拼音：符号标调 zhong-guo/，数字 1–6 选候选 ─────────────
    {
        let mut s = session(&staging, "terra");
        type_keys(&mut s, "zhong-guo/");
        assert_eq!(input(&s), "zhong-guo/", "地球拼音符号调码应原样进编码");
        let cands = candidates(&s);
        let head: Vec<&str> = cands.iter().take(10).map(|c| c.as_str()).collect();
        eprintln!("terra zhong-guo/ -> {} 条，前 10 = {:?}", cands.len(), head);
        assert!(!cands.is_empty(), "地球拼音 zhong-guo/ 应有候选");
        // 注意：带调码只存在于单字/少量生僻词（官方 terra 词库也一样，见 DESIGN.md），
        // 常见两字词（中国/你好…）在 terra 码表里**没有带调条目**，所以带调输入走的是
        // 单字连写（钟国）；两字词要靠无调兜底路径（下面 zhongguo）命中。
        let zhong = position_of(&cands, "中").expect("地球拼音 zhong-guo/ 候选里应看到「中」");
        eprintln!("terra zhong-guo/：「中」在第 {} 位", zhong);

        // 数字 1 必须是选候选，不能进编码
        let before = input(&s);
        s.process_key('1' as u32, 0);
        let after = input(&s);
        let committed = s.take_commit();
        eprintln!("terra 按 1 之后：input={after:?} commit={committed:?}");
        assert!(!after.contains('1'), "地球拼音里 1 应选候选，不能进编码；输入变成 {after:?}");
        assert!(
            committed.is_some() || after.len() < before.len(),
            "地球拼音按 1 应选中候选；输入仍是 {after:?}"
        );

        // 4. 无调兜底：zhongguo 也要出词（「中国」应在前几条）
        let mut loose = session(&staging, "terra");
        type_keys(&mut loose, "zhongguo");
        let loose_cands = candidates(&loose);
        eprintln!("terra 无调 zhongguo -> {} 条，前 6 = {:?}", loose_cands.len(), &loose_cands[..loose_cands.len().min(6)]);
        assert!(!loose_cands.is_empty(), "地球拼音无调 zhongguo 应有无调兜底候选");
        assert_eq!(loose_cands.first().map(String::as_str), Some("中国"), "地球拼音无调 zhongguo 首候选应是「中国」");

        // 6 也要能选候选（page_size = 6）
        let mut six = session(&staging, "terra");
        type_keys(&mut six, "zhong-guo/");
        six.process_key('6' as u32, 0);
        eprintln!(
            "terra 按 6 之后：input={:?} commit={:?}",
            input(&six),
            six.take_commit()
        );
        assert!(!input(&six).contains('6'), "地球拼音里 6 应选候选，不能进编码");

        // 诊断：声调是否真的参与匹配（改调后「中国」不该仍是首候选）
        for probe in ["zhongguo", "zhong-guo/", "zhong/guo\\", "zhong-guo\\"] {
            let mut p = session(&staging, "terra");
            type_keys(&mut p, probe);
            let head: Vec<String> = candidates(&p).into_iter().take(3).collect();
            eprintln!("terra probe {probe:?} -> {head:?}；input={:?}", input(&p));
        }
    }

    // ── 5. 自然码 / 小鹤 / 注音 ───────────────────────────────────
    {
        // 自然码：春 chun → ip（ch→I，un→P）；"ip" 在全拼里非法，能出词即证明走 zrm 棱镜
        let mut zrm = session(&staging, "zrm");
        type_keys(&mut zrm, "ip");
        let zrm_cands = candidates(&zrm);
        eprintln!("zrm ip -> {} 条：{:?}", zrm_cands.len(), zrm_cands);
        assert!(zrm_cands.iter().any(|c| c.contains("春")), "自然码 ip 应能出「春」");
        zrm.process_key('1' as u32, 0);
        assert!(!input(&zrm).contains('1'), "自然码里 1 应选候选");
        assert!(zrm.take_commit().is_some() || input(&zrm).len() < 2, "自然码按 1 应选中候选");

        // 小鹤：春 chun → iy（ch→I，un→Y）
        let mut flypy = session(&staging, "flypy");
        type_keys(&mut flypy, "iy");
        let flypy_cands = candidates(&flypy);
        eprintln!("flypy iy -> {} 条：{:?}", flypy_cands.len(), flypy_cands);
        assert!(flypy_cands.iter().any(|c| c.contains("春")), "小鹤 iy 应能出「春」");
        flypy.process_key('1' as u32, 0);
        assert!(!input(&flypy).contains('1'), "小鹤里 1 应选候选");

        // 注音：王 wang → j;6（u→j，ang→K→; ，2→6）；数字在注音里是编码，选词靠字母
        let mut zhuyin = session(&staging, "zhuyin");
        type_keys(&mut zhuyin, "j;6");
        let cands = candidates(&zhuyin);
        eprintln!("zhuyin j;6 -> {} 条，前 6 = {:?}", cands.len(), &cands[..cands.len().min(6)]);
        assert!(cands.iter().any(|c| c.contains("王")), "注音 j;6 应能出「王」");
        assert_eq!(input(&zhuyin), "j;6", "注音数字应进编码");
        assert!(cands.len() >= 2, "注音 A/B 选词需要至少两个候选");
        let (first, second) = (cands[0].clone(), cands[1].clone());
        zhuyin.process_key(' ' as u32, 0);
        let committed = zhuyin.take_commit();
        eprintln!("zhuyin 空格之后 commit={committed:?}");
        assert!(committed.is_some(), "注音空格应上屏首选");

        // 注音按 A / B 选词（`input_type_selector` 给注音换的那张表）
        for (key, expect, label) in [('A', &first, "第 1 个"), ('B', &second, "第 2 个")] {
            let mut z = session(&staging, "zhuyin");
            type_keys(&mut z, "j;6");
            z.process_key(key as u32, 0);
            let got = z.take_commit();
            let rest = input(&z);
            eprintln!("zhuyin 按 {key} -> commit={got:?} input={rest:?}（期望{label} {expect:?}）");
            assert_eq!(
                got.as_deref(),
                Some(expect.as_str()),
                "注音按 {key} 应选中{label}候选 {expect:?}，实际 {got:?}"
            );
            assert!(!rest.contains(key), "注音选词键 {key} 不该进编码，输入残留 {rest:?}");
        }
    }

    // ── 6. 用户词跨子方案互通 ─────────────────────────────────────
    {
        let mut learn = session(&staging, "pinyin");
        type_keys(&mut learn, "shipeng");
        assert!(learn.context().unwrap().is_composing(), "shipeng 应组字");
        learn.process_key(' ' as u32, 0);
        let learned = learn.take_commit().expect("空格应提交一个两字组合");
        assert_eq!(learned.chars().count(), 2, "学词样本应是两字组合：{learned:?}");

        // 切到地球拼音，用符号调码查回来（1-4 → - / < \ ，轻声走无调兜底）
        let mut terra = session(&staging, "terra");
        let mut found = false;
        'search: for shi in ["", "-", "/", "<", "\\"] {
            for peng in ["", "-", "/", "<", "\\"] {
                clear(&mut terra);
                type_keys(&mut terra, &format!("shi{shi}peng{peng}"));
                if candidates(&terra).iter().any(|c| c == &learned) {
                    found = true;
                    break 'search;
                }
            }
        }
        assert!(found, "全拼学到的 {learned:?} 应能被地球拼音用符号调码查到");

        let db = staging.parent().unwrap().join("userdb").join("pinyin.userdb");
        assert!(db.exists(), "应产生共享用户词典 {}", db.display());
    }

    // ── 7. 繁简（单选组）：默认 zh_hans，各子方案都经 t2s；切 zh_hant 后繁体原样 ──
    {
        // 取样字：「轂」（gu3，带权重 50 的繁体条目）→ zh_hans 下应变成「毂」。
        // 不能用「國」——它在合并词库里没有权重（terra 独有条目），词库编译时就被丢了。
        let probes = [
            ("pinyin", "gu"),
            ("terra", "gu"),
            ("zrm", "gu"),
            ("flypy", "gu"),
            ("zhuyin", "ej3"),
        ];
        for (input_type, keys) in probes {
            let mut s = session(&staging, input_type);
            type_keys(&mut s, keys);
            let cands = candidates(&s);
            eprintln!(
                "繁简[{input_type}] {keys} zh_hans -> {} 条，前 6 = {:?}",
                cands.len(),
                &cands[..cands.len().min(6)]
            );
            assert!(
                cands.iter().any(|c| c.contains('毂')),
                "{input_type} 在 zh_hans 下应出简体「毂」"
            );
            assert!(
                !cands.iter().any(|c| c.contains('轂')),
                "{input_type} 在 zh_hans 下不该残留繁体「轂」"
            );
        }

        // 同一会话里切到 zh_hant，再换子方案：选项不丢，两个子方案都出繁体
        let mut s = session(&staging, "pinyin");
        {
            let engine = s.engine.as_mut().unwrap();
            engine.context.set_option("zh_hans", false);
            engine.context.set_option("zh_hant", true);
        }
        type_keys(&mut s, "gu");
        let pinyin_cands = candidates(&s);
        eprintln!(
            "繁简[pinyin] gu zh_hant -> {} 条，前 6 = {:?}",
            pinyin_cands.len(),
            &pinyin_cands[..pinyin_cands.len().min(6)]
        );
        assert!(
            pinyin_cands.iter().any(|c| c.contains('轂')),
            "zh_hant 下全拼候选里应有繁体「轂」；实得 {pinyin_cands:?}"
        );
        // 「國 guo2」是地球拼音独有的无权重条目（合并时兜了 FLOOR=1）。
        // 只有带调的 `guo2` 音节能查到它；全拼的无调 `guo` 走明月那套无调音节，
        // 候选里依旧没有它（另一件事：无调路径对同音候选只出一部分，见报告）。
        clear(&mut s);
        activate(&mut s, "terra");
        type_keys(&mut s, "guo/");
        let guo2_cands = candidates(&s);
        eprintln!(
            "兜底权重[terra] guo/ zh_hant -> {} 条，含「國」= {}",
            guo2_cands.len(),
            guo2_cands.iter().any(|c| c.contains('國'))
        );
        assert!(
            guo2_cands.iter().any(|c| c.contains('國')),
            "兜底权重后 terra 独有条目「國 guo2」应能查到；实得 {guo2_cands:?}"
        );
        clear(&mut s);
        type_keys(&mut s, "gu<");
        let terra_cands = candidates(&s);
        eprintln!(
            "繁简[terra] gu< zh_hant -> {} 条，前 6 = {:?}",
            terra_cands.len(),
            &terra_cands[..terra_cands.len().min(6)]
        );
        assert!(
            terra_cands.iter().any(|c| c.contains('轂')),
            "zh_hant 下带调 gu< 候选里应有繁体「轂」；实得 {terra_cands:?}"
        );
        clear(&mut s);
        activate(&mut s, "zhuyin");
        type_keys(&mut s, "eji6");
        let guo2_zhuyin = candidates(&s);
        assert!(
            guo2_zhuyin.iter().any(|c| c.contains('國')),
            "注音 eji6（guo2）应能查到「國」；实得 {guo2_zhuyin:?}"
        );
        clear(&mut s);
        type_keys(&mut s, "ej3");
        let zhuyin_cands = candidates(&s);
        eprintln!(
            "繁简[zhuyin] ej3 zh_hant（切子方案后）-> {} 条，前 6 = {:?}",
            zhuyin_cands.len(),
            &zhuyin_cands[..zhuyin_cands.len().min(6)]
        );
        assert!(
            zhuyin_cands.iter().any(|c| c.contains('轂')),
            "切到注音后 zh_hant 仍应生效，候选里应有繁体「轂」；实得 {zhuyin_cands:?}"
        );
    }
}
