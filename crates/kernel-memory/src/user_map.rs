//! 用户区映射（`user_mode.md` §5）。
//!
//! 与 [`crate::paging`] 的恒等映射不同，这里把**物理页映射到用户虚拟地址**，并且每个叶项带上
//! `U/S = 1`，使 CPL 3 能访问它们——而内核的恒等映射（`U/S = 0`）对用户态完全不可见。
//!
//! 与 `paging` 一样，本模块**只用切片索引**（表以"竞技场内的偏移"表示），因此不需要任何
//! `unsafe`，整段逻辑都能在宿主平台单测。
//!
//! M4 只有一个地址空间：用户区的表直接挂在**当前（内核的）页表**上（`USER_BASE = 4 GiB`，即
//! 恒等映射终点之后的第一片地址），因此 M4 **不切换 CR3**［决策 #54］。用独立 PML4 + 按线程
//! 地址空间是下一个里程碑的工作。

use core::fmt;

use crate::map::PAGE_SIZE;
use crate::paging::{ADDRESS_MASK, PD_ENTRIES, PT_ENTRIES};

/// 用户区基址：正好是内核恒等映射（`MAX_IDENTITY_BYTES = 4 GiB`）的终点。
pub const USER_BASE: u64 = 0x1_0000_0000;

/// 页表项标志（与 [`crate::paging`] 一致，这里只用到这几个）。
const PRESENT: u64 = 1 << 0;
const WRITABLE: u64 = 1 << 1;
/// 用户可访问：这是用户区与内核映射唯一的区别所在。
const USER: u64 = 1 << 2;

/// 一段要建立的用户映射。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mapping {
    /// 用户虚拟地址（必须页对齐）。
    pub vaddr: u64,
    /// 对应的物理地址（必须页对齐）。
    pub paddr: u64,
    /// 字节长度（必须页对齐）。
    pub size: u64,
    /// 是否可写（`true` = RW，`false` = 只读）。
    pub writable: bool,
}

impl Mapping {
    /// 结束虚拟地址。
    #[must_use]
    pub const fn vaddr_end(&self) -> u64 {
        self.vaddr + self.size
    }
}

/// 映射结果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mapped {
    /// 实际映射的字节数。
    pub mapped_bytes: u64,
    /// 消耗的页表页数。
    pub tables_used: usize,
}

/// 映射失败的原因。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MapError {
    /// 竞技场不足以容纳还需要新建的页表。
    ArenaTooSmall {
        /// 需要的页数。
        needed: usize,
        /// 实际可用的页数。
        got: usize,
    },
    /// 映射的虚拟地址低于 [`USER_BASE`]（会覆盖内核的恒等映射）。
    BelowUserBase {
        /// 违规的虚拟地址。
        vaddr: u64,
    },
    /// 地址、长度或物理地址未按页对齐，或长度为零。
    NotPageAligned {
        /// 出问题的值。
        value: u64,
    },
    /// 请求的映射超出本实现支持的区间（一个 PD，即 1 GiB）。
    OutOfRange {
        /// 结束虚拟地址。
        vaddr_end: u64,
    },
    /// 该用户区已经映射过（M4a 只支持一次调用）。
    AlreadyMapped,
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArenaTooSmall { needed, got } => {
                write!(f, "用户页表竞技场过小：需要 {needed} 页，实际 {got} 页")
            }
            Self::BelowUserBase { vaddr } => write!(
                f,
                "用户映射的虚拟地址 0x{vaddr:X} 低于 USER_BASE（0x{USER_BASE:X}）"
            ),
            Self::NotPageAligned { value } => {
                write!(f, "地址/长度 0x{value:X} 未按 4 KiB 对齐或为零")
            }
            Self::OutOfRange { vaddr_end } => write!(
                f,
                "用户映射结束地址 0x{vaddr_end:X} 超出本实现支持的范围（USER_BASE + 1 GiB）"
            ),
            Self::AlreadyMapped => write!(f, "该用户区已经映射过（M4a 只支持一次调用）"),
        }
    }
}

/// 本实现支持的用户区上限：一个 PD 覆盖 1 GiB。
pub const USER_MAX_BYTES: u64 = PD_ENTRIES as u64 * 2 * 1024 * 1024;

/// 建立用户映射（全部以竞技场偏移表示，物理地址 = 竞技场基址 + 偏移）。
///
/// `pml4_offset` 是**当前生效**的 PML4 在竞技场里的偏移（内核页表就在同一个竞技场里，用户区的
/// 新表也放这里，因此 `CR3` 不必切换——M4 只有一个地址空间，决策 #54）；`first_free_offset`
/// 是内核表之后第一个可用的页偏移（显式给出，不靠"扫全零页"这种脆弱的猜测）。
///
/// M4a 只支持**一次调用**：用户区在同一个 PD 下建好全部映射。重复映射同一个区域返回
/// [`MapError::AlreadyMapped`]——增量映射（用于按需分页）是后续里程碑的事。
///
/// # Errors
/// 见 [`MapError`]。
pub fn map_user_region(
    arena: &mut [u8],
    pml4_offset: usize,
    first_free_offset: usize,
    mappings: &[Mapping],
) -> Result<Mapped, MapError> {
    let address = arena.as_ptr() as u64;
    if !(arena.as_ptr() as usize).is_multiple_of(PAGE_SIZE as usize) {
        return Err(MapError::NotPageAligned { value: address });
    }
    if !first_free_offset.is_multiple_of(PAGE_SIZE as usize)
        || !pml4_offset.is_multiple_of(PAGE_SIZE as usize)
    {
        return Err(MapError::NotPageAligned {
            value: first_free_offset as u64,
        });
    }
    let pages = arena.len() / PAGE_SIZE as usize;
    let region_end = USER_BASE + USER_MAX_BYTES;
    if mappings.is_empty() {
        return Ok(Mapped {
            mapped_bytes: 0,
            tables_used: 0,
        });
    }

    for mapping in mappings {
        if mapping.vaddr < USER_BASE {
            return Err(MapError::BelowUserBase {
                vaddr: mapping.vaddr,
            });
        }
        if mapping.vaddr_end() > region_end {
            return Err(MapError::OutOfRange {
                vaddr_end: mapping.vaddr_end(),
            });
        }
        if !mapping.vaddr.is_multiple_of(PAGE_SIZE)
            || !mapping.paddr.is_multiple_of(PAGE_SIZE)
            || !mapping.size.is_multiple_of(PAGE_SIZE)
            || mapping.size == 0
        {
            return Err(MapError::NotPageAligned {
                value: mapping.vaddr,
            });
        }
    }

    // PML4[0] 必须已存在（内核恒等映射），否则说明调用方给错了偏移。
    let pdpt_entry = read_entry_at(arena, pml4_offset, 0);
    if pdpt_entry & PRESENT == 0 {
        return Err(MapError::BelowUserBase {
            vaddr: pml4_offset as u64,
        });
    }
    // **每一级**都必须带 U/S = 1，否则 CPL 3 的访问会在中间层就被拒（`#PF`，错误码的 P 位=1、
    // U 位=1）。真正的可达性由**叶项**决定：内核恒等映射的 2 MiB 大块叶项是 U=0，所以即使中间层
    // 允许用户访问，CPL 3 依然碰不到内核内存。
    if pdpt_entry & USER == 0 {
        write_entry_at(arena, pml4_offset, 0, pdpt_entry | USER);
    }
    let pdpt = ((pdpt_entry & ADDRESS_MASK) - address) as usize;
    let pdpt_index = ((USER_BASE >> 30) & 0x1FF) as usize;
    if read_entry_at(arena, pdpt, pdpt_index) & PRESENT != 0 {
        return Err(MapError::AlreadyMapped);
    }

    // 需要的页数：1 张 PD + 每 2 MiB 一张 PT。
    let mut highest_slot = 0usize;
    for mapping in mappings {
        let last = (((mapping.vaddr_end() - USER_BASE - 1) / PAGE_SIZE) as usize) / PT_ENTRIES;
        highest_slot = highest_slot.max(last);
    }
    let pt_count = highest_slot + 1;
    let first_page = first_free_offset / PAGE_SIZE as usize;
    let needed = first_page + 1 + pt_count;
    if pages < needed {
        return Err(MapError::ArenaTooSmall { needed, got: pages });
    }

    let pd_offset = first_free_offset;
    clear_page(arena, pd_offset);
    write_entry_at(
        arena,
        pdpt,
        pdpt_index,
        (address + pd_offset as u64) | PRESENT | WRITABLE | USER,
    );

    let mut pt_offsets = [0usize; PD_ENTRIES];
    let mut next = first_page + 1;
    for offset in pt_offsets.iter_mut().take(pt_count) {
        let table_offset = next * PAGE_SIZE as usize;
        *offset = table_offset;
        clear_page(arena, table_offset);
        write_entry_at(
            arena,
            pd_offset,
            next - (first_page + 1),
            (address + table_offset as u64) | PRESENT | WRITABLE | USER,
        );
        next += 1;
    }

    let mut mapped_bytes = 0u64;
    for mapping in mappings {
        for index in 0..(mapping.size / PAGE_SIZE) {
            let vaddr = mapping.vaddr + index * PAGE_SIZE;
            let page_index = ((vaddr - USER_BASE) / PAGE_SIZE) as usize;
            let slot = page_index / PT_ENTRIES;
            let entry_index = page_index % PT_ENTRIES;
            let mut flags = PRESENT | USER;
            if mapping.writable {
                flags |= WRITABLE;
            }
            write_entry_at(
                arena,
                pt_offsets[slot],
                entry_index,
                (mapping.paddr + index * PAGE_SIZE) | flags,
            );
            mapped_bytes += PAGE_SIZE;
        }
    }

    Ok(Mapped {
        mapped_bytes,
        tables_used: next - first_page,
    })
}

/// 清零一张页。
fn clear_page(arena: &mut [u8], offset: usize) {
    for byte in arena[offset..offset + PAGE_SIZE as usize].iter_mut() {
        *byte = 0;
    }
}

/// 读取表项。
fn read_entry_at(arena: &[u8], table_offset: usize, index: usize) -> u64 {
    let offset = table_offset + index * 8;
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&arena[offset..offset + 8]);
    u64::from_le_bytes(bytes)
}

/// 写入表项。
fn write_entry_at(arena: &mut [u8], table_offset: usize, index: usize, value: u64) {
    let offset = table_offset + index * 8;
    arena[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: u64 = PAGE_SIZE;

    /// 4 KiB 对齐的竞技场：前两页当"内核页表"（PML4 + PDPT），后面留给构建器。
    #[repr(align(4096))]
    struct Arena([u8; 32 * 1024]);

    impl Arena {
        fn new() -> Self {
            Self([0u8; 32 * 1024])
        }
        fn bytes(&mut self) -> &mut [u8] {
            &mut self.0
        }
    }

    /// 造一份"内核页表"：PML4（页 0）→ PDPT（页 1）；返回 (pml4 偏移, 第一个空闲偏移)。
    fn kernel_tables(arena: &mut [u8]) -> (usize, usize) {
        let base = arena.as_ptr() as u64;
        write_entry_at(arena, 0, 0, (base + PAGE) | PRESENT | WRITABLE);
        (0, 2 * PAGE as usize)
    }

    #[test]
    fn maps_a_user_region_with_user_pages() {
        let mut arena = Arena::new();
        let arena_ref = arena.bytes();
        let (pml4, free) = kernel_tables(arena_ref);
        let mapping = Mapping {
            vaddr: USER_BASE,
            paddr: 0x0200_0000,
            size: 4 * PAGE,
            writable: false,
        };
        let mapped = map_user_region(arena_ref, pml4, free, &[mapping]).expect("映射成功");
        assert_eq!(mapped.mapped_bytes, 4 * PAGE);

        let base = arena_ref.as_ptr() as u64;
        let pdpt = (read_entry_at(arena_ref, pml4, 0) & ADDRESS_MASK) - base;
        let pd = (read_entry_at(arena_ref, pdpt as usize, 4) & ADDRESS_MASK) - base;
        assert_ne!(pd, 0);
        let pt = (read_entry_at(arena_ref, pd as usize, 0) & ADDRESS_MASK) - base;
        assert_ne!(pt, 0);
        for index in 0..4usize {
            let entry = read_entry_at(arena_ref, pt as usize, index);
            assert_eq!(entry & PRESENT, PRESENT);
            assert_eq!(entry & USER, USER, "用户页必须带 U/S = 1");
            assert_eq!(entry & WRITABLE, 0, "只读映射不得带 RW");
            assert_eq!(entry & ADDRESS_MASK, 0x0200_0000 + index as u64 * PAGE);
        }
    }

    #[test]
    fn writable_mappings_get_rw_and_alignment_is_enforced() {
        let mut arena = Arena::new();
        let arena_ref = arena.bytes();
        let (pml4, free) = kernel_tables(arena_ref);
        let mapping = Mapping {
            vaddr: USER_BASE + PAGE,
            paddr: 0x1000,
            size: PAGE,
            writable: true,
        };
        map_user_region(arena_ref, pml4, free, &[mapping]).expect("映射成功");
        let base = arena_ref.as_ptr() as u64;
        let pdpt = (read_entry_at(arena_ref, pml4, 0) & ADDRESS_MASK) - base;
        let pd = (read_entry_at(arena_ref, pdpt as usize, 4) & ADDRESS_MASK) - base;
        let pt = (read_entry_at(arena_ref, pd as usize, 0) & ADDRESS_MASK) - base;
        let entry = read_entry_at(arena_ref, pt as usize, 1);
        assert_eq!(
            entry & (PRESENT | USER | WRITABLE),
            PRESENT | USER | WRITABLE
        );

        let bad = Mapping {
            vaddr: USER_BASE + 1,
            paddr: 0x1000,
            size: PAGE,
            writable: true,
        };
        assert_eq!(
            map_user_region(arena_ref, pml4, free, &[bad]).unwrap_err(),
            MapError::NotPageAligned {
                value: USER_BASE + 1
            }
        );
        let zero = Mapping {
            vaddr: USER_BASE,
            paddr: 0x1000,
            size: 0,
            writable: true,
        };
        assert_eq!(
            map_user_region(arena_ref, pml4, free, &[zero]).unwrap_err(),
            MapError::NotPageAligned { value: USER_BASE }
        );
    }

    #[test]
    fn mappings_below_user_base_or_too_large_are_refused() {
        let mut arena = Arena::new();
        let arena_ref = arena.bytes();
        let (pml4, free) = kernel_tables(arena_ref);
        let below = Mapping {
            vaddr: USER_BASE - PAGE,
            paddr: 0x1000,
            size: PAGE,
            writable: false,
        };
        assert!(matches!(
            map_user_region(arena_ref, pml4, free, &[below]),
            Err(MapError::BelowUserBase { .. })
        ));
        let huge = Mapping {
            vaddr: USER_BASE,
            paddr: 0x1000,
            size: USER_MAX_BYTES + PAGE,
            writable: false,
        };
        assert!(matches!(
            map_user_region(arena_ref, pml4, free, &[huge]),
            Err(MapError::OutOfRange { .. })
        ));
    }

    #[test]
    fn a_small_arena_is_reported_with_the_exact_need() {
        // 3 页：PML4 + PDPT + 1 页空位，而 PD + PT 需要 2 页。
        let mut arena = Arena::new();
        let arena_ref = &mut arena.bytes()[..3 * PAGE as usize];
        let base = arena_ref.as_ptr() as u64;
        write_entry_at(arena_ref, 0, 0, (base + PAGE) | PRESENT | WRITABLE);
        let mapping = Mapping {
            vaddr: USER_BASE,
            paddr: 0x1000,
            size: PAGE,
            writable: false,
        };
        assert_eq!(
            map_user_region(arena_ref, 0, 2 * PAGE as usize, &[mapping]).unwrap_err(),
            MapError::ArenaTooSmall { needed: 4, got: 3 }
        );
    }

    #[test]
    fn mapping_into_the_second_two_mib_uses_a_second_page_table() {
        let mut arena = Arena::new();
        let arena_ref = arena.bytes();
        let (pml4, free) = kernel_tables(arena_ref);
        let mapping = Mapping {
            vaddr: USER_BASE + 2 * 1024 * 1024,
            paddr: 0x0400_0000,
            size: PAGE,
            writable: true,
        };
        let mapped = map_user_region(arena_ref, pml4, free, &[mapping]).expect("映射成功");
        assert_eq!(mapped.tables_used, 3, "PD + 两张 PT（槽位 0 与 1）");
        let base = arena_ref.as_ptr() as u64;
        let pdpt = (read_entry_at(arena_ref, pml4, 0) & ADDRESS_MASK) - base;
        let pd = (read_entry_at(arena_ref, pdpt as usize, 4) & ADDRESS_MASK) - base;
        let pt1 = (read_entry_at(arena_ref, pd as usize, 1) & ADDRESS_MASK) - base;
        assert_ne!(pt1, 0, "第二个 2 MiB 需要自己的 PT");
        assert_eq!(
            read_entry_at(arena_ref, pt1 as usize, 0) & ADDRESS_MASK,
            0x0400_0000
        );
    }

    #[test]
    fn every_level_of_the_chain_carries_the_user_bit() {
        // 少了任何一级，CPL 3 的取指就会以"保护违例"失败（真机实测踩过）。
        let mut arena = Arena::new();
        let arena_ref = arena.bytes();
        let (pml4, free) = kernel_tables(arena_ref);
        let mapping = Mapping {
            vaddr: USER_BASE,
            paddr: 0x0200_0000,
            size: PAGE,
            writable: true,
        };
        map_user_region(arena_ref, pml4, free, &[mapping]).expect("映射成功");
        let base = arena_ref.as_ptr() as u64;
        let pml4_entry = read_entry_at(arena_ref, pml4, 0);
        assert_eq!(pml4_entry & USER, USER, "PML4 项必须带 U/S = 1");
        let pdpt = (pml4_entry & ADDRESS_MASK) - base;
        let pdpt_entry = read_entry_at(arena_ref, pdpt as usize, 4);
        assert_eq!(pdpt_entry & USER, USER, "PDPT 项必须带 U/S = 1");
        let pd = (pdpt_entry & ADDRESS_MASK) - base;
        let pd_entry = read_entry_at(arena_ref, pd as usize, 0);
        assert_eq!(pd_entry & USER, USER, "PD 项必须带 U/S = 1");
        let pt = (pd_entry & ADDRESS_MASK) - base;
        assert_eq!(
            read_entry_at(arena_ref, pt as usize, 0) & USER,
            USER,
            "PT 叶项必须带 U/S = 1"
        );
    }

    #[test]
    fn user_base_is_the_identity_map_limit() {
        assert_eq!(USER_BASE, crate::paging::MAX_IDENTITY_BYTES);
        assert_eq!((USER_BASE >> 30) & 0x1FF, 4, "4 GiB 落在 PDPT 的第 4 项");
    }
}
