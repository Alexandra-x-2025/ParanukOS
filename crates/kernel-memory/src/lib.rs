//! ParanukOS 内核内存子系统。
//!
//! 依据 [`docs/architecture/memory_subsystem.md`]（Milestone 2 接口）：
//!
//! * [`map`]：解析 UEFI 内存图（40/48 字节步长、未知类型视为非 RAM、溢出即判损坏）；
//! * [`paging`]：由内存图构建恒等映射的四级页表（纯逻辑，不碰 `CR3`）；
//! * [`frame`]：位图式物理页帧分配器（只发放 `EfiConventionalMemory`，最低地址优先）；
//! * [`heap`]：首次匹配空闲链表（块头在竞技场内，释放时合并）。
//!
//! 设计约束：
//! * **零依赖**、`no_std`：内核要链接它，同时它必须能在宿主平台被单元测试覆盖
//!   （内核二进制 `test = false`，见根 `Cargo.toml`）；
//! * **`#![forbid(unsafe_code)]`**：本 crate 只做算术、表项填充与切片内的块管理。构造内存图
//!   切片、读写 `CR3`、把堆偏移换算成指针等真正需要 `unsafe` 的动作留在内核侧，
//!   因此这里的每一行都能被宿主测试覆盖。
//!
//! [`docs/architecture/memory_subsystem.md`]: https://github.com/Alexandra-x-2025/ParanukOS/blob/main/docs/architecture/memory_subsystem.md

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod frame;
pub mod heap;
pub mod map;
pub mod paging;
