// main.rs

#![no_std]
#![no_main]

use core::panic::PanicInfo;

/// 这个函数将在 panic 时被调用
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}

/// 内核入口点
#[no_mangle] // 防止 Rust 修改函数名
pub extern "C" fn _start() -> ! {
    // 这里的逻辑将是内核启动的第一步
    loop {}
}
