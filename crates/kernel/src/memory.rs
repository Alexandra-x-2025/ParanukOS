//! 内核侧内存初始化：自己的页表、物理页帧分配器与内核堆。
//!
//! 依据 `docs/architecture/memory_subsystem.md` §3–§6。本模块负责：
//!
//! 1. 断言 CPU 处于预期的分页模式（四级、已开启、未启用 PCIDE）；
//! 2. 把 `BootInfo` 里的内存图交给 `kernel-memory` 构建恒等映射页表；
//! 3. 写入 `CR3`，然后**重新校验 `BootInfo`** —— 它位于物理地址上，只有恒等映射
//!    确实覆盖了交接结构才仍然可读，这是"映射真的生效了"的最小证据；
//! 4. 用内存图初始化位图页帧分配器（`LOADER_DATA` 与显式保留区间一律视为已占用）；
//! 5. 取一段连续页帧作为内核堆并接入 `#[global_allocator]`；
//! 6. `self_check` 逐条验证上面的东西真的能用（写读回、不重叠、可合并、页帧计数）。
//!
//! 所有可单测的逻辑都在 `kernel-memory` 里；这里只剩下确实需要 `unsafe` 的部分
//! （内联汇编、裸指针切片、静态分配器的内部可变性）。

use boot_info::BootInfo;
use core::cell::UnsafeCell;
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};
use kernel_memory::frame::{
    self, Config as FrameConfig, FrameAllocator, FrameStats, InitError as FrameInitError,
    ReservedRange,
};
use kernel_memory::heap::HEADER_SIZE;
use kernel_memory::map::{MapError, MemoryMap, buffer_len};
use kernel_memory::paging::{self, BuildError, PageTables, PagingConfig};

use alloc::vec::Vec;

use crate::heap;

/// 页表竞技场的页数：4 GiB 上限下是 7 页 = 28 KiB（有单元测试钉住）。
const ARENA_PAGES: usize = paging::tables_needed(paging::MAX_IDENTITY_BYTES, paging::BLOCK_SIZE);
/// 页表竞技场的字节数。
const ARENA_BYTES: usize = ARENA_PAGES * paging::PAGE_SIZE as usize;

/// 页帧位图的字节数：两张位图各占一半（4 GiB 上限下共 256 KiB）。
const FRAME_BITMAP_BYTES: usize =
    frame::bitmap_bytes((paging::MAX_IDENTITY_BYTES - frame::MIN_FREE_ADDR) / paging::PAGE_SIZE);

/// 内核自己拥有、绝不能被页帧分配器发放的物理区间数量上限。
const OWNED_RANGES: usize = frame::MAX_RESERVED_RANGES;

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

/// `.bss` 里的静态页帧位图：建立分配器时还没有别的地方能提供内存。
#[repr(align(8))]
struct FrameBitmap(UnsafeCell<[u8; FRAME_BITMAP_BYTES]>);

// SAFETY: 与 `PageArena` 同样的理由：单核、中断关闭，且只在初始化路径里独占借用。
unsafe impl Sync for FrameBitmap {}

static FRAME_BITMAP: FrameBitmap = FrameBitmap(UnsafeCell::new([0u8; FRAME_BITMAP_BYTES]));

/// 页帧分配器本体。
struct FrameAllocatorCell(UnsafeCell<Option<FrameAllocator<'static>>>);

// SAFETY: 见 `FrameBitmap`；分配器的每次使用都在单核、不可重入的执行流里。
unsafe impl Sync for FrameAllocatorCell {}

static FRAME_ALLOCATOR: FrameAllocatorCell = FrameAllocatorCell(UnsafeCell::new(None));

/// 取内核堆**之前**的空闲页帧数，自检用它核对"堆恰好消耗了多少帧"。
static FREE_FRAMES_AFTER_INIT: AtomicU64 = AtomicU64::new(0);

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
    /// 页帧分配器初始化失败。
    Frames(FrameInitError),
    /// 无法从页帧分配器取得连续的堆区间。
    HeapRegionUnavailable {
        /// 需要的页帧数。
        frames: usize,
    },
    /// 内存自检失败：`step` 是步骤号，`what` 是具体不变量，`address` 是相关地址。
    SelfCheck {
        /// 步骤号（对应文档 §6.4）。
        step: u8,
        /// 失败的不变量。
        what: &'static str,
        /// 相关地址（不适用时为 0）。
        address: u64,
    },
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
            Self::Frames(err) => write!(f, "页帧分配器初始化失败：{err}"),
            Self::HeapRegionUnavailable { frames } => write!(
                f,
                "找不到 {frames} 个连续空闲页帧作为内核堆（{} KiB）",
                frames * paging::PAGE_SIZE as usize / 1024
            ),
            Self::SelfCheck {
                step,
                what,
                address,
            } => write!(f, "内存自检第 {step} 步失败：{what}（地址 0x{address:X}）"),
        }
    }
}

impl From<FrameInitError> for MemoryError {
    fn from(err: FrameInitError) -> Self {
        Self::Frames(err)
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

/// 初始化后的内存布局摘要，供日志与后续里程碑使用。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MemoryLayout {
    /// 恒等映射的上限（页表也照此建立）。
    pub limit: u64,
    /// 页帧分配器的统计。
    pub frames: FrameStats,
    /// 内核堆区间 `(起始物理地址, 长度)`。
    pub heap: (u64, usize),
}

/// 初始化页帧分配器与内核堆（M2b）。必须在 [`install`] 之后调用。
///
/// # Errors
/// 见 [`MemoryError`]。
///
/// # Safety
/// 单核、中断关闭；必须在页表已安装、且本函数只调用一次的前提下使用。
pub unsafe fn init_allocators(
    boot_info: &BootInfo,
    limit: u64,
) -> Result<MemoryLayout, MemoryError> {
    // 1. 页帧分配器：只发放 conventional 内存，并把内核自己拥有的区间显式排除。
    // SAFETY: 内存图由引导器提供且已通过 `install` 阶段的解析；这里只读。
    let map = unsafe { boot_info_memory_map(boot_info)? };
    let owned = owned_ranges(boot_info);
    // SAFETY: 单核、中断关闭；位图是本模块的静态存储，只在这里独占借用一次。
    let bitmap = unsafe { &mut *FRAME_BITMAP.0.get() };
    let allocator = FrameAllocator::init(
        bitmap,
        &map,
        &owned,
        &FrameConfig::new(limit.min(paging::MAX_IDENTITY_BYTES)),
    )?;
    // 记下"取堆之前"的空闲页帧数：自检要用它核对堆恰好消耗了 HEAP_SIZE / PAGE_SIZE 帧。
    FREE_FRAMES_AFTER_INIT.store(allocator.stats().free, Ordering::Relaxed);
    // SAFETY: 同上：独占写入静态槽位一次。
    unsafe { *FRAME_ALLOCATOR.0.get() = Some(allocator) };

    // 2. 内核堆：一段连续页帧，接上全局分配器。
    let heap_frames = heap::HEAP_SIZE / paging::PAGE_SIZE as usize;
    // SAFETY: 单核、中断关闭，且不与上面的借用重叠。
    let allocator = unsafe { frame_allocator() };
    let region =
        allocator
            .alloc_contiguous(heap_frames)
            .ok_or(MemoryError::HeapRegionUnavailable {
                frames: heap_frames,
            })?;
    // SAFETY: 该区间来自页帧分配器且已被恒等映射；只初始化一次。
    unsafe { heap::init(region.start, heap::HEAP_SIZE) };

    Ok(MemoryLayout {
        limit,
        frames: allocator.stats(),
        heap: (region.start, heap::HEAP_SIZE),
    })
}

/// 页帧分配器的自检（文档 §6.4 第 6 步）。
fn frame_self_check(boot_info: &BootInfo) -> Result<(), MemoryError> {
    // SAFETY: 单核、中断关闭，且与初始化路径不重叠。
    let allocator = unsafe { frame_allocator() };

    // 两次分配必须落在不同页帧上。这是唯一能抓住"重复分配"（页帧分配器最危险的 bug）
    // 的检查，也是 `inject-memory-fault` 特意要触发的失败。
    let first = allocator
        .alloc()
        .ok_or(failure(6, "页帧分配器已无空闲页帧", 0))?;
    let second = allocator
        .alloc()
        .ok_or(failure(6, "页帧分配器已无空闲页帧", 0))?;
    if first == second {
        return Err(failure(6, "两次分配返回了同一个页帧", first.address));
    }
    allocator
        .free(first)
        .map_err(|_| failure(6, "释放刚分配的页帧失败", first.address))?;
    allocator
        .free(second)
        .map_err(|_| failure(6, "释放刚分配的页帧失败", second.address))?;

    // 最低地址优先：释放后重新分配应当拿回同一个（最低的）页帧。
    let again = allocator
        .alloc()
        .ok_or(failure(6, "页帧分配器已无空闲页帧", 0))?;
    if again != first {
        return Err(failure(6, "释放后未按最低地址优先重新发放", again.address));
    }
    allocator
        .free(again)
        .map_err(|_| failure(6, "释放刚分配的页帧失败", again.address))?;

    // 重复释放必须被拒绝。
    if allocator.free(again).is_ok() {
        return Err(failure(6, "重复释放没有被拒绝", again.address));
    }

    // 保留区间（分配器自己记录的那一份）里不得有任何空闲页帧。
    for range in allocator.reserved_ranges() {
        if let Some(frame) = allocator.first_free_in(range.start, range.end) {
            return Err(failure(6, "保留区间内出现了空闲页帧", frame.address));
        }
    }
    // 交叉校验：内核按 BootInfo 独立算出的"属于我们的内存"同样不得是空闲页帧。
    for range in owned_ranges(boot_info) {
        if let Some(frame) = allocator.first_free_in(range.start, range.end) {
            return Err(failure(6, "内核自己的内存被算作空闲页帧", frame.address));
        }
    }

    // 堆恰好消耗 HEAP_SIZE / PAGE_SIZE 个页帧。
    let expected = heap::HEAP_SIZE as u64 / paging::PAGE_SIZE;
    let before = FREE_FRAMES_AFTER_INIT.load(Ordering::Relaxed);
    let stats = allocator.stats();
    if stats.free + expected != before {
        return Err(failure(
            6,
            "堆消耗的页帧数与预期不符（free 计数不一致）",
            stats.free,
        ));
    }
    Ok(())
}

/// 内核堆自检（文档 §6.4 第 1–5 步）。
///
/// 关键在第 3 步：**逐字节写读回**。只验算术的自检会漏掉"页帧根本没映射"这类错误。
fn heap_self_check() -> Result<(), MemoryError> {
    let (base, len) = heap::region();
    if len != heap::HEAP_SIZE {
        return Err(failure(1, "堆区间长度不是 1 MiB", len as u64));
    }
    let heap_start = base;
    let heap_end = base + len as u64;

    // 第 1 步：分配 BLOCK_COUNT 个大小各异的块，每个字节写入由块号派生的图案。
    let mut blocks: Vec<Option<Vec<u8>>> = Vec::with_capacity(BLOCK_COUNT);
    for index in 0..BLOCK_COUNT {
        let size = BLOCK_SIZES[index % BLOCK_SIZES.len()];
        let block = alloc::vec![pattern(index); size];
        if block.len() != size {
            return Err(failure(1, "分配到的块长度不符", block.len() as u64));
        }
        blocks.push(Some(block));
    }

    // 第 2 步：两两不相交、都在堆区间内。
    for (index, block) in blocks.iter().enumerate() {
        let Some(block) = block else { continue };
        let start = block.as_ptr() as u64;
        let end = start + block.len() as u64;
        if start < heap_start || end > heap_end {
            return Err(failure(2, "堆块越出堆区间", start));
        }
        for (other_index, other) in blocks.iter().enumerate().skip(index + 1) {
            let Some(other) = other else { continue };
            let other_start = other.as_ptr() as u64;
            let other_end = other_start + other.len() as u64;
            if start < other_end && other_start < end {
                return Err(failure(
                    2,
                    "两个同时存活的堆块重叠",
                    if start >= other_start {
                        start
                    } else {
                        other_start
                    },
                ));
            }
            let _ = other_index;
        }
    }

    // 第 3 步：逐字节读回。这一步才真正证明页帧被映射且可写。
    for (index, block) in blocks.iter().enumerate() {
        let Some(block) = block else { continue };
        let expected = pattern(index);
        for (offset, byte) in block.iter().enumerate() {
            if *byte != expected {
                return Err(failure(
                    3,
                    "堆块内容读回不一致（页帧未映射或已被覆写）",
                    block.as_ptr() as u64 + offset as u64,
                ));
            }
        }
    }

    // 第 4 步：释放一半，再分配同样大小的替换块；验证互不相交且存活块未被破坏。
    for index in (0..BLOCK_COUNT).step_by(2) {
        blocks[index] = None;
    }
    let mut replacements: Vec<Vec<u8>> = Vec::with_capacity(BLOCK_COUNT / 2);
    for index in (0..BLOCK_COUNT).step_by(2) {
        let size = BLOCK_SIZES[index % BLOCK_SIZES.len()];
        replacements.push(alloc::vec![pattern(index) ^ 0xFF; size]);
    }
    for (index, block) in blocks.iter().enumerate() {
        let Some(block) = block else { continue };
        let expected = pattern(index);
        for (offset, byte) in block.iter().enumerate() {
            if *byte != expected {
                return Err(failure(
                    4,
                    "释放一半后，存活的堆块被破坏",
                    block.as_ptr() as u64 + offset as u64,
                ));
            }
        }
    }
    for (index, replacement) in replacements.iter().enumerate() {
        let start = replacement.as_ptr() as u64;
        let end = start + replacement.len() as u64;
        if start < heap_start || end > heap_end {
            return Err(failure(4, "替换块越出堆区间", start));
        }
        for (other_index, other) in blocks.iter().enumerate() {
            let Some(other) = other else { continue };
            let other_start = other.as_ptr() as u64;
            let other_end = other_start + other.len() as u64;
            if start < other_end && other_start < end {
                let _ = (index, other_index);
                return Err(failure(4, "替换块与存活块重叠", start));
            }
        }
    }

    // 第 5 步：全部释放 → 堆必须回到"一个空闲块覆盖整段"。
    drop(replacements);
    drop(blocks);
    let stats = heap::stats();
    if stats.free_blocks != 1 || stats.free_bytes != len - HEADER_SIZE {
        return Err(failure(
            5,
            "全部释放后堆没有合并回单个空闲块",
            stats.free_bytes as u64,
        ));
    }

    // 过大的请求必须干净地失败（返回空指针），而不是回绕或 panic。
    let layout = core::alloc::Layout::from_size_align(len * 2, 8)
        .map_err(|_| failure(5, "构造过大的分配请求失败", 0))?;
    // SAFETY: 只请求内存，不写入；失败时返回空指针，由这里断言。
    let pointer = unsafe { alloc::alloc::alloc(layout) };
    if !pointer.is_null() {
        // SAFETY: 上面确认非空，因此是本次分配返回的块。
        unsafe { alloc::alloc::dealloc(pointer, layout) };
        return Err(failure(5, "过大的分配请求竟然成功了", pointer as u64));
    }
    Ok(())
}

/// 内存自检（文档 §6.4）：堆（第 1–5 步）与页帧分配器（第 6 步）。
///
/// # Errors
/// 失败时返回 [`MemoryError::SelfCheck`]，其中带有步骤号、失败的不变量与相关地址。
pub fn self_check(boot_info: &BootInfo) -> Result<(), MemoryError> {
    heap_self_check()?;
    frame_self_check(boot_info)?;
    Ok(())
}

/// 构造"自检第 `step` 步失败"的错误。
fn failure(step: u8, what: &'static str, address: u64) -> MemoryError {
    MemoryError::SelfCheck {
        step,
        what,
        address,
    }
}

/// 由块号派生的字节图案（相邻块不同，便于定位越界写）。
const fn pattern(index: usize) -> u8 {
    PATTERN_SEED ^ index as u8
}

/// 每个块的大小（循环取用），刻意包含非 8 字节对齐与非 2 的幂的大小。
const BLOCK_SIZES: [usize; 8] = [16, 33, 64, 129, 256, 1025, 4096, 7];
/// 自检分配的块数。
const BLOCK_COUNT: usize = 64;
/// 图案种子。
const PATTERN_SEED: u8 = 0xA5;

/// 页帧分配器本体。
///
/// # Safety
/// 单核、中断关闭，且调用方不得与另一次借用重叠。
unsafe fn frame_allocator() -> &'static mut FrameAllocator<'static> {
    // SAFETY: 由调用方契约保证；尚未初始化时返回 None 视为内部错误（panic → 41）。
    unsafe { (*FRAME_ALLOCATOR.0.get()).as_mut() }
        .expect("页帧分配器尚未初始化：init_allocators 必须先于任何使用者调用")
}

/// 内核自己拥有、绝不能被发放的物理区间（文档 §5.2 第 4 条）。
///
/// 这些区间在内存图里本来就标着 `LoaderData`，因此规则上与"类型不是 conventional"重复；
/// 之所以还要显式列出来，是为了**不依赖固件是否正确分类了我们自己的分配**。
fn owned_ranges(boot_info: &BootInfo) -> [ReservedRange; OWNED_RANGES] {
    let arena = PAGE_ARENA.0.get() as u64;
    let bitmap = FRAME_BITMAP.0.get() as u64;
    let mmap_len = boot_info
        .mmap_len
        .saturating_mul(u64::from(boot_info.mmap_desc_size));
    let boot_info_page = (boot_info as *const BootInfo as u64) & !(paging::PAGE_SIZE - 1);
    let stack_start = boot_info.stack_top.saturating_sub(boot_info.stack_size);

    [
        page_range(boot_info.kernel_base, boot_info.kernel_size),
        page_range(stack_start, boot_info.stack_size),
        page_range(boot_info_page, paging::PAGE_SIZE),
        page_range(boot_info.mmap_ptr, mmap_len),
        page_range(arena, ARENA_BYTES as u64),
        page_range(bitmap, FRAME_BITMAP_BYTES as u64),
        ReservedRange::default(),
        ReservedRange::default(),
    ]
}

/// 把 `[start, start + len)` 扩到页边界，得到保留区间。
fn page_range(start: u64, len: u64) -> ReservedRange {
    if len == 0 {
        return ReservedRange::default();
    }
    let first = start & !(paging::PAGE_SIZE - 1);
    let last = start
        .saturating_add(len)
        .saturating_add(paging::PAGE_SIZE - 1)
        & !(paging::PAGE_SIZE - 1);
    ReservedRange::new(first, last)
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
