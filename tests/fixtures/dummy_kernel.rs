//! 冒烟测试用的内核占位镜像。
//!
//! 引导器目前只校验 ELF 头并把字节复制进内存，不会跳转执行，
//! 因此这里只需是一个合法的 ELF64 / x86-64 / ET_EXEC 文件。
#![no_std]
#![no_main]

#[no_mangle]
pub extern "C" fn _start() -> ! {
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
