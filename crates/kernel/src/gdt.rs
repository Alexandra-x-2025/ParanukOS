//! 内核自己的 GDT、TSS 与 IST 栈（`threads_and_scheduling.md` §3）。
//!
//! M1 有意沿用固件的 GDT；M3 装上自己的，因为抢占会把三重故障的代价放大：栈不可用时发生的
//! `#DF` 只能靠 IST 救回来。
//!
//! 选择子保持惯例：`0x08` 内核代码、`0x10` 内核数据、`0x18` TSS——因此 `lgdt` 之后正在执行的
//! 代码仍然有效。

use core::arch::asm;
use core::cell::UnsafeCell;
use core::mem::size_of;

/// 内核代码段选择子。
pub const KERNEL_CODE: u16 = 0x08;
/// 内核数据段选择子。
pub const KERNEL_DATA: u16 = 0x10;
/// TSS 选择子。
pub const TSS_SELECTOR: u16 = 0x18;
/// `#DF` 使用的 IST 索引（IST1）。
pub const IST_DOUBLE_FAULT: u8 = 1;
/// NMI 使用的 IST 索引（IST2）。
pub const IST_NMI: u8 = 2;
/// `#DF` 的向量号（IDT 里按它挂 IST 门）。
pub const IST_VECTOR_DOUBLE_FAULT: u8 = 8;
/// NMI 的向量号。
pub const IST_VECTOR_NMI: u8 = 2;

// 选择子是硬编码进汇编字面量的；这里钉住它们与常量一致。
const _: () = assert!(KERNEL_CODE == 0x08);
const _: () = assert!(KERNEL_DATA == 0x10);
const _: () = assert!(TSS_SELECTOR == 0x18);

/// 64 位 TSS（104 字节）。
#[repr(C, packed)]
struct Tss {
    _reserved0: u32,
    rsp0: u64,
    rsp1: u64,
    rsp2: u64,
    _reserved1: u64,
    /// IST1..IST7 的栈顶地址。
    ist: [u64; 7],
    _reserved2: u64,
    _reserved3: u16,
    /// I/O 权限位图偏移；设为结构体长度表示"没有位图"。
    iomap_base: u16,
}

impl Tss {
    const fn new() -> Self {
        Self {
            _reserved0: 0,
            rsp0: 0,
            rsp1: 0,
            rsp2: 0,
            _reserved1: 0,
            ist: [0; 7],
            _reserved2: 0,
            _reserved3: 0,
            iomap_base: size_of::<Self>() as u16,
        }
    }
}

/// GDT：null、内核代码、内核数据、TSS（2 项）、3 个给 M4 预留的槽位。
#[repr(C, align(16))]
struct Gdt([u64; 8]);

/// GDT/TSS 需要 `Sync`：单核初始化阶段写入一次，之后只被 CPU 读取。
struct GdtHolder {
    gdt: UnsafeCell<Gdt>,
    tss: UnsafeCell<Tss>,
}

// SAFETY: 只在单核初始化阶段写入一次；之后 CPU 只读，我们不再修改。
unsafe impl Sync for GdtHolder {}

static GDT: GdtHolder = GdtHolder {
    gdt: UnsafeCell::new(Gdt([0; 8])),
    tss: UnsafeCell::new(Tss::new()),
};

/// `lgdt`/`sgdt` 的操作数（10 字节，packed）。
#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

/// 平坦 64 位代码段。
const fn code_descriptor() -> u64 {
    // limit=0xFFFF、access=0x9A（P=1,DPL=0,S=1,代码/可读）、flags=0xAF（G=1,L=1）
    0x00AF_9A00_0000_FFFF
}

/// 平坦数据段。
const fn data_descriptor() -> u64 {
    // access=0x92（P=1,DPL=0,S=1,数据/可写）、flags=0xCF（G=1,D/B=1）
    0x00CF_9200_0000_FFFF
}

/// TSS 描述符占两个 u64（16 字节）。
fn tss_descriptor(base: u64) -> (u64, u64) {
    let limit = (size_of::<Tss>() - 1) as u64;
    let low = (limit & 0xFFFF)
        | ((base & 0x00FF_FFFF) << 16)
        | (0x89u64 << 40) // P=1, DPL=0, type=0b1001（64 位可用 TSS）
        | (((limit >> 16) & 0xF) << 48)
        | (((base >> 24) & 0xFF) << 56);
    (low, base >> 32)
}

/// 安装 GDT、填充 TSS 并装载任务寄存器。
///
/// `ist_double_fault` / `ist_nmi` 是两个 IST 栈的**栈顶**地址。
///
/// # Safety
/// 必须在单核、中断关闭、且还没有任何中断依赖新 GDT 之前调用一次。
///
/// # Errors
/// 装载后 `sgdt` 读回的基址与大小不符时返回错误（这种情况通常已经三重故障了，因此这条检查
/// 主要是给"回归时至少能看见原因"用的）。
pub unsafe fn install(ist_double_fault: u64, ist_nmi: u64) -> Result<(), &'static str> {
    let gdt = unsafe { &mut *GDT.gdt.get() };
    let tss = unsafe { &mut *GDT.tss.get() };
    let tss_base = tss as *mut Tss as u64;

    tss.ist[IST_DOUBLE_FAULT as usize - 1] = ist_double_fault;
    tss.ist[IST_NMI as usize - 1] = ist_nmi;

    let (tss_low, tss_high) = tss_descriptor(tss_base);
    gdt.0 = [
        0,
        code_descriptor(),
        data_descriptor(),
        tss_low,
        tss_high,
        0, // M4：用户代码
        0, // M4：用户数据
        0, // 保留
    ];

    let gdtr = DescriptorTablePointer {
        limit: (size_of::<Gdt>() - 1) as u16,
        base: gdt as *const Gdt as u64,
    };

    // SAFETY: gdtr 指向已填好的静态 GDT；随后立即用远返回重新装载 CS，并把数据段寄存器
    // 指向新的内核数据段。`retfq` 会弹掉前面压入的选择子与偏移，栈收支平衡。
    unsafe {
        asm!(
            "lgdt [{gdtr}]",
            "push 0x08",                 // 内核代码选择子
            "lea {target}, [rip + 2f]",  // 远返回的偏移
            "push {target}",
            "retfq",
            "2:",
            "mov ax, 0x10",
            "mov ds, ax",
            "mov es, ax",
            "mov ss, ax",
            "xor eax, eax",
            "mov fs, ax",
            "mov gs, ax",
            gdtr = in(reg) &gdtr,
            target = out(reg) _,
            out("rax") _,
            options(preserves_flags),
        );
    }

    // SAFETY: 装载 TSS：IST 只有在内核读取到有效 TR 时才生效。
    unsafe {
        asm!("ltr {selector:x}", selector = in(reg) TSS_SELECTOR, options(nomem, nostack, preserves_flags));
    }

    // 验证：读回 GDTR 必须就是我们刚装载的那张表。
    let mut check = DescriptorTablePointer { limit: 0, base: 0 };
    // SAFETY: sgdt 写 10 字节到 check。
    unsafe {
        asm!("sgdt [{}]", in(reg) &mut check, options(nostack, preserves_flags));
    }
    if check.base != gdtr.base || check.limit != gdtr.limit {
        return Err("sgdt 读回的 GDTR 与装载的不一致");
    }

    Ok(())
}

/// 当前 `CS`。
#[must_use]
pub fn current_cs() -> u16 {
    let cs: u16;
    // SAFETY: 只读段寄存器。
    unsafe {
        asm!("mov {0:x}, cs", out(reg) cs, options(nomem, nostack, preserves_flags));
    }
    cs
}

/// 当前 `SS`。
#[must_use]
pub fn current_ss() -> u16 {
    let ss: u16;
    // SAFETY: 只读段寄存器。
    unsafe {
        asm!("mov {0:x}, ss", out(reg) ss, options(nomem, nostack, preserves_flags));
    }
    ss
}

/// TSS 的地址（自检用：确认 IST 栈确实被填进了 TSS）。
#[must_use]
pub fn tss_address() -> u64 {
    GDT.tss.get() as u64
}

/// 读取 TSS 里某个 IST 条目的栈顶地址。
#[must_use]
pub fn ist_top(index: u8) -> u64 {
    if !(1..=7).contains(&index) {
        return 0;
    }
    // SAFETY: 只读；TSS 在初始化后不再改变。
    let tss = unsafe { &*GDT.tss.get() };
    tss.ist[index as usize - 1]
}
