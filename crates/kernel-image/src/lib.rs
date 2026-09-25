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

#[cfg(test)]
mod tests {
    use super::*;

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
}
