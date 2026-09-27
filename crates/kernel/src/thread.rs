//! 内核线程：上下文、栈、创建/让出/退出（`threads_and_scheduling.md` §6、§14）。
//!
//! 线程 = 入口函数 + 自己的 4 页内核栈 + 保存的 `IrqFrame`（`Context.rsp` 永远指向它）。
//! 三种切换（时钟抢占、`yield`、`exit`）都经过 `idt.rs` 的同一个公共入口，因此**只有一种栈
//! 形状**——这是文档 §14.2 修正过的设计：两种形状会让"协作让出的线程被时钟恢复"时弹错寄存器。
//!
//! 所有对线程表的访问都在 `critical()`（`IF = 0`）之下，且只在单核上运行。

use core::arch::asm;
use core::cell::UnsafeCell;
use core::mem::size_of;

use kernel_memory::frame::PhysRange;
use kernel_sched::MAX_THREADS;

use crate::idt::{FxSaveArea, IrqFrame, YIELD_VECTOR};
use crate::sched;
use crate::{gdt, memory};

/// 每个线程的内核栈页数（4 页 = 16 KiB）。
pub const THREAD_STACK_PAGES: usize = 4;
/// 每个线程的内核栈字节数。
pub const THREAD_STACK_SIZE: usize = THREAD_STACK_PAGES * 4096;
/// 栈金丝雀：写在栈的最低 8 字节，回收前检查（没有保护页，见文档 §6.2）。
pub const STACK_CANARY: u64 = 0x5061_7261_6E75_6B02;

/// 线程的机器状态。`rsp` 永远指向该线程保存的 [`IrqFrame`]。
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Context {
    /// 保存的 `IrqFrame` 地址。
    pub rsp: u64,
    /// 线程号（自检与诊断）。
    pub thread: u32,
    /// 对齐填充。
    pub _pad: u32,
}

/// 每个线程的机器侧存储。
struct Slot {
    /// 保存的上下文。
    context: Context,
    /// 内核栈区间；`None` 表示该槽位没有线程。
    stack: Option<PhysRange>,
    /// 金丝雀所在的地址（栈的最低 8 字节）。
    canary_address: u64,
    /// 该线程的 FXSAVE 区（512 字节，16 字节对齐）。
    fx: FxSaveArea,
    /// 入口函数（仅用于诊断）。
    entry: u64,
    /// 是否为用户线程（CPL 3）；决定切换时是否要刷新 `TSS.rsp0`。
    user: bool,
    /// 该线程内核栈的高端地址（用户线程运行时 `TSS.rsp0` 必须指向它）。
    kernel_stack_top: u64,
}

impl Slot {
    const fn new() -> Self {
        Self {
            context: Context {
                rsp: 0,
                thread: 0,
                _pad: 0,
            },
            stack: None,
            canary_address: 0,
            fx: FxSaveArea::zeroed(),
            entry: 0,
            user: false,
            kernel_stack_top: 0,
        }
    }
}

/// 线程表。
struct SlotTable(UnsafeCell<[Slot; MAX_THREADS]>);

// SAFETY: 所有访问都在 `critical()` 之下（见模块文档），且单核。
unsafe impl Sync for SlotTable {}

static SLOTS: SlotTable = SlotTable(UnsafeCell::new([const { Slot::new() }; MAX_THREADS]));

/// 线程入口：收到自己的线程号（由蹦床从构造帧的 `rsi` 传入）。
pub type ThreadEntry = extern "C" fn(u64) -> !;

/// 创建线程失败的原因。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CreateError {
    /// 线程表已满。
    NoFreeSlot,
    /// 页帧分配器给不出 4 页连续内存。
    NoStack,
}

impl core::fmt::Display for CreateError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoFreeSlot => write!(f, "线程表已满"),
            Self::NoStack => write!(f, "无法为线程栈分配 {THREAD_STACK_PAGES} 个连续页帧"),
        }
    }
}

/// 在临界区里访问线程表。
fn with_slots<R>(f: impl FnOnce(&mut [Slot; MAX_THREADS]) -> R) -> R {
    let _critical = crate::lock::critical();
    // SAFETY: 临界区内中断关闭、单核，因此这是独占访问。
    let slots = unsafe { &mut *SLOTS.0.get() };
    f(slots)
}

/// 创建一个线程：分配栈、构造初始 `IrqFrame`、登记到调度器。
///
/// # Errors
/// 见 [`CreateError`]。失败时会把已经分配的资源还回去。
pub fn create(entry: ThreadEntry) -> Result<usize, CreateError> {
    let id = match sched::create_thread() {
        Ok(id) => id,
        Err(_) => return Err(CreateError::NoFreeSlot),
    };
    create_with_id(id, entry)
}

/// 创建一个**用户线程**（M4a）：入口与栈都在用户区，帧里的选择子是 CPL 3。
///
/// 它同样需要一个内核栈：用户态发生中断/系统调用时，CPU 会按 `TSS.rsp0` 切到这个栈上
/// （`user_mode.md` §6/§7）。
///
/// # Errors
/// 见 [`CreateError`]。
pub fn create_user(entry_va: u64, user_stack_top: u64) -> Result<usize, CreateError> {
    let id = match sched::create_thread() {
        Ok(id) => id,
        Err(_) => return Err(CreateError::NoFreeSlot),
    };
    let kernel_stack = match memory::alloc_frames(THREAD_STACK_PAGES) {
        Some(stack) => stack,
        None => return Err(CreateError::NoStack),
    };
    // SAFETY: 刚分配、恒等映射、当前无人使用。
    unsafe {
        prepare_user_frame(id, entry_va, user_stack_top, &kernel_stack);
    }
    Ok(id)
}

/// 构造用户线程的初始帧：`iretq` 会切到 CPL 3 并从帧里取用户 `rsp`。
///
/// # Safety
/// `kernel_stack` 必须是一段已分配、已恒等映射、当前无人使用的连续页帧。
unsafe fn prepare_user_frame(
    id: usize,
    entry_va: u64,
    user_stack_top: u64,
    kernel_stack: &PhysRange,
) {
    let base = kernel_stack.start;
    let top = kernel_stack.end();
    // SAFETY: 由调用方保证可写。
    unsafe { core::ptr::write_volatile(base as *mut u64, STACK_CANARY) };

    let frame_address = top - size_of::<IrqFrame>() as u64;
    // SAFETY: frame_address 在栈内、8 字节对齐，由调用方保证可写。
    unsafe {
        let frame = frame_address as *mut IrqFrame;
        core::ptr::write_bytes(frame as *mut u8, 0, size_of::<IrqFrame>());
        (*frame).rip = entry_va;
        (*frame).rsp = user_stack_top; // 用户栈：`iretq` 会把它装回 rsp
        (*frame).cs = u64::from(gdt::USER_CODE | 3);
        (*frame).ss = u64::from(gdt::USER_DATA | 3);
        // bit 1 恒为 1；IF = 1，于是服务在中断开启下起跑。
        (*frame).rflags = 0x202;
        (*frame).vector = u64::from(YIELD_VECTOR);
    }

    with_slots(|slots| {
        slots[id].context = Context {
            rsp: frame_address,
            thread: id as u32,
            _pad: 0,
        };
        slots[id].stack = Some(*kernel_stack);
        slots[id].canary_address = base;
        slots[id].entry = entry_va;
        slots[id].fx = FxSaveArea::default_state();
        slots[id].user = true;
        slots[id].kernel_stack_top = top;
    });
}

/// 该线程是否为用户线程。
#[must_use]
pub fn is_user(id: usize) -> bool {
    with_slots(|slots| slots[id].user)
}

/// 该线程的内核栈顶（用户线程运行时写入 `TSS.rsp0`）。
#[must_use]
pub fn kernel_stack_top(id: usize) -> u64 {
    with_slots(|slots| slots[id].kernel_stack_top)
}

/// 创建空闲线程：登记为空闲（不入队），分配栈并构造初始帧。
///
/// # Errors
/// 见 [`CreateError`]。
pub fn create_idle(entry: ThreadEntry) -> Result<usize, CreateError> {
    let id = match sched::register_idle_thread() {
        Ok(id) => id,
        Err(_) => return Err(CreateError::NoFreeSlot),
    };
    create_with_id(id, entry)
}

/// 为一个已经登记好的线程号分配栈并构造初始帧。
fn create_with_id(id: usize, entry: ThreadEntry) -> Result<usize, CreateError> {
    let stack = match memory::alloc_frames(THREAD_STACK_PAGES) {
        Some(stack) => stack,
        None => {
            return Err(CreateError::NoStack);
        }
    };
    debug_assert!(id < MAX_THREADS);

    // SAFETY: 栈页刚由页帧分配器发出、恒等映射，且此刻还没有别的上下文在用它。
    unsafe {
        prepare_stack(id, entry, &stack);
    }
    Ok(id)
}

/// 在 `stack` 上写好金丝雀与初始帧，并登记到线程表。
///
/// # Safety
/// `stack` 必须是一段已分配、已恒等映射、且当前无人使用的连续页帧。
unsafe fn prepare_stack(id: usize, entry: ThreadEntry, stack: &PhysRange) {
    debug_assert_eq!(
        stack.len as usize, THREAD_STACK_SIZE,
        "线程栈大小必须符合约定"
    );
    let base = stack.start;
    let top = stack.end();

    // 金丝雀：栈的最低 8 字节。
    // SAFETY: 由调用方保证该页可写。
    unsafe { core::ptr::write_volatile(base as *mut u64, STACK_CANARY) };

    // 初始 IrqFrame 放在栈顶之下，紧挨着栈顶（下面还有 8 字节给 iretq 恢复的 rsp）。
    let frame_address = top - size_of::<IrqFrame>() as u64;
    debug_assert!(frame_address > base + 8, "栈太小，装不下初始帧");

    // SAFETY: frame_address 在 [base, top) 内、8 字节对齐（top 是页对齐，IrqFrame 是 8 的倍数），
    // 由调用方保证该内存可写。
    unsafe {
        let frame = frame_address as *mut IrqFrame;
        core::ptr::write_bytes(frame as *mut u8, 0, size_of::<IrqFrame>());
        (*frame).rdi = entry as usize as u64; // 蹦床的第一个参数
        (*frame).rsi = id as u64; // 第二个参数：线程号
        (*frame).vector = u64::from(YIELD_VECTOR);
        (*frame).rip = thread_trampoline as *const () as u64;
        (*frame).cs = u64::from(gdt::KERNEL_CODE);
        (*frame).ss = u64::from(gdt::KERNEL_DATA);
        // bit 1 恒为 1；IF = 1，于是 iretq 之后线程在中断开启下起跑（文档 §14.5）。
        (*frame).rflags = 0x202;
        // 蹦床入口要满足 rsp % 16 == 8，与真的被 call 过一致。
        (*frame).rsp = top - 8;
    }

    with_slots(|slots| {
        slots[id].context = Context {
            rsp: frame_address,
            thread: id as u32,
            _pad: 0,
        };
        slots[id].stack = Some(*stack);
        slots[id].canary_address = base;
        slots[id].entry = entry as usize as u64;
        slots[id].fx = FxSaveArea::default_state();
        slots[id].user = false;
        // 内核线程的"用户栈"就是它自己的内核栈；`rsp0` 只在用户线程上有意义。
        slots[id].kernel_stack_top = top;
    });
}

/// 线程蹦床：由 `iretq` 进入，参数来自构造帧的 `rdi`/`rsi`。
///
/// 入口函数若返回，就等同于退出。
extern "C" fn thread_trampoline(entry: ThreadEntry, id: u64) -> ! {
    // 入口函数的类型就是 `-> !`：线程想结束必须调用 `exit()`，返回是不允许的。
    entry(id)
}

/// 主动让出 CPU。
///
/// 持锁时让出会在单核上死锁（另一个线程永远等一把只有你能放的锁），因此这里直接 panic(41)。
pub fn yield_now() {
    if crate::lock::held_count() != 0 {
        panic!(
            "持锁让出 CPU 会死锁：held_count = {}",
            crate::lock::held_count()
        );
    }
    // SAFETY: 向量 0x30 已安装为中断门；该指令进入 irq_handler，恢复时正常返回。
    unsafe {
        asm!("int 0x30", options(nomem, nostack, preserves_flags));
    }
}

/// 退出当前线程。永不返回。
pub fn exit() -> ! {
    if crate::lock::held_count() != 0 {
        panic!("持锁退出会死锁：held_count = {}", crate::lock::held_count());
    }
    // SAFETY: 向量 0x31 已安装；处理器把当前线程标为 Exited 并切换走，不会返回。
    unsafe {
        asm!("int 0x31", options(nomem, nostack, preserves_flags));
    }
    unreachable!("exit 之后不应回到线程");
}

/// 取得某个线程的上下文与 FXSAVE 区指针（切换时用）。
///
/// # Safety
/// 必须在 `critical()` 之下调用；返回的指针只在下一次切换之前有效。
pub unsafe fn context_and_fx(id: usize) -> (*mut Context, *mut FxSaveArea) {
    debug_assert!(id < MAX_THREADS);
    let slots = unsafe { &mut *SLOTS.0.get() };
    (&mut slots[id].context, &mut slots[id].fx)
}

/// 金丝雀是否完好。
#[must_use]
pub fn canary_ok(id: usize) -> bool {
    with_slots(|slots| {
        let address = slots[id].canary_address;
        if address == 0 {
            return true;
        }
        // SAFETY: 该地址仍是本线程栈的最低 8 字节，且此刻线程已退出（不在使用该栈）。
        unsafe { core::ptr::read_volatile(address as *const u64) == STACK_CANARY }
    })
}

/// 释放某个已退出线程的栈并清空槽位。
pub fn release(id: usize) {
    let stack = with_slots(|slots| slots[id].stack.take());
    if let Some(stack) = stack {
        let _ = memory::free_frames(&stack);
    }
    with_slots(|slots| {
        slots[id].canary_address = 0;
        slots[id].entry = 0;
        slots[id].user = false;
        slots[id].kernel_stack_top = 0;
        slots[id].context.rsp = 0;
    });
}

/// 空闲线程的入口：开中断下 `hlt`，等待下一次时钟。
pub extern "C" fn idle_main(_id: u64) -> ! {
    loop {
        // SAFETY: `hlt` 在 ring 0 合法；中断开启时它会等待下一次中断后继续。
        unsafe {
            asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}
