//! 从引导镜像所在的 ESP 卷读取内核镜像（ELF64）并载入内存。
//!
//! 使用 uefi-rs 0.39 的全局 API：
//! * `uefi::boot::image_handle` / `uefi::boot::get_image_file_system`
//!   直接拿到「本引导器所在卷」，无需遍历所有句柄去猜哪个是 ESP。
//! * `uefi::fs::FileSystem` 高层 API 负责处理文件信息缓冲区对齐等细节。

use core::fmt::Write;

use uefi::boot::{self, AllocateType, MemoryType, PAGE_SIZE};
use uefi::fs::{self, FileSystem};
use uefi::prelude::*;
use uefi::proto::console::text::Output;
use uefi::CStr16;

/// 内核镜像在 ESP 中的约定路径。
const KERNEL_PATH: &CStr16 = cstr16!("\\EFI\\PARANUKO\\KERNEL.ELF");

// ---- ELF64 头部字段偏移与取值（见 ELF 规范） ----
const ELF_HEADER_MIN_LEN: usize = 64;
const EI_CLASS: usize = 4;
const EI_DATA: usize = 5;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const E_TYPE_OFFSET: usize = 16;
const E_MACHINE_OFFSET: usize = 18;
const E_ENTRY_OFFSET: usize = 24;
const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;

/// 载入成功的内核信息。
#[derive(Debug)]
pub struct LoadedKernel {
    /// 内核镜像被复制到的物理基址（页对齐）。
    pub base_address: usize,
    /// 内核镜像字节数。
    pub size: usize,
    /// ELF 头中声明的入口点虚拟地址。
    pub entry_point: u64,
}

/// 内核加载失败的原因。
#[derive(Debug)]
pub enum LoadError {
    /// UEFI 层错误（打开卷、分配内存等）。
    Efi(uefi::Error),
    /// 读取文件失败。
    Fs(fs::Error),
    /// 镜像过小，连 ELF 头都不完整。
    TooSmall(usize),
    /// 缺少 ELF magic（`\x7fELF`）。
    NotElf,
    /// 不是 64 位 ELF。
    NotElf64,
    /// 不是小端序 ELF。
    NotLittleEndian,
    /// `e_type` 不是 `ET_EXEC`。
    NotExecutable(u16),
    /// `e_machine` 不是 x86-64。
    UnsupportedMachine(u16),
}

impl LoadError {
    /// 映射为 UEFI 状态码，供引导器返回给固件。
    pub fn status(&self) -> Status {
        match self {
            Self::Efi(err) => err.status(),
            Self::Fs(fs::Error::Io(io)) => io.uefi_error.status(),
            Self::Fs(_) => Status::VOLUME_CORRUPTED,
            Self::TooSmall(_)
            | Self::NotElf
            | Self::NotElf64
            | Self::NotLittleEndian
            | Self::NotExecutable(_)
            | Self::UnsupportedMachine(_) => Status::LOAD_ERROR,
        }
    }
}

impl From<uefi::Error> for LoadError {
    fn from(err: uefi::Error) -> Self {
        Self::Efi(err)
    }
}

impl From<fs::Error> for LoadError {
    fn from(err: fs::Error) -> Self {
        Self::Fs(err)
    }
}

/// 解析 ELF64 头并返回入口点地址；任何格式不符都返回错误。
fn parse_elf_header(data: &[u8]) -> Result<u64, LoadError> {
    if data.len() < ELF_HEADER_MIN_LEN {
        return Err(LoadError::TooSmall(data.len()));
    }
    if data[0..4] != [0x7f, b'E', b'L', b'F'] {
        return Err(LoadError::NotElf);
    }
    if data[EI_CLASS] != ELFCLASS64 {
        return Err(LoadError::NotElf64);
    }
    if data[EI_DATA] != ELFDATA2LSB {
        return Err(LoadError::NotLittleEndian);
    }

    let e_type = u16::from_le_bytes([data[E_TYPE_OFFSET], data[E_TYPE_OFFSET + 1]]);
    if e_type != ET_EXEC {
        return Err(LoadError::NotExecutable(e_type));
    }

    let e_machine = u16::from_le_bytes([data[E_MACHINE_OFFSET], data[E_MACHINE_OFFSET + 1]]);
    if e_machine != EM_X86_64 {
        return Err(LoadError::UnsupportedMachine(e_machine));
    }

    let mut entry = [0u8; 8];
    entry.copy_from_slice(&data[E_ENTRY_OFFSET..E_ENTRY_OFFSET + 8]);
    Ok(u64::from_le_bytes(entry))
}

pub struct FsLoader;

impl FsLoader {
    /// 定位 ESP 卷、读取并校验内核镜像，最后复制到新分配的物理页中。
    pub fn load_kernel(stdout: &mut Output) -> Result<LoadedKernel, LoadError> {
        // 1. 直接打开「本引导镜像所在的卷」，而不是遍历所有 SimpleFileSystem
        //    句柄、靠 \EFI\ 目录去猜哪个是 ESP。
        let _ = writeln!(stdout, "[*] 打开引导镜像所在卷 ...");
        let fs_proto = boot::get_image_file_system(boot::image_handle())?;
        let mut fs = FileSystem::new(fs_proto);

        // 2. 读取内核镜像
        let _ = writeln!(stdout, "[*] 读取内核镜像 {KERNEL_PATH} ...");
        let data = fs.read(KERNEL_PATH)?;
        let file_size = data.len();
        let _ = writeln!(stdout, "[*] 读取完成: {file_size} 字节");

        // 3. 校验 ELF 格式并取出入口点
        let entry_point = parse_elf_header(&data)?;
        let _ = writeln!(stdout, "[*] ELF64/x86-64 校验通过，入口 0x{entry_point:X}");

        // 4. 按页分配内核装载空间（页对齐由 allocate_pages 保证）
        let pages_needed = file_size.div_ceil(PAGE_SIZE);
        let kernel_addr = boot::allocate_pages(
            AllocateType::AnyPages,
            MemoryType::LOADER_DATA,
            pages_needed,
        )?;

        // 5. 把镜像字节复制进新申请的物理内存
        // SAFETY: `allocate_pages` 保证返回页对齐、且至少有
        // `pages_needed * PAGE_SIZE >= file_size` 字节的可写空间；`data` 是有效的
        // `file_size` 字节只读切片，两块内存不重叠。
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), kernel_addr.as_ptr(), file_size);
        }

        let base_address = kernel_addr.as_ptr() as usize;
        let _ = writeln!(
            stdout,
            "[+ SUCCESS] 内核已载入 0x{base_address:X}（{pages_needed} 页）"
        );

        Ok(LoadedKernel {
            base_address,
            size: file_size,
            entry_point,
        })
    }
}
