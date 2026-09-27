//! 最小 IDT：让内核在出错时能打印出"为什么错"，而不是三重故障重启。
//!
//! 为什么这是 M1 的第一件事（接口文档 §9）：没有 IDT 时，内核里的任何异常都会升级为
//! 三重故障，表现为 QEMU 静默重启——几乎无法调试。
//!
//! M3 的扩展（`threads_and_scheduling.md` §3、§4、§7.5）：
//! * 自建 GDT/TSS（见 `gdt.rs`），`#DF` 与 NMI 走 IST 栈——栈不可用时的双重故障现在可诊断；
//! * 增加 IRQ 桩（向量 0x20..0x2F）：**完整保存/恢复通用寄存器**，并在进入 Rust 之前
//!   `fxsave` 到本线程栈上，因此被抢占线程的 XMM 不会被接替者踩坏；
//! * `irq_common` 是 M3b 抢占切换的落点：处理器返回的 `rsp` 非 0 时换栈再 `iretq`。
//!
//! 异常仍然**不返回**：异常即内核崩溃，打印诊断后以退出码 41 结束。

use core::arch::{asm, naked_asm};
use core::cell::UnsafeCell;
use core::mem::size_of;

use boot_info::EXIT_VALUE_KERNEL_FAULT;

use crate::kerror;

/// 32 个 CPU 异常的名字（M1 只需能指认向量）。
const EXCEPTION_NAMES: [&str; 32] = [
    "#DE 除零",
    "#DB 调试",
    "NMI 不可屏蔽中断",
    "#BP 断点",
    "#OF 溢出",
    "#BR 越界",
    "#UD 非法指令",
    "#NM 设备不可用",
    "#DF 双重故障",
    "保留(9)",
    "#TS 无效 TSS",
    "#NP 段不存在",
    "#SS 栈段错误",
    "#GP 通用保护",
    "#PF 页错误",
    "保留(15)",
    "#MF x87 浮点",
    "#AC 对齐检查",
    "#MC 机器检查",
    "#XM SIMD 浮点",
    "#VE 虚拟化异常",
    "#CP 控制流保护",
    "保留(22)",
    "保留(23)",
    "保留(24)",
    "保留(25)",
    "保留(26)",
    "保留(27)",
    "#HV 虚拟化注入",
    "#VC SEV 异常",
    "#SX 安全异常",
    "保留(31)",
];

/// IDT 门描述符（16 字节）。
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Gate {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl Gate {
    /// 未使用的门（全零 = 不存在）。
    const fn missing() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attr: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    /// 构造一个 ring 0 中断门，并指定 IST 索引（0 = 使用当前栈）。
    const fn with_ist(handler: u64, selector: u16, ist: u8) -> Self {
        Self {
            offset_low: handler as u16,
            selector,
            ist,
            // P=1, DPL=0, 类型=0b1110（64 位中断门）
            type_attr: 0x8E,
            offset_mid: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            reserved: 0,
        }
    }
}

/// 整张 IDT（256 项）。
#[repr(C, align(16))]
struct Idt([Gate; 256]);

/// `static` 需要 `Sync`；单核、初始化阶段独占写入，因此用 `UnsafeCell` 包一层。
struct IdtHolder(UnsafeCell<Idt>);

// SAFETY: 只在单核初始化阶段写入一次，之后只被 CPU 读取。
unsafe impl Sync for IdtHolder {}

static IDT: IdtHolder = IdtHolder(UnsafeCell::new(Idt([Gate::missing(); 256])));

/// `lidt` 的操作数（10 字节，packed）。
#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

/// 异常发生时栈上的布局（与本模块的桩约定一致）。
///
/// 顺序：CPU 先压 `RIP/CS/RFLAGS/RSP/SS`（从 ring 0 触发时 `RSP/SS` 可能不存在，
/// 但 QEMU 下始终可读，M1 不做区分）；桩再压入 `vector` 与其后的 `error_code`。
#[repr(C)]
pub struct ExceptionFrame {
    /// 向量号（由桩压入）。
    pub vector: u64,
    /// 错误码（无错误码的异常由桩压入 0）。
    pub error_code: u64,
    /// 出错处的指令指针。
    pub rip: u64,
    /// 出错时的代码段。
    pub cs: u64,
    /// 出错时的标志寄存器。
    pub rflags: u64,
    /// 出错时的栈指针。
    pub rsp: u64,
    /// 出错时的栈段。
    pub ss: u64,
}

/// IRQ 桩保存的寄存器帧（与 `irq_common` 的压栈顺序严格对应）。
///
/// 从低地址到高地址：r15..r8、rdi、rsi、rbp、rbx、rdx、rcx、rax，然后是桩压入的
/// 占位错误码与向量号，最后是 CPU 压入的 `rip/cs/rflags/rsp/ss`。
#[repr(C)]
pub struct IrqFrame {
    /// r15。
    pub r15: u64,
    /// r14。
    pub r14: u64,
    /// r13。
    pub r13: u64,
    /// r12。
    pub r12: u64,
    /// r11。
    pub r11: u64,
    /// r10。
    pub r10: u64,
    /// r9。
    pub r9: u64,
    /// r8。
    pub r8: u64,
    /// rdi。
    pub rdi: u64,
    /// rsi。
    pub rsi: u64,
    /// rbp。
    pub rbp: u64,
    /// rbx。
    pub rbx: u64,
    /// rdx。
    pub rdx: u64,
    /// rcx。
    pub rcx: u64,
    /// rax。
    pub rax: u64,
    /// 向量号（由桩压入）。
    pub vector: u64,
    /// 占位错误码（IRQ 没有错误码，由桩压入 0）。
    pub error_code: u64,
    /// 被中断处的指令指针。
    pub rip: u64,
    /// 被中断处的代码段。
    pub cs: u64,
    /// 被中断处的标志寄存器。
    pub rflags: u64,
    /// 被中断处的栈指针。
    pub rsp: u64,
    /// 被中断处的栈段。
    pub ss: u64,
}

/// 时钟桩在本线程栈上为 FXSAVE 预留的区域（512 字节，16 字节对齐）。
///
/// 内核并不"与浮点无关"：编译器可能为大结构体搬移/类 `memcpy` 循环使用 XMM。被抢占线程的
/// XMM 若被踩坏，就是非确定性的数据损坏，因此时钟路径必须保存/恢复它（文档 §7.5）。
#[repr(C, align(16))]
pub struct FxSaveArea(#[allow(dead_code)] [u8; 512]);

/// 读取当前 `CS`：`lgdt` 之后选择子仍是 `0x08`，因此这里读到的就是内核代码段。
fn current_cs() -> u16 {
    let cs: u16;
    // SAFETY: 只读段寄存器，无副作用。
    unsafe {
        asm!("mov {0:x}, cs", out(reg) cs, options(nomem, nostack, preserves_flags));
    }
    cs
}

macro_rules! isr_stub {
    ($name:ident, $vector:expr, no_error) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            // 无错误码的异常：先压占位 0，再压向量号，得到统一的栈布局。
            naked_asm!(
                "push 0",
                "push {vector}",
                "jmp {common}",
                vector = const $vector,
                common = sym isr_common,
            );
        }
    };
    ($name:ident, $vector:expr, with_error) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            // CPU 已经压入错误码，这里只补向量号。
            naked_asm!(
                "push {vector}",
                "jmp {common}",
                vector = const $vector,
                common = sym isr_common,
            );
        }
    };
}

isr_stub!(isr_0, 0, no_error);
isr_stub!(isr_1, 1, no_error);
isr_stub!(isr_2, 2, no_error);
isr_stub!(isr_3, 3, no_error);
isr_stub!(isr_4, 4, no_error);
isr_stub!(isr_5, 5, no_error);
isr_stub!(isr_6, 6, no_error);
isr_stub!(isr_7, 7, no_error);
isr_stub!(isr_8, 8, with_error);
isr_stub!(isr_9, 9, no_error);
isr_stub!(isr_10, 10, with_error);
isr_stub!(isr_11, 11, with_error);
isr_stub!(isr_12, 12, with_error);
isr_stub!(isr_13, 13, with_error);
isr_stub!(isr_14, 14, with_error);
isr_stub!(isr_15, 15, no_error);
isr_stub!(isr_16, 16, no_error);
isr_stub!(isr_17, 17, with_error);
isr_stub!(isr_18, 18, no_error);
isr_stub!(isr_19, 19, no_error);
isr_stub!(isr_20, 20, no_error);
isr_stub!(isr_21, 21, with_error);
isr_stub!(isr_22, 22, no_error);
isr_stub!(isr_23, 23, no_error);
isr_stub!(isr_24, 24, no_error);
isr_stub!(isr_25, 25, no_error);
isr_stub!(isr_26, 26, no_error);
isr_stub!(isr_27, 27, no_error);
isr_stub!(isr_28, 28, no_error);
isr_stub!(isr_29, 29, with_error);
isr_stub!(isr_30, 30, with_error);
isr_stub!(isr_31, 31, no_error);

/// 生成一个 IRQ 桩：压占位错误码与向量号后进入公共入口（与异常帧布局一致）。
macro_rules! irq_stub {
    ($name:ident, $vector:expr) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            naked_asm!(
                "push 0",                 // IRQ 没有错误码：占位，保持帧布局统一
                "push {vector}",
                "jmp {common}",
                vector = const $vector,
                common = sym irq_common,
            );
        }
    };
}

// 向量 0x20..0x2F 对应 IRQ0..IRQ15（8259 重映射之后）。
irq_stub!(irq_32, 32);
irq_stub!(irq_33, 33);
irq_stub!(irq_34, 34);
irq_stub!(irq_35, 35);
irq_stub!(irq_36, 36);
irq_stub!(irq_37, 37);
irq_stub!(irq_38, 38);
irq_stub!(irq_39, 39);
irq_stub!(irq_40, 40);
irq_stub!(irq_41, 41);
irq_stub!(irq_42, 42);
irq_stub!(irq_43, 43);
irq_stub!(irq_44, 44);
irq_stub!(irq_45, 45);
irq_stub!(irq_46, 46);
irq_stub!(irq_47, 47);

/// IRQ 的公共入口：保存全部通用寄存器与 XMM，调用 Rust，然后（可能换栈）返回。
///
/// 这是 M3b 抢占切换的落点：处理器返回值非 0 时，那个值就是下一个线程保存好的栈指针，
/// 而它指向的栈内容与本函数开头压出的布局完全一致。
#[unsafe(naked)]
extern "C" fn irq_common() {
    naked_asm!(
        // 保存通用寄存器（与 `IrqFrame` 的字段顺序相反：最后压的在最低地址）
        "push rax",
        "push rcx",
        "push rdx",
        "push rbx",
        "push rbp",
        "push rsi",
        "push rdi",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        // rdi = &IrqFrame（在改栈之前取好）
        "mov rdi, rsp",
        // 16 字节对齐后在栈上留出 FXSAVE 区
        "and rsp, -16",
        "sub rsp, 512",
        "fxsave [rsp]",
        "mov rsi, rsp",
        "call {handler}",
        // 返回 0 = 回到被中断的上下文；非 0 = 该 rsp 是下一个线程保存好的状态
        "test rax, rax",
        "jz 3f",
        "mov rsp, rax",
        "3:",
        "fxrstor [rsp]",
        "add rsp, 512",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rbp",
        "pop rbx",
        "pop rdx",
        "pop rcx",
        "pop rax",
        // 弹掉桩压入的向量号与占位错误码
        "add rsp, 16",
        "iretq",
        handler = sym crate::sched::irq_handler,
    );
}

/// 所有异常的公共入口：把栈指针交给 Rust，然后**不再返回**。
#[unsafe(naked)]
extern "C" fn isr_common() {
    naked_asm!(
        // rsp 指向 ExceptionFrame（vector 在最低地址）
        "mov rdi, rsp",
        // 对齐到 16 字节后再 call（调用约定要求；我们不返回，因此无需还原 rsp）
        "and rsp, -16",
        "call {handler}",
        // exception_handler 声明为 `-> !`，正常不会回到这里
        "ud2",
        handler = sym exception_handler,
    );
}

/// 当前栈指针是否落在某个 IST 栈页内（自检证据：证明异常确实换到了 IST 栈）。
#[must_use]
pub fn current_stack_kind() -> &'static str {
    let rsp: u64;
    // SAFETY: 只读栈指针，无副作用。
    unsafe {
        asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags));
    }
    for (index, name) in [
        (crate::gdt::IST_DOUBLE_FAULT, "IST1 栈（#DF）"),
        (crate::gdt::IST_NMI, "IST2 栈（NMI）"),
    ] {
        let top = crate::gdt::ist_top(index);
        if top != 0 && rsp <= top && rsp > top.saturating_sub(4096) {
            return name;
        }
    }
    "当前栈（非 IST）"
}

/// 仅测试：把某个 IDT 门的选择子改成非法值，用来制造**真正的**双重故障。
///
/// 为什么需要它：`int $8` 不会压入错误码（软件中断一律不压），因此帧会被错位解析、诊断全是
/// 垃圾。真正的 #DF 是"交付某个异常时又出错"：把 #PF 与 #GP 的门都改坏，然后访问未映射地址，
/// CPU 就会在交付 #PF 失败、交付 #GP 又失败之后抛出一个**格式完整**的 #DF。
///
/// # Safety
/// 只能在会立刻触发故障的注入构建里调用。
#[cfg(feature = "inject-double-fault")]
pub unsafe fn corrupt_gate_for_injection(vector: usize) {
    // SAFETY: 由调用方契约保证；只改选择子，不改偏移。
    let idt = unsafe { &mut *IDT.0.get() };
    idt.0[vector].selector = 0x30; // 不存在的段
}

/// 异常处理器：打印诊断信息，然后以"内核崩溃"退出码结束。
extern "C" fn exception_handler(frame: &ExceptionFrame) -> ! {
    let name = EXCEPTION_NAMES
        .get(frame.vector as usize)
        .copied()
        .unwrap_or("未知向量");

    kerror!("未处理的 CPU 异常: {name} (vector {})", frame.vector);
    kerror!(
        "  error_code=0x{:X} rip=0x{:X} cs=0x{:X} rflags=0x{:X}",
        frame.error_code,
        frame.rip,
        frame.cs,
        frame.rflags
    );
    kerror!("  rsp=0x{:X} ss=0x{:X}", frame.rsp, frame.ss);
    // 直接测量"异常是否落在 IST 栈上"：这是 M3 的 IST 配置是否生效的直接证据。
    kerror!("  实际运行栈：{}", current_stack_kind());

    // #PF（14）时 CR2 保存出错的线性地址，是排查页错误最关键的信息
    if frame.vector == 14 {
        let cr2: u64;
        // SAFETY: 只读控制寄存器 CR2，无副作用。
        unsafe {
            asm!("mov {0}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags));
        }
        kerror!("  cr2=0x{cr2:X} (触发页错误的地址)");
    }

    crate::finish(EXIT_VALUE_KERNEL_FAULT)
}

/// IRQ 向量基址（8259 重映射之后）。
const IRQ_VECTOR_BASE: u8 = 0x20;

/// 安装 IDT：32 个异常向量 + 16 个 IRQ 向量，然后 `lidt`。
///
/// # Safety
/// 必须在单核、且尚未依赖异常处理的阶段调用一次。
pub unsafe fn install() {
    let selector = current_cs();

    // 函数指针到整数的转换只能在运行期做（const eval 不允许）
    let handlers: [u64; 32] = [
        isr_0 as *const () as u64,
        isr_1 as *const () as u64,
        isr_2 as *const () as u64,
        isr_3 as *const () as u64,
        isr_4 as *const () as u64,
        isr_5 as *const () as u64,
        isr_6 as *const () as u64,
        isr_7 as *const () as u64,
        isr_8 as *const () as u64,
        isr_9 as *const () as u64,
        isr_10 as *const () as u64,
        isr_11 as *const () as u64,
        isr_12 as *const () as u64,
        isr_13 as *const () as u64,
        isr_14 as *const () as u64,
        isr_15 as *const () as u64,
        isr_16 as *const () as u64,
        isr_17 as *const () as u64,
        isr_18 as *const () as u64,
        isr_19 as *const () as u64,
        isr_20 as *const () as u64,
        isr_21 as *const () as u64,
        isr_22 as *const () as u64,
        isr_23 as *const () as u64,
        isr_24 as *const () as u64,
        isr_25 as *const () as u64,
        isr_26 as *const () as u64,
        isr_27 as *const () as u64,
        isr_28 as *const () as u64,
        isr_29 as *const () as u64,
        isr_30 as *const () as u64,
        isr_31 as *const () as u64,
    ];

    // IRQ 桩（向量 0x20..0x2F）。
    let irq_handlers: [u64; 16] = [
        irq_32 as *const () as u64,
        irq_33 as *const () as u64,
        irq_34 as *const () as u64,
        irq_35 as *const () as u64,
        irq_36 as *const () as u64,
        irq_37 as *const () as u64,
        irq_38 as *const () as u64,
        irq_39 as *const () as u64,
        irq_40 as *const () as u64,
        irq_41 as *const () as u64,
        irq_42 as *const () as u64,
        irq_43 as *const () as u64,
        irq_44 as *const () as u64,
        irq_45 as *const () as u64,
        irq_46 as *const () as u64,
        irq_47 as *const () as u64,
    ];

    // SAFETY: 由调用者保证单核且只调用一次；此处独占可变访问。
    let idt = unsafe { &mut *IDT.0.get() };
    for (vector, handler) in handlers.iter().enumerate() {
        // `#DF`（8）与 NMI（2）走各自的 IST 栈：这两个异常必须能在"当前栈已经不可用"
        // 的情况下被处理（`threads_and_scheduling.md` §3）。
        let ist = match vector as u8 {
            crate::gdt::IST_VECTOR_DOUBLE_FAULT => crate::gdt::IST_DOUBLE_FAULT,
            crate::gdt::IST_VECTOR_NMI => crate::gdt::IST_NMI,
            _ => 0,
        };
        idt.0[vector] = Gate::with_ist(*handler, selector, ist);
    }
    for (offset, handler) in irq_handlers.iter().enumerate() {
        idt.0[IRQ_VECTOR_BASE as usize + offset] = Gate::with_ist(*handler, selector, 0);
    }

    let idtr = Idtr {
        limit: (size_of::<Idt>() - 1) as u16,
        base: idt as *const Idt as u64,
    };

    // SAFETY: idtr 指向已填好的静态 IDT；lidt 只读取它。
    unsafe {
        asm!("lidt [{}]", in(reg) &idtr, options(nostack, preserves_flags));
    }
}
