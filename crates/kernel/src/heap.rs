//! 内核全局堆：把 `kernel-memory` 的首次匹配空闲链表接到 `#[global_allocator]` 上。
//!
//! 依据 `docs/architecture/memory_subsystem.md` §6：1 MiB、连续页帧、M2 不增长。
//!
//! 接上之后 `Box` / `Vec` / `String` / `BTreeMap` / `format!` 在内核里都能用；代价是内核
//! 从此也能泄漏，因此 `memory::self_check` 会断言"全部释放后堆回到单个空闲块"。
//!
//! 并发：没有锁。单核、中断关闭是前置条件（文档 §5.4）；启用中断或第二个核之前必须先加锁。

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use kernel_memory::heap::{FreeList, HeapStats};

use crate::lock::SpinLock;

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
///
/// M3 起中断是开的，堆会被普通线程与（理论上）中断上下文共同使用，因此状态必须由
/// 中断安全自旋锁保护——这正是 M2 决策 #20 留下的前置条件。
struct KernelHeap {
    state: SpinLock<HeapState>,
}

impl KernelHeap {
    const fn new() -> Self {
        Self {
            state: SpinLock::new(HeapState {
                list: FreeList::empty(),
                base: 0,
                len: 0,
            }),
        }
    }

    /// 在 `[base, base + len)` 上初始化堆。
    ///
    /// # Safety
    /// 该区间必须已映射可写，且只调用一次。
    unsafe fn init(&self, base: u64, len: usize) {
        let mut state = self.state.lock();
        // SAFETY: 由调用方契约保证该物理区间可读写；它是恒等映射的，且我们持有锁。
        let arena = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, len) };
        state.list = FreeList::init(arena);
        state.base = base;
        state.len = len;
    }

    /// 堆区间 `(base, len)`。
    fn region(&self) -> (u64, usize) {
        let state = self.state.lock();
        (state.base, state.len)
    }
}

unsafe impl GlobalAlloc for KernelHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut state = self.state.lock();
        if state.len == 0 || state.base == 0 {
            return ptr::null_mut();
        }
        // SAFETY: 堆区间在初始化时已确认可读写；这里只在**空闲块元数据**所在的字节上操作，
        // 已交给调用方的内存不会被本分配器再次触碰，而且我们持有堆锁。
        let arena = unsafe { core::slice::from_raw_parts_mut(state.base as *mut u8, state.len) };
        match state.list.alloc(arena, layout.size(), layout.align()) {
            Some(offset) => (state.base + offset as u64) as *mut u8,
            None => ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, _layout: Layout) {
        let mut state = self.state.lock();
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
    KERNEL_HEAP.state.lock().list.stats()
}

/// 堆区间 `(起始物理地址, 长度)`。
#[must_use]
pub fn region() -> (u64, usize) {
    KERNEL_HEAP.region()
}
