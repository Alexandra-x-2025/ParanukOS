#![no_std]
#![no_main]

//! ParanukOS 引导器入口。
//!
//! 流程（见 `docs/architecture/kernel_interface.md`）：
//! 1. 从 ESP 读取内核镜像并按 ELF 的 `PT_LOAD` 段装载（§3）；
//! 2. 分配内核栈与 `BootInfo` 页、读取 ACPI RSDP（§5/§6）；
//! 3. 打印最后一行 → `exit_boot_services` → 设置 `rsp`/`rdi` → 跳入内核（§6.1）。
//!
//! 成功路径**不会返回**：引导器把控制权交给内核，由内核报告自检结果（退出码 37/39）。
//! 因此"引导器装载完成"的 33 在 M0 之后不再出现在正常路径上（见 boot-info 中的说明）。

pub mod bootloader;

use core::fmt::Write;

#[cfg(feature = "qemu-exit")]
use boot_info::EXIT_VALUE_LOAD_FAILURE;
use uefi::prelude::*;
use uefi::proto::console::text::Color;

use bootloader::fs_loader::FsLoader;
use bootloader::handoff;

#[entry]
fn main() -> Status {
    // 初始化 uefi 助手（logger 等）
    uefi::helpers::init().unwrap();

    // 关闭固件看门狗（默认超时到点会强制复位机器）
    if let Err(err) = uefi::boot::set_watchdog_timer(0, 0, None) {
        log::warn!("无法关闭看门狗定时器: {err:?}");
    }

    // 只有失败路径会从这个闭包返回；成功路径在 handoff::enter_kernel 中永不返回。
    let status = uefi::system::with_stdout(|stdout| -> Status {
        let _ = stdout.clear();
        let _ = stdout.set_color(Color::LightCyan, Color::Black);
        let _ = writeln!(stdout, "========================================");
        let _ = writeln!(stdout, "   ParanukOS Bootloader (M0)             ");
        let _ = writeln!(stdout, "========================================");
        let _ = stdout.set_color(Color::White, Color::Black);

        let kernel = match FsLoader::load_kernel(stdout) {
            Ok(kernel) => kernel,
            Err(err) => {
                let _ = stdout.set_color(Color::Red, Color::Black);
                let _ = writeln!(stdout, "[-] 内核装载失败: {err}");
                return err.status();
            }
        };

        let prepared = match handoff::prepare(stdout) {
            Ok(prepared) => prepared,
            Err(err) => {
                let _ = stdout.set_color(Color::Red, Color::Black);
                let _ = writeln!(stdout, "[-] 交接准备失败: {err}");
                return err.status();
            }
        };

        let _ = stdout.set_color(Color::LightGreen, Color::Black);
        let _ = writeln!(
            stdout,
            "[+ SUCCESS] 内核镜像已装载: base=0x{:X} size={} entry=0x{:X} 段数={} (调试退出端口 {})",
            kernel.base,
            kernel.size,
            kernel.entry,
            kernel.segments,
            handoff::exit_port()
        );
        let _ = stdout.set_color(Color::White, Color::Black);

        handoff::enter_kernel(stdout, &kernel, prepared)
    });

    // 走到这里说明装载或准备失败（成功路径永不返回）。
    #[cfg(feature = "qemu-exit")]
    {
        log::error!("引导失败，状态 {status:?}");
        handoff::exit_qemu(EXIT_VALUE_LOAD_FAILURE);
    }

    #[cfg(not(feature = "qemu-exit"))]
    return status;
}
