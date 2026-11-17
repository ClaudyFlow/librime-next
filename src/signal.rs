//! librime-next 的**统一信号层**（C++ `Messenger` + `Service::Notify` 的 Rust 版）。
//!
//! 原版 librime 的信号图：
//! ```text
//! Context::set_option ─ option_update_notifier ─▶ Engine::OnOptionUpdate ─┐
//! Context::set_property ─ property_update_notifier ─▶ OnPropertyUpdate ───┤
//! Engine::OnSchemaUpdate ────────────────────────────────────────────────┤
//! Deployer::Run ─ ("deploy", start/success/failure) ─────────────────────┤
//!                                                                         ▼
//!                                              Service::Notify(session_id, type, value)
//!                                                                         │
//!                                              RimeNotificationHandler（C 回调）
//! ```
//!
//! Rust 版原先只剩一条 `c_api::fire_schema_notification`（单个 C 函数指针、只发
//! `schema`），部署通知退化成 `Deployer::messages` 向量里的字符串记录。本模块把
//! **四类官方对外通知全部信号化**：
//! * [`Signal::SchemaChanged`] —— 方案切换（后台重建线程）
//! * [`Signal::OptionChanged`] —— 开关变化（`ascii_mode` / 简繁 / 输入类型…）
//! * [`Signal::PropertyChanged`] —— 会话属性变化
//! * [`Signal::Deploy`] —— 部署开始 / 成功 / 失败（`session_id = 0`）
//!
//! # 分层
//!
//! **机制全部在 [`crate::util::Bus`]**（copy-on-write 快照、Direct/Queued 两种连接、
//! 计数短路、锁外分发——对齐 Qt 信号槽，逐条说明见 `util.rs` 模块文档）。本模块只做
//! 两件事：定义领域类型（[`Signal`] / [`Notification`] / [`DeployPhase`]），并把
//! `Bus<Notification>` 组装成进程级全局单例（等价于 C++ 的全局 `Messenger`）。
//! 对外 API（`connect` / `connect_queued` / `disconnect` / `emit_for` / `drain`）
//! 与抽取前完全一致，调用方零改动。
//!
//! 设计约束：
//! * **发射方不碰宿主状态**：只 `emit`，由订阅者决定做什么；
//! * **多订阅者**：C ABI 的 `RimeNotificationHandler` 只是总线上一个槽，
//!   纯 Rust 宿主/测试可以同时 `connect` 自己的槽；
//! * **C ABI 不变**：槽把 [`Notification`] 编码成官方 `(message_type, message_value)`；
//! * 引擎内部同步逻辑（组件条件刷新、重译）仍走原来的直接调用，信号只负责
//!   「对外发生了什么」，不替代返回值语义。

use std::sync::OnceLock;
use std::sync::mpsc::SyncSender;

use crate::util::{self, Bus};

/// 会话标识（`c_api::RimeSessionId` 的底层宽度）。`0` = 不绑定会话的全局通知（部署）。
pub type SessionId = usize;

/// 部署阶段（C++ `deployer.cc` 的三次 `message_sink_`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeployPhase {
    Start,
    Success,
    Failure,
}

/// 一条对外信号。结构化字段供 Rust 订阅者直接用；`message_*` 只给 C ABI 兼容层。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    /// 当前方案已换入（后台重建完成后）。
    SchemaChanged { schema_id: String },
    /// 开关状态变化。
    OptionChanged { name: String, on: bool },
    /// 会话属性变化（官方消息值格式 `name=value`）。
    PropertyChanged { name: String, value: String },
    /// 部署阶段变化。
    Deploy(DeployPhase),
}

/// 带会话归属的信号（＝ C++ `Service::Notify` 的三个参数）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub session_id: SessionId,
    pub signal: Signal,
}

impl Notification {
    /// 官方 `message_type`：`schema` / `option` / `property` / `deploy`。
    pub fn message_type(&self) -> &'static str {
        match self.signal {
            Signal::SchemaChanged { .. } => "schema",
            Signal::OptionChanged { .. } => "option",
            Signal::PropertyChanged { .. } => "property",
            Signal::Deploy(_) => "deploy",
        }
    }

    /// 官方 `message_value`。
    ///
    /// `schema` 一项保留 librime-next 既有格式 `schema_id/<id>`（官方 C++ 是
    /// `<id>/<name>`）：宿主回调只判 `message_type`，改格式没有收益、徒增兼容风险。
    pub fn message_value(&self) -> String {
        match &self.signal {
            Signal::SchemaChanged { schema_id } => format!("schema_id/{schema_id}"),
            Signal::OptionChanged { name, on } => {
                if *on {
                    name.clone()
                } else {
                    format!("!{name}")
                }
            }
            Signal::PropertyChanged { name, value } => format!("{name}={value}"),
            Signal::Deploy(phase) => match phase {
                DeployPhase::Start => "start",
                DeployPhase::Success => "success",
                DeployPhase::Failure => "failure",
            }
            .to_string(),
        }
    }
}

/// 槽函数类型（`Notification` 版的便捷别名，机制见 [`crate::util::Handler`]）。
pub type Handler = util::Handler<Notification>;

/// 接收线程从自己的队列取出 `Notification` 并分派（对应 Qt 事件循环消费
/// `QMetaCallEvent`）。返回取出的条数。
///
/// 调用方决定如何分派（按 `note.message_type()` / `note.signal` 路由到具体处理），
/// 这与宿主现在在 C 槽里把 `note` 翻译成 `Work` 投递到引擎工作队列是同一件事。
pub use util::drain;

/// 进程级全局总线：`Bus<Notification>` 的惰性单例（`Bus::new` 不是 const fn，
/// 不能写在 `static` 初值里）。
static BUS: OnceLock<Bus<Notification>> = OnceLock::new();
#[inline]
fn bus() -> &'static Bus<Notification> {
    BUS.get_or_init(Bus::new)
}

/// 订阅一个**直接**槽（`Qt::DirectConnection`），返回可退订的句柄号。
pub fn connect(handler: Handler) -> u64 {
    bus().connect(handler)
}

/// 订阅一个**原子投递**槽（`Qt::QueuedConnection`）：整条 `Notification` 原子
/// `try_send` 到 `queue`，接收线程用 [`drain`] 分派；队列满则丢弃该条。
pub fn connect_queued(queue: SyncSender<Notification>) -> u64 {
    bus().connect_queued(queue)
}

/// 退订；返回是否真的摘掉了一个槽。
pub fn disconnect(id: u64) -> bool {
    bus().disconnect(id)
}

/// 当前槽数量（诊断 / 测试用；发射方用它做热路径短路）。
pub fn subscriber_count() -> usize {
    bus().subscriber_count()
}

/// 发一条带会话归属的信号。**所有事件源最终都走这里。**
pub fn emit_for(session_id: SessionId, signal: Signal) {
    bus().emit(Notification { session_id, signal });
}

/// 部署通知：官方 `Service` 把 deployer 接到 `Notify(0, ...)`，没有会话号。
pub fn emit_deploy(phase: DeployPhase) {
    emit_for(0, Signal::Deploy(phase));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex as TestMutex};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    /// 总线是进程级全局单例（等价于 C++ 的全局 `Messenger`）。并行测试会互相
    /// connect/disconnect 污染计数与槽，故所有信号测试用同一把锁串行执行。
    fn signal_test_guard() -> std::sync::MutexGuard<'static, ()> {
        static G: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        G.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 总线是进程级单例、测试并行跑：每个用例领一个**唯一** sentinel 名，
    /// 槽只收自己的信号，互不干扰。
    fn unique_tag() -> String {
        static SEQ: AtomicU64 = AtomicU64::new(1);
        format!("__signal_test_{}__", SEQ.fetch_add(1, Ordering::Relaxed))
    }

    fn subscribe(tag: String) -> (u64, Arc<TestMutex<Vec<Notification>>>) {
        let got = Arc::new(TestMutex::new(Vec::new()));
        let sink = Arc::clone(&got);
        let id = connect(Arc::new(move |note| {
            if matches!(
                &note.signal,
                Signal::OptionChanged { name, .. } if name == &tag
            ) {
                sink.lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .push(note.clone());
            }
        }));
        (id, got)
    }

    fn sentinel(tag: &str, on: bool) -> Signal {
        Signal::OptionChanged {
            name: tag.to_string(),
            on,
        }
    }

    #[test]
    fn emit_reaches_every_subscriber_and_disconnect_stops_delivery() {
        let _guard = signal_test_guard();
        let tag = unique_tag();
        let (a, got_a) = subscribe(tag.clone());
        let (b, got_b) = subscribe(tag.clone());
        // 输入：会话 7 上开一个开关
        emit_for(7, sentinel(&tag, true));
        // 输出：两个槽都收到 (session_id=7, OptionChanged{..., on:true})
        let expect = Notification {
            session_id: 7,
            signal: sentinel(&tag, true),
        };
        assert_eq!(
            *got_a.lock().unwrap_or_else(|err| err.into_inner()),
            vec![expect.clone()]
        );
        assert_eq!(
            *got_b.lock().unwrap_or_else(|err| err.into_inner()),
            vec![expect.clone()]
        );

        // 输入：退订 b，再发一条 off
        assert!(disconnect(b), "退订应命中已注册的槽");
        assert!(!disconnect(b), "重复退订应返回 false");
        emit_for(7, sentinel(&tag, false));
        let off = Notification {
            session_id: 7,
            signal: sentinel(&tag, false),
        };
        assert_eq!(
            *got_a.lock().unwrap_or_else(|err| err.into_inner()),
            vec![expect, off],
            "a 应收到 on / off 两条"
        );
        assert_eq!(
            got_b.lock().unwrap_or_else(|err| err.into_inner()).len(),
            1,
            "b 退订后不应再收到"
        );
        disconnect(a);
    }

    #[test]
    fn notification_encodes_official_message_pairs() {
        let _guard = signal_test_guard();
        // 输入 → 输出：(message_type, message_value)，逐条对齐 rime_api.h 的注释
        let cases = [
            (
                Notification {
                    session_id: 3,
                    signal: Signal::SchemaChanged {
                        schema_id: "pinyin".into(),
                    },
                },
                ("schema", "schema_id/pinyin"),
            ),
            (
                Notification {
                    session_id: 3,
                    signal: Signal::OptionChanged {
                        name: "ascii_mode".into(),
                        on: true,
                    },
                },
                ("option", "ascii_mode"),
            ),
            (
                Notification {
                    session_id: 3,
                    signal: Signal::OptionChanged {
                        name: "ascii_mode".into(),
                        on: false,
                    },
                },
                ("option", "!ascii_mode"),
            ),
            (
                Notification {
                    session_id: 3,
                    signal: Signal::PropertyChanged {
                        name: "client_type".into(),
                        value: "tsf".into(),
                    },
                },
                ("property", "client_type=tsf"),
            ),
            (
                Notification {
                    session_id: 0,
                    signal: Signal::Deploy(DeployPhase::Start),
                },
                ("deploy", "start"),
            ),
            (
                Notification {
                    session_id: 0,
                    signal: Signal::Deploy(DeployPhase::Success),
                },
                ("deploy", "success"),
            ),
            (
                Notification {
                    session_id: 0,
                    signal: Signal::Deploy(DeployPhase::Failure),
                },
                ("deploy", "failure"),
            ),
        ];
        for (note, (kind, value)) in cases {
            assert_eq!(
                (note.message_type(), note.message_value().as_str()),
                (kind, value),
                "信号 {note:?} 的 C ABI 编码"
            );
        }
    }

    #[test]
    fn session_id_comes_from_the_emitter_not_a_global() {
        let _guard = signal_test_guard();
        // C++ 里 session_id 由 Service::Session 的 connect 闭包捕获；Rust 版由
        // Context 上绑定的号显式传入，两个会话交错发射也不会串号。
        let tag = unique_tag();
        let (id, got) = subscribe(tag.clone());
        emit_for(42, sentinel(&tag, true));
        emit_for(9, sentinel(&tag, false));
        let notes = got.lock().unwrap_or_else(|err| err.into_inner());
        assert_eq!(notes[0].session_id, 42, "第一条属于会话 42");
        assert_eq!(notes[1].session_id, 9, "第二条属于会话 9");
        disconnect(id);
    }

    #[test]
    fn emit_deploy_is_always_global_session_zero() {
        let _guard = signal_test_guard();
        let sink = Arc::new(TestMutex::new(Vec::new()));
        let out = Arc::clone(&sink);
        let id = connect(Arc::new(move |note| {
            if matches!(note.signal, Signal::Deploy(_)) {
                out.lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .push(note.clone());
            }
        }));
        emit_deploy(DeployPhase::Start);
        emit_deploy(DeployPhase::Success);
        // 全量 `cargo test --lib` 时别的模块也可能跑部署，所以只断言「至少这两条」
        // 且所有部署通知都不带会话号，不写死总数。
        let notes = sink.lock().unwrap_or_else(|err| err.into_inner());
        let deploys: Vec<&Notification> = notes
            .iter()
            .filter(|note| matches!(note.signal, Signal::Deploy(_)))
            .collect();
        assert!(deploys.len() >= 2, "应至少收到 start / success：{deploys:?}");
        assert!(
            deploys.iter().all(|note| note.session_id == 0),
            "部署通知恒为 session_id=0：{deploys:?}"
        );
        assert!(deploys
            .iter()
            .any(|note| note.signal == Signal::Deploy(DeployPhase::Start)));
        assert!(deploys
            .iter()
            .any(|note| note.signal == Signal::Deploy(DeployPhase::Success)));
        disconnect(id);
    }

    #[test]
    fn queued_connection_delivers_whole_notification_across_threads() {
        let _guard = signal_test_guard();
        // 对齐 Qt::QueuedConnection：信号从发射线程原子投递到接收线程队列，
        // 由接收线程的事件循环取出分派，发射方不被阻塞。
        let (tx, rx) = mpsc::sync_channel::<Notification>(16);
        let id = connect_queued(tx);
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50)); // 让发射方先 emit 完
            let mut got = Vec::new();
            while let Ok(note) = rx.try_recv() {
                got.push(note);
            }
            got
        });
        // 发射：会话 11 切到 pinyin（来自后台重建线程的等价场景）
        emit_for(11, Signal::SchemaChanged { schema_id: "pinyin".into() });
        let got = handle.join().unwrap();
        assert_eq!(
            got,
            vec![Notification {
                session_id: 11,
                signal: Signal::SchemaChanged {
                    schema_id: "pinyin".into()
                }
            }],
            "Queued 连接应把整条 Notification 跨线程完整送达"
        );
        disconnect(id);
    }

    #[test]
    fn emit_is_atomic_respects_in_flight_snapshot() {
        let _guard = signal_test_guard();
        // 原子隔离：遍历快照时某槽 connect 了新槽，新槽不应出现在本次分发里
        // （等价于 Qt 在 activate 期间新增连接不影响当前这次发射）。
        let tag = unique_tag();
        let (_early, _got_early) = subscribe(tag.clone());
        // 老槽在 handler 里 connect 一个新槽
        let late_log: Arc<TestMutex<Vec<Notification>>> = Arc::new(TestMutex::new(Vec::new()));
        let late_sink = Arc::clone(&late_log);
        let late_tag = tag.clone();
        let early_tag = tag.clone();
        let early2_log: Arc<TestMutex<Vec<Notification>>> = Arc::new(TestMutex::new(Vec::new()));
        let early2_sink = Arc::clone(&early2_log);
        let id_early = connect(Arc::new(move |note| {
            // 只有命中 early_tag 的才触发"运行时 connect"
            if matches!(
                &note.signal,
                Signal::OptionChanged { name, .. } if name == &early_tag
            ) {
                let late_sink = Arc::clone(&late_sink);
                let late_tag = late_tag.clone();
                connect(Arc::new(move |n| {
                    if matches!(
                        &n.signal,
                        Signal::OptionChanged { name, .. } if name == &late_tag
                    ) {
                        late_sink
                            .lock()
                            .unwrap_or_else(|err| err.into_inner())
                            .push(n.clone());
                    }
                }));
            }
            // early 自己记一条
            early2_sink
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .push(note.clone());
        }));

        emit_for(7, sentinel(&tag, true));

        // early 收到 1 条；late 是 emit 过程中才 connect 的，本次不应收到。
        assert_eq!(
            early2_log.lock().unwrap_or_else(|err| err.into_inner()).len(),
            1,
            "early 槽收到本次信号"
        );
        assert_eq!(
            late_log.lock().unwrap_or_else(|err| err.into_inner()).len(),
            0,
            "emit 中途新增的 late 槽不应出现在本次分发（原子隔离）"
        );
        // 再发一条，late 现在已在快照里，应收到。
        emit_for(7, sentinel(&tag, false));
        assert_eq!(
            late_log.lock().unwrap_or_else(|err| err.into_inner()).len(),
            1,
            "第二次 emit 时 late 已稳定连接，应收到"
        );
        disconnect(id_early);
    }

    #[test]
    fn no_subscribers_short_circuits_without_touching_bus() {
        let _guard = signal_test_guard();
        // 没有任何槽时，emit 不应构造/遍历任何东西（热路径零开销）。
        let before = subscriber_count();
        emit_for(1, Signal::OptionChanged { name: "x".into(), on: true });
        assert_eq!(subscriber_count(), before);
    }
}
