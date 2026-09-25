#![no_std]
#![no_main]

//! ParanukOS 引导器入口。
//!
//! 基于 uefi-rs 0.39 的全局 API（global API）模型：
//! 不再手动持有 `SystemTable` / `BootServices`，而是通过
//! `uefi::boot`、`uefi::system`、`uefi::runtime` 下的自由函数访问。
//!
//! panic 处理器由 `uefi` 的 `panic_handler` 特性提供（打印信息并复位），
//! 因此这里不再自定义 `#[panic_handler]`（否则会重复定义）。
//!
//! 启用 `qemu-exit` 特性时，引导器改为通过 QEMU 的 `isa-debug-exit` 设备
//! 报告结果，使 `tests/smoke.sh` 可以断言精确的退出码，而不是猜日志文本。

pub mod bootloader;

use core::fmt::Write;

use uefi::prelude::*;
use uefi::proto::console::text::Color;

use bootloader::fs_loader::FsLoader;

/// `isa-debug-exit` 的 I/O 端口。QEMU 以 `(value << 1) | 1` 作为进程退出码。
#[cfg(feature = "qemu-exit")]
const QEMU_DEBUG_EXIT_PORT: u16 = 0xF4;

/// 引导成功时写入的值 → QEMU 退出码 33。
#[cfg(feature = "qemu-exit")]
const QEMU_EXIT_SUCCESS: u8 = 0x10;

/// 内核加载失败时写入的值 → QEMU 退出码 35。
#[cfg(feature = "qemu-exit")]
const QEMU_EXIT_FAILURE: u8 = 0x11;

/// 通过 QEMU 的 `isa-debug-exit` 设备退出，让自动化测试能断言精确退出码。
///
/// 该设备只在 QEMU 命令行显式传入 `-device isa-debug-exit` 时存在；
/// 对未映射的 I/O 端口执行 `out` 指令在 x86 上没有副作用。
#[cfg(feature = "qemu-exit")]
fn exit_qemu(value: u8) -> ! {
    // SAFETY: 0xF4 是 isa-debug-exit 的端口号；写入后 QEMU 立即退出，不会返回。
    // 对不存在的设备写入该端口是无害的。
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") QEMU_DEBUG_EXIT_PORT,
            in("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
    loop {
        core::hint::spin_loop();
    }
}

#[entry]
fn main() -> Status {
    // 初始化 uefi 助手（logger 等）
    uefi::helpers::init().unwrap();

    // 关闭固件看门狗。默认超时（通常 5 分钟）到点后固件会强制复位机器，
    // 对长时间调试极不友好；这里容忍个别固件不支持的情况。
    if let Err(err) = uefi::boot::set_watchdog_timer(0, 0, None) {
        log::warn!("无法关闭看门狗定时器: {err:?}");
    }

    let status = uefi::system::with_stdout(|stdout| {
        let _ = stdout.clear();
        let _ = stdout.set_color(Color::LightCyan, Color::Black);
        let _ = writeln!(stdout, "========================================");
        let _ = writeln!(stdout, "   ParanukOS Integrated Bootloader      ");
        let _ = writeln!(stdout, "========================================");
        let _ = stdout.set_color(Color::White, Color::Black);

        let _ = writeln!(stdout, "[*] 正在从 ESP 载入内核镜像 ...");
        match FsLoader::load_kernel(stdout) {
            Ok(kernel) => {
                let _ = stdout.set_color(Color::LightGreen, Color::Black);
                let _ = writeln!(stdout, "[+ SUCCESS] 内核镜像校验通过。");
                let _ = writeln!(
                    stdout,
                    "[+] 加载基址: 0x{:X} | 大小: {} 字节 | 入口: 0x{:X}",
                    kernel.base_address, kernel.size, kernel.entry_point
                );
                Status::SUCCESS
            }
            Err(err) => {
                let _ = stdout.set_color(Color::Red, Color::Black);
                let _ = writeln!(stdout, "[-] 内核加载失败: {err}");
                err.status()
            }
        }
    });

    if status != Status::SUCCESS {
        // 自动化测试下用退出码报告失败；否则把状态码交还给固件。
        #[cfg(feature = "qemu-exit")]
        exit_qemu(QEMU_EXIT_FAILURE);

        #[cfg(not(feature = "qemu-exit"))]
        return status;
    }

    uefi::system::with_stdout(|stdout| {
        let _ = stdout.set_color(Color::Yellow, Color::Black);
        #[cfg(feature = "qemu-exit")]
        let _ = writeln!(
            stdout,
            "[!] 测试模式：以退出码报告结果（尚未实现向内核跳转）。"
        );
        #[cfg(not(feature = "qemu-exit"))]
        let _ = writeln!(stdout, "[!] 尚未实现向内核跳转，进入自旋等待。");
    });

    // 自动化测试下按约定退出码结束；正常模式下自旋等待（尚未实现跳转内核）。
    #[cfg(feature = "qemu-exit")]
    exit_qemu(QEMU_EXIT_SUCCESS);

    #[cfg(not(feature = "qemu-exit"))]
    loop {
        core::hint::spin_loop();
    }
}
