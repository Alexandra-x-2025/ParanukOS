//! ParanukOS 内核 —— Milestone 0：自检 + 串口输出。
//!
//! 依据 `docs/architecture/kernel_interface.md`：
//! * §4 入口 ABI：引导器 `jmp` 到 ELF 的 `e_entry`，`rdi` 传入 `BootInfo` 的物理地址，
//!   栈已由引导器准备好且满足 `rsp % 16 == 8`。因此这里不需要汇编 stub。
//! * §7.3 `exit_port`：测试模式下内核通过 `isa-debug-exit` 报告自检结果（37/39）。
//! * §8.4 验收标准：打印 `BootInfo` 摘要，且内存图条目数 > 0、`rsdp != 0`。

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

mod serial;

use boot_info::{
    BootInfo, EXIT_VALUE_KERNEL_FAILURE, EXIT_VALUE_KERNEL_OK, qemu_exit_code,
};
use core::sync::atomic::{AtomicU32, Ordering};
use serial::Serial;

/// panic 处理器拿不到 `BootInfo`，因此入口处把退出端口存进这里。
static EXIT_PORT: AtomicU32 = AtomicU32::new(0);

macro_rules! kprintln {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let mut out = $crate::serial::Serial;
        let _ = core::writeln!(&mut out, $($arg)*);
    }};
}

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

    kprintln!("[kernel] ParanukOS kernel alive (Milestone 0)");

    // 1. 先校验 BootInfo 头部：magic/version/size 任一不符都必须拒绝继续
    if let Err(err) = boot_info.validate() {
        kprintln!("[kernel] self-check FAILED: {err}");
        finish(false);
    }
    kprintln!(
        "[kernel] BootInfo v{} size={} magic=0x{:016X}",
        boot_info.version,
        boot_info.size,
        boot_info.magic
    );

    // 2. 内存图必须可用：后续所有里程碑都依赖它
    if !boot_info.has_memory_map() {
        kprintln!("[kernel] self-check FAILED: 内存图不可用（mmap_ptr/len/desc_size 不完整）");
        finish(false);
    }
    kprintln!(
        "[kernel] memory map: {} 项, desc_size={}, desc_ver={}",
        boot_info.mmap_len,
        boot_info.mmap_desc_size,
        boot_info.mmap_desc_ver
    );

    // 3. 内核镜像与栈的范围（供人工核对装载是否正确）
    kprintln!(
        "[kernel] kernel range: 0x{:X}..0x{:X}",
        boot_info.kernel_base,
        boot_info.kernel_base + boot_info.kernel_size
    );
    kprintln!(
        "[kernel] stack: 0x{:X}..0x{:X}",
        boot_info.stack_top - boot_info.stack_size,
        boot_info.stack_top
    );

    // 4. ACPI：本项目的硬件约束是 x86-64 + UEFI + ACPI 6.x，因此 RSDP 缺失视为自检失败
    if boot_info.rsdp == 0 {
        kprintln!("[kernel] self-check FAILED: 未找到 ACPI RSDP");
        finish(false);
    }
    kprintln!("[kernel] rsdp=0x{:X}", boot_info.rsdp);

    kprintln!("[kernel] self-check OK");
    finish(true)
}

/// 结束自检。
///
/// 测试模式（`exit_port != 0`）下通过 `isa-debug-exit` 报告精确退出码（37/39）；
/// 交互模式下写入 `hlt` 停机循环。
fn finish(ok: bool) -> ! {
    let port = EXIT_PORT.load(Ordering::Relaxed);
    if port == 0 {
        kprintln!("[kernel] 未启用调试退出，进入 hlt 停机");
    } else {
        let value = if ok {
            EXIT_VALUE_KERNEL_OK
        } else {
            EXIT_VALUE_KERNEL_FAILURE
        };
        kprintln!(
            "[kernel] 退出码 {} (isa-debug-exit 端口 0x{:X} <- 0x{:02X})",
            qemu_exit_code(value),
            port,
            value
        );
        // 端口来自 BootInfo.exit_port 且非 0；QEMU 的 isa-debug-exit 设备收到写入后
        // 会立即终止虚拟机。若该设备不存在（例如不在 QEMU 中），写入是无害的。
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
    kprintln!("[kernel] PANIC: {info}");
    finish(false)
}
