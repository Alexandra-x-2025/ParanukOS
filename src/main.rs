#![no_std]
#![no_main]

use core::fmt::Write;
use uefi::prelude::*;
use uefi::proto::console::text::Color;

#[entry]
fn main(_image_handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    // 1. 关闭看门狗定时器（防止主板误以为内核卡死并在 5 分钟后强行重启）
    system_table
        .boot_services()
        .set_watchdog_timer(0, 0, None)
        .unwrap();

    // 2. 清理屏幕并拿到输出控制台
    let stdout = system_table.stdout();
    stdout.clear().unwrap();

    // 3. 展现 ParanukOS 的开机致敬（设置前景色为亮青色）
    stdout.set_color(Color::LightCyan, Color::Black).unwrap();
    writeln!(stdout, "========================================").unwrap();
    writeln!(stdout, "       Welcome to ParanukOS (v0.1.0)    ").unwrap();
    writeln!(stdout, "========================================").unwrap();

    stdout.set_color(Color::White, Color::Black).unwrap();
    writeln!(stdout, "[+] Booting on modern x86_64 UEFI firmware...").unwrap();
    writeln!(stdout, "[+] System initialized successfully.").unwrap();

    // 4. 让 CPU 进入高效节能的死循环（防止程序退出）
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
