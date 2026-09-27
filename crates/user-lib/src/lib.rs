//! ParanukOS 用户态服务的最小运行库。
//!
//! 系统调用 ABI 只在这里定义一次（`user_mode.md` §8）：`rax` = 调用号，
//! `rdi`/`rsi`/`rdx`/`r10`/`r8` = 参数，返回值也在 `rax` 里；入口是 `int 0x40`。

#![no_std]

/// 系统调用号（与内核的分发表一一对应）。
pub mod call {
    /// `exit(status)`：永不返回。
    pub const EXIT: u64 = 0;
    /// `write(ptr, len) -> 已写字节数或 -1`。
    pub const WRITE: u64 = 1;
    /// `yield()`：让出 CPU。
    pub const YIELD: u64 = 2;
    /// M4a 的哨兵：证明服务确实运行在 CPL 3（内核只记录并结束线程，尚无调用表）。
    pub const ALIVE: u64 = 0xDEAD;
}

/// 发起一次系统调用。
///
/// # Safety
/// 参数必须符合被调用号的约定（尤其是 `write` 的指针必须是本进程用户区内的可读地址）；
/// 内核会校验，但调用方仍不应故意传入非法值。
#[inline]
pub unsafe fn syscall(number: u64, a: u64, b: u64, c: u64) -> u64 {
    let result: u64;
    // SAFETY: 由调用方保证参数合法；`int 0x40` 是内核安装的 DPL 3 中断门。
    unsafe {
        core::arch::asm!(
            "int 0x40",
            inlateout("rax") number => result,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            options(nostack, preserves_flags),
        );
    }
    result
}

/// 让出 CPU。
pub fn yield_now() {
    // SAFETY: 没有参数。
    unsafe {
        syscall(call::YIELD, 0, 0, 0);
    }
}

/// 退出服务。永不返回。
pub fn exit(status: u64) -> ! {
    // SAFETY: 没有参数；内核不会返回。
    unsafe {
        syscall(call::EXIT, status, 0, 0);
    }
    unreachable!("exit 系统调用不应返回")
}

/// 打印一段字节，返回实际写出的字节数（失败为 `u64::MAX`）。
pub fn write(bytes: &[u8]) -> u64 {
    // SAFETY: 指针与长度都指向本进程用户区内的可读内存。
    unsafe { syscall(call::WRITE, bytes.as_ptr() as u64, bytes.len() as u64, 0) }
}
