#![no_std]
#![no_main]

use uefi::prelude::*;
use uefi::proto::console::text::Color;
use core::fmt::Write;

#[entry]
fn main() -> Status {
    // 1. Initialize helpers (Logger) — no arguments in 0.41
    uefi::helpers::init().unwrap();

    // 2. Disable the watchdog timer via the boot-services free function
    uefi::boot::set_watchdog_timer(0, 0, None).unwrap();

    // 3. Print the ParanukOS boot banner to stdout (safe access)
    uefi::system::with_stdout(|stdout| {
        stdout.clear().unwrap();
        stdout.set_color(Color::LightCyan, Color::Black).unwrap();
        writeln!(stdout, "========================================").unwrap();
        writeln!(stdout, "   ParanukOS Next-Gen Kernel (v0.41)   ").unwrap();
        writeln!(stdout, "========================================").unwrap();

        stdout.set_color(Color::White, Color::Black).unwrap();
        writeln!(stdout, "[+] Booting on 2026 ultra-modern UEFI environment...").unwrap();
        writeln!(stdout, "[+] Powered by zero-dependency uefi-rs crate.").unwrap();
    });

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
