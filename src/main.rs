#![no_std]
#![no_main]

pub mod bootloader;

use uefi::prelude::*;
use uefi::proto::console::text::Color;
use core::fmt::Write;
use bootloader::fs_loader::FsLoader;

#[entry]
fn main() -> Status {
    // 1. 获取系统表 (使用最基础的初始化)
    let mut system_table = uefi::boot::system_table();
    
    // 初始化 Logger
    uefi::helpers::init(&mut system_table).unwrap();
    
    // 获取启动服务
    let boot_services = uefi::boot::boot_services();

    // 设置定时器 (忽略错误，防止某些固件不支持)
    let _ = boot_services.set_watchdog_timer(0, 0, None);

    let stdout = system_table.stdout();
    stdout.clear().unwrap();

    // 打印欢迎信息
    stdout.set_color(Color::LightCyan, Color::Black).unwrap();
    writeln!(stdout, "========================================").unwrap();
    writeln!(stdout, "   ParanukOS Integrated Bootloader      ").unwrap();
    writeln!(stdout, "========================================").unwrap();
    stdout.set_color(Color::White, Color::Black).unwrap();

    // 2. 调用 FsLoader 模块进行内核加载
    writeln!(stdout, "[+] Invoking FsLoader subsystem...").unwrap();
    match FsLoader::load_kernel(boot_services, &mut stdout) {
        Ok(kernel) => {
            stdout.set_color(Color::LightGreen, Color::Black).unwrap();
            writeln!(stdout, "[+ SUCCESS] Verification complete! Ready to transition to Kernel.").unwrap();
            writeln!(stdout, "[+] Kernel Info -> Entry: 0x{:X}, Size: {} bytes.", kernel.base_address, kernel.size).unwrap();
        }
        Err(e) => {
            stdout.set_color(Color::Red, Color::Black).unwrap();
            writeln!(stdout, "[-] FsLoader error terminated process: {:?}", e.status()).unwrap();
            return e.status();
        }
    }

    stdout.set_color(Color::White, Color::Black).unwrap();
    writeln!(stdout, "[+] Spinning forever in safe sandbox state.").unwrap();

    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
