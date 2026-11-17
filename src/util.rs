//! 可复用的进程内信号总线 —— Qt 信号槽的零依赖 Rust 版（**纯机制，零业务**）。
//!
//! 从 `signal.rs` 抽出的通用层：总线只认事件类型 `T`，不知道 `Signal` /
//! `Notification` 是什么。领域语义（四类信号、会话归属、官方消息编码）留在
//! `signal.rs`，它用 [`Bus<Notification>`] 组装出全局单例。
//!
//! # 能力（逐条对齐 Qt）
//!
//! * **`Connection::Direct`** ↔ `Qt::DirectConnection`：发射线程同步调用槽。
//! * **`Connection::Queued`** ↔ `Qt::QueuedConnection`：把**整条**事件原子
//!   `try_send` 到目标线程的有界队列，由接收线程用 [`drain`] 分派；发射方不被
//!   阻塞、不被中断（要么完整入队，要么丢弃，不存在半条状态）。
//! * **热路径零阻塞**：槽表是一份 `Arc<Vec<Slot<T>>>` 的整体快照，`emit` 只做
//!   一次 `Arc::clone`（O(1)）就离开锁，锁外遍历分发；`connect`/`disconnect`
//!   整体复制并替换快照（copy-on-write，连接变更不频繁所以 O(n) 可接受）。
//! * **发射线程安全**：`emit` 持有的是 clone 出来的旧快照，槽里再
//!   `connect` / `disconnect` 只会换掉新快照，互不死锁。
//! * **计数短路**：无订阅者时 `emit` 直接返回，连快照读都不做。
//!
//! Qt 的 `Auto`/`BlockingQueued` 不在此内置：`Auto` 依赖"对象线程亲和"，Rust 侧
//! 由调用方决定 Direct 还是 Queued；`BlockingQueued` 会阻塞发射方、对热路径有害，
//! 需要同步语义时直接用 Direct。
//!
//! # 用法
//!
//! ```ignore
//! let bus = Bus::<String>::new();
//! let id = bus.connect(Arc::new(|event| println!("direct: {event}")));
//! let (tx, rx) = std::sync::mpsc::sync_channel(16);
//! bus.connect_queued(tx);
//! bus.emit("hello".into());          // Direct 槽当场打印；Queued 槽入队
//! drain(&rx, |event| { ... });       // 接收线程在自己的时机消费队列
//! bus.disconnect(id);
//! ```

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, RwLock};

/// 槽函数：线程安全、可共享。`emit` 在锁外调用，槽里不得做阻塞 I/O。
pub type Handler<T> = Arc<dyn Fn(&T) + Send + Sync + 'static>;

/// 连接类型，对齐 Qt 的 `Qt::ConnectionType`。
///
/// `Queued` 持有 `Arc<SyncSender>`（而非裸 `Sender`）：`Sender` 不可克隆，但
/// `Slot<T>` 需要 `Clone` 才能整体替换快照；`SyncSender` 提供 `try_send`（有界、
/// 非阻塞、满则丢弃），正好对应 Qt 的有界事件队列语义。
#[derive(Clone)]
pub enum Connection<T> {
    /// `Qt::DirectConnection`：发射线程同步调用槽。
    Direct(Handler<T>),
    /// `Qt::QueuedConnection`：整条事件原子投递到 `queue`，由接收线程的
    /// [`drain`] 异步分派；发射方不阻塞、不被中断。
    Queued { queue: Arc<SyncSender<T>> },
}

/// 总线里的一个连接项。需要 `Clone` 才能整体替换快照（见 [`Bus::emit`]）。
#[derive(Clone)]
struct Slot<T> {
    id: u64,
    connection: Connection<T>,
}

/// 一条总线。需要 `T: Clone`（Queued 投递给多个槽时各 clone 一份）。
pub struct Bus<T: Clone + Send + Sync + 'static> {
    /// 槽表的**整体快照**。`emit` 只 `Arc::clone` 读（O(1)、锁外分发）；
    /// `connect`/`disconnect` 复制后整体替换。`Arc::new` 不是 const fn，
    /// 全局单例场景用 `OnceLock` 包一层惰性初始化（见 `signal.rs`）。
    slots: RwLock<Arc<Vec<Slot<T>>>>,
    next_id: AtomicU64,
    subscriber_count: AtomicUsize,
}

impl<T: Clone + Send + Sync + 'static> Default for Bus<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone + Send + Sync + 'static> Bus<T> {
    pub fn new() -> Self {
        Self {
            slots: RwLock::new(Arc::new(Vec::new())),
            next_id: AtomicU64::new(1),
            subscriber_count: AtomicUsize::new(0),
        }
    }

    /// 当前订阅数（发射方用它做热路径短路）。
    pub fn subscriber_count(&self) -> usize {
        self.subscriber_count.load(Ordering::Acquire)
    }

    /// 订阅一个**直接**槽（`Qt::DirectConnection`），返回可退订的句柄号。
    pub fn connect(&self, handler: Handler<T>) -> u64 {
        self.connect_conn(Connection::Direct(handler))
    }

    /// 订阅一个**原子投递**槽（`Qt::QueuedConnection`）。
    ///
    /// 事件不会被同步调用，而是整条 `try_send` 到 `queue`；接收线程用 [`drain`]
    /// 从 `Receiver` 取出并自行分派（等价于 Qt 事件循环消费 `QMetaCallEvent`）。
    /// `queue` 满则丢弃该条（与 Qt 队列满时丢弃行为一致）。
    pub fn connect_queued(&self, queue: SyncSender<T>) -> u64 {
        self.connect_conn(Connection::Queued { queue: Arc::new(queue) })
    }

    /// 退订；返回是否真的摘掉了一个槽。
    pub fn disconnect(&self, id: u64) -> bool {
        let mut guard = self.slots.write().unwrap_or_else(|err| err.into_inner());
        let before = guard.len();
        let mut next = (**guard).clone();
        next.retain(|slot| slot.id != id);
        let removed = next.len() != before;
        if removed {
            *guard = Arc::new(next);
            self.subscriber_count.fetch_sub(1, Ordering::Release);
        }
        removed
    }

    fn connect_conn(&self, connection: Connection<T>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.slots.write().unwrap_or_else(|err| err.into_inner());
        let mut next = (**guard).clone();
        next.push(Slot { id, connection });
        *guard = Arc::new(next);
        self.subscriber_count.fetch_add(1, Ordering::Release);
        id
    }

    /// 发一条事件。
    ///
    /// 热路径：无订阅者直接返回；否则 O(1) clone 出共享快照、离开锁再遍历分发——
    /// Direct 同步调用、Queued 原子投递。遍历期间任何槽 `connect`/`disconnect`
    /// 都只影响**新**快照，本次分发看到的仍是进入 `emit` 那一刻的连接表（原子隔离，
    /// 对齐 Qt `QMetaObject::activate` 读共享连接表的行为）。
    pub fn emit(&self, event: T) {
        if self.subscriber_count() == 0 {
            return;
        }
        let slots = Arc::clone(&*self.slots.read().unwrap_or_else(|err| err.into_inner()));
        for slot in slots.iter() {
            match &slot.connection {
                Connection::Direct(handler) => handler(&event),
                Connection::Queued { queue } => {
                    // 原子投递：整条事件完整入队或丢弃，不可中断。
                    let _: Result<(), TrySendError<T>> = queue.try_send(event.clone());
                }
            }
        }
    }
}

/// 接收线程从自己的队列取出事件并分派（对应 Qt 事件循环消费 `QMetaCallEvent`）。
/// 返回取出的条数。没有后台线程——接收方在**自己的时机**调它。
pub fn drain<T: Clone>(queue: &Receiver<T>, mut dispatch: impl FnMut(&T)) -> usize {
    let mut count = 0;
    while let Ok(event) = queue.try_recv() {
        dispatch(&event);
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    //! 实例级总线没有全局状态，测试可并行，不需要 signal.rs 那样的串行 guard。
    use super::*;
    use std::sync::mpsc;
    use std::sync::Mutex as TestMutex;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Msg {
        topic: String,
        seq: u32,
    }

    fn msg(topic: &str, seq: u32) -> Msg {
        Msg { topic: topic.to_string(), seq }
    }

    #[test]
    fn direct_slot_receives_and_disconnect_stops_delivery() {
        // 输入：Direct 槽收集事件；emit 两条后退订，再 emit 一条。
        // 期望：前两条到达、第三条不到；disconnect 返回 true，重复退订返回 false。
        let bus = Bus::new();
        let got = Arc::new(TestMutex::new(Vec::new()));
        let sink = Arc::clone(&got);
        let id = bus.connect(Arc::new(move |event: &Msg| sink.lock().unwrap().push(event.clone())));

        bus.emit(msg("a", 1));
        bus.emit(msg("a", 2));
        assert_eq!(got.lock().unwrap().len(), 2);
        assert!(bus.disconnect(id));
        assert!(!bus.disconnect(id), "重复退订应返回 false");
        bus.emit(msg("a", 3));
        assert_eq!(got.lock().unwrap().len(), 2, "退订后不应再收到");
    }

    #[test]
    fn queued_slot_delivers_atomically_via_drain() {
        // 输入：Queued 槽（容量 16）+ drain 分派。
        // 期望：emit 不直接调槽，事件完整入队；drain 取出 2 条并逐条分派。
        let bus = Bus::new();
        let (tx, rx) = mpsc::sync_channel(16);
        bus.connect_queued(tx);
        bus.emit(msg("b", 1));
        bus.emit(msg("b", 2));
        let got = Arc::new(TestMutex::new(Vec::new()));
        let sink = Arc::clone(&got);
        let n = drain(&rx, move |event: &Msg| sink.lock().unwrap().push(event.clone()));
        assert_eq!(n, 2, "drain 应取出 2 条");
        assert_eq!(got.lock().unwrap().len(), 2);
        assert_eq!(got.lock().unwrap()[0].seq, 1, "入队顺序保持");
    }

    #[test]
    fn full_queue_drops_instead_of_blocking() {
        // 输入：容量 1 的 Queued 槽，连发 3 条。
        // 期望：不阻塞、不 panic；drain 最多收到 1 条（满则丢，与 Qt 队列满一致）。
        let bus = Bus::new();
        let (tx, rx) = mpsc::sync_channel(1);
        bus.connect_queued(tx);
        bus.emit(msg("c", 1));
        bus.emit(msg("c", 2));
        bus.emit(msg("c", 3));
        let mut received = Vec::new();
        drain(&rx, |event: &Msg| received.push(event.seq));
        assert_eq!(received.len(), 1, "容量 1 只装得下第一条");
        assert_eq!(received[0], 1);
    }

    #[test]
    fn handler_can_connect_and_disconnect_inside_emit() {
        // 输入：一个会 connect 新槽、一个会 disconnect 自己的槽（Bus 用 Arc 共享进闭包）。
        // 期望：不死锁；本次 emit 仍按旧快照分发完，变更只影响下一次。
        let bus = Arc::new(Bus::<Msg>::new());
        let late_id = Arc::new(TestMutex::new(None::<u64>));
        let id_ref = Arc::clone(&late_id);
        let bus_remover = Arc::clone(&bus);
        let remover_id = bus.connect(Arc::new(move |_event: &Msg| {
            if let Some(id) = *id_ref.lock().unwrap() {
                bus_remover.disconnect(id);
            }
        }));
        let bus_adder = Arc::clone(&bus);
        let adder_id = bus.connect(Arc::new(move |_event: &Msg| {
            // 第一次 emit 时挂一个新 Direct 槽；本次它收不到（旧快照）。
            let mut guard = late_id.lock().unwrap();
            if guard.is_none() {
                *guard = Some(bus_adder.connect(Arc::new(|_: &Msg| {})));
            }
        }));
        bus.emit(msg("d", 1));
        bus.disconnect(adder_id);
        bus.disconnect(remover_id);
        assert!(bus.subscriber_count() <= 2);
    }
}
