//! 校验 ParanukOS 内核镜像：ELF64 / 小端 / x86-64 / `ET_EXEC`，并取出入口点。
//!
//! 该 crate **零依赖**，因此可以在宿主平台上直接用
//! `cargo test -p kernel-image --target x86_64-unknown-linux-gnu` 做单元测试；
//! 而 UEFI 引导器本身因为 `std` 测试框架与 UEFI panic 处理器冲突
//! （重复的 `panic_impl` lang item），必须设置 `[[bin]] test = false`。
//!
//! 之所以只接受 `ET_EXEC`：当前引导器的做法是"把字节复制到新分配的页再跳转"，
//! 不做重定位，因此 PIE（`ET_DYN`）镜像必须被拒绝，而不是被错误地接受。

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use core::fmt;

/// ELF64 头部最小长度。
pub const ELF_HEADER_MIN_LEN: usize = 64;

/// ELF 头中的字段偏移（见 ELF 规范）。
const EI_CLASS: usize = 4;
const EI_DATA: usize = 5;
const E_TYPE_OFFSET: usize = 16;
const E_MACHINE_OFFSET: usize = 18;
const E_ENTRY_OFFSET: usize = 24;

const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
/// 可执行文件（非 PIE）。
pub const ET_EXEC: u16 = 2;
/// `EM_X86_64`
pub const EM_X86_64: u16 = 62;

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

/// 镜像校验失败的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageError {
    /// 数据长度不足以容纳完整的 ELF 头。
    TooSmall(usize),
    /// 缺少 ELF magic（`\x7fELF`）。
    NotElf,
    /// 不是 64 位 ELF。
    NotElf64,
    /// 不是小端序 ELF。
    NotLittleEndian,
    /// `e_type` 不是 `ET_EXEC`（例如 PIE 的 `ET_DYN`）。
    NotExecutable(u16),
    /// `e_machine` 不是 x86-64。
    UnsupportedMachine(u16),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooSmall(len) => {
                write!(f, "镜像过小（{len} 字节），不足以容纳 ELF 头")
            }
            Self::NotElf => write!(f, "缺少 ELF magic"),
            Self::NotElf64 => write!(f, "不是 64 位 ELF"),
            Self::NotLittleEndian => write!(f, "不是小端序 ELF"),
            Self::NotExecutable(e_type) => {
                write!(
                    f,
                    "e_type = {e_type}，期望 ET_EXEC({ET_EXEC})（PIE/ET_DYN 需要重定位，不支持）"
                )
            }
            Self::UnsupportedMachine(machine) => {
                write!(f, "e_machine = {machine}，期望 x86-64({EM_X86_64})")
            }
        }
    }
}

fn read_u16(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

fn read_u64(data: &[u8], offset: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&data[offset..offset + 8]);
    u64::from_le_bytes(raw)
}

/// 校验 `data` 是否是可被引导器直接载入并跳转的 ELF64/x86-64 镜像。
///
/// 成功时返回 ELF 头中声明的入口点虚拟地址。
pub fn validate(data: &[u8]) -> Result<u64, ImageError> {
    if data.len() < ELF_HEADER_MIN_LEN {
        return Err(ImageError::TooSmall(data.len()));
    }
    if data[0..4] != ELF_MAGIC {
        return Err(ImageError::NotElf);
    }
    if data[EI_CLASS] != ELFCLASS64 {
        return Err(ImageError::NotElf64);
    }
    if data[EI_DATA] != ELFDATA2LSB {
        return Err(ImageError::NotLittleEndian);
    }

    let e_type = read_u16(data, E_TYPE_OFFSET);
    if e_type != ET_EXEC {
        return Err(ImageError::NotExecutable(e_type));
    }

    let e_machine = read_u16(data, E_MACHINE_OFFSET);
    if e_machine != EM_X86_64 {
        return Err(ImageError::UnsupportedMachine(e_machine));
    }

    Ok(read_u64(data, E_ENTRY_OFFSET))
}

// ---------------------------------------------------------------------------
// Program Header 解析（装载规则见 docs/architecture/kernel_interface.md §3.2）
// ---------------------------------------------------------------------------

/// `p_flags` 中的可执行位。
pub const PF_X: u32 = 1;
/// `p_flags` 中的可写位。
pub const PF_W: u32 = 2;
/// `p_flags` 中的可读位。
pub const PF_R: u32 = 4;

/// 页大小（x86-64 的 4 KiB 页）。
pub const PAGE_SIZE: u64 = 4096;

/// ELF64 Program Header 的固定长度。
pub const PHDR_LEN: usize = 56;

const PT_LOAD: u32 = 1;

const P_TYPE_OFFSET: usize = 0;
const P_FLAGS_OFFSET: usize = 4;
const P_OFFSET_OFFSET: usize = 8;
const P_PADDR_OFFSET: usize = 24;
const P_FILESZ_OFFSET: usize = 32;
const P_MEMSZ_OFFSET: usize = 40;

const E_PHOFF_OFFSET: usize = 32;
const E_PHENTSIZE_OFFSET: usize = 54;
const E_PHNUM_OFFSET: usize = 56;

/// 一个需要被装载的段（`PT_LOAD`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// 数据在文件中的偏移（`p_offset`）。
    pub offset: u64,
    /// 装载到的**物理**地址（`p_paddr`）。
    pub paddr: u64,
    /// 文件中有多少字节（`p_filesz`）。
    pub filesz: u64,
    /// 装载后占多少字节（`p_memsz`，通常大于 `filesz`，差额为 BSS）。
    pub memsz: u64,
    /// 段权限（`p_flags`）。
    pub flags: u32,
}

impl Segment {
    /// 该段必须占用的**页对齐**区间 `[start, end)`。
    ///
    /// 注意 `p_paddr` 不保证页对齐（链接器只保证 `p_vaddr ≡ p_offset (mod p_align)`），
    /// 因此不能直接用它去申请页，而要按本函数给出的对齐区间申请，再按 `paddr` 写入。
    #[must_use]
    pub fn page_range(&self) -> (u64, u64) {
        let start = self.paddr & !(PAGE_SIZE - 1);
        let end = (self.paddr + self.memsz).div_ceil(PAGE_SIZE) * PAGE_SIZE;
        (start, end)
    }

    /// 段是否可执行。
    #[must_use]
    pub fn is_executable(&self) -> bool {
        self.flags & PF_X != 0
    }
}

/// `Segment` 上的不可恢复错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentError {
    /// Program Header 表越界或长度不足。
    BadProgramHeaders,
    /// `p_filesz > p_memsz`。
    FileszLargerThanMemsz,
    /// 段的数据超出文件长度。
    DataOutOfBounds,
    /// 段地址范围溢出。
    AddressOverflow,
    /// 段长度为 0。
    EmptySegment,
    /// 两个段的字节区间重叠。
    OverlappingSegments,
    /// 入口点不在任何可执行段内。
    EntryNotExecutable,
}

impl fmt::Display for SegmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadProgramHeaders => write!(f, "Program Header 表越界或不完整"),
            Self::FileszLargerThanMemsz => write!(f, "p_filesz 大于 p_memsz"),
            Self::DataOutOfBounds => write!(f, "段数据超出文件长度"),
            Self::AddressOverflow => write!(f, "段地址范围溢出"),
            Self::EmptySegment => write!(f, "段长度为 0"),
            Self::OverlappingSegments => write!(f, "两个段的区间重叠"),
            Self::EntryNotExecutable => write!(f, "入口点不在任何可执行段内"),
        }
    }
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

/// `PT_LOAD` 段的迭代器（不分配内存，便于在 `no_std` 引导器里使用）。
pub struct SegmentIter<'a> {
    data: &'a [u8],
    table_offset: u64,
    entry_size: u16,
    remaining: u16,
    index: u16,
}

impl Iterator for SegmentIter<'_> {
    type Item = Result<Segment, SegmentError>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.remaining > 0 {
            let index = self.index;
            self.index += 1;
            self.remaining -= 1;

            let entry = self.table_offset + u64::from(index) * u64::from(self.entry_size);
            let entry = usize::try_from(entry).ok()?;
            // 表本身越界 → 直接报错并结束迭代
            let end = entry.checked_add(PHDR_LEN)?;
            if end > self.data.len() {
                return Some(Err(SegmentError::BadProgramHeaders));
            }
            let entry_data = &self.data[entry..end];

            if read_u32(entry_data, P_TYPE_OFFSET) != PT_LOAD {
                continue;
            }

            let segment = Segment {
                offset: read_u64(entry_data, P_OFFSET_OFFSET),
                paddr: read_u64(entry_data, P_PADDR_OFFSET),
                filesz: read_u64(entry_data, P_FILESZ_OFFSET),
                memsz: read_u64(entry_data, P_MEMSZ_OFFSET),
                flags: read_u32(entry_data, P_FLAGS_OFFSET),
            };
            return Some(Ok(segment));
        }
        None
    }
}

/// 构造 `PT_LOAD` 段迭代器。`data` 必须是已通过 [`validate`] 的镜像。
pub fn segments(data: &[u8]) -> Result<SegmentIter<'_>, SegmentError> {
    if data.len() < ELF_HEADER_MIN_LEN {
        return Err(SegmentError::BadProgramHeaders);
    }
    let table_offset = read_u64(data, E_PHOFF_OFFSET);
    let entry_size = u16::from_le_bytes([data[E_PHENTSIZE_OFFSET], data[E_PHENTSIZE_OFFSET + 1]]);
    let count = u16::from_le_bytes([data[E_PHNUM_OFFSET], data[E_PHNUM_OFFSET + 1]]);

    if count == 0 || usize::from(entry_size) < PHDR_LEN {
        return Err(SegmentError::BadProgramHeaders);
    }

    Ok(SegmentIter {
        data,
        table_offset,
        entry_size,
        remaining: count,
        index: 0,
    })
}

/// 逐个检查段：长度、越界、溢出。返回段数量。
///
/// **不检查重叠**（那需要两两比较，交给 [`check_no_overlap`]）。
pub fn check_segments(data: &[u8]) -> Result<usize, SegmentError> {
    let mut count = 0usize;
    for segment in segments(data)? {
        let segment = segment?;
        if segment.memsz == 0 {
            return Err(SegmentError::EmptySegment);
        }
        if segment.filesz > segment.memsz {
            return Err(SegmentError::FileszLargerThanMemsz);
        }
        let file_end = segment
            .offset
            .checked_add(segment.filesz)
            .ok_or(SegmentError::AddressOverflow)?;
        if file_end > data.len() as u64 {
            return Err(SegmentError::DataOutOfBounds);
        }
        segment
            .paddr
            .checked_add(segment.memsz)
            .ok_or(SegmentError::AddressOverflow)?;
        count += 1;
    }
    if count == 0 {
        return Err(SegmentError::BadProgramHeaders);
    }
    Ok(count)
}

/// 校验所有 `PT_LOAD` 段的字节区间两两不重叠。
pub fn check_no_overlap(data: &[u8]) -> Result<(), SegmentError> {
    let mut ranges: [(u64, u64); 16] = [(0, 0); 16];

    for (len, segment) in segments(data)?.enumerate() {
        let segment = segment?;
        let start = segment.paddr;
        let end = segment
            .paddr
            .checked_add(segment.memsz)
            .ok_or(SegmentError::AddressOverflow)?;
        for (other_start, other_end) in &ranges[..len] {
            if start < *other_end && *other_start < end {
                return Err(SegmentError::OverlappingSegments);
            }
        }
        if len == ranges.len() {
            return Err(SegmentError::BadProgramHeaders);
        }
        ranges[len] = (start, end);
    }
    Ok(())
}

/// 校验入口点落在某个可执行段内。
pub fn check_entry(data: &[u8], entry: u64) -> Result<(), SegmentError> {
    for segment in segments(data)? {
        let segment = segment?;
        let end = segment
            .paddr
            .checked_add(segment.memsz)
            .ok_or(SegmentError::AddressOverflow)?;
        if segment.is_executable() && (segment.paddr..end).contains(&entry) {
            return Ok(());
        }
    }
    Err(SegmentError::EntryNotExecutable)
}

/// 所有段覆盖的页对齐总区间 `[start, end)`，用于向内核报告镜像范围。
pub fn loaded_span(data: &[u8]) -> Result<(u64, u64), SegmentError> {
    let mut span: Option<(u64, u64)> = None;
    for segment in segments(data)? {
        let segment = segment?;
        let (start, end) = segment.page_range();
        span = Some(match span {
            None => (start, end),
            Some((lo, hi)) => (lo.min(start), hi.max(end)),
        });
    }
    span.ok_or(SegmentError::BadProgramHeaders)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 仅测试构造 Program Header 时需要（库本身不读 p_vaddr）。
    const P_VADDR_OFFSET: usize = 16;

    const ENTRY: u64 = 0x0000_0000_0020_1190;

    /// 构造一个最小的合法 ELF64/x86-64/ET_EXEC 头部。
    fn valid_header() -> [u8; ELF_HEADER_MIN_LEN] {
        let mut header = [0u8; ELF_HEADER_MIN_LEN];
        header[0..4].copy_from_slice(&ELF_MAGIC);
        header[EI_CLASS] = ELFCLASS64;
        header[EI_DATA] = ELFDATA2LSB;
        header[E_TYPE_OFFSET..E_TYPE_OFFSET + 2].copy_from_slice(&ET_EXEC.to_le_bytes());
        header[E_MACHINE_OFFSET..E_MACHINE_OFFSET + 2].copy_from_slice(&EM_X86_64.to_le_bytes());
        header[E_ENTRY_OFFSET..E_ENTRY_OFFSET + 8].copy_from_slice(&ENTRY.to_le_bytes());
        header
    }

    #[test]
    fn accepts_valid_image_and_returns_entry_point() {
        assert_eq!(validate(&valid_header()), Ok(ENTRY));
    }

    #[test]
    fn accepts_longer_image_with_trailing_sections() {
        let mut data = valid_header().to_vec();
        data.extend_from_slice(&[0xAA; 4096]);
        assert_eq!(validate(&data), Ok(ENTRY));
    }

    #[test]
    fn rejects_too_small_input() {
        let data = [0u8; ELF_HEADER_MIN_LEN - 1];
        assert_eq!(
            validate(&data),
            Err(ImageError::TooSmall(ELF_HEADER_MIN_LEN - 1))
        );
        assert_eq!(validate(&[]), Err(ImageError::TooSmall(0)));
    }

    #[test]
    fn accepts_exact_header_length() {
        // 边界：恰好 64 字节必须通过
        assert!(validate(&valid_header()).is_ok());
    }

    #[test]
    fn rejects_missing_magic() {
        let mut header = valid_header();
        header[1] = b'X';
        assert_eq!(validate(&header), Err(ImageError::NotElf));
    }

    #[test]
    fn rejects_non_64_bit_image() {
        let mut header = valid_header();
        header[EI_CLASS] = 1; // ELFCLASS32
        assert_eq!(validate(&header), Err(ImageError::NotElf64));
    }

    #[test]
    fn rejects_big_endian_image() {
        let mut header = valid_header();
        header[EI_DATA] = 2; // ELFDATA2MSB
        assert_eq!(validate(&header), Err(ImageError::NotLittleEndian));
    }

    #[test]
    fn rejects_pie_image() {
        // PIE 是 ET_DYN(3)：复制后跳转无法工作，必须拒绝而不是放行
        let mut header = valid_header();
        header[E_TYPE_OFFSET..E_TYPE_OFFSET + 2].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(validate(&header), Err(ImageError::NotExecutable(3)));
    }

    #[test]
    fn rejects_foreign_architecture() {
        let mut header = valid_header();
        header[E_MACHINE_OFFSET..E_MACHINE_OFFSET + 2].copy_from_slice(&0x28u16.to_le_bytes()); // aarch64
        assert_eq!(validate(&header), Err(ImageError::UnsupportedMachine(0x28)));
    }

    #[test]
    fn error_messages_are_specific() {
        // 报错信息要能直接指出原因，便于排查
        assert!(ImageError::NotExecutable(3).to_string().contains("ET_DYN"));
        assert!(
            ImageError::UnsupportedMachine(0x28)
                .to_string()
                .contains("x86-64")
        );
    }

    // ---------------- Program Header / 装载规则 ----------------

    fn header_with_phdrs(phnum: u16) -> Vec<u8> {
        let mut data = valid_header().to_vec();
        data[E_PHOFF_OFFSET..E_PHOFF_OFFSET + 8]
            .copy_from_slice(&(ELF_HEADER_MIN_LEN as u64).to_le_bytes());
        data[E_PHENTSIZE_OFFSET..E_PHENTSIZE_OFFSET + 2]
            .copy_from_slice(&(PHDR_LEN as u16).to_le_bytes());
        data[E_PHNUM_OFFSET..E_PHNUM_OFFSET + 2].copy_from_slice(&phnum.to_le_bytes());
        data
    }

    // 逐字段构造 Program Header；参数多是有意的，便于测试里一眼看清每个字段
    #[allow(clippy::too_many_arguments)]
    fn push_phdr(
        data: &mut Vec<u8>,
        p_type: u32,
        flags: u32,
        offset: u64,
        vaddr: u64,
        paddr: u64,
        filesz: u64,
        memsz: u64,
    ) {
        let mut entry = [0u8; PHDR_LEN];
        entry[P_TYPE_OFFSET..P_TYPE_OFFSET + 4].copy_from_slice(&p_type.to_le_bytes());
        entry[P_FLAGS_OFFSET..P_FLAGS_OFFSET + 4].copy_from_slice(&flags.to_le_bytes());
        entry[P_OFFSET_OFFSET..P_OFFSET_OFFSET + 8].copy_from_slice(&offset.to_le_bytes());
        entry[P_VADDR_OFFSET..P_VADDR_OFFSET + 8].copy_from_slice(&vaddr.to_le_bytes());
        entry[P_PADDR_OFFSET..P_PADDR_OFFSET + 8].copy_from_slice(&paddr.to_le_bytes());
        entry[P_FILESZ_OFFSET..P_FILESZ_OFFSET + 8].copy_from_slice(&filesz.to_le_bytes());
        entry[P_MEMSZ_OFFSET..P_MEMSZ_OFFSET + 8].copy_from_slice(&memsz.to_le_bytes());
        data.extend_from_slice(&entry);
    }

    /// 两个段：可执行段 + 未页对齐的可写段（复现真实内核的布局）
    fn two_segment_image() -> Vec<u8> {
        let mut data = header_with_phdrs(2);
        // 先占位，稍后填充分段数据
        push_phdr(
            &mut data,
            PT_LOAD,
            PF_R | PF_X,
            0x2000,
            0x101000,
            0x101000,
            16,
            16,
        );
        push_phdr(
            &mut data,
            PT_LOAD,
            PF_R | PF_W,
            0x3000,
            0x104B38,
            0x104B38,
            8,
            56,
        );
        data.resize(0x3000 + 8, 0);
        data
    }

    #[test]
    fn parses_two_loadable_segments() {
        let data = two_segment_image();
        assert_eq!(check_segments(&data), Ok(2));
        assert_eq!(check_no_overlap(&data), Ok(()));
        let segs: Vec<_> = segments(&data).unwrap().map(Result::unwrap).collect();
        assert_eq!(segs.len(), 2);
        assert!(segs[0].is_executable());
        assert!(!segs[1].is_executable());
        // 非 PT_LOAD 的条目会被跳过
        let mut with_note = two_segment_image();
        with_note[E_PHNUM_OFFSET..E_PHNUM_OFFSET + 2].copy_from_slice(&3u16.to_le_bytes());
        push_phdr(&mut with_note, 4 /* PT_NOTE */, PF_R, 0, 0, 0, 0, 0);
        assert_eq!(check_segments(&with_note), Ok(2));
    }

    #[test]
    fn unaligned_paddr_needs_a_page_aligned_range() {
        // 真实内核里 .data 的 p_paddr 并不是页对齐的（例如 0x104B38）。
        // 装载时必须申请覆盖它的页对齐区间，而不是直接用它申请页。
        let segment = Segment {
            offset: 0,
            paddr: 0x104B38,
            filesz: 8,
            memsz: 56,
            flags: PF_R | PF_W,
        };
        assert_eq!(segment.page_range(), (0x104000, 0x105000));

        let aligned = Segment {
            paddr: 0x100000,
            memsz: 4096,
            ..segment
        };
        assert_eq!(aligned.page_range(), (0x100000, 0x101000));

        // 跨页的情况
        let crossing = Segment {
            paddr: 0x100FF0,
            memsz: 0x20,
            ..segment
        };
        assert_eq!(crossing.page_range(), (0x100000, 0x102000));
    }

    #[test]
    fn rejects_bad_segments() {
        // p_filesz > p_memsz
        let mut data = header_with_phdrs(1);
        push_phdr(&mut data, PT_LOAD, PF_R, 0, 0x1000, 0x1000, 100, 10);
        data.resize(0x1000, 0);
        assert_eq!(
            check_segments(&data),
            Err(SegmentError::FileszLargerThanMemsz)
        );

        // 数据超出文件
        let mut data = header_with_phdrs(1);
        push_phdr(&mut data, PT_LOAD, PF_R, 0, 0x1000, 0x1000, 4096, 4096);
        assert_eq!(check_segments(&data), Err(SegmentError::DataOutOfBounds));

        // 空段
        let mut data = header_with_phdrs(1);
        push_phdr(&mut data, PT_LOAD, PF_R, 0, 0x1000, 0x1000, 0, 0);
        assert_eq!(check_segments(&data), Err(SegmentError::EmptySegment));

        // Program Header 表越界
        let mut data = header_with_phdrs(4);
        push_phdr(&mut data, PT_LOAD, PF_R, 0, 0x1000, 0x1000, 1, 1);
        assert_eq!(check_segments(&data), Err(SegmentError::BadProgramHeaders));
    }

    #[test]
    fn rejects_overlapping_segments() {
        let mut data = header_with_phdrs(2);
        push_phdr(&mut data, PT_LOAD, PF_R, 0, 0x1000, 0x1000, 8, 64);
        push_phdr(&mut data, PT_LOAD, PF_R, 0, 0x1020, 0x1020, 8, 64);
        data.resize(0x1000, 0);
        assert_eq!(
            check_no_overlap(&data),
            Err(SegmentError::OverlappingSegments)
        );
    }

    #[test]
    fn entry_must_be_inside_an_executable_segment() {
        let data = two_segment_image();
        // two_segment_image 的头部入口是 ENTRY（0x201190），不在段内
        assert_eq!(
            check_entry(&data, ENTRY),
            Err(SegmentError::EntryNotExecutable)
        );
        assert_eq!(check_entry(&data, 0x101008), Ok(()));
        // 落在可写但不可执行的段里 → 拒绝
        assert_eq!(
            check_entry(&data, 0x104B38),
            Err(SegmentError::EntryNotExecutable)
        );
    }

    #[test]
    fn loaded_span_covers_all_segments() {
        let data = two_segment_image();
        assert_eq!(loaded_span(&data), Ok((0x101000, 0x105000)));
    }
}
