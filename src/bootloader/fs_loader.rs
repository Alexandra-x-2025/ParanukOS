//! 从引导镜像所在的 ESP 卷读取内核镜像，并按 ELF 的 Program Header 装载到内存。
//!
//! 装载规则见 `docs/architecture/kernel_interface.md` §3：
//! * 按 `p_paddr` 装载（而不是复制到任意地址——那样一跳转必崩）；
//! * 段必须按**页对齐区间**申请内存（`p_paddr` 本身不保证页对齐）；
//! * `[p_paddr + p_filesz, p_paddr + p_memsz)` 清零（BSS）；
//! * 段之间不得重叠，入口必须落在可执行段内。

use core::fmt::Write;

use kernel_image::{self, ImageError, PAGE_SIZE as IMAGE_PAGE_SIZE, SegmentError};
use uefi::CStr16;
use uefi::boot::{self, AllocateType, MemoryType};
use uefi::fs::{self, FileSystem};
use uefi::prelude::*;
use uefi::proto::console::text::Output;

/// 内核镜像在 ESP 中的约定路径。
const KERNEL_PATH: &CStr16 = cstr16!("\\EFI\\PARANUKO\\KERNEL.ELF");

/// 用户态服务镜像在 ESP 中的约定路径（`user_mode.md` §4）。
const USER_PATH: &CStr16 = cstr16!("\\EFI\\PARANUKO\\USER.ELF");

/// 一次装载最多覆盖的独立页数。
///
/// M0 的内核只有几页；M2 起内核镜像里多了两块静态 `.bss`（页表竞技场 28 KiB、
/// M2b 的页帧位图 256 KiB），加上 `alloc` 与格式化代码，镜像已接近 110 页，
/// 因此上限放宽到 1 MiB。这个上限的作用是防止在栈上放不下的 `allocated` 数组，
/// 而不是限制内核功能；真正的镜像大小由链接脚本决定。
const MAX_KERNEL_PAGES: usize = 256;

/// 载入成功的内核信息。
#[derive(Debug)]
pub struct LoadedKernel {
    /// 所有段覆盖的页对齐区间起点（物理地址）。
    pub base: u64,
    /// 该区间的字节长度。
    pub size: u64,
    /// ELF 头声明的入口点。
    pub entry: u64,
    /// `PT_LOAD` 段数量。
    pub segments: usize,
    /// 第一个 `PT_LOAD` 段的 `p_vaddr - p_paddr` 差值。
    ///
    /// 内核镜像恒等链接（差值为 0），用户镜像则用它把**物理**装载区间换算成用户虚拟地址：
    /// 内核不需要 ELF 解析器就能把整段映射到 `[vaddr + delta, …)`（`user_mode.md` §3，决策 #43）。
    pub vaddr_delta: i64,
}

/// 内核装载失败的原因。
#[derive(Debug)]
pub enum LoadError {
    /// UEFI 层错误（打开卷、分配内存等）。
    Efi(uefi::Error),
    /// 读取文件失败。
    Fs(fs::Error),
    /// ELF 头不合法。
    Image(ImageError),
    /// Program Header 或段布局不合法。
    Segment(SegmentError),
    /// 段覆盖的独立页数超过上限。
    TooManyPages(usize),
}

impl LoadError {
    /// 映射为 UEFI 状态码，供引导器返回给固件。
    pub fn status(&self) -> Status {
        match self {
            Self::Efi(err) => err.status(),
            Self::Fs(fs::Error::Io(io)) => io.uefi_error.status(),
            Self::Fs(_) => Status::VOLUME_CORRUPTED,
            Self::Image(_) | Self::Segment(_) => Status::LOAD_ERROR,
            Self::TooManyPages(_) => Status::OUT_OF_RESOURCES,
        }
    }
}

impl core::fmt::Display for LoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Efi(err) => write!(f, "{err}"),
            Self::Fs(fs::Error::Io(io)) => write!(f, "读取文件失败: {}", io.uefi_error),
            Self::Fs(err) => write!(f, "文件系统错误: {err}"),
            Self::Image(err) => write!(f, "{err}"),
            Self::Segment(err) => write!(f, "{err}"),
            Self::TooManyPages(pages) => {
                write!(f, "内核镜像覆盖的独立页数过多（{pages} > 上限）")
            }
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

impl From<SegmentError> for LoadError {
    fn from(err: SegmentError) -> Self {
        Self::Segment(err)
    }
}

pub struct FsLoader;

impl FsLoader {
    /// 定位 ESP 卷、读取并校验内核镜像，然后按段装载。
    pub fn load_kernel(stdout: &mut Output) -> Result<LoadedKernel, LoadError> {
        Self::load(stdout, KERNEL_PATH, "内核")
    }

    /// 装载用户态服务镜像（`user_mode.md` §4）。
    ///
    /// 规则与内核镜像完全相同：ELF64 / 小端 / `ET_EXEC` / 段不重叠 / 入口在可执行段内；
    /// 缺失或非法一律与内核镜像同样处理（引导器返回装载失败 → 35）。
    pub fn load_user(stdout: &mut Output) -> Result<LoadedKernel, LoadError> {
        Self::load(stdout, USER_PATH, "用户")
    }

    /// 按给定路径装载一个 ELF 镜像。
    fn load(stdout: &mut Output, path: &CStr16, what: &str) -> Result<LoadedKernel, LoadError> {
        // 1. 打开「本引导镜像所在的卷」
        let _ = writeln!(stdout, "[*] 打开引导镜像所在卷 ...");
        let fs_proto = boot::get_image_file_system(boot::image_handle())?;
        let mut fs = FileSystem::new(fs_proto);

        // 2. 读取内核镜像
        let _ = writeln!(stdout, "[*] 读取{what}镜像 {path} ...");
        let data = fs.read(path)?;
        let _ = writeln!(stdout, "[*] 读取完成: {} 字节", data.len());

        // 3. 校验 ELF 头与段布局
        let entry = kernel_image::validate(&data)?;
        let segments = kernel_image::check_segments(&data)?;
        kernel_image::check_no_overlap(&data)?;
        kernel_image::check_entry(&data, entry)?;
        let (base, end) = kernel_image::loaded_span(&data)?;
        // 用户镜像的 p_vaddr 与 p_paddr 不同（链接在用户区、装载在低地址），差值由第一个段给出。
        // `check_segments` 已经确认至少有一个非空段，因此这里不会为空。
        let first = kernel_image::segments(&data)?
            .next()
            .ok_or(SegmentError::EmptySegment)??;
        let vaddr_delta = first.vaddr as i64 - first.paddr as i64;
        let _ = writeln!(
            stdout,
            "[*] ELF64/x86-64 校验通过: {segments} 个 PT_LOAD 段, 入口 0x{entry:X}, 占用 0x{base:X}..0x{end:X}"
        );

        // 4. 按段装载。`allocated` 记录本镜像已申请过的页，避免重复申请
        //    （不同段的页区间可能包含同一页：某段的尾部与下一段共享一页）。
        let mut allocated = [0u64; MAX_KERNEL_PAGES];
        let mut allocated_len = 0usize;

        for segment in kernel_image::segments(&data)? {
            let segment = segment?;
            let (page_start, page_end) = segment.page_range();

            let mut page = page_start;
            while page < page_end {
                if !allocated[..allocated_len].contains(&page) {
                    if allocated_len == allocated.len() {
                        return Err(LoadError::TooManyPages(allocated_len + 1));
                    }
                    boot::allocate_pages(AllocateType::Address(page), MemoryType::LOADER_DATA, 1)?;
                    allocated[allocated_len] = page;
                    allocated_len += 1;
                }
                page += IMAGE_PAGE_SIZE;
            }

            let file_start = segment.offset as usize;
            let file_end = (segment.offset + segment.filesz) as usize;
            // SAFETY: `check_segments` 已确认 file_end <= data.len()，因此切片合法；
            // 目标区间 [p_paddr, p_paddr + p_memsz) 所在的页已由上面的 allocate_pages 申请，
            // 且 `check_no_overlap` 保证各段的字节区间互不重叠，因此写入不会破坏其他段。
            unsafe {
                let dest = segment.paddr as *mut u8;
                core::ptr::copy_nonoverlapping(
                    data[file_start..file_end].as_ptr(),
                    dest,
                    file_end - file_start,
                );
                let bss_len = (segment.memsz - segment.filesz) as usize;
                if bss_len > 0 {
                    core::ptr::write_bytes(dest.add(file_end - file_start), 0, bss_len);
                }
            }

            let _ = writeln!(
                stdout,
                "    [+] 段 0x{:X}..0x{:X} (filesz={} memsz={} 页 0x{:X}..0x{:X})",
                segment.paddr,
                segment.paddr + segment.memsz,
                segment.filesz,
                segment.memsz,
                page_start,
                page_end
            );
        }

        // `data` 是池分配，离开本函数即释放；此时镜像内容已复制完毕。
        Ok(LoadedKernel {
            base,
            size: end - base,
            entry,
            segments,
            vaddr_delta,
        })
    }
}
