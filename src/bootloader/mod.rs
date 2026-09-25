//! ParanukOS 引导加载器。
//!
//! 职责（见 `docs/architecture/kernel_interface.md`）：
//! * `fs_loader`：从 ESP 读取内核镜像，按 ELF 的 `PT_LOAD` 段装载到内存（§3）；
//! * `handoff`：分配内核栈与 `BootInfo`、读取 RSDP、退出引导服务并跳入内核（§6）；
//! * `block_io`：块设备薄封装（后续里程碑使用，当前无调用方）。

pub mod block_io;
pub mod fs_loader;
pub mod handoff;
