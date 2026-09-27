//! ParanukOS 第一个用户态服务（Milestone 4a）。
//!
//! M4a 还没有系统调用表：它唯一能证明的事是"CPU 真的在 CPL 3 上执行我的代码"。
//! 因此它先自旋一会儿（让时钟必须**在用户态**抢占它一次，从而验证 `TSS.rsp0`），
//! 然后用哨兵调用号发起一次 `int 0x40`：内核记录 `cs`、结束这个线程并按 37/47 报告。
//!
//! 真正的 `write`/`exit`/`yield` 调用表在 M4b 补齐（`user_mode.md` §8）。

#![no_std]
#![no_main]

use core::panic::PanicInfo;

/// 自旋轮数：足够让 100 Hz 的时钟在用户态抢占若干次。
const SPIN_ROUNDS: u64 = 5_000_000;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    // 自旋：这一步本身没有输出，但内核能观察到"用户线程运行期间 tick 在推进"。
    let mut counter = 0u64;
    while counter < SPIN_ROUNDS {
        counter = counter.wrapping_add(1);
        core::hint::spin_loop();
    }

    #[cfg(feature = "inject-user-fault")]
    {
        // SAFETY: 故意解引用未映射地址；这正是要验证的 CPL 3 故障路径。
        unsafe {
            core::ptr::read_volatile(core::ptr::null::<u8>());
        }
    }

    // 哨兵：告诉内核"我在 CPL 3 上活着"。内核记录 cs 之后结束本线程。
    // SAFETY: 没有参数；M4a 的内核不会返回，因此这之后不可达。
    unsafe {
        user_lib::syscall(user_lib::call::ALIVE, counter, 0, 0);
    }

    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    // 用户态 panic 没有输出通道（M4b 才有 write），直接死循环：内核会因为抢占继续，
    // 并在这个线程始终不退出时由自检报 47。
    loop {
        core::hint::spin_loop();
    }
}
