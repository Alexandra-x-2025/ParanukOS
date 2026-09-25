//! 调度器的机器侧粘合（M3a：描述符表 + 时钟 + tick 记账；M3b 在这里换栈）。
//!
//! 依据 `docs/architecture/threads_and_scheduling.md` §5.3、§8：**调度器状态只用 `critical()`
//! 保护**，因为时钟 ISR 也会修改它，而单核上的自旋锁在那里会死锁。纯逻辑（线程表、就绪队列、
//! 轮转、回收）在 `kernel-sched` 里，是宿主单测覆盖的部分。

use core::cell::UnsafeCell;
use kernel_sched::{SchedError, SchedStats, Scheduler};

use crate::idt::{FxSaveArea, IrqFrame};
use crate::{kerror, kinfo, lock, pic};

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
pub(crate) extern "C" fn irq_handler(frame: *const IrqFrame, _fxsave: *const FxSaveArea) -> u64 {
    // SAFETY: 由调用方（irq_common）保证指针有效。
    let vector = (unsafe { (*frame).vector }) as u8;

    if vector == pic::TIMER_VECTOR {
        // M3a：只记账（reschedule = false）。真正的抢占切换在 M3b 打开。
        with_sched(|sched| {
            sched.on_tick(false);
        });
        // SAFETY: 端口 I/O；时钟是本处理器负责的中断，必须 EOI。
        unsafe { pic::eoi(pic::TIMER_IRQ) };
    } else if vector == pic::SPURIOUS_IRQ7_VECTOR {
        // SAFETY: 端口 I/O；伪中断不能无条件 EOI。
        let spurious = unsafe { pic::handle_spurious_irq7() };
        if spurious {
            kerror!("收到伪 IRQ7（未在服务寄存器里）：未发送 EOI，避免误清真实中断");
        } else {
            kerror!("意外的 IRQ7：已 EOI");
        }
    } else {
        // 其余 IRQ 都被屏蔽；真出现了说明屏蔽字或向量表出了问题。
        let irq = vector.wrapping_sub(pic::PIC1_OFFSET) & 0x0F;
        kerror!("未预期的中断向量 0x{vector:02X}（IRQ{irq}）：已屏蔽并 EOI");
        // SAFETY: 端口 I/O。
        unsafe {
            pic::mask(irq);
            pic::eoi(irq);
        }
    }
    0
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
