//! 中断安全的锁与临界区（M3 的 `threads_and_scheduling.md` §5）。
//!
//! 单核上有两种"互斥"：
//!
//! * [`critical`]：关中断的临界区。**调度器状态只用它保护**——因为调度器也会从时钟 ISR 里被
//!   修改，那里用自旋锁会死锁（单核上无法前进）。
//! * [`SpinLock`]：关中断 + 抢标志。堆与页帧分配器用它；它们在普通线程上下文里被调用。
//!
//! 两条铁律（文档 §5.4）：
//! 1. **先关中断，再抢标志**——反过来会留下"锁已被占用但持有者还没进临界区"的窗口；
//! 2. **任何锁都不跨上下文切换持有**——`yield`/`exit` 用 [`held_count`] 把它变成 panic(41)，
//!    而不是让另一个线程永远自旋。

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// 当前线程持有的锁数量（不含纯 `critical()` 区间）。
static HELD: AtomicUsize = AtomicUsize::new(0);

/// 中断是否已开启。
#[must_use]
pub fn interrupts_enabled() -> bool {
    let flags: u64;
    // SAFETY: 读 RFLAGS 无副作用；`pushfq`/`pop` 成对，栈指针净变化为 0。
    unsafe {
        core::arch::asm!("pushfq", "pop {}", out(reg) flags, options(preserves_flags));
    }
    flags & (1 << 9) != 0
}

/// 关中断（`cli`）。
///
/// # Safety
/// 调用方必须保证之后会在合适的时机恢复中断状态（通常通过 [`CriticalSection`]）。
unsafe fn disable_interrupts() {
    // SAFETY: ring 0 执行 `cli` 合法。
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
    }
}

/// 开中断（`sti`）。
///
/// # Safety
/// 调用方必须保证此刻开启中断是安全的：调度器状态一致、没有持有任何锁。
pub unsafe fn enable_interrupts() {
    // SAFETY: ring 0 执行 `sti` 合法。
    unsafe {
        core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
    }
}

/// 关中断的临界区；析构时恢复进入前的 `IF`。
///
/// 这是调度器的"锁"：单核 + `IF = 0` 就意味着没有时钟能插入。
pub struct CriticalSection {
    restore: bool,
}

impl CriticalSection {
    /// 关中断并记住原来的 `IF`。
    #[must_use]
    pub fn enter() -> Self {
        let restore = interrupts_enabled();
        if restore {
            // SAFETY: 紧随其后用 Drop 恢复。
            unsafe { disable_interrupts() };
        }
        Self { restore }
    }
}

impl Drop for CriticalSection {
    fn drop(&mut self) {
        if self.restore {
            // SAFETY: 进入时中断是开的，说明调用方不在 ISR 里，此刻恢复是安全的。
            unsafe { enable_interrupts() };
        }
    }
}

/// 进入临界区（关中断）。
#[must_use]
pub fn critical() -> CriticalSection {
    CriticalSection::enter()
}

/// 当前线程持有的锁数量。`yield`/`exit` 用它拒绝"持锁切换"。
#[must_use]
pub fn held_count() -> usize {
    HELD.load(Ordering::Relaxed)
}

/// 先关中断、再抢标志的自旋锁。
pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: 互斥由 `locked` 保证；`T: Send` 才能把值的所有权在线程之间传递。
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// 构造。
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// 获取锁：**先关中断**，再自旋抢标志。
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let irq = CriticalSection::enter();
        while self.locked.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        HELD.fetch_add(1, Ordering::Relaxed);
        SpinLockGuard {
            lock: self,
            _irq: irq,
        }
    }
}

/// 锁守卫：析构时释放标志、把 `held_count` 减一，然后恢复中断状态。
pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
    /// 只为析构而存在：它负责在锁释放后恢复中断状态。
    _irq: CriticalSection,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: 持有守卫即持有锁。
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: 持有守卫即独占持有锁。
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        HELD.fetch_sub(1, Ordering::Relaxed);
        self.lock.locked.store(false, Ordering::Release);
        // `irq` 之后自动析构，在那里恢复中断状态。
    }
}
