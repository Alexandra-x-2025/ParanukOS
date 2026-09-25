use uefi::prelude::*;
use uefi::proto::media::file::{File, FileAttribute, FileMode, FileInfo};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::table::boot::{AllocateType, MemoryType};
use core::fmt::Write;

pub struct FsLoader;

/// 载入成功的内核信息
#[derive(Debug)]
pub struct LoadedKernel {
    pub base_address: usize,
    pub size: usize,
}

impl FsLoader {
    /// 定位 ESP 分区并加载 KERNEL.ELF
    pub fn load_kernel(
        boot_services: &BootServices, 
        stdout: &mut uefi::proto::console::text::Output<'_>
    ) -> Result<LoadedKernel, uefi::Error> {
        
        writeln!(stdout, "Bootloader initialized. Searching for ESP partition...").unwrap();

        // 1. 遍历系统中所有支持 SimpleFileSystem 协议的物理硬件句柄
        let handles = boot_services.find_handles::<SimpleFileSystem>()?;
        let mut target_fs = None;

        for handle in handles {
            // 以独占模式安全开启当前硬件的文件系统接口
            if let Ok(mut fs) = boot_services.open_protocol_exclusive::<SimpleFileSystem>(handle) {
                if let Ok(mut root_dir) = fs.open_volume() {
                    // 精准特征检查：尝试打开 \EFI\ 目录来验证是否为标准的 ESP 引导盘
                    if root_dir.open("\\EFI\\", FileMode::Read, FileAttribute::empty()).is_ok() {
                        target_fs = Some(fs);
                        break;
                    }
                }
            }
        }

        // 修正 1：使用标准且最新的 uefi::Status::NOT_FOUND 构造错误，载荷直接传入 ()
        let mut file_system = target_fs.ok_or_else(|| {
            uefi::Error::new(uefi::Status::NOT_FOUND, ())
        })?;

        writeln!(stdout, "ESP found. Searching for kernel at \\EFI\\PARANUKO\\KERNEL.ELF...").unwrap();

        // 2. 打开确认后的根目录，直奔内核路径
        let mut root_dir = file_system.open_volume()?;
        let mut kernel_file = root_dir.open(
            "\\EFI\\PARANUKO\\KERNEL.ELF",
            FileMode::Read,
            FileAttribute::empty(),
        )?.into_regular_file().ok_or_else(|| {
            uefi::Error::new(uefi::Status::UNSUPPORTED, ())
        })?;

        // 3. 获取内核元数据以动态向主板索要 RAM 空间
        let mut info_buffer = [0u8; 128];
        let file_info = kernel_file.get_info::<FileInfo>(&mut info_buffer)?;
        let file_size = file_info.file_size() as usize;
        writeln!(stdout, "Found kernel: {} bytes", file_size).unwrap();

        // 4. 通过直接分配物理页来天然锁定 4KB 边界对齐 (1 Page = 4096 字节)
        let pages_needed = (file_size + 4095) / 4096;
        let kernel_addr = boot_services.allocate_pages(
            AllocateType::AnyPages,
            MemoryType::LOADER_DATA,
            pages_needed,
        )?;

        // 修正 2：内存物理对齐审计，使用标准的 Status::INVALID_PARAMETER 替换
        if kernel_addr % 4096 != 0 {
            return Err(uefi::Error::new(uefi::Status::INVALID_PARAMETER, ()));
        }
        writeln!(stdout, "Memory alignment check passed: 0x{:X} (aligned to 4KB)", kernel_addr).unwrap();

        // 5. 正式将内核字节灌入新申请的物理内存空间
        let kernel_slice = unsafe {
            core::slice::from_raw_parts_mut(kernel_addr as *mut u8, file_size)
        };
        kernel_file.read(kernel_slice)?;
        writeln!(stdout, "[+ SUCCESS] Kernel data injected successfully.").unwrap();

        Ok(LoadedKernel {
            base_address: kernel_addr as usize,
            size: file_size,
        })
    }
}
