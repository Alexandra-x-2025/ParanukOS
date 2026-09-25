//! ParanukOS 内核 —— Milestone 2a：自建恒等映射页表。
//!
//! 在 M0（自检 + 串口输出）与 M1（IDT / panic / 日志）之上：
//! * 解析 `BootInfo` 里的 UEFI 内存图，**自建四级恒等映射页表**并写入 `CR3`；
//! * 只映射与 RAM 区域相交的地址：非 RAM 与页 0 一律 not present，误访问即 `#PF`；
//! * 内存初始化失败以退出码 43 结束，与"交接契约坏了"的 39 区分。
//!
//! 依据 `docs/architecture/kernel_interface.md`（§4 入口 ABI、§7.2 退出码）与
//! `docs/architecture/memory_subsystem.md`（§3 页表、§4 内存图、§7 退出码 43）。

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

mod heap;
mod idt;
mod logging;
mod memory;
mod serial;

use boot_info::{
    BootInfo, EXIT_VALUE_KERNEL_FAILURE, EXIT_VALUE_KERNEL_FAULT, EXIT_VALUE_KERNEL_MEMORY_FAILURE,
    qemu_exit_code,
};
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

    kinfo!("ParanukOS kernel alive (Milestone 2)");

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

    // 5. 内存子系统：建立并安装内核自己的恒等映射页表（M2a）。
    //    从这一步起，内核不再依赖固件遗留的页表：MMIO 不会被映射，未映射地址一律 #PF。
    // SAFETY: 单核、中断已关闭、只调用一次；调用后不再使用任何引导服务。
    let tables = match unsafe { memory::install(boot_info) } {
        Ok(tables) => tables,
        Err(err) => {
            kerror!("memory init FAILED: {err}");
            finish(EXIT_VALUE_KERNEL_MEMORY_FAILURE);
        }
    };
    kinfo!(
        "paging: 恒等映射 {} 个 4 KiB 页 + {} 个 2 MiB 大块（{} MiB，不含空洞）",
        tables.pages,
        tables.blocks,
        tables.mapped_bytes >> 20
    );
    kinfo!(
        "paging: 上限 0x{:X}，页表 {} 页，CR3=0x{:X}",
        tables.limit,
        tables.tables_used,
        tables.pml4_phys
    );
    kinfo!("paging: 切换 CR3 后 BootInfo 仍可读（恒等映射覆盖了交接结构）");

    // 故障注入（仅测试）：页 0 按策略永不映射。固件的恒等映射通常把页 0 也映射了，
    // 因此"这一读会 #PF"就直接证明了生效的是内核自己的页表。
    // 正常情况下这一步**不会返回**（#PF → 41）；若真的返回，说明策略没生效，判为 43。
    #[cfg(feature = "inject-null-deref")]
    if probe_null_page() {
        kerror!("地址 0 可读：内核页表未按策略生效（页 0 本应 not present）");
        finish(EXIT_VALUE_KERNEL_MEMORY_FAILURE);
    }

    // 6. 页帧分配器与内核堆（M2b）。
    //    从这一步起内核有了真正能用的动态内存：Box / Vec / String 都可用。
    // SAFETY: 单核、中断已关闭；页表已安装，且本函数只调用一次。
    let layout = match unsafe { memory::init_allocators(boot_info, tables.limit) } {
        Ok(layout) => layout,
        Err(err) => {
            kerror!("memory init FAILED: {err}");
            finish(EXIT_VALUE_KERNEL_MEMORY_FAILURE);
        }
    };
    kinfo!(
        "frames: 管理 {} 帧（{} MiB），堆取走后空闲 {} 帧",
        layout.frames.managed,
        layout.frames.managed * 4096 / (1024 * 1024),
        layout.frames.free
    );
    kinfo!(
        "heap: 0x{:X}..0x{:X}（{} KiB，占用 {} 个连续页帧）",
        layout.heap.0,
        layout.heap.0 + layout.heap.1 as u64,
        layout.heap.1 / 1024,
        layout.heap.1 / 4096
    );

    // 7. 内存自检：写读回、不重叠、可合并、页帧计数（memory_subsystem.md §6.4）。
    if let Err(err) = memory::self_check(boot_info) {
        kerror!("memory self-check FAILED: {err}");
        finish(EXIT_VALUE_KERNEL_MEMORY_FAILURE);
    }
    let heap_stats = heap::stats();
    kinfo!(
        "heap: 自检 OK（全部释放后空闲 {} 字节 / {} 个块）",
        heap_stats.free_bytes,
        heap_stats.free_blocks
    );

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

/// 故障注入（仅测试）：读取按策略未映射的页 0。
///
/// 返回值只在"页 0 竟然可读"时产生——正常情况下这次读取触发 `#PF`，永不返回。
#[cfg(feature = "inject-null-deref")]
fn probe_null_page() -> bool {
    kinfo!("[inject] 故意读取未映射的页 0，用于验证内核页表确实生效");
    // SAFETY: 页 0 按决策 #21 未映射；读到值即说明内核页表未生效，由调用方报告失败。
    let value = unsafe { core::ptr::read_volatile(core::ptr::null::<u8>()) };
    kinfo!("[inject] 页 0 读到 {value}");
    true
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
