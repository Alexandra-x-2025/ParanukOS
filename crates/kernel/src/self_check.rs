//! M3b 的调度自检（`threads_and_scheduling.md` §10、§15）。
//!
//! 六步全部是断言：互斥、轮转顺序、抢占、tick 推进、退出/回收/记账、持锁纪律。
//! 失败以退出码 45 结束，并带上步骤号、被破坏的不变量与相关数值——与内存自检同一风格。

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::lock::SpinLock;
use crate::{lock, memory, sched, thread};

/// 互斥测试：线程数。
const LOCK_WORKERS: usize = 4;
/// 互斥测试：每线程自增次数。
const LOCK_INCREMENTS: u64 = 2000;
/// 轮转测试：线程数。
const ROUND_ROBIN_WORKERS: usize = 3;
/// 轮转测试：每线程轮数。
const ROUND_ROBIN_ROUNDS: usize = 10;
/// 抢占测试的 tick 预算（§14.4：500 tick ≈ 5 秒）。
const PREEMPTION_TICK_BUDGET: u64 = 500;
/// 等待其它线程完成的自旋预算（调试构建下约几秒；正常路径只需几十个 tick）。
const WAIT_BUDGET: u64 = 100_000_000;

/// 第 1 步：被 SpinLock 保护的计数器。
static COUNTER: SpinLock<u64> = SpinLock::new(0);
/// 第 2 步：轮转记录，10 轮 × 3 个线程。
struct RoundRobinLog(UnsafeCell<[u8; ROUND_ROBIN_ROUNDS * ROUND_ROBIN_WORKERS]>);

// SAFETY: 写入永远在 `ROUND_ROBIN_LOCK` 之下，读取发生在所有测试线程退出之后。
unsafe impl Sync for RoundRobinLog {}

static ROUND_ROBIN: RoundRobinLog = RoundRobinLog(UnsafeCell::new(
    [0xFF; ROUND_ROBIN_ROUNDS * ROUND_ROBIN_WORKERS],
));
/// 第 2 步：写入下标。
static ROUND_ROBIN_INDEX: AtomicUsize = AtomicUsize::new(0);
/// 第 2 步：与 `ROUND_ROBIN` 同锁保护。
static ROUND_ROBIN_LOCK: SpinLock<()> = SpinLock::new(());
/// 第 3 步：抢占标志与观察值。
static PREEMPTED: AtomicBool = AtomicBool::new(false);
static PREEMPT_TICKS_START: AtomicU64 = AtomicU64::new(0);
static PREEMPT_TICKS_END: AtomicU64 = AtomicU64::new(0);
/// 第 3 步：结果（0 = 未完成，1 = 成功，2 = 预算耗尽）。
static PREEMPT_RESULT: AtomicUsize = AtomicUsize::new(0);
/// 已完成的测试线程数。
static FINISHED: AtomicUsize = AtomicUsize::new(0);

/// 自检失败。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Failure {
    /// 步骤号（对应文档 §10）。
    pub step: u8,
    /// 被破坏的不变量。
    pub what: &'static str,
    /// 相关数值（不适用时为 0）。
    pub value: u64,
}

impl core::fmt::Display for Failure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "调度自检第 {} 步失败：{}（相关值 {:#X}）",
            self.step, self.what, self.value
        )
    }
}

const fn failure(step: u8, what: &'static str, value: u64) -> Failure {
    Failure { step, what, value }
}

/// 运行全部六步。`budget` 是每一步等待子线程完成的预算（自旋次数）。
pub fn run() -> Result<(), Failure> {
    let frames_before = memory::frames_free();

    // 抢占是其它每一步的前提：没有抢占，测试线程根本不会被调度到，后面的等待只会超时。
    // 因此执行顺序把第 3 步放在最前面（步骤编号只是标签，见文档 §10）——这样
    // `inject-no-preempt` 触发的就是"抢占失败"这个具体结论，而不是一个笼统的超时。
    step3_and_4_preemption()?;
    step1_mutual_exclusion()?;
    step2_round_robin()?;

    // 第 5 步：所有测试线程退出并被回收之后，只剩引导上下文与空闲线程。
    if !sched::wait_until(|| sched::stats().live == 2, WAIT_BUDGET) {
        return Err(failure(
            5,
            "仍在等待线程退出/回收",
            sched::stats().live as u64,
        ));
    }
    if sched::stats().ready != 0 {
        return Err(failure(5, "就绪队列应为空", sched::stats().ready as u64));
    }
    // 引导上下文应当仍在运行，空闲线程应当是"就绪但不入队"的状态。
    if sched::state(0) != Ok(kernel_sched::ThreadState::Running) {
        return Err(failure(5, "引导上下文不在运行状态", 0));
    }
    let idle = sched::idle_thread();
    if sched::state(idle) != Ok(kernel_sched::ThreadState::Ready) {
        return Err(failure(5, "空闲线程不在就绪状态", idle as u64));
    }
    if sched::canary_failures() != 0 {
        return Err(failure(
            5,
            "有线程栈金丝雀被破坏",
            sched::canary_failures() as u64,
        ));
    }
    // `frames_before` 是在空闲线程已经创建之后取的（main 先建空闲线程再跑自检），
    // 因此所有测试线程被回收之后，空闲页帧必须**恰好回到**这个值——多一页少一页都是泄漏或重复释放。
    let expected = frames_before;
    if memory::frames_free() != expected {
        return Err(failure(
            5,
            "页帧数没有回到预期值（线程栈泄漏？）",
            memory::frames_free(),
        ));
    }

    // 第 6 步：持锁记账。
    if lock::held_count() != 0 {
        return Err(failure(6, "自检结束时仍持有锁", lock::held_count() as u64));
    }
    {
        let guard = ROUND_ROBIN_LOCK.lock();
        if lock::held_count() != 1 {
            drop(guard);
            return Err(failure(
                6,
                "取锁后 held_count 应为 1",
                lock::held_count() as u64,
            ));
        }
    }
    if lock::held_count() != 0 {
        return Err(failure(
            6,
            "放锁后 held_count 应回到 0",
            lock::held_count() as u64,
        ));
    }
    Ok(())
}

/// 第 1 步：4 个线程各在锁下自增 2000 次，最终必须恰好 8000。
fn step1_mutual_exclusion() -> Result<(), Failure> {
    *COUNTER.lock() = 0;
    FINISHED.store(0, Ordering::Relaxed);
    for _ in 0..LOCK_WORKERS {
        thread::create(lock_worker).map_err(|_| failure(1, "无法创建互斥测试线程", 0))?;
    }
    if !sched::wait_until(
        || FINISHED.load(Ordering::Relaxed) >= LOCK_WORKERS,
        WAIT_BUDGET,
    ) {
        return Err(failure(
            1,
            "互斥测试线程没有全部完成",
            FINISHED.load(Ordering::Relaxed) as u64,
        ));
    }
    let value = *COUNTER.lock();
    if value != LOCK_WORKERS as u64 * LOCK_INCREMENTS {
        return Err(failure(1, "计数器丢失更新（锁未提供互斥）", value));
    }
    Ok(())
}

/// 第 2 步：3 个线程轮流写入并让出，记录必须是严格的轮转。
fn step2_round_robin() -> Result<(), Failure> {
    // SAFETY: 此刻所有相关线程都已退出，只有引导上下文在跑。
    unsafe { *ROUND_ROBIN.0.get() = [0xFF; ROUND_ROBIN_ROUNDS * ROUND_ROBIN_WORKERS] };
    ROUND_ROBIN_INDEX.store(0, Ordering::Relaxed);
    FINISHED.store(0, Ordering::Relaxed);

    let mut workers = [0usize; ROUND_ROBIN_WORKERS];
    for slot in workers.iter_mut() {
        *slot = thread::create(round_robin_worker)
            .map_err(|_| failure(2, "无法创建轮转测试线程", 0))?;
    }
    if !sched::wait_until(
        || FINISHED.load(Ordering::Relaxed) >= ROUND_ROBIN_WORKERS,
        WAIT_BUDGET,
    ) {
        return Err(failure(
            2,
            "轮转测试线程没有全部完成",
            FINISHED.load(Ordering::Relaxed) as u64,
        ));
    }

    // SAFETY: 测试线程都已退出，且此刻只有引导上下文在跑。
    let recorded = unsafe { *ROUND_ROBIN.0.get() };
    for (index, got) in recorded.iter().enumerate() {
        let expected = workers[index % ROUND_ROBIN_WORKERS] as u8;
        if *got != expected {
            return Err(failure(
                2,
                "轮转顺序不是严格轮流",
                ((index as u64) << 8) | u64::from(*got),
            ));
        }
    }
    Ok(())
}

/// 第 3、4 步：一个从不 yield 的线程必须被时钟抢占，否则另一个线程永远跑不起来。
fn step3_and_4_preemption() -> Result<(), Failure> {
    PREEMPTED.store(false, Ordering::Relaxed);
    PREEMPT_RESULT.store(0, Ordering::Relaxed);
    FINISHED.store(0, Ordering::Relaxed);
    let ticks_before = sched::stats().ticks;

    // 先创建等待者（它会一直自旋），再创建设置者（只有抢占才能让它运行）。
    thread::create(preempt_waiter).map_err(|_| failure(3, "无法创建抢占等待线程", 0))?;
    thread::create(preempt_setter).map_err(|_| failure(3, "无法创建抢占设置线程", 0))?;

    if !sched::wait_until(|| PREEMPT_RESULT.load(Ordering::Relaxed) != 0, WAIT_BUDGET) {
        return Err(failure(
            3,
            "抢占测试没有结束",
            PREEMPT_RESULT.load(Ordering::Relaxed) as u64,
        ));
    }
    if PREEMPT_RESULT.load(Ordering::Relaxed) != 1 {
        return Err(failure(
            3,
            "自旋线程在 tick 预算内没有被抢占（PIT 未生效或抢占被关闭）",
            sched::stats().ticks - ticks_before,
        ));
    }

    // 第 4 步：抢占期间 tick 必须真的推进了。
    let elapsed =
        PREEMPT_TICKS_END.load(Ordering::Relaxed) - PREEMPT_TICKS_START.load(Ordering::Relaxed);
    if elapsed == 0 {
        return Err(failure(4, "抢占发生但 tick 没有推进", elapsed));
    }
    if sched::stats().switches == 0 {
        return Err(failure(4, "没有发生任何上下文切换", 0));
    }
    Ok(())
}

/// 第 1 步的线程：在锁下自增。
extern "C" fn lock_worker(_id: u64) -> ! {
    for _ in 0..LOCK_INCREMENTS {
        let mut counter = COUNTER.lock();
        *counter += 1;
    }
    FINISHED.fetch_add(1, Ordering::Relaxed);
    thread::exit()
}

/// 第 2 步的线程：写入自己的线程号，然后让出。
extern "C" fn round_robin_worker(id: u64) -> ! {
    for _ in 0..ROUND_ROBIN_ROUNDS {
        {
            let _guard = ROUND_ROBIN_LOCK.lock();
            let index = ROUND_ROBIN_INDEX.fetch_add(1, Ordering::Relaxed);
            // SAFETY: 下标在范围内，且写入被 `ROUND_ROBIN_LOCK` 保护。
            unsafe {
                (*ROUND_ROBIN.0.get())[index] = id as u8;
            }
        }
        thread::yield_now();
    }
    FINISHED.fetch_add(1, Ordering::Relaxed);
    thread::exit()
}

/// 第 3 步的线程：**从不**让出，只等一个只有别人能设置的标志。
extern "C" fn preempt_waiter(_id: u64) -> ! {
    let start = sched::ticks();
    PREEMPT_TICKS_START.store(start, Ordering::Relaxed);
    let mut result = 2; // 预算耗尽
    loop {
        if PREEMPTED.load(Ordering::Relaxed) {
            result = 1;
            break;
        }
        if sched::ticks() - start >= PREEMPTION_TICK_BUDGET {
            break;
        }
        core::hint::spin_loop();
    }
    PREEMPT_TICKS_END.store(sched::ticks(), Ordering::Relaxed);
    PREEMPT_RESULT.store(result, Ordering::Relaxed);
    thread::exit()
}

/// 第 3 步的线程：只有被抢占调度到才会运行。
extern "C" fn preempt_setter(_id: u64) -> ! {
    PREEMPTED.store(true, Ordering::Relaxed);
    thread::exit()
}
