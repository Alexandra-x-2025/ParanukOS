//! ParanukOS 引导器与内核之间共享的 `BootInfo` 结构与协议常量。
//!
//! 依据 [`docs/architecture/kernel_interface.md`] 的 §5（`BootInfo` v0）与 §7（退出码约定）。
//!
//! 放在独立 crate 的目的：引导器与内核**共用同一份定义**，避免两侧手写 `repr(C)`
//! 结构体静默漂移——那种故障的表现是"内核读到错位的内存然后莫名崩"，极难排查。
//!
//! 该 crate 零依赖，既能被 `no_std` 的内核使用，也能在宿主平台跑单元测试
//! （包括钉住 ABI 偏移的测试）。
//!
//! [`docs/architecture/kernel_interface.md`]: https://github.com/Alexandra-x-2025/ParanukOS/blob/main/docs/architecture/kernel_interface.md

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use core::fmt;

/// `BootInfo` 的 magic：`"Paranuk"` 的 ASCII 字节 + 接口版本 `0x01`。
pub const BOOT_INFO_MAGIC: u64 = 0x5061_7261_6E75_6B01;

/// 本 crate 实现的接口版本（对应 `docs/architecture/user_mode.md` 的 v1）。
///
/// v1 = v0 的 88 字节前缀 + 4 个 `u64` 用户镜像字段（只追加，见 §5.5）。
/// 内核仍然接受 v0：那表示"没有用户载荷"。
pub const BOOT_INFO_VERSION: u32 = 1;

/// v0 的结构体大小（没有用户镜像字段）。
pub const BOOT_INFO_SIZE_V0: u32 = 88;

/// 引导器与内核之间传递的启动信息。布局由 `#[repr(C)]` 固定，字段偏移有单元测试钉住。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BootInfo {
    /// 必须等于 [`BOOT_INFO_MAGIC`]。
    pub magic: u64,
    /// 接口版本，当前为 [`BOOT_INFO_VERSION`]。
    pub version: u32,
    /// 本结构体的总字节数（v0 = 88）。内核只读取此范围内的字段。
    pub size: u32,

    /// `EFI_MEMORY_DESCRIPTOR` 数组的物理地址；`mmap_len == 0` 表示不可用。
    pub mmap_ptr: u64,
    /// 描述符个数。
    pub mmap_len: u64,
    /// 描述符步长（x86-64 上通常为 40，必须以此遍历，**不能**用 `size_of`）。
    pub mmap_desc_size: u32,
    /// UEFI 描述符版本。
    pub mmap_desc_ver: u32,

    /// ACPI RSDP 物理地址；0 表示未找到。
    pub rsdp: u64,

    /// 内核镜像被装载到的物理基址。
    pub kernel_base: u64,
    /// 内核镜像字节数。
    pub kernel_size: u64,

    /// 内核栈顶（等于跳转时 `rsp + 8`）。
    pub stack_top: u64,
    /// 内核栈字节数。
    pub stack_size: u64,

    /// `isa-debug-exit` 端口；0 表示不启用（此时内核应在自检后 `hlt` 循环）。
    pub exit_port: u32,
    /// 填充，保持 8 字节对齐；新增字段从这里之后开始。
    pub _reserved: u32,

    // ---- v1：用户镜像（只追加，v0 的 88 字节前缀逐字节不变）----
    /// 已装载用户镜像的物理起始地址。
    pub user_phys: u64,
    /// 用户镜像字节数（页对齐区间）。
    pub user_size: u64,
    /// 与 `user_phys` 对应的虚拟地址（用户区基址）。
    pub user_vaddr: u64,
    /// 用户入口点（镜像内的一个虚拟地址）。
    pub user_entry: u64,
}

/// [`BootInfo::validate`] 的失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootInfoError {
    /// magic 不匹配。
    WrongMagic { found: u64 },
    /// 接口版本不受支持。
    UnsupportedVersion { found: u32, supported: u32 },
    /// `size` 小于该版本必须包含的字段。
    TooSmall { size: u32, expected: u32 },
}

impl fmt::Display for BootInfoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongMagic { found } => write!(
                f,
                "BootInfo magic 不匹配（得到 0x{found:016X}，期望 0x{BOOT_INFO_MAGIC:016X}）"
            ),
            Self::UnsupportedVersion { found, supported } => {
                write!(f, "BootInfo 版本不受支持（得到 {found}，支持 {supported}）")
            }
            Self::TooSmall { size, expected } => {
                write!(
                    f,
                    "BootInfo 过小（得到 {size} 字节，至少需要 {expected} 字节）"
                )
            }
        }
    }
}

impl BootInfo {
    /// 填好 magic / version / size 三个头部字段的实例。
    #[must_use]
    pub fn new() -> Self {
        Self {
            magic: BOOT_INFO_MAGIC,
            version: BOOT_INFO_VERSION,
            size: core::mem::size_of::<Self>() as u32,
            ..Self::default()
        }
    }

    /// 校验 magic、version 与 size。内核必须在做任何其他事情之前调用它。
    ///
    /// **同时接受 v0 与 v1**（`user_mode.md` §3，决策 #42）：v0 表示"没有用户载荷"。只检查头部：
    /// 其余字段的可用性由各自的约定表达（例如 `mmap_len == 0` 表示内存图不可用、
    /// [`Self::has_user_image`] 为假表示没有服务可跑）。
    pub fn validate(&self) -> Result<(), BootInfoError> {
        if self.magic != BOOT_INFO_MAGIC {
            return Err(BootInfoError::WrongMagic { found: self.magic });
        }
        // v0 只需要 88 字节；v1 在其之上追加，因此用"该版本必须包含多少"来判断。
        let expected = match self.version {
            0 => BOOT_INFO_SIZE_V0,
            BOOT_INFO_VERSION => core::mem::size_of::<Self>() as u32,
            other => {
                return Err(BootInfoError::UnsupportedVersion {
                    found: other,
                    supported: BOOT_INFO_VERSION,
                });
            }
        };
        if self.size < expected {
            return Err(BootInfoError::TooSmall {
                size: self.size,
                expected,
            });
        }
        Ok(())
    }

    /// 是否带有可运行的用户镜像（v1 且四个字段都有效）。
    #[must_use]
    pub fn has_user_image(&self) -> bool {
        self.version >= 1
            && self.size >= core::mem::size_of::<Self>() as u32
            && self.user_phys != 0
            && self.user_size != 0
            && self.user_vaddr != 0
            && self.user_entry != 0
    }

    /// 用户镜像区间 `[user_vaddr, user_vaddr + user_size)` 结束地址。
    #[must_use]
    pub fn user_vaddr_end(&self) -> u64 {
        self.user_vaddr.saturating_add(self.user_size)
    }

    /// 内存图是否可用。
    #[must_use]
    pub fn has_memory_map(&self) -> bool {
        self.mmap_ptr != 0 && self.mmap_len != 0 && self.mmap_desc_size != 0
    }

    /// 退出端口是否启用（测试模式）。
    #[must_use]
    pub fn exit_enabled(&self) -> bool {
        self.exit_port != 0
    }
}

// ---------------------------------------------------------------------------
// §7 可观测通道与退出码约定
// ---------------------------------------------------------------------------

/// `isa-debug-exit` 的默认 I/O 端口。QEMU 8.2 起该设备默认 iobase 为 `0x501`，
/// 因此命令行必须显式写成 `-device isa-debug-exit,iobase=0xf4,iosize=0x04`。
pub const DEBUG_EXIT_PORT: u16 = 0xF4;

/// 写入 `isa-debug-exit` 端口的值：引导器装载完成。
pub const EXIT_VALUE_BOOTLOADER_OK: u8 = 0x10;
/// 写入值：内核镜像装载失败。
pub const EXIT_VALUE_LOAD_FAILURE: u8 = 0x11;
/// 写入值：内核自检通过。
pub const EXIT_VALUE_KERNEL_OK: u8 = 0x12;
/// 写入值：内核自检失败（可预期的检查未通过，例如 `BootInfo` 非法）。
pub const EXIT_VALUE_KERNEL_FAILURE: u8 = 0x13;
/// 写入值：内核发生未处理异常或 panic（意外崩溃）。
///
/// 与 [`EXIT_VALUE_KERNEL_FAILURE`] 区分：39 表示"内核按预期判定失败"，
/// 41 表示"内核自己崩了"——两者的排查方向完全不同。
pub const EXIT_VALUE_KERNEL_FAULT: u8 = 0x14;
/// 写入值：内核内存初始化失败（页表、页帧分配器或内核堆）。
///
/// 与 [`EXIT_VALUE_KERNEL_FAILURE`]（39）区分：39 表示"引导器与内核之间的契约坏了"
/// （`BootInfo` 非法、内存图无法解析、没有 RSDP），43 表示"契约没问题，但内存子系统
/// 自己起不来"。依据 `docs/architecture/memory_subsystem.md` §7.1。
pub const EXIT_VALUE_KERNEL_MEMORY_FAILURE: u8 = 0x15;
/// 写入值：内核调度自检失败（GDT/TSS/IST、时钟中断、线程或锁未达预期）。
///
/// 与 41（内核崩溃）区分：41 是"内核自己崩了"，45 是"内核活着，但调度子系统没通过自检"。
/// 依据 `docs/architecture/threads_and_scheduling.md` §9.1。
pub const EXIT_VALUE_KERNEL_SCHED_FAILURE: u8 = 0x16;

/// QEMU 把写入 `isa-debug-exit` 的值转换为进程退出码：`(value << 1) | 1`。
#[must_use]
pub const fn qemu_exit_code(value: u8) -> i32 {
    ((value as i32) << 1) | 1
}

/// 引导器装载完成 → 33。
pub const EXIT_CODE_BOOTLOADER_OK: i32 = qemu_exit_code(EXIT_VALUE_BOOTLOADER_OK);
/// 内核镜像装载失败 → 35。
pub const EXIT_CODE_LOAD_FAILURE: i32 = qemu_exit_code(EXIT_VALUE_LOAD_FAILURE);
/// 内核自检通过 → 37。
pub const EXIT_CODE_KERNEL_OK: i32 = qemu_exit_code(EXIT_VALUE_KERNEL_OK);
/// 内核自检失败 → 39。
pub const EXIT_CODE_KERNEL_FAILURE: i32 = qemu_exit_code(EXIT_VALUE_KERNEL_FAILURE);
/// 内核未处理异常 / panic → 41。
pub const EXIT_CODE_KERNEL_FAULT: i32 = qemu_exit_code(EXIT_VALUE_KERNEL_FAULT);
/// 内核内存初始化失败 → 43。
pub const EXIT_CODE_KERNEL_MEMORY_FAILURE: i32 = qemu_exit_code(EXIT_VALUE_KERNEL_MEMORY_FAILURE);
/// 内核调度自检失败 → 45。
pub const EXIT_CODE_KERNEL_SCHED_FAILURE: i32 = qemu_exit_code(EXIT_VALUE_KERNEL_SCHED_FAILURE);

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    #[test]
    fn v0_layout_is_pinned() {
        // 这些偏移是对内核的 ABI 承诺：一旦改动，两侧必须同时改，并提升 version。
        assert_eq!(offset_of!(BootInfo, magic), 0);
        assert_eq!(offset_of!(BootInfo, version), 8);
        assert_eq!(offset_of!(BootInfo, size), 12);
        assert_eq!(offset_of!(BootInfo, mmap_ptr), 16);
        assert_eq!(offset_of!(BootInfo, mmap_len), 24);
        assert_eq!(offset_of!(BootInfo, mmap_desc_size), 32);
        assert_eq!(offset_of!(BootInfo, mmap_desc_ver), 36);
        assert_eq!(offset_of!(BootInfo, rsdp), 40);
        assert_eq!(offset_of!(BootInfo, kernel_base), 48);
        assert_eq!(offset_of!(BootInfo, kernel_size), 56);
        assert_eq!(offset_of!(BootInfo, stack_top), 64);
        assert_eq!(offset_of!(BootInfo, stack_size), 72);
        assert_eq!(offset_of!(BootInfo, exit_port), 80);
        assert_eq!(offset_of!(BootInfo, _reserved), 84);
        // v1 追加的字段：v0 的 88 字节前缀逐字节不变（只追加，见 §5.5）。
        assert_eq!(offset_of!(BootInfo, user_phys), 88);
        assert_eq!(offset_of!(BootInfo, user_size), 96);
        assert_eq!(offset_of!(BootInfo, user_vaddr), 104);
        assert_eq!(offset_of!(BootInfo, user_entry), 112);
        assert_eq!(size_of::<BootInfo>(), 120);
    }

    #[test]
    fn a_v0_boot_info_is_still_accepted() {
        // 老引导器（v0）必须仍然能启动新内核：v0 = 88 字节、没有用户字段。
        let v0 = BootInfo {
            version: 0,
            size: BOOT_INFO_SIZE_V0,
            ..BootInfo::new()
        };
        assert_eq!(v0.validate(), Ok(()));
        assert!(!v0.has_user_image(), "v0 表示没有用户载荷");
    }

    #[test]
    fn a_truncated_v1_is_rejected_and_a_future_version_too() {
        let truncated = BootInfo {
            version: 1,
            size: BOOT_INFO_SIZE_V0,
            ..BootInfo::new()
        };
        assert_eq!(
            truncated.validate(),
            Err(BootInfoError::TooSmall {
                size: BOOT_INFO_SIZE_V0,
                expected: 120
            })
        );
        let future = BootInfo {
            version: 2,
            ..BootInfo::new()
        };
        assert_eq!(
            future.validate(),
            Err(BootInfoError::UnsupportedVersion {
                found: 2,
                supported: 1
            })
        );
    }

    #[test]
    fn user_image_availability_requires_all_four_fields() {
        let base = BootInfo::new();
        assert!(!base.has_user_image(), "v1 的全零实例没有用户镜像");
        let complete = BootInfo {
            user_phys: 0x10_0000,
            user_size: 0x2000,
            user_vaddr: 0x1_0000_0000,
            user_entry: 0x1_0000_0100,
            ..base
        };
        assert!(complete.has_user_image());
        assert_eq!(complete.user_vaddr_end(), 0x1_0000_2000);
        for broken in [
            BootInfo { user_phys: 0, ..complete },
            BootInfo { user_size: 0, ..complete },
            BootInfo { user_vaddr: 0, ..complete },
            BootInfo { user_entry: 0, ..complete },
        ] {
            assert!(!broken.has_user_image());
        }
    }

    #[test]
    fn new_is_valid() {
        let info = BootInfo::new();
        assert_eq!(info.magic, BOOT_INFO_MAGIC);
        assert_eq!(info.version, BOOT_INFO_VERSION);
        assert_eq!(info.size, size_of::<BootInfo>() as u32);
        assert_eq!(info.validate(), Ok(()));
        assert!(!info.has_memory_map());
        assert!(!info.exit_enabled());
    }

    #[test]
    fn zeroed_magic_is_rejected() {
        // 内核可能被错误地跳转进来，此时 BootInfo 内容是垃圾（含全零），必须被拒绝
        let info = BootInfo::default();
        assert_eq!(info.validate(), Err(BootInfoError::WrongMagic { found: 0 }));
    }

    #[test]
    fn wrong_magic_is_rejected() {
        let info = BootInfo {
            magic: BOOT_INFO_MAGIC ^ 0xFFFF,
            ..BootInfo::new()
        };
        assert!(matches!(
            info.validate(),
            Err(BootInfoError::WrongMagic { .. })
        ));
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let info = BootInfo {
            version: BOOT_INFO_VERSION + 1,
            ..BootInfo::new()
        };
        assert_eq!(
            info.validate(),
            Err(BootInfoError::UnsupportedVersion {
                found: BOOT_INFO_VERSION + 1,
                supported: BOOT_INFO_VERSION,
            })
        );
    }

    #[test]
    fn too_small_size_is_rejected() {
        let info = BootInfo {
            size: (size_of::<BootInfo>() - 1) as u32,
            ..BootInfo::new()
        };
        assert!(matches!(
            info.validate(),
            Err(BootInfoError::TooSmall { .. })
        ));
    }

    #[test]
    fn larger_size_from_a_newer_loader_is_accepted() {
        // 只允许追加字段：更新的引导器给出更大的 size，旧内核应继续工作
        let info = BootInfo {
            size: (size_of::<BootInfo>() + 64) as u32,
            ..BootInfo::new()
        };
        assert_eq!(info.validate(), Ok(()));
    }

    #[test]
    fn exit_codes_match_the_documented_table() {
        // 文档 §7.2 的退出码表
        assert_eq!(EXIT_CODE_BOOTLOADER_OK, 33);
        assert_eq!(EXIT_CODE_LOAD_FAILURE, 35);
        assert_eq!(EXIT_CODE_KERNEL_OK, 37);
        assert_eq!(EXIT_CODE_KERNEL_FAILURE, 39);
        assert_eq!(EXIT_CODE_KERNEL_FAULT, 41);
        assert_eq!(EXIT_CODE_KERNEL_MEMORY_FAILURE, 43);
        assert_eq!(EXIT_CODE_KERNEL_SCHED_FAILURE, 45);
        assert_ne!(EXIT_CODE_KERNEL_FAULT, EXIT_CODE_KERNEL_SCHED_FAILURE);
        assert_ne!(
            EXIT_CODE_KERNEL_MEMORY_FAILURE,
            EXIT_CODE_KERNEL_SCHED_FAILURE
        );
        // "自检失败"与"内核崩溃"也必须可区分
        assert_ne!(EXIT_CODE_KERNEL_FAILURE, EXIT_CODE_KERNEL_FAULT);
        // 交接契约坏了（39）与内存子系统起不来（43）的排查方向不同
        assert_ne!(EXIT_CODE_KERNEL_FAILURE, EXIT_CODE_KERNEL_MEMORY_FAILURE);
        assert_ne!(EXIT_CODE_KERNEL_FAULT, EXIT_CODE_KERNEL_MEMORY_FAILURE);
        // 33 与 37 必须不同，否则测试无法区分"引导器装完"与"内核真的跑了"
        assert_ne!(EXIT_CODE_BOOTLOADER_OK, EXIT_CODE_KERNEL_OK);
        assert_eq!(DEBUG_EXIT_PORT, 0xF4);
    }

    #[test]
    fn memory_map_availability_requires_all_three_fields() {
        let base = BootInfo::new();
        assert!(!base.has_memory_map());
        let only_ptr = BootInfo {
            mmap_ptr: 0x1000,
            ..base
        };
        assert!(!only_ptr.has_memory_map());
        let complete = BootInfo {
            mmap_ptr: 0x1000,
            mmap_len: 12,
            mmap_desc_size: 40,
            ..base
        };
        assert!(complete.has_memory_map());
    }
}
