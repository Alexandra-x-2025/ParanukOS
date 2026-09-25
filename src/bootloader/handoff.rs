//! 引导器 → 内核的交接（`docs/architecture/kernel_interface.md` §6.1）。
//!
//! 顺序不可交换，关键点有三个：
//! 1. 所有**打印**必须在 `exit_boot_services` 之前完成——之后固件控制台不再可用；
//! 2. 内存图取自 `exit_boot_services` 的**返回值**（权威版本）：它的 key 正是被用来
//!    退出引导服务的那一个。因此 `BootInfo` 在退出之后才写入（纯内存操作，安全）；
//! 3. 退出后不得再调用任何 boot service，也不得 drop 任何会调用它们的类型。

use core::fmt::Write;

use boot_info::{BOOT_INFO_MAGIC, BOOT_INFO_VERSION, BootInfo, DEBUG_EXIT_PORT};
use uefi::boot::{self, AllocateType, MemoryType, PAGE_SIZE};
use uefi::mem::memory_map::MemoryMap;
use uefi::proto::console::text::Output;
use uefi::table::cfg::ConfigTableEntry;

use super::fs_loader::{LoadError, LoadedKernel};

/// 内核栈大小（接口文档 §4 约定 64 KiB）。
pub const KERNEL_STACK_SIZE: usize = 64 * 1024;

/// 交接前已准备好的内容。
pub struct Prepared {
    /// 内核栈顶（高地址端）。
    pub stack_top: u64,
    /// 已分配并待写入的 `BootInfo` 页。
    pub boot_info: *mut BootInfo,
    /// ACPI RSDP 的物理地址（0 表示未找到）。
    pub rsdp: u64,
}

/// 分配内核栈与 `BootInfo` 页，并读取 ACPI RSDP。
///
/// 这些步骤都必须发生在 `exit_boot_services` 之前（之后无法再分配）。
pub fn prepare(stdout: &mut Output) -> Result<Prepared, LoadError> {
    // 内核栈：页对齐、类型为 LOADER_DATA，因此内核的分配器会把它视为"已占用"
    let stack_pages = KERNEL_STACK_SIZE / PAGE_SIZE;
    let stack_base =
        boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, stack_pages)?;
    let stack_top = stack_base.as_ptr() as u64 + KERNEL_STACK_SIZE as u64;
    let _ = writeln!(
        stdout,
        "[*] 内核栈: 0x{:X}..0x{:X} ({} KiB)",
        stack_top - KERNEL_STACK_SIZE as u64,
        stack_top,
        KERNEL_STACK_SIZE / 1024
    );

    // BootInfo 独占一页：内核可以长期持有该指针，且不会被回收
    let boot_info_page = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, 1)?;
    let boot_info = boot_info_page.as_ptr() as *mut BootInfo;
    let _ = writeln!(stdout, "[*] BootInfo 页: 0x{:X}", boot_info as u64);

    // ACPI RSDP：必须在退出引导服务之前从配置表取出
    let rsdp = find_rsdp();
    let _ = writeln!(stdout, "[*] ACPI RSDP: 0x{rsdp:X}");

    Ok(Prepared {
        stack_top,
        boot_info,
        rsdp,
    })
}

/// 优先 ACPI 2.0 的 RSDP，缺失时回退到 ACPI 1.0。
fn find_rsdp() -> u64 {
    uefi::system::with_config_table(|entries| {
        let mut fallback = 0u64;
        for entry in entries {
            if entry.guid == ConfigTableEntry::ACPI2_GUID {
                return entry.address as u64;
            }
            if entry.guid == ConfigTableEntry::ACPI_GUID && fallback == 0 {
                fallback = entry.address as u64;
            }
        }
        fallback
    })
}

/// 完成交接并跳入内核。**永不返回。**
pub fn enter_kernel(stdout: &mut Output, kernel: &LoadedKernel, prepared: Prepared) -> ! {
    let _ = writeln!(
        stdout,
        "[+] 交接准备就绪: rsp=0x{:X} rdi=0x{:X} jmp=0x{:X}",
        prepared.stack_top - 8,
        prepared.boot_info as u64,
        kernel.entry
    );
    let _ = writeln!(
        stdout,
        "[!] exit_boot_services 之后固件控制台不再可用 (§6.2)"
    );

    // 退出引导服务。返回的映射是权威版本（key 就是用来退出的那个）。
    // SAFETY: 调用点满足该 API 的全部前置条件：已完成所有分配与打印，
    // 且此后不再使用任何 boot service。
    let memory_map = unsafe { boot::exit_boot_services(Some(MemoryType::LOADER_DATA)) };

    // **立刻关闭中断**：退出引导服务后固件的中断向量与处理程序都不再有效，
    // 此时若有遗留的硬件中断到达，CPU 会跳进已失效的固件 ISR 并破坏内存
    // （实测表现为 BootInfo 被局部改写、且随代码布局变化而时有时无）。
    // SAFETY: `cli` 只清除 IF 标志。
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
    }

    // 退出之后仍然可以安全地做纯内存读写，因此现在写入 BootInfo。
    #[allow(unused_mut)]
    let mut info = BootInfo {
        mmap_ptr: memory_map.buffer().as_ptr() as u64,
        mmap_len: memory_map.len() as u64,
        mmap_desc_size: memory_map.meta().desc_size as u32,
        mmap_desc_ver: memory_map.meta().desc_version,
        rsdp: prepared.rsdp,
        kernel_base: kernel.base,
        kernel_size: kernel.size,
        stack_top: prepared.stack_top,
        stack_size: KERNEL_STACK_SIZE as u64,
        exit_port: u32::from(exit_port()),
        ..BootInfo::new()
    };
    // 故障注入（仅测试）：用来验证内核确实会拒绝错误的 magic 并以 39 退出
    #[cfg(feature = "inject-bad-magic")]
    {
        info.magic = BOOT_INFO_MAGIC ^ 0xFFFF_FFFF;
    }

    #[cfg(not(feature = "inject-bad-magic"))]
    debug_assert_eq!(info.magic, BOOT_INFO_MAGIC);
    debug_assert_eq!(info.version, BOOT_INFO_VERSION);

    // SAFETY: `prepared.boot_info` 指向一页由引导器分配、类型为 LOADER_DATA 的可写内存，
    // 且 `BootInfo` 是 `repr(C)` 的普通数据类型，写入不会 panic。
    unsafe { prepared.boot_info.write(info) };

    // 内存图缓冲与 BootInfo 页都必须活到内核接管之后。`MemoryMapOwned` 的 Drop 在退出
    // 引导服务后不会真正释放内存，但显式 forget 更清楚地表达"这块内存属于内核"。
    core::mem::forget(memory_map);

    // SAFETY: 满足接口文档 §4 的入口 ABI：entry 指向已装载的内核代码，
    // boot_info 指针有效，stack_top 是已分配内核栈的高端且 16 字节对齐
    // （因此 stack_top - 8 满足 rsp % 16 == 8）。
    unsafe { jump_to_kernel(kernel.entry, prepared.boot_info as u64, prepared.stack_top) }
}

/// 跳转到内核：`cli` → 设置 `rsp`/`rdi` → `jmp e_entry`。
///
/// # Safety
/// 所有参数必须指向已就绪的内存，且 `entry` 必须是已按 `p_paddr` 装载的内核入口。
unsafe fn jump_to_kernel(entry: u64, boot_info: u64, stack_top: u64) -> ! {
    // SAFETY: 由调用者保证；`options(noreturn)` 表明控制流不再回到 Rust。
    unsafe {
        core::arch::asm!(
            "cli",
            "mov rsp, {stack}",
            "mov rdi, {bi}",
            "jmp {entry}",
            stack = in(reg) stack_top - 8,
            bi = in(reg) boot_info,
            entry = in(reg) entry,
            options(noreturn)
        );
    }
}

/// 测试模式下交给内核的调试退出端口；交互模式为 0（内核自检后 `hlt`）。
#[must_use]
pub const fn exit_port() -> u16 {
    #[cfg(feature = "qemu-exit")]
    {
        DEBUG_EXIT_PORT
    }
    #[cfg(not(feature = "qemu-exit"))]
    {
        0
    }
}

/// 写 8 位 I/O 端口。
///
/// # Safety
/// 调用者必须确保端口合法且允许写入。
unsafe fn outb(port: u16, value: u8) {
    // SAFETY: 由调用者保证；`out` 不访问内存。
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// 通过 QEMU 的 `isa-debug-exit` 退出，使测试能断言精确退出码。
///
/// 端口应在 QEMU 命令行显式指定：`-device isa-debug-exit,iobase=0xf4,iosize=0x04`
/// （QEMU 8.2 起该设备默认 iobase 为 `0x501`）。设备不存在时写入无害，函数会自旋。
pub fn exit_qemu(value: u8) -> ! {
    // SAFETY: 端口是 QEMU isa-debug-exit 的约定端口（见上）。
    unsafe { outb(DEBUG_EXIT_PORT, value) };
    loop {
        core::hint::spin_loop();
    }
}
