//! ParanukOS 调度器的纯逻辑。
//!
//! 依据 `docs/architecture/threads_and_scheduling.md` §8：FIFO 就绪队列上的严格轮转、独立的
//! 空闲线程、tick 记账、退出线程的回收候选。
//!
//! 这里**没有**任何机器相关的东西：没有汇编、没有端口 I/O、没有静态可变状态。机器侧
//! （`Context`、`switch`、`fxsave`、IDT）留在 `crates/kernel`，与 `kernel-memory` 的拆分方式
//! 一致——因此本 crate 可以 `#![forbid(unsafe_code)]` 并在宿主平台被完整单测。
//!
//! 不变量（有测试钉住）：
//! * 就绪队列里恰好是「除空闲线程之外的所有 `Ready` 线程」，且互不重复；
//! * `Running` 线程**不在**队列里；`Running` 至多一个；
//! * 空闲线程永远是 `Ready` 或 `Running`，但**永不入队**、永不被回收。

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

/// 线程表容量。
pub const MAX_THREADS: usize = 16;

/// 空槽 / 无对象的哨兵值。
pub const NO_THREAD: usize = usize::MAX;

/// 线程状态。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreadState {
    /// 空槽，可以复用。
    Unused,
    /// 已就绪，等待 CPU（空闲线程不入队，但同样处于 `Ready`）。
    Ready,
    /// 正在运行（至多一个）。
    Running,
    /// 已退出，等待调度器回收其栈。
    Exited,
}

/// 线程种类。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreadKind {
    /// 普通内核线程。
    Normal,
    /// 空闲线程：只在没有别的就绪线程时运行，永不入队、永不回收。
    Idle,
    /// 引导上下文：`kernel_main` 自身所在的那个上下文，栈来自引导器，永不释放。
    Boot,
}

/// 调度器错误。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SchedError {
    /// 线程表已满。
    NoFreeSlot,
    /// 非法的线程号。
    InvalidId(usize),
    /// 没有登记过引导上下文（`register_boot` 必须先调用）。
    NoCurrentThread,
    /// 空闲线程已经登记过。
    IdleAlreadyRegistered,
    /// 退出时既没有别的就绪线程、也没有空闲线程可运行。
    NothingToRun,
}

impl core::fmt::Display for SchedError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoFreeSlot => write!(f, "线程表已满（{MAX_THREADS} 项）"),
            Self::InvalidId(id) => write!(f, "非法线程号 {id}"),
            Self::NoCurrentThread => write!(f, "尚未登记引导上下文"),
            Self::IdleAlreadyRegistered => write!(f, "空闲线程已经登记过"),
            Self::NothingToRun => write!(f, "没有任何可运行的线程（也没有空闲线程）"),
        }
    }
}

/// 一次调度决策。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    /// 保持当前线程继续运行。
    Keep,
    /// 切换到 `to`：调度器状态已经更新，机器侧现在就该换栈。
    Switch {
        /// 被换出的线程（它的状态已经变成 `Ready` 或 `Exited`）。
        from: usize,
        /// 将要运行的线程（它的状态已经是 `Running`）。
        to: usize,
    },
}

/// 统计信息。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SchedStats {
    /// 累计 tick 数。
    pub ticks: u64,
    /// 累计上下文切换次数。
    pub switches: u64,
    /// 当前就绪队列长度（不含空闲线程）。
    pub ready: usize,
    /// 非空槽数量。
    pub live: usize,
}

/// 调度器状态机。
pub struct Scheduler {
    states: [ThreadState; MAX_THREADS],
    kinds: [ThreadKind; MAX_THREADS],
    /// 就绪队列（环形缓冲）。
    queue: [usize; MAX_THREADS],
    /// 队首在 `queue` 中的下标。
    head: usize,
    /// 队列长度。
    len: usize,
    /// 当前运行线程；[`NO_THREAD`] 表示尚未登记。
    current: usize,
    /// 空闲线程；[`NO_THREAD`] 表示尚未登记。
    idle: usize,
    ticks: u64,
    switches: u64,
}

impl Scheduler {
    /// 全新的调度器：线程表全空，没有当前线程。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            states: [ThreadState::Unused; MAX_THREADS],
            kinds: [ThreadKind::Normal; MAX_THREADS],
            queue: [NO_THREAD; MAX_THREADS],
            head: 0,
            len: 0,
            current: NO_THREAD,
            idle: NO_THREAD,
            ticks: 0,
            switches: 0,
        }
    }

    /// 登记引导上下文（线程 0）：它一开始就在运行，且**不入队**。
    ///
    /// # Errors
    /// 已经登记过引导上下文时返回 [`SchedError::NoCurrentThread`] 之外……实际上只在重复登记时
    /// 返回 [`SchedError::InvalidId`]。
    pub fn register_boot(&mut self, id: usize) -> Result<(), SchedError> {
        if id >= MAX_THREADS {
            return Err(SchedError::InvalidId(id));
        }
        self.states[id] = ThreadState::Running;
        self.kinds[id] = ThreadKind::Boot;
        self.current = id;
        Ok(())
    }

    /// 登记空闲线程：状态为 `Ready`，但**不入队**，只有队列为空时才会被选中。
    ///
    /// # Errors
    /// 重复登记或线程号非法时返回错误。
    pub fn register_idle(&mut self, id: usize) -> Result<(), SchedError> {
        if id >= MAX_THREADS {
            return Err(SchedError::InvalidId(id));
        }
        if self.idle != NO_THREAD {
            return Err(SchedError::IdleAlreadyRegistered);
        }
        if self.states[id] != ThreadState::Unused {
            return Err(SchedError::InvalidId(id));
        }
        self.states[id] = ThreadState::Ready;
        self.kinds[id] = ThreadKind::Idle;
        self.idle = id;
        Ok(())
    }

    /// 创建一个普通线程：占用最小的空槽，状态 `Ready` 并入队（队尾）。
    ///
    /// # Errors
    /// 线程表已满时返回 [`SchedError::NoFreeSlot`]。
    pub fn create(&mut self) -> Result<usize, SchedError> {
        let id = self
            .states
            .iter()
            .position(|state| *state == ThreadState::Unused)
            .ok_or(SchedError::NoFreeSlot)?;
        self.states[id] = ThreadState::Ready;
        self.kinds[id] = ThreadKind::Normal;
        self.enqueue(id);
        Ok(id)
    }

    /// 当前线程号（[`NO_THREAD`] 表示尚未登记）。
    #[must_use]
    pub const fn current(&self) -> usize {
        self.current
    }

    /// 空闲线程号（[`NO_THREAD`] 表示尚未登记）。
    #[must_use]
    pub const fn idle(&self) -> usize {
        self.idle
    }

    /// 线程状态。
    ///
    /// # Errors
    /// 线程号非法时返回 [`SchedError::InvalidId`]。
    pub fn state(&self, id: usize) -> Result<ThreadState, SchedError> {
        self.states
            .get(id)
            .copied()
            .ok_or(SchedError::InvalidId(id))
    }

    /// 线程种类。
    ///
    /// # Errors
    /// 线程号非法时返回 [`SchedError::InvalidId`]。
    pub fn kind(&self, id: usize) -> Result<ThreadKind, SchedError> {
        self.kinds.get(id).copied().ok_or(SchedError::InvalidId(id))
    }

    /// 统计信息。
    #[must_use]
    pub fn stats(&self) -> SchedStats {
        SchedStats {
            ticks: self.ticks,
            switches: self.switches,
            ready: self.len,
            live: self
                .states
                .iter()
                .filter(|state| **state != ThreadState::Unused)
                .count(),
        }
    }

    /// 就绪队列内容（仅供测试与自检观察顺序）。
    pub fn ready_queue(&self) -> impl Iterator<Item = usize> + '_ {
        let head = self.head;
        let len = self.len;
        (0..len).map(move |offset| self.queue[(head + offset) % MAX_THREADS])
    }

    /// 一次时钟中断。
    ///
    /// `reschedule` 为 `false` 时只计 tick（用于把"时钟在走"与"时钟会抢占"分开测试）。
    pub fn on_tick(&mut self, reschedule: bool) -> Decision {
        self.ticks += 1;
        if !reschedule {
            return Decision::Keep;
        }
        self.rotate()
    }

    /// 当前线程主动让出 CPU。
    pub fn yield_current(&mut self) -> Decision {
        self.rotate()
    }

    /// 当前线程退出：标记 `Exited`，并选出下一个要运行的线程。
    ///
    /// # Errors
    /// 没有登记当前线程、或退出后无任何可运行线程时返回错误。
    pub fn exit_current(&mut self) -> Result<Decision, SchedError> {
        let from = self.current;
        if from == NO_THREAD {
            return Err(SchedError::NoCurrentThread);
        }
        self.states[from] = ThreadState::Exited;
        let to = self.pick_after_stop()?;
        self.current = to;
        self.switches += 1;
        Ok(Decision::Switch { from, to })
    }

    /// 取出一个可回收的已退出线程（状态变为 `Unused`，槽位可复用）。
    ///
    /// 内核拿到号之后负责释放它的栈。当前运行线程不可能是 `Exited`，因此不会被取出。
    pub fn take_reapable(&mut self) -> Option<usize> {
        let id = self
            .states
            .iter()
            .position(|state| *state == ThreadState::Exited)?;
        self.states[id] = ThreadState::Unused;
        self.kinds[id] = ThreadKind::Normal;
        Some(id)
    }

    /// 轮转：当前线程回队尾（除非它是空闲线程），队首成为新的运行线程。
    ///
    /// 队列为空时保持当前线程——把 CPU 让给空闲线程没有任何好处（空闲线程只是 `hlt`，
    /// 而当前线程还有活干）。
    fn rotate(&mut self) -> Decision {
        let from = self.current;
        if from == NO_THREAD {
            return Decision::Keep;
        }
        let is_idle = self.kinds[from] == ThreadKind::Idle;
        if self.len == 0 {
            // 没有别的就绪线程：空闲线程继续 `hlt`，普通线程继续跑。
            return Decision::Keep;
        }
        // 空闲线程同样要离开 Running（只是不入队）：否则会出现两个 Running 线程。
        self.states[from] = ThreadState::Ready;
        if !is_idle {
            self.enqueue(from);
        }
        let to = self.dequeue();
        self.states[to] = ThreadState::Running;
        self.current = to;
        self.switches += 1;
        Decision::Switch { from, to }
    }

    /// 当前线程停止（退出）后选出下一个：优先就绪队列，其次空闲线程。
    fn pick_after_stop(&mut self) -> Result<usize, SchedError> {
        if self.len > 0 {
            let to = self.dequeue();
            self.states[to] = ThreadState::Running;
            return Ok(to);
        }
        if self.idle != NO_THREAD {
            self.states[self.idle] = ThreadState::Running;
            return Ok(self.idle);
        }
        Err(SchedError::NothingToRun)
    }

    /// 入队（队尾）。
    fn enqueue(&mut self, id: usize) {
        debug_assert!(self.len < MAX_THREADS, "就绪队列不应溢出");
        self.queue[(self.head + self.len) % MAX_THREADS] = id;
        self.len += 1;
    }

    /// 出队（队首）。
    fn dequeue(&mut self) -> usize {
        debug_assert!(self.len > 0, "空队列不应出队");
        let id = self.queue[self.head];
        self.queue[self.head] = NO_THREAD;
        self.head = (self.head + 1) % MAX_THREADS;
        self.len -= 1;
        id
    }

    /// 队列中是否包含 `id`（测试用）。
    #[cfg(test)]
    fn queued(&self, id: usize) -> bool {
        self.ready_queue().any(|entry| entry == id)
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 登记引导上下文 + 空闲线程，并创建 `count` 个普通线程，返回它们的号。
    fn bootstrapped(count: usize) -> (Scheduler, Vec<usize>) {
        let mut sched = Scheduler::new();
        sched.register_boot(0).expect("登记引导上下文");
        sched.register_idle(1).expect("登记空闲线程");
        let mut threads = Vec::new();
        for _ in 0..count {
            threads.push(sched.create().expect("创建线程"));
        }
        (sched, threads)
    }

    /// 校验两个不变量：队列里恰好是"除空闲线程外的 Ready 线程"、Running 至多一个。
    fn assert_invariants(sched: &Scheduler) {
        let mut running = 0;
        for id in 0..MAX_THREADS {
            let state = sched.state(id).expect("线程号合法");
            let kind = sched.kind(id).expect("线程号合法");
            match state {
                ThreadState::Running => {
                    running += 1;
                    assert!(!sched.queued(id), "运行中的线程不应在就绪队列里");
                }
                ThreadState::Ready => {
                    if kind == ThreadKind::Idle {
                        assert!(!sched.queued(id), "空闲线程永不入队");
                    } else {
                        assert!(sched.queued(id), "就绪的普通线程必须在队列里");
                    }
                }
                ThreadState::Unused => {
                    assert!(!sched.queued(id), "空槽不应在队列里");
                }
                ThreadState::Exited => {
                    assert!(!sched.queued(id), "已退出线程不应在队列里");
                }
            }
        }
        assert!(running <= 1, "至多一个线程处于 Running");
        assert_eq!(sched.stats().ready, sched.ready_queue().count());
    }

    #[test]
    fn new_scheduler_has_nothing_running() {
        let sched = Scheduler::new();
        assert_eq!(sched.current(), NO_THREAD);
        assert_eq!(sched.idle(), NO_THREAD);
        assert_eq!(sched.stats(), SchedStats::default());
        assert_eq!(sched.ready_queue().count(), 0);
        assert_eq!(sched.state(0), Ok(ThreadState::Unused));
    }

    #[test]
    fn boot_context_runs_and_is_not_queued() {
        let mut sched = Scheduler::new();
        sched.register_boot(0).expect("登记引导上下文");
        assert_eq!(sched.current(), 0);
        assert_eq!(sched.state(0), Ok(ThreadState::Running));
        assert_eq!(sched.kind(0), Ok(ThreadKind::Boot));
        assert_eq!(sched.stats().ready, 0, "引导上下文不入队");
        assert_invariants(&sched);
    }

    #[test]
    fn create_enqueues_in_creation_order() {
        let (sched, threads) = bootstrapped(3);
        assert_eq!(threads, vec![2, 3, 4], "应占用最小的空槽");
        let queue: Vec<_> = sched.ready_queue().collect();
        assert_eq!(queue, vec![2, 3, 4]);
        assert_eq!(sched.stats().ready, 3);
        assert_eq!(sched.stats().live, 5, "引导 + 空闲 + 3 个普通线程");
        assert_invariants(&sched);
    }

    #[test]
    fn idle_is_never_queued_and_never_reused_as_a_normal_slot() {
        let (mut sched, _) = bootstrapped(1);
        assert_eq!(sched.state(1), Ok(ThreadState::Ready));
        assert!(!sched.queued(1));
        assert_invariants(&sched);
        assert_eq!(
            sched.register_idle(5),
            Err(SchedError::IdleAlreadyRegistered)
        );
        assert_eq!(
            sched.register_idle(0),
            Err(SchedError::IdleAlreadyRegistered),
            "已登记空闲线程时优先报该错误"
        );
    }

    #[test]
    fn yielding_rotates_strictly_round_robin() {
        let (mut sched, threads) = bootstrapped(3);
        // 引导上下文让出 CPU：它自己去队尾，队首（线程 2）运行。
        let mut order = Vec::new();
        for _ in 0..9 {
            let current = sched.current();
            order.push(current);
            let decision = sched.yield_current();
            assert!(matches!(decision, Decision::Switch { .. }));
            assert_invariants(&sched);
        }
        assert_eq!(
            order,
            vec![0, 2, 3, 4, 0, 2, 3, 4, 0],
            "必须严格按 0,2,3,4 轮转"
        );
        assert_eq!(sched.current(), 2);
        assert_eq!(sched.stats().switches, 9);
        let _ = threads;
    }

    #[test]
    fn yielding_with_an_empty_queue_keeps_running() {
        let mut sched = Scheduler::new();
        sched.register_boot(0).expect("登记");
        sched.register_idle(1).expect("登记空闲");
        assert_eq!(
            sched.yield_current(),
            Decision::Keep,
            "只有引导上下文时不该切到空闲线程"
        );
        assert_eq!(sched.current(), 0);
        assert_eq!(sched.stats().switches, 0);
        assert_invariants(&sched);
    }

    #[test]
    fn tick_counts_even_without_rescheduling() {
        let mut sched = Scheduler::new();
        sched.register_boot(0).expect("登记");
        for _ in 0..5 {
            assert_eq!(sched.on_tick(false), Decision::Keep);
        }
        assert_eq!(sched.stats().ticks, 5);
        assert_eq!(sched.stats().switches, 0);
    }

    #[test]
    fn tick_preempts_when_another_thread_is_ready() {
        let (mut sched, _) = bootstrapped(2);
        let decision = sched.on_tick(true);
        assert_eq!(decision, Decision::Switch { from: 0, to: 2 });
        assert_eq!(sched.current(), 2);
        assert_eq!(sched.state(0), Ok(ThreadState::Ready));
        assert_eq!(sched.stats().ticks, 1);
        assert_eq!(sched.stats().switches, 1);
        assert_invariants(&sched);
    }

    #[test]
    fn tick_with_an_empty_queue_keeps_running() {
        let mut sched = Scheduler::new();
        sched.register_boot(0).expect("登记");
        sched.register_idle(1).expect("登记空闲");
        assert_eq!(sched.on_tick(true), Decision::Keep);
        assert_eq!(sched.current(), 0);
    }

    #[test]
    fn idle_only_runs_when_nothing_else_is_ready() {
        let mut sched = Scheduler::new();
        sched.register_boot(0).expect("登记");
        sched.register_idle(1).expect("登记空闲");
        let id = sched.create().expect("创建线程");
        assert_eq!(id, 2);

        // 引导上下文退出 → 线程 2 运行（空闲线程不入队，所以不是它）。
        let decision = sched.exit_current().expect("退出成功");
        assert_eq!(decision, Decision::Switch { from: 0, to: 2 });
        assert_eq!(sched.state(0), Ok(ThreadState::Exited));

        // 线程 2 退出 → 队列为空 → 空闲线程运行。
        let decision = sched.exit_current().expect("退出成功");
        assert_eq!(decision, Decision::Switch { from: 2, to: 1 });
        assert_eq!(sched.current(), 1);
        assert_eq!(sched.state(1), Ok(ThreadState::Running));
        assert_invariants(&sched);

        // 空闲线程让出 CPU：队列空 → 保持。
        assert_eq!(sched.yield_current(), Decision::Keep);
        assert_invariants(&sched);
    }

    #[test]
    fn idle_yields_to_a_ready_thread() {
        let mut sched = Scheduler::new();
        sched.register_boot(0).expect("登记");
        sched.register_idle(1).expect("登记空闲");
        // 让引导上下文退出，于是空闲线程在跑。
        let _ = sched.exit_current().expect("退出成功");
        assert_eq!(sched.current(), 1);

        // 现在有人创建了一个线程并让出 CPU：空闲线程必须立刻让位。
        let id = sched.create().expect("创建线程");
        let decision = sched.yield_current();
        assert_eq!(decision, Decision::Switch { from: 1, to: id });
        assert_eq!(sched.current(), id);
        assert_eq!(sched.state(1), Ok(ThreadState::Ready));
        assert_invariants(&sched);
    }

    #[test]
    fn a_dead_thread_is_never_scheduled_again() {
        let (mut sched, _) = bootstrapped(2);
        // 线程 2 退出。
        let _ = sched.yield_current();
        assert_eq!(sched.current(), 2);
        let decision = sched.exit_current().expect("退出成功");
        assert_eq!(decision, Decision::Switch { from: 2, to: 3 });

        // 轮转一圈也不该再看到线程 2。
        for _ in 0..6 {
            let current = sched.current();
            assert_ne!(current, 2, "已退出的线程不得再被调度");
            sched.yield_current();
        }
        assert_invariants(&sched);
    }

    #[test]
    fn reapable_threads_are_returned_once_and_free_their_slot() {
        let (mut sched, _) = bootstrapped(1);
        let _ = sched.yield_current();
        assert_eq!(sched.current(), 2);
        let _ = sched.exit_current().expect("退出成功");
        // 此刻线程 2 已退出但还没被回收。
        assert_eq!(sched.state(2), Ok(ThreadState::Exited));
        assert_eq!(sched.take_reapable(), Some(2));
        assert_eq!(sched.state(2), Ok(ThreadState::Unused));
        assert_eq!(sched.take_reapable(), None, "同一个线程只能被回收一次");
        assert!(sched.take_reapable().is_none());

        // 槽位复用：新线程应当拿到刚空出来的 2 号槽。
        assert_eq!(sched.create(), Ok(2));
    }

    #[test]
    fn the_running_thread_is_never_reapable() {
        let (mut sched, _) = bootstrapped(1);
        assert_eq!(sched.take_reapable(), None, "运行中的线程不可能是待回收的");
    }

    #[test]
    fn exiting_the_last_thread_without_idle_is_an_error() {
        let mut sched = Scheduler::new();
        sched.register_boot(0).expect("登记");
        assert_eq!(sched.exit_current(), Err(SchedError::NothingToRun));
        assert_eq!(sched.state(0), Ok(ThreadState::Exited));
    }

    #[test]
    fn the_thread_table_fills_up() {
        let (mut sched, threads) = bootstrapped(MAX_THREADS - 2);
        assert_eq!(threads.len(), MAX_THREADS - 2);
        assert_eq!(sched.stats().live, MAX_THREADS);
        assert_eq!(sched.create(), Err(SchedError::NoFreeSlot));
        // 回收一个之后又能创建。
        let _ = sched.yield_current();
        let _ = sched.exit_current().expect("退出成功");
        let freed = sched.take_reapable().expect("应有可回收线程");
        assert_eq!(sched.create(), Ok(freed));
    }

    #[test]
    fn invalid_thread_ids_are_rejected() {
        let sched = Scheduler::new();
        assert_eq!(
            sched.state(MAX_THREADS),
            Err(SchedError::InvalidId(MAX_THREADS))
        );
        assert_eq!(sched.kind(999), Err(SchedError::InvalidId(999)));
        let mut sched = sched;
        assert_eq!(
            sched.register_boot(MAX_THREADS),
            Err(SchedError::InvalidId(MAX_THREADS))
        );
    }

    #[test]
    fn long_round_robin_run_stays_fair() {
        let (mut sched, threads) = bootstrapped(3);
        let mut counts = [0usize; MAX_THREADS];
        for _ in 0..30 {
            counts[sched.current()] += 1;
            sched.yield_current();
            assert_invariants(&sched);
        }
        // 4 个参与者（引导 + 3 个线程）在 30 次让出中应当各跑 7 或 8 次。
        for id in [0, 2, 3, 4] {
            assert!(
                (7..=8).contains(&counts[id]),
                "线程 {id} 只跑了 {} 次，轮转不公平",
                counts[id]
            );
        }
        assert_eq!(counts[1], 0, "空闲线程一次都不该跑");
        let _ = threads;
    }
}
