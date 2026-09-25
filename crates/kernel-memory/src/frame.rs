//! 物理页帧分配器：位图 + 最低地址优先。
//!
//! 依据 `docs/architecture/memory_subsystem.md` §5。要点：
//!
//! * 粒度 4 KiB；位图每帧 1 bit，**`1 = 已占用`**；
//! * 初始时所有页帧都标为已占用，只有**完整落在 `EfiConventionalMemory` 区域内**、
//!   且不与任何显式保留区间相交的页帧才会被标为空闲；
//! * 显式保留区间（内核镜像、内核栈、`BootInfo` 页、内存图缓冲、页表竞技场、位图自身）
//!   在信息上与"类型不是 Conventional"重复，但**刻意保留**：分配器不应依赖固件是否
//!   正确分类了我们自己的分配（文档 §5.2 第 4 条）；
//! * 最低地址优先，因此分配结果确定，宿主测试与 QEMU 自检都能做精确断言；
//! * 本模块不含任何 `unsafe`：位图由调用方提供，越界访问都返回错误而不是 UB。
//!
//! 并发：没有锁。单核、中断关闭是调用方的前置条件（文档 §5.4）。

use crate::map::{MemoryKind, MemoryMap, PAGE_SIZE};

/// 低于此地址的页帧永不发放。
///
/// 低端内存放着固件结构，而且 2 MiB 以下只是低端页表的附带映射；把它们挡在分配器之外
/// 可以消除一整类意外（文档 §5.2 第 2 条）。
pub const MIN_FREE_ADDR: u64 = 1024 * 1024;

/// 构造时最多接受的显式保留区间数量。
pub const MAX_RESERVED_RANGES: usize = 8;

/// 一个 4 KiB 物理页帧。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct PhysFrame {
    /// 页帧的物理起始地址（4 KiB 对齐）。
    pub address: u64,
}

impl PhysFrame {
    /// 由物理地址构造（不校验对齐；对齐由调用方保证）。
    #[must_use]
    pub const fn new(address: u64) -> Self {
        Self { address }
    }
}

/// 一段连续的物理内存。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PhysRange {
    /// 起始物理地址。
    pub start: u64,
    /// 字节长度。
    pub len: u64,
}

impl PhysRange {
    /// 结束地址（不含）。
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.start + self.len
    }

    /// 覆盖的页帧数量。
    #[must_use]
    pub const fn frames(&self) -> u64 {
        self.len / PAGE_SIZE
    }

    /// 地址是否落在区间内。
    #[must_use]
    pub const fn contains(&self, address: u64) -> bool {
        self.start <= address && address < self.end()
    }
}

/// 一段不得发放的物理区间（半开区间）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ReservedRange {
    /// 起始地址。
    pub start: u64,
    /// 结束地址（不含）。
    pub end: u64,
}

impl ReservedRange {
    /// 构造。
    #[must_use]
    pub const fn new(start: u64, end: u64) -> Self {
        Self { start, end }
    }

    /// 区间 `[start, end)` 是否与本区间相交。
    #[must_use]
    pub const fn overlaps(&self, start: u64, end: u64) -> bool {
        self.start < end && start < self.end
    }

    /// 是否为空区间。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// 构造配置。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// 受管理区间的起点（M2 取 [`MIN_FREE_ADDR`]）。
    pub managed_start: u64,
    /// 受管理区间的终点（M2 取页表的 `limit`）。
    pub limit: u64,
}

impl Config {
    /// M2 的配置：`[1 MiB, limit)`。
    #[must_use]
    pub const fn new(limit: u64) -> Self {
        Self {
            managed_start: MIN_FREE_ADDR,
            limit,
        }
    }
}

/// 分配器统计。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FrameStats {
    /// 受管理的页帧总数。
    pub managed: u64,
    /// 当前空闲的页帧数量。
    pub free: u64,
}

/// 初始化失败的原因。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InitError {
    /// 位图缓冲区不足以覆盖受管理区间。
    BitmapTooSmall {
        /// 需要的字节数。
        needed: usize,
        /// 实际提供的字节数。
        got: usize,
    },
    /// 保留区间数量超过 [`MAX_RESERVED_RANGES`]。
    TooManyReservedRanges {
        /// 实际传入的数量。
        count: usize,
    },
    /// 受管理区间为空或非法。
    EmptyRange {
        /// 配置里的起点。
        managed_start: u64,
        /// 配置里的终点。
        limit: u64,
    },
    /// 没有任何可发放的页帧。
    NoUsableMemory,
}

impl core::fmt::Display for InitError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BitmapTooSmall { needed, got } => {
                write!(f, "页帧位图过小：需要 {needed} 字节，实际 {got} 字节")
            }
            Self::TooManyReservedRanges { count } => {
                write!(f, "保留区间过多：{count} 个，上限 {MAX_RESERVED_RANGES} 个")
            }
            Self::EmptyRange {
                managed_start,
                limit,
            } => write!(f, "受管理的物理区间非法：0x{managed_start:X}..0x{limit:X}"),
            Self::NoUsableMemory => write!(f, "没有任何可发放的 EfiConventionalMemory 页帧"),
        }
    }
}

/// 释放失败的原因。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FreeError {
    /// 地址不在受管理区间内。
    NotManaged(u64),
    /// 该页帧本来就是空闲的（重复释放）。
    AlreadyFree(u64),
    /// 该页帧落在显式保留区间内，不属于分配器。
    Reserved(u64),
}

impl core::fmt::Display for FreeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotManaged(address) => write!(f, "页帧 0x{address:X} 不在受管理区间内"),
            Self::AlreadyFree(address) => write!(f, "页帧 0x{address:X} 重复释放"),
            Self::Reserved(address) => write!(f, "页帧 0x{address:X} 位于保留区间内，不可释放"),
        }
    }
}

/// 位图页帧分配器。
///
/// 用**两张位图**：`allocatable` 标记"这一帧属于可发放集合"（完整落在 conventional 区域内、
/// 且不在保留区间里），`used` 标记"已发放"。分成两张的理由是 [`Self::free`] 必须能区分
/// "释放一个从未属于分配器的页帧"（例如固件保留区或 MMIO）与"重复释放"——只有一张位图时
/// 两者都是 `1`，无法区分，`free` 就会把不该发放的内存放进空闲池。
#[derive(Debug)]
pub struct FrameAllocator<'a> {
    /// 可发放位图：每帧 1 bit，`1 = 可发放`。
    allocatable: &'a mut [u8],
    /// 占用位图：每帧 1 bit，`1 = 已发放`。
    used: &'a mut [u8],
    /// 位图第 0 位对应的页帧号。
    first_frame: u64,
    /// 受管理的页帧数量。
    frames: u64,
    /// 当前空闲页帧数量（与两张位图保持同步）。
    free_frames: u64,
    /// 显式保留区间。
    reserved: [ReservedRange; MAX_RESERVED_RANGES],
    /// `reserved` 中有效项的数量。
    reserved_len: usize,
}

/// 覆盖 `frames` 个页帧所需的位图字节数（两张位图各占一半）。
#[must_use]
pub const fn bitmap_bytes(frames: u64) -> usize {
    2 * frames.div_ceil(8) as usize
}

/// 读取一位。
fn bit(bytes: &[u8], index: u64) -> bool {
    bytes[(index / 8) as usize] & (1u8 << (index % 8)) != 0
}

/// 写入一位。
fn set_bit(bytes: &mut [u8], index: u64, value: bool) {
    let mask = 1u8 << (index % 8);
    let byte = &mut bytes[(index / 8) as usize];
    if value {
        *byte |= mask;
    } else {
        *byte &= !mask;
    }
}

impl<'a> FrameAllocator<'a> {
    /// 由内存图构造分配器。
    ///
    /// 两张位图都会被完全重写，因此调用方不需要预先清零。
    ///
    /// # Errors
    /// 见 [`InitError`]。
    pub fn init(
        bitmap: &'a mut [u8],
        map: &MemoryMap<'_>,
        reserved: &[ReservedRange],
        config: &Config,
    ) -> Result<Self, InitError> {
        if reserved.len() > MAX_RESERVED_RANGES {
            return Err(InitError::TooManyReservedRanges {
                count: reserved.len(),
            });
        }

        let first_frame = config.managed_start.div_ceil(PAGE_SIZE);
        let end_frame = config.limit / PAGE_SIZE;
        if end_frame <= first_frame {
            return Err(InitError::EmptyRange {
                managed_start: config.managed_start,
                limit: config.limit,
            });
        }
        let frames = end_frame - first_frame;
        let half = (frames as usize).div_ceil(8);
        let needed = 2 * half;
        if bitmap.len() < needed {
            return Err(InitError::BitmapTooSmall {
                needed,
                got: bitmap.len(),
            });
        }
        let (allocatable, used) = bitmap.split_at_mut(half);

        let mut reserved_slots = [ReservedRange::default(); MAX_RESERVED_RANGES];
        reserved_slots[..reserved.len()].copy_from_slice(reserved);

        let mut allocator = Self {
            allocatable,
            used,
            first_frame,
            frames,
            free_frames: 0,
            reserved: reserved_slots,
            reserved_len: reserved.len(),
        };

        // 起点是"默认安全"：没有任何页帧可发放，也没有任何页帧被算作空闲。
        allocator.allocatable[..half].fill(0x00);
        allocator.used[..half].fill(0xFF);

        let managed_start = first_frame * PAGE_SIZE;
        let limit = end_frame * PAGE_SIZE;
        for region in map.iter() {
            // M2 只发放 conventional 内存（决策 #15）。复用 Boot Services 内存值得单独测量，
            // 而 M2 并不缺这点内存。
            if region.kind != MemoryKind::Conventional {
                continue;
            }
            let start = region.base.max(managed_start);
            let end = region.end().min(limit);
            let mut address = start;
            while address < end {
                if !allocator.is_reserved(address) {
                    allocator.mark_available(address);
                }
                address += PAGE_SIZE;
            }
        }

        if allocator.free_frames == 0 {
            return Err(InitError::NoUsableMemory);
        }
        Ok(allocator)
    }

    /// 分配一个页帧（最低地址优先）。
    pub fn alloc(&mut self) -> Option<PhysFrame> {
        let index = self.find_free_run(1)?;
        self.mark_used(index);
        Some(PhysFrame::new((self.first_frame + index) * PAGE_SIZE))
    }

    /// 分配 `count` 个**连续**页帧（首次匹配最低地址）。
    ///
    /// 内核堆是一段连续的字节区间，因此需要这个接口；找不到足够长的连续空闲区时返回 `None`，
    /// 而不是拼出一段碎片。
    pub fn alloc_contiguous(&mut self, count: usize) -> Option<PhysRange> {
        if count == 0 {
            return None;
        }
        let count = count as u64;
        let start = self.find_free_run(count)?;
        for index in start..start + count {
            self.mark_used(index);
        }
        Some(PhysRange {
            start: (self.first_frame + start) * PAGE_SIZE,
            len: count * PAGE_SIZE,
        })
    }

    /// 释放一个页帧。
    ///
    /// # Errors
    /// 见 [`FreeError`]。M2 没有生产调用者（堆不会收缩），它服务于自检与 M3。
    pub fn free(&mut self, frame: PhysFrame) -> Result<(), FreeError> {
        let Some(index) = self.frame_index(frame.address) else {
            return Err(FreeError::NotManaged(frame.address));
        };
        if self.is_reserved(frame.address) {
            return Err(FreeError::Reserved(frame.address));
        }
        if !self.is_allocatable(index) {
            // 从未属于可发放集合（例如固件保留区、MMIO、非 conventional 内存）。
            return Err(FreeError::NotManaged(frame.address));
        }
        if !self.is_used(index) {
            // 可发放但未发放 —— 只有重复释放会走到这里。
            return Err(FreeError::AlreadyFree(frame.address));
        }
        set_bit(self.used, index, false);
        self.free_frames += 1;
        Ok(())
    }

    /// 统计信息。
    #[must_use]
    pub const fn stats(&self) -> FrameStats {
        FrameStats {
            managed: self.frames,
            free: self.free_frames,
        }
    }

    /// 显式保留区间（自检用来交叉校验）。
    #[must_use]
    pub fn reserved_ranges(&self) -> &[ReservedRange] {
        &self.reserved[..self.reserved_len]
    }

    /// `[start, end)` 内是否存在**空闲**页帧。
    ///
    /// 自检用它验证"没有任何空闲页帧落在保留区间里"，也就是文档 §5.2 第 4 条的交叉校验。
    #[must_use]
    pub fn first_free_in(&self, start: u64, end: u64) -> Option<PhysFrame> {
        let first = start / PAGE_SIZE;
        let last = end.div_ceil(PAGE_SIZE);
        for frame in first..last {
            if let Some(index) = self.frame_index(frame * PAGE_SIZE)
                && self.is_free(index)
            {
                return Some(PhysFrame::new(frame * PAGE_SIZE));
            }
        }
        None
    }

    /// 页帧号是否在受管理区间内。
    fn frame_index(&self, address: u64) -> Option<u64> {
        let frame = address / PAGE_SIZE;
        if !address.is_multiple_of(PAGE_SIZE)
            || frame < self.first_frame
            || frame >= self.first_frame + self.frames
        {
            return None;
        }
        Some(frame - self.first_frame)
    }

    /// 地址是否落在显式保留区间内。
    fn is_reserved(&self, address: u64) -> bool {
        let end = address + PAGE_SIZE;
        self.reserved_ranges()
            .iter()
            .any(|range| range.overlaps(address, end))
    }

    /// 该页帧是否属于可发放集合。
    fn is_allocatable(&self, index: u64) -> bool {
        bit(self.allocatable, index)
    }

    /// 该页帧是否已被发放。
    fn is_used(&self, index: u64) -> bool {
        bit(self.used, index)
    }

    /// 该页帧是否空闲（可发放且未被发放）。
    fn is_free(&self, index: u64) -> bool {
        self.is_allocatable(index) && !self.is_used(index)
    }

    /// 把一个页帧加入可发放集合，并标记为空闲。
    fn mark_available(&mut self, address: u64) {
        let Some(index) = self.frame_index(address) else {
            return;
        };
        set_bit(self.allocatable, index, true);
        set_bit(self.used, index, false);
        self.free_frames += 1;
    }

    /// 标记为已发放。
    ///
    /// 注入特性 `inject-double-alloc` 把它换成空操作：分配器于是会把同一个页帧发出去两次，
    /// 正好命中"重复分配"这个最危险的 bug 类型（内核自检会发现并要求以 43 退出）。
    #[cfg(not(feature = "inject-double-alloc"))]
    fn mark_used(&mut self, index: u64) {
        set_bit(self.used, index, true);
        self.free_frames -= 1;
    }

    /// 注入版本：不置位，于是同一个页帧会被重复发出。
    #[cfg(feature = "inject-double-alloc")]
    fn mark_used(&mut self, _index: u64) {}

    /// 找到第一段长度至少为 `count` 的连续空闲页帧，返回起始页帧号。
    fn find_free_run(&self, count: u64) -> Option<u64> {
        if count == 0 || count > self.frames {
            return None;
        }
        let mut run_start = 0u64;
        let mut run = 0u64;
        for index in 0..self.frames {
            if !self.is_free(index) {
                run = 0;
                continue;
            }
            if run == 0 {
                run_start = index;
            }
            run += 1;
            if run == count {
                return Some(run_start);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 够放 4 GiB 位图的缓冲区（128 KiB）。
    #[repr(align(8))]
    struct Bitmap([u8; 128 * 1024]);

    impl Bitmap {
        fn new() -> Self {
            Self([0u8; 128 * 1024])
        }

        fn bytes(&mut self) -> &mut [u8] {
            &mut self.0
        }
    }

    fn map_of(regions: &[(MemoryKind, u64, u64)]) -> Vec<u8> {
        let stride = 48;
        let mut bytes = vec![0u8; regions.len() * stride];
        for (index, (kind, base, pages)) in regions.iter().enumerate() {
            let offset = index * stride;
            bytes[offset..offset + 4].copy_from_slice(&kind.raw().to_le_bytes());
            bytes[offset + 8..offset + 16].copy_from_slice(&base.to_le_bytes());
            bytes[offset + 24..offset + 32].copy_from_slice(&pages.to_le_bytes());
        }
        bytes
    }

    const LIMIT: u64 = 16 * 1024 * 1024;

    /// 16 MiB 上限下的常规内存：1 MiB..8 MiB + 10 MiB..16 MiB，中间 8..10 MiB 是保留区。
    fn default_map() -> Vec<u8> {
        map_of(&[
            (MemoryKind::Conventional, 0x0, 0x9F),
            (MemoryKind::Conventional, 0x100000, 0x700),
            (MemoryKind::Reserved, 0x800000, 0x200),
            (MemoryKind::Conventional, 0xA00000, 0x600),
        ])
    }

    /// 描述符步长固定为 48（与 OVMF 实测一致），条数由缓冲区长度推出。
    fn parsed(map_bytes: &[u8]) -> MemoryMap<'_> {
        let entries = (map_bytes.len() / 48) as u64;
        MemoryMap::from_bytes(map_bytes, entries, 48, 2).expect("解析成功")
    }

    fn allocator<'a>(
        bitmap: &'a mut [u8],
        map_bytes: &'a [u8],
        reserved: &[ReservedRange],
    ) -> FrameAllocator<'a> {
        let map = parsed(map_bytes);
        FrameAllocator::init(bitmap, &map, reserved, &Config::new(LIMIT)).expect("初始化成功")
    }

    #[test]
    fn bit_is_one_for_used_and_zero_for_free() {
        let map_bytes = default_map();
        let mut bitmap = Bitmap::new();
        let mut frames = allocator(bitmap.bytes(), &map_bytes, &[]);
        // 16 MiB / 4 KiB = 4096 帧，但低于 1 MiB 的不受管理：受管理 4096 - 256 = 3840 帧。
        assert_eq!(frames.stats().managed, 3840);
        // 1MiB..8MiB 共 1792 帧，10MiB..16MiB 共 1536 帧 → 3328 帧空闲。
        assert_eq!(frames.stats().free, 3328);

        // 最低地址优先：第一个发放的是 1 MiB。
        let first = frames.alloc().expect("应有空闲页帧");
        assert_eq!(first.address, 0x100000);
        assert_eq!(frames.stats().free, 3327);
        assert_eq!(frames.alloc().expect("应有空闲页帧").address, 0x101000);

        // 释放后重新分配应拿回同一帧。
        frames.free(first).expect("释放成功");
        assert_eq!(frames.stats().free, 3327);
        assert_eq!(frames.alloc().expect("应有空闲页帧").address, 0x100000);
    }

    #[test]
    fn reserved_ranges_are_never_handed_out() {
        let map_bytes = default_map();
        let mut bitmap = Bitmap::new();
        // 保留 1MiB..2MiB（正是最低的那一批页帧）。
        let reserved = [ReservedRange::new(0x100000, 0x200000)];
        let frames = allocator(bitmap.bytes(), &map_bytes, &reserved);

        assert_eq!(frames.stats().free, 3328 - 256);
        assert_eq!(frames.reserved_ranges().len(), 1);
        assert_eq!(
            frames.first_free_in(0x100000, 0x200000),
            None,
            "保留区间内不得有任何空闲页帧"
        );
        assert_eq!(
            frames.first_free_in(0x200000, 0x201000),
            Some(PhysFrame::new(0x200000))
        );

        let mut frames = frames;
        assert_eq!(frames.alloc().expect("应有空闲页帧").address, 0x200000);
        assert_eq!(
            frames.alloc().expect("应有空闲页帧").address,
            0x201000,
            "保留区必须被整段跳过"
        );
    }

    #[test]
    fn free_refuses_frames_outside_the_allocatable_set() {
        // LoaderData 在内存图里也是 RAM，但 M2 不发放它（决策 #15）。即便这些帧落在
        // 受管理区间内，`free` 也必须拒绝，否则会把不属于分配器的内存放进空闲池。
        let map_bytes = map_of(&[
            (MemoryKind::Conventional, 0x100000, 0x100),
            (MemoryKind::LoaderData, 0x200000, 0x100),
        ]);
        let mut bitmap = Bitmap::new();
        let mut frames = allocator(bitmap.bytes(), &map_bytes, &[]);
        assert_eq!(
            frames.free(PhysFrame::new(0x200000)),
            Err(FreeError::NotManaged(0x200000))
        );
        assert_eq!(
            frames.free(PhysFrame::new(0x300000)),
            Err(FreeError::NotManaged(0x300000)),
            "内存图里根本没有的地址同样不可释放"
        );
        assert_eq!(frames.stats().free, 0x100, "只有 conventional 那一批可发放");
    }

    #[test]
    fn only_conventional_memory_is_handed_out() {
        // LoaderData/BootServices/ACPI 都算 RAM，但 M2 只发放 Conventional（决策 #15）。
        let map_bytes = map_of(&[
            (MemoryKind::Conventional, 0x100000, 0x100),
            (MemoryKind::LoaderData, 0x200000, 0x100),
            (MemoryKind::BootServicesData, 0x300000, 0x100),
            (MemoryKind::AcpiReclaim, 0x400000, 0x100),
        ]);
        let mut bitmap = Bitmap::new();
        let frames = allocator(bitmap.bytes(), &map_bytes, &[]);
        assert_eq!(
            frames.stats().free,
            0x100,
            "只有 Conventional 的那 256 帧空闲"
        );
        assert_eq!(frames.first_free_in(0x200000, 0x500000), None);
    }

    #[test]
    fn frames_below_one_mib_are_not_managed() {
        // 唯一的 conventional 区间整个落在 1 MiB 以下 → 没有可发放的页帧，直接拒绝初始化。
        let map_bytes = map_of(&[(MemoryKind::Conventional, 0x1000, 0x9F)]);
        let map = parsed(&map_bytes);
        let mut bitmap = Bitmap::new();
        assert_eq!(
            FrameAllocator::init(bitmap.bytes(), &map, &[], &Config::new(LIMIT)).unwrap_err(),
            InitError::NoUsableMemory
        );
    }

    #[test]
    fn rejects_a_map_without_conventional_memory() {
        let map_bytes = map_of(&[(MemoryKind::LoaderData, 0x100000, 0x100)]);
        let mut bitmap = Bitmap::new();
        let map = parsed(&map_bytes);
        assert_eq!(
            FrameAllocator::init(bitmap.bytes(), &map, &[], &Config::new(LIMIT)).unwrap_err(),
            InitError::NoUsableMemory
        );
    }

    #[test]
    fn rejects_a_small_bitmap_and_reports_the_need() {
        let map_bytes = default_map();
        let map = parsed(&map_bytes);
        let mut bitmap = Bitmap::new();
        // 受管理 3840 帧 → 两张位图各 480 字节，共 960 字节。
        assert_eq!(
            FrameAllocator::init(&mut bitmap.bytes()[..479], &map, &[], &Config::new(LIMIT))
                .unwrap_err(),
            InitError::BitmapTooSmall {
                needed: 960,
                got: 479
            }
        );
    }

    #[test]
    fn rejects_too_many_and_empty_ranges() {
        let map_bytes = default_map();
        let map = parsed(&map_bytes);
        let mut bitmap = Bitmap::new();
        let many = [ReservedRange::new(0, PAGE_SIZE); MAX_RESERVED_RANGES + 1];
        assert_eq!(
            FrameAllocator::init(bitmap.bytes(), &map, &many, &Config::new(LIMIT)).unwrap_err(),
            InitError::TooManyReservedRanges {
                count: MAX_RESERVED_RANGES + 1
            }
        );

        let mut bitmap = Bitmap::new();
        assert_eq!(
            FrameAllocator::init(
                bitmap.bytes(),
                &map,
                &[],
                &Config {
                    managed_start: 0x200000,
                    limit: 0x200000
                }
            )
            .unwrap_err(),
            InitError::EmptyRange {
                managed_start: 0x200000,
                limit: 0x200000
            }
        );
    }

    #[test]
    fn contiguous_allocation_skips_fragments() {
        let map_bytes = map_of(&[
            (MemoryKind::Conventional, 0x100000, 0x10), // 1MiB..1MiB+64KiB
            (MemoryKind::Reserved, 0x110000, 0x10),     // 让它断开
            (MemoryKind::Conventional, 0x200000, 0x100),
        ]);
        let mut bitmap = Bitmap::new();
        let map = parsed(&map_bytes);
        let mut frames =
            FrameAllocator::init(bitmap.bytes(), &map, &[], &Config::new(LIMIT)).expect("初始化");

        // 4 帧的连续块放不进 16 帧的第一段吗？放得进（16 帧）。用 20 帧来要求落在第二段。
        let range = frames.alloc_contiguous(20).expect("第二段有 256 帧连续");
        assert_eq!(range.start, 0x200000);
        assert_eq!(range.len, 20 * PAGE_SIZE);
        assert_eq!(range.frames(), 20);
        assert!(range.contains(0x200000));
        assert!(!range.contains(range.end()));
        // 第一段仍在（8 帧以下还能拿）。
        assert_eq!(frames.alloc().expect("第一段还有空闲").address, 0x100000);

        // 请求超过任何一段连续区 → None，而不是拼碎片。
        assert!(frames.alloc_contiguous(4096).is_none());
        assert!(frames.alloc_contiguous(0).is_none());
    }

    #[test]
    fn free_reports_not_managed_already_free_and_reserved() {
        let map_bytes = map_of(&[(MemoryKind::Conventional, 0x100000, 0x100)]);
        let mut bitmap = Bitmap::new();
        let reserved = [ReservedRange::new(0x104000, 0x105000)];
        let mut frames = allocator(bitmap.bytes(), &map_bytes, &reserved);

        assert_eq!(
            frames.free(PhysFrame::new(0x500000)),
            Err(FreeError::NotManaged(0x500000))
        );
        assert_eq!(
            frames.free(PhysFrame::new(0x100001)),
            Err(FreeError::NotManaged(0x100001)),
            "非页对齐地址不属于任何页帧"
        );
        assert_eq!(
            frames.free(PhysFrame::new(0x104000)),
            Err(FreeError::Reserved(0x104000))
        );

        let frame = frames.alloc().expect("应有空闲页帧");
        assert_eq!(frame.address, 0x100000);
        frames.free(frame).expect("释放成功");
        assert_eq!(
            frames.free(frame),
            Err(FreeError::AlreadyFree(0x100000)),
            "重复释放必须被发现"
        );
    }

    #[test]
    fn the_bitmaps_cover_every_managed_frame_exactly_once() {
        // 位图最后一个字节里的"多余位"必须是**不可发放**：否则它们会被当成空闲页帧发出去。
        let map_bytes = map_of(&[(MemoryKind::Conventional, 0x100000, 0x123)]);
        let mut bitmap = Bitmap::new();
        let frames = allocator(bitmap.bytes(), &map_bytes, &[]);
        let managed = frames.stats().managed;
        assert_eq!(managed, 3840, "受管理区间是 [1 MiB, 16 MiB)，与内存图无关");
        assert_eq!(frames.stats().free, 0x123, "只有那 0x123 帧是 conventional");
        assert_eq!(bitmap_bytes(managed), 2 * (managed as usize).div_ceil(8));

        let beyond = 0x100000u64 + managed * PAGE_SIZE;
        assert_eq!(
            frames.frame_index(beyond),
            None,
            "受管理区间之上的地址不属于分配器"
        );

        let valid_bits = managed % 8;
        if valid_bits != 0 {
            let last_byte = ((managed - 1) / 8) as usize;
            let mask = !((1u8 << valid_bits) - 1);
            assert_eq!(
                frames.allocatable[last_byte] & mask,
                0,
                "尾部多余位不得可发放"
            );
            assert_eq!(
                frames.used[last_byte] & mask,
                mask,
                "尾部多余位必须视为已占用"
            );
        }
    }

    #[test]
    fn config_and_range_helpers() {
        let config = Config::new(LIMIT);
        assert_eq!(config.managed_start, MIN_FREE_ADDR);
        assert_eq!(config.limit, LIMIT);
        let range = PhysRange {
            start: 0x1000,
            len: 3 * PAGE_SIZE,
        };
        assert_eq!(range.end(), 0x1000 + 3 * PAGE_SIZE);
        assert_eq!(range.frames(), 3);
        let reserved = ReservedRange::new(0x1000, 0x2000);
        assert!(reserved.overlaps(0, 0x1001));
        assert!(!reserved.overlaps(0x2000, 0x3000));
        assert!(!reserved.is_empty());
        assert!(ReservedRange::new(0x1000, 0x1000).is_empty());
    }
}
