//! 「librime 全部信号化」端到端回归：事件源 → 总线 → C ABI 编码。
//!
//! 具体输入 / 输出都写进断言（不是只看「测试通过」）：
//! * `Context::set_option` / `set_property` 要带会话号、按官方格式编码；
//! * `Deployer::run` 要发 `deploy start` / `deploy success`，且 `messages()` 仍保留旧记录。
//!
//! 本仓测试目录是单数 `test/`，新文件必须在 `Cargo.toml` 里登记 `[[test]]`。

use std::sync::{Arc, Mutex, OnceLock};

use librime_next::engine::Context;
use librime_next::lever::deployer::Deployer;
use librime_next::signal::{self, DeployPhase, Notification, Signal};

type Log = Arc<Mutex<Vec<Notification>>>;

/// 总线是进程级全局单例（等价于 C++ 的全局 `Messenger`）。本文件里的集成测试会互相
/// connect/disconnect 污染计数与槽，用同一把锁串行执行，避免与 lib 单测一样的竞态。
fn signal_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static G: OnceLock<Mutex<()>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn subscribe(filter: impl Fn(&Notification) -> bool + Send + Sync + 'static) -> (u64, Log) {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    let id = signal::connect(Arc::new(move |note| {
        if filter(note) {
            sink.lock()
                .unwrap_or_else(|err| err.into_inner())
                .push(note.clone());
        }
    }));
    (id, log)
}

#[test]
fn context_emits_option_and_property_with_its_session_id() {
    let _guard = signal_test_guard();
    let tag = format!("signal_it_{}", std::process::id());
    let wanted = tag.clone();
    let (id, log) = subscribe(move |note| match &note.signal {
        Signal::OptionChanged { name, .. } | Signal::PropertyChanged { name, .. } => {
            name.starts_with(&wanted)
        }
        _ => false,
    });

    // 输入：会话 7 的 Context 开一次、关一次 ascii_mode，再设一个属性
    let mut ctx = Context::default();
    ctx.signal_session_id = 7;
    ctx.set_option(&format!("{tag}.ascii_mode"), true);
    ctx.set_option(&format!("{tag}.ascii_mode"), false);
    ctx.set_property(&format!("{tag}.client"), "tsf");

    // 输出：三条信号，顺序 / 会话号 / 开关值 / 属性值都要对上
    let option = format!("{tag}.ascii_mode");
    let property = format!("{tag}.client");
    let got = log
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone();
    assert_eq!(
        got,
        vec![
            Notification {
                session_id: 7,
                signal: Signal::OptionChanged {
                    name: option.clone(),
                    on: true,
                },
            },
            Notification {
                session_id: 7,
                signal: Signal::OptionChanged {
                    name: option.clone(),
                    on: false,
                },
            },
            Notification {
                session_id: 7,
                signal: Signal::PropertyChanged {
                    name: property.clone(),
                    value: "tsf".into(),
                },
            },
        ],
        "Context 的三条信号：{got:?}"
    );

    // 宿主真正收到的是字符串：开 = `name`，关 = `!name`，属性 = `name=value`
    let pairs: Vec<(&str, String)> = got
        .iter()
        .map(|note| (note.message_type(), note.message_value()))
        .collect();
    assert_eq!(
        pairs,
        vec![
            ("option", option.clone()),
            ("option", format!("!{option}")),
            ("property", format!("{property}=tsf")),
        ]
    );
    signal::disconnect(id);
}

#[test]
fn deployer_emits_start_and_success_and_keeps_messages() {
    let _guard = signal_test_guard();
    let (id, log) = subscribe(|note| matches!(note.signal, Signal::Deploy(_)));
    // 沙箱根放在 target/ 下，台账/耗时账本都不会写进真实 %APPDATA%/Rime-Next。
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("signal_test_sandbox");
    let mut deployer = Deployer::new_in(&root);
    deployer.set_records_root(Some(root.as_path()));

    // 输入：空任务队列跑一次部署
    let ok = deployer.run();

    // 输出：start / success 两条全局信号（session_id = 0）
    let got = log
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone();
    assert!(ok, "空任务队列应返回成功");
    assert_eq!(
        got,
        vec![
            Notification {
                session_id: 0,
                signal: Signal::Deploy(DeployPhase::Start),
            },
            Notification {
                session_id: 0,
                signal: Signal::Deploy(DeployPhase::Success),
            },
        ],
        "部署两阶段信号：{got:?}"
    );
    assert_eq!(
        deployer.messages(),
        vec![
            ("deploy".to_string(), "start".to_string()),
            ("deploy".to_string(), "success".to_string()),
        ]
        .as_slice(),
        "旧的 messages() 事件记录仍保留，离线对照测试不受影响"
    );
    signal::disconnect(id);
}

#[test]
fn queued_connection_delivers_context_signals_end_to_end() {
    let _guard = signal_test_guard();
    use std::sync::mpsc;
    // 接收方队列（等价于 Qt 接收线程的事件循环）。有界通道对应 Qt 的有界事件队列。
    let (tx, rx) = mpsc::sync_channel(16);
    let id = signal::connect_queued(tx);

    // 发射方：会话 7 的 Context 触发一次开关 + 一次属性变化。
    let mut ctx = Context::default();
    ctx.signal_session_id = 7;
    ctx.set_option("ascii_mode", true);
    ctx.set_property("client", "tsf");

    // 接收线程侧：把队列里的信号 drain 出来（原子投递 → 完整到达）。
    let mut log: Vec<Notification> = Vec::new();
    signal::drain(&rx, |note| log.push(note.clone()));

    // 输出：option + property 两条，且都带会话号 7。
    assert_eq!(log.len(), 2, "Queued 应收到 option + property 两条");
    assert!(
        matches!(log[0].signal, Signal::OptionChanged { .. }),
        "第一条是 option"
    );
    assert!(
        matches!(log[1].signal, Signal::PropertyChanged { .. }),
        "第二条是 property"
    );
    assert!(
        log.iter().all(|note| note.session_id == 7),
        "两条都带会话号 7"
    );
    signal::disconnect(id);
}

