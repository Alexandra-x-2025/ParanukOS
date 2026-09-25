# ParanukOS 内核接口定义与 Milestone 0

[English](kernel_interface.md) | [中文]

> **状态：接口 v0 已定稿。实现状态：未实现（M0 尚未开工）。**
>
> 本文只定义**引导器与内核之间的接口**，以及第一个可验证的里程碑。
> 微内核本体（IPC、能力模型、调度、用户态服务、Wasm）**不在本文范围**，
> 属于 [core_kernel_CN.md](core_kernel_CN.md) 中标注为「方向性构想」的部分。

## 1. 范围

### 1.1 本文定义
* 内核镜像的格式与装载规则（§3）
* 引导器跳转到内核的入口 ABI（§4）
* 两侧共享的数据结构 `BootInfo`（§5）
* 交接语义与交接后不可用的资源（§6）
* 可观测通道与退出码约定（§7）
* Milestone 0 的交付物与可证伪的验收标准（§8）
* 接口变更流程（§11）

### 1.2 本文不定义
分页与虚拟内存、用户态、IPC、能力模型、调度、SMP、Wasm/WASI、文件系统、ACPICA 表解析、
内核堆、图形输出、中断处理设施。

### 1.3 术语

| 术语 | 含义 |
|---|---|
| ESP | EFI System Partition，UEFI 固件加载 `\EFI\BOOT\BOOTX64.EFI` 的 FAT 分区 |
| `PT_LOAD` | ELF Program Header 中"需要被装入内存"的段 |
| `p_paddr` | 段的**物理**装载地址（ELF 规范：物理地址） |
| `p_vaddr` | 段的**虚拟**装载地址 |
| `ET_EXEC` | ELF 类型"可执行文件"（非 PIE）；PIE 是 `ET_DYN` |
| `LOADER_DATA` | UEFI 内存类型：引导器占用的内存，内核的分配器必须视为"已占用" |

## 2. 现状基线

### 2.1 已实现

| 能力 | 实现位置 |
|---|---|
| 可编译、可引导的 UEFI 应用（PE32+ / subsystem=10） | `src/main.rs`、`.cargo/config.toml` |
| 定位自身所在 ESP 卷 | `boot::get_image_file_system`（`src/bootloader/fs_loader.rs`） |
| 校验内核镜像为 ELF64 / 小端 / x86-64 / `ET_EXEC` 并取出入口点 | `crates/kernel-image` |
| 把镜像按页复制到内存（**任意地址**） | `boot::allocate_pages(AnyPages, …)` |
| 端到端冒烟测试、PE 结构校验、单元测试、CI | `tests/`、`.github/workflows/ci.yml` |
| 精确退出码（33 成功 / 35 装载失败 / 124 超时判失败） | `--features qemu-exit` + `run-qemu.sh` |

### 2.2 缺口（M0 要补的）

| 缺口 | 后果 |
|---|---|
| 不调用 `exit_boot_services`、不跳转 | 内核从未执行 |
| 复制到**任意**地址，与链接地址无关 | 一旦跳转必崩（按链接地址取指/取数） |
| 没有入口约定、没有 `BootInfo` | 内核即使被执行也拿不到内存图、RSDP、栈 |
| 没有内核侧代码 | 仓库内不存在内核 |

## 3. 内核镜像格式与装载规则

### 3.1 格式要求
ELF64、小端（`ELFDATA2LSB`）、`e_machine = EM_X86_64(62)`、`e_type = ET_EXEC(2)`。

### 3.2 装载算法
1. 校验 ELF 头（沿用 `crates/kernel-image`）；
2. 读取 Program Header 表（`e_phoff` / `e_phnum` / `e_phentsize`，`e_phentsize` 必须 ≥ 56）；
3. 对每个 `p_type == PT_LOAD` 的段：
   1. 要求 `p_memsz > 0`、`p_align` 为 2 的幂（或 0/1）；
   2. 以 `AllocateType::Address(p_paddr)` 申请 `ceil(p_memsz / 4096)` 页；
   3. 把 `p_filesz` 字节复制到 `p_paddr`；
   4. 把 `[p_paddr + p_filesz, p_paddr + p_memsz)` 清零（BSS）；
4. 所有被装载段的物理区间**不得重叠**，且不得覆盖引导器自身、`BootInfo`、内核栈、内存图缓冲；
5. 校验 `e_entry` 落在某个 `p_flags & PF_X` 的 `PT_LOAD` 段内。

### 3.3 拒绝条件（一律以退出码 35 结束，并打印具体原因）
ELF 头非法 / 非 `ET_EXEC` / Program Header 表越界 / 段类型非法 / 段重叠 /
`p_filesz > p_memsz` / 地址分配失败 / `e_entry` 不在可执行段内。

### 3.4 为什么这样定
* **必须按 `p_paddr` 装载**：非 PIE 内核访问全局变量、静态表、函数指针使用**绝对地址**；
  装载到别处再跳转，第一条访问 `.rodata`/`.bss` 的指令就会读到错误物理地址。
  按 `p_paddr` 装载是唯一与链接器保持一致的通用做法。
* **单一真相来源**：装载地址由内核自己的 Program Header 决定，不在引导器里写常量，
  避免"引导器常量"与"内核链接脚本"两处漂移。
* **不牺牲未来**：将来支持 higher-half 时契约演进为"按 `p_paddr` 装载、按 `p_vaddr` 映射、
  跳 `p_vaddr`"，属平滑演进。
* **只接受 `ET_EXEC` 是有意简化**：PIE（`ET_DYN`）需要重定位处理（`R_X86_64_RELATIVE`），
  M0 不做。这是一个明确的取舍，不是遗漏。

## 4. 入口 ABI v0

| 项 | 约定 |
|---|---|
| 入口地址 | ELF 的 `e_entry`（**不**通过符号名查找，避免依赖符号表） |
| 参数 | `rdi = BootInfo 的物理地址`（System V AMD64 调用约定） |
| 栈 | 引导器分配 64 KiB（16 页）并设置 `rsp` = 栈顶**下方 8 字节**（见 §4.1） |
| 中断 | 关闭（`cli`） |
| 页表 | 保持固件遗留的 identity map，**但内核不得依赖** |
| 返回值 | 入口不得返回（`-> !`）；返回即协议违规 |
| `BootInfo` 位置 | 由引导器分配一页并写入（见 §5.4） |

### 4.1 栈对齐的精确要求
SysV AMD64 规定函数入口处 `(rsp + 8) % 16 == 0`（因为 `call` 压入了 8 字节返回地址）。
我们使用 `jmp` 而非 `call`，因此必须人为满足该条件：**引导器设置 `rsp` 时令 `rsp % 16 == 8`**。

违反后果：内核中任何使用 SSE（`movaps` 等）的代码会触发 `#GP`，且发生在内核第一条
有意义的指令之前，现场几乎无法排查。内核侧允许在自己的入口再做一次对齐（双保险）。

### 4.2 推论：内核入口可以是纯 Rust
由于栈已由引导器准备好且满足对齐，内核侧第一版可以是：

```rust
#[unsafe(no_mangle)]
pub extern "C" fn kernel_main(boot_info: &BootInfo) -> ! { /* ... */ }
```

**不需要汇编 stub 或裸函数**——M0 中最容易写错的环节因此被消除。

## 5. `BootInfo` v0

### 5.1 定义
放在独立 crate（`crates/boot-info`），由引导器与内核共同依赖，避免两侧结构体静默漂移。
零依赖、`#![no_std]`，并提供宿主平台可运行的 `validate()` 单元测试。

```rust
#[repr(C)]
pub struct BootInfo {
    pub magic: u64,          // 0x5061_7261_6E75_6B01（"Paranuk" + 接口版本）
    pub version: u32,        // v0 = 0
    pub size: u32,           // 本结构体总字节数

    pub mmap_ptr: u64,       // EFI_MEMORY_DESCRIPTOR 数组的物理地址
    pub mmap_len: u64,       // 描述符个数
    pub mmap_desc_size: u32, // 描述符步长（见 §5.3）
    pub mmap_desc_ver: u32,  // UEFI 描述符版本

    pub rsdp: u64,           // ACPI RSDP 物理地址；0 = 未找到

    pub kernel_base: u64,    // 内核镜像装载后的物理基址
    pub kernel_size: u64,    // 内核镜像字节数

    pub stack_top: u64,      // 内核栈顶（= 跳转时 rsp + 8）
    pub stack_size: u64,     // 内核栈字节数

    pub exit_port: u32,      // isa-debug-exit 端口；0 = 不启用（见 §7.3）
    pub _reserved: u32,      // 保持 8 字节对齐

    // 追加字段一律从这里之后开始，并让 version+1、size 随之增大
}
```

### 5.2 字段语义
* `mmap_ptr` 指向的内存类型为 `LOADER_DATA`，**必须由内核保留**（见 §6.3）；`mmap_len = 0` 表示不可用。
* `rsdp` 由引导器在 `exit_boot_services` **之前**从 UEFI 配置表中取出：
  优先 `ConfigTableEntry::ACPI2_GUID`，回退 `ACPI_GUID`。
* `stack_top` 是栈顶地址（高地址端），`stack_size` 为总字节数；栈内存类型同样为 `LOADER_DATA`。

### 5.3 内存图描述符布局
内核侧**不得依赖 uefi-rs**，需按下表自行解析（x86-64 上共 **40 字节**）：

| 偏移 | 字段 | 类型 | 说明 |
|---|---|---|---|
| 0 | `Type` | u32 | `EFI_MEMORY_TYPE`（1=LoaderCode、7=Conventional、…） |
| 4 | *padding* | u32 | 对齐填充 |
| 8 | `PhysicalStart` | u64 | 物理起始地址 |
| 16 | `VirtualStart` | u64 | 未建映射前恒为 0 |
| 24 | `NumberOfPages` | u64 | **页数（4 KiB 页）** |
| 32 | `Attribute` | u64 | 内存属性位 |

> ⚠️ 必须以 `mmap_desc_size` 为步长遍历，**不能**用 `sizeof::<MemoryDescriptor>()`：
> UEFI 规范允许固件返回比标准结构更大的描述符。

### 5.4 `BootInfo` 自身的存放
引导器分配**一页**（`MemoryType::LOADER_DATA`），把 `BootInfo` 写入该页起始处，
把**该页物理地址**放进 `rdi`。这样：

* 结构体不会随引导器的栈/池分配而失效；
* 内核可以长期持有该指针；
* 内存类型为 `LOADER_DATA`，内核分配器不会回收它。

### 5.5 兼容规则
* 内核**只读 `size` 范围内的字段**；
* 新增字段一律追加到末尾，`version` 加一、`size` 增大；
* 删除/改动已有字段或语义 = 破坏性变更，需提升 major（`magic` 中的版本字段）；
* `magic` 或 `version` 不匹配时，内核必须打印原因并以**退出码 39** 结束，不得盲目继续。

## 6. 交接语义

### 6.1 交接顺序（引导器侧，顺序不可交换）
1. 解析 ELF 并按 §3.2 装载内核；
2. 分配内核栈（`LOADER_DATA`，64 KiB）；
3. 收集内存图（`boot::memory_map`）并保留其缓冲；
4. 读取 RSDP（`system::with_config_table`）；
5. 分配一页并写入 `BootInfo`；
6. **打印最后一行**（此后固件控制台不再可用）；
7. `unsafe { boot::exit_boot_services(Some(MemoryType::LOADER_DATA)) }`；
8. `cli`；
9. 设置 `rsp = stack_top - 8`（满足 §4.1）；
10. `rdi = BootInfo 物理地址`；
11. `jmp e_entry`。

### 6.2 交接后不可用的资源

| 资源 | 交接后 |
|---|---|
| Boot Services | ❌ 全部失效。含 `uefi` crate 的 logger、`println!`、`system::with_stdout`（其内部断言 boot services 仍有效）、任何 `allocate_*` |
| 池分配器 | ❌ 失效；**不得**再分配。带 `Drop` 的类型必须提前释放或 `mem::forget` |
| `BOOT_SERVICES_CODE` / `BOOT_SERVICES_DATA` 内存 | 变为**空闲内存**（含引导器自身的执行映像 —— 交接后**不可回调引导器**） |
| Runtime Services | ✅ 仍可用（`ResetSystem`、`GetTime`、变量服务） |
| 固件的 GDT / IDT | ❌ 不再有效。内核安装自己的 IDT 之前，任何异常/中断都可能导致三重故障重启 |
| 页表 | ⚠️ 通常仍是 identity map，但**不保证** |

### 6.3 交接后的内存归属
内核的物理分配器**必须把 `LOADER_DATA` 视为已占用**。至少包含：
内核镜像（§3.2 申请的页）、内核栈、内存图缓冲、`BootInfo` 所在页。

### 6.4 失败语义（重要）
`uefi::boot::exit_boot_services` 的失败**不可恢复**：uefi-rs 的实现会**直接重置系统**，
而不是返回错误。因此不存在"退出引导服务失败 → 用退出码优雅报告"这条路径。
测试必须把"机器被 reset"判定为**失败**，而不能因为"QEMU 正常退出"就认为成功——
这正是冒烟测试使用精确退出码的原因。

## 7. 可观测通道与退出码

### 7.1 串口（内核的第一条输出通道）
COM1 = `0x3F8`，**轮询写**（不依赖中断）。理由：固件控制台在 `exit_boot_services` 后不可用，
而串口在退出后仍可用，且 QEMU 的 `-nographic` 会把串口输出直接送入测试的日志文件。

### 7.2 退出码约定
QEMU 的 `isa-debug-exit` 退出码 = `(value << 1) | 1`。

| 退出码 | 含义 | 报告者 | 状态 |
|---|---|---|---|
| 0 | 人类退出（`Ctrl+A` 然后 `X`）或固件正常结束 | — | 已实现 |
| **33** | 引导器装载完成 | 引导器 | 已实现（`--features qemu-exit`） |
| **35** | 内核镜像装载失败 | 引导器 | 已实现（同上） |
| **37** | **内核自检通过**（`BootInfo` 有效 + 串口可用） | **内核** | **M0 待实现** |
| **39** | **内核自检失败**（magic/version 不匹配等） | **内核** | **M0 待实现** |
| 124 | 超时未退出（判为卡死） | — | 已实现（判失败） |

**33 与 37 必须分开**：否则测试无法区分"引导器装完就停了"与"内核真的跑起来了"，
而后者正是 M0 要证明的事。未启用 `qemu-exit` 特性时，引导器打印提示后自旋（交互模式）。

### 7.3 `exit_port` 语义
* `0`：不启用调试退出。内核完成自检后应进入 `hlt` 循环（交互/正常模式）。
* 非 `0`：该值是 `isa-debug-exit` 的 I/O 端口。内核自检通过时写 `0x12`（→ 37），
  失败时写 `0x13`（→ 39）。
* 引导器必须在 QEMU 命令行传入 `-device isa-debug-exit,iobase=0xf4,iosize=0x04`
  （QEMU 8.2 起默认 `iobase` 是 `0x501`），并在测试模式下把 `0xf4` 填入 `exit_port`。

## 8. Milestone 0：Hello from kernel

### 8.1 目标
引导器按 §3 装载内核 → 按 §6.1 交接 → **内核**在串口打印一行含 `BootInfo` 摘要的信息
→ 以退出码 **37** 结束。

### 8.2 非目标
分页/虚拟内存、用户态、IPC、能力模型、调度、SMP、Wasm/WASI、文件系统、ACPICA 表解析
（只透传 RSDP）、内核堆、图形输出、中断处理。

### 8.3 交付物

| # | 交付物 | 说明 |
|---|---|---|
| 1 | `crates/boot-info` | `BootInfo` v0 + `validate()` + 宿主单测（零依赖） |
| 2 | `crates/kernel` | `#![no_std]`、`x86_64-unknown-none`、固定链接地址（暂定 `0x100000`）、单 `PT_LOAD`；入口校验 `BootInfo` → 串口输出 → 写 `exit_port` |
| 3 | `src/bootloader/` | Program Header 解析、按 `p_paddr` 装载 + BSS 清零、栈分配、`BootInfo` 写入、`exit_boot_services`、跳转 |
| 4 | `run-qemu.sh` | ESP 中同时放入 `BOOTX64.EFI` 与 `KERNEL.ELF` |
| 5 | `tests/smoke.sh` | 新增 M0 用例：断言退出码 37 + 断言串口出现内核自检行 |

### 8.4 验收标准（每条可机器判定）
- [ ] `cargo build -p kernel` 产出的 ELF：`e_type=2`、`e_machine=62`、`e_entry` 落在可执行 `PT_LOAD` 段内；
- [ ] 引导器产物仍为 PE32+ / subsystem=10；
- [ ] 冒烟测试断言退出码 **37**（内核自检通过），而**不是** 33；
- [ ] 串口日志含内核输出，且其中内存图条目数 **> 0**、`rsdp != 0`（QEMU+OVMF 下应成立）；
- [ ] **反例 1**：`KERNEL.ELF` 换成非 ELF → 退出码 **35**；
- [ ] **反例 2**：注入错误的 `BootInfo.magic` → 内核自检失败，退出码 **39**；
- [ ] 现有 9 项断言不回归。

### 8.5 已知坑与规避（按踩中概率排序）

| # | 坑 | 规避 |
|---|---|---|
| 1 | `exit_boot_services` 之后误用 boot services（最常见于"想再打印一行"） | 所有打印必须在 §6.1 第 6 步之前完成 |
| 2 | 内核是 PIE（`ET_DYN`） | 内核编译加 `-C relocation-model=static`；装载器按 §3.3 拒绝 |
| 3 | 装载地址 ≠ 链接地址 | 严格按 `p_paddr` 装载（§3.2） |
| 4 | 栈对齐不满足 `rsp % 16 == 8` | 引导器按 §4.1 设置；内核入口可再对齐一次 |
| 5 | 未清 BSS | 按 §3.2 第 3.4 步清零 |
| 6 | 交接后固件 IDT 失效 → 任何 bug 都是三重故障重启 | M1 第一件事就是安装最小 IDT（§9） |
| 7 | `isa-debug-exit` 端口不对 | `-device isa-debug-exit,iobase=0xf4,iosize=0x04` + `exit_port=0xf4` |
| 8 | 内存图用 `sizeof` 而非 `desc_size` 遍历 | 按 §5.3 的告警执行 |

## 9. 后续里程碑（占位，**未设计**）

| 里程碑 | 内容 | 前置 |
|---|---|---|
| M1 | 最小 IDT + panic 处理器 + 内核串口日志设施 | M0 |
| M2 | 物理页帧分配器（基于 M0 传入的内存图，`LOADER_DATA` 视为已占用）+ 内核堆 | M1 |
| M3 | 单核内核线程/调度骨架 | M2 |
| M4 | 第一个用户态服务（最小特权级切换，暂不定义 IPC 语义） | M3 |
| M5 | IPC 消息格式 + 能力令牌语义（**此时才谈**，需要真实用户态进程作为约束） | M4 |

> 本表仅用于表达**顺序依赖**，不构成设计。任何一项在开工前都需要各自的接口文档。

## 10. 决策记录

| # | 决策 | 结论 | 理由 | 可逆性 |
|---|---|---|---|---|
| 1 | 内核装载方式 | 按 `p_paddr` 分段装载 | 与链接器一致；避免两处地址常量漂移；不妨碍将来 higher-half | 中（改契约需同步两侧） |
| 2 | 镜像格式 | ELF64 `ET_EXEC` | 工具链原生产出、gdb 可用符号调试、仓库已有校验；扁平格式省不了多少且丢掉调试信息 | 高（可换，但需重写装载器与工具链） |
| 3 | 入口传递 | `e_entry` + `rdi=&BootInfo` | 不依赖符号表（strip 后仍可用）；SysV 使内核入口可用纯 Rust | 低（属 ABI） |
| 4 | 栈由谁提供 | 引导器分配并设好 `rsp` | 内核第一行代码之前就有可用栈；配合 #3 可省掉汇编 stub | 低（属 ABI） |
| 5 | `BootInfo` 位置 | 独立 crate 共用 | 两份 `repr(C)` 必然漂移，且故障是"读到错位内存后莫名崩" | 高 |
| 6 | 内核输出通道 | 串口轮询 COM1 | 固件控制台在退出后不可用；串口可被 `-nographic` 捕获并断言 | 高（将来可加 framebuffer） |
| 7 | "内核已运行"如何断言 | 内核写 `exit_port`（37/39） | 若沿用 33，"装完不跳转"也会通过——测试将失去区分度 | 中（退出码语义一旦发布不宜复用） |
| 8 | `kernel` 独立 crate | 是 | 目标平台不同（`x86_64-unknown-none`）、且内核不得链接 `uefi` | 高 |
| 9 | 最小 IDT 的时机 | M1 第一件事（M0 不做） | M0 内核只做自检输出，故障面小；先拿到闭合回路更值 | 高 |
| 10 | 文档双语 | 同步维护 `_CN` | AGENTS.md 第 4 条 | — |

## 11. 接口变更流程
1. **`BootInfo`**：只允许"追加字段 + `version` 加一 + `size` 增大"；破坏性变更提升 major。
2. **入口 ABI**（§4）：变更必须同时改动 `crates/boot-info`、引导器与内核，并更新本文与
   [core_kernel_CN.md](core_kernel_CN.md) 的接口索引。
3. **退出码**（§7.2）：新增码不得复用已有语义；已发布语义只允许标注废弃。
4. **文档双语**：任何改动必须同步 `kernel_interface.md` 与本文（AGENTS.md 第 4 条）。
5. **实现状态**：每个里程碑完成后更新 §2.1 / §7.2 表中状态列，避免文档与代码再次分叉。
