//! 最小 IDT：让内核在出错时能打印出"为什么错"，而不是三重故障重启。
//!
//! 为什么这是 M1 的第一件事（接口文档 §9）：没有 IDT 时，内核里的任何异常都会升级为
//! 三重故障，表现为 QEMU 静默重启——几乎无法调试。
//!
//! 设计取舍（M1 有意保持最小）：
//! * **不建 GDT**：中断门的目标选择子直接取当前 `CS`（我们已在 ring 0，异常处理不需要
//!   特权级切换）。等 M4 引入用户态时才需要自建 GDT/TSS。
//! * **不用 IST**：`#DF`（双重故障）仍在当前栈上处理。真正需要 IST 的场景（栈损坏导致的
//!   双重故障）留给后续里程碑。
//! * **处理器不返回**：异常即视为内核崩溃，打印诊断后以退出码 41 结束，因此不需要保存/恢复
//!   通用寄存器，公共入口非常短。

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

    /// 构造一个 ring 0 中断门。`selector` 取当前 `CS`。
    const fn new(handler: u64, selector: u16) -> Self {
        Self {
            offset_low: handler as u16,
            selector,
            ist: 0,
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

/// 读取当前 `CS`：我们用固件遗留的 GDT，异常处理不涉及特权级切换，因此直接沿用。
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

/// 安装 IDT：把 32 个异常向量指向各自的桩，然后 `lidt`。
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

    // SAFETY: 由调用者保证单核且只调用一次；此处独占可变访问。
    let idt = unsafe { &mut *IDT.0.get() };
    for (vector, handler) in handlers.iter().enumerate() {
        idt.0[vector] = Gate::new(*handler, selector);
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
