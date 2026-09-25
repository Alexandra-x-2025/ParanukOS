//! UEFI 内存图的内核侧解析。
//!
//! 依据 `docs/architecture/memory_subsystem.md` §4。三条硬规则：
//!
//! 1. **必须按 `BootInfo.mmap_desc_size` 给出的步长遍历**。标准 `EFI_MEMORY_DESCRIPTOR`
//!    是 40 字节，但固件允许返回更大的描述符——QEMU 8.2 的 OVMF 实际返回 48。按 40 遍历
//!    会让每一条都错位。
//! 2. **未知类型视为非 RAM**。未来的固件不能让我们映射看不懂的东西。
//! 3. **`base + pages * 4096` 溢出即判内存图损坏**，而不是静默截断。
//!
//! 本模块不构造裸指针切片：内核自己用 [`buffer_len`] 算出长度后创建切片再交给
//! [`MemoryMap::from_bytes`]，因此这里可以 `#![forbid(unsafe_code)]`。

use core::fmt;

/// 页大小：4 KiB。
pub const PAGE_SIZE: u64 = 4096;

/// 标准 `EFI_MEMORY_DESCRIPTOR` 的长度（字节）。实际步长可能更大。
pub const DESCRIPTOR_MIN_SIZE: u32 = 40;

/// 描述符内的字段偏移（见 UEFI 规范 `EFI_MEMORY_DESCRIPTOR`）。
const OFF_TYPE: usize = 0;
const OFF_PHYSICAL_START: usize = 8;
const OFF_NUMBER_OF_PAGES: usize = 24;

/// `EFI_MEMORY_TYPE` 的内核侧映射。
///
/// 未知取值保留原始编号（[`MemoryKind::Unknown`]），并且**不算 RAM**。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemoryKind {
    /// 0 —— 固件保留，不得使用。
    Reserved,
    /// 1 —— 引导器代码。
    LoaderCode,
    /// 2 —— 引导器数据。
    LoaderData,
    /// 3 —— Boot Services 代码。
    BootServicesCode,
    /// 4 —— Boot Services 数据。
    BootServicesData,
    /// 5 —— Runtime Services 代码。
    RuntimeServicesCode,
    /// 6 —— Runtime Services 数据。
    RuntimeServicesData,
    /// 7 —— 常规可用内存。
    Conventional,
    /// 8 —— 固件判定不可用。
    Unusable,
    /// 9 —— ACPI 可回收内存。
    AcpiReclaim,
    /// 10 —— ACPI NVS。
    AcpiNvs,
    /// 11 —— 内存映射 I/O。
    Mmio,
    /// 12 —— 内存映射 I/O 端口空间。
    MmioPortSpace,
    /// 13 —— PAL 代码（Itanium 遗留）。
    PalCode,
    /// 14 —— 持久内存。
    Persistent,
    /// 15 —— 尚未接受的加密内存（UEFI 2.9）。
    Unaccepted,
    /// 本内核不认识的类型：按非 RAM 处理。
    Unknown(u32),
}

impl MemoryKind {
    /// 由 `EFI_MEMORY_TYPE` 原始值构造。
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        match raw {
            0 => Self::Reserved,
            1 => Self::LoaderCode,
            2 => Self::LoaderData,
            3 => Self::BootServicesCode,
            4 => Self::BootServicesData,
            5 => Self::RuntimeServicesCode,
            6 => Self::RuntimeServicesData,
            7 => Self::Conventional,
            8 => Self::Unusable,
            9 => Self::AcpiReclaim,
            10 => Self::AcpiNvs,
            11 => Self::Mmio,
            12 => Self::MmioPortSpace,
            13 => Self::PalCode,
            14 => Self::Persistent,
            15 => Self::Unaccepted,
            other => Self::Unknown(other),
        }
    }

    /// 原始 `EFI_MEMORY_TYPE` 值。
    #[must_use]
    pub const fn raw(self) -> u32 {
        match self {
            Self::Reserved => 0,
            Self::LoaderCode => 1,
            Self::LoaderData => 2,
            Self::BootServicesCode => 3,
            Self::BootServicesData => 4,
            Self::RuntimeServicesCode => 5,
            Self::RuntimeServicesData => 6,
            Self::Conventional => 7,
            Self::Unusable => 8,
            Self::AcpiReclaim => 9,
            Self::AcpiNvs => 10,
            Self::Mmio => 11,
            Self::MmioPortSpace => 12,
            Self::PalCode => 13,
            Self::Persistent => 14,
            Self::Unaccepted => 15,
            Self::Unknown(other) => other,
        }
    }

    /// 该类型是否属于「可以按普通内存映射」的 RAM。
    ///
    /// 覆盖 Loader*、BootServices*、RuntimeServices*、Conventional、ACPI 可回收与 ACPI NVS
    /// （见 `memory_subsystem.md` §3.4）。**不含** `Reserved`：那是固件保留区，映射它没有
    /// 任何好处，不映射还能让误访问大声故障。
    #[must_use]
    pub const fn is_ram(self) -> bool {
        matches!(
            self,
            Self::LoaderCode
                | Self::LoaderData
                | Self::BootServicesCode
                | Self::BootServicesData
                | Self::RuntimeServicesCode
                | Self::RuntimeServicesData
                | Self::Conventional
                | Self::AcpiReclaim
                | Self::AcpiNvs
        )
    }
}

/// 内存图中的一条区域。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Region {
    /// 内存类型。
    pub kind: MemoryKind,
    /// 物理起始地址。
    pub base: u64,
    /// 页数（4 KiB 页）。
    pub pages: u64,
}

impl Region {
    /// 区域的结束地址（不含）。
    ///
    /// [`MemoryMap::from_bytes`] 已确认 `base + pages * PAGE_SIZE` 不溢出，
    /// 因此这里的算术不会回绕。
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.base + self.pages * PAGE_SIZE
    }

    /// 区域字节长度。
    #[must_use]
    pub const fn byte_len(&self) -> u64 {
        self.pages * PAGE_SIZE
    }

    /// 区间 `[start, end)` 是否与本区域相交（半开区间语义）。
    #[must_use]
    pub const fn overlaps(&self, start: u64, end: u64) -> bool {
        self.base < end && start < self.end()
    }
}

/// 内存图解析失败的原因。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MapError {
    /// 描述符条数为 0：没有可用的内存图。
    Empty,
    /// 步长小于标准描述符长度。
    StrideTooSmall(u32),
    /// 提供（或需要）的缓冲区小于 `条数 × 步长`。
    TooShort {
        /// 需要的字节数。
        needed: usize,
        /// 实际可用的字节数。
        got: usize,
    },
    /// 描述符条数乘以步长超出 `usize` 可表示范围。
    TooLarge {
        /// 描述符条数。
        entries: u64,
        /// 步长（字节）。
        stride: u32,
    },
    /// 第 `index` 条描述符的结束地址溢出 `u64`：内存图已损坏。
    AddressOverflow {
        /// 描述符下标。
        index: usize,
    },
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "内存图为空（0 条描述符）"),
            Self::StrideTooSmall(stride) => write!(
                f,
                "内存描述符步长 {stride} 小于标准长度 {DESCRIPTOR_MIN_SIZE}"
            ),
            Self::TooShort { needed, got } => {
                write!(f, "内存图缓冲区过小：需要 {needed} 字节，实际 {got} 字节")
            }
            Self::TooLarge { entries, stride } => {
                write!(f, "内存图过大：{entries} 条 × {stride} 字节超出地址空间")
            }
            Self::AddressOverflow { index } => {
                write!(f, "第 {index} 条内存描述符的结束地址溢出")
            }
        }
    }
}

/// 计算内存图缓冲区所需的字节数；`entries × stride` 溢出 `usize` 时返回 `None`。
///
/// 内核用它在 `unsafe { slice::from_raw_parts }` 之前算出长度，从而让本 crate 无需 `unsafe`。
#[must_use]
pub fn buffer_len(entries: u64, stride: u32) -> Option<usize> {
    if stride < DESCRIPTOR_MIN_SIZE {
        return None;
    }
    let total = entries.checked_mul(u64::from(stride))?;
    usize::try_from(total).ok()
}

/// 已解析且已校验的 UEFI 内存图。
///
/// 构造时一次性校验所有描述符（步长、边界、地址溢出），因此 [`MemoryMap::iter`] 不会失败。
#[derive(Clone, Copy, Debug)]
pub struct MemoryMap<'a> {
    bytes: &'a [u8],
    entries: usize,
    stride: usize,
    version: u32,
}

impl<'a> MemoryMap<'a> {
    /// 从原始描述符字节构造。
    ///
    /// * `entries` 为描述符条数，`stride` 为 `BootInfo.mmap_desc_size`（**必须**用它，
    ///   而不是 `size_of::<EfiMemoryDescriptor>()`），`version` 为 `mmap_desc_ver`。
    ///
    /// # Errors
    /// 见 [`MapError`]。
    pub fn from_bytes(
        bytes: &'a [u8],
        entries: u64,
        stride: u32,
        version: u32,
    ) -> Result<Self, MapError> {
        if entries == 0 {
            return Err(MapError::Empty);
        }
        if stride < DESCRIPTOR_MIN_SIZE {
            return Err(MapError::StrideTooSmall(stride));
        }
        let needed = buffer_len(entries, stride).ok_or(MapError::TooLarge { entries, stride })?;
        if bytes.len() < needed {
            return Err(MapError::TooShort {
                needed,
                got: bytes.len(),
            });
        }
        let entries = needed / stride as usize;
        let map = Self {
            bytes: &bytes[..needed],
            entries,
            stride: stride as usize,
            version,
        };
        // 逐条确认结束地址不溢出：宁可在这里判内存图损坏，也不要之后用回绕的地址去建映射。
        for index in 0..entries {
            let region = map.get(index);
            region
                .pages
                .checked_mul(PAGE_SIZE)
                .and_then(|len| region.base.checked_add(len))
                .ok_or(MapError::AddressOverflow { index })?;
        }
        Ok(map)
    }

    /// 描述符条数。
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries
    }

    /// 是否没有描述符（`from_bytes` 拒绝空图，因此恒为 `false`）。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries == 0
    }

    /// 描述符步长（字节）。
    #[must_use]
    pub const fn stride(&self) -> u32 {
        self.stride as u32
    }

    /// `EFI_MEMORY_DESCRIPTOR` 版本。
    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// 读取第 `index` 条描述符。`index` 越界时返回一条空区域。
    #[must_use]
    pub fn get(&self, index: usize) -> Region {
        let base_offset = index.saturating_mul(self.stride);
        Region {
            kind: MemoryKind::from_raw(read_u32(self.bytes, base_offset + OFF_TYPE)),
            base: read_u64(self.bytes, base_offset + OFF_PHYSICAL_START),
            pages: read_u64(self.bytes, base_offset + OFF_NUMBER_OF_PAGES),
        }
    }

    /// 遍历所有描述符。
    #[must_use]
    pub const fn iter(&self) -> Regions<'a> {
        Regions {
            bytes: self.bytes,
            entries: self.entries,
            stride: self.stride,
            index: 0,
        }
    }

    /// 所有 RAM 区域中最高的结束地址；没有 RAM 时返回 `None`。
    #[must_use]
    pub fn ram_top(&self) -> Option<u64> {
        let mut top: Option<u64> = None;
        for region in self.iter() {
            if !region.kind.is_ram() {
                continue;
            }
            let end = region.end();
            top = Some(match top {
                Some(current) if current >= end => current,
                _ => end,
            });
        }
        top
    }
}

/// [`MemoryMap::iter`] 的迭代器。
#[derive(Clone, Copy, Debug)]
pub struct Regions<'a> {
    bytes: &'a [u8],
    entries: usize,
    stride: usize,
    index: usize,
}

impl Iterator for Regions<'_> {
    type Item = Region;

    fn next(&mut self) -> Option<Region> {
        if self.index >= self.entries {
            return None;
        }
        let base_offset = self.index * self.stride;
        self.index += 1;
        Some(Region {
            kind: MemoryKind::from_raw(read_u32(self.bytes, base_offset + OFF_TYPE)),
            base: read_u64(self.bytes, base_offset + OFF_PHYSICAL_START),
            pages: read_u64(self.bytes, base_offset + OFF_NUMBER_OF_PAGES),
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.entries - self.index;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for Regions<'_> {}

/// 读取小端 `u32`；越界时返回 0（`from_bytes` 已保证不会越界）。
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    let mut buf = [0u8; 4];
    if let Some(end) = offset.checked_add(4)
        && let Some(src) = bytes.get(offset..end)
    {
        buf.copy_from_slice(src);
    }
    u32::from_le_bytes(buf)
}

/// 读取小端 `u64`；越界时返回 0（`from_bytes` 已保证不会越界）。
fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    let mut buf = [0u8; 8];
    if let Some(end) = offset.checked_add(8)
        && let Some(src) = bytes.get(offset..end)
    {
        buf.copy_from_slice(src);
    }
    u64::from_le_bytes(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按给定的步长编码一批描述符，未使用的填充字节填 `0xAA`。
    ///
    /// 填充非零是刻意的：如果实现错误地按 40 字节步长读取 48 字节描述符，
    /// 或者误读了填充，测试会立刻发现。
    fn encode(regions: &[(u32, u64, u64)], stride: usize) -> Vec<u8> {
        let mut bytes = vec![0xAAu8; regions.len() * stride];
        for (index, (kind, base, pages)) in regions.iter().enumerate() {
            let offset = index * stride;
            bytes[offset..offset + 4].copy_from_slice(&kind.to_le_bytes());
            bytes[offset + 8..offset + 16].copy_from_slice(&base.to_le_bytes());
            bytes[offset + 24..offset + 32].copy_from_slice(&pages.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn parses_with_the_ovmf_stride_of_48() {
        // OVMF/QEMU 8.2 实测步长 48，而标准结构只有 40 字节。
        let bytes = encode(
            &[(7, 0x1000, 0x9F), (2, 0x100000, 8), (0, 0xFE000000, 1)],
            48,
        );
        let map = MemoryMap::from_bytes(&bytes, 3, 48, 2).expect("内存图应可解析");
        assert_eq!(map.len(), 3);
        assert_eq!(map.stride(), 48);
        assert_eq!(map.version(), 2);

        let regions: Vec<_> = map.iter().collect();
        assert_eq!(
            regions[0],
            Region {
                kind: MemoryKind::Conventional,
                base: 0x1000,
                pages: 0x9F
            }
        );
        assert_eq!(regions[0].end(), 0x1000 + 0x9F * PAGE_SIZE);
        assert_eq!(regions[1].kind, MemoryKind::LoaderData);
        assert_eq!(regions[1].base, 0x100000);
        assert_eq!(regions[2].kind, MemoryKind::Reserved);
        assert_eq!(regions[2].base, 0xFE00_0000);
    }

    #[test]
    fn stride_40_parses_only_when_the_data_really_uses_it() {
        // 同样的两条描述符，一条按 48 字节步长编码（填充取 0，便于展示错位结果）。
        // 若按 40 字节遍历，第二条会从填充里读到 type=0、base=2 —— 地址全错。
        let mut bytes = vec![0u8; 2 * 48];
        bytes[0..4].copy_from_slice(&7u32.to_le_bytes());
        bytes[8..16].copy_from_slice(&0x1000u64.to_le_bytes());
        bytes[24..32].copy_from_slice(&0x9Fu64.to_le_bytes());
        bytes[48..52].copy_from_slice(&2u32.to_le_bytes());
        bytes[56..64].copy_from_slice(&0x200000u64.to_le_bytes());
        bytes[72..80].copy_from_slice(&0x100u64.to_le_bytes());

        let wrong = MemoryMap::from_bytes(&bytes, 2, 40, 2).expect("步长 40 本身合法");
        assert_ne!(
            wrong.get(1).base,
            0x200000,
            "按 40 解析 48 字节描述符必然错位"
        );
        assert_ne!(wrong.get(1).kind, MemoryKind::LoaderData);

        let right = MemoryMap::from_bytes(&bytes, 2, 48, 2).expect("按真实步长解析");
        assert_eq!(right.get(1).base, 0x200000);
        assert_eq!(right.get(1).pages, 0x100);
        assert_eq!(right.get(1).kind, MemoryKind::LoaderData);
    }

    #[test]
    fn rejects_empty_and_short_strides() {
        let bytes = encode(&[(7, 0, 1)], 40);
        assert_eq!(
            MemoryMap::from_bytes(&bytes, 0, 40, 0).unwrap_err(),
            MapError::Empty
        );
        assert_eq!(
            MemoryMap::from_bytes(&bytes, 1, 39, 0).unwrap_err(),
            MapError::StrideTooSmall(39)
        );
        assert_eq!(
            MemoryMap::from_bytes(&bytes, 1, 0, 0).unwrap_err(),
            MapError::StrideTooSmall(0)
        );
        assert_eq!(
            MemoryMap::from_bytes(&bytes, u64::MAX, 40, 0).unwrap_err(),
            MapError::TooLarge {
                entries: u64::MAX,
                stride: 40
            }
        );
    }

    #[test]
    fn rejects_a_truncated_buffer() {
        let bytes = encode(&[(7, 0, 1)], 40);
        assert_eq!(
            MemoryMap::from_bytes(&bytes[..32], 1, 40, 0).unwrap_err(),
            MapError::TooShort {
                needed: 40,
                got: 32
            }
        );
    }

    #[test]
    fn rejects_an_overflowing_descriptor() {
        let bytes = encode(&[(7, 0, 1), (7, u64::MAX - PAGE_SIZE + 1, 4)], 40);
        assert_eq!(
            MemoryMap::from_bytes(&bytes, 2, 40, 0).unwrap_err(),
            MapError::AddressOverflow { index: 1 }
        );
    }

    #[test]
    fn unknown_types_are_not_ram() {
        let kind = MemoryKind::from_raw(200);
        assert_eq!(kind, MemoryKind::Unknown(200));
        assert_eq!(kind.raw(), 200);
        assert!(!kind.is_ram());

        let bytes = encode(&[(200, 0x1000, 4)], 40);
        let map = MemoryMap::from_bytes(&bytes, 1, 40, 0).expect("未知类型不应导致解析失败");
        assert_eq!(map.get(0).kind, MemoryKind::Unknown(200));
        assert_eq!(map.ram_top(), None);
    }

    #[test]
    fn is_ram_covers_exactly_the_documented_types() {
        // 与 memory_subsystem.md §3.4 的 is_ram 列表一致；改了这里就必须同时改文档。
        let ram = [1u32, 2, 3, 4, 5, 6, 7, 9, 10];
        for raw in 0..=15 {
            let expected = ram.contains(&raw);
            assert_eq!(
                MemoryKind::from_raw(raw).is_ram(),
                expected,
                "type {raw} 的 is_ram 判定与文档不一致"
            );
            assert_eq!(MemoryKind::from_raw(raw).raw(), raw, "raw 往返必须一致");
        }
    }

    #[test]
    fn ram_top_takes_the_highest_ram_region_and_ignores_non_ram() {
        let bytes = encode(
            &[
                (7, 0x1000, 0x9F),
                (0, 0x100000, 0x1000),  // 保留区更高，但不算 RAM
                (7, 0x200000, 0x100),   // 1 MiB
                (11, 0xFEE00000, 0x10), // MMIO 更高，不算 RAM
            ],
            48,
        );
        let map = MemoryMap::from_bytes(&bytes, 4, 48, 2).expect("解析成功");
        assert_eq!(map.ram_top(), Some(0x200000 + 0x100 * PAGE_SIZE));
    }

    #[test]
    fn ram_top_is_none_without_ram() {
        let bytes = encode(&[(0, 0x1000, 1), (11, 0x2000, 1)], 40);
        let map = MemoryMap::from_bytes(&bytes, 2, 40, 0).expect("解析成功");
        assert_eq!(map.ram_top(), None);
    }

    #[test]
    fn buffer_len_checks_overflow_and_stride() {
        assert_eq!(buffer_len(3, 48), Some(144));
        assert_eq!(buffer_len(0, 48), Some(0));
        assert_eq!(buffer_len(3, 39), None);
        assert_eq!(buffer_len(u64::MAX, 40), None);
    }

    #[test]
    fn region_overlap_is_half_open() {
        let region = Region {
            kind: MemoryKind::Conventional,
            base: 0x1000,
            pages: 1,
        };
        assert!(region.overlaps(0, 0x1001));
        assert!(region.overlaps(0x1000, 0x2000));
        assert!(region.overlaps(0x1FFF, 0x3000));
        assert!(!region.overlaps(0, 0x1000), "左端相接不算相交");
        assert!(!region.overlaps(0x2000, 0x3000), "右端相接不算相交");
    }

    #[test]
    fn iterator_reports_exact_size() {
        let bytes = encode(&[(7, 0, 1), (7, 0x1000, 1), (7, 0x2000, 1)], 40);
        let map = MemoryMap::from_bytes(&bytes, 3, 40, 0).expect("解析成功");
        assert_eq!(map.iter().len(), 3);
        assert_eq!(map.iter().count(), 3);
    }
}
