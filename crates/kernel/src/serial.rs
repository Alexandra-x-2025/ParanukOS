//! COM1（16550 UART）轮询输出。
//!
//! 为什么是串口而不是固件控制台：`exit_boot_services` 之后固件控制台不再可用
//! （见 docs/architecture/kernel_interface.md §6.2），而串口在交接后仍然工作，
//! 且 QEMU 的 `-nographic` 会把它直接送进测试的日志文件。

use core::fmt;

/// COM1 的 I/O 基址。
const COM1: u16 = 0x3F8;

// 16550 的寄存器偏移（DLAB 影响 0/1 的含义）
const REG_DATA: u16 = 0; // THR（写）/ RBR（读）；DLAB=1 时为除数低字节
const REG_IER: u16 = 1; // 中断使能；DLAB=1 时为除数高字节
const REG_FCR: u16 = 2; // FIFO 控制（写）
const REG_LCR: u16 = 3; // 线路控制
const REG_MCR: u16 = 4; // 调制解调器控制
const REG_LSR: u16 = 5; // 线路状态

/// 向 8 位 I/O 端口写一个字节。
///
/// # Safety
/// 调用者必须确保 `port` 是当前平台上一个合法且允许写入的 I/O 端口。
#[inline]
pub(crate) unsafe fn outb(port: u16, value: u8) {
    // SAFETY: 由调用者保证端口合法性；`out` 不访问内存。
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// 从 8 位 I/O 端口读一个字节。
///
/// # Safety
/// 调用者必须确保 `port` 是当前平台上一个合法且允许读取的 I/O 端口。
#[inline]
pub(crate) unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: 由调用者保证端口合法性；`in` 不访问内存。
    unsafe {
        core::arch::asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}

/// COM1 串口。
pub struct Serial;

impl Serial {
    /// 按标准 16550 寄存器布局把 COM1 初始化为 115200 8N1、轮询模式。
    ///
    /// # Safety
    /// 必须在 COM1 硬件存在时调用；在没有该硬件的机器上，写入这些端口是空操作，
    /// 但本函数不做探测。
    pub unsafe fn init() {
        // SAFETY: 端口均为 COM1 的标准寄存器偏移（常量）。
        unsafe {
            outb(COM1 + REG_IER, 0x00); // 关闭全部中断
            outb(COM1 + REG_LCR, 0x80); // 打开 DLAB
            outb(COM1 + REG_DATA, 0x01); // 除数低字节 = 1 → 115200
            outb(COM1 + REG_IER, 0x00); // 除数高字节 = 0
            outb(COM1 + REG_LCR, 0x03); // 8 位、无校验、1 停止位
            outb(COM1 + REG_FCR, 0xC7); // 使能并清空 FIFO，14 字节阈值
            outb(COM1 + REG_MCR, 0x03); // DTR | RTS
        }
    }

    fn wait_tx_ready() {
        // LSR bit 5 = THR 为空，可以写下一个字节
        while unsafe { inb(COM1 + REG_LSR) } & 0x20 == 0 {
            core::hint::spin_loop();
        }
    }

    /// 写一个字节；`\n` 会补成 `\r\n`，保证换行在终端上正确。
    pub fn write_byte(byte: u8) {
        if byte == b'\n' {
            Self::wait_tx_ready();
            // SAFETY: COM1 的数据寄存器（常量端口）。
            unsafe { outb(COM1 + REG_DATA, b'\r') };
        }
        Self::wait_tx_ready();
        // SAFETY: COM1 的数据寄存器（常量端口）。
        unsafe { outb(COM1 + REG_DATA, byte) };
    }

    /// 写一个字符串。
    pub fn write_str(text: &str) {
        for byte in text.bytes() {
            Self::write_byte(byte);
        }
    }
}

impl fmt::Write for Serial {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        Serial::write_str(text);
        Ok(())
    }
}
