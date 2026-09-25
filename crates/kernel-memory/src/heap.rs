//! 内核堆：首次匹配（first-fit）空闲链表。
//!
//! 依据 `docs/architecture/memory_subsystem.md` §6.2。设计要点：
//!
//! * 所有状态都存在调用方提供的**竞技场内部**（块头里），本结构只保存入口与统计，
//!   因此整个算法可以在宿主平台对着一个 `&mut [u8]` 单元测试；
//! * 块头固定 24 字节：`magic`(u32) + 填充(u32) + `size`(usize) + `next`(usize)，8 字节对齐；
//! * 空闲链表按**地址序**排列，释放时与前后邻居合并；
//! * 已分配块的块头就放在负载**紧前面**，因此 `dealloc` 只凭指针就能找回头部，
//!   调用方给出的 `Layout` 可以不同（`GlobalAlloc` 允许这样）；
//! * `magic` 区分"空闲块"与"已分配块"：重复释放与释放陌生指针都会被拒绝；
//! * 本模块不含任何 `unsafe`：所有读写都走切片索引，指针算术留在内核侧。
//!
//! 局限（文档 §6.1/§6.2）：M2 的堆不增长；统计在每次操作后重算整条链表，用可接受的额外
//! 扫描换取不会算错的账。

/// 块头长度（字节）。
pub const HEADER_SIZE: usize = 24;
/// 一个块至少要能装下的负载（字节）。
pub const MIN_PAYLOAD: usize = 16;
/// 会被单独切分出来的最小块（块头 + 最小负载）。
pub const MIN_BLOCK: usize = HEADER_SIZE + MIN_PAYLOAD;
/// 块头要求的对齐。
pub const HEADER_ALIGN: usize = 8;
/// 本实现支持的最大对齐。
pub const MAX_ALIGN: usize = 4096;

/// 空闲块的 magic（"FREE"）。
const MAGIC_FREE: u32 = 0x4652_4545;
/// 已分配块的 magic（"ALLO"）。
const MAGIC_ALLOC: u32 = 0x414C_4C4F;
/// 链表结束标记。
const NONE: usize = usize::MAX;

/// 释放失败的原因。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FreeListError {
    /// 指针不在竞技场内，或未按块头要求对齐。
    OutOfArena {
        /// 传入的偏移。
        offset: usize,
    },
    /// 该地址本来就在空闲链表里（重复释放）。
    AlreadyFree {
        /// 传入的偏移。
        offset: usize,
    },
    /// 该地址前面没有合法的已分配块头（不是本分配器发出的指针）。
    NotAllocated {
        /// 传入的偏移。
        offset: usize,
    },
    /// 链表或块头已损坏（例如 `size` 越出竞技场）。
    Corrupted {
        /// 出问题的偏移。
        offset: usize,
    },
}

impl core::fmt::Display for FreeListError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::OutOfArena { offset } => {
                write!(f, "堆偏移 0x{offset:X} 不在堆区间内或未对齐")
            }
            Self::AlreadyFree { offset } => write!(f, "堆偏移 0x{offset:X} 重复释放"),
            Self::NotAllocated { offset } => {
                write!(f, "堆偏移 0x{offset:X} 之前没有已分配的块头")
            }
            Self::Corrupted { offset } => write!(f, "堆元数据在偏移 0x{offset:X} 处已损坏"),
        }
    }
}

/// 堆的统计信息。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct HeapStats {
    /// 空闲块数量。
    pub free_blocks: usize,
    /// 空闲负载字节总数（不含块头）。
    pub free_bytes: usize,
    /// 最大的单个空闲块能提供的负载字节数。
    pub largest_free: usize,
}

/// 首次匹配空闲链表。
#[derive(Clone, Copy, Debug)]
pub struct FreeList {
    /// 第一个空闲块的偏移；[`NONE`] 表示没有空闲块。
    head: usize,
    /// 竞技场长度（已按 [`HEADER_ALIGN`] 向下取整）。
    arena_len: usize,
    /// 空闲块数量。
    free_blocks: usize,
    /// 空闲负载字节总数。
    free_bytes: usize,
    /// 最大的单个空闲块负载字节数。
    largest_free: usize,
}

impl FreeList {
    /// 尚未初始化的空链表。
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            head: NONE,
            arena_len: 0,
            free_blocks: 0,
            free_bytes: 0,
            largest_free: 0,
        }
    }

    /// 把整段竞技场初始化为一个空闲块。
    ///
    /// 竞技场长度不足 [`MIN_BLOCK`] 时返回未初始化的链表（此后任何分配都会失败）。
    #[must_use]
    pub fn init(arena: &mut [u8]) -> Self {
        let arena_len = arena.len() & !(HEADER_ALIGN - 1);
        if arena_len < MIN_BLOCK {
            return Self::empty();
        }
        write_header(arena, 0, MAGIC_FREE, arena_len, NONE);
        Self {
            head: 0,
            arena_len,
            free_blocks: 1,
            free_bytes: arena_len - HEADER_SIZE,
            largest_free: arena_len - HEADER_SIZE,
        }
    }

    /// 分配 `size` 字节、按 `align` 对齐的负载，返回其**竞技场偏移**。
    ///
    /// 失败返回 `None`：调用方据此返回空指针，由 `handle_alloc_error` 决定后续行为。
    pub fn alloc(&mut self, arena: &mut [u8], size: usize, align: usize) -> Option<usize> {
        if size == 0
            || !align.is_power_of_two()
            || align > MAX_ALIGN
            || self.head == NONE
            || !self.arena_matches(arena)
        {
            return None;
        }

        let mut previous = NONE;
        let mut current = self.head;
        let mut guard = 0usize;
        while current != NONE {
            guard += 1;
            if guard > self.max_blocks() {
                return None; // 链表疑似成环：宁可失败也不要挂死
            }
            let (magic, block_size, next) = read_header(arena, current)?;
            if magic != MAGIC_FREE || block_size < HEADER_SIZE {
                return None; // 元数据损坏
            }
            let block_end = current.checked_add(block_size)?;
            if block_end > self.arena_len {
                return None;
            }

            // 块头必须紧贴负载，因此负载之前只允许有两种情况：完全没有空隙，
            // 或者空隙大到足以单独成为一个空闲块。小的空隙被抬到 MIN_BLOCK。
            let payload_min = current + HEADER_SIZE;
            let mut payload = align_up(payload_min, align)?;
            let mut front_gap = payload - payload_min;
            if front_gap > 0 && front_gap < MIN_BLOCK {
                payload = align_up(payload_min.checked_add(MIN_BLOCK)?, align)?;
                front_gap = payload - payload_min;
            }
            let payload_end = payload.checked_add(size)?;
            if payload_end > block_end {
                previous = current;
                current = next;
                continue;
            }

            // 尾部分块的块头也必须对齐，因此从 payload_end 向上取整到 HEADER_ALIGN；
            // 装不下一个最小块时余量一并算进本次分配。
            let header_offset = payload - HEADER_SIZE;
            let tail_start = align_up(payload_end, HEADER_ALIGN)?;
            let tail = block_end.checked_sub(tail_start)?;
            let tail_block = if tail >= MIN_BLOCK { Some(tail) } else { None };
            let alloc_size = if tail_block.is_some() {
                tail_start - header_offset
            } else {
                block_end - header_offset
            };
            let previous_size = if previous == NONE {
                0
            } else {
                read_header(arena, previous)?.1
            };

            // 依次写好：本次分配的块头、尾部分块、前端分块。
            write_header(arena, header_offset, MAGIC_ALLOC, alloc_size, NONE);
            let mut new_next = next;
            if let Some(tail_size) = tail_block {
                write_header(arena, tail_start, MAGIC_FREE, tail_size, new_next);
                new_next = tail_start;
            }
            if front_gap > 0 {
                write_header(arena, current, MAGIC_FREE, front_gap, new_next);
                new_next = current;
            }

            // 把新的链节接回链表（`current` 已被消耗或被前端分块取代）。
            if previous == NONE {
                self.head = new_next;
            } else {
                write_header(arena, previous, MAGIC_FREE, previous_size, new_next);
            }
            self.recompute(arena);
            return Some(payload);
        }
        None
    }

    /// 释放偏移 `offset` 处的负载（必须是 `alloc` 返回过的值）。
    ///
    /// # Errors
    /// 见 [`FreeListError`]。
    pub fn dealloc(&mut self, arena: &mut [u8], offset: usize) -> Result<(), FreeListError> {
        if !self.arena_matches(arena)
            || offset < HEADER_SIZE
            || offset >= self.arena_len
            || !offset.is_multiple_of(HEADER_ALIGN)
        {
            return Err(FreeListError::OutOfArena { offset });
        }
        let header = offset - HEADER_SIZE;
        let (magic, block_size, _) =
            read_header(arena, header).ok_or(FreeListError::Corrupted { offset: header })?;
        if magic == MAGIC_FREE {
            return Err(FreeListError::AlreadyFree { offset });
        }
        if magic != MAGIC_ALLOC {
            return Err(FreeListError::NotAllocated { offset });
        }
        let block_end = header
            .checked_add(block_size)
            .ok_or(FreeListError::Corrupted { offset: header })?;
        // 一次分配可能把不足 MIN_BLOCK 的尾部余量吸收进来，因此已分配块只要装得下块头即可。
        if block_size < HEADER_SIZE || block_end > self.arena_len {
            return Err(FreeListError::Corrupted { offset: header });
        }

        // 按地址序找插入点，同时检查是否与现有空闲块重叠（另一道重复释放的防线）。
        let mut previous = NONE;
        let mut current = self.head;
        let mut guard = 0usize;
        while current != NONE {
            guard += 1;
            if guard > self.max_blocks() {
                return Err(FreeListError::Corrupted { offset: current });
            }
            let (magic, size, next) =
                read_header(arena, current).ok_or(FreeListError::Corrupted { offset: current })?;
            if magic != MAGIC_FREE || size < HEADER_SIZE || current + size > self.arena_len {
                return Err(FreeListError::Corrupted { offset: current });
            }
            if current >= header {
                if current < block_end {
                    return Err(FreeListError::AlreadyFree { offset });
                }
                break;
            }
            if current + size > header {
                return Err(FreeListError::AlreadyFree { offset });
            }
            previous = current;
            current = next;
        }

        // 合并：先吞掉紧随其后的空闲块，再把前面的空闲块扩展过来。
        let mut start = header;
        let mut size = block_size;
        if current != NONE && block_end == current {
            let (_, next_size, next_next) =
                read_header(arena, current).ok_or(FreeListError::Corrupted { offset: current })?;
            size += next_size;
            current = next_next;
        }
        if previous != NONE {
            let (_, previous_size, _) = read_header(arena, previous)
                .ok_or(FreeListError::Corrupted { offset: previous })?;
            if previous + previous_size == start {
                start = previous;
                size += previous_size;
                previous = self
                    .previous_free(arena, previous)
                    .ok_or(FreeListError::Corrupted { offset: start })?;
            }
        }

        write_header(arena, start, MAGIC_FREE, size, current);
        if previous == NONE {
            self.head = start;
        } else {
            let (_, previous_size, _) = read_header(arena, previous)
                .ok_or(FreeListError::Corrupted { offset: previous })?;
            write_header(arena, previous, MAGIC_FREE, previous_size, start);
        }
        self.recompute(arena);
        Ok(())
    }

    /// 统计信息。
    #[must_use]
    pub const fn stats(&self) -> HeapStats {
        HeapStats {
            free_blocks: self.free_blocks,
            free_bytes: self.free_bytes,
            largest_free: self.largest_free,
        }
    }

    /// 是否尚未初始化（或竞技场小到无法使用）。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.head == NONE
    }

    /// 竞技场长度。
    #[must_use]
    pub const fn arena_len(&self) -> usize {
        self.arena_len
    }

    /// 传入的竞技场是否就是初始化时的那一段。
    fn arena_matches(&self, arena: &[u8]) -> bool {
        (arena.len() & !(HEADER_ALIGN - 1)) == self.arena_len
    }

    /// 链表最多可能有多少个块：给遍历一个上界，元数据损坏时不至于死循环。
    fn max_blocks(&self) -> usize {
        self.arena_len / HEADER_ALIGN + 2
    }

    /// 找到 `target` 之前的那个空闲块。
    fn previous_free(&self, arena: &[u8], target: usize) -> Option<usize> {
        let mut previous = NONE;
        let mut current = self.head;
        let mut guard = 0usize;
        while current != NONE {
            guard += 1;
            if guard > self.max_blocks() {
                return None;
            }
            if current == target {
                return Some(previous);
            }
            let (_, _, next) = read_header(arena, current)?;
            previous = current;
            current = next;
        }
        None
    }

    /// 重新扫描整条空闲链表，重算统计。
    fn recompute(&mut self, arena: &[u8]) {
        let mut free_blocks = 0usize;
        let mut free_bytes = 0usize;
        let mut largest_free = 0usize;
        let mut current = self.head;
        let mut guard = 0usize;
        while current != NONE {
            guard += 1;
            if guard > self.max_blocks() {
                break;
            }
            let Some((magic, size, next)) = read_header(arena, current) else {
                break;
            };
            if magic != MAGIC_FREE || size < HEADER_SIZE || current + size > self.arena_len {
                break;
            }
            free_blocks += 1;
            let payload = size - HEADER_SIZE;
            free_bytes += payload;
            largest_free = largest_free.max(payload);
            current = next;
        }
        self.free_blocks = free_blocks;
        self.free_bytes = free_bytes;
        self.largest_free = largest_free;
    }
}

/// 向上对齐；溢出时返回 `None`。
fn align_up(value: usize, align: usize) -> Option<usize> {
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
}

/// 写入块头。
fn write_header(arena: &mut [u8], offset: usize, magic: u32, size: usize, next: usize) {
    arena[offset..offset + 4].copy_from_slice(&magic.to_le_bytes());
    arena[offset + 4..offset + 8].copy_from_slice(&0u32.to_le_bytes());
    arena[offset + 8..offset + 16].copy_from_slice(&size.to_le_bytes());
    arena[offset + 16..offset + 24].copy_from_slice(&next.to_le_bytes());
}

/// 读取块头；越界时返回 `None`。
fn read_header(arena: &[u8], offset: usize) -> Option<(u32, usize, usize)> {
    let end = offset.checked_add(HEADER_SIZE)?;
    let header = arena.get(offset..end)?;
    let magic = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let mut size_bytes = [0u8; 8];
    size_bytes.copy_from_slice(&header[8..16]);
    let mut next_bytes = [0u8; 8];
    next_bytes.copy_from_slice(&header[16..24]);
    Some((
        magic,
        usize::from_le_bytes(size_bytes),
        usize::from_le_bytes(next_bytes),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARENA_LEN: usize = 16 * 1024;

    /// 4 KiB 对齐的竞技场（比真实堆小得多，但足够跑完所有分支）。
    #[repr(align(4096))]
    struct Arena([u8; 16 * 1024]);

    impl Arena {
        fn new() -> Self {
            Self([0u8; 16 * 1024])
        }

        fn bytes(&mut self) -> &mut [u8] {
            &mut self.0
        }
    }

    /// 更大的竞技场，供"多块交替分配"这类负载使用。
    #[repr(align(4096))]
    struct BigArena([u8; 64 * 1024]);

    impl BigArena {
        fn new() -> Self {
            Self([0u8; 64 * 1024])
        }

        fn bytes(&mut self) -> &mut [u8] {
            &mut self.0
        }
    }

    /// 校验空闲链表的结构不变量：地址序、不重叠、不越界、统计一致、块头紧贴负载。
    fn assert_free_list_is_sane(arena: &[u8], list: &FreeList) {
        let mut previous_end = 0usize;
        let mut current = list.head;
        let mut blocks = 0usize;
        let mut bytes = 0usize;
        let mut largest = 0usize;
        while current != NONE {
            let (magic, size, next) = read_header(arena, current).expect("块头可读");
            assert_eq!(magic, MAGIC_FREE, "空闲块 magic 不对");
            // 空闲块至少要有块头；小于 MIN_BLOCK 的空闲块是合法但暂时无法利用的余量。
            assert!(size >= HEADER_SIZE, "空闲块连块头都装不下");
            assert!(current + size <= list.arena_len, "空闲块越界");
            assert_eq!(current % HEADER_ALIGN, 0, "空闲块起点必须满足块头对齐");
            assert!(current >= previous_end, "空闲块未按地址序排列或相互重叠");
            previous_end = current + size;
            blocks += 1;
            bytes += size - HEADER_SIZE;
            largest = largest.max(size - HEADER_SIZE);
            current = next;
            assert!(blocks <= list.max_blocks(), "链表疑似成环");
        }
        assert_eq!(list.stats().free_blocks, blocks);
        assert_eq!(list.stats().free_bytes, bytes);
        assert_eq!(list.stats().largest_free, largest);
    }

    #[test]
    fn init_creates_one_free_block_covering_the_arena() {
        let mut arena = Arena::new();
        let list = FreeList::init(arena.bytes());
        assert!(!list.is_empty());
        assert_eq!(list.arena_len(), ARENA_LEN);
        assert_eq!(list.stats().free_blocks, 1);
        assert_eq!(list.stats().free_bytes, ARENA_LEN - HEADER_SIZE);
        assert_eq!(list.stats().largest_free, ARENA_LEN - HEADER_SIZE);
        assert_free_list_is_sane(arena.bytes(), &list);
    }

    #[test]
    fn a_tiny_arena_is_refused() {
        let mut arena = Arena::new();
        let mut list = FreeList::init(&mut arena.bytes()[..MIN_BLOCK - 1]);
        assert!(list.is_empty());
        assert_eq!(list.arena_len(), 0);
        assert_eq!(list.alloc(arena.bytes(), 8, 8), None);
    }

    #[test]
    fn allocations_are_disjoint_inside_the_arena_and_shaped_like_their_request() {
        let mut arena = Arena::new();
        let mut list = FreeList::init(arena.bytes());
        let mut taken: Vec<(usize, usize)> = Vec::new();
        for index in 0..32usize {
            let size = 1 + index * 7 % 257;
            let offset = list.alloc(arena.bytes(), size, 8).expect("应当分配成功");
            assert_eq!(offset % HEADER_ALIGN, 0);
            assert!(offset >= HEADER_SIZE && offset + size <= list.arena_len());
            for (start, end) in &taken {
                assert!(
                    offset + size <= *start || *end <= offset,
                    "分配区间不得重叠"
                );
            }
            taken.push((offset, offset + size));
            assert_free_list_is_sane(arena.bytes(), &list);
        }
        // 一路顺序分配时，首次匹配只会不断切出尾部余量：空闲链表始终只有一个块。
        assert_eq!(list.stats().free_blocks, 1);

        // 释放中间一块才会出现第二个链节，而它应当被下一次分配优先复用。
        let (freed, freed_end) = taken[5];
        let freed_size = freed_end - freed;
        list.dealloc(arena.bytes(), freed).expect("释放成功");
        assert_eq!(list.stats().free_blocks, 2);
        assert_free_list_is_sane(arena.bytes(), &list);
        let again = list.alloc(arena.bytes(), freed_size, 8).expect("分配成功");
        assert_eq!(again, freed, "地址更低的空闲块应当被首次匹配选中");
    }

    #[test]
    fn a_freed_block_is_reused_by_the_next_allocation() {
        let mut arena = Arena::new();
        let mut list = FreeList::init(arena.bytes());
        let first = list.alloc(arena.bytes(), 64, 8).expect("分配成功");
        let second = list.alloc(arena.bytes(), 64, 8).expect("分配成功");
        list.dealloc(arena.bytes(), first).expect("释放成功");
        let again = list.alloc(arena.bytes(), 64, 8).expect("分配成功");
        assert_eq!(again, first, "首次匹配应当复用刚释放的块");
        assert_ne!(again, second);
        assert_free_list_is_sane(arena.bytes(), &list);
    }

    #[test]
    fn freeing_everything_coalesces_back_to_one_block() {
        let mut arena = Arena::new();
        let mut list = FreeList::init(arena.bytes());
        let mut offsets = Vec::new();
        for index in 0..16usize {
            offsets.push(list.alloc(arena.bytes(), 100 + index, 8).expect("分配成功"));
        }
        for offset in offsets {
            list.dealloc(arena.bytes(), offset).expect("释放成功");
        }
        assert_eq!(
            list.stats(),
            HeapStats {
                free_blocks: 1,
                free_bytes: ARENA_LEN - HEADER_SIZE,
                largest_free: ARENA_LEN - HEADER_SIZE,
            },
            "全部释放后必须合并回单个空闲块，且不丢字节"
        );
        assert_free_list_is_sane(arena.bytes(), &list);
    }

    #[test]
    fn out_of_order_frees_still_coalesce_completely() {
        let mut arena = Arena::new();
        let mut list = FreeList::init(arena.bytes());
        let mut offsets = Vec::new();
        for _ in 0..8 {
            offsets.push(list.alloc(arena.bytes(), 300, 8).expect("分配成功"));
        }
        for index in [3usize, 1, 5, 0, 7, 2, 6, 4] {
            list.dealloc(arena.bytes(), offsets[index])
                .expect("释放成功");
        }
        assert_eq!(list.stats().free_blocks, 1);
        assert_eq!(list.stats().free_bytes, ARENA_LEN - HEADER_SIZE);
        assert_free_list_is_sane(arena.bytes(), &list);
    }

    #[test]
    fn over_aligned_allocations_are_aligned_and_recoverable() {
        let mut arena = Arena::new();
        let mut list = FreeList::init(arena.bytes());
        // 先制造一个不对齐的起始点，逼出前端空隙。
        let _ = list.alloc(arena.bytes(), 8, 8).expect("分配成功");
        for align in [16usize, 32, 64, 256, 4096] {
            let offset = list.alloc(arena.bytes(), 24, align).expect("分配成功");
            assert_eq!(offset % align, 0, "负载必须满足请求的对齐");
            assert_eq!(offset % HEADER_ALIGN, 0, "负载必须满足块头对齐");
            // 块头紧贴负载：这是 dealloc 能只凭指针找回头部的前提。
            let (magic, size, _) = read_header(arena.bytes(), offset - HEADER_SIZE).expect("块头");
            assert_eq!(magic, MAGIC_ALLOC);
            assert!(offset - HEADER_SIZE + size <= list.arena_len());
            list.dealloc(arena.bytes(), offset).expect("释放成功");
            assert_free_list_is_sane(arena.bytes(), &list);
        }
    }

    #[test]
    fn dealloc_does_not_need_the_original_layout() {
        // GlobalAlloc 允许用不同的 Layout 释放：块头紧贴负载，因此这与 Layout 无关。
        let mut arena = Arena::new();
        let mut list = FreeList::init(arena.bytes());
        let offset = list.alloc(arena.bytes(), 37, 16).expect("分配成功");
        list.dealloc(arena.bytes(), offset).expect("释放成功");
        assert_eq!(list.stats().free_blocks, 1);
    }

    #[test]
    fn double_free_is_detected() {
        let mut arena = Arena::new();
        let mut list = FreeList::init(arena.bytes());
        let offset = list.alloc(arena.bytes(), 64, 8).expect("分配成功");
        list.dealloc(arena.bytes(), offset).expect("首次释放成功");
        assert_eq!(
            list.dealloc(arena.bytes(), offset),
            Err(FreeListError::AlreadyFree { offset })
        );
    }

    #[test]
    fn bogus_pointers_are_rejected() {
        let mut arena = Arena::new();
        let mut list = FreeList::init(arena.bytes());
        let offset = list.alloc(arena.bytes(), 64, 8).expect("分配成功");

        assert_eq!(
            list.dealloc(arena.bytes(), 3),
            Err(FreeListError::OutOfArena { offset: 3 })
        );
        assert_eq!(
            list.dealloc(arena.bytes(), ARENA_LEN),
            Err(FreeListError::OutOfArena { offset: ARENA_LEN })
        );
        assert_eq!(
            list.dealloc(arena.bytes(), offset + 1),
            Err(FreeListError::OutOfArena { offset: offset + 1 })
        );
        // 指向空闲区（那里的 magic 是 "FREE"）→ 重复释放。
        assert_eq!(
            list.dealloc(arena.bytes(), list.head + HEADER_SIZE),
            Err(FreeListError::AlreadyFree {
                offset: list.head + HEADER_SIZE
            })
        );
        list.dealloc(arena.bytes(), offset)
            .expect("正常释放仍然可用");
    }

    #[test]
    fn exhausted_heap_returns_none_instead_of_wrapping() {
        let mut arena = Arena::new();
        let mut list = FreeList::init(arena.bytes());
        assert!(list.alloc(arena.bytes(), ARENA_LEN, 8).is_none());
        assert!(
            list.alloc(arena.bytes(), ARENA_LEN - HEADER_SIZE, 8)
                .is_some()
        );
        assert!(list.alloc(arena.bytes(), 1, 8).is_none(), "已经用光了");
    }

    #[test]
    fn invalid_requests_are_refused() {
        let mut arena = Arena::new();
        let mut list = FreeList::init(arena.bytes());
        assert_eq!(list.alloc(arena.bytes(), 0, 8), None, "size 0 非法");
        assert_eq!(list.alloc(arena.bytes(), 16, 3), None, "对齐必须是 2 的幂");
        assert_eq!(
            list.alloc(arena.bytes(), 16, MAX_ALIGN * 2),
            None,
            "对齐过大"
        );
        // 竞技场长度不匹配（调用方传错堆区间）时必须拒绝，而不是乱写。
        let mut other = [0u8; 8192];
        assert_eq!(list.alloc(&mut other, 16, 8), None);
        assert_eq!(
            list.dealloc(&mut other, HEADER_SIZE),
            Err(FreeListError::OutOfArena {
                offset: HEADER_SIZE
            })
        );
    }

    #[test]
    fn many_blocks_of_varying_sizes_keep_the_invariants() {
        // 更接近内核自检的负载：分配 → 释放一半 → 再分配 → 全部释放。
        let mut arena = BigArena::new();
        let mut list = FreeList::init(arena.bytes());
        let sizes = [16usize, 33, 64, 129, 256, 1025, 4096, 7];
        let mut live: Vec<Option<usize>> = Vec::new();
        for index in 0..64usize {
            let size = sizes[index % sizes.len()];
            let offset = list.alloc(arena.bytes(), size, 8);
            assert!(offset.is_some(), "第 {index} 次分配失败");
            live.push(offset);
        }
        for index in (0..64).step_by(2) {
            let offset = live[index].take().expect("存活块");
            list.dealloc(arena.bytes(), offset).expect("释放成功");
        }
        for index in (0..64).step_by(2) {
            let size = sizes[index % sizes.len()];
            live[index] = list.alloc(arena.bytes(), size, 8);
            assert!(live[index].is_some(), "第 {index} 次替换分配失败");
        }
        assert_free_list_is_sane(arena.bytes(), &list);
        for offset in live.into_iter().flatten() {
            list.dealloc(arena.bytes(), offset).expect("释放成功");
        }
        assert_eq!(list.stats().free_blocks, 1, "全部释放后应当只剩一个空闲块");
        assert_eq!(list.stats().free_bytes, 64 * 1024 - HEADER_SIZE);
        assert_free_list_is_sane(arena.bytes(), &list);
    }

    #[test]
    fn largest_free_tracks_the_biggest_block() {
        let mut arena = Arena::new();
        let mut list = FreeList::init(arena.bytes());
        let before = list.stats().largest_free;
        let offset = list.alloc(arena.bytes(), 4096, 8).expect("分配成功");
        assert!(list.stats().largest_free <= before);
        list.dealloc(arena.bytes(), offset).expect("释放成功");
        assert_eq!(list.stats().largest_free, before);
    }
}
