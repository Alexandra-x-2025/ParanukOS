//! 从引导镜像所在的 ESP 卷读取内核镜像（ELF64）并载入内存。
//!
//! 使用 uefi-rs 0.39 的全局 API：
//! * `uefi::boot::image_handle` / `uefi::boot::get_image_file_system`
//!   直接拿到「本引导器所在卷」，无需遍历所有句柄去猜哪个是 ESP。
//! * `uefi::fs::FileSystem` 高层 API 负责处理文件信息缓冲区对齐等细节。
//!
//! 镜像格式校验（ELF64 / 小端 / x86-64 / `ET_EXEC`）委托给零依赖的
//! `kernel-image` crate，因此那部分逻辑可以在宿主平台上被单元测试覆盖。

use core::fmt::Write;

use kernel_image::{self, ImageError};
use uefi::CStr16;
use uefi::boot::{self, AllocateType, MemoryType, PAGE_SIZE};
use uefi::fs::{self, FileSystem};
use uefi::prelude::*;
use uefi::proto::console::text::Output;

/// 内核镜像在 ESP 中的约定路径。
const KERNEL_PATH: &CStr16 = cstr16!("\\EFI\\PARANUKO\\KERNEL.ELF");

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
    /// 镜像格式不合法。
    Image(ImageError),
}

impl LoadError {
    /// 映射为 UEFI 状态码，供引导器返回给固件。
    pub fn status(&self) -> Status {
        match self {
            Self::Efi(err) => err.status(),
            Self::Fs(fs::Error::Io(io)) => io.uefi_error.status(),
            Self::Fs(_) => Status::VOLUME_CORRUPTED,
            Self::Image(_) => Status::LOAD_ERROR,
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

impl From<ImageError> for LoadError {
    fn from(err: ImageError) -> Self {
        Self::Image(err)
    }
}

impl core::fmt::Display for LoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Efi(err) => write!(f, "{err}"),
            Self::Fs(fs::Error::Io(io)) => write!(f, "读取文件失败: {}", io.uefi_error),
            Self::Fs(err) => write!(f, "文件系统错误: {err}"),
            Self::Image(err) => write!(f, "{err}"),
        }
    }
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
        let entry_point = kernel_image::validate(&data)?;
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
