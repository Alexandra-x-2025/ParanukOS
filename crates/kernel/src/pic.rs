//! 8259 PIC 与 PIT：纯端口 I/O 的时钟中断（`threads_and_scheduling.md` §4.1）。
//!
//! 为什么不用本地 APIC：`memory_subsystem.md` §3.4 明确 M2 不映射任何 MMIO，而 APIC 是 MMIO
//! 设备。PIC/PIT 只用端口 I/O，于是 M3 拿到时钟中断却完全不动分页设计。

use crate::serial::{inb, outb};

/// 主片命令 / 数据端口。
const PIC1: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
/// 从片命令 / 数据端口。
const PIC2: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

/// 主片重映射的向量基址（IRQ0 → 0x20）。
pub const PIC1_OFFSET: u8 = 0x20;
/// 从片重映射的向量基址（IRQ8 → 0x28）。
pub const PIC2_OFFSET: u8 = 0x28;
/// 时钟挂在 IRQ0。
pub const TIMER_IRQ: u8 = 0;
/// 时钟中断的向量号。
pub const TIMER_VECTOR: u8 = PIC1_OFFSET + TIMER_IRQ;
/// 伪中断（IRQ7）的向量号。
pub const SPURIOUS_IRQ7_VECTOR: u8 = PIC1_OFFSET + 7;

/// PIT 的输入频率（Hz）。
pub const PIT_FREQUENCY: u32 = 1_193_182;
/// 目标时钟频率。分频值必须取整，因此实际频率是 99.9984 Hz（文档 §4.1 如实记录）。
pub const TIMER_HZ: u32 = 100;

/// 端口写之后插入一个短暂的 I/O 延迟（写 0x80，传统做法）。
fn io_wait() {
    // SAFETY: 0x80 是历史遗留的"POST 诊断端口"，写它只消耗一个总线周期。
    unsafe { outb(0x80, 0) };
}

/// 重映射 PIC：主片 → `0x20..0x27`，从片 → `0x28..0x2F`，并把所有 IRQ 屏蔽。
///
/// # Safety
/// 必须在单核、中断关闭、且 IDT 里已经装好对应向量之后调用一次。
pub unsafe fn remap() {
    // SAFETY: 端口 I/O 与 ICW 时序由本函数保证（每一步之间插入 io_wait）。
    unsafe {
        outb(PIC1, 0x11); // ICW1：初始化 + 需要 ICW4
        io_wait();
        outb(PIC2, 0x11);
        io_wait();
        outb(PIC1_DATA, PIC1_OFFSET); // ICW2：主片向量基址
        io_wait();
        outb(PIC2_DATA, PIC2_OFFSET); // ICW2：从片向量基址
        io_wait();
        outb(PIC1_DATA, 0x04); // ICW3：从片接在 IRQ2
        io_wait();
        outb(PIC2_DATA, 0x02); // ICW3：从片的级联标识
        io_wait();
        outb(PIC1_DATA, 0x01); // ICW4：8086 模式
        io_wait();
        outb(PIC2_DATA, 0x01);
        io_wait();

        // 全部屏蔽；只发放行 IRQ0 时再解除。
        outb(PIC1_DATA, 0xFF);
        outb(PIC2_DATA, 0xFF);
    }
}

/// 解除某个 IRQ 的屏蔽。
///
/// # Safety
/// 该 IRQ 的向量必须在 IDT 里已安装；否则中断到来会跳到未定义入口。
pub unsafe fn unmask(irq: u8) {
    let (port, bit) = if irq < 8 {
        (PIC1_DATA, irq)
    } else {
        (PIC2_DATA, irq - 8)
    };
    // SAFETY: 读改写中断屏蔽字；调用方保证向量已就绪。
    unsafe {
        let mask = inb(port);
        outb(port, mask & !(1 << bit));
        if irq >= 8 {
            // 从片的 IRQ 还要放行主片的 IRQ2（级联）。
            let mask = inb(PIC1_DATA);
            outb(PIC1_DATA, mask & !(1 << 2));
        }
    }
}

/// 屏蔽某个 IRQ。
///
/// # Safety
/// 端口 I/O，须在单核下调用。
pub unsafe fn mask(irq: u8) {
    let (port, bit) = if irq < 8 {
        (PIC1_DATA, irq)
    } else {
        (PIC2_DATA, irq - 8)
    };
    // SAFETY: 读改写中断屏蔽字。
    unsafe {
        let value = inb(port);
        outb(port, value | (1 << bit));
    }
}

/// 发送 EOI。
///
/// # Safety
/// 端口 I/O，须在单核下调用。
pub unsafe fn eoi(irq: u8) {
    // SAFETY: 端口 I/O。
    unsafe {
        if irq >= 8 {
            outb(PIC2, 0x20);
        }
        outb(PIC1, 0x20);
    }
}

/// 读主片的中断服务寄存器（ISR），用来识别伪中断 IRQ7。
///
/// # Safety
/// 端口 I/O，须在单核下调用。
unsafe fn master_in_service() -> u8 {
    // SAFETY: OCW3 = 0x0B 选择"读 ISR"。
    unsafe {
        outb(PIC1, 0x0B);
        inb(PIC1)
    }
}

/// 处理一次伪中断（向量 0x27）。
///
/// 关键在于**不能无条件 EOI**：伪 IRQ7 并没有真的进入服务寄存器，贸然 EOI 会把一个真实
/// 在服务的中断（例如 IRQ0）弹出栈，从而丢掉后续时钟。
///
/// # Safety
/// 只能在向量 0x27 的处理器里调用。
pub unsafe fn handle_spurious_irq7() -> bool {
    // SAFETY: 端口 I/O。
    let spurious = unsafe { master_in_service() } & 0x80 == 0;
    if !spurious {
        // SAFETY: 真的 IRQ7，需要正常 EOI。
        unsafe { eoi(7) };
    }
    spurious
}

/// 把 PIT 通道 0 设为方波（模式 3），返回实际使用的分频值。
///
/// # Safety
/// 端口 I/O，须在单核下调用。
pub unsafe fn init_pit(hz: u32) -> u16 {
    // 四舍五入到最近的分频值：1193182 / 100 = 11931.82 → 11932（实际 99.9984 Hz）。
    let divisor = ((PIT_FREQUENCY + hz / 2) / hz).clamp(1, u32::from(u16::MAX)) as u16;
    // SAFETY: 端口 I/O；0x36 = 通道 0、低高字节、模式 3、二进制计数。
    unsafe {
        outb(0x43, 0x36);
        io_wait();
        outb(0x40, (divisor & 0xFF) as u8);
        io_wait();
        outb(0x40, (divisor >> 8) as u8);
        io_wait();
    }
    divisor
}

/// 初始化 PIC 与 PIT 并把时钟向量放行。
///
/// # Safety
/// 见 [`remap`]；另外调用方必须保证 IDT 已安装、且**尚未**开中断。
pub unsafe fn init() -> u16 {
    // SAFETY: 由调用方契约保证。
    unsafe {
        remap();
        let divisor = init_pit(TIMER_HZ);
        unmask(TIMER_IRQ);
        divisor
    }
}
