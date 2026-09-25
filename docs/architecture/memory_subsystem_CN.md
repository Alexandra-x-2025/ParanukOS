# ParanukOS 内存子系统 —— Milestone 2 接口

[English](memory_subsystem.md) | [中文]

> **状态：接口提案，尚未开始实现。**
>
> 前置条件：M0（交接 + 自检）与 M1（IDT / panic / 日志）已完成，见
> [kernel_interface_CN.md](kernel_interface_CN.md)。本文档定义 M2 交付的内容：
> **内核自己的页表、物理页帧分配器与内核堆**。它正是
> [kernel_interface_CN.md](kernel_interface_CN.md) §9 所说「每个里程碑都需要自己的接口文档」中的那一份。

## 1. 范围

### 1.1 本文档定义什么
* 内核自己的页表：恒等映射策略、粒度、属性、存储、安装流程（§3）
* 内核侧解析 UEFI 内存图（§4）
* 物理页帧分配器：空闲/占用判定、粒度、API 契约、上限（§5）
* 内核堆：位置、算法、`#[global_allocator]` 接入、失败语义（§6）
* 退出码 43 与使可测试的故障注入特性（§7）
* M2a/M2b 的拆分、交付物与可否证的验收标准（§8）

### 1.2 本文档不定义什么
平坦恒等映射之外的虚拟内存（无高半区布局、无进程地址空间、无按需分页、无写时复制）、
MMIO 映射（无 APIC / HPET / 帧缓冲）、用户态页权限与 W^X（M4）、SMP 与 TLB shootdown、
内存热插拔、NUMA、换页、ACPI 表解析、堆增长、内核栈保护页。

### 1.3 术语

| 术语 | 含义 |
|---|---|
| 恒等映射 | 虚拟地址 == 物理地址 |
| PML4 / PDPT / PD / PT | x86-64 四级分页的四层表 |
| 2 MiB 大块 | PD 中 `PS=1` 的项，用一个表项映射 2 MiB |
| 页帧（frame） | 4 KiB 物理页，分配器的粒度 |
| RAM 区域 | 内核愿意按普通内存映射的内存图区域（§3.4） |
| `phys_limit` | 内核实际映射的最高物理地址，被 `MAX_IDENTITY_BYTES` 截断 |

## 2. 基线与前置条件

M0 与 M1 已实现并合并。M2 使用以下 `BootInfo` v0 字段：
`mmap_ptr` / `mmap_len` / `mmap_desc_size` / `mmap_desc_ver`、`kernel_base` / `kernel_size`、
`stack_top` / `stack_size`、`rsdp`、`exit_port`。

M2 继承且不得破坏的约定：

* 入口 ABI（`kernel_interface_CN.md` §4）不变，**`BootInfo` 不重新定义**——M2 不新增字段；
* 退出码 33 / 35 / 37 / 39 / 41 含义不变；43 是新增（§7.1）；
* 引导器的装载规则不变，只有页数上限 `MAX_KERNEL_PAGES` 需要提高（§8.1）；
* 串口日志（`kinfo!` / `kwarn!` / `kerror!`）仍是唯一输出通道。

## 3. 内核自己的页表

### 3.1 M2 为什么要自建页表
`kernel_interface_CN.md` §4 已经写明内核「不得依赖」固件遗留的恒等映射，§6.2 再次强调它只是
*通常*存在。M2 是第一个**使用**非引导器所给内存的里程碑（堆背后的页帧），因此从 M2 起映射
是承重的。自建页表消除了这个依赖［决策 #11］，同时也是 M4 用户态的前置条件。

### 3.2 分页模式
四级分页——正是 OVMF 已经启用的模式。动工前内核先断言 `CR4.PAE = 1`、`CR4.LA57 = 0`
（未启用五级分页）、`EFER.LMA = 1`；不满足即为内存初始化失败（43）。内核不会关闭分页、
不会改写 `CR0`/`CR4`、也不会启用 `CR4.PGE`。

### 3.3 粒度
* 前 2 MiB（`0x0..0x20_0000`）：**4 KiB 页**，用一张页表；
* 其余部分：**2 MiB 大块**。

理由：2 MiB 大块让页表存储很小（每 GiB 地址空间 4 KiB），更重要的是可以**静态**分配——
引导阶段不需要分配器就能建立映射。前 2 MiB 之所以例外，是因为那里有低端空洞（VGA/BIOS），
而且我们要让空指针页**故意**不被映射（§3.4）。

### 3.4 哪些地址被映射

定义：

* `is_ram(type)` 对 `EfiLoaderCode(1)`、`EfiLoaderData(2)`、`EfiBootServicesCode(3)`、
  `EfiBootServicesData(4)`、`EfiRuntimeServicesCode(5)`、`EfiRuntimeServicesData(6)`、
  `EfiConventionalMemory(7)`、`EfiACPIReclaimMemory(9)`、`EfiACPIMemoryNVS(10)` 为真；
* `ram_top` = 所有 RAM 区域中 `start + pages * 4096` 的最大值；
* `phys_limit = min(ram_top, MAX_IDENTITY_BYTES)`，其中 `MAX_IDENTITY_BYTES = 4 GiB`
  ［决策 #14］。

一个页（2 MiB 以下）或一个 2 MiB 大块（2 MiB 以上）**存在（present）**当且仅当它与至少一个
RAM 区域相交，例外与属性如下：

| 规则 | 理由 |
|---|---|
| 页 0（`0x0..0x1000`）**永不映射**，即使固件把它标为 RAM | 策略：空指针解引用必须是干净的 `#PF`（→ 41），§8.3 正是用它证明页表确实生效 |
| 非 RAM 区域（`EfiReservedMemoryType`、`EfiUnusableMemory`、MMIO / 端口空间、unaccepted、persistent）**不映射** | 访问它们会大声故障，而不是悄悄打到设备或保留内存 |
| 存在项属性：`RW=1, US=0, PWT=0, PCD=0, NX=0`，仅 2 MiB 大块带 `PS=1` | 尚无用户态（M4）、不映射 MMIO、不分 W^X、不改缓存属性 |
| 与 RAM **部分**相交的 2 MiB 大块会被**整块**映射 | 粒度权衡：大块中的非 RAM 尾部变成「已映射但不会使用」，而页帧分配器仍然不会把它发出去（§5.2） |

`ram_top > MAX_IDENTITY_BYTES` **不是**错误：内核映射前 4 GiB，打印一条警告并继续。4 GiB 远
超测试虚拟机（QEMU 默认 128 MiB，`run-qemu.sh` 不传 `-m`）；这个上限只是因为页表存储和页帧
位图是静态的（§3.5）。日后要取消上限，就得把两者改为从页帧分配器自身取内存。

> ⚠️ **M2 不映射 MMIO。** 内核不驱动任何 MMIO 设备：串口是端口 I/O（`0x3F8`），本地 APIC、
> HPET、帧缓冲都未被触碰。RSDP 只是被**转发**——M2 不读取 ACPI 表，这才使该限制当前无害。
> 之后任何要碰 ACPI 表、本地 APIC 或帧缓冲的里程碑，都必须先补上显式映射。

### 3.5 页表存储

```
MAX_IDENTITY_BYTES = 4 GiB
tables = PML4（1 页）+ PDPT（1 页）+ 低端 2 MiB 的 PT（1 页）+ PD（4 页，每页覆盖 1 GiB）
       = 7 页 = 28 KiB
```

存储是内核镜像 `.bss` 中一个 `#[repr(align(4096))] static` 竞技场（arena）［决策 #14］：不需要
分配器、引导器已经清零、其物理地址等于链接地址。使用前内核断言该竞技场 4 KiB 对齐且落在内核
镜像区间内；任一不满足即内存初始化失败（43）。若将来某台机器需要的表超出预算，构建函数返回
`Err(OutOfArena)` 而不是越界写出。

### 3.6 构建与安装
1. 从内存图算出 `phys_limit`（§4）；
2. `kernel_memory::paging::build(arena, &map, config)` → `Result<PageTables, BuildError>`，
   其中 `PageTables { pml4_phys, mapped_bytes, blocks, tables_used }`——纯逻辑，可在宿主平台单测；
3. 打印一行：`paging: 恒等映射 N MiB，块粒度 2 MiB，低端 2 MiB 用 4 KiB 页，CR3=0x…`；
4. `mov cr3, pml4_phys`，写之前**先断言 `CR4.PCIDE` 为 0**（若 PCIDE 置位，`CR3` 低位携带 PCID，
   直接写入裸表地址会破坏它）；
5. **切换后重新校验 `BootInfo`**：它位于物理地址上，只有恒等映射确实覆盖它才可达；
6. 不需要 `invlpg` 与 `sfence`（写 `CR3` 会刷新非全局 TLB 项；单核、无写合并内存）。

固件页表被**弃置**而非释放：它们位于 `EfiBootServices*` 内存中，而 M2 从不发放这类内存（§5.2）。

## 4. 解析 UEFI 内存图

`crates/kernel-memory/src/map.rs`——零依赖、可在宿主平台单测，并让内核不引入 uefi-rs
（`kernel_interface_CN.md` §5.3）。

```rust
pub struct MemoryMap<'a> { /* bytes, entries, stride, version */ }
pub struct Region { pub kind: MemoryKind, pub base: u64, pub pages: u64 }

impl<'a> MemoryMap<'a> {
    pub fn parse(ptr: u64, entries: u64, stride: u32, version: u32) -> Result<Self, MapError>;
    pub fn iter(&self) -> impl Iterator<Item = Region> + '_;
    pub fn ram_top(&self) -> Option<u64>;
}

pub enum MapError { Empty, StrideTooSmall(u32), AddressOverflow { index: usize } }
```

规则：

* 必须用 `BootInfo.mmap_desc_size` 遍历——OVMF/QEMU 8.2 下是 48——**绝不**用
  `size_of::<EfiMemoryDescriptor>()` = 40；这正是 `kernel_interface_CN.md` §5.3 记录过的坑；
* `stride < 40` → `StrideTooSmall`；`entries == 0` → `Empty`；某条描述符的
  `base + pages * 4096` 溢出 `u64` → `AddressOverflow`；
* 未知 `type` 变成 `MemoryKind::Unknown(u32)`，并视为**非 RAM**——未来的固件不能让我们映射
  看不懂的东西；
* 解析器只读取偏移 0、8、24 三个字段（type、base、pages）；忽略 `VirtualStart` 与 `Attribute`
  （M2 不用），但绝不假设尾部填充为零。

内存图错误属于**自检失败（39）**而不是 43：它意味着引导器与内核之间的契约坏了，与 `BootInfo`
非法同类。43 留给之后真正发生的「内存初始化失败」。

## 5. 物理页帧分配器

`crates/kernel-memory/src/frame.rs`。粒度：**4 KiB**［决策 #16］。

### 5.1 状态

```rust
pub struct FrameAllocator<'a> { /* 两张位图 + 管理区间 + 保留区间 + 计数 */ }
```

* 两张 `.bss` 中的 `static` 位图，**各每帧 1 bit**：`allocatable`（`1 = 这一帧可被发放`）
  与 `used`（`1 = 当前已发放`）；
* 之所以分成两张：`free` 必须能区分"从未属于分配器的页帧"（固件保留区、MMIO、非 conventional
  内存）与"本来就是空闲的页帧"。只有一张位图时两者都是 `1`，`free` 就会把绝不能发放的内存放回
  空闲池；
* 当 `MAX_IDENTITY_BYTES = 4 GiB` 时两张共 `2 × 4 GiB / 4 KiB / 8 = 256 KiB`；
* 第 *i* 位描述页帧 `managed_start + i * 4096`。

### 5.2 哪些页帧是空闲的

`init(bitmap, &map, reserved, config)` 先把所有页帧标为"不可发放"（`allocatable = 0`）且
"已占用"（`used = 1`），只有同时满足以下条件的页帧才会被标为空闲且可发放：

1. 该页帧完整落在某个 `EfiConventionalMemory` 区域内——M2 **只**发放 conventional 内存
   ［决策 #15］，尽管 `kernel_interface_CN.md` §6.2 说 Boot Services 内存在交接后已归空闲。
   这是刻意的：复用固件内存值得单独测量，而 M2 并不缺这点内存；
2. 页帧位于 `MIN_FREE_ADDR = 1 MiB` 及以上［决策 #15］：低端内存存有固件结构，且 2 MiB 以下
   本来只是低端页表的附带映射；
3. 页帧低于 `phys_limit`；
4. 页帧不与任何**显式保留**区间相交：内核镜像
   （`kernel_base..kernel_base + kernel_size`）、内核栈、`BootInfo` 页、内存图缓冲
   （`mmap_ptr..mmap_ptr + len * desc_size`）、页表竞技场。

第 4 条与第 1 条在信息上是冗余的——上述区间都会被报告为 `EfiLoaderData`——但**刻意保留**：
分配器不应依赖固件是否正确分类了我们自己的分配。自检会断言没有任何空闲页帧与保留区间相交，
而这正是此处出 bug 时会破坏的不变量。自检会交叉校验**两份**清单：分配器自己记录的那份，以及
内核按 `BootInfo` 独立算出的那份（内核镜像、栈、`BootInfo` 页、内存图缓冲、页表竞技场、页帧位图）。

规则 1 的实测代价（QEMU 8.2 + OVMF，默认 128 MiB）：受管理区间是 127 MiB（32512 帧），但其中只有
约 78 MiB（20027 帧）是 `EfiConventionalMemory`；其余是 `BootServices*` / `RuntimeServices*` / ACPI
内存，M2 刻意尚未复用。

### 5.3 API 契约

```rust
pub fn init(bitmap: &mut [u8], map: &MemoryMap, reserved: &[Range<u64>], cfg: Config)
    -> Result<Self, InitError>;                       // InitError::BitmapTooSmall
pub fn alloc(&mut self) -> Option<PhysFrame>;         // 最低的空闲页帧
pub fn alloc_contiguous(&mut self, count: usize) -> Option<PhysRange>;
pub fn free(&mut self, frame: PhysFrame) -> Result<(), FreeError>;
                                                      // AlreadyFree | NotManaged | Reserved
pub fn stats(&self) -> FrameStats;                    // managed / free
pub fn reserved_ranges(&self) -> &[ReservedRange];
pub fn first_free_in(&self, start: u64, end: u64) -> Option<PhysFrame>;
```

* **最低地址优先**：确定性，使宿主测试与 QEMU 自检都能做精确断言。ASLR 与抗碎片策略明确不在
  M2 范围内［决策 #17］；
* `alloc_contiguous` 存在的原因是堆是一段连续字节区间（§6.1）；它首次匹配 `count` 个连续空闲
  位，找不到就报告「没有足够连续内存」，而不是返回一段碎片；
* `free` 区分 `NotManaged`（不在受管理区间内，或从未可发放）、`Reserved`（落在显式保留区间内）与
  `AlreadyFree`（重复释放），使自检可以断言三种情况。M2 中 `free` **没有生产调用者**（堆不会收缩）；
  它为自检和 M3 存在，并由宿主测试覆盖；
* `reserved_ranges` 与 `first_free_in` 供自检做交叉校验；它们只读，不会扰动空闲集合；
* 页帧 0 永不被管理（低于 `MIN_FREE_ADDR`），因此 `alloc` 绝不会返回它。

### 5.4 并发
M2 内部没有并发：交接后中断关闭、单核，因此 `FrameAllocator` 就是 `UnsafeCell` 包裹的 `static`
加 `Sync` 实现——与 M1 的 IDT 同一模式——外加一条写明的前置条件「启用中断之前必须加锁」
［决策 #20］。

> ✅ **M3 已经把这条前置条件落实**：`FrameAllocator` 现在位于一把中断安全 `SpinLock` 之后，
> 每次调用都经过它（见 [threads_and_scheduling_CN.md](threads_and_scheduling_CN.md) §5）。
> 因此决策 #20 是**被满足**，而不是被推翻。

## 6. 内核堆

### 6.1 区间
* `HEAP_SIZE = 1 MiB`（256 个连续页帧），M2 期间固定，初始化时从页帧分配器取得［决策 #18］；
* 必须连续，因为堆是一段字节区间；`alloc_contiguous` 失败即内存初始化失败（43），并打印具体原因；
* **M2 不支持增长**：增长需要在分配器下面挂区间链表，或者扩展全局分配器的竞技场，那本身就是
  一个设计。M2 记录该限制及其失败方式（分配失败 → panic → 41）；
* 堆区间与其他内存一样是恒等映射的，内核用物理地址引用它——这是可接受的 M2 简化，等引入高半区
  布局时再重审。

### 6.2 算法
手写的**首次匹配（first-fit）空闲链表**，块头存放在空闲块内部［决策 #18］，位于
`crates/kernel-memory/src/heap.rs`，作用于调用方提供的 `&mut [u8]` 竞技场，因此整个算法都能在
宿主平台单测：

* 每个块带 24 字节块头：`magic`（u32）、填充、`size`（块总大小）、`next`（下一个空闲块的偏移）。
  magic 区分空闲块与已分配块，重复释放与陌生指针因此可以被拒绝；
* `MIN_BLOCK = 最小负载(16) + 块头(24) = 40` 字节，是分配器**会单独切分**的最小块；请求留下的尾部
  小于该值时会被一并吸收，因此**已分配块**可以小到只剩块头，而**空闲块**也可能暂时小于
  `MIN_BLOCK`，直到邻居被释放并与它合并。两种情况都有宿主测试覆盖；
* `alloc(layout)`：首次匹配；当剩余部分还能容纳一个块头加最小负载时切分该块；
* `dealloc(ptr)`：把块按**地址序**放回链表，并与前后邻居**合并**；
* 块头**永远紧贴负载**：负载之前的填充要么为空，要么大到足以单独成为一个空闲块（很小的填充会被
  抬到 `MIN_BLOCK`）。这正是 `dealloc` 只凭指针就能找回头部的原因，也让它不依赖分配时的 `Layout`；
* 对齐：支持不超过 `PAGE_SIZE` 的 `layout.align()`；返回的负载同时满足请求的对齐与块头的 8 字节对齐；
* 按 `GlobalAlloc` 契约拒绝 `Layout.size == 0` 与不支持的对齐；
* 失败返回空指针；`alloc::alloc::handle_alloc_error`（Rust 1.68 起稳定的默认处理器）以
  "memory allocation of N bytes failed" panic，再由 M1 的 panic 处理器变成退出码 41——**无需任何
  unstable 特性，也不需要 `#[alloc_error_handler]` 属性**。

### 6.3 接入方式
全局分配器由内核二进制自己持有，于是 `Box`、`Vec`、`String`、`BTreeMap`、`format!` 在内核里
都能用：

```rust
#[global_allocator]
static KERNEL_HEAP: KernelHeap = KernelHeap::new();   // GlobalAlloc → heap::FreeList
```

两方面后果都要如实承认：内核获得了动态数据结构，也获得了泄漏的能力。因此自检结束时断言堆回到
**单个**空闲块（§6.4 第 5 步）。

### 6.4 自检（证明 M2 真正工作的部分）
在 `kernel_main` 末尾、`self-check OK` 之前运行；失败时打印步骤号、被破坏的不变量与相关地址：

1. 分配 `N = 64` 个大小各异的块（16、33、64、129、256、1025、4096、7 字节），向每个块的
   **每一个字节**写入由块序号派生的图案；
2. 断言所有块两两不相交、都在堆区间内——每一条都是真检查，不是恒真式；
3. 逐字节读回比对：这证明页帧真的被映射且可写，而不仅仅是分配器的算术正确；
4. 释放其中一半，再分配 `N/2` 个同样大小的替换块，验证替换块与仍存活的块不相交（按地址集合
   判不相交，而不是假设某个地址），并且原有存活块内容未被破坏；
5. 全部释放，断言堆回到恰好**一个**空闲块，其大小等于整个堆减去一个块头（1 MiB − 24 =
   1048552 字节）——即合并生效——并断言两次"申请两倍堆大小"的请求都干净地失败（返回空，不回绕、
   不 panic）；
6. 页帧分配器检查：两次分配必须是**不同页帧**（唯一能抓住"重复分配"的检查）；释放后重新分配必须
   拿回同一个最低页帧；重复释放必须被拒绝；保留区间内不得有空闲页帧——既对照分配器自己记录的清单，
   也对照内核按 `BootInfo` 独立算出的清单；且 `FrameAllocator::stats().free` 相比取堆之前恰好减少
   `HEAP_SIZE / 4096 = 256`；
7. 打印 `heap: 0x…..0x… (1 MiB), 自检 OK`。

任何一步失败都会打印步骤、不变量与地址，并以 **43** 退出。

## 7. 退出码与故障注入

### 7.1 新增退出码

| 退出码 | 含义 | 报告者 | 状态 |
|---|---|---|---|
| **43** | **内核内存初始化失败**——页表无法构建或安装、页帧分配器无法初始化、或堆自检失败 | 内核 | M2 新增 |

39 的含义不变（「内核判定自己无法运行」：`BootInfo` 非法、内存图无法解析、没有 RSDP）。这个区分
很重要：39 是交接坏了，43 是内存子系统坏了，二者的负责人和修法都不同。

### 7.2 故障注入（仅测试）
两个特性，均带有长期声明「生产构建绝不启用」：

* `inject-memory-fault`（新增）：页帧分配器发放页帧时**不把它标记为已占用**（crate 特性
  `inject-double-alloc`，由内核同名特性转发），于是 §6.4 第 6 步「两次分配必须是不同页帧」的检查
  必然失败 → **43**。这个故障值得注入：两个调用方悄悄拿到同一块内存是页帧分配器最危险的 bug，
  而且在有人被覆写之前完全看不出来；
* `inject-null-deref`（新增）：页表安装完成后，故意读取地址 `0`——按策略未被映射（§3.4）→ `#PF`
  （向量 14）→ M1 的异常处理器 → **41**。这是唯一能证明生效的是**内核的**页表而不是固件的页表的测试。

`inject-fault`（M1，`ud2` → 41）现在运行在页表安装**之后**，因此既有的 M1 验收用例额外证明了异常
处理在我们自己的页表下依然工作。

## 8. 交付计划、交付物与验收标准

### 8.1 拆分

| 部分 | 内容 | 为什么拆 |
|---|---|---|
| **M2a** | `crates/kernel-memory` 中的 `map.rs` + `paging.rs`（纯逻辑、宿主单测）；内核构建并安装自己的页表，切换后重新校验 `BootInfo`；退出码 43；引导器提高 `MAX_KERNEL_PAGES` | 页表 bug 与堆 bug 的诊断方式完全不同；拆开让每个 PR 的失败面更小 |
| **M2b** | `frame.rs` + `heap.rs`（宿主单测）；`#[global_allocator]`；§6.4 自检；两个注入特性；新的冒烟用例 | 建立在 M2a 已经证明过的映射之上 |

`MAX_KERNEL_PAGES` 提高了两次：M2a 的 64 → 128 页（页表竞技场 28 KiB），M2b 的 128 → 256 页
（1 MiB，两张页帧位图 256 KiB，加上 `alloc` 与格式化代码后镜像已达 104 页）。忘了这件事的表现是
引导器报 `TooManyPages`（退出码 35）——正是关于镜像区间的那条验收标准在防的问题。

### 8.2 交付物

| # | 交付物 | 说明 |
|---|---|---|
| 1 | `crates/kernel-memory` | 零依赖、`no_std`；内存图 + 页表（M2a）与页帧 + 堆（M2b）的宿主单元测试 |
| 2 | 内核页表 | 由内存图构建、经 `CR3` 安装、切换后重新校验 `BootInfo` |
| 3 | 页帧分配器 | conventional 内存上的位图、最低地址优先、连续分配、自检交叉校验 |
| 4 | 内核堆 | 1 MiB 首次匹配空闲链表、`#[global_allocator]`、由自检验证的合并 |
| 5 | `crates/kernel/src/memory.rs` | 内核侧粘合层：静态量、竞技场、初始化序列、自检 |
| 6 | 退出码 43 + 两个注入特性 | §7 |
| 7 | `tests/smoke.sh` | 新增 M2 用例（§8.3 / §8.4） |
| 8 | 文档 | 本文档、`kernel_interface_CN.md` §7.2 与 §9、`README_CN` / `VISION_CN` 的能力表——全部双语 |

### 8.3 验收标准 —— M2a（**已满足**，PR #16）
- [x] 内核打印 `paging: 恒等映射 …`，映射大小 ≥ 64 MiB，且块粒度为 2 MiB；
- [x] 内核在安装页表之后重新校验 `BootInfo`（有对应日志行），即恒等映射确实覆盖交接结构；
- [x] `tests/smoke.sh` 正常用例仍断言 37，M1 的反例仍为 35 / 35 / 39 / 41；
- [x] `inject-null-deref` 退出 **41**，且日志指出向量 14（`#PF`）——直接证明生效的是内核页表而非固件页表；
- [x] `python3 tests/check_kernel_elf.py` 通过，并额外断言页对齐后的装载区间不超过 `MAX_KERNEL_PAGES`；
- [x] `cargo test -p kernel-memory` 通过（内存图 + 页表测试）。

### 8.4 验收标准 —— M2b（**已满足**，PR #17）
- [x] 日志包含页帧计数且 `free > 0`，以及 1 MiB 堆区间的日志行；
- [x] 七步自检在真实 QEMU + OVMF 上全部通过，包括 64 个块的逐字节读回、重复释放检测与合并断言；
- [x] `inject-memory-fault` 退出 **43**，正常用例仍退出 **37**；
- [x] `inject-fault`（M1）仍退出 **41**，且此时运行在内核自己的页表下；
- [x] 初始化后空闲页帧数恰好减少 `HEAP_SIZE / 4096`；
- [x] `cargo test -p kernel-memory` 覆盖分配 / 释放 / 复用 / 合并 / 对齐 / 耗尽 / 重复释放；
- [x] 既有冒烟断言无回归，且 fmt/clippy 在 CI 已用的全部配置下保持干净。

在 QEMU 8.2.2 + OVMF（默认 128 MiB）上实测，35/35 条冒烟断言全绿：

```
frames: 管理 32512 帧（127 MiB），堆取走后空闲 20027 帧
heap: 0x168000..0x268000（1024 KiB，占用 256 个连续页帧）
heap: 自检 OK（全部释放后空闲 1048552 字节 / 1 个块）
```

### 8.5 已知坑（按可能性排序）

| # | 坑 | 对策 |
|---|---|---|
| 1 | 通过固件的映射写页表项、切换 `CR3` 后再读回——物理地址相同，但极易混淆 | 构建函数是纯的且有宿主测试；内核只负责装载成品页表 |
| 2 | 用 40 字节步长遍历内存图 | §4，并有一条 `stride = 48` 的宿主测试 |
| 3 | `MAX_KERNEL_PAGES` 小于新镜像 | §8.1；验收标准检查装载区间 |
| 4 | `CR4.PCIDE` 已置位，裸写 `CR3` 破坏地址 | 写之前先断言（§3.6） |
| 5 | 假设内核镜像 / 栈 / `BootInfo` 绝不在 conventional 内存中 | 显式保留区间 + 自检交叉校验（§5.2） |
| 6 | 用与分配时不同的 `Layout` 释放（`GlobalAlloc` 允许这样） | 块头位于负载之前；宿主测试用一种对齐分配、另一种对齐释放 |
| 7 | 目标平台的 sysroot 里没有 `#[global_allocator]` + `alloc` | M2b 第一件事就是先编译验证 |
| 8 | 页帧未映射而堆自检仍然通过（只验算术） | 第 3 步逐字节写读回 |

## 9. 决策记录

| # | 决策 | 结果 | 理由 | 可逆性 |
|---|---|---|---|---|
| 11 | M2 自建页表，而不是信任固件的恒等映射 | 四级、由内存图构建、经 `CR3` 安装 | 从 M2 起映射是承重的（堆页帧）；`kernel_interface_CN.md` §4/§6.2 已警告它不被保证；也是 M4 的前置条件 | 中（构建函数是纯的、可替换） |
| 12 | 前 2 MiB 之外使用 2 MiB 大块 | 只有 `0x0..0x20_0000` 用 4 KiB 页 | 同样覆盖率下页表存储少约 1000 倍，从而可以静态分配 | 高 |
| 13 | 非 RAM 地址不映射 | 任何访问都会大声故障（→ 41） | 对内存子系统而言，静默是最糟的失败方式 | 高 |
| 14 | 页表用静态 `.bss` 竞技场、页帧用静态位图、上限 4 GiB | 28 KiB + 128 KiB | 引导阶段不需要分配器；静态上限诚实且可测试；两者改从分配内存取得后即可取消 | 中 |
| 15 | 只发放 `EfiConventionalMemory` 且 ≥ 1 MiB | 保守的空闲集合 | 复用 Boot Services 内存值得单独测量；1 MiB 把固件结构与低端附带映射挡在分配器之外 | 高 |
| 16 | 页帧粒度 4 KiB | 统一 | 唯一能支撑 4 KiB 页表项的粒度；感知大页的分配是后续优化 | 高 |
| 17 | 最低地址优先、支持连续分配 | 确定性 | 宿主与 QEMU 断言都能精确；M2 不做 ASLR 与抗碎片策略 | 高 |
| 18 | 手写首次匹配堆、1 MiB、不增长 | 在 `crates/kernel-memory` 中宿主单测 | 保持内核零第三方依赖（AGENTS.md 第 2 条），并让算法待在一个真能被单测的 crate 里 | 高 |
| 19 | 内存初始化失败用新退出码 43 | 与 39 区分 | 内存子系统坏了与交接坏了，负责人和修法都不同 | 中（已发布的含义冻结） |
| 20 | M2 不加锁 | 写成前置条件 | 单核、中断关闭；一把未经验证的锁比写明前置条件更糟 | 高 |
| 21 | 空指针页永不映射 | 作为策略，无视固件的类型 | 让空指针解引用成为 `#PF`（→ 41），并给 M2a 一个证明页表确实生效的测试 | 高 |
| 22 | 页帧用两张位图（`allocatable` + `used`）而不是一张 | 256 KiB 而不是 128 KiB | 只有一张位图时，`free` 无法区分"从未可发放"（MMIO、保留区）与"本来就空闲"，会把绝不能发放的内存放回空闲池 | 中 |
| 23 | 43 的注入方式定为"同一个页帧发两次" | crate 特性 `inject-double-alloc` | 重复分配是页帧分配器最危险、也是自检最该抓住的 bug；改成"去掉保留区间"则根本不可观测 | 高 |

## 10. 接口变更流程
1. `crates/kernel-memory` 的公开项是内核与其自身逻辑之间的接口；改动必须配套宿主测试。
2. 任何改动**引导器↔内核**契约（`BootInfo`、入口 ABI、退出码）的变更，遵循
   `kernel_interface_CN.md` §11。
3. 双语文档：每次改动必须同时更新本文件与 [memory_subsystem.md](memory_subsystem.md)
   （AGENTS.md 第 4 条）。
4. 实现状态：每完成一部分，更新 §8 的状态列与 `kernel_interface_CN.md` §9 的 M2 行。
