//! 由 UEFI 内存图构建**恒等映射**的四级页表。
//!
//! 依据 `docs/architecture/memory_subsystem.md` §3。要点：
//!
//! * 四级分页，物理地址 == 虚拟地址；
//! * 前 `fine_grained_bytes`（M2 取 2 MiB）用 4 KiB 页，其余用 2 MiB 大块；
//! * **只映射与 RAM 区域相交的地址**；非 RAM（保留区、不可用、MMIO、端口空间、持久内存、
//!   未知类型）一律不映射，误访问即 `#PF`；
//! * **页 0 永不映射**（决策 #21）：即使固件把它标成常规内存，空指针解引用也必须故障；
//! * 页表本体由调用方提供（内核用 `.bss` 里的静态竞技场），本模块只做算术与表项填充，
//!   因此**不含任何 `unsafe`**，全部逻辑可在宿主平台单元测试。
//!
//! 本模块**不**写 `CR3`、**不**碰 `CR0`/`CR4`：那是内核侧 `memory.rs` 的职责。

use core::fmt;

use crate::map::MemoryMap;
// 页大小由内存图模块定义（4 KiB），此处再导出，方便内核侧引用。
pub use crate::map::PAGE_SIZE;

/// 2 MiB 大块的粒度。
pub const BLOCK_SIZE: u64 = 2 * 1024 * 1024;

/// M2 恒等映射的地址空间上限：4 GiB。
///
/// 页表存储与页帧位图都是静态 `.bss`，因此必须有上限；QEMU 默认 128 MiB，远超测试所需。
/// 取消该上限的做法是把两者改为从页帧分配器自身取内存（见文档 §3.5）。
pub const MAX_IDENTITY_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// 一级表（PML4）条目数。
pub const PML4_ENTRIES: usize = 512;
/// 二级表（PDPT）条目数。
pub const PDPT_ENTRIES: usize = 512;
/// 三级表（PD）条目数；每个条目覆盖 2 MiB。
pub const PD_ENTRIES: usize = 512;
/// 四级表（PT）条目数；每个条目覆盖 4 KiB。
pub const PT_ENTRIES: usize = 512;

/// 一个 PDPT 覆盖的地址空间：512 × 1 GiB = 512 GiB。
pub const PDPT_COVERAGE: u64 = 512 * 1024 * 1024 * 1024;
/// 一个 PD 覆盖的地址空间：512 × 2 MiB = 1 GiB。
pub const PD_COVERAGE: u64 = 1024 * 1024 * 1024;

/// 本实现支持的映射上限：一个 PML4 项（512 GiB）。再大就需要多张 PDPT/PD，
/// 而 M2 的静态竞技场预算只按 4 GiB 计算。
pub const MAX_IDENTITY_SUPPORTED: u64 = PD_COVERAGE * PD_ENTRIES as u64;
/// 本实现支持的细粒度区间上限：一个 PD（1 GiB）。
pub const MAX_FINE_GRAINED_SUPPORTED: u64 = BLOCK_SIZE * PD_ENTRIES as u64;

/// 表项标志位。
pub const PRESENT: u64 = 1 << 0;
/// 可写。
pub const WRITABLE: u64 = 1 << 1;
/// 用户可访问（M2 不使用：尚无用户态）。
pub const USER: u64 = 1 << 2;
/// 直写（write-through）。
pub const WRITE_THROUGH: u64 = 1 << 3;
/// 禁止缓存。
pub const CACHE_DISABLE: u64 = 1 << 4;
/// 大块标志（仅在 PD 中有效）。
pub const BLOCK: u64 = 1 << 7;
/// 物理地址掩码（4 KiB 对齐，52 位物理地址）。
pub const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// 构建策略。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PagingConfig {
    /// 恒等映射的地址空间上限。
    pub max_identity_bytes: u64,
    /// 使用 4 KiB 页的地址区间长度（必须是 [`BLOCK_SIZE`] 的整数倍）。
    pub fine_grained_bytes: u64,
}

impl PagingConfig {
    /// M2 使用的配置：4 GiB 上限，前 2 MiB 用 4 KiB 页。
    pub const M2: Self = Self {
        max_identity_bytes: MAX_IDENTITY_BYTES,
        fine_grained_bytes: BLOCK_SIZE,
    };
}

/// 构建页表所需的字节数（用于静态竞技场的大小）。
///
/// M2 的取值：PML4 1 页 + PDPT 1 页 + PD 4 页 + 低端 PT 1 页 = 7 页 = 28 KiB
/// （文档 §3.5 钉住了这个数字）。
#[must_use]
pub const fn tables_needed(max_bytes: u64, fine_grained_bytes: u64) -> usize {
    let pml4 = max_bytes.div_ceil(PDPT_COVERAGE) as usize;
    let pd = max_bytes.div_ceil(PD_COVERAGE) as usize;
    let pt = fine_grained_bytes.div_ceil(BLOCK_SIZE) as usize;
    1 + pml4 + pd + pt
}

/// 构建页表失败的原因。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BuildError {
    /// 竞技场未按 4 KiB 对齐（页表项里的物理地址必须页对齐）。
    ArenaMisaligned {
        /// 竞技场实际地址。
        address: u64,
    },
    /// 竞技场不足以容纳所需的页表。
    ArenaTooSmall {
        /// 需要的字节数。
        needed: usize,
        /// 实际提供的字节数。
        got: usize,
    },
    /// 内存图里没有任何 RAM 区域，或可用 RAM 全在映射上限之上。
    NoUsableRam,
    /// 映射上限超出本实现支持的范围。
    ConfigTooLarge {
        /// 配置里的上限。
        max_identity_bytes: u64,
        /// 本实现支持的上限。
        supported: u64,
    },
    /// 细粒度区间超出本实现支持的范围。
    FineGrainedTooLarge {
        /// 配置里的细粒度区间长度。
        fine_grained_bytes: u64,
        /// 本实现支持的长度。
        supported: u64,
    },
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArenaMisaligned { address } => {
                write!(f, "页表竞技场 0x{address:X} 未按 {PAGE_SIZE} 字节对齐")
            }
            Self::ArenaTooSmall { needed, got } => {
                write!(f, "页表竞技场过小：需要 {needed} 字节，实际 {got} 字节")
            }
            Self::NoUsableRam => write!(f, "内存图中没有可用的 RAM 区域"),
            Self::ConfigTooLarge {
                max_identity_bytes,
                supported,
            } => write!(
                f,
                "映射上限 {max_identity_bytes} 字节超出本实现支持的 {supported} 字节"
            ),
            Self::FineGrainedTooLarge {
                fine_grained_bytes,
                supported,
            } => write!(
                f,
                "细粒度区间 {fine_grained_bytes} 字节超出本实现支持的 {supported} 字节"
            ),
        }
    }
}

/// 构建结果，供内核打印与自检使用。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PageTables {
    /// 顶级表（PML4）的物理地址，也就是要写进 `CR3` 的值。
    pub pml4_phys: u64,
    /// 实际映射的最高物理地址（= `min(ram_top, 上限)`）。
    pub limit: u64,
    /// 实际映射的地址空间总量（已映射的 4 KiB 页与大块之和；不含空洞）。
    pub mapped_bytes: u64,
    /// 已映射的 2 MiB 大块数量。
    pub blocks: usize,
    /// 已映射的 4 KiB 页数量。
    pub pages: usize,
    /// 竞技场消耗的页数。
    pub tables_used: usize,
}

/// 在 `arena` 中构建恒等映射页表。
///
/// `arena` 必须 4 KiB 对齐（内核用 `#[repr(align(4096))]` 的静态数组保证）。
/// 竞技场会被**完全清零**，因此不依赖它在 `.bss` 中被预先清零。
///
/// # Errors
/// 见 [`BuildError`]。
pub fn build(
    arena: &mut [u8],
    map: &MemoryMap<'_>,
    config: &PagingConfig,
) -> Result<PageTables, BuildError> {
    let arena_address = arena.as_ptr() as u64;
    if !arena_address.is_multiple_of(PAGE_SIZE) {
        return Err(BuildError::ArenaMisaligned {
            address: arena_address,
        });
    }
    if config.max_identity_bytes > MAX_IDENTITY_SUPPORTED {
        return Err(BuildError::ConfigTooLarge {
            max_identity_bytes: config.max_identity_bytes,
            supported: MAX_IDENTITY_SUPPORTED,
        });
    }
    if config.fine_grained_bytes > MAX_FINE_GRAINED_SUPPORTED {
        return Err(BuildError::FineGrainedTooLarge {
            fine_grained_bytes: config.fine_grained_bytes,
            supported: MAX_FINE_GRAINED_SUPPORTED,
        });
    }

    let pd_count = config.max_identity_bytes.div_ceil(PD_COVERAGE) as usize;
    let pt_count = config.fine_grained_bytes.div_ceil(BLOCK_SIZE) as usize;
    let tables = tables_needed(config.max_identity_bytes, config.fine_grained_bytes);
    let needed = tables * PAGE_SIZE as usize;
    if arena.len() < needed {
        return Err(BuildError::ArenaTooSmall {
            needed,
            got: arena.len(),
        });
    }

    let ram_top = map.ram_top().ok_or(BuildError::NoUsableRam)?;
    let limit = ram_top.min(config.max_identity_bytes);
    if limit == 0 {
        return Err(BuildError::NoUsableRam);
    }

    // 竞技场清零：未使用的表项必须是 not present，不能依赖调用方预清零。
    for byte in arena[..needed].iter_mut() {
        *byte = 0;
    }

    // 依次切出：PML4 1 页、PDPT 1 页（上限 512 GiB 只需一张）、PD `pd_count` 页、
    // 低端细粒度 PT `pt_count` 页。
    let base_phys = arena_address;
    let mut next_page = 0usize;
    let mut take_page = || {
        let address = base_phys + (next_page * PAGE_SIZE as usize) as u64;
        next_page += 1;
        address
    };
    let pml4 = take_page();
    let pdpt = take_page();
    let mut pds = [0u64; PD_ENTRIES];
    for slot in pds.iter_mut().take(pd_count) {
        *slot = take_page();
    }
    let mut pts = [0u64; PD_ENTRIES];
    for slot in pts.iter_mut().take(pt_count) {
        *slot = take_page();
    }

    let mut mapped_bytes = 0u64;
    let mut blocks = 0usize;
    let mut pages = 0usize;

    // 一级表 → 二级表；二级表 → 三级表。
    write_entry(arena, pml4, 0, pdpt | PRESENT | WRITABLE);
    for (index, pd) in pds.iter().enumerate().take(pd_count) {
        write_entry(arena, pdpt, index, *pd | PRESENT | WRITABLE);
    }

    // 三级表：低端 `pt_count` 个 2 MiB 槽位指向 PT（4 KiB 页），其余按大块填充。
    for (index, pd) in pds.iter().enumerate().take(pd_count) {
        for slot in 0..PD_ENTRIES {
            let block_index = index * PD_ENTRIES + slot;
            let address = index as u64 * PD_COVERAGE + slot as u64 * BLOCK_SIZE;
            if block_index < pt_count {
                write_entry(arena, *pd, slot, pts[block_index] | PRESENT | WRITABLE);
                continue;
            }
            if address >= limit || !overlaps_ram(map, address, address + BLOCK_SIZE) {
                continue;
            }
            write_entry(arena, *pd, slot, address | PRESENT | WRITABLE | BLOCK);
            blocks += 1;
            mapped_bytes += BLOCK_SIZE;
        }
    }

    // 四级表：细粒度区间。页 0 按策略永不映射。
    let fine_limit = config.fine_grained_bytes.min(limit);
    for (index, pt) in pts.iter().enumerate().take(pt_count) {
        for slot in 0..PT_ENTRIES {
            let address = index as u64 * BLOCK_SIZE + slot as u64 * PAGE_SIZE;
            if address == 0 || address >= fine_limit {
                continue;
            }
            if !overlaps_ram(map, address, address + PAGE_SIZE) {
                continue;
            }
            write_entry(arena, *pt, slot, address | PRESENT | WRITABLE);
            pages += 1;
            mapped_bytes += PAGE_SIZE;
        }
    }

    Ok(PageTables {
        pml4_phys: pml4,
        limit,
        mapped_bytes,
        blocks,
        pages,
        tables_used: tables,
    })
}

/// 写入一个表项：`table` 是表所在页的物理地址，`index` 是页内表项下标。
fn write_entry(arena: &mut [u8], table: u64, index: usize, value: u64) {
    let offset = (table - arena.as_ptr() as u64) as usize + index * 8;
    arena[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

/// 地址区间是否与某个 RAM 区域相交。
fn overlaps_ram(map: &MemoryMap<'_>, start: u64, end: u64) -> bool {
    map.iter()
        .any(|region| region.kind.is_ram() && region.overlaps(start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::MemoryKind;

    /// 4 KiB 对齐的竞技场（最大测试配置只需 5 页，这里留足余量）。
    #[repr(align(4096))]
    struct Arena([u8; 64 * 1024]);

    impl Arena {
        fn new() -> Self {
            Self([0u8; 64 * 1024])
        }

        fn bytes(&mut self) -> &mut [u8] {
            &mut self.0
        }
    }

    /// 编码一份内存图（步长 48，与 OVMF 实测一致）。
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

    fn parse(regions: &[(MemoryKind, u64, u64)]) -> Vec<u8> {
        map_of(regions)
    }

    fn read_entry(arena: &[u8], table: u64, index: usize) -> u64 {
        let offset = (table - arena.as_ptr() as u64) as usize + index * 8;
        u64::from_le_bytes(arena[offset..offset + 8].try_into().expect("8 字节"))
    }

    /// 取低端 PT 的物理地址（地址 0 所在的那张）。
    fn low_pt(arena: &[u8], pml4_phys: u64) -> u64 {
        let pdpt = read_entry(arena, pml4_phys, 0) & ADDRESS_MASK;
        let pd0 = read_entry(arena, pdpt, 0) & ADDRESS_MASK;
        read_entry(arena, pd0, 0) & ADDRESS_MASK
    }

    /// 取覆盖地址 0 所在 1 GiB 的 PD。
    fn low_pd(arena: &[u8], pml4_phys: u64) -> u64 {
        let pdpt = read_entry(arena, pml4_phys, 0) & ADDRESS_MASK;
        read_entry(arena, pdpt, 0) & ADDRESS_MASK
    }

    /// 近似 QEMU 的内存图：
    /// 0..0x9F000 常规 | 0x9F000..0x100000 空洞 | 1MiB..8MiB 常规
    /// 8MiB..10MiB 保留（正好一个 2 MiB 大块）| 10MiB..14MiB 常规
    fn qemu_like() -> Vec<u8> {
        parse(&[
            (MemoryKind::Conventional, 0x0, 0x9F),
            (MemoryKind::Conventional, 0x100000, 0x700),
            (MemoryKind::Reserved, 0x800000, 0x200),
            (MemoryKind::Conventional, 0xA00000, 0x400),
        ])
    }

    #[test]
    fn m2_table_budget_is_seven_pages() {
        // 文档 §3.5 的 28 KiB 就是这 7 页；改这里必须同时改文档。
        assert_eq!(tables_needed(MAX_IDENTITY_BYTES, BLOCK_SIZE), 7);
        assert_eq!(tables_needed(0, 0), 1);
        // 8 MiB 上限：PML4 + PDPT + 1 张 PD + 1 张 PT = 4 页
        assert_eq!(tables_needed(8 * 1024 * 1024, BLOCK_SIZE), 4);
    }

    #[test]
    fn builds_identity_mappings_with_the_expected_attributes() {
        let bytes = qemu_like();
        let map = MemoryMap::from_bytes(&bytes, 4, 48, 2).expect("解析成功");
        let mut arena = Arena::new();
        let tables = build(arena.bytes(), &map, &PagingConfig::M2).expect("构建成功");

        let arena = arena.bytes();
        let pml4 = tables.pml4_phys;
        let pdpt = read_entry(arena, pml4, 0) & ADDRESS_MASK;
        let pd0 = low_pd(arena, pml4);
        let pt = low_pt(arena, pml4);

        assert_eq!(read_entry(arena, pml4, 0), pdpt | PRESENT | WRITABLE);
        assert_eq!(read_entry(arena, pdpt, 0), pd0 | PRESENT | WRITABLE);
        assert_eq!(
            read_entry(arena, pd0, 0) & BLOCK,
            0,
            "低端必须挂页表，而不是 2 MiB 大块"
        );

        // 页 0 永不映射。
        assert_eq!(read_entry(arena, pt, 0), 0, "页 0 必须保持 not present");
        // 0x1000..0x9F000 是常规内存 → 已映射，属性只有 P|RW。
        assert_eq!(read_entry(arena, pt, 1), 0x1000 | PRESENT | WRITABLE);
        assert_eq!(read_entry(arena, pt, 0x9E), 0x9E000 | PRESENT | WRITABLE);
        // 0xA0000 起是低端空洞，不在任何 RAM 区域内 → 不映射。
        assert_eq!(read_entry(arena, pt, 0xA0), 0, "非 RAM 的低端空洞不得映射");
        assert_eq!(read_entry(arena, pt, 0xFF), 0, "0xFF000 也不在 RAM 内");

        // 2 MiB 大块：2/4/6 MiB 落在第二个常规区域内。
        assert_eq!(
            read_entry(arena, pd0, 1),
            0x200000 | PRESENT | WRITABLE | BLOCK
        );
        assert_eq!(
            read_entry(arena, pd0, 3),
            0x600000 | PRESENT | WRITABLE | BLOCK
        );
        // 8 MiB 大块只与保留区相交 → 不映射。
        assert_eq!(read_entry(arena, pd0, 4), 0, "只覆盖保留区的大块不得映射");
        // 10 MiB、12 MiB 又是常规内存 → 恢复映射（大块粒度的固有行为）。
        assert_eq!(
            read_entry(arena, pd0, 5),
            0xA00000 | PRESENT | WRITABLE | BLOCK
        );
        assert_eq!(
            read_entry(arena, pd0, 6),
            0xC00000 | PRESENT | WRITABLE | BLOCK
        );
        // 14 MiB 已达 limit → 不映射。
        assert_eq!(read_entry(arena, pd0, 7), 0, "limit 之上的大块不得映射");

        assert_eq!(tables.limit, 0xE00000);
        assert_eq!(tables.tables_used, 7);
        assert_eq!(tables.blocks, 5);
        // 0x1000..0x9F000 共 0x9E 页（去掉页 0），外加 1MiB..2MiB 的 0x100 页。
        assert_eq!(tables.pages, 0x9E + 0x100);
        assert_eq!(
            tables.mapped_bytes,
            (0x9E + 0x100) as u64 * PAGE_SIZE + 5 * BLOCK_SIZE
        );
    }

    #[test]
    fn every_mapped_entry_is_identity_and_has_no_extra_flags() {
        let bytes = qemu_like();
        let map = MemoryMap::from_bytes(&bytes, 4, 48, 2).expect("解析成功");
        let mut arena = Arena::new();
        let tables = build(arena.bytes(), &map, &PagingConfig::M2).expect("构建成功");
        let arena = arena.bytes();

        let pd0 = low_pd(arena, tables.pml4_phys);
        let pt = low_pt(arena, tables.pml4_phys);

        for slot in 0..PT_ENTRIES {
            let entry = read_entry(arena, pt, slot);
            if entry & PRESENT == 0 {
                continue;
            }
            let address = slot as u64 * PAGE_SIZE;
            assert_eq!(entry, address | PRESENT | WRITABLE, "必须严格恒等映射");
            assert_eq!(entry & (USER | WRITE_THROUGH | CACHE_DISABLE | BLOCK), 0);
            assert_eq!(entry >> 63, 0, "M2 不设 NX（全部可执行）");
        }

        let mut block_count = 0;
        for slot in 0..PD_ENTRIES {
            let entry = read_entry(arena, pd0, slot);
            if entry & PRESENT == 0 || (slot == 0) {
                continue;
            }
            let address = slot as u64 * BLOCK_SIZE;
            assert_eq!(entry, address | PRESENT | WRITABLE | BLOCK);
            assert_eq!(entry >> 63, 0);
            block_count += 1;
        }
        assert_eq!(block_count, tables.blocks);
    }

    #[test]
    fn unused_entries_are_left_zero() {
        let bytes = parse(&[(MemoryKind::Conventional, 0x1000, 2)]);
        let map = MemoryMap::from_bytes(&bytes, 1, 48, 0).expect("解析成功");
        let mut arena = Arena::new();
        let tables = build(arena.bytes(), &map, &PagingConfig::M2).expect("构建成功");
        let arena = arena.bytes();

        let pml4 = tables.pml4_phys;
        let pdpt = read_entry(arena, pml4, 0) & ADDRESS_MASK;
        // 4 GiB 上限只需一张 PDPT、四张 PD 中的第一张被引用。
        for slot in 1..PML4_ENTRIES {
            assert_eq!(
                read_entry(arena, pml4, slot),
                0,
                "PML4[{slot}] 应为 not present"
            );
        }
        // 4 GiB 上限 → PDPT 的前 4 个槽位各指向一张 PD，其余必须是 not present。
        for slot in 4..PDPT_ENTRIES {
            assert_eq!(
                read_entry(arena, pdpt, slot),
                0,
                "PDPT[{slot}] 应为 not present"
            );
        }
        let pt = low_pt(arena, pml4);
        assert_eq!(read_entry(arena, pt, 0), 0, "页 0 永不映射");
        assert_eq!(read_entry(arena, pt, 1), 0x1000 | PRESENT | WRITABLE);
        assert_eq!(read_entry(arena, pt, 2), 0x2000 | PRESENT | WRITABLE);
        for slot in 3..PT_ENTRIES {
            assert_eq!(read_entry(arena, pt, slot), 0, "超出 RAM 的页不得映射");
        }
        assert_eq!(tables.pages, 2);
        assert_eq!(tables.blocks, 0);
        assert_eq!(tables.limit, 0x3000);
    }

    #[test]
    fn page_zero_is_never_mapped_even_when_firmware_calls_it_conventional() {
        let bytes = parse(&[(MemoryKind::Conventional, 0x0, 0x100)]);
        let map = MemoryMap::from_bytes(&bytes, 1, 48, 0).expect("解析成功");
        let mut arena = Arena::new();
        let tables = build(arena.bytes(), &map, &PagingConfig::M2).expect("构建成功");
        let arena = arena.bytes();

        let pt = low_pt(arena, tables.pml4_phys);
        assert_eq!(read_entry(arena, pt, 0), 0, "决策 #21：页 0 永不映射");
        assert_eq!(read_entry(arena, pt, 1), 0x1000 | PRESENT | WRITABLE);
        assert_eq!(read_entry(arena, pt, 0xFF), 0xFF000 | PRESENT | WRITABLE);
    }

    #[test]
    fn respects_the_identity_limit() {
        // 上限 8 MiB：更高的 RAM 不得进入页表。
        let bytes = parse(&[(MemoryKind::Conventional, 0x1000, 0x2000)]);
        let map = MemoryMap::from_bytes(&bytes, 1, 48, 0).expect("解析成功");
        let config = PagingConfig {
            max_identity_bytes: 8 * 1024 * 1024,
            fine_grained_bytes: BLOCK_SIZE,
        };
        let mut arena = Arena::new();
        let tables = build(arena.bytes(), &map, &config).expect("构建成功");
        assert_eq!(tables.limit, 8 * 1024 * 1024);
        assert_eq!(tables.blocks, 3, "2/4/6 MiB 三块；8 MiB 已在 limit 之外");
    }

    #[test]
    fn supports_more_than_one_fine_grained_table() {
        // 前 4 MiB 用 4 KiB 页：需要两张 PT，且都挂在同一张 PD 的前两个槽位上。
        let bytes = parse(&[(MemoryKind::Conventional, 0x1000, 0x800)]);
        let map = MemoryMap::from_bytes(&bytes, 1, 48, 0).expect("解析成功");
        let config = PagingConfig {
            max_identity_bytes: 8 * 1024 * 1024,
            fine_grained_bytes: 4 * 1024 * 1024,
        };
        assert_eq!(
            tables_needed(config.max_identity_bytes, config.fine_grained_bytes),
            5,
            "PML4 + PDPT + PD + 2×PT"
        );
        let mut arena = Arena::new();
        let tables = build(arena.bytes(), &map, &config).expect("构建成功");
        let arena = arena.bytes();

        assert_eq!(tables.tables_used, 5);
        let pd0 = low_pd(arena, tables.pml4_phys);
        let pt0 = read_entry(arena, pd0, 0) & ADDRESS_MASK;
        let pt1 = read_entry(arena, pd0, 1) & ADDRESS_MASK;
        assert_ne!(pt0, pt1);
        assert_eq!(read_entry(arena, pd0, 0) & BLOCK, 0);
        assert_eq!(read_entry(arena, pd0, 1) & BLOCK, 0);
        assert_eq!(read_entry(arena, pt0, 0x1FF), 0x1FF000 | PRESENT | WRITABLE);
        assert_eq!(read_entry(arena, pt1, 0), 0x200000 | PRESENT | WRITABLE);
        assert_eq!(read_entry(arena, pt1, 0x1FF), 0x3FF000 | PRESENT | WRITABLE);
        // 细粒度区间是 0..4MiB：0x1000..0x400000 共 0x400 页，去掉页 0 → 0x3FF 页；
        // 4 MiB 以上改用 2 MiB 大块。
        assert_eq!(tables.pages, 0x3FF);
        assert_eq!(tables.blocks, 2, "4..6 MiB 与 6..8 MiB 两块");
        assert_eq!(tables.limit, 8 * 1024 * 1024);
    }

    #[test]
    fn a_region_that_overlaps_a_block_only_partly_still_maps_the_block() {
        // RAM 是 1MiB..3MiB：2MiB..4MiB 这个 2 MiB 大块只被覆盖一半（3MiB..4MiB 是空洞），
        // 但按粒度它整块被映射（文档 §3.4 的权衡）。
        let bytes = parse(&[(MemoryKind::Conventional, 0x100000, 0x200)]);
        let map = MemoryMap::from_bytes(&bytes, 1, 48, 0).expect("解析成功");
        let mut arena = Arena::new();
        let tables = build(arena.bytes(), &map, &PagingConfig::M2).expect("构建成功");
        let arena = arena.bytes();

        let pd0 = low_pd(arena, tables.pml4_phys);
        assert_eq!(
            read_entry(arena, pd0, 1),
            0x200000 | PRESENT | WRITABLE | BLOCK
        );
        assert_eq!(read_entry(arena, pd0, 2), 0, "更上面没有 RAM，不映射");
        assert_eq!(tables.pages, 0x100, "1MiB..2MiB 全在细粒度区间内");
        assert_eq!(tables.blocks, 1);
        assert_eq!(tables.limit, 0x300000);
    }

    #[test]
    fn rejects_a_small_arena_and_reports_the_need() {
        let bytes = qemu_like();
        let map = MemoryMap::from_bytes(&bytes, 4, 48, 2).expect("解析成功");
        let mut arena = Arena::new();
        let err = build(&mut arena.bytes()[..4096 * 3], &map, &PagingConfig::M2)
            .expect_err("竞技场过小必须报错");
        assert_eq!(
            err,
            BuildError::ArenaTooSmall {
                needed: 4096 * 7,
                got: 4096 * 3
            }
        );
    }

    #[test]
    fn rejects_a_misaligned_arena() {
        let bytes = qemu_like();
        let map = MemoryMap::from_bytes(&bytes, 4, 48, 2).expect("解析成功");
        let mut arena = Arena::new();
        let address = arena.bytes()[1..].as_ptr() as u64;
        let err = build(&mut arena.bytes()[1..], &map, &PagingConfig::M2).expect_err("必须报错");
        assert_eq!(err, BuildError::ArenaMisaligned { address });
    }

    #[test]
    fn rejects_a_map_without_ram() {
        let bytes = parse(&[
            (MemoryKind::Reserved, 0x1000, 0x10),
            (MemoryKind::Mmio, 0x100000, 0x10),
        ]);
        let map = MemoryMap::from_bytes(&bytes, 2, 48, 0).expect("解析成功");
        let mut arena = Arena::new();
        assert_eq!(
            build(arena.bytes(), &map, &PagingConfig::M2).expect_err("没有 RAM 必须报错"),
            BuildError::NoUsableRam
        );
    }

    #[test]
    fn rejects_configs_beyond_the_supported_range() {
        let bytes = parse(&[(MemoryKind::Conventional, 0x1000, 0x10)]);
        let map = MemoryMap::from_bytes(&bytes, 1, 48, 0).expect("解析成功");
        let mut arena = Arena::new();

        let too_large = PagingConfig {
            max_identity_bytes: MAX_IDENTITY_SUPPORTED + PD_COVERAGE,
            fine_grained_bytes: BLOCK_SIZE,
        };
        assert_eq!(
            build(arena.bytes(), &map, &too_large).expect_err("上限过大必须报错"),
            BuildError::ConfigTooLarge {
                max_identity_bytes: MAX_IDENTITY_SUPPORTED + PD_COVERAGE,
                supported: MAX_IDENTITY_SUPPORTED,
            }
        );

        let too_fine = PagingConfig {
            max_identity_bytes: MAX_IDENTITY_BYTES,
            fine_grained_bytes: MAX_FINE_GRAINED_SUPPORTED + BLOCK_SIZE,
        };
        assert_eq!(
            build(arena.bytes(), &map, &too_fine).expect_err("细粒度区间过大必须报错"),
            BuildError::FineGrainedTooLarge {
                fine_grained_bytes: MAX_FINE_GRAINED_SUPPORTED + BLOCK_SIZE,
                supported: MAX_FINE_GRAINED_SUPPORTED,
            }
        );
    }

    #[test]
    fn arena_is_cleared_before_use() {
        // 竞技场预先填满 0xFF：未使用的表项必须被清零，不能留下脏数据。
        let bytes = qemu_like();
        let map = MemoryMap::from_bytes(&bytes, 4, 48, 2).expect("解析成功");
        let mut arena = Arena::new();
        for byte in arena.bytes().iter_mut() {
            *byte = 0xFF;
        }
        let tables = build(arena.bytes(), &map, &PagingConfig::M2).expect("构建成功");
        let arena = arena.bytes();

        let pdpt = read_entry(arena, tables.pml4_phys, 0) & ADDRESS_MASK;
        let pd3 = read_entry(arena, pdpt, 3) & ADDRESS_MASK;
        assert_ne!(pd3, 0, "第 4 张 PD 应存在（上限 4 GiB）");
        for slot in 0..PD_ENTRIES {
            let address = 3 * PD_COVERAGE + slot as u64 * BLOCK_SIZE;
            let entry = read_entry(arena, pd3, slot);
            let expected =
                if address < tables.limit && overlaps_ram(&map, address, address + BLOCK_SIZE) {
                    address | PRESENT | WRITABLE | BLOCK
                } else {
                    0
                };
            assert_eq!(entry, expected, "3 GiB 区间的表项内容不符");
        }
        for slot in 0..PML4_ENTRIES {
            if slot == 0 {
                continue;
            }
            assert_eq!(read_entry(arena, tables.pml4_phys, slot), 0);
        }
    }

    #[test]
    fn overlaps_ram_uses_half_open_intervals() {
        let bytes = parse(&[(MemoryKind::Conventional, 0x1000, 1)]);
        let map = MemoryMap::from_bytes(&bytes, 1, 48, 0).expect("解析成功");
        assert!(overlaps_ram(&map, 0, 0x2000));
        assert!(overlaps_ram(&map, 0x1000, 0x2000));
        assert!(!overlaps_ram(&map, 0x2000, 0x3000), "右端相接不算相交");
        assert!(!overlaps_ram(&map, 0, 0x1000), "左端相接不算相交");

        // 非 RAM 区域不参与判定：一处保留区不应让地址被映射。
        let bytes = parse(&[(MemoryKind::Reserved, 0x1000, 1)]);
        let map = MemoryMap::from_bytes(&bytes, 1, 48, 0).expect("解析成功");
        assert!(!overlaps_ram(&map, 0x1000, 0x2000));
    }
}
