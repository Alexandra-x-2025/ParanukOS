//! ParanukOS 引导加载器。
//!
//! 该模块负责在 UEFI 环境下定位内核镜像、校验其格式并载入内存。
//! 目前尚未实现退出引导服务并向内核跳转。

pub mod block_io;
pub mod fs_loader;
