//! 第一个用户态服务（M4a）：映射镜像与用户栈，创建 CPL 3 线程，处理哨兵系统调用。
//!
//! 依据 `docs/architecture/user_mode.md` §4–§10。M4a 还没有系统调用表：服务唯一的"输出"是
//! 发起一次哨兵调用号（`0xDEAD`），内核据此证明**代码确实运行在 CPL 3 上**——`cs = 0x2B`
//! 是 CPU 给的，不是内核自己写下的结论。

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use boot_info::BootInfo;
use kernel_memory::user_map::{self, Mapping, USER_BASE};
use kernel_sched::Decision;

use crate::idt::IrqFrame;
use crate::{kerror, kinfo, memory, thread};

/// 用户栈页数。
pub const USER_STACK_PAGES: usize = 4;
/// 用户栈的虚拟地址：镜像之上 2 MiB，避免与镜像重叠。
pub const USER_STACK_VA: u64 = USER_BASE + 0x20_0000;
/// 服务线程号（未创建时为 `usize::MAX`）。
static SERVICE: AtomicUsize = AtomicUsize::new(usize::MAX);
/// 是否观察到来自 CPL 3 的系统调用。
static REACHED_CPL3: AtomicBool = AtomicBool::new(false);
/// 观察到系统调用时的 `cs`（应当是 `0x2B`）。
static SYSCALL_CS: AtomicU64 = AtomicU64::new(0);
/// 创建服务之前的 tick 数（用于证明"服务运行期间时钟在推进"）。
static TICKS_BEFORE: AtomicU64 = AtomicU64::new(0);

/// 初始化失败的原因。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InitError {
    /// `BootInfo` 里没有可运行的用户镜像（v0，或字段不全）。
    NoUserImage,
    /// 页帧不够（用户栈）。
    NoStack,
    /// 用户区映射失败。
    Map(user_map::MapError),
    /// 无法创建用户线程。
    Thread(thread::CreateError),
}

impl core::fmt::Display for InitError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoUserImage => write!(f, "BootInfo 里没有可运行的用户镜像（v1 字段不全）"),
            Self::NoStack => write!(f, "无法为用户栈分配 {USER_STACK_PAGES} 个连续页帧"),
            Self::Map(err) => write!(f, "用户区映射失败：{err}"),
            Self::Thread(err) => write!(f, "无法创建用户线程：{err}"),
        }
    }
}

/// 建立用户地址空间并启动服务线程。
///
/// # Errors
/// 见 [`InitError`]。
///
/// # Safety
/// 单核、中断已开启（服务要被时钟抢占才能运行）；只调用一次。
pub unsafe fn init(boot_info: &BootInfo) -> Result<usize, InitError> {
    if !boot_info.has_user_image() {
        return Err(InitError::NoUserImage);
    }
    let stack = memory::alloc_frames(USER_STACK_PAGES).ok_or(InitError::NoStack)?;

    // SAFETY: 单核，且此刻没有别的代码在用竞技场（用户线程还没创建）。
    let (arena, pml4_offset, first_free) = unsafe { memory::page_arena() };
    let mappings = [
        // 镜像：只读（M4a 的程序没有 .data/.bss，只读足够；NX 暂未启用）。
        Mapping {
            vaddr: boot_info.user_vaddr,
            paddr: boot_info.user_phys,
            size: boot_info.user_size,
            writable: false,
        },
        // 用户栈：可写。
        Mapping {
            vaddr: USER_STACK_VA,
            paddr: stack.start,
            size: (USER_STACK_PAGES as u64) * 4096,
            writable: true,
        },
    ];
    let mapped = user_map::map_user_region(arena, pml4_offset, first_free, &mappings)
        .map_err(InitError::Map)?;
    kinfo!(
        "user: 镜像 0x{:X}→0x{:X}（{} 页，U/S=1），栈 0x{:X}，入口 0x{:X}，页表 {} 页",
        boot_info.user_phys,
        boot_info.user_vaddr,
        boot_info.user_size / 4096,
        USER_STACK_VA,
        boot_info.user_entry,
        mapped.tables_used
    );

    TICKS_BEFORE.store(crate::sched::ticks(), Ordering::Relaxed);
    let id = thread::create_user(
        boot_info.user_entry,
        USER_STACK_VA + 4096 * USER_STACK_PAGES as u64,
    )
    .map_err(InitError::Thread)?;
    SERVICE.store(id, Ordering::Relaxed);
    kinfo!("user: 服务线程 = 线程 {id}，等待它运行到 CPL 3");
    Ok(id)
}

/// 服务是否已经真的在 CPL 3 上运行过。
#[must_use]
pub fn reached_cpl3() -> bool {
    REACHED_CPL3.load(Ordering::Relaxed)
}

/// 观察到系统调用时的 `cs`。
#[must_use]
pub fn syscall_cs() -> u64 {
    SYSCALL_CS.load(Ordering::Relaxed)
}

/// 服务运行期间时钟推进了多少个 tick。
#[must_use]
pub fn elapsed_ticks() -> u64 {
    crate::sched::ticks().saturating_sub(TICKS_BEFORE.load(Ordering::Relaxed))
}

/// 系统调用入口（M4a：只认哨兵调用号）。
///
/// 返回与其它中断路径相同的调度决策——M4a 在这里结束服务线程，与 `exit` 系统调用同义。
pub(crate) fn on_syscall(frame: &IrqFrame) -> Decision {
    SYSCALL_CS.store(frame.cs, Ordering::Relaxed);
    REACHED_CPL3.store(true, Ordering::Relaxed);
    kinfo!(
        "user: 服务确实运行在 CPL 3（cs=0x{:X}，调用号 0x{:X}，自旋计数 {}），M4a 到此结束该线程",
        frame.cs,
        frame.rax,
        frame.rdi
    );
    if frame.cs & 3 != 3 {
        kerror!(
            "系统调用竟然来自 CPL {}——0x40 门的 DPL 配置有问题",
            frame.cs & 3
        );
    }
    crate::sched::exit_thread()
}
