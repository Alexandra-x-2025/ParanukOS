//! ParanukOS 内核 —— Milestone 1：异常与日志基础设施。
//!
//! 在 M0（自检 + 串口输出）之上：
//! * 安装最小 IDT（32 个 CPU 异常），使内核出错时能**打印诊断**而不是三重故障重启；
//! * panic 处理器与异常处理器统一以"内核崩溃"退出码 41 结束（与自检失败的 39 区分）；
//! * 统一的最小串口日志设施（`kinfo!` / `kwarn!` / `kerror!`）。
//!
//! 依据 `docs/architecture/kernel_interface.md`：§4（入口 ABI）、§7.2（退出码）、§9（M1）。

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

mod idt;
mod logging;
mod serial;

use boot_info::{BootInfo, EXIT_VALUE_KERNEL_FAILURE, EXIT_VALUE_KERNEL_FAULT, qemu_exit_code};
// 注入模式下不会走到"正常结束"，因此该常量仅在非注入构建中使用
#[cfg(not(feature = "inject-fault"))]
use boot_info::EXIT_VALUE_KERNEL_OK;
use core::sync::atomic::{AtomicU32, Ordering};
use logging::{kerror, kinfo, kwarn};

use serial::Serial;

/// panic 处理器拿不到 `BootInfo`，因此入口处把退出端口存进这里。
static EXIT_PORT: AtomicU32 = AtomicU32::new(0);

/// 内核入口。
///
/// # Safety（ABI 契约）
/// 由引导器通过 `jmp e_entry` 进入，必须满足 `docs/architecture/kernel_interface.md` §4：
/// `rdi` 指向一页由引导器分配并填写好的 `BootInfo`，栈已就绪且 16 字节对齐（`rsp % 16 == 8`），
/// 中断已关闭。违反任一条都会导致未定义行为。
#[unsafe(no_mangle)]
pub extern "C" fn kernel_main(boot_info: &BootInfo) -> ! {
    // SAFETY: 引导器保证 COM1 可用；这是内核唯一可用的早期输出通道。
    unsafe { Serial::init() };
    EXIT_PORT.store(boot_info.exit_port, Ordering::Relaxed);

    // 安装 IDT 必须尽早：在此之前任何异常都会升级为三重故障（QEMU 静默重启），无法排查。
    // SAFETY: 单核、初始化阶段，且只调用一次。
    unsafe { idt::install() };
    kinfo!("IDT 已安装（32 个异常向量）");

    kinfo!("ParanukOS kernel alive (Milestone 1)");

    // 1. 校验 BootInfo 头部
    if let Err(err) = boot_info.validate() {
        kerror!("self-check FAILED: {err}");
        // 打印原始字节：引导器与内核之间只有这一处交接，一旦内存被篡改要靠这些数据定位
        let base = boot_info as *const BootInfo as u64;
        for index in 0..4u64 {
            // SAFETY: BootInfo 页由引导器分配为可读的 LOADER_DATA，偏移 0..32 均在页内。
            let word = unsafe { core::ptr::read_volatile((base + index * 8) as *const u64) };
            kerror!("  bootinfo+{} = 0x{word:016X}", index * 8);
        }
        finish(EXIT_VALUE_KERNEL_FAILURE);
    }
    kinfo!(
        "BootInfo v{} size={} magic=0x{:016X}",
        boot_info.version,
        boot_info.size,
        boot_info.magic
    );

    // 2. 内存图必须可用
    if !boot_info.has_memory_map() {
        kerror!("self-check FAILED: 内存图不可用（mmap_ptr/len/desc_size 不完整）");
        finish(EXIT_VALUE_KERNEL_FAILURE);
    }
    if boot_info.mmap_desc_size != 40 {
        // 标准 EFI_MEMORY_DESCRIPTOR 是 40 字节；QEMU 8.2 的 OVMF 实际返回 48。
        // 必须按 BootInfo 给出的步长遍历，否则每一条都会错位。
        kwarn!(
            "内存描述符步长是 {} 字节（标准为 40），按 BootInfo 给出的步长处理",
            boot_info.mmap_desc_size
        );
    }
    kinfo!(
        "memory map: {} 项, desc_size={}, desc_ver={}",
        boot_info.mmap_len,
        boot_info.mmap_desc_size,
        boot_info.mmap_desc_ver
    );

    // 3. 内核镜像与栈的范围
    kinfo!(
        "kernel range: 0x{:X}..0x{:X}",
        boot_info.kernel_base,
        boot_info.kernel_base + boot_info.kernel_size
    );
    kinfo!(
        "stack: 0x{:X}..0x{:X}",
        boot_info.stack_top - boot_info.stack_size,
        boot_info.stack_top
    );

    // 4. ACPI：本项目的硬件约束是 x86-64 + UEFI + ACPI 6.x
    if boot_info.rsdp == 0 {
        kerror!("self-check FAILED: 未找到 ACPI RSDP");
        finish(EXIT_VALUE_KERNEL_FAILURE);
    }
    kinfo!("rsdp=0x{:X}", boot_info.rsdp);

    kinfo!("self-check OK");

    // 故障注入（仅测试）：验证异常处理器路径。`ud2` 触发 #UD(6)。
    #[cfg(feature = "inject-fault")]
    {
        kinfo!("[inject] 故意执行 ud2 触发 #UD，用于验证异常处理器");
        // SAFETY: `ud2` 一定会触发 #UD，CPU 随即进入我们刚安装的处理器，不会返回。
        unsafe { core::arch::asm!("ud2", options(noreturn)) };
    }

    #[cfg(not(feature = "inject-fault"))]
    finish(EXIT_VALUE_KERNEL_OK)
}

/// 结束内核运行：测试模式下通过 `isa-debug-exit` 报告退出码，交互模式下 `hlt` 停机。
///
/// 退出码取自 `crates/boot-info`（33/35/37/39/41），避免两侧各写一份魔数。
pub(crate) fn finish(value: u8) -> ! {
    let port = EXIT_PORT.load(Ordering::Relaxed);
    if port == 0 {
        kinfo!("未启用调试退出，进入 hlt 停机");
    } else {
        kinfo!(
            "退出码 {} (isa-debug-exit 端口 0x{:X} <- 0x{:02X})",
            qemu_exit_code(value),
            port,
            value
        );
        // 端口来自 BootInfo.exit_port 且非 0；QEMU 的 isa-debug-exit 设备收到写入后
        // 会立即终止虚拟机。该设备不存在时（例如不在 QEMU 中）写入无害。
        unsafe { serial::outb(port as u16, value) };
    }
    halt_loop()
}

/// 停机循环。
fn halt_loop() -> ! {
    loop {
        // SAFETY: `hlt` 在 ring 0 合法；中断已关闭，因此 CPU 保持停机直到 NMI。
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // panic 与 CPU 异常同属"内核意外崩溃"，因此共用 41（而不是自检失败的 39）
    kerror!("PANIC: {info}");
    finish(EXIT_VALUE_KERNEL_FAULT)
}
