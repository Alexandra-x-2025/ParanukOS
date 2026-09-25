//! 内核全局堆：把 `kernel-memory` 的首次匹配空闲链表接到 `#[global_allocator]` 上。
//!
//! 依据 `docs/architecture/memory_subsystem.md` §6：1 MiB、连续页帧、M2 不增长。
//!
//! 接上之后 `Box` / `Vec` / `String` / `BTreeMap` / `format!` 在内核里都能用；代价是内核
//! 从此也能泄漏，因此 `memory::self_check` 会断言"全部释放后堆回到单个空闲块"。
//!
//! 并发：没有锁。单核、中断关闭是前置条件（文档 §5.4）；启用中断或第二个核之前必须先加锁。

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;
use kernel_memory::heap::{FreeList, HeapStats};

/// 内核堆大小：256 个连续页帧 = 1 MiB。
pub const HEAP_SIZE: usize = 1024 * 1024;

/// 堆的内部状态。
struct HeapState {
    /// 空闲链表：元数据全部存在堆区间内部。
    list: FreeList,
    /// 堆区间起始物理地址；0 表示尚未初始化。
    base: u64,
    /// 堆区间长度。
    len: usize,
}

/// 全局分配器的内部状态。
struct KernelHeap(UnsafeCell<HeapState>);

// SAFETY: 单核、中断关闭（本里程碑的前置条件），每次分配/释放都在同一条不可重入的执行流里
// 完成。启用中断或多核之前必须先加锁，否则这里就是不安全的。
unsafe impl Sync for KernelHeap {}

impl KernelHeap {
    const fn new() -> Self {
        Self(UnsafeCell::new(HeapState {
            list: FreeList::empty(),
            base: 0,
            len: 0,
        }))
    }

    /// 在 `[base, base + len)` 上初始化堆。
    ///
    /// # Safety
    /// 单核、中断关闭；该区间必须已映射可写，且只调用一次。
    unsafe fn init(&self, base: u64, len: usize) {
        // SAFETY: 由调用方契约保证独占访问。
        let state = unsafe { &mut *self.0.get() };
        // SAFETY: 由调用方契约保证该物理区间可读写；它是恒等映射的。
        let arena = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, len) };
        state.list = FreeList::init(arena);
        state.base = base;
        state.len = len;
    }

    /// 取出 `&mut` 状态。
    ///
    /// # Safety
    /// 调用方必须保证当下没有别的代码持有该引用（单核、不可重入）。
    #[allow(clippy::mut_from_ref)]
    unsafe fn state(&self) -> &mut HeapState {
        // SAFETY: 由调用方契约保证。
        unsafe { &mut *self.0.get() }
    }

    /// 堆区间 `(base, len)`。
    fn region(&self) -> (u64, usize) {
        // SAFETY: 只读，且 `base`/`len` 在初始化后不再改变。
        let state = unsafe { &*self.0.get() };
        (state.base, state.len)
    }
}

unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: 由 GlobalAlloc 契约与单核、不可重入的前置条件保证独占访问。
        let state = unsafe { self.state() };
        if state.len == 0 || state.base == 0 {
            return ptr::null_mut();
        }
        // SAFETY: 堆区间在初始化时已确认可读写；这里只在**空闲块元数据**所在的字节上操作，
        // 已交给调用方的内存不会被本分配器再次触碰。
        let arena = unsafe { core::slice::from_raw_parts_mut(state.base as *mut u8, state.len) };
        match state.list.alloc(arena, layout.size(), layout.align()) {
            Some(offset) => (state.base + offset as u64) as *mut u8,
            None => ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, _layout: Layout) {
        // SAFETY: 同 `alloc`。
        let state = unsafe { self.state() };
        if state.len == 0 || state.base == 0 || pointer.is_null() {
            return;
        }
        let address = pointer as u64;
        if address < state.base || address >= state.base + state.len as u64 {
            return; // 不属于本堆：静默忽略，而不是破坏别的内存
        }
        let offset = (address - state.base) as usize;
        // SAFETY: 同 `alloc`。
        let arena = unsafe { core::slice::from_raw_parts_mut(state.base as *mut u8, state.len) };
        // 重复释放/陌生指针由空闲链表自己拒绝（那里有 magic 校验），这里不必 panic：
        // 全局分配器的 dealloc 没有返回通道，最稳妥的做法是保持堆的一致性。
        let _ = state.list.dealloc(arena, offset);
    }
}

#[global_allocator]
static KERNEL_HEAP: KernelHeap = KernelHeap::new();

/// 初始化内核堆。
///
/// # Safety
/// 单核、中断关闭；`[base, base + len)` 必须是已映射可写的连续内存，且本函数只调用一次。
pub unsafe fn init(base: u64, len: usize) {
    // SAFETY: 由调用方契约保证。
    unsafe { KERNEL_HEAP.init(base, len) };
}

/// 堆统计信息。
#[must_use]
pub fn stats() -> HeapStats {
    // SAFETY: 只读统计；`list` 只在单核、不可重入的分配/释放路径里被修改。
    unsafe { (&*KERNEL_HEAP.0.get()).list.stats() }
}

/// 堆区间 `(起始物理地址, 长度)`。
#[must_use]
pub fn region() -> (u64, usize) {
    KERNEL_HEAP.region()
}
