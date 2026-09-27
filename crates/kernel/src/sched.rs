//! 调度器的机器侧粘合（M3a：描述符表 + 时钟 + tick 记账；M3b 在这里换栈）。
//!
//! 依据 `docs/architecture/threads_and_scheduling.md` §5.3、§8：**调度器状态只用 `critical()`
//! 保护**，因为时钟 ISR 也会修改它，而单核上的自旋锁在那里会死锁。纯逻辑（线程表、就绪队列、
//! 轮转、回收）在 `kernel-sched` 里，是宿主单测覆盖的部分。

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};
use kernel_sched::{Decision, SchedError, SchedStats, Scheduler};

use crate::idt::{self, IrqFrame};
use crate::thread;
use crate::{kerror, kinfo, lock, pic};

/// 抢占是否开启。`inject-no-preempt` 只让时钟照常计数、但永不切换（自检第 3 步据此失败）。
const PREEMPTION_ENABLED: bool = !cfg!(feature = "inject-no-preempt");

/// 金丝雀被破坏的次数（回收时检查）。
static CANARY_FAILURES: AtomicUsize = AtomicUsize::new(0);

/// 调度器状态。
struct SchedCell(UnsafeCell<Scheduler>);

// SAFETY: 所有访问都在 `critical()`（中断关闭）或 ISR（中断已关闭）里进行，且单核。
unsafe impl Sync for SchedCell {}

static SCHED: SchedCell = SchedCell(UnsafeCell::new(Scheduler::new()));

/// 在临界区里访问调度器状态。
fn with_sched<R>(f: impl FnOnce(&mut Scheduler) -> R) -> R {
    let _critical = lock::critical();
    // SAFETY: 临界区内中断关闭、单核，因此这是独占访问。
    let sched = unsafe { &mut *SCHED.0.get() };
    f(sched)
}

/// 登记引导上下文（线程 0：`kernel_main` 自身）。
///
/// # Errors
/// 见 [`SchedError`]。
pub fn init_boot() -> Result<(), SchedError> {
    with_sched(|sched| sched.register_boot(0))
}

/// 创建一个普通线程（转发到 `kernel-sched`）。
pub fn create_thread() -> Result<usize, SchedError> {
    with_sched(|sched| sched.create())
}

/// 登记空闲线程（不入队）。
pub fn register_idle_thread() -> Result<usize, SchedError> {
    with_sched(|sched| sched.register_idle_any())
}

/// 某个线程的状态。
pub fn state(id: usize) -> Result<kernel_sched::ThreadState, SchedError> {
    with_sched(|sched| sched.state(id))
}

/// 空闲线程号（[`kernel_sched::NO_THREAD`] 表示尚未登记）。
pub fn idle_thread() -> usize {
    with_sched(|sched| sched.idle())
}

/// 金丝雀被破坏的次数。
#[must_use]
pub fn canary_failures() -> usize {
    CANARY_FAILURES.load(Ordering::Relaxed)
}

/// 有界等待：`predicate` 在预算内变为真时返回 `true`。
///
/// 等待期间中断保持开启（否则其它线程根本没法运行），因此这是"自旋 + 让时钟抢占我们"。
#[must_use]
pub fn wait_until(mut predicate: impl FnMut() -> bool, budget: u64) -> bool {
    let mut spins = 0u64;
    while !predicate() {
        if spins >= budget {
            return false;
        }
        spins += 1;
        core::hint::spin_loop();
    }
    true
}

/// 回收所有已退出的线程（释放它们的栈）。
///
/// 由**被换入**的线程在每次进入中断处理器时调用：退出者已经切换走，它的栈此刻没人用。
fn reap_exited() {
    while let Some(id) = with_sched(|sched| sched.take_reapable()) {
        if !thread::canary_ok(id) {
            CANARY_FAILURES.fetch_add(1, Ordering::Relaxed);
            kerror!("线程 {id} 的栈金丝雀被破坏：栈溢出（M3 不设保护页）");
        }
        thread::release(id);
    }
}

/// 真正换栈：记下被换出的帧，保存/恢复 XMM，返回被换入线程的 `rsp`。
///
/// # Safety
/// 必须在中断门内（`IF = 0`）调用，且 `from`/`to` 是当前有效的线程号。
unsafe fn switch_to(from: usize, to: usize, frame: u64) -> u64 {
    // SAFETY: 由调用方保证；线程表在临界区/中断门内独占访问。
    unsafe {
        let (from_context, from_fx) = thread::context_and_fx(from);
        let (to_context, to_fx) = thread::context_and_fx(to);
        (*from_context).rsp = frame;
        // 只在真正切换时碰 XMM（附录 §14.3）：把 FXSAVE 放在栈上会让帧偏移依赖被中断时的
        // rsp 对齐，因此改为每线程一个区。
        core::arch::asm!("fxsave [{}]", in(reg) from_fx, options(nostack, preserves_flags));
        core::arch::asm!("fxrstor [{}]", in(reg) to_fx, options(nostack, preserves_flags));
        (*to_context).rsp
    }
}

/// 当前 tick 数。
#[must_use]
pub fn ticks() -> u64 {
    with_sched(|sched| sched.stats().ticks)
}

/// 统计信息。
#[must_use]
pub fn stats() -> SchedStats {
    with_sched(|sched| sched.stats())
}

/// 时钟中断处理（M3a 只计 tick，不做切换）。
///
/// # Safety（ABI 契约）
/// 由 `idt.rs` 的 `irq_common` 调用：`frame` 指向已保存的寄存器帧，`fxsave` 指向本线程栈上
/// 16 字节对齐的 FXSAVE 区。返回 `0` 表示回到被中断的上下文；M3b 会返回下一个线程保存的
/// 栈指针，由 `irq_common` 换栈。
/// 处理器内**不得分配内存**（`threads_and_scheduling.md` §4.3）。
pub(crate) extern "C" fn irq_handler(frame: *const IrqFrame) -> u64 {
    // 每次进入中断都先回收已经退出的线程：它们已经切换走，栈可以安全释放。
    reap_exited();

    // SAFETY: 由调用方（irq_common）保证指针有效。
    let vector = (unsafe { (*frame).vector }) as u8;

    let decision = if vector == pic::TIMER_VECTOR {
        let decision = with_sched(|sched| sched.on_tick(PREEMPTION_ENABLED));
        // SAFETY: 端口 I/O；时钟是本处理器负责的中断，必须 EOI（漏掉 EOI 就只剩一次 tick）。
        unsafe { pic::eoi(pic::TIMER_IRQ) };
        decision
    } else if vector == idt::YIELD_VECTOR {
        with_sched(|sched| sched.yield_current())
    } else if vector == idt::EXIT_VECTOR {
        match with_sched(|sched| sched.exit_current()) {
            Ok(decision) => decision,
            Err(err) => {
                kerror!("线程退出失败：{err}");
                Decision::Keep
            }
        }
    } else if vector == pic::SPURIOUS_IRQ7_VECTOR {
        // SAFETY: 端口 I/O；伪中断不能无条件 EOI。
        let spurious = unsafe { pic::handle_spurious_irq7() };
        if spurious {
            kerror!("收到伪 IRQ7（未在服务寄存器里）：未发送 EOI，避免误清真实中断");
        } else {
            kerror!("意外的 IRQ7：已 EOI");
        }
        Decision::Keep
    } else {
        // 其余 IRQ 都被屏蔽；真出现了说明屏蔽字或向量表出了问题。
        let irq = vector.wrapping_sub(pic::PIC1_OFFSET) & 0x0F;
        kerror!("未预期的中断向量 0x{vector:02X}（IRQ{irq}）：已屏蔽并 EOI");
        // SAFETY: 端口 I/O。
        unsafe {
            pic::mask(irq);
            pic::eoi(irq);
        }
        Decision::Keep
    };

    match decision {
        Decision::Keep => 0,
        // SAFETY: 中断门内（IF = 0），from/to 由调度器给出，frame 是本处理器刚保存的帧。
        Decision::Switch { from, to } => unsafe { switch_to(from, to, frame as u64) },
    }
}

/// M3a 的时钟自检：在中断开启下等待至少 `wanted` 次 tick，带有限次数的自旋预算。
///
/// 返回实际观察到的 tick 数；超预算返回 `None`（调用方据此以 45 退出，而不是挂死）。
#[must_use]
pub fn wait_for_ticks(wanted: u64) -> Option<u64> {
    const SPIN_BUDGET: u64 = 200_000_000;
    let mut spins = 0u64;
    loop {
        let ticks = ticks();
        if ticks >= wanted {
            return Some(ticks);
        }
        if spins >= SPIN_BUDGET {
            return None;
        }
        spins += 1;
        core::hint::spin_loop();
    }
}

/// 记录一行 M3 的启动摘要（便于冒烟测试断言）。
pub fn log_ready(ready_threads: usize) {
    kinfo!(
        "sched: {ready_threads} 个线程就绪，PIT {} Hz，GDT/TSS 已装载",
        pic::TIMER_HZ
    );
}
