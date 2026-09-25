//! 内核侧内存初始化：建立并安装内核**自己的**页表。
//!
//! 依据 `docs/architecture/memory_subsystem.md` §3、§4。本模块只做三件事：
//!
//! 1. 断言 CPU 处于预期的分页模式（四级、已开启、未启用 PCIDE）；
//! 2. 把 `BootInfo` 里的内存图交给 `kernel-memory` 构建恒等映射页表；
//! 3. 写入 `CR3`，然后**重新校验 `BootInfo`** —— 它位于物理地址上，只有恒等映射
//!    确实覆盖了交接结构才仍然可读，这是"映射真的生效了"的最小证据。
//!
//! 所有可单测的逻辑都在 `kernel-memory` 里；这里只剩下确实需要 `unsafe` 的部分
//! （内联汇编与裸指针切片）。

use boot_info::BootInfo;
use core::cell::UnsafeCell;
use core::fmt;
use kernel_memory::map::{MapError, MemoryMap, buffer_len};
use kernel_memory::paging::{self, BuildError, PageTables, PagingConfig};

/// 页表竞技场的页数：4 GiB 上限下是 7 页 = 28 KiB（有单元测试钉住）。
const ARENA_PAGES: usize = paging::tables_needed(paging::MAX_IDENTITY_BYTES, paging::BLOCK_SIZE);
/// 页表竞技场的字节数。
const ARENA_BYTES: usize = ARENA_PAGES * paging::PAGE_SIZE as usize;

/// `.bss` 里的静态页表竞技场。
///
/// 放在静态存储里的理由：建立映射时还没有任何分配器可用，而 `.bss` 由引导器负责清零，
/// 且其链接地址就是物理地址（恒等加载），因此表项里可以直接写它自己的地址。
///
/// 4 KiB 对齐是硬要求：页表项里的物理地址必须页对齐，PD/PDPT/PML4 指针也一样。
#[repr(align(4096))]
struct PageArena(UnsafeCell<[u8; ARENA_BYTES]>);

// SAFETY: 单核、中断已关闭，且这块内存只在 `install` 中被独占借用一次
// （与 `idt.rs` 的静态 IDT 同一模式）。启用中断或多核之前必须重新审视。
unsafe impl Sync for PageArena {}

static PAGE_ARENA: PageArena = PageArena(UnsafeCell::new([0u8; ARENA_BYTES]));

/// 内存初始化失败的原因。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemoryError {
    /// CPU 不处于预期的分页模式（四级、已开启、无 PCIDE）。
    UnexpectedCpuMode {
        /// 具体情况。
        detail: CpuModeProblem,
    },
    /// 页表竞技场不在内核镜像区间内（说明内核没有被加载到链接地址）。
    ArenaOutsideKernel {
        /// 竞技场地址。
        arena: u64,
    },
    /// `BootInfo` 里的内存图字段无法换算成合法缓冲区。
    Map(MapError),
    /// 页表构建失败。
    Build(BuildError),
    /// 写入 `CR3` 之后 `BootInfo` 不再可读——恒等映射没有覆盖交接结构。
    BootInfoLostAfterSwitch,
}

/// 具体的 CPU 模式问题。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CpuModeProblem {
    /// 分页未开启（`CR0.PG = 0`）。
    PagingOff,
    /// 未启用物理地址扩展（`CR4.PAE = 0`）。
    PaeOff,
    /// 处于五级分页（`CR4.LA57 = 1`），本实现只支持四级。
    FiveLevelPaging,
    /// 未处于长模式（`EFER.LMA = 0`）。
    NotInLongMode,
    /// 已启用 PCID（`CR4.PCIDE = 1`）：此时 `CR3` 低位携带 PCID，
    /// 直接写入裸表地址会破坏地址。
    PcideEnabled,
}

impl fmt::Display for CpuModeProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PagingOff => write!(f, "分页未开启（CR0.PG = 0）"),
            Self::PaeOff => write!(f, "未启用 PAE（CR4.PAE = 0）"),
            Self::FiveLevelPaging => write!(f, "处于五级分页（CR4.LA57 = 1），本实现只支持四级"),
            Self::NotInLongMode => write!(f, "未处于长模式（EFER.LMA = 0）"),
            Self::PcideEnabled => write!(f, "已启用 PCID（CR4.PCIDE = 1），写入裸 CR3 会破坏地址"),
        }
    }
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedCpuMode { detail } => write!(f, "CPU 分页模式不符合预期：{detail}"),
            Self::ArenaOutsideKernel { arena } => write!(
                f,
                "页表竞技场 0x{arena:X} 不在内核镜像区间内（内核未按链接地址加载？）"
            ),
            Self::Map(err) => write!(f, "内存图不可用：{err}"),
            Self::Build(err) => write!(f, "页表构建失败：{err}"),
            Self::BootInfoLostAfterSwitch => {
                write!(f, "切换 CR3 后 BootInfo 不再可读：恒等映射没有覆盖交接结构")
            }
        }
    }
}

impl From<MapError> for MemoryError {
    fn from(err: MapError) -> Self {
        Self::Map(err)
    }
}

impl From<BuildError> for MemoryError {
    fn from(err: BuildError) -> Self {
        Self::Build(err)
    }
}

/// 建立并安装内核自己的恒等映射页表，返回构建结果供日志与后续里程碑使用。
///
/// # Errors
/// 见 [`MemoryError`]。
///
/// # Safety（调用方契约）
/// 只能调用一次；调用后**不得**再使用任何引导服务，也不得假设固件的页表仍然有效
/// （本函数会替换 `CR3`，且只映射内存图里标记为 RAM 的区域：MMIO 一律不映射）。
pub unsafe fn install(boot_info: &BootInfo) -> Result<PageTables, MemoryError> {
    check_cpu_mode()?;

    // 竞技场必须在镜像区间内：否则内核明显没有被加载到链接地址，继续下去只会写出诡异的映射。
    // SAFETY: 单核、中断已关闭，且本函数只调用一次。
    let arena = unsafe { arena_slice() };
    let arena_address = arena.as_ptr() as u64;
    let kernel_end = boot_info.kernel_base.saturating_add(boot_info.kernel_size);
    if arena_address < boot_info.kernel_base || arena_address >= kernel_end {
        return Err(MemoryError::ArenaOutsideKernel {
            arena: arena_address,
        });
    }

    // SAFETY: 引导器保证 mmap_ptr 指向 BootInfo.mmap_len 条描述符、每条 mmap_desc_size 字节，
    // 且该区间是 LOADER_DATA（见 kernel_interface.md §5.2/§6.1）。这里只读，不写。
    let map = unsafe { boot_info_memory_map(boot_info)? };
    let tables = paging::build(arena, &map, &PagingConfig::M2)?;

    // SAFETY: `pml4_phys` 是本函数刚在竞技场里构建好的顶级页表物理地址；单核、中断已关闭。
    unsafe { write_cr3(tables.pml4_phys) };

    // 切换后重新校验：BootInfo 位于物理地址上，只有恒等映射覆盖它才仍然可读。
    if boot_info.validate().is_err() {
        return Err(MemoryError::BootInfoLostAfterSwitch);
    }

    Ok(tables)
}

/// 借用静态页表竞技场。
///
/// # Safety
/// 返回一个指向静态存储的可变引用：调用方必须保证在借用期间没有其他代码访问该竞技场，
/// 且单核、中断已关闭。
unsafe fn arena_slice() -> &'static mut [u8] {
    // SAFETY: 由调用方契约保证。
    unsafe { &mut *PAGE_ARENA.0.get() }
}

/// 由 `BootInfo` 的四个字段构造内存图视图。
///
/// # Safety
/// 调用方必须保证 `boot_info.mmap_ptr` 指向 `mmap_len × mmap_desc_size` 字节可读的内存。
unsafe fn boot_info_memory_map(boot_info: &BootInfo) -> Result<MemoryMap<'_>, MemoryError> {
    // 先给出"步长过小"这个具体原因：`buffer_len` 对步长过小与长度溢出都返回 None，
    // 直接映射成 TooLarge 会把损坏的 BootInfo 说成"内存图过大"。
    if boot_info.mmap_desc_size < kernel_memory::map::DESCRIPTOR_MIN_SIZE {
        return Err(MapError::StrideTooSmall(boot_info.mmap_desc_size).into());
    }
    let len =
        buffer_len(boot_info.mmap_len, boot_info.mmap_desc_size).ok_or(MapError::TooLarge {
            entries: boot_info.mmap_len,
            stride: boot_info.mmap_desc_size,
        })?;
    // SAFETY: 由调用方契约保证；`len` 已按描述符条数与步长算出，不越界。
    let bytes = unsafe { core::slice::from_raw_parts(boot_info.mmap_ptr as *const u8, len) };
    Ok(MemoryMap::from_bytes(
        bytes,
        boot_info.mmap_len,
        boot_info.mmap_desc_size,
        boot_info.mmap_desc_ver,
    )?)
}

/// 断言 CPU 处于预期的分页模式。
fn check_cpu_mode() -> Result<(), MemoryError> {
    let cr0 = read_cr0();
    let cr4 = read_cr4();
    let efer = read_efer();

    let problem = if cr0 & CR0_PG == 0 {
        Some(CpuModeProblem::PagingOff)
    } else if cr4 & CR4_PAE == 0 {
        Some(CpuModeProblem::PaeOff)
    } else if cr4 & CR4_LA57 != 0 {
        Some(CpuModeProblem::FiveLevelPaging)
    } else if efer & EFER_LMA == 0 {
        Some(CpuModeProblem::NotInLongMode)
    } else if cr4 & CR4_PCIDE != 0 {
        Some(CpuModeProblem::PcideEnabled)
    } else {
        None
    };

    match problem {
        Some(detail) => Err(MemoryError::UnexpectedCpuMode { detail }),
        None => Ok(()),
    }
}

/// `CR0.PG`：分页开启。
const CR0_PG: u64 = 1 << 31;
/// `CR4.PAE`：物理地址扩展（x86-64 长模式的前提）。
const CR4_PAE: u64 = 1 << 5;
/// `CR4.LA57`：五级分页。
const CR4_LA57: u64 = 1 << 12;
/// `CR4.PCIDE`：PCID 支持启用。
const CR4_PCIDE: u64 = 1 << 17;
/// `EFER.LMA`：长模式已激活。
const EFER_LMA: u64 = 1 << 10;
/// `EFER` 的 MSR 编号。
const MSR_EFER: u32 = 0xC000_0080;

/// 读取 `CR0`。
fn read_cr0() -> u64 {
    let value: u64;
    // SAFETY: ring 0 读控制寄存器无副作用，只读不写。
    unsafe {
        core::arch::asm!("mov {}, cr0", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// 读取 `CR4`。
fn read_cr4() -> u64 {
    let value: u64;
    // SAFETY: 同上。
    unsafe {
        core::arch::asm!("mov {}, cr4", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// 读取 `EFER`（MSR `0xC0000080`）。
fn read_efer() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: `rdmsr` 在 ring 0 合法；`0xC0000080` 是 EFER，读取无副作用。
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") MSR_EFER,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags)
        );
    }
    (u64::from(high) << 32) | u64::from(low)
}

/// 写入 `CR3`（同时刷新非全局 TLB 项）。
///
/// # Safety
/// `value` 必须是当前映射下有效、且已构建完成的顶级页表物理地址；写入后旧页表立即失效。
unsafe fn write_cr3(value: u64) {
    // SAFETY: 由调用方契约保证；调用前已确认 CR4.PCIDE = 0，因此 CR3 不含 PCID 字段。
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) value, options(nomem, nostack, preserves_flags));
    }
}
