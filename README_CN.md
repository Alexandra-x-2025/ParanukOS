# ParanukOS

[English](README.md) | [中文]

ParanukOS 是一个基于 Rust 开发的高可靠性微内核操作系统，旨在为现代硬件提供安全且稳定的计算基础。通过采用微内核架构和用户态服务模型，ParanukOS 致力于构建一个既能支持底层系统编程，又能承载高层 Web/Wasm 应用的稳固平台。

## 🔗 参考项目
本项目受到 [os.phil-opp.com](https://os.phil-opp.com/) 的架构启发并参考了其相关设计。

## 🌟 核心哲学
ParanukOS 的核心理念是 **“强隔离”**。我们相信系统的稳定性源于对组件的最小化权限控制：
*   **微内核架构：** 内核仅负责最基础的任务，如 IPC、内存管理和基本调度。
*   **高可靠性：** 通过将所有驱动程序和文件系统移至用户态，确保单个服务的崩溃不会导致整个系统挂掉。
*   **现代硬件聚焦（明确边界）：** 仅 x86-64 + UEFI + ACPI 6.x —— 不支持 BIOS/CSM、32 位 x86 与遗留设备。
*   **Wasm 为核心的应用层：** 提供安全的沙箱环境，让用户能够通过 WebAssembly 运行软件。

## 🚀 关键特性

### 🏗️ 微内核架构
采用纯 Rust 实现的微内核，提供：
*   **零拷贝 (Zero-copy) IPC:** 高性能的进程间通信机制。
*   **基于能力的安全性 (Capability-based Security):** 对所有硬件资源进行精细化的权限管理。
*   **多核支持 (SMP):** 原生支持多核心调度与同步。

### 💾 文件系统路线图
*   **当前：** 只用 FAT32 —— ESP 本身就是 FAT32，读取"镜像被加载的那个卷"不需要新驱动。
*   **以后（需要各自的接口文档）：** 基于 TFS 设计理念，用纯 Rust 在用户态实现 **写时复制 (CoW)** 文件系统，实现物理级别的数据写入隔离。
*   **明确排除在短期之外：只读 Ext4** —— extent 树、日志、间接块与校验和使其成为以月计的工程量，bring-up 阶段几乎没有回报。

### 🖥️ 硬件与启动
*   **UEFI 引导器（不使用 GRUB）：** 基于 [uefi-rs](https://github.com/rust-osdev/uefi-rs) 0.39、以 `x86_64-unknown-uefi` 编译的 UEFI 应用。它会定位自己被加载时所在的 ESP 卷，校验内核镜像是 ELF64/x86-64 可执行文件，然后载入内存。整个引导路径中不使用 GRUB。
*   **Rust 原生开发:** 从底层开始使用 Rust 构建，消除内存安全漏洞。
*   **接口规范:** 引导器与内核之间的契约（镜像格式与装载规则、入口 ABI、`BootInfo`、交接语义、退出码）定义在 [docs/architecture/kernel_interface_CN.md](docs/architecture/kernel_interface_CN.md)。

### 🌐 应用环境
*   **Web/Wasm 运行时:** 用户应用的主要运行环境，提供接近原生性能的沙箱化软件商店体验。
*   **极简 POSIX 层:** 仅为驱动程序和基础系统服务提供必要的兼容性接口。

## 🛠️ 快速开始

### 前置依赖

*   **Rust stable**（已在 1.98 上验证；**不需要** nightly）。
*   目标平台 `x86_64-unknown-uefi`（引导器）与 `x86_64-unknown-none`（冒烟测试用来编译占位内核镜像）。
*   **QEMU 与 OVMF** —— 仅在运行或测试镜像时需要。

```bash
rustup target add x86_64-unknown-uefi x86_64-unknown-none

# Ubuntu / Debian
sudo apt install qemu-system-x86 ovmf
# Fedora / RHEL
sudo dnf install qemu-system-x86 edk2-ovmf
```

### 构建

```bash
cargo build            # 产物: target/x86_64-unknown-uefi/debug/paranukos.efi
cargo build --release  # 产物: target/x86_64-unknown-uefi/release/paranukos.efi
```

产物是 **PE32+ EFI 应用**（`subsystem = 10`），即 UEFI 固件可以加载的格式。目标平台已在 `.cargo/config.toml` 中默认为 `x86_64-unknown-uefi`；`x86_64-unknown-none` 产出的是 ELF，固件无法引导。

### 在 QEMU 中运行

```bash
cargo kernel   # 构建内核（裸机目标）→ target/x86_64-unknown-none/debug/kernel
cargo run      # 构建引导器、把内核放进 ESP、用 OVMF 启动
```

启动脚本会建立符合规范的 ESP 目录树（`/EFI/BOOT/BOOTX64.EFI`），把内核放到约定路径
（`\EFI\PARANUKO\KERNEL.ELF`），然后用 OVMF 引导。内核打印完 `BootInfo` 摘要后，
QEMU 要么以状态码退出（启用 `qemu-exit` 特性时），要么 CPU 停机。

退出模拟器：**先按 `Ctrl + A`，再按 `X`**。请勿使用 `Ctrl + C`，否则会留下僵尸 QEMU 进程并霸占串口。

### 内核镜像

引导器会在 ESP 的 `\EFI\PARANUKO\KERNEL.ELF` 处查找内核镜像，校验它是 ELF64 / x86-64 / `ET_EXEC` 后复制到新分配的物理页中。仓库里目前还没有真正的内核：任意合法的 ELF64 可执行文件都可以用来跑通这条路径（例如编译 `tests/fixtures/dummy_kernel.rs`）。

### 测试

```bash
python3 tests/check_pe.py target/x86_64-unknown-uefi/debug/paranukos.efi  # 静态校验 PE 结构
bash tests/smoke.sh                                                       # QEMU 端到端冒烟测试
```

冒烟测试包含两个用例：正向（ESP 中存在合法内核镜像 → 必须成功引导并载入）与反向（缺少内核镜像 → 必须优雅报错，而不是挂死、崩溃或误报成功）。

### 已知限制

*   **目前只到 Milestone 0。** 引导器会装载内核、调用 `exit_boot_services` 并跳转到入口；内核随后校验 `BootInfo`、在 COM1 打印摘要并停机。尚无分页、用户态、IPC 与调度 —— 顺序见 `docs/architecture/kernel_interface.md` §9。
*   只接受 `ET_EXEC` 内核镜像；PIE（`ET_DYN`）内核需要重定位处理，目前未实现。

## 🗺️ 路线图
- [ ] **第一阶段：核心基础** (IPC、内存管理、能力模型)
- [ ] **第二阶段：硬件抽象** (ACPI 解析、块设备驱动)
- [ ] **第三阶段：系统服务** (网络协议栈、CoW 文件系统)
- [ ] **第四阶段：应用运行时** (WASI 集成)

---
*ParanukOS - 通过隔离实现稳定性。*
