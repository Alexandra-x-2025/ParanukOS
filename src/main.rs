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

pub mod bootloader;

use core::fmt::Write;

use uefi::prelude::*;
use uefi::proto::console::text::Color;

use bootloader::fs_loader::FsLoader;

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
                let _ = writeln!(stdout, "[-] 内核加载失败: {err:?}");
                err.status()
            }
        }
    });

    if status != Status::SUCCESS {
        return status;
    }

    // 尚未实现 exit_boot_services 与跳转到内核入口，
    // 因此这里明确停留在自旋状态，仅用于冒烟验证。
    uefi::system::with_stdout(|stdout| {
        let _ = stdout.set_color(Color::Yellow, Color::Black);
        let _ = writeln!(stdout, "[!] 尚未实现向内核跳转，进入自旋等待。");
    });

    loop {
        core::hint::spin_loop();
    }
}
